use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use std::sync::atomic::Ordering;
use std::time::Instant;

use eframe::egui;
use egui::{Color32, RichText};

use crate::config;
use crate::session::{self, Session};
use crate::terminal;

/// 启动宽限期：会话创建后 10 秒内不弹「运行结束」/「执行完成」系统通知与
/// 任务栏闪烁。刚启动的会话 shell 初始化/命令首屏输出会制造大量看似「完成」
/// 的瞬间，宽限期过滤误报（进度型命令会在宽限期后再按正常规则提示）。
const STARTUP_GRACE_MS: u64 = 10_000;

/// 配置节流落盘间隔：仅窗口位置/尺寸变化（拖动/resize 每帧连续变化）时
/// 最多每 CONFIG_SAVE_INTERVAL 写盘一次；页签结构变化、显式保存都立即落盘。
const CONFIG_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// 全部静止时的慢心跳帧间隔（cmd/conhost 式节能：无脏区不持续重绘）。
/// 输出到达、鼠标/键盘事件都走 request_repaint() 即时唤醒，心跳只负责
/// 兜底捕获无事件干系的状态推进（任务完成通知稳定窗口、状态栏倒计时等）。
const IDLE_HEARTBEAT_MS: u64 = 500;

/// 忙时的固定帧间隔：输出/加载期间按 10 FPS 重绘。
/// ponytail: 可调帧率档（30/60）已整体移除——持续高帧率重绘干扰 Windows
/// 悬停激活窗口（焦点随鼠标）：10 FPS 正常、30 FPS 输出中实测失效（反复
/// 回归 3 次，见 921f062/0ae5904）。根治 = 锁死 10 FPS，杜绝再被调高。
const BUSY_FRAME_MS: u64 = 100;

/// 页签状态判定：仅凭终端内容（last_output_ms，reader 每收到一块输出即
/// 刷新）。最近 3 秒内有输出 → 运行中；3 秒无内容 → 视为「输出结束/完成」。
/// 不做进程树 CPU 采样，不做网格/光标/锁存启发——后台任何「固定刷新」
/// （周期重绘、CPU 轮询、TUI 思考间隙）都不再影响判定；滚动/翻页/裁窗只改
/// 视口、不产生内容 → 天然不计更新状态。
/// 代价即本方案：后台 TUI 静默思考 / 网络等待（>3s 无输出）会被判为完成；
/// 用户要求以终端内容为准，出现该情况即 3s 后亮 ✅/弹通知（后果已知晓）。
const OUTPUT_END_MS: u64 = 3_000;
/// 用户驱动回显例外（✂ 不吞任务真实输出）：键盘/IME/粘贴、点击/中键转发都写
/// last_input_ms（terminal.rs 统一走 stamp_user_input 记账）→ 直接引发的回显
/// 是用户驱动、不是任务在跑，其窗口内跳过 🔄 判定；滚动转发的 TUI 重绘回显记
/// last_scroll_ms 走自己的 500ms 短窗（见 SCROLL_ECHO_MS）。真实输出晚于各自
/// 窗口仍按 last_out 正常判 🔄——输入/滚动看日志期间页签照常实时刷新运行状态。
const INPUT_ACTIVE_MS: u64 = 1_500;
/// 滚动转发回显例外窗口：鼠标上报/备用屏路径滚轮转发 TUI 后，TUI 立即整屏
/// 重绘回显 → 刷新 last_output_ms → 若不加例外会误亮 🔄 3s。记 last_scroll_ms
/// 专用短窗（terminal.rs 滚动处理处 stamps_scroll_echo 统一写，两个转发分支
/// 都不漏），只吞滚动驱动的这一下重绘；真实任务输出晚于窗口即照常判 🔄——
/// 持续滚动看日志时页签仍实时显示运行中。
/// 本地缓冲滚动（普通 shell）不产生 PTY 输出，不写此字段，完全不影响图标。
const SCROLL_ECHO_MS: u64 = 500;
/// 「执行完成」通知/闪烁需在 ✅ 稳定停留 2s（过滤 🔄↔✅ 间隙横跳）。
const DONE_STABLE_MS: u64 = 2_000;
/// 「运行/完成」状态的重算间隔：**最快 1 秒一次**（用户拍板的降频）。
///
/// 页签图标与「任务完成」通知都读同一份每秒快照（Session.state_icon /
/// state_done），好处三条：① 检测频率封顶（帧率再高也不重算时间判据）；
/// ② 图标不会亚秒抖动；③ 图标与通知不会各判各的、互相打脸。
/// 三个**事件驱动**的例外不排队，立即重算：首次判定、退出/加载态翻转（❌ 与
/// 启动 🔄 必须即时）、出现新的实质内容（命令刚跑起来 🔄 要立刻亮）。
const STATE_CHECK_MS: u64 = 1_000;
/// 动画活性阈值：最近 ANIM_BUSY_MS 内出现过**纯动画**输出 ⇒ 判定「画面还在高频
/// 动」⇒ **还在思考/还在跑命令，不判完成**（不亮 ✅、不发完成通知）。
///
/// 这是纯屏幕启发式里唯一能分开「思考中」与「真停了」的分量，不依赖任何 agent
/// 私有协议（不追 JSONL、不查 DB）：思考期 agent 都在刷 spinner/时钟/进度条，
/// 帧间隔 ~80-150ms；空闲时的光标闪烁是 ~0.5-1s 一次。
/// 阈值 250ms 正好卡在两者之间：任取 1s 里的采样点，spinner 恒落在窗口内
/// （→ 恒判「在动」），而光标闪烁绝大多数采样点落在窗口外（→ 照常判完成），
/// 于是不再出现「pi 回合跑完 🔄 直接变空、✅ 永远亮不出来」。
///
/// 两侧的已知误差（都是「屏幕启发式」的固有边界，只能靠调 ANIM_BUSY_MS 换）:
/// ① 真·全程无输出也无动画的长命令（静默编译）3s 后仍会判完成——屏幕一动不动
///    时，与「跑完了」在信息上不可区分；
/// ② 空闲时仍高频刷动画的全屏 TUI 会常亮 🔄——这是「不误报完成」的对价：
///    宁可少亮一次 ✅，也不在思考/执行途中谎报完成。
const ANIM_BUSY_MS: u64 = 250;
/// 同一页签两条系统通知的最小间隔：完成提醒每轮 ✅ 都可再弹（done_notified
/// 随 ✅ 离开复位），靠 10s 节流防周期输出/退出-完成连发轰炸（用户拍板）。
const TOAST_MIN_INTERVAL_MS: u64 = 10_000;
/// 后台页签收割子进程（try_wait）的低频间隔：孙进程继承 ConPTY 句柄时管道
/// 不 EOF，reader 判不了退出，只能低频轮询进程状态（每帧 syscall 不值得）。
const BG_REAP_MS: u64 = 3_000;

/// 通知过滤阈值：会话累计实质输出字节数（非动画块的可打印字节，见
/// session.rs out_bytes）不足此值时，「运行结束」/「任务完成」一律不弹通知。
/// 过滤目标：终端零输入零输出、界面无任何变化却突然弹「完成」的误报——
/// bare shell 提示符（一二十字节）、秒退命令、纯 spinner 动画（动画不计入
/// 可打印字节）都不构成用户可看的内容；任何一行真实命令输出（>32 字节）
/// 即达标，正常任务完成提醒不受影响。
const MIN_OUTPUT_BYTES: u64 = 32;

/// 首页里的两个子页。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
}

/// 顶部页签：第一个永远是首页，后面每个对应一个终端会话。
pub enum Tab {
    Home,
    // Session 承载大量缓存（galley/图集/hash 缓冲）约 26KB，
    // 不 Box 会让每个 Tab 都撑到最大变体大小。
    Session(Box<Session>),
    /// 重启/切换命令期间的占位页签：保持位置不变，标题可见，
    /// 防止页签消失再出现的闪烁。
    Placeholder { title: String },
    Settings,
}

/// 待下一帧在会话页签布局里重新启动/切换命令的会话（异步，不阻塞 UI）。
pub struct PendingRelaunch {
    pub tab_index: usize,
    pub title: String,
    pub dir: String,
    pub cmd: String,
}



/// 待下一帧在会话页签布局里启动的会话。
/// 点击「启动」时若立刻 spawn，窗口还停在首页布局，拿不到终端真实可用面积，
/// 只能估算（窗口减左栏/状态栏余量），与会话页签全宽实际面积恒差一截；TUI
/// 启动即按估算尺寸整屏画页，首帧 resize 事件偶发丢失 → 页面尺寸对不上窗口。
/// 推迟一帧：下一帧的中央面板布局已切到会话页签，按精确尺寸 spawn，零纠正。
pub struct PendingLaunch {
    pub title: String,
    pub dir: String,
    pub cmd: String,
}

/// 输入弹窗的类型与当前文本。
pub enum InputDialog {
    AddProject { name: String, path: String },
    Rename { value: String },
    EditPath { value: String },
}

impl InputDialog {
    fn title(&self) -> &'static str {
        match self {
            InputDialog::AddProject { .. } => "添加项目",
            InputDialog::Rename { .. } => "重命名项目",
            InputDialog::EditPath { .. } => "修改项目路径",
        }
    }
}

/// 确认弹窗。
pub enum ConfirmDialog {
    DeleteProject { index: usize, name: String },
    /// 会话页签崩溃后的处理选择：重新打开 / 关闭。
    RelaunchSession { dir: String, title: String, reason: String },
}

/// 页签栏点击/右键菜单产生的动作。
enum TabAction {
    Activate(usize),
    Close(usize),
    OpenDir(usize),
    OpenVSCode(usize),
    SwitchCommand(usize, String),
    Restart(usize),
}

/// 项目列表点击/双击/右键菜单产生的动作。
enum ProjectAction {
    Select(usize),
    Launch(usize),
    OpenDir(usize),
    OpenVSCode(usize),
    Rename(usize),
    EditPath(usize),
    Delete(usize),
    ToggleHide(usize),
}

/// 加载中文字体作为 Proportional 与 Monospace 的 fallback。
/// 基础字体用内嵌 HACK（替代 egui 默认 Ubuntu+NotoEmoji，省 ~15MB，UI 无 emoji
/// 需求）；CJK 优先 simhei.ttf（~9MB 单 face，比 msyh.ttc 20MB 省 11MB 且解析快，
/// 终端渲染效果稍逊雅黑）。
fn setup_fonts(ctx: &egui::Context) {
    // 基础等宽字体：UI 菜单/按钮/列表与终端共用，中文回落到 cjk。
    ctx.add_font(egui::epaint::text::FontInsert::new(
        "hack",
        egui::epaint::text::FontData::from_static(epaint_default_fonts::HACK_REGULAR),
        vec![
            egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: egui::epaint::text::FontPriority::Lowest,
            },
            egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Monospace,
                priority: egui::epaint::text::FontPriority::Lowest,
            },
        ],
    ));
    let candidates = [
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\msyhbd.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
    ];
    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            ctx.add_font(egui::epaint::text::FontInsert::new(
                "cjk",
                egui::epaint::text::FontData::from_owned(data),
                vec![
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Proportional,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Monospace,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                ],
            ));
            break;
        }
    }
    // 状态图标 ✅🔄❌ 是 emoji，HACK/CJK 无这些字形 → 显示方块。注入系统
    // Segoe UI Emoji 作逐字形兜底（egui 按缺字形回退，不影响 ASCII/CJK 度量）；
    // seguisym.ttf 额外覆盖 ✓✗ 等符号。仅缺失时入图集，内存开销≈0。
    let emoji = [
        ("segoe-emoji", r"C:\Windows\Fonts\seguiemj.ttf"),
        ("segoe-symbol", r"C:\Windows\Fonts\seguisym.ttf"),
    ];
    for (name, path) in emoji {
        if let Ok(data) = std::fs::read(path) {
            ctx.add_font(egui::epaint::text::FontInsert::new(
                name,
                egui::epaint::text::FontData::from_owned(data),
                vec![
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Proportional,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Monospace,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                ],
            ));
        }
    }
}

/// 应用深浅主题（egui 全部控件/字体颜色随之切换）。
fn apply_theme(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
    // 全局文本纯色：深色模式一律纯白、浅色模式一律纯黑，覆盖列表/按钮/状态栏等
    // 所有控件自带的灰色系配色（显式 RichText 强调色仍保留）。
    // 选中底色统一淡灰、选中文字近黑：深浅主题一致，汉字笔划在淡灰底上最稳。
    ctx.all_styles_mut(|style| {
        style.visuals.override_text_color = Some(if dark {
            Color32::WHITE
        } else {
            Color32::BLACK
        });
        style.visuals.selection.bg_fill = Color32::from_gray(176);
        style.visuals.selection.stroke.color = Color32::from_gray(24);
    });
}

/// 从 eframe 的创建上下文里取原生窗口句柄（Windows HWND）。
fn hwnd_of(cc: &eframe::CreationContext<'_>) -> isize {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    match cc.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(w)) => w.hwnd.get(),
        _ => 0,
    }
}

// 固定深色标题栏：egui 的 ThemePreference 只改面板颜色，标题栏由 DWM 绘制，
// 需要显式 DwmSetWindowAttribute。固定为黑色标题栏，不随深浅主题切换。
#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: isize,
        attr: u32,
        attr_value: *const std::ffi::c_void,
        attr_size: u32,
    ) -> i32;
}

#[link(name = "uxtheme")]
unsafe extern "system" {
    fn SetWindowTheme(hwnd: isize, psz_sub_app_name: *const u16, psz_sub_id_list: *const u16) -> i32;
}

/// 只设 DWM 深色属性，不调 refresh_titlebar（避免 SWP_FRAMECHANGED 重置 hover 跟踪）。
/// 再调 SetWindowTheme 把主题名钉回 DarkMode_Explorer：winit 深浅切换只会
/// SetWindowTheme(hwnd, L"", …) 复位主题名，纯 DwmSetWindowAttribute 压不住它；
/// Win11 额外把标题栏底色钉成纯黑、文字纯白（Win10 不支持这两个属性，静默失败）。
#[cfg(target_os = "windows")]
fn set_dwm_dark(hwnd: isize) {
    if hwnd == 0 { return; }
    let value: i32 = 1;
    unsafe {
        for attr in [20u32, 19u32] {
            let ok = DwmSetWindowAttribute(
                hwnd,
                attr,
                &value as *const i32 as *const std::ffi::c_void,
                4,
            );
            if ok >= 0 { break; }
        }
    }
    unsafe {
        // Win11：标题栏底色纯黑、文字纯白（属性 35/36；Win10 不支持，返回失败忽略）。
        let black: u32 = 0x0000_0000;
        let white: u32 = 0x00FF_FFFF;
        DwmSetWindowAttribute(hwnd, 35, &black as *const u32 as *const std::ffi::c_void, 4);
        DwmSetWindowAttribute(hwnd, 36, &white as *const u32 as *const std::ffi::c_void, 4);
        // 主题名钉回深色：winit 的 SetTheme(Light) 只 SetWindowTheme(hwnd, L"", …)。
        let dark_name = "DarkMode_Explorer\0".encode_utf16().collect::<Vec<u16>>();
        SetWindowTheme(hwnd, dark_name.as_ptr(), std::ptr::null());
    }
}

/// 设 DWM 深色 + 强制重绘标题栏（SWP_FRAMECHANGED）。仅初始化时调一次。
#[cfg(target_os = "windows")]
fn set_titlebar_theme(hwnd: isize) {
    if hwnd == 0 { return; }
    set_dwm_dark(hwnd);
    unsafe { refresh_titlebar(hwnd); }
}

/// 强制重绘标题栏非客户区：SWP_FRAMECHANGED 让 DWM 重新布局非客户区
/// （触发 WM_NCCALCSIZE），RedrawWindow 立即重绘帧。
/// 注意：每次调用会重置 winit 的 hover 跟踪，导致「鼠标悬停激活窗口」失效，
/// 因此只在初始化时调用，不在每帧轮询中调用。
#[cfg(target_os = "windows")]
unsafe fn refresh_titlebar(hwnd: isize) {
    unsafe extern "system" {
        fn SetWindowPos(
            hwnd: isize,
            hwnd_insert_after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            uflags: u32,
        ) -> i32;
        fn RedrawWindow(
            hwnd: isize,
            lprc_update: *const std::ffi::c_void,
            hrgn_update: isize,
            uflags: u32,
        ) -> i32;
    }
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    const RDW_FRAME: u32 = 0x0400;
    const RDW_INVALIDATE: u32 = 0x0001;
    const RDW_UPDATENOW: u32 = 0x0100;
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        RedrawWindow(hwnd, std::ptr::null(), 0, RDW_FRAME | RDW_INVALIDATE | RDW_UPDATENOW);
    }
}

/// 深浅主题下可读的提示色。
fn ui_warn(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(220, 170, 60)
    } else {
        Color32::from_rgb(160, 125, 15)
    }
}

/// 查询 Windows 注册表获取系统 AppsUseLightTheme 值：
/// 0 = 深色，1 = 浅色。egui 的 system_theme() 依赖 WM_SETTINGCHANGE 消息，
/// 窗口未激活/消息丢失时返回 None，导致跟随系统主题检测失效。
/// 直接读注册表更可靠。
#[cfg(target_os = "windows")]
fn query_windows_dark_mode() -> bool {
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            hkey: isize,
            lp_subkey: *const u16,
            ul_options: u32,
            sam_desired: u32,
            phk_result: *mut isize,
        ) -> i32;
        fn RegQueryValueExW(
            hkey: isize,
            lp_value_name: *const u16,
            lp_reserved: *mut u32,
            lp_type: *mut u32,
            lp_data: *mut u8,
            lpcb_data: *mut u32,
        ) -> i32;
        fn RegCloseKey(hkey: isize) -> i32;
    }
    const HKEY_CURRENT_USER: isize = 0x80000001;
    const KEY_READ: u32 = 0x20019;
    // Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme
    let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = "AppsUseLightTheme"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut hkey: isize = 0;
    let hr = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, key_path.as_ptr(), 0, KEY_READ, &mut hkey)
    };
    if hr != 0 {
        return true; // 打不开默认深色
    }
    let mut data_type: u32 = 0;
    let mut data: u32 = 0;
    let mut data_size: u32 = 4;
    let hr = unsafe {
        RegQueryValueExW(
            hkey,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut data_type,
            &mut data as *mut u32 as *mut u8,
            &mut data_size,
        )
    };
    unsafe { RegCloseKey(hkey); }
    if hr != 0 {
        return true; // 读取失败默认深色
    }
    data == 0 // AppsUseLightTheme=0 → 深色
}

fn ui_ok(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(90, 200, 90)
    } else {
        Color32::from_rgb(35, 150, 45)
    }
}

fn ui_gray(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::GRAY
    } else {
        Color32::from_gray(90)
    }
}

/// 渲染时强制过一遍颜色：按实际背景亮度差验证前景色，
/// 对比不足（含真假同色）时翻成黑/白兑底，保证文字与背景永不相同。
/// 量一段文字的宽度（与 egui 排版一致：正文号、不换行）。状态栏做宽度预算
/// 时用它把右侧固定簇/左侧按钮的真实占宽算出来。
pub fn text_width(ui: &egui::Ui, text: &str) -> f32 {
    let font = ui.style().text_styles[&egui::TextStyle::Body].clone();
    ui.painter()
        .layout(text.to_owned(), font, egui::Color32::PLACEHOLDER, f32::INFINITY)
        .size()
        .x
}

/// 量一个**文本按钮**的宽度：正文号 + 框内边距。依据 egui 的
/// `Style::button_style`：按钮的 fallback 字号就是 TextStyle::Body，
/// inner_margin = button_padding + expansion - bg_stroke.width（左右各一份）。
pub fn button_text_width(ui: &egui::Ui, label: &str) -> f32 {
    let v = &ui.visuals().widgets.inactive;
    let pad = (ui.spacing().button_padding.x + v.expansion - v.bg_stroke.width).max(0.0);
    text_width(ui, label) + 2.0 * pad + v.bg_stroke.width
}

/// 状态栏消息该占多宽（纯函数，便于单测）。
///
/// `avail` 是整行行宽、`left_w` 是扣掉右侧固定簇后左段真正能用的宽度、
/// `buttons_w` 是待办按钮（自更新 / 工具入口 / 重启）的实测总宽。规则：
/// 封顶行宽的 45%（下限 120px，窗口极窄就全给消息），再扣掉按钮实际占的宽度；
/// 剩下的不足 MIN_MSG_W 就**整条消息不画**（返回 0）—— 此时按钮已经放不下，
/// 由外层横向滚动区出滚动条兜底，消息不能没有而按钮被藏起来。
pub fn status_msg_width(avail: f32, left_w: f32, buttons_w: f32, gap: f32) -> f32 {
    const MIN_MSG_W: f32 = 40.0; // 再窄这条消息就没信息量了
    let cap = if avail < 80.0 { avail } else { (avail * 0.45).max(120.0) };
    let free = left_w - buttons_w - gap;
    if free < MIN_MSG_W {
        0.0
    } else {
        cap.min(free)
    }
}

/// 渲染时强制过一遍颜色：按实际背景亮度差验证前景色，
/// 对比不足（含真假同色）时翻成黑/白兑底，保证文字与背景永不相同。
fn forced_contrast_color(fg: Color32, bg: Color32) -> Color32 {
    let lum = |c: Color32| 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    if (lum(fg) - lum(bg)).abs() < 60.0 {
        if lum(bg) > 128.0 {
            Color32::BLACK
        } else {
            Color32::WHITE
        }
    } else {
        fg
    }
}

/// 网络诊断日志：保留调用点但不再落盘（用户要求移除写入 update.log 的逻辑，
/// 除错信息不对外写文件；误用时改回带时间戳追加即可）。
#[allow(unused_variables)]
fn log_update(_msg: &str) {}

/// 取 curl 可执行文件：Windows 钉死系统版（C:\Windows\System32\curl.exe）。
/// PATH 里常见的 MSYS2/mingw 版 curl 依赖运行时 DLL，由 GUI 进程裸 spawn 时
/// 启动即崩（ACCESS_VIOLATION，rc=0xC0000005，本机实测）→ 所有 curl 源集体
/// 失败、检查更新/下载"链接都访问不上"。System32 版静态依赖 Schannel，任意
/// 机器可控；精简系统无此文件时回退 PATH 查找。非 Windows 直接用 curl。
#[cfg(windows)]
fn curl_bin() -> &'static str {
    const SYS_CURL: &str = "C:\\Windows\\System32\\curl.exe";
    if std::path::Path::new(SYS_CURL).exists() {
        SYS_CURL
    } else {
        "curl"
    }
}
#[cfg(not(windows))]
fn curl_bin() -> &'static str {
    "curl"
}

/// 强制直连：给子进程挂上「不走任何代理」的全部开关。
///
/// 三层保险（用户要求：下载时不带任何代理）：
///  1. `--noproxy '*'`：curl 侧通配直连（curlrc 已被 `-q` 禁掉，这里再挡一层）；
///  2. `env_remove` 清掉 http_proxy/https_proxy/all_proxy（含大写）：环境变量
///     这条道也堵死，将来换传输层也不会凭空捡起代理；
///  3. PS 通道在脚本里置 `[Net.WebRequest]::DefaultWebProxy = $null`
///     （Invoke-WebRequest 走系统代理，WinPS 没有 `--noproxy` 那种开关）。
///
/// 为什么全禁而不是「有代理更好」：镜像池 10 条链若都被同一个代理端口串起来
/// 限速，会一起跌破 `--speed-limit 4096 --speed-time 8` 的速度地板 → 集体被判
/// 死，复现「所有下载源下载失败」；本机也确有 Clash 残留配置的病史。
fn direct(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd.arg("--noproxy").arg("*");
    for k in [
        "http_proxy", "https_proxy", "all_proxy", "ftp_proxy", "no_proxy", "HTTP_PROXY",
        "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
    ] {
        cmd.env_remove(k);
    }
    cmd
}

/// 用 curl 请求 URL 并把响应体取回内存（自更新与工具资产表探查共用）。
/// 通用性说明：`-q` 让 curl 完全不读 ~/.curlrc（曾有残留 Clash 127.0.0.1:7897
/// 代理配置导致所有 curl 走指定端口、检查更新一律网络错误），且 `direct()` 把
/// 环境/系统代理一并清掉——**任何机器上的任何代理配置都不参与**。connect_timeout
/// / max_time（秒）由调用方决定。
fn curl_get(url: &str, connect_timeout: u64, max_time: u64) -> Result<Vec<u8>, String> {
    let mut cmd = std::process::Command::new(curl_bin());
    let ct = connect_timeout.to_string();
    let mt = max_time.to_string();
    cmd.args([
        // -q 忽略 .curlrc / _curlrc，防用户机器上的残留代理端口
        "-q", "-s", "-f", "-L", "--connect-timeout", &ct, "--max-time", &mt, "--ssl-no-revoke",
        "-H", "User-Agent: TUIProjectManager",
    ]);
    direct(&mut cmd);
    cmd.arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output().map_err(|e| format!("启动 curl 失败: {e}"))?;
    if !output.status.success() {
        return Err(format!("HTTP {}", output.status.code().unwrap_or(0)));
    }
    Ok(output.stdout)
}

/// GitHub HTML 302：只取重定向 URL（`-w %{redirect_url}`）原样返回，
/// 从里面挖 /releases/tag/<tag> 的活儿交给 parse_tag。同样 -q、不读代理端口。
fn fetch_tag_html(url: &str, connect_timeout: u64, max_time: u64) -> Result<String, String> {
    let mut cmd = std::process::Command::new(curl_bin());
    let ct = connect_timeout.to_string();
    let mt = max_time.to_string();
    cmd.args([
        "-q", "-s", "-f",
        "-o", "NUL", // 丢弃响应体，只要重定向头
        "-w", "%{redirect_url}",
        "--connect-timeout", &ct, "--max-time", &mt, "--ssl-no-revoke",
        "-H", "User-Agent: TUIProjectManager",
    ]);
    direct(&mut cmd);
    cmd.arg(url);
    // GUI 程序 spawn 控制台程序（curl.exe）会闪一个黑窗口：
    // CREATE_NO_WINDOW 让子进程不分配控制台，彻底消除。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let o = cmd.output().map_err(|e| format!("启动 curl 失败: {e}"))?;
    if !o.status.success() {
        return Err(format!("HTTP {}", o.status.code().unwrap_or(0)));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// 跑一段 PowerShell 并把 stdout 取回（redirect 时 PS 输出编码默认 GBK，ASCII
/// 无差异，仍显式钉死 UTF-8 防意外）。CreationFlags CREATE_NO_WINDOW 不闪黑
/// 窗，与 curl 路径一致。
#[cfg(windows)]
fn ps_run(script: &str) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell")
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(script)
        .output()
        .map_err(|e| format!("启动 PowerShell 失败: {e}"))?;
    if !out.status.success() {
        return Err(format!("PowerShell 退出码 {}", out.status.code().unwrap_or(0)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// PowerShell Invoke-WebRequest 取回任意 URL 的正文（独立网络栈 WinHTTP/Schannel，
/// 吃系统代理——Steam++/加速器系统代理模式可救直连被墙；不设 DefaultWebProxy=$null）。
/// curl 在个别目标机上会 ACCESS_VIOLATION 启动即崩，PS 通道是「换个办法绕过」的
/// 主力源。tag 探查与资产表探查共用它，取回来的正文由调用方的 parse 决定怎么用。
#[cfg(windows)]
fn ps_fetch_body(url: &str, timeout_secs: u64) -> Result<String, String> {
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         $ErrorActionPreference='Stop'; \
         (Invoke-WebRequest -Uri '{url}' -Headers @{{'User-Agent'='TUIProjectManager'}} -TimeoutSec {t} -UseBasicParsing).Content",
        url = url,
        t = timeout_secs,
    );
    ps_run(&script)
}

/// 本软件自身的 GitHub 仓库（检查自身更新用）。
const SELF_REPO: &str = "qq458249269/TUIProjectManager";

/// 本软件自身 Release 的页面前缀（资产片段 / 标签页 URL 拼在这里）。
const SELF_REPO_PAGE: &str = "https://github.com/qq458249269/TUIProjectManager";

/// 探查源的**正文形态**：既决定「怎么把正文取回来」，也决定「正文里挖什么」。
///
/// 这套枚举 + 下面的 Probe/probe_first 就是「检查更新」的镜像源下载逻辑本身，
/// pi / opencode 的资产表探查直接复用它（同一份 GH_MIRRORS、同一份 PS 通道、
/// 同一套「逐个尝试」调度），不再各写一串只直连 api.github.com 的取数逻辑。
#[derive(Clone, Copy)]
enum ProbeKind {
    /// JSON 响应体：解析后由 extract 取字段（GitHub API 取 tag_name）。
    Json(fn(&serde_json::Value) -> Option<String>),
    /// GitHub HTML 302：curl -L 后只取重定向头（`-w %{redirect_url}`），不下载
    /// 正文，免 API 限流。
    HtmlTag,
    /// HTML 正文原样取回（expanded_assets 资产列表片段）。
    Html,
    /// JSON **数组**响应体（GitHub `/releases` 发布列表，取回正文后由 parse
    /// 逐个 release 读 tag + assets）。取数通道与 Json 完全相同，区别只在于
    /// 顶层是数组而不是对象——单列一档是为了让调用点的语义自洽（配 Json 会
    /// 让人以为能直接取 `tag_name`）。
    JsonList,
}

/// 一个探查源。desc/URL/通道/超时档位都在这里配，probe_first 负责逐个调度。
/// repo/tag 挂在源上（parse 是 fn 指针、捕获不了局部变量，由解析方自行取用）。
struct Probe {
    desc: String,
    url: String,
    kind: ProbeKind,
    /// 走 PowerShell WinHTTP（独立网络栈 + 系统代理），curl 崩溃/被墙时绕过。
    ps: bool,
    /// 属 GH_MIRRORS 加速池（挂了零成本的一档）：串行下可按 PROBE_POOL_BUDGET
    /// 提前跳过剩下的池内源，直连/PS 通道则永远试到底。
    pool: bool,
    /// 连接超时 / 总超时（秒）。
    ct: u64,
    mt: u64,
    /// 本次探查的仓库与标签（解析正文时按需取用；tag 探查不关心）。
    repo: String,
    tag: String,
}

impl Probe {
    fn new(desc: &str, url: String, kind: ProbeKind, ct: u64, mt: u64) -> Self {
        Self {
            desc: desc.to_string(),
            url,
            kind,
            ps: false,
            pool: false,
            ct,
            mt,
            repo: String::new(),
            tag: String::new(),
        }
    }

    /// 同一 URL 追加一条走 PS WinHTTP 的源（curl 崩/被墙时的独立网络栈兜底）。
    fn via_ps(mut self) -> Self {
        self.ps = true;
        self
    }

    /// 标为「加速池」源：逐个尝试时可以按预算提前跳过（见 probe_first）。
    fn pool(mut self) -> Self {
        self.pool = true;
        self
    }

    /// 标上本次探查的 repo / tag，供 parse 从正文里定位资产。
    fn ctx(mut self, repo: &str, tag: &str) -> Self {
        self.repo = repo.to_string();
        self.tag = tag.to_string();
        self
    }
}

/// 按 kind 取回单个源的正文（已归一：JSON/HTML 给原文，HtmlTag 给重定向 URL）。
/// 具体「从正文里取值」交给 probe_first 的 parse（fn 指针，取数与解析分层）。
fn probe_body(p: &Probe) -> Result<String, String> {
    #[cfg(windows)]
    if p.ps {
        return ps_fetch_body(&p.url, p.mt);
    }
    match p.kind {
        ProbeKind::HtmlTag => fetch_tag_html(&p.url, p.ct, p.mt),
        ProbeKind::Json(_) | ProbeKind::Html | ProbeKind::JsonList => {
            let b = curl_get(&p.url, p.ct, p.mt)?;
            Ok(String::from_utf8_lossy(&b).into_owned())
        }
    }
}

/// 拉取某个 repo 的最新 tag 的源表：GH_MIRRORS 国内镜像（每个前缀拼在
/// api.github.com 直链前）、GitHub HTML 302（免 API 限流）、GitHub API
/// （可能限流）以及 PS WinHTTP 通道（curl 崩溃/失败时的绕过源）。
/// **这里曾经有一个 jsDelivr 源（`data.jsdelivr.com/v1/packages/gh/{repo}`），
/// 已删除——它答的根本不是同一个问题。** 该接口返回的是该仓库**发布到 npm 的
/// 包版本**，与 GitHub Release 的 tag 是两套编号，混用会直接把下载链打死：
///  - opencode：npm 包（`opencode-ai`）最新版是 `2.0.20`，而 Release tag 是
///    `v1.18.33`。`2.0.20` 这个 tag 根本不存在，拼出的直链
///    `.../releases/download/2.0.20/opencode-windows-x64.zip` 必然 404；
///  - pi：npm 版本 `0.99.1` 被剥掉了 `v` 前缀，真实 tag 是 `v0.99.1`，
///    直链同样 404。
/// 附带伤害：错 tag 还会喂给 `version_newer`（`2.0.20 > 1.18.32`），于是装完
/// 也永远显示「有新版本」，下载按钮反复冒出来。
/// jsDelivr 还是个抢跑很快的 CDN，一旦被排在前面就**总是它先赢**，其他源根本没机会纠正。
///
/// 现在这里只放「以 GitHub Release 为准」的源；万一将来又混进答非所问的源，
/// `download_tool_archive` 还会用发布列表兜底（见 pick_release_tag_with_assets），
/// 错 tag 会被就地纠正成「真正带产物的那个 tag」，而不是 5 次重试白烧 300MB。
///
/// 镜像一律 `.pool()` 标记：逐个尝试时它们是「快赢的一档」，预算用尽就跳过
/// 剩下的，把时间留给直连/PS 通道（见 probe_first）。
fn tag_probes(repo: &str) -> Vec<Probe> {
    let api_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let html_url = format!("https://github.com/{repo}/releases/latest");

    let gh_api: fn(&serde_json::Value) -> Option<String> =
        |v| v["tag_name"].as_str().map(str::to_string);
    let mut sources: Vec<Probe> = Vec::new();
    for m in GH_MIRRORS {
        sources.push(
            Probe::new(m, format!("{m}{api_url}"), ProbeKind::Json(gh_api), 3, 6).pool(),
        );
    }
    sources.push(Probe::new("GitHub HTML", html_url.clone(), ProbeKind::HtmlTag, 6, 12));
    sources.push(Probe::new("GitHub API", api_url.clone(), ProbeKind::Json(gh_api), 6, 12));
    // PowerShell 通道（独立 WinHTTP 网络栈，吃系统代理）：curl 失败/崩溃时仍可
    // 检查更新。实测本机直连 api.github.com 1.1s 可达；不读任何代理端口。
    #[cfg(windows)]
    {
        sources.push(Probe::new("PS API", api_url, ProbeKind::Json(gh_api), 8, 15).via_ps());
        sources.push(Probe::new("PS HTML", html_url, ProbeKind::HtmlTag, 8, 15).via_ps());
    }
    #[cfg(not(windows))]
    let _ = (api_url, html_url);
    sources
}

/// tag 是否长得像 GitHub Release 的 tag：首字符是字母/数字，其余只允许
/// 字母数字与 `.` `-` `_` `+`，长度 ≤ 64（`v1.18.33` / `1.0.0-rc.1` 都过）。
///
/// 这是 tag 解析的**最后一道闸**：解析器一旦咬到 HTML 模板、错误页或某个
/// 答非所问的源，凭形状就能拒掉，不必把垃圾 tag 放行进下载链。
fn is_plausible_tag(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars().all(|c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

/// 从 HTML 里挖 tag。两种形态都含 `/releases/tag/<tag>`：
///  - curl 的 HtmlTag 源给的是**重定向 URL**（整段就是那一个链接）；
///  - PS 的 HtmlTag 源给的是**标签页整页正文**。
///
/// **旧实现只取正文里第一个 `/releases/tag/`，在标签页正文上必然咬错。**
/// GitHub 是 React 页，`<head>` 里有一行路由元数据
/// `<meta name="route-pattern" content="/:user_id/:repository/releases/tag/*name">`
/// ——它排在真正的 tag 链接**前面**，于是 `find` 命中它，抠出来的「tag」是
/// `/*name" data-turbo-transient>` 这种 HTML 碎片。它照样非空、能过
/// 「返回空 tag」检查、照样赢下探查（PS 通道 8s 连通时，镜像往往还没回来），
/// 于是状态栏顶上冒出一个鬼版本号，点下去拼出的
/// `…/releases/download/*name" data-turbo-transient>/opencode-windows-x64.zip`
/// 必然 404——**正是「检测出一个乱七八糟的版本、点下载却全挂」这类事故的形状**。
///
/// 改成：按出现顺序扫全部 `/releases/tag/`，逐个取到分隔符为止的片段，用
/// `is_plausible_tag` 过滤，第一个**合法**的即答案（真实 tag 紧随其后）；
/// 一个都没有就判失败，让这一源安静退场（别的源还在跑）。标签页正文里
/// 13 处 `/releases/tag/` 依次是 `*name`（模板）、`v1.18.33`×6（真 tag）、
/// `v1.18.33&quot;,...`（内嵌 JSON，被 `;` 截断后仍不合法）——只有第 2 类
/// 能过闸。
fn parse_tag_from_html(body: &str) -> Result<String, String> {
    const NEEDLE: &str = "/releases/tag/";
    let stop = |c: char| {
        c == '\'' || c == '"' || c == '<' || c == '>' || c == '?' || c == '#' || c == '/'
            || c == '\\' || c.is_whitespace() || c == ';'
    };
    let mut from = 0usize;
    while let Some(rel) = body[from..].find(NEEDLE) {
        let at = from + rel;
        let start = at + NEEDLE.len();
        let end = body[start..]
            .find(stop)
            .map(|i| start + i)
            .unwrap_or(body.len());
        let cand = body[start..end].trim();
        if is_plausible_tag(cand) {
            return Ok(cand.to_string());
        }
        from = start;
    }
    Err(format!(
        "HTML 里没有合法的 /releases/tag/ 链接（正文 {} 字节）",
        body.len()
    ))
}

/// 从探查正文里挖 tag（JSON 取 tag_name；HTML 走上面的多候选 + 合法性过滤）。
fn parse_tag(p: &Probe, body: &str) -> Result<String, String> {
    let tag = match &p.kind {
        ProbeKind::Json(f) => {
            let v: serde_json::Value =
                serde_json::from_str(body).map_err(|_| "无法解析 JSON 响应".to_string())?;
            f(&v).ok_or_else(|| "返回结构不符合预期".to_string())?
        }
        ProbeKind::HtmlTag | ProbeKind::Html => parse_tag_from_html(body)?,
        // 发布列表不是「一个 release 的 tag」，由 parse_release_list 单独处理。
        ProbeKind::JsonList => return Err("发布列表不该走 tag 解析".to_string()),
    };
    let tag = tag.trim().to_string();
    if tag.is_empty() {
        return Err("返回空 tag".to_string());
    }
    // 兜底闸：JSON 源同样可能被换成答非所问的实现（历史上就混进过一个返回
    // npm 包版本的源），形状不对就地拒掉，绝不带着它去拼直链。
    if !is_plausible_tag(&tag) {
        return Err(format!("tag 不合法: {tag:?}"));
    }
    Ok(tag)
}

/// 拉取某个 repo 的最新 tag。所有源**逐个**探查、先成功先得：任一源在自身超时内
/// 返回有效 tag 即胜出，坏源安静退场（每个源只花自己那一份超时），全程 -q 直连、
/// 不读任何代理配置与端口。
///
/// **从并发改成逐个**（事故：「所有下载源下载失败」）。旧的并发同时打 8 个镜像 +
/// GitHub API + PS 通道 ≈ 10 条请求，一次检查把同一份 API 文档同时问十遍：
/// api.github.com 匿名调用是**按 IP 每小时 60 次**限流的，镜像前缀代理转发的
/// 请求同样从限流池里扣，撞到 403/429 就整片源同时躺平；何况并发时**谁先回谁赢**，
/// 一个答非所问的源照样能抢跑。现在一个一个来，谁先成用谁，请求数降到个位数，
/// 限流基本碰不到；代价是全挂时要多花几份超时，由 PROBE_POOL_BUDGET 兜住。
fn fetch_latest_tag(repo: &str) -> Result<String, String> {
    let (_desc, tag) = probe_first(repo, tag_probes(repo), parse_tag)?;
    log_update(&format!("检查更新 {repo} 最新 tag: {tag}"));
    Ok(tag)
}

/// 所有源**逐个**探查、先成功先得：按源表顺序一个一个试，任一源取回正文且
/// parse 成功即胜出，剩下的不再取数（全败时逐条记日志）。返回 (胜出源名, 值)。
///
/// 这是「检查更新」与「工具下载」共用的取数骨架：GH_MIRRORS 前缀镜像、
/// GitHub 直连、PowerShell WinHTTP 三类通道在这里统一调度，新增数据源只需
/// 往源表里加一条。parse 是 fn 指针（不捕获局部变量）。
///
/// **不再一源一线程同时开跑**（见 fetch_latest_tag 的事故说明）：并发探查把
/// 同一份文档同时问十遍，最容易撞 api.github.com 的匿名 60 次/小时限流，
/// 而且「谁先回谁赢」会让答非所问的源抢跑。逐个试把请求数压到个位数，代价是
/// 全挂时的耗时——用下面的「加速池预算」兜住。
///
/// **加速池预算**：源表顺序 = 优先级（国内加速 → 直连 → PS），前 8 条镜像标了
/// `.pool()`；累计耗时超过 PROBE_POOL_BUDGET 秒就放弃**剩下的**池内源（它们
/// 挂了也不会再活），直连/PS 通道则一定试到底。正常情况下第一个源就赢，
/// 根本走不到这条分支。
const PROBE_POOL_BUDGET: u64 = 12; // 秒

fn probe_first<T>(
    ctx: &str,
    sources: Vec<Probe>,
    parse: fn(&Probe, &str) -> Result<T, String>,
) -> Result<(String, T), String> {
    if sources.is_empty() {
        return Err("没有可用源".to_string());
    }
    let start = Instant::now();
    let mut errors: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    for s in sources {
        if s.pool && start.elapsed().as_secs() >= PROBE_POOL_BUDGET {
            skipped += 1;
            continue;
        }
        match probe_body(&s).and_then(|body| parse(&s, &body)) {
            Ok(v) => {
                log_update(&format!(
                    "探查 {ctx} {} 成功（{:.1}s）",
                    s.desc,
                    start.elapsed().as_secs_f64()
                ));
                return Ok((s.desc, v));
            }
            Err(e) => {
                log_update(&format!(
                    "探查 {ctx} {} 失败（{:.1}s）: {e}",
                    s.desc,
                    start.elapsed().as_secs_f64()
                ));
                errors.push(e);
            }
        }
    }
    if skipped > 0 {
        log_update(&format!(
            "探查 {ctx} 加速池预算用尽（{PROBE_POOL_BUDGET}s），跳过剩余 {skipped} 个镜像源"
        ));
    }
    // 具体失败原因已逐条 log_update；这里只给用户一句可行动的提示
    //（逐源错误已写日志，展开只会把状态栏撑成一条长串）。
    log_update(&format!("探查 {ctx} 全部源失败: {}", errors.join("；")));
    Err("网络错误（镜像与直连源均失败，请检查网络连接或加速工具如 Steam++）".to_string())
}

/// 拉取本软件自身最新版本号，生成状态栏消息 + 有新版本时的 tag。
fn fetch_latest_release() -> (String, Option<String>) {
    match fetch_latest_tag(SELF_REPO) {
        Ok(tag) => {
            let msg = version_message(&tag);
            if msg.contains("发现新版本") {
                (msg, Some(tag))
            } else {
                (msg, None)
            }
        }
        Err(e) => (format!("检查更新失败：{e}"), None),
    }
}

/// 根据 tag 与本地版本比较生成状态栏消息。
fn version_message(tag: &str) -> String {
    let latest = tag.trim_start_matches('v');
    if version_newer(latest, crate::app_version()) {
        // 「点这条」= 跳设置页顶部的更新横幅（下载按钮早搬到那儿了，原文案说
        // “点击下方按钮”已经指不到任何东西）。
        format!("发现新版本 {tag}，点这条跳设置页下载")
    } else {
        format!("已是最新版本 ({tag})")
    }
}

/// 下载 GitHub Release 最新版本的 exe 到指定目录，实时报告进度。
/// 返回 Ok(下载文件路径) 或 Err(错误信息)。
fn download_update(
    tag: &str,
    dest_dir: &Path,
    progress_tx: std::sync::mpsc::Sender<(u64, u64)>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<String, String> {
    // 从 GitHub Release 里找 exe 直链。优先级：expanded_assets 资产片段
    // （github.com CDN、无限流、比 api.github.com 更易连通，且真的带下载链接）
    // → 标签页 HTML（有些镜像/缓存会直接把它渲染出来）→ 直拼直链（零请求保底）
    // → GitHub API 最低（仅前面都没拿到时兜底，成功可补字节数）。
    // 旧实现 API 优先：限流/被墙时每次下载都先撞 API 失败（HTTP 22），错误
    // 汇总里「源① API」长期打头误导；现在 API 降为最低优先级，curl 带 -f
    // 失败时透出真实报错，HTML 兜底成功则照常下载。
    let mut exe_info: Option<ExeAsset> = None;
    let mut api_err: Option<String> = None;
    // 源②主路径：取 expanded_assets 片段（标签页初始 HTML 的资产列表是
    // lazy-load 的 include-fragment，一个下载链接都没有，旧实现据此判定
    // 「HTML 必失败」——直接取那个片段才是有效源）。
    // 复用与「检查更新 / 工具下载」同一套镜像源探查：以前这段只裸 curl 直连
    // github.com，被墙时先白等 2×8s 再退到直拼直链，白白卡十几秒。
    // ponytail: 要百分比进度可另发一次 HEAD 取 Content-Length，或 API 仅补 size。
    match fetch_self_exe_asset(tag) {
        Ok((src, info)) => {
            log_update(&format!(
                "下载 源② HTML 命中（{src}）：{} ← {}",
                info.url, info.name
            ));
            exe_info = Some(info);
        }
        Err(e) => log_update(&format!("下载 源② HTML 全部源失败（转 API/直拼）：{e}")),
    }
    // 源①（已降为最低优先级）：仅 HTML 拿不到时才问 API，成功可补字节数。
    // 限流/被墙（curl HTTP 22）时此失败不再最先发生、不打头进错误汇总。
    if exe_info.is_none() {
        let api_url = format!(
            "https://api.github.com/repos/qq458249269/TUIProjectManager/releases/tags/{tag}"
        );
        let mut api_cmd = std::process::Command::new(curl_bin());
        api_cmd.args([
            "-q", "-s", "-f", "--connect-timeout", "8", "--ssl-no-revoke",
            "-H", "User-Agent: TUIProjectManager",
        ]);
        direct(&mut api_cmd);
        api_cmd.arg(&api_url);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            api_cmd.creation_flags(0x08000000);
        }
        match api_cmd.output() {
            Ok(o) if o.status.success() => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    for a in v["assets"].as_array().into_iter().flatten() {
                        let Some(name) = a["name"].as_str() else { continue };
                        if !name.ends_with(".exe") {
                            continue;
                        }
                        let Some(url) = a["browser_download_url"].as_str() else { continue };
                        exe_info = Some(ExeAsset {
                            url: url.to_string(),
                            name: name.to_string(),
                            size: a["size"].as_u64().unwrap_or(0),
                        });
                        break;
                    }
                }
                if exe_info.is_none() {
                    api_err = Some("GitHub API 响应中没有 exe 资产".to_string());
                    log_update("下载 源① API 成功但无 exe 资产");
                }
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stderr = stderr.trim();
                api_err = Some(if stderr.is_empty() {
                    format!("GitHub API 请求失败（HTTP {}）", o.status.code().unwrap_or(0))
                } else {
                    format!("GitHub API 请求失败：{stderr}")
                });
                log_update(&format!(
                    "下载 源① API HTTP {}：{stderr}",
                    o.status.code().unwrap_or(0)
                ));
            }
            Err(e) => {
                log_update(&format!("下载 源① API: 启动 curl 失败: {e}"));
                api_err = Some(format!("启动 curl 失败: {e}"));
            }
        }
    }
    // 源③保底：直拼直链，零网络请求，永远可用。GitHub 真实产物名是
    // tui-project-manager.exe（小写连字符，2026-09 实测一直如此）；历史曾用
    // TUIProjectManager.exe。两个候选都试，不依赖任何 API/页面解析。
    // HTML 已取的 info（真实文件名）优先。
    let (fallback, total) = match exe_info {
        Some(a) => (vec![(a.url, a.name)], a.size),
        None => (
            ["tui-project-manager.exe", "TUIProjectManager.exe"]
                .iter()
                .map(|f| {
                    (
                        format!(
                            "https://github.com/qq458249269/TUIProjectManager/releases/download/{tag}/{f}"
                        ),
                        f.to_string(),
                    )
                })
                .collect(),
            0,
        ),
    };
    // ── 候选下载链：国内镜像 + PS WinHTTP + curl 直链，每个文件名下逐个尝试 ──
    //（见 download_chain）：一次只跑一条，坏源/停滞源花光自己的超时退场，下一条
    // 接着上。单个 fallback 失败再试下一个候选文件名。
    let mut parts: Vec<String> = Vec::new();
    for (url, name) in fallback {
        let candidates = candidate_chains(&url);
        log_update(&format!(
            "下载 {} 逐个尝试 {} 个候选链（tag={tag}，total={total}）",
            name,
            candidates.len()
        ));
        // 用户取消时立即停止本轮尝试。
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("下载已取消".to_string());
        }
        match download_chain(
            &name,
            // 分片指纹带 tag：不同版本的 exe 分片不得互相续传（否则会把旧版
            // 前半截接到新版后半截上，拼出一个能过 PE 头校验却跑不起来的文件）。
            &shard_fp(tag),
            candidates,
            total,
            dest_dir,
            &progress_tx,
            cancel,
            looks_like_exe,
        ) {
            Ok(p) => {
                log_update(&format!("下载 成功：{name} → {p}"));
                return Ok(p);
            }
            Err(e) => {
                log_update(&format!("下载 失败：{name}：{e}"));
                parts.push(format!("{name}: {e}"));
            }
        }
    }
    // API 失败的真实原因（限流 403 等）并入汇总，不再被直拼兜底掩盖。
    if let Some(e) = &api_err {
        parts.push(format!("源① API: {e}"));
    }
    Err(format!("所有下载源失败：{}", parts.join("；")))
}

/// GitHub Release 检查/下载加速镜像（前缀拼接原始 github.com 或
/// api.github.com 直链，如 {mirror}https://github.com/...）。大陆直连 GitHub
/// 慢/被墙，镜像 CDN 缓存、延迟低；检查更新时镜像逐个探查、谁先答对谁赢；
/// 下载则逐链尝试、挂了自动跳下一条，镜像全不行才走 PS 通道与原始直链。
/// 列表换成当前可用即可，多放几个零成本、坏节点自动跳过。
// 镜像列表是「加速前缀池」而非命运列表：检查更新/下载都按池顺序**逐个**试，
// 连接失败与坏文件（错误页/截断）都被即时剔除记错并轮到下一条——所以越多越稳，
// 坏节点只是多花自己那一份 connect 超时。池内包含踩点验证过数量级的常见国内
// 加速：热门前缀代理、jsDelivr CDN 之外的家 gh-proxy 系。个别历史 403/超时/
// 证书过期的源保留在池里：连通状态随时变化，哪家活了立即自动启用。
//
// **别再改回“一源一线程全部并发”**（事故：整轮提示「所有下载源下载失败」）。
// 一次检查/下载同时打 8 个镜像 + 直连 + PS ≈ 10 条并行请求 + 10 份镜像同时
// 拉同一个 60MB 文件：
//  1. api.github.com 匿名调用按 IP 限流 60 次/小时，一次扇出等于把配额一下
//     花光，403/429 让整片源同时躺平（检查更新与资产表都靠它）；
//  2. 带宽被 N 份平分，每份都可能跌破 `--speed-limit 4096 --speed-time 8`
//     的速度地板 → **全部候选在同一个判定下被判死**，正好就是「所有下载源
//     下载失败」；单源独享带宽时它下得飞快；
//  3. “谁先回谁赢”让一个答非所问 / 报错页的源最容易被当赢家。
// 现在一条一条来：请求与带宽都独占，第一个成功的即胜出。
const GH_MIRRORS: &[&str] = &[
    "https://gh-proxy.com/",   // 热门前缀代理（实测 3.8MB/s，最快）
    "https://ghfast.top/",     // gh-proxy 系，同量级（实测 3.8MB/s）
    "https://gh-proxy.net/",   // 同系备用：答 200 但常常是几百字节的报错页（靠产物校验剔掉）
    "https://ghps.cc/",        // 极速代理：会 302 到 56yy.com 的 HTML 页（同上，靠校验剔掉）
    "https://ghproxy.net/",    // ghproxy 系：能用但慢（实测 ~0.5MB/s，13MB 要 25s）
    // 已实测死链、已从池里移除（每条要白占一份 connect 超时，全挂时纯浪费 15s）：
    //   https://mirror.ghproxy.com/   连接超时 5s（老 ghproxy 系早已停运）
    //   https://gh.llkk.cc/           Recv failure: Connection was reset
    //   https://github.moeyy.xyz/     连接超时 5s
];

/// 下载分片的指纹（FNV-1a 64 → 8 位十六进制）：分片文件名里必须带上它。
///
/// **为什么**：分片文件原先只按 `{产物名}.c{i}.new` 命名，而产物名是常量
/// （`.pi-update.zip` / `tui-project-manager.exe`），于是不同 tag、不同资产
/// 名（opencode 的 avx2 版与 baseline 版）复用同一个分片，curl 的 `-C -`
/// 会把 A 包的前半截接上 B 包的后半截——拼出来的东西**尾部照样有
/// `PK\x05\x06`**（看起来合法），却是个谁也解不开/解出坏 exe 的包，还白烧
/// 一个 60MB 下载。指纹把「这批字节属于哪个 tag 的哪个资产」显式写进文件名，
/// 不同批次彻底隔离。
fn shard_fp(key: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    // 取高 32 位写进文件名：够区分版本/资产名，又不把临时文件名撑太长
    format!("{:08x}", (h >> 32) as u32)
}

/// 清掉同族产物名下**其它指纹**的残留分片（换 tag / 换资产名时上一批的半截
/// 文件）。这些文件既占磁盘（一个工具就可能压着几百 MB），又是下一轮 `-C -`
/// 把坏包续出来的原料。保留本轮指纹的分片（那是可用的断点续传进度）。
fn purge_other_shards(dest_dir: &Path, asset_name: &str, fp: &str) -> usize {
    let prefix = format!("{asset_name}.");
    let keep = format!("{prefix}{fp}.");
    let Ok(rd) = std::fs::read_dir(dest_dir) else {
        return 0;
    };
    let mut n = 0;
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        // 只动本族分片 / 未晋升的 .new，且必须不是本轮指纹的。
        if !name.starts_with(&prefix) || !name.ends_with(".new") || name.starts_with(&keep) {
            continue;
        }
        if let Some(p) = e.path().to_str() {
            if cleanup_file(p) {
                n += 1;
            }
        }
    }
    if n > 0 {
        log_update(&format!("清理旧分片 {n} 个（{asset_name}，非本轮 {fp}）"));
    }
    n
}

/// 文件字节数（拿不到按 0 算）。
fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// 一条候选下载链：一个资产直链 + 通道 + 是否属镜像加速池。
#[derive(Debug)]
struct Chain {
    /// 完整 URL（镜像前缀已拼好，或原始 github.com 直链）。
    url: String,
    /// 走 PowerShell WinHTTP（独立网络栈 + 系统代理），否则 curl 直连。
    ps: bool,
    /// 属 GH_MIRRORS 加速池：逐个尝试时可按 DL_POOL_BUDGET 提前跳过剩下的池内
    /// 链；直链与 PS 通道则一定试到底（它们是保底，不能被预算砍掉）。
    pool: bool,
}

/// 候选链的 connect 超时（秒）：镜像给短的（挂了要快速退场——逐个尝试下 8 个死
/// 镜像不能各占 8s），直链与 PS 给长的（最后的保底，值得多等一会儿）。
const POOL_CONNECT_TIMEOUT: &str = "5";
const BACKUP_CONNECT_TIMEOUT: &str = "8";

/// 把一条资产直链铺成候选链：国内镜像池（GH_MIRRORS 前缀）→ PS WinHTTP →
/// curl 直连。**顺序即优先级**，逐个尝试（见 download_chain）。
fn candidate_chains(url: &str) -> Vec<Chain> {
    let mut out: Vec<Chain> = GH_MIRRORS
        .iter()
        .map(|m| Chain {
            url: format!("{m}{url}"),
            ps: false,
            pool: true,
        })
        .collect();
    // PS/WinHTTP 通道吃系统代理（Steam++/Clash 系统代理模式能救直连被墙；不设
    // DefaultWebProxy=$null——远端曾禁代理导致下载不了）。
    #[cfg(windows)]
    out.push(Chain {
        url: url.to_string(),
        ps: true,
        pool: false,
    });
    // curl 直连兜底：Windows 下 PS 失败（无 PowerShell 等）时仍有它；非 Windows
    // 更是唯一通道。
    out.push(Chain {
        url: url.to_string(),
        ps: false,
        pool: false,
    });
    out
}

/// 下载的「加速池干等预算」（秒）：**连续**这么久没从任何镜像链上拿到新字节，
/// 就放弃剩下的镜像链，直接走 PS 通道与直链保底。串行化之后全挂时的耗时全靠
/// 它兜底（没有并发去「同时试」，8 个死镜像各吃一份 connect 超时会拖很久）。
///
/// 刻意按「干等」而不是总耗时计：某条镜像正下到一半（哪怕下了 40MB 才断流），
/// 说明链路本身是通的，此刻换链等于把已下的字节全扔掉重下一份；只有连不上、
/// 一字节都拿不到的镜像才该被预算砍掉。
const DL_POOL_BUDGET: u64 = 20;

/// 把某个候选链的单个 URL 下载到 dest_dir/{asset}.{fp}.c{i}.new，下完且过校验
/// 才 promote 为 {asset}.{fp}.new。返回 Ok(产物路径) 或 Err(具体失败原因)。
///
/// 同一份产物的候选链（国内镜像 × 8 → PS WinHTTP → curl 直连）**逐个尝试**，
/// 一次只跑一条：请求与带宽都独占它，第一个下成功且过校验的即胜出。不是并发——
/// 事故「所有下载源下载失败」正是并发扇出造成的（见 GH_MIRRORS 上方说明：
/// 同时打 10 条请求撞 API 限流；带宽被平分后每条都跌破速度地板，一起被判死）。
///
/// 每条链的护栏：
///  - `--connect-timeout`：镜像 5s、直链与 PS 8s，挡连接挂死；
///  - `--speed-limit 4096 --speed-time 8`：持续 <4KB/s 达 8s 的僵尸源判死，
///    不让它把整轮的时间都占了（独占带宽时正常镜像远在这个地板之上）；
///  - 字节数对账 + validate（自更新 = looks_like_exe，pi/opencode =
///    looks_like_zip）：错误页 / 截断文件永不胜出，坏分片当场删掉；
///  - 失败/取消**保留**分片，下次同一条链 `-C -` 续传；fp 带 tag + 资产名，
///    绝不跨版本串包；
///  - 换链时进度会回到新链的起点（分片按链隔离的必然结果），这是如实反映
///    当前这条链下了多少，不是卡死。
///
/// 返回 Ok(下载文件路径) 或 Err(所有候选链失败的聚合)。
fn download_chain(
    asset_name: &str,
    fp: &str,
    candidates: Vec<Chain>,
    total: u64,
    dest_dir: &Path,
    progress_tx: &std::sync::mpsc::Sender<(u64, u64)>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    validate: fn(&Path) -> bool,
) -> Result<String, String> {
    // 换批次前先清掉上一批的半截文件（别让它们占盘 + 续成坏包）。
    purge_other_shards(dest_dir, asset_name, fp);
    let new_name = format!("{asset_name}.{fp}.new");
    let dest_path = dest_dir.join(&new_name);
    let n = candidates.len();
    let mut errs: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    // 「干等」计时起点：只有连续拿不到字节才累加预算（见 DL_POOL_BUDGET）。
    let mut dry_since = Instant::now();
    let start = Instant::now();
    for (i, ch) in candidates.iter().enumerate() {
        // 取消：立刻停，不再起下一条链。
        if cancel.load(Ordering::Relaxed) {
            return Err("下载已取消".to_string());
        }
        // 镜像池预算：干等太久就别在镜像上耗着了，直链/PS 一定还会试。
        if ch.pool && dry_since.elapsed().as_secs() >= DL_POOL_BUDGET {
            skipped += 1;
            continue;
        }
        let tmp = dest_dir.join(format!("{asset_name}.{fp}.c{i}.new"));
        let tmp_str = tmp.to_str().unwrap_or("update.exe.new").replace('\'', "''");
        let ct = if ch.pool {
            POOL_CONNECT_TIMEOUT
        } else {
            BACKUP_CONNECT_TIMEOUT
        };
        let mut cmd = if ch.ps {
            let mut c = std::process::Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command"]);
            for k in ["http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
                c.env_remove(k);
            }
            let script = format!(
                // DefaultWebProxy=$null：Invoke-WebRequest 默认读系统代理，这里
                // 先置空再发请求，PS 通道也不带代理（与 curl 的 direct() 对齐）。
                "$ErrorActionPreference='Stop'; [System.Net.WebRequest]::DefaultWebProxy=$null; \
                 Invoke-WebRequest -Uri '{url}' -Headers @{{'User-Agent'='TUIProjectManager'}} \
                 -TimeoutSec 120 -OutFile '{dest}' -UseBasicParsing",
                url = ch.url.replace('\'', "''"),
                dest = tmp_str,
            );
            c.arg(script);
            c
        } else {
            let mut c = std::process::Command::new(curl_bin());
            c.args([
                "-q", "-L", "-f", "--ssl-no-revoke",
                // 挡连接挂死：镜像 5s，直链保底 8s。
                "--connect-timeout", ct,
                // 传输停滞判死：持续 <4KB/s 达 8s 中止本候选，轮到下一条链。
                "--speed-limit", "4096", "--speed-time", "8",
                // 直连：`direct()` 禁掉环境/系统代理（历史坑 3a70473 的
                // .curlrc 残留由 -q 挡），镜像池 10 条链不串同一个代理端口。
                "-H", "User-Agent: TUIProjectManager",
                "-o", tmp.to_str().unwrap_or("update.exe.new"),
            ]);
            direct(&mut c);
            // 断点续传：上次遗留的同指纹分片非空则续传（PS 无续传，直接覆盖重下）。
            // 已知总字节数时分片不可能比它还大——那就是别的批次残留/拼坏的，
            // 删掉重下，绝不拿它当续传起点。
            let have = file_len(&tmp);
            if have > 0 {
                if total > 0 && have >= total {
                    log_update(&format!("分片 {have} ≥ 总数 {total}，丢弃后重下"));
                    let _ = cleanup_file(tmp.to_str().unwrap_or(""));
                } else {
                    c.arg("-C").arg("-");
                }
            }
            c.arg(&ch.url);
            c
        };
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW，不闪黑窗
        }
        let kind = if ch.pool {
            "镜像"
        } else if ch.ps {
            "PS 通道"
        } else {
            "直连"
        };
        let before = file_len(&tmp);
        log_update(&format!(
            "下载 链 {i}/{n}（{kind}，第 {:.0}s，续传 {before} 字节）: {}",
            start.elapsed().as_secs_f64(),
            ch.url
        ));
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                errs.push(format!("{}: 启动下载失败 {e}", ch.url));
                continue;
            }
        };
        // 轮询到本链结束：退出码里才有成功/失败的真相。进度 = 本链分片当前大小。
        let status = loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                return Err("下载已取消".to_string());
            }
            let _ = progress_tx.send((file_len(&tmp), total));
            match child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
                Err(e) => {
                    let _ = child.kill();
                    errs.push(format!("{}: 等待下载进程失败 {e}", ch.url));
                    break None;
                }
            }
        };
        let Some(st) = status else { continue };
        // 本链确实推了字节 → 干等预算重新计时（链路是通的，别急着换链）。
        let got = file_len(&tmp);
        if got > before {
            dry_since = Instant::now();
        }
        if !st.success() {
            // 失败（HTTP 非零、429 限流、停滞判死）→ 记错误，分片保留供续传。
            let code = st
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "信号终止".to_string());
            errs.push(format!(
                "{kind}失败（{code}，已下 {got} 字节）: {}",
                ch.url
            ));
            continue;
        }
        // 字节数对账：已知总字节数时，产物必须分毫不差。少 = 截断，
        // 多 = 续传时服务器没认 Range（把整个包又追加了一遍）——两种
        // 情况产物头尾都可能长得像模像样（zip 尾部 PK\x05\x06 在位），
        // 光看 looks_like_zip 拦不住，装上去才发现是个坏 exe。
        if total > 0 && got != total {
            let _ = std::fs::remove_file(&tmp);
            errs.push(format!(
                "{}: 字节数不符（收到 {got}，应为 {total}，疑似截断或续传重复追加）",
                ch.url
            ));
            continue;
        }
        // 源返回了非 exe/zip 产物（错误页 HTML / 截断文件）：视作该链失败，
        // 删其分片后试下一条链——坏源永不胜出，杜绝「替换失败: 下载文件损坏」
        // 反复出现（错误页体积小、并发时最容易被当成赢家）。
        if !validate(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            errs.push(format!(
                "{}: 文件损坏（产物校验失败，疑似错误页或截断）",
                ch.url
            ));
            continue;
        }
        // promote：rename 被杀软/Defender 短持有（os error 5）时退避重试。
        let mut wait_ms = 300u64;
        loop {
            match std::fs::rename(&tmp, &dest_path) {
                Ok(()) => break,
                Err(e) => {
                    log_update(&format!("下载 promote rename 失败: {e}"));
                    if wait_ms > 4000 {
                        let _ = std::fs::copy(&tmp, &dest_path);
                        let _ = std::fs::remove_file(&tmp);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(wait_ms));
                    wait_ms = (wait_ms * 2).min(4000);
                }
            }
        }
        let final_size = file_len(&dest_path);
        let _ = progress_tx.send((final_size, total));
        return Ok(dest_path.to_string_lossy().into_owned());
    }
    if skipped > 0 {
        log_update(&format!(
            "下载 加速池干等预算用尽（{DL_POOL_BUDGET}s），跳过剩余 {skipped} 条镜像链"
        ));
    }
    if errs.is_empty() {
        return Err("无候选可启动".to_string());
    }
    // 错误汇总要能塞进状态栏：逐链原文可能有十来条，这里留头两条 + 末条（末条
    // 是最后试的保底链，最接近“现在到底卡在哪”），其余折成条数。log_update
    // 早已是空函数（不落盘），这句就是用户唯一能看到的原因，别把它撑成一行
    // 几千字符。
    Err(if errs.len() <= 3 {
        errs.join("；")
    } else {
        format!(
            "{}；……；{}（共 {} 条链失败）",
            errs[..2].join("；"),
            errs[errs.len() - 1],
            errs.len()
        )
    })
}

/// 带指数退避的 rename 重试。Windows 下杀软/Defender 实时扫描会短暂持有/// 删文件，被占用（刚被 kill 的落败 curl 进程句柄尚未释放）时退一小会重试。
/// 残留的 .c{i}.new 危害有 twofold：占安装目录空间；更要命的是下轮换个版本
/// 还会拿它们 -C - 续传，拼出坏包再被产物校验判死，白下载一次。
fn cleanup_file(path: &str) -> bool {
    for i in 0..5u32 {
        if std::fs::remove_file(path).is_ok() {
            return true;
        }
        if !std::path::Path::new(path).exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(120 * (i as u64 + 1)));
    }
    log_update(&format!("清理下载分片失败: {path}"));
    false
}

/// 带指数退避的 rename 重试。Windows 下杀软/Defender 实时扫描会短暂持有
/// 源或目标文件的句柄（未授予 FILE_SHARE_DELETE），rename 因此报拒绝访问
/// 源或目标文件的句柄（未授予 FILE_SHARE_DELETE），rename 因此报拒绝访问
/// （os error 5）；扫描大多几秒内结束，等待后重试即可成功。
/// progress_msg 每 report_every 次尝试通过 sink 报一条进度提示——注意消息
/// 不能含「失败」字样：自更新 UI 按该关键字复位 downloading 状态，会干扰安装。
/// sink 是消息出口（自更新 = 发状态栏通道；pi/opencode = 发工具事件通道），
/// 顺带负责唤醒重绘。
/// 返回 Ok(()) 或 deadline 耗尽时的最后一次错误。
fn retry_rename(
    src: &Path,
    dst: &Path,
    deadline: std::time::Duration,
    sink: &dyn Fn(&str),
    progress_msg: &str,
    report_every: usize,
) -> std::io::Result<()> {
    let start = std::time::Instant::now();
    let mut wait_ms: u64 = 300;
    let mut attempt = 0usize;
    loop {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1;
                log_update(&format!(
                    "替换 重试 rename {src:?}→{dst:?} 第 {attempt} 次失败: {e}"
                ));
                if report_every > 0 && attempt % report_every == 0 {
                    sink(&format!("{progress_msg}（等待系统释放…）"));
                }
                if start.elapsed() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(wait_ms));
                wait_ms = (wait_ms * 2).min(2000);
            }
        }
    }
}

/// 把已下载的 .new 文件安装到正式名 exe，处理 Windows 下目标/源被占用的场景。
///
/// Windows 规则：运行中的映像文件允许 rename（Vista+），但禁止原地替换/删除。
/// 替换链路的任一步都可能撞上杀软瞬时句柄（拒绝访问 os error 5），因此：
/// 1) 快路径：直接 rename(.new → 正式名)，带 ~10s 退避重试等杀软释放；
/// 2) 慢路径：正式名仍被占用时，先 rename(正式名 → .old) 腾名（运行中的
///    映像也可 rename，.old 兼作旧版备份），再放入新文件——最后一步是
///    最常失败处（刚下载完的 .new 正被 Defender 扫描），给足 ~20s 重试；
/// 失败自动回滚，正式名始终可用。返回安装结果：Done 已装上；BadDownload
/// 产物损坏（可自动换源重下，并非重试 rename 能救）；Occupied 被占（只能稍后重试）。
#[derive(PartialEq)]
enum InstallOutcome {
    Done,
    BadDownload,
    Occupied,
}

/// install_update 会先做一遍 PE 头校验；工具更新装的是从 zip 里取出的 exe，
/// 同样适用（validate 已在下载层用过，这里是双保险）。
/// 消息统一走 sink 回调（自更新 = 状态栏通道；pi/opencode = 工具事件通道）。
fn install_update(
    new_file: &Path,
    final_path: &Path,
    old_path: &Path,
    sink: &dyn Fn(&str),
) -> InstallOutcome {
    // 0) 校验下载产物（MZ+PE 头）：镜像偶发返回错误页/截断文件，装上就无法启动。
    //    下载层已前置剔除坏源，这里双保险；失败上层会自动换源重下，无需用户手动干预。
    if !looks_like_exe(new_file) {
        let _ = std::fs::remove_file(new_file); // 删掉坏的，避免被 -C - 续传拼坏
        sink(&format!(
            "下载到损坏文件，已自动换源重新下载（{new_file:?} 非有效 exe）"
        ));
        return InstallOutcome::BadDownload;
    }
    // 1) 快路径：正式名空闲 → 直接替换。~10s 重试窗口：杀软/Defender 扫描
    //    刚下载完的 .new 或正式名副本（拒绝访问 os error 5），等它释放。
    if retry_rename(
        new_file,
        final_path,
        std::time::Duration::from_secs(10),
        sink,
        "正在替换 exe",
        4,
    )
        .is_ok()
    {
        return InstallOutcome::Done;
    }
    // 2) 慢路径：正式名仍被占用。先清掉旧 .old（避免 rename 目标被占，
    //    遇到杀软持有旧 .old 时也带重试等待），再把正式名 rename 走腾出
    //    名字；运行中的映像也允许 rename。
    for i in 0..10 {
        match std::fs::remove_file(old_path) {
            Ok(()) => break,
            Err(e) => {
                log_update(&format!("替换 清理旧 .old 第 {} 次失败: {e}", i + 1));
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
    if retry_rename(
        final_path,
        old_path,
        std::time::Duration::from_secs(8),
        sink,
        "正在挪开旧版本",
        4,
    )
    .is_err()
    {
        sink(&format!(
            "替换失败: 正式名 {final_path:?} 一直被其他进程占用（多为杀软扫描或另一个正在运行的实例），新文件保留在 {new_file:?}，请稍后重试"
        ));
        return InstallOutcome::Occupied;
    }
    // 3) 最后一步：把 .new 放进腾出的正式名。这是最常失败的一步——刚下载
    //    完的 .new 正被 Defender 实时扫描，给它 ~20s 重试窗口。
    match retry_rename(
        new_file,
        final_path,
        std::time::Duration::from_secs(20),
        sink,
        "正在放入新版本",
        4,
    ) {
        Ok(()) => InstallOutcome::Done,
        Err(e) => {
            // 回滚：把挪走的旧映像放回正式名，确保目录里始终有可用 exe。
            let _ = retry_rename(
                old_path,
                final_path,
                std::time::Duration::from_secs(5),
                sink,
                "正在回滚旧版本",
                5,
            );
            // 区分失败原因：20s 后仍失败，多半是 .new 被杀软长时间持有
            //（写打开再失败即源被占用，与目标名无关）。
            let src_locked = std::fs::OpenOptions::new()
                .write(true)
                .open(new_file)
                .is_err();
            if src_locked {
                sink(&format!(
                    "替换失败: 新文件 {new_file:?} 持续被其他进程占用（多为杀软/Defender 实时扫描），已回滚保留旧版本；请稍后重试，或将应用目录加入 Windows 安全中心排除项"
                ));
            } else {
                sink(&format!(
                    "替换失败: {e}（已自动回滚，正式名保留旧版本；新文件仍在 {new_file:?}）"
                ));
            }
            InstallOutcome::Occupied
        }
    }
}

/// 校验下载产物是完整的 Windows PE 可执行文件：MZ 头 + e_lfanew（0x3C 偏移）
/// → "PE\0\0" 头 + 体积下限。错误页 HTML / 空文件 / 只下载到前几 KB 的截断
/// 文件（带 MZ 头但无 PE 上下文）一律判否，防止装坏 exe。
fn looks_like_exe(p: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(p) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    // 真实 exe 至少数百 KB；错误页/未知 200 响应通常只有 `config.json` 大小级别。
    if meta.len() < 256 * 1024 {
        return false;
    }
    let mut dos = [0u8; 0x40]; // DOS 头（含 0x3C 处的 e_lfanew 偏移）
    if f.read(&mut dos).unwrap_or(0) < 0x40 {
        return false;
    }
    let e_lfanew = u32::from_le_bytes([dos[0x3C], dos[0x3D], dos[0x3E], dos[0x3F]]) as u64;
    if e_lfanew + 4 > meta.len() || f.seek(SeekFrom::Start(e_lfanew)).is_err() {
        return false;
    }
    let mut pe = [0u8; 4];
    f.read(&mut pe).unwrap_or(0) == 4 && &pe == b"PE\0\0"
}

/// 校验下载产物是完整的 zip：局部文件头 "PK\x03\x04"（空包是 "PK\x05\x06"）、
/// 中央目录结尾 "PK\x05\x06"（截断包致命）、体积下限（1MB）。
/// 工具（pi/opencode）的 release 产物就是 zip，不能沿用 looks_like_exe，
/// 否则好包会被当成损坏包。
fn looks_like_zip(p: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(p) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    // pi 的 windows-x64 压缩包 ~44MB、opencode ~62MB；1MB 足以挡住错误页/空包，
    // 又不会误杀小体积的合法资产。
    if meta.len() < 1024 * 1024 {
        return false;
    }
    let mut head = [0u8; 4];
    if f.read(&mut head).unwrap_or(0) < 4 || &head != b"PK\x03\x04" {
        return false;
    }
    // 末尾 64KB 内必须有中央目录结束记录：zip 尾部缺它说明下载被截断
    // （镜像断流/代理改写都会这样），装上去解不开。
    let tail_len = std::cmp::min(meta.len(), 64 * 1024) as u64;
    if f.seek(SeekFrom::End(-(tail_len as i64))).is_err() {
        return false;
    }
    let mut tail = vec![0u8; tail_len as usize];
    if f.read_exact(&mut tail).is_err() {
        return false;
    }
    tail.windows(4).any(|w| w == b"PK\x05\x06")
}

/// 从 GitHub Release 的资产列表 HTML 里抽全部资产直链，返回 (文件名, 下载 URL)。
///
/// **必须按 href 里给出的完整路径重建 URL**：GitHub 的 href 是
/// `/<owner>/<repo>/releases/download/<tag>/<name>`，旧实现只截取
/// `releases/download/` 之后的两段，再拼成 `https://github.com/releases/download/…`
/// ——owner/repo 整个丢了，拼出来的地址必然 404。也就是说这个 HTML 源从来没
/// 成功过（自更新的「源② HTML」与工具下载的「源② HTML」都白等一次超时）。
///
/// 输入既可以是标签页 HTML，也可以是 `releases/expanded_assets/<tag>` 片段
/// （后者才是真正带资产链接的那份，标签页初始 HTML 一个链接都没有——资产列表
/// 是 lazy-load 的 include-fragment）。
fn assets_from_html(html: &str, repo: &str, tag: &str) -> Vec<(String, String)> {
    const NEEDLE: &str = "releases/download/";
    let mut from = 0;
    let mut out: Vec<(String, String)> = Vec::new();
    let (want_owner, want_repo) = repo.split_once('/').unwrap_or(("", repo));
    while let Some(rel) = html[from..].find(NEEDLE) {
        let at = from + rel; // NEEDLE 在 html 里的绝对位置
        let start = at + NEEDLE.len();
        let rest = &html[start..];
        let end = rest
            .find(['\'', '\"', '<', '?', '\n', ' ', '\t', '\r'])
            .unwrap_or(rest.len());
        // 形如 {owner}/{repo}/{tag}/{asset}：owner/repo 在 NEEDLE **之前**
        // （href 可能是 /owner/repo/... 相对路径，也可能是完整 URL），得回看
        // 到属性起点才能取到；旧实现只截了 NEEDLE 之后的两段，把这两段丢了。
        let head_all = &html[..at];
        let hstart = head_all
            .rfind(['\'', '\"', '<', ' ', '\t', '\n', '\r'])
            .map(|i| i + 1)
            .unwrap_or(0);
        let hp: Vec<&str> = head_all[hstart..]
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let (owner, rpo) = (
            hp.len().checked_sub(2).and_then(|i| hp.get(i)).copied().unwrap_or_default(),
            hp.last().copied().unwrap_or_default(),
        );
        let parts: Vec<&str> = rest[..end].split('/').collect();
        if parts.len() == 2
            && parts[0] == tag
            && owner.eq_ignore_ascii_case(want_owner)
            && rpo.eq_ignore_ascii_case(want_repo)
        {
            out.push((
                parts[1].to_string(),
                format!("https://github.com/{owner}/{rpo}/{NEEDLE}{}/{}", parts[0], parts[1]),
            ));
        }
        from = start;
    }
    out
}

/// 一个待下载的 exe 资产。**用字段名而不是元组位置**。
///
/// 事故：曾经这里是 `Option<(String, String, u64)>` 且约定为「(文件名, URL, 字节数)」，
/// 而 `download_update` 那边按「(URL, 文件名, 字节数)」解构——两边都没写错，合起来
/// 正好**反了**。后果是自更新 100% 失败，而且症状极具迷惑性：
///  - `url` 字段拿到的是纯文件名 → 每条候选链都请求 `https://gh-proxy.com/tui-project-manager.exe`
///    → 10 条链全 404/403，看起来像「镜像全挂了」；
///  - `name` 字段拿到的是整条 URL → 临时分片名变成
///    `…/https://github.com/…/tui-project-manager.exe.<fp>.c0.new`（含 `:` 与 `/`），
///    Windows 上开不出这个文件 → `curl: (23) client returned ERROR on write`，
///    **进度永远停在 0**，正是用户看到的「卡在 0 然后失败」。
///  工具下载（pi/opencode）走 API 资产表那套，不经过这里，所以它们一直很快——
///  更显得「只有自更新坏了」。
struct ExeAsset {
    /// 可直接请求的完整 URL（自更新恒为 github.com release 直链，镜像由调用方拼前缀）。
    url: String,
    /// 纯文件名（**必须能当路径用**：不得含 `:` `/` 等分隔符），产物落盘与校验都用它。
    name: String,
    /// 已知总字节数（0 = 未知，例如 HTML 源不带长度）。
    size: u64,
}

/// 从 GitHub Release 的资产列表 HTML 里找 exe 下载直链（API 限流/被墙时兜底）。
/// 返回 `ExeAsset { size: 0 }`（HTML 不含字节数）；没有 exe 直链时 None。
fn exe_asset_from_html(html: &str, repo: &str, tag: &str) -> Option<ExeAsset> {
    assets_from_html(html, repo, tag)
        .into_iter()
        .find(|(_, name)| name.ends_with(".exe"))
        .map(|(name, url)| ExeAsset { url, name, size: 0 })
}

/// 自更新的 exe 直链探查：与检查更新（tag）、工具下载（资产表）完全同一套
/// 镜像源逻辑——GH_MIRRORS 前缀镜像 × 8 → 直连 → PS WinHTTP **逐个尝试**，
/// 先成功者为准。返回 (胜出源名, `ExeAsset`)。
///
/// 页面选取：优先 `releases/expanded_assets/{tag}`（真带下载链接的那份），
/// 标签页只挂直连/PS（部分镜像会把 include-fragment 一起渲染出来，值得一试，
/// 但没必要 ×8 镜像重复拉同一页）。
fn fetch_self_exe_asset(tag: &str) -> Result<(String, ExeAsset), String> {
    let frag = format!("{SELF_REPO_PAGE}/releases/expanded_assets/{tag}");
    let page = format!("{SELF_REPO_PAGE}/releases/tag/{tag}");
    let mut sources: Vec<Probe> = Vec::new();
    for m in GH_MIRRORS {
        sources.push(
            Probe::new(&format!("{m}/资产片段"), format!("{m}{frag}"), ProbeKind::Html, 3, 6)
                .pool()
                .ctx(SELF_REPO, tag),
        );
    }
    sources.push(Probe::new("GitHub 资产片段", frag, ProbeKind::Html, 6, 12).ctx(SELF_REPO, tag));
    sources.push(Probe::new("GitHub 标签页", page.clone(), ProbeKind::Html, 6, 12).ctx(SELF_REPO, tag));
    #[cfg(windows)]
    sources.push(
        Probe::new("PS 标签页", page, ProbeKind::Html, 8, 15).via_ps().ctx(SELF_REPO, tag),
    );
    probe_first(&format!("{tag} 自更新 exe 资产"), sources, |p, body| {
        exe_asset_from_html(body, &p.repo, &p.tag)
            .ok_or_else(|| "HTML 里没有 exe 直链".to_string())
    })
}

// ── 外部工具（pi / opencode）更新 ──────────────────────────────────────
//
// 自更新那套流水线（多源逐个探查 → 候选链逐个下载 .new → 解压 → install_update
// 三段式替换 .old/.new）原样复用，只在两处因工具而变：
//  1) 产物是 zip（pi-windows-x64.zip / opencode-windows-x64.zip），下载校验
//     换成 looks_like_zip，装上前必须解压；解开后的主产物 exe 才进替换链；
//  2) 目标不是本软件的 exe，而是 PATH 里找到的 pi.exe / opencode.exe——
//     安装目录 = 该 exe 所在目录，替换时可能正被本软件的内嵌终端会话占用，
//     这正是 install_update 慢路径（挪走 .old 腾名）要处理的情形。

/// 外部工具的更新目标描述。资产名模板按优先级排列，{arch} 运行时替换。
struct ToolSpec {
    /// 稳定 id（临时文件名/日志用）。
    id: &'static str,
    /// 状态栏按钮与消息里的显示名。
    label: &'static str,
    repo: &'static str,
    /// 本地 exe 名（同时是 zip 解压后要取出的主产物名）。
    exe_name: &'static str,
    /// 候选 release 资产名模板（Windows 压缩包），{arch} → x64 / arm64。
    asset_tpls: &'static [&'static str],
    /// 除 exe 外是否把解压出的整棵树覆盖同步进安装目录。pi 的 zip 含
    /// assets/native/theme/docs/examples 等运行期文件（新版可能新增或改名），
    /// 只换 exe 会与新版本对不上；opencode 的 zip 只含 exe，无需同步。
    /// 同步是**覆盖式**（不删旧文件），用户自装的 node_modules/扩展不受影响。
    sync_tree: bool,
    /// **未安装**时（同级目录/配置路径/PATH 都没找到 exe）新装到哪：true = 装进
    /// `<软件目录>\<工具名>\` 子目录，false = 平铺在软件同级目录。
    /// pi 的 zip 含整棵程序树（assets/native/theme/docs/node_modules…），平铺会把
    /// 软件目录弄脏、还和本软件的文件混在一起，故走子目录；opencode 的 zip 只含
    /// 一个 exe，平铺即可（也与「D:\agent\opencode.exe」的既有摆法一致）。
    fresh_in_subdir: bool,
}

const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        id: "pi",
        label: "pi",
        repo: "earendil-works/pi",
        exe_name: "pi.exe",
        asset_tpls: &["pi-windows-{arch}.zip"],
        sync_tree: true,
        fresh_in_subdir: true,
    },
    ToolSpec {
        id: "opencode",
        label: "opencode",
        repo: "anomalyco/opencode",
        exe_name: "opencode.exe",
        // baseline 版是给无 AVX2 的老 CPU 准备的，主版本失败时再试。
        asset_tpls: &[
            "opencode-windows-{arch}.zip",
            "opencode-windows-{arch}-baseline.zip",
        ],
        sync_tree: false,
        fresh_in_subdir: false,
    },
];

/// Windows 资产名里的架构段：x86_64 → x64，aarch64 → arm64。
fn win_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x64",
    }
}

/// 该工具在本机的 Windows 压缩包候选名（按优先级，{arch} 已替换）。
fn tool_asset_names(spec: &ToolSpec) -> Vec<String> {
    spec.asset_tpls
        .iter()
        .map(|t| t.replace("{arch}", win_arch()))
        .collect()
}

/// 工具后台线程 → UI 线程的事件。
enum ToolEvent {
    /// 检查完成：本地版本（空 = 未检测到）+ 有新版本时的 tag + 安装目录
    /// （exe 所在目录，即后续下载/解压/替换的操作位置）。
    Checked {
        local: String,
        latest: Option<String>,
        install_dir: PathBuf,
        missing: bool,
    },
    /// 过程中的状态栏消息（下载/解压/替换）。
    Status(String),
    /// 下载进度 (已下载字节, 总字节，总数为 0 时只按已下载显示)。
    Progress(u64, u64),
    /// 下载 + 解压 + 替换全部完成（携带状态栏消息与装好的版本号）。
    Done { msg: String, version: String },
    /// 本轮任务结束（成功/失败/取消），UI 据此复位 downloading 状态。
    Finished,
}

/// 工具更新作业的最大尝试次数（自更新是无限重试，工具这边一次要重下
/// 60MB 量级的压缩包，封顶更合理）。
const MAX_TOOL_ATTEMPTS: u32 = 5;

/// 自更新下载的最大尝试次数。**原来是无限重试**：所有源都挂时每 3 秒重跑一
/// 轮 download_chain（10 条链）永远停不下来，而且 UI 看到含「失败」的状态栏
/// 文案就把 downloading 复位 → 「✕ 取消」按钮随之消失 → 用户**连按都按不到
/// 停止**（下载线程还在后台每 3 秒烧一次网）。现在：封顶 5 次 + 退避递增，
/// 收手后提示手动重试，取消始终有按钮可按。
const MAX_SELF_UPDATE_ATTEMPTS: u32 = 5;

/// 重试退避：3s → 6s → 12s → 24s → 封顶 30s。
fn retry_backoff(attempt: u32) -> std::time::Duration {
    let secs = 3u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(4));
    std::time::Duration::from_secs(secs.min(30))
}

/// 可中断的等待：`cancel` 一置位立刻返回（分片 100ms 睡，不做满全程）。
/// 返回 true = 期间被取消。原来的 `thread::sleep(3s)` 让「✕ 取消」最多 3 秒
/// 才生效，重试期间一直这样。
fn sleep_until_cancel(
    cancel: &std::sync::atomic::AtomicBool,
    total: std::time::Duration,
) -> bool {
    let mut left = total;
    while !left.is_zero() {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        let step = left.min(std::time::Duration::from_millis(100));
        std::thread::sleep(step);
        left -= step;
    }
    cancel.load(Ordering::Relaxed)
}

/// 设置页三个折叠分区的 id（`settings_section` 的 `id_salt`）。列在这里是因为
/// 它们是**互斥**的：`open_settings_sec` 里只放得下一块，`settings_section` 每帧
/// 按它强制各块的开合，所以必须能拿到全量 id（见 `App::settings_section`）。顺序
/// 不影响行为，只是列出「设置」页的三大块。
const SETTINGS_SEC_IDS: [&str; 3] = [
    "settings_sec_cmds",
    "settings_sec_tool_dirs",
    "settings_sec_providers",
];


/// 设置页「供应商配置」区的页签表：(显示名, `model_settings_tab` 下标)。
///
/// oh-my-pi（下标 1）的供应商配置**已隐藏**，不在这儿列出；但下标原样保留不动，
/// 编辑缓冲 / 配置读写那套按 0=pi、1=omp、2=opencode 走的索引不用改。停在那张
/// 隐藏页签上时由 `providers_section_ui` 落回 pi。
const PROVIDER_TABS: [(&str, usize); 2] = [("pi 供应商配置", 0), ("opencode 供应商配置", 2)];

/// 单个工具的更新状态。install_dir 在检查线程里按 PATH 定位 exe 后回填。
struct ToolState {
    /// 本地版本（`--version` 解析结果，空 = 未检测到）。
    local: String,
    /// 有新版本时的 tag（下载按钮的标签）。
    latest: Option<String>,
    /// 正在下载/解压/替换。
    downloading: bool,
    /// 本机未检测到该工具（软件同级目录 / 配置路径 / PATH 都没有）：状态栏出
    /// 「⬇ 安装」按钮，装到 install_dir（软件同级目录），版本号点下去时现查。
    missing: bool,
    /// exe 所在目录（= 解压暂存、临时 zip、.old 备份都落在这里，同卷 rename）。
    install_dir: PathBuf,
    /// 本轮在装的 tag：作业非成功结束时用它把下载按钮重新点亮（可再点重试）。
    pending_tag: Option<String>,
    /// 我们亲手装上的那个 tag（Done 时记下）。当本地版本读不出来时
    /// （`--version` 输出认不出）拿它兜底，否则同一个 tag 会被反复当成
    /// 「有新版本」，按钮刚点完又冒出来。
    installed_tag: Option<String>,
}

impl ToolState {
    fn new() -> Self {
        Self {
            local: String::new(),
            latest: None,
            downloading: false,
            missing: false,
            install_dir: PathBuf::new(),
            pending_tag: None,
            installed_tag: None,
        }
    }
}

/// 本软件 exe 所在目录（current_exe 的父目录；current_exe 可能是 unlock_exe
/// 改名后的 {name}.running 锁名，父目录一致，仍走 canonical_exe_path 归一）。
/// 没有任何硬编码路径：「打开软件目录」与工具查找都以它为准。
fn software_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .map(ClientApp::canonical_exe_path)
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// 本软件同级目录下 pi / opencode 的**预期路径**（逐工具两种摆法：
/// `<软件目录>\pi.exe` 与 `<软件目录>\pi\pi.exe`），全部由 current_exe 推得，
/// 不含任何硬编码盘符——换机器/换安装位置自动跟着变。这些路径即使当前不存
/// 在也照样写进配置（用户事后把 exe 丢进去就能被找到）；真正决定“显不显示
/// 下载按钮”的是检查时该文件是否真存在。
fn sibling_tool_paths() -> Vec<PathBuf> {
    let Some(dir) = software_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for spec in TOOL_SPECS {
        out.push(dir.join(spec.exe_name));
        out.push(dir.join(spec.id).join(spec.exe_name));
    }
    out
}

/// 未安装时的新装目录（纯函数，便于单测）：pi 进 `<软件目录>\pi\`，
/// opencode 平铺在软件同级目录。
fn tool_fresh_dir_in(app_dir: &Path, spec: &ToolSpec) -> PathBuf {
    if spec.fresh_in_subdir {
        app_dir.join(spec.id)
    } else {
        app_dir.to_path_buf()
    }
}

/// 未安装时的新装目录（运行期版）：以本软件 exe 所在目录为根，全部由
/// current_exe 推得，不含任何硬编码盘符。current_exe 取不到（理论上不会）时
/// 返回 None —— 没有目录可装，UI 也就不会给出安装入口。
fn fresh_tool_dir(spec: &ToolSpec) -> Option<PathBuf> {
    software_dir().map(|d| tool_fresh_dir_in(&d, spec))
}

/// 状态栏里一个工具该给什么下载入口。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ToolEntryKind {
    /// 不给按钮（已是最新且本软件目录里也有，或没处可装）。
    None,
    /// 升级：装到该工具当前所在目录（它现在跑在哪就更新哪份）。
    Update,
    /// 装到本软件同级目录：本机没装，或装在别处（PATH / 别的配置路径）而
    /// 本软件目录里没有 —— 后者也得给，否则这种机器上一个入口都没有。
    InstallFresh,
}

/// 状态栏入口的判定（纯函数，便于单测）：
/// `has_update` = 检查发现有新版；`missing` = 本机完全没检测到；
/// `fresh_present` = 本软件同级目录里已经有一份；`fresh_dir_ok` = 同级目录可写
/// （current_exe 取得到）。
///
/// 升级优先：有新版就更新现在跑的那份（换机器/改 PATH 都不会让用户丢配置）。
/// 已是最新时仍保留“装到本软件目录”入口：把工具收一份在本软件旁边，之后
/// 检查更新以这份为准，PATH 变了、全局安装被删都不影响。
fn tool_entry_kind(
    has_update: bool,
    missing: bool,
    fresh_present: bool,
    fresh_dir_ok: bool,
) -> ToolEntryKind {
    if has_update {
        return ToolEntryKind::Update;
    }
    if !fresh_dir_ok {
        // 拿不到本软件目录 → 只有“有新版”才有地方可写。
        return ToolEntryKind::None;
    }
    if missing || !fresh_present {
        ToolEntryKind::InstallFresh
    } else {
        ToolEntryKind::None
    }
}

/// 这一轮该工具会不会在设置页更新区画出入口（`tool_entry_button_ui` 同款判定）。
///
/// **必须把「正在下载」算作有入口**：下载按钮一点，`start_tool_download` 立刻把
/// `latest` 清成 None（tag 交给后台作业）；工具若就装在本软件同级目录
/// （opencode 就是 exe 旁边的 `opencode.exe`），此刻 has_update=false、
/// missing=false、fresh_present=true → [`tool_entry_kind`] 给 None。只按它判
/// “有没有入口”，`update_zone_ui` 的 `tools_pending` 会在下载**刚开始时变 false**
/// → 整块更新区（连同下载中的「⬇ opencode 下载中…」和「✕ 取消」按钮）整块消失，
/// 用户既看不到进度也**没法取消**（本程序自身那路把 `downloading` 计进了
/// `self_pending`，所以只有工具下载会犯这个毛病）。
fn tool_entry_visible(
    downloading: bool,
    has_update: bool,
    missing: bool,
    fresh_present: bool,
    fresh_dir_ok: bool,
) -> bool {
    downloading
        || !matches!(
            tool_entry_kind(has_update, missing, fresh_present, fresh_dir_ok),
            ToolEntryKind::None
        )
}

/// 把缺失的同级默认路径补进配置（已存在的不动，用户改过的不会被覆写）。
/// 返回 true = 配置有变化，需要落盘。
fn fill_default_tool_paths(config: &mut config::Config) -> bool {
    let defaults = sibling_tool_paths();
    if defaults.is_empty() {
        return false;
    }
    let mut changed = false;
    for p in defaults {
        let s = p.to_string_lossy().to_string();
        if !config.settings.tool_paths.iter().any(|d| d == &s) {
            config.settings.tool_paths.push(s);
            changed = true;
        }
    }
    changed
}

/// 工具 exe 的查找顺序（先到先用，同路径只留一条）：
///  1) 调用方给的条目（默认本软件所在目录 + 配置里的路径）：每一项可以是
///     **exe 完整路径**（文件名为当前工具的 exe 才收，否则跳过——同一列表里
///     混着 pi / opencode 的路径互不干扰），也可以是**目录**（则同时试
///     `<目录>\<exe>` 与 `<目录>\<工具名>\<exe>`，后者是 pi 目录安装的摆法）；
///  2) PATH 的各个目录（全局安装那份，路径均来自环境变量，不硬编码）。
/// 只拼路径不判存在，调用方取第一个 is_file 的（便于单测）。
fn tool_exe_candidates(
    exe_name: &str,
    sub_dir: &str,
    entries: &[PathBuf],
    path: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for entry in entries {
        if entry.as_os_str().is_empty() {
            continue;
        }
        // 有 .exe 扩展名 = 指向具体 exe；否则当目录用。
        if entry
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("exe"))
        {
            if entry.file_name().is_some_and(|n| n == exe_name) {
                push(entry.clone());
            }
            continue;
        }
        push(entry.join(exe_name));
        push(entry.join(sub_dir).join(exe_name));
    }
    // PATH 手动扫目录，不 spawn `where`——GUI 程序 spawn 控制台程序会闪黑窗；
    // 顺带避开 Git 自带 where.exe 的行为差异。
    if let Some(path) = path {
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            push(dir.join(exe_name));
        }
    }
    out
}

/// 按 tool_exe_candidates 的顺序找工具 exe（第一个存在的文件即目标）。
fn find_tool_exe(spec: &ToolSpec, entries: &[PathBuf], use_path: bool) -> Option<PathBuf> {
    let cands = tool_exe_candidates(
        spec.exe_name,
        spec.id,
        entries,
        if use_path {
            std::env::var_os("PATH")
        } else {
            None
        },
    );
    cands.into_iter().find(|p| p.is_file())
}

/// 从一段输出里抠出版本号（首个「数字+点」形态的 token，去掉 v 前缀）：
/// pi --version → 0.87.1，opencode --version → 1.18.32。抠不到返回空串。
fn parse_version_token(text: &str) -> String {
    for tok in text.split(|c: char| {
        c.is_whitespace() || c == ',' || c == '(' || c == ')' || c == '"' || c == '\'' || c == '='
    }) {
        let t = tok.trim().trim_start_matches('v');
        // 至少一个点、全是数字/点、且以数字开头（排掉 "windows-x64" 之类）。
        if t.contains('.')
            && t.starts_with(|c: char| c.is_ascii_digit())
            && t.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            return t.to_string();
        }
    }
    String::new()
}

/// 读工具本地版本：`<exe> --version`。失败/抠不出数字一律返回空串
/// （上层当作「版本未知」，不误报有更新）。
fn local_tool_version(exe: &Path) -> String {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW，不闪黑窗
    }
    let Ok(o) = cmd.output() else {
        return String::new();
    };
    let stdout = String::from_utf8_lossy(&o.stdout);
    let text = if stdout.trim().is_empty() {
        String::from_utf8_lossy(&o.stderr)
    } else {
        stdout
    };
    parse_version_token(&text)
}

/// 解压用 tar：Windows 钉死系统 bsdtar（C:\Windows\System32\tar.exe，自带 zip
/// 读取器）。PATH 里常见的 GNU tar（Git 自带）**不解 zip**（实测报 “This does
/// not look like a tar archive”），抽出来的是垃圾目录，必须避开。精简系统无此
/// 文件时回退 PATH，PowerShell 通道作最终兜底。
#[cfg(windows)]
fn tar_bin() -> &'static str {
    const SYS_TAR: &str = "C:\\Windows\\System32\\tar.exe";
    if std::path::Path::new(SYS_TAR).exists() {
        SYS_TAR
    } else {
        "tar"
    }
}
#[cfg(not(windows))]
fn tar_bin() -> &'static str {
    "tar"
}

/// 解压 zip 到 dest（先清空 dest 再解）。系统 bsdtar 优先（快、零依赖），
/// 失败/缺失退 PowerShell 的 [IO.Compression.ZipFile]::ExtractToDirectory。
/// 两条通道都在同进程 spawn 且 CREATE_NO_WINDOW，不闪黑窗。
fn extract_zip(zip: &Path, dest: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).map_err(|e| format!("创建解压目录失败: {e}"))?;
    let zip_s = zip.to_str().ok_or("压缩包路径含非 ASCII 字符")?;
    let dest_s = dest.to_str().ok_or("解压目录路径含非 ASCII 字符")?;
    // 源①系统 bsdtar。tar 对 zip 里的反斜杠/中文名兼容性不如 .NET，失败即退下一源。
    let mut cmd = std::process::Command::new(tar_bin());
    cmd.args(["-xf", zip_s, "-C", dest_s]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    match cmd.output() {
        Ok(o) if o.status.success() => return Ok(()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            log_update(&format!("解压 tar 失败: {err}"));
        }
        Err(e) => log_update(&format!("解压 tar 启动失败: {e}")),
    }
    // 源②PowerShell .NET 解压（吃系统代理，tar 不吃）。
    #[cfg(windows)]
    let result = {
        let script = format!(
            "$ErrorActionPreference='Stop'; \
             Add-Type -AssemblyName System.IO.Compression.FileSystem; \
             [System.IO.Compression.ZipFile]::ExtractToDirectory('{zip}','{dest}')",
            zip = zip_s.replace('\'', "''"),
            dest = dest_s.replace('\'', "''"),
        );
        ps_run(&script)
            .map_err(|e| format!("解压失败（tar 与 PowerShell 均未成功）: {e}"))
            .map(|_| ())
    };
    #[cfg(not(windows))]
    let result: Result<(), String> =
        Err("解压失败：系统 tar 不支持该压缩包，且无 PowerShell 兜底".to_string());
    result
}

/// 资产表：name → (browser_download_url, 字节数)。
/// 带 size 是为了下载进度能显示百分比（HTML 源拿不到字节数）。
type AssetTable = std::collections::HashMap<String, (String, u64)>;

/// 把 API 响应里的 assets 数组收进资产表。
fn asset_table_from_json(v: &serde_json::Value) -> Result<AssetTable, String> {
    let mut out = AssetTable::new();
    for a in v["assets"].as_array().into_iter().flatten() {
        if let (Some(n), Some(u)) = (a["name"].as_str(), a["browser_download_url"].as_str()) {
            out.insert(n.to_string(), (u.to_string(), a["size"].as_u64().unwrap_or(0)));
        }
    }
    if out.is_empty() {
        // 空表当失败：否则「这个 tag 没有资产」会被当成「资产表拿到了」，
        // 后面的候选筛选一步也不会做，直接落到直拼直链。
        return Err("响应里没有 assets".to_string());
    }
    Ok(out)
}

/// 探查某个 tag 的资产表。**与检查更新完全同一套镜像源逻辑**（GH_MIRRORS
/// 前缀镜像 → GitHub 直连 → PS WinHTTP，逐个尝试先成功先得），两种正文形态：
///   1. `api.github.com/repos/{repo}/releases/tags/{tag}` → 资产表 + 字节数；
///   2. `github.com/{repo}/releases/expanded_assets/{tag}` → 资产列表片段
///      （标签页初始 HTML 里一个链接都没有，片段页才是真带链接的那份）。
///
/// 旧实现只直连 api.github.com 一次（connect 8s / max 15s），大陆机器上必然
/// 先白等 15 秒、再白等 15 秒拿标签页 HTML（且那份 HTML 解析出来还是 404 的
/// 假 URL），最后才落到直拼直链——检查更新明明能过镜像拿到 tag，一到下载就
/// 变成「先卡半分钟再慢慢下」。
fn fetch_release_assets(repo: &str, tag: &str) -> Result<(String, AssetTable), String> {
    let api_url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
    let frag_url = format!("https://github.com/{repo}/releases/expanded_assets/{tag}");
    let gh_api: fn(&serde_json::Value) -> Option<String> = |v| v["tag_name"].as_str().map(str::to_string);
    let mut sources: Vec<Probe> = Vec::new();
    for m in GH_MIRRORS {
        sources.push(Probe::new(m, format!("{m}{api_url}"), ProbeKind::Json(gh_api), 3, 6).pool().ctx(repo, tag));
    }
    for m in GH_MIRRORS {
        sources.push(Probe::new(m, format!("{m}{frag_url}"), ProbeKind::Html, 3, 6).pool().ctx(repo, tag));
    }
    sources.push(Probe::new("GitHub API", api_url.clone(), ProbeKind::Json(gh_api), 6, 12).ctx(repo, tag));
    sources.push(Probe::new("GitHub 资产片段", frag_url.clone(), ProbeKind::Html, 6, 12).ctx(repo, tag));
    #[cfg(windows)]
    {
        sources.push(Probe::new("PS API", api_url, ProbeKind::Json(gh_api), 8, 15).via_ps().ctx(repo, tag));
        sources.push(Probe::new("PS 资产片段", frag_url, ProbeKind::Html, 8, 15).via_ps().ctx(repo, tag));
    }
    probe_first(&format!("{repo}@{tag} 资产表"), sources, parse_assets)
}

/// 从探查正文里收资产表（API 给 JSON，expanded_assets 片段给 HTML）。
fn parse_assets(p: &Probe, body: &str) -> Result<AssetTable, String> {
    match &p.kind {
        ProbeKind::Json(_) => {
            let v: serde_json::Value =
                serde_json::from_str(body).map_err(|e| format!("API 响应解析失败: {e}"))?;
            asset_table_from_json(&v)
        }
        _ => {
            let all = assets_from_html(body, &p.repo, &p.tag);
            if all.is_empty() {
                return Err("HTML 里没有资产链接".to_string());
            }
            Ok(all.into_iter().map(|(n, u)| (n, (u, 0))).collect())
        }
    }
}

/// 从资产表里挑该工具的 Windows 压缩包，按优先级返回 (名, URL, 字节数)。
///
/// 先按模板精确命中（pi-windows-x64.zip / opencode-windows-x64.zip …），
/// 模板没命中就在表里**按形态发现**：名字含 windows + 本机架构 + .zip 即算
/// 候选，按「普通版 → baseline/兼容版」排序。没有这一步的话，上游把产物名
/// 改成 pi-windows-x64-gnu.zip / opencode_windows_x64.zip 之类，就得跟着改
/// 代码才能下载——而资产表明明就在手里。checksums / 源码包 / 其它架构一律排除。
fn pick_tool_assets(table: &AssetTable, names: &[String]) -> Vec<(String, String, u64)> {
    let mut out: Vec<(String, String, u64)> = Vec::new();
    let mut push = |n: &str| {
        // 模板命中过的名字不再被「发现」段重复收一遍（否则同一个资产多探两轮）
        if out.iter().any(|(rn, _, _)| rn == n) {
            return;
        }
        if let Some((u, s)) = table.get(n) {
            out.push((n.to_string(), u.clone(), *s));
        }
    };
    for n in names {
        push(n); // 源①：模板精确命中（保留模板顺序）
    }
    // 源①补：形态发现（模板名对不上也不至于整个下载不了）。
    let arch = win_arch();
    let mut discovered: Vec<(u8, &String)> = Vec::new();
    for (n, _) in table {
        let low = n.to_ascii_lowercase();
        if !low.ends_with(".zip") || !low.contains("windows") || !low.contains(arch) {
            continue;
        }
        if ["checksum", "sha256", "sbom", "source"].iter().any(|k| low.contains(k)) {
            continue;
        }
        let compat = low.contains("baseline") || low.contains("gnu") || low.contains("musl");
        discovered.push((u8::from(compat), n));
    }
    discovered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    for (_, n) in discovered {
        push(n);
    }
    out
}

/// 一个 release 的 (tag, 资产表)。
type ReleaseEntry = (String, AssetTable);

/// 解析 `/releases` 发布列表（JSON 数组，每个元素的 `assets` 内联在响应里）：
/// 收成「按新到旧」的 (tag, 资产表) 序列。
///
/// GitHub 的 `/releases` 本来就按 `created_at` 降序返回，所以**保留原序**即
/// 「新 → 旧」；不去自己比版本号——那正是本次事故的教训：跨版本线（npm 包版本
/// vs Release tag）比出来的「新」是假的。
fn parse_release_list(body: &str) -> Result<Vec<ReleaseEntry>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("发布列表解析失败: {e}"))?;
    let arr = v.as_array().ok_or("发布列表不是数组".to_string())?;
    let mut out = Vec::new();
    for r in arr {
        let Some(tag) = r["tag_name"].as_str() else { continue };
        // draft / prerelease 不进候选：它们可能压根没发 Windows 包。
        if r["draft"].as_bool().unwrap_or(false) || r["prerelease"].as_bool().unwrap_or(false) {
            continue;
        }
        // asset_table_from_json 对空 assets 返 Err（这是刻意的），这里单个
        // release 没资产时跳过即可，整列表为空才判失败。
        if let Ok(table) = asset_table_from_json(r) {
            out.push((tag.to_string(), table));
        }
    }
    if out.is_empty() {
        return Err("发布列表里没有可用 release".to_string());
    }
    Ok(out)
}

/// 拉取仓库的发布列表（新 → 旧）。同样走 GH_MIRRORS 镜像（逐个试）+ PS 通道，
/// 与 `fetch_release_assets` 同一套骨架。
fn fetch_release_list(repo: &str) -> Result<Vec<ReleaseEntry>, String> {
    let api_url = format!("https://api.github.com/repos/{repo}/releases?per_page=20");
    let mut sources: Vec<Probe> = Vec::new();
    for m in GH_MIRRORS {
        sources.push(Probe::new(m, format!("{m}{api_url}"), ProbeKind::JsonList, 3, 6).pool().ctx(repo, ""));
    }
    sources.push(Probe::new("GitHub API", api_url.clone(), ProbeKind::JsonList, 6, 12).ctx(repo, ""));
    #[cfg(windows)]
    sources.push(Probe::new("PS API", api_url, ProbeKind::JsonList, 8, 15).via_ps().ctx(repo, ""));
    #[cfg(not(windows))]
    let _ = api_url;
    probe_first(&format!("{repo} 发布列表"), sources, |_p, body| parse_release_list(body))
        .map(|(_, v)| v)
}

/// 从发布列表里挑**最新且确实带该工具 Windows 压缩包**的 tag。
///
/// 这是 tag 探查的权威兜底：`tag_probes` 是「先成功先得」的快查询，任何一个源
/// 答错就会把错 tag 带进整条下载链（拼出的直链必然 404，11 条候选链 × 5 次
/// 重试 = 白烧几百 MB + 几分钟干等）。这里用**产物存在性**当判据：不信任任何
/// 源报上来的 tag，只认「那个 release 里真的躺着我们要的 zip」的 tag。
///
/// 纯函数：GitHub 已按新到旧排好序，返回第一个能挑出 Windows 压缩包的即可
/// （跳过 `tag`，下载端还要比对不重下）。
fn pick_release_tag_with_assets(
    releases: &[ReleaseEntry],
    names: &[String],
) -> Option<String> {
    releases
        .iter()
        .find(|(_, table)| !pick_tool_assets(table, names).is_empty())
        .map(|(tag, _)| tag.clone())
}

/// 工具下载的失败：区分「重下一次可能就好」（网络）与「重下多少次都是同一个
/// 坏结果」（结构性问题）。后者不重试——工具包 44~62MB，重试 5 次就是白烧
/// 300MB 流量 + 几分钟干等，用户看到的还是同一个错。
#[derive(Debug, Clone)]
struct ToolErr {
    msg: String,
    /// 结构性失败：换源重下无意义（产物名不存在 / 包解不开 / 校验不过）。
    structural: bool,
}

impl ToolErr {
    fn net(msg: impl Into<String>) -> Self {
        Self { msg: msg.into(), structural: false }
    }
    fn structural(msg: impl Into<String>) -> Self {
        Self { msg: msg.into(), structural: true }
    }
}

impl From<String> for ToolErr {
    fn from(msg: String) -> Self {
        Self::net(msg)
    }
}

/// 下载工具 zip 产物到 {install_dir}/.{id}-update.{fp}.zip.{fp}.new。
///
/// 直链解析：资产表（**与检查更新同一套镜像源** + expanded_assets 片段）
/// → 模板没命中就在表里按形态发现 → 都不行才直拼约定 URL（零请求保底）。
/// 解析出直链后同样是「国内镜像 × 8 → PS 通道 → curl 直链」逐个尝试
/// （与自更新同一个 download_chain，只是校验函数换成 looks_like_zip）。
/// 返回 (zip 路径, 资产字节数)。
///
/// **tag 自愈**：传进来的 tag 只当作「探查来的候选」，本函数会先拿它探一次
/// 资产表；探不到（错的 tag 在 GitHub 上直接 404）或探到的表里没有我们要的
/// Windows 压缩包时，用发布列表把 tag 重新锚定到「真的带着产物」的那一个。
/// 这一步专门收拾 `tag_probes` 里任何一个源报错的情况——错 tag 拼出的直链必
/// 404，11 条候选链 × 5 轮重试就是几百 MB 白流量和几分钟干等，而多花的代价
/// 只是失败路径上一次发布列表请求（约百来 KB）。探查正常时不额外发请求。
///
/// 返回 (zip 路径, 资产字节数, **实际下载用的 tag**)。
fn download_tool_archive(
    spec: &ToolSpec,
    tag: &str,
    dest_dir: &Path,
    progress_tx: &std::sync::mpsc::Sender<(u64, u64)>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(PathBuf, u64, String), ToolErr> {
    let names = tool_asset_names(spec);
    // 资产表（镜像逐个试，与检查更新同一套源）。
    let mut tag = tag.to_string();
    let mut table: Option<AssetTable> = None;
    match fetch_release_assets(spec.repo, &tag) {
        Ok((src, t)) => {
            log_update(&format!("工具下载 {} 资产表来自 {src}（{} 项）", spec.label, t.len()));
            table = Some(t);
        }
        Err(e) => log_update(&format!("工具下载 {} 资产表探查失败（仍走直拼兜底）: {e}", spec.label)),
    }
    // 拿不到资产表、或表里没有我们要的压缩包 → tag 很可能报错了，用发布列表
    // 重新锚定。发布列表本身就内联了每个 release 的 assets，换 tag 的同时把
    // 资产表一起接过来，省掉第二次探查。
    if table.as_ref().is_none_or(|t| pick_tool_assets(t, &names).is_empty()) {
        match fetch_release_list(spec.repo) {
            Ok(releases) => match pick_release_tag_with_assets(&releases, &names) {
                Some(fixed) if fixed != tag => {
                    log_update(&format!(
                        "工具下载 {}：tag {tag} 取不到 Windows 压缩包，改用 {fixed}",
                        spec.label
                    ));
                    table = releases
                        .iter()
                        .find(|(tg, _)| *tg == fixed)
                        .map(|(_, t)| t.clone());
                    tag = fixed;
                }
                _ => {}
            },
            Err(e) => log_update(&format!("工具下载 {} 发布列表兜底失败: {e}", spec.label)),
        }
    }
    // 资产表里挑出候选；表没拿到就空着（下面直拼兜底）。
    let mut resolved: Vec<(String, String, u64)> =
        table.as_ref().map(|t| pick_tool_assets(t, &names)).unwrap_or_default();
    // 「有资产表但一个可用压缩包都挑不出」= 产物名/产物线对不上，与网络无关。
    let no_asset = table.is_some() && resolved.is_empty();
    // 兜底：表里没给出的模板名按约定 URL 直拼（零请求，镜像前缀照打）。
    for n in &names {
        if !resolved.iter().any(|(rn, _, _)| *rn == *n) {
            resolved.push((
                n.clone(),
                format!(
                    "https://github.com/{}/releases/download/{}/{}",
                    spec.repo, tag, n
                ),
                0,
            ));
        }
    }
    let mut errs: Vec<String> = Vec::new();
    for (name, url, total) in resolved {
        if cancel.load(Ordering::Relaxed) {
            return Err(ToolErr::net("下载已取消"));
        }
        let candidates = candidate_chains(&url);
        let asset_name = format!(".{}-update.zip", spec.id);
        log_update(&format!(
            "工具下载 {} {name} 逐个尝试 {} 个候选链（tag={tag}）",
            spec.label,
            candidates.len()
        ));
        match download_chain(
            &asset_name,
            // 分片指纹 = tag + 资产名：opencode 的 avx2 版与 baseline 版、
            // 以及不同 tag 之间，分片绝不互相续传（旧的分片名只按工具 id 命名，
            // `-C -` 会把两个不同的包首尾拼起来，尾部还照样有 PK\x05\x06，
            // 能过 looks_like_zip 却装出坏 exe）。
            &shard_fp(&format!("{tag}/{name}")),
            candidates,
            total,
            dest_dir,
            progress_tx,
            cancel,
            looks_like_zip,
        ) {
            Ok(p) => {
                log_update(&format!("工具下载 成功：{} {name} → {p}", spec.label));
                // 带上**实际用的** tag：自愈可能把它改过，调用方要按它报版本，
                // 否则装的是新版本、状态栏却写着一个下不下来的错版本号。
                return Ok((PathBuf::from(p), total, tag));
            }
            Err(e) => {
                log_update(&format!("工具下载 失败：{} {name}：{e}", spec.label));
                errs.push(format!("{name}: {e}"));
            }
        }
    }
    // 全败。分类只看「**我们手上有没有一个真的能用的直链**」：
    //  - 资产表拿到了却一个可用压缩包都挑不出（no_asset）→ 结构性：tag 已在上面
    //    用发布列表兜底过，仍挑不出说明上游真的改了产物名。换源重下多少次都是
    //    同一个结果（重试一次 = 再烧一个 60MB），不重试。
    //  - 其余（表里明明有产物、只是所有源都连不上 / 校验不过）→ 网络问题，
    //    值得按节奏重试。旧实现只拿 `table_ok`（=表取到了）判结构性，于是镜像
    //    集体超时这种纯网络故障也被判成「不再重试」，用户干等一轮就收手。
    let detail = format!("所有下载源失败：{}", errs.join("；"));
    if no_asset {
        return Err(ToolErr::structural(format!(
            "{detail}（已拿到 {tag} 的资产表，但里面没有 Windows 压缩包，上游可能改了产物名）"
        )));
    }
    Err(ToolErr::net(detail))
}

/// 暂存目录里找主 exe。发布方把整包套一层同名目录（pi-windows-x64/pi.exe）是
/// 常见做法，原实现只认 `stage/pi.exe`，一遇到就从「包结构不符」重下 5 次
/// 44MB 压缩包，最后还是同样的错。这里最多下探 MAX_EXE_DEPTH 层，找不到再说。
const MAX_EXE_DEPTH: usize = 3;

fn find_exe_in_stage(stage: &Path, exe_name: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, exe_name: &str, depth: usize) -> Option<PathBuf> {
        let direct = dir.join(exe_name);
        if direct.is_file() {
            return Some(direct);
        }
        if depth == 0 {
            return None;
        }
        let mut subs: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subs.sort(); // 目录序稳定：多个候选包时结果可复现
        subs.into_iter().find_map(|d| walk(&d, exe_name, depth - 1))
    }
    walk(stage, exe_name, MAX_EXE_DEPTH)
}

/// 把解压暂存目录里除主 exe 外的全部内容覆盖同步进安装目录（pi 用）。
/// 覆盖式：同名文件覆盖、缺失目录新建，**不删**任何已有内容——用户自装的
/// node_modules / 自定义 theme 等一律保留。返回失败条目数。
fn sync_tree(stage: &Path, install_dir: &Path, skip_name: &str) -> usize {
    fn walk(stage: &Path, rel: &Path, install_dir: &Path, skip_name: &str, fails: &mut usize) {
        let Ok(rd) = std::fs::read_dir(stage) else {
            *fails += 1;
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name();
            let name_s = name.to_string_lossy().into_owned();
            // 顶层的主 exe 已经由 install_update 装好了，跳过（它已不在 stage 里）。
            if rel.as_os_str().is_empty() && name_s == skip_name {
                continue;
            }
            let src = e.path();
            let dst = install_dir.join(rel.join(&name));
            if src.is_dir() {
                if std::fs::create_dir_all(&dst).is_err() {
                    *fails += 1;
                    continue;
                }
                walk(&src, &rel.join(&name), install_dir, skip_name, fails);
            } else if std::fs::copy(&src, &dst).is_err() {
                log_update(&format!("工具同步文件失败: {:?} → {dst:?}", src));
                *fails += 1;
            }
        }
    }
    let mut fails = 0;
    walk(stage, Path::new(""), install_dir, skip_name, &mut fails);
    fails
}

/// 把探查来的 tag **锚定到「真的带着该工具 Windows 压缩包」的那个 release**。
///
/// `tag_probes` 是「先成功先得」的快查询，**任何一个源报上来的 tag 都不作数**：
/// 它可能是 HTML 模板碎片、错误页、或者历史上混进来的「答非所问的版本源」
/// （jsDelivr 返回的是 npm 包版本，opencode 那边是 `2.0.20` 而 Release tag 是
/// `v1.18.33`）。这类 tag 有两个害处：
///  1. `version_newer` 恒真 → 状态栏/设置页反复冒出一个**点下去必定 404** 的
///     「有新版本」；
///  2. 下载端拼出的直链必然 404。
///
/// 判据只用**产物存在性**：拿 tag 探一次资产表，表里有我们要的 Windows 压缩包
/// 才认；否则用 `/releases` 发布列表（新 → 旧）找到第一个真带产物的 tag。
/// 探查正常时**不发第二个请求**（资产表探查就是那一个，命中即返回原 tag）；
/// 网络全挂时保持原样（不把「网络故障」升级成「报了个假版本号」，重试会再来）。
///
/// 检查阶段也跑这一层，是为了让**错误版本号根本没有机会显示到 UI 上**——
/// 之前只有下载端自愈，用户看到的是「检测出 X，点下载却失败」，正是本次事故的
/// 完整形状。代价是每次检查多一次镜像请求（命中即止）。
fn anchor_tag_on_assets(spec: &ToolSpec, probed: String) -> String {
    let names = tool_asset_names(spec);
    // 判据一：这个 tag 的 release 里真的有我们要的 Windows 压缩包 → 认，不多发请求。
    let suspect = match fetch_release_assets(spec.repo, &probed) {
        Ok((src, table)) if !pick_tool_assets(&table, &names).is_empty() => {
            log_update(&format!(
                "工具检查 {}: tag {probed} 带 Windows 压缩包（资产表来自 {src}）",
                spec.label
            ));
            false
        }
        Ok((src, _)) => {
            log_update(&format!(
                "工具检查 {}: tag {probed} 的资产表（{src}）里没有 Windows 压缩包，重新锚定",
                spec.label
            ));
            true
        }
        Err(e) => {
            log_update(&format!(
                "工具检查 {}: tag {probed} 资产表探查失败（{e}），尝试用发布列表纠正",
                spec.label
            ));
            true
        }
    };
    if !suspect {
        return probed;
    }
    // 判据二：发布列表（新 → 旧）里第一个真带产物的 tag。
    match fetch_release_list(spec.repo) {
        Ok(releases) => {
            if let Some(fixed) = pick_release_tag_with_assets(&releases, &names)
                && fixed != probed
            {
                log_update(&format!(
                    "工具检查 {}: tag {probed} → {fixed}（后者真带产物）",
                    spec.label
                ));
                return fixed;
            }
        }
        Err(e) => log_update(&format!(
            "工具检查 {}: 发布列表兜底失败（{e}），仍用 {probed}",
            spec.label
        )),
    }
    probed
}

/// Ctrl+(Shift+)Tab 的落点：**只在页签栏里的页签之间循环，不碰首页 / 设置页**。
///
/// 旧实现是在整个 `tabs` 上做 `(current ± 1) % len`，于是快捷键会把人从第一个
/// 项目页签直接甩到「首页」，再按一下又进「设置」——这两个是**页面**不是**页签**，
/// 手不离键盘想回到项目里得连按好几下，而且很容易在两个页面之间来回弹
/// （设置页里还嵌着大量控件，一进去连 Ctrl+Tab 的手感都变了）。
///
/// 规则（`fwd` = Ctrl+Tab 前进，`false` = Ctrl+Shift+Tab 后退）：
///  - 候选集 = 除 `Home` / `Settings` 外的全部页签（会话页签 + 重启/切换期间的
///    `Placeholder` 占位页签——占位页签就是一个真实的位置，允许落上去）；
///  - 当前页已在候选集里 → 在**候选集内部**走一步（不是整个 tabs 走一步）；
///  - 当前页是首页/设置 → 前进跳候选集**第一个**、后退跳**最后一个**，
///    第一次按就能进页签区，但**永远不会落在首页/设置上**；
///  - 候选集为空（还没开过任何项目）→ None，按键不做事。
///
/// 纯函数（只读 tabs，不碰 self），便于单测钉住行为。
fn tab_cycle_target(tabs: &[Tab], current: usize, fwd: bool) -> Option<usize> {
    let ring: Vec<usize> = (0..tabs.len())
        .filter(|i| !matches!(tabs[*i], Tab::Home | Tab::Settings))
        .collect();
    if ring.is_empty() {
        return None;
    }
    let Some(pos) = ring.iter().position(|i| *i == current) else {
        return Some(if fwd { ring[0] } else { *ring.last()? });
    };
    let n = ring.len();
    // 后退 = 逆序一步（(pos + n - 1) % n）；只有一个页签时原地不动。
    Some(ring[(pos + if fwd { 1 } else { n - 1 }) % n])
}

/// 检查单个工具（pi / opencode）的新版本：按 tool_dirs（默认本软件所在目录）
/// → PATH 定位 exe → `--version` 读本地版本 → 复用自更新的多源逐个探查拿
/// 最新 tag → **按产物存在性锚定 tag**（`anchor_tag_on_assets`）→
/// version_newer 比较。找不到 exe（未安装）时**仍然给出下载入口**：
/// 安装目录取「软件同级目录」（pi 走同名子目录），latest 留空由下载作业自己
/// 解析最新 tag（这样即便检查时网络不通也还有安装按钮，不用等下次检查）。
fn check_tool_update(idx: usize, dirs: Vec<PathBuf>, use_path: bool) -> ToolEvent {
    let spec = &TOOL_SPECS[idx];
    let Some(exe) = find_tool_exe(spec, &dirs, use_path) else {
        let install_dir = fresh_tool_dir(spec).unwrap_or_default();
        log_update(&format!(
            "检查更新 {}: 目录 {dirs:?}{} 中均未找到 {}，改为提供一键安装到 {install_dir:?}",
            spec.label,
            if use_path { " 与 PATH" } else { "" },
            spec.exe_name
        ));
        return ToolEvent::Checked {
            local: String::new(),
            latest: None,
            install_dir,
            missing: true,
        };
    };
    let install_dir = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let local = local_tool_version(&exe);
    log_update(&format!(
        "检查更新 {}: 本地 {local}（{exe:?}），安装目录 {install_dir:?}",
        spec.label
    ));
    match fetch_latest_tag(spec.repo) {
        Ok(tag) => {
            // 锚定：探查来的 tag 只是「某个源报上来的」，不确认它真带着我们要的
            // 产物就直接拿去比较版本，UI 上就会出现一个点下去必定 404 的版本号
            // （事故现场：报上来的是 npm 包版本 2.0.20，Release tag 却是 v1.18.33）。
            let tag = anchor_tag_on_assets(spec, tag);
            let latest = tag.trim_start_matches('v').to_string();
            // 本地版本读不出来时只报「已是最新」不如报「有更新」：点下载也就是
            // 装最新版，不会出事；反过来误报「有更新」不了才是真的漏升级。
            let newer = local.is_empty() || version_newer(&latest, &local);
            ToolEvent::Checked {
                local,
                latest: newer.then_some(tag),
                install_dir,
                missing: false,
            }
        }
        Err(e) => {
            log_update(&format!("检查更新 {} 失败: {e}", spec.label));
            ToolEvent::Checked {
                local,
                latest: None,
                install_dir,
                missing: false,
            }
        }
    }
}

/// 工具更新的后台作业：下载 → 解压 → 备份 .old → 替换 → 同步其余文件 →
/// 清理暂存。失败（下载源全挂 / 解压失败 / 产物损坏）3 秒后整链重来，
/// 连续失败超过 MAX_TOOL_ATTEMPTS 次收手（自更新是无限重试，工具这边多了
/// 一次就要重下 60MB 压缩包，不宜无限烧流量）。取消信号置位立即退出。
/// 事件通过 tool_tx 报回 UI。
///
/// tag 传 None = 本机还没装（检查时未检测到），版本号在作业内现查；这样即使
/// 检查更新时网络不通、没拿到 tag，状态栏那个「⬇ 安装」按钮照样能用。
fn run_tool_update(
    idx: usize,
    mut tag: Option<String>,
    install_dir: PathBuf,
    progress_tx: std::sync::mpsc::Sender<(u64, u64)>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: std::sync::mpsc::Sender<(usize, ToolEvent)>,
    redraw_tx: std::sync::mpsc::SyncSender<()>,
) {
    let spec = &TOOL_SPECS[idx];
    let label = spec.label;
    // 消息出口：统一成「{label}: …」前缀，UI 直接显示在状态栏。
    let sink = |msg: &str| {
        let _ = tx.send((idx, ToolEvent::Status(format!("{label}: {msg}"))));
        let _ = redraw_tx.try_send(());
    };
    if install_dir.as_os_str().is_empty() {
        sink("没有可写入的安装目录，已放弃");
        return;
    }
    // 首次安装（pi 装进 <软件目录>\pi\）时目录还不存在，先建好。
    if let Err(e) = std::fs::create_dir_all(&install_dir) {
        sink(&format!("创建安装目录 {install_dir:?} 失败: {e}"));
        return;
    }
    let stage = install_dir.join(format!(".{}-update-stage", spec.id));
    // 上次若因「被占用」没装上，暂存里已留有解好的 exe：校验通过就别再重下
    // 60MB 压缩包，直接进替换（用户只需先关掉占用的进程再点一次按钮）。
    let reuse = find_exe_in_stage(&stage, spec.exe_name).is_some_and(|s| looks_like_exe(&s));
    // 下载层可能把探查来的错 tag 自愈成「真带产物」的那个，完成文案/版本号
    // 一律用 effective_tag，不能拿 tag 报。
    let mut effective_tag = String::new();
    let mut attempt = 1u32;
    loop {
        if cancel.load(Ordering::Relaxed) {
            sink("下载已取消");
            return;
        }
        // 重试封顶：网络类失败重试有意义（源会恢复），但「包结构不对」这类
        // 永久性失败每轮都要重下 60MB，无限重试纯属烧流量——到顶就收手。
        if attempt > MAX_TOOL_ATTEMPTS {
            sink(&format!(
                "连续 {MAX_TOOL_ATTEMPTS} 次未能完成更新，已停止重试；请检查网络后手动点按钮重试"
            ));
            return;
        }
        // 0) 本机没装过（tag 为空）→ 现在查最新版本号；与下载同属网络环节，
        //    同样按 3 秒节奏重试、共用 MAX_TOOL_ATTEMPTS 的封顶。
        if tag.is_none() {
            match fetch_latest_tag(spec.repo) {
                Ok(t) => {
                    sink(&format!("正在安装 {t}…"));
                    tag = Some(t);
                }
                Err(e) => {
                    sink(&format!(
                        "查询最新版本号未成功（第 {attempt}/{MAX_TOOL_ATTEMPTS} 次）: {e}，3 秒后自动重试…"
                    ));
                    if sleep_until_cancel(&cancel, retry_backoff(attempt)) {
                        sink("下载已取消");
                        return;
                    }
                    attempt += 1;
                    continue;
                }
            }
        }
        let tag = tag.as_deref().unwrap_or_default();
        // 1) 下载 zip（候选链逐个尝试，产物 looks_like_zip + 字节数对账）；复用
        //    暂存时跳过。
        let zip: Option<std::path::PathBuf> = if reuse {
            sink("复用上次已解压的文件，直接重试替换…");
            None
        } else {
            match download_tool_archive(spec, tag, &install_dir, &progress_tx, &cancel) {
                Ok((zip, _total, used_tag)) => {
                    effective_tag = used_tag;
                    Some(zip)
                }
                Err(e) => {
                    if cancel.load(Ordering::Relaxed) {
                        sink("下载已取消");
                        return;
                    }
                    if e.structural {
                        // 产物名/包结构不对：换源重下多少次都是同一个结果，
                        // 不烧那 5×60MB，直接收手并说清楚。
                        sink(&format!("下载中断：{}；不再自动重试", e.msg));
                        return;
                    }
                    let wait = retry_backoff(attempt);
                    sink(&format!(
                        "下载失败（第 {attempt}/{MAX_TOOL_ATTEMPTS} 次）: {}，{} 秒后自动重试…",
                        e.msg,
                        wait.as_secs()
                    ));
                    // 可中断等待：取消即刻生效；被取消就直接收手。
                    if sleep_until_cancel(&cancel, wait) {
                        sink("下载已取消");
                        return;
                    }
                    attempt += 1;
                    continue;
                }
            }
        };
        // 2) 解压到暂存目录（与安装目录同卷，后续 rename 才是原子替换）。
        let drop_zip = |z: &Option<std::path::PathBuf>| {
            if let Some(p) = z {
                let _ = std::fs::remove_file(p);
            }
        };
        if let Some(z) = &zip {
            sink("正在解压…");
            if let Err(e) = extract_zip(z, &stage) {
                drop_zip(&zip);
                let _ = std::fs::remove_dir_all(&stage);
                // 包已过 looks_like_zip 校验，tar 与 PS 却都解不开 → 包本身不对
                // （拼坏的/换了压缩算法），重下同一个包没有意义。
                sink(&format!("解压中断：{e}；不再自动重试"));
                return;
            }
        }
        // 3) 替换：备份旧 exe 为 .old（copy，运行中的映像也能读），再走
        //    install_update 的快路径/慢路径/回滚三段式。主 exe 可能裹在
        //    一层同名目录里（pi-windows-x64/pi.exe），故在暂存里找而不是死认
        //    stage/pi.exe。
        let Some(staged_exe) = find_exe_in_stage(&stage, spec.exe_name) else {
            let _ = std::fs::remove_dir_all(&stage);
            drop_zip(&zip);
            sink(&format!(
                "压缩包里找不到 {}（包结构与预期不符），已停止重试",
                spec.exe_name
            ));
            return;
        };
        // 套壳目录里的主 exe 装到安装目录后，其余文件也从那一层同步，
        // 否则会在安装目录里凭空多出一层同名目录。
        let sync_root = staged_exe.parent().unwrap_or(&stage).to_path_buf();
        let final_exe = install_dir.join(spec.exe_name);
        let old_exe = install_dir.join(format!("{}.old", spec.exe_name));
        // 首次安装（本机之前没有这个 exe）：没有 .old 可备份，完成文案也换一套。
        let first_install = !final_exe.is_file();
        // copy 目标已存在则直接覆盖（.old 始终保留最近一版旧程序），失败不阻断替换。
        // 提示语不能含「失败」字样：自更新 UI 按关键字复位 downloading 状态。
        if let Err(e) = std::fs::copy(&final_exe, &old_exe) {
            log_update(&format!("工具 {label} 备份 .old 失败: {e}"));
        }
        match install_update(&staged_exe, &final_exe, &old_exe, &sink) {
            InstallOutcome::Done => {
                // 4) 其余文件覆盖同步（pi 的 assets/native/theme/docs…）。
                if spec.sync_tree {
                    sink("正在同步程序文件…");
                    let fails = sync_tree(&sync_root, &install_dir, spec.exe_name);
                    if fails > 0 {
                        log_update(&format!("工具 {label} 同步 {fails} 个文件失败"));
                    }
                }
                // 5) 清理暂存（下载包已装完，不再需要续传）。
                let _ = std::fs::remove_dir_all(&stage);
                drop_zip(&zip);
                let done_tag = if effective_tag.is_empty() { tag } else { &effective_tag };
                log_update(&format!(
                    "工具 {label} 更新完成：{done_tag} → {final_exe:?}（旧版已备份 {old_exe:?}）"
                ));
                let _ = tx.send((
                    idx,
                    ToolEvent::Done {
                        msg: if first_install {
                            // 具体路径/是否已入启动命令由 UI 拼（它才知道配置改动
                            // 结果）；这里只给一句短消息。
                            format!("{label} {done_tag} 已安装")
                        } else {
                            format!("{label} 已更新到 {done_tag}，重新启动 {label} 即可生效")
                        },
                        // 直接采信装上去的 tag，不再去跑 `--version`：某些版本
                        // 改了输出格式抠不出数字，那样按钮会永远停在“有新版本”。
                        version: done_tag.trim_start_matches('v').to_string(),
                    },
                ));
                let _ = redraw_tx.try_send(());
                return;
            }
            InstallOutcome::Occupied => {
                // 目标被占用（{label} 正在本软件的内嵌终端里跑着、或被杀软持有）：
                // 保留解压暂存与下载包，下次点下载可直接重试替换（不再重下包）。
                // 必须让状态复位、按钮重新出现：否则「下载中」会一直卡住，
                // 用户点「✕ 取消」也无从下手（线程已退出），只能重启软件。
                sink(&format!(
                    "安装被占用：正式名 {final_exe:?} 一直被其他进程占用（多为本软件内正在运行的 {label} 会话或杀软扫描），已解好的文件保留在 {staged_exe:?}；关掉占用的 {label} 后再点一次下载即可，无需重新下载"
                ));
                let _ = tx.send((idx, ToolEvent::Finished));
                return;
            }
            InstallOutcome::BadDownload => {
                // 解压出的 exe 不合法：清掉暂存与压缩包残留。包已过 zip 校验、
                // 解压也成功，却拿不到合法 PE——同一个包重下多少次都一样
                // （镜像改写了内容 / 包里那版 exe 本身就是坏的），不重试。
                drop_zip(&zip);
                let _ = std::fs::remove_dir_all(&stage);
                sink("下载到损坏文件（压缩包里的 exe 不是有效程序），已停止重试");
                return;
            }
        }
    }
}

/// 点分数字版本比较（如 2025.06.30.0001），a > b 返回 true。
fn version_newer(a: &str, b: &str) -> bool {
    let pa: Vec<u64> = a.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    let pb: Vec<u64> = b.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 本地版本的兜底值：`--version` 输出认不出（空）或认出来的比「我们亲手装的
/// 那个 tag」还旧时，用后者。
///
/// 为什么：pi / opencode 的 `--version` 格式会变，认不出时旧逻辑把空串当
/// “版本未知”→ 同一个 tag 被反复报成“有新版本”，用户刚点完下载，按钮又冒出
/// 来，看着像没装成功。装成功过就以装上的 tag 为准。纯函数，便于单测。
fn local_version_with_floor(parsed: &str, installed: Option<&str>) -> String {
    match installed {
        Some(v) if parsed.is_empty() || version_newer(v, parsed) => v.to_string(),
        _ => parsed.to_string(),
    }
}

/// 项目目录存在性（带 TTL 缓存）。每帧 UI 都要显示目录状态，直接 is_dir()
/// 是每帧每项目一次文件系统调用；2 秒内复用上次结果，过期才重新采样。
/// 独立成函数拿 cache 参数而非 &mut self：调用点在遍历 self.config 的
/// 循环里，避免借用冲突。
fn dir_exists(cache: &mut HashMap<String, (bool, Instant)>, path: &str) -> bool {
    const TTL: std::time::Duration = std::time::Duration::from_secs(2);
    let now = Instant::now();
    if let Some(&(ok, at)) = cache.get(path)
        && now.duration_since(at) < TTL
    {
        return ok;
    }
    let ok = Path::new(path).is_dir();
    cache.insert(path.to_string(), (ok, now));
    ok
}
/// 首页项目列表排序模式。升/降序仅作用于显示层（按名称首字），
/// 不修改 config 里的原始数据；「默认顺序」展示原始数据顺序，
/// 自定义排序通过拖拽重排完成（会持久化修改原始数据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectSort {
    /// 原始数据顺序（可拖拽自定义重排）。
    Default,
    /// 按名称首字升序，仅显示层排序。
    NameAsc,
    /// 按名称首字降序，仅显示层排序。
    NameDesc,
}

/// 后台 spawn 完成的结果消息：(result, is_restore, saved_index)。
/// saved_index 仅恢复时有效（保存时的 dirs 索引），用于按原序插入页签。
type SpawnResult = (Result<Session, String>, bool, Option<usize>);

pub struct ClientApp {
    pub config: config::Config,
    pub tabs: Vec<Tab>,
    pub current: usize,
    pub screen: Screen,
    pub selected_project: usize,
    pub settings_command: String,
    pub settings_commands: Vec<String>,
    pub settings_new_command: String,
    /// 编辑命令时的索引和缓冲区（Some(i) = 内联编辑第 i 行）。
    pub settings_edit_idx: Option<usize>,
    pub settings_edit_buffer: String,
    /// 设置页里编辑的「工具更新目录」列表（工作副本，改动写回 config）。
    pub settings_tool_dirs: Vec<String>,
    /// 新增工具目录的输入框。
    pub settings_new_tool_dir: String,
    pub status: Option<String>,
    pub config_path: PathBuf,
    pub term_focused: bool,
    update_latest: Option<String>,
    check_tx: Sender<(String, Option<String>)>,
    update_rx: Receiver<(String, Option<String>)>,
    /// 下载进度：后台线程通过通道报告 (bytes_downloaded, total_bytes)。
    download_progress_rx: Option<Receiver<(u64, u64)>>,
    /// 当前正在下载更新（显示进度条，禁用下载按钮）。
    downloading: bool,
    /// 取消下载信号：用户点击取消时置 true，下载线程检测后退出。
    cancel_download: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 下载完成、新 exe 已替换，等待用户重启应用（显示重启按钮）。
    update_done: bool,
    /// 本次更新安装到的正式名 exe 路径：安装兜底可能把运行映像 rename 成 .old，
    /// 重启必须仍指向正式名（新版本），不能依赖 current_exe() 现算。
    update_final: Option<PathBuf>,
    /// pi / opencode 的更新状态（索引对应 TOOL_SPECS 顺序）。
    tools: Vec<ToolState>,
    /// 工具更新事件通道：(工具下标, 事件)。检查/下载/解压/替换全在后台线程。
    tool_tx: Sender<(usize, ToolEvent)>,
    tool_rx: Receiver<(usize, ToolEvent)>,
    /// 每个工具独立的下载取消信号（与自更新的 cancel_download 同构）。
    tool_cancel: Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub input: Option<InputDialog>,
    pub confirm: Option<ConfirmDialog>,
    redraw_tx: std::sync::mpsc::SyncSender<()>,
    /// 主题切换后的延迟全量重绘时刻：立即清缓存之外，等子进程重绘尘埃落定
    /// 后（约 100ms）再清一遍所有会话缓存并强制整帧，兜住晚到的脏状态。
    theme_settle_at: Option<std::time::Instant>,
    /// 页签拖动中记录的源索引；松手那帧消费掉并执行重排（None = 未在拖动）。
    drag_tab: Option<usize>,
    /// 项目列表拖动中记录的源索引；松手那帧消费掉并执行重排（None = 未在拖动）。
    drag_project: Option<usize>,
    /// 设置页命令列表拖动中记录的源索引；松手那帧消费掉并执行重排（None = 未在拖动）。
    drag_command: Option<usize>,
    /// 点击「启动」待 spawn 的会话（下一帧会话页签布局里按精确尺寸启动）。
    pending_launch: Vec<PendingLaunch>,
    /// 后台线程正在 spawn 的会话：(title, is_restore, relaunch_tab_index)。
    spawning: Vec<(String, bool, Option<usize>)>,
    /// 后台 spawn 完成的结果通道：(result, is_restore, saved_index)。
    /// saved_index 仅恢复时有效（保存时的 dirs 索引），用于按原序插入页签。
    spawn_rx: Option<Receiver<SpawnResult>>,
    /// 启动时待恢复的会话（同样推迟到首帧会话页签布局）。
    pending_restore: Vec<PendingLaunch>,
    /// 恢复的会话处理完后一次性应用上次激活页签（消费一次）。
    restore_active: Option<usize>,
/// 退出时设置页签开着：启动时**固定**插在首页之后（消费一次）。
    restore_settings: bool,
    /// 会话页签区（首页/设置右侧那一段）的横向滚动偏移（px）。
    /// 只滚会话页签：首页与设置常驻左侧，页签过多时才需要滚。
    tab_scroll_x: f32,
    /// 启动恢复的会话按保存序暂存于此（save_i 槽位），全部完成后按序插入页签。
    /// 后台 spawn 完成顺序随机，逐条插入会因先到的高索引越界 panic，故先攒槽。
    restore_slots: Vec<Option<Result<Session, String>>>,
    /// 终端上次渲染的真实网格尺寸：重开崩溃页签/切启动命令时按它 spawn，
    /// 避免再走 80x24 → 首帧 resize 的错尺寸启动路径。
    last_term_size: (u16, u16),
    /// 原生窗口句柄：每帧轮询确保标题栏始终深色（防御 WM_SETTINGCHANGE 重置）。
    titlebar_hwnd: isize,
    /// 上一次实际生效的主题深浅：跟随系统时每帧对比，系统主题变化即重应用。
    last_theme_dark: bool,
    /// 主题切换后延迟补设黑色标题栏的时刻（等 winit 应用完帧尾的 SetTheme 命令）。
    titlebar_restore_at: Option<std::time::Instant>,
    /// 上次配置成功落盘时刻：窗口拖动/缩放变化时按它节流（见 CONFIG_SAVE_INTERVAL）。
    last_config_save: std::time::Instant,
    /// 连续落盘失败标记：每个失败区间只在状态栏提示一次，避免反复刷屏。
    config_save_failed: bool,
    /// 终端会话用 egui 上下文做 OSC 52 剪贴板写入并传给后台解析线程。
    ctx: egui::Context,

    /// 项目目录存在性缓存：path → (是否存在, 采样时间)。首页列表与详情页
    /// 每帧都要画"目录存在/不存在"，直接 is_dir() 是每帧每项目一次磁盘
    /// 调用（网络盘上明显拖帧），TTL 内复用结果。
    dir_exists_cache: HashMap<String, (bool, Instant)>,
    /// 页签标题排版宽度缓存：title → 宽度。标题几乎不变，避免每页签每帧
    /// 一次全量 layout_no_wrap 测宽。
    title_width_cache: HashMap<String, f32>,
    /// pi 模型配置（编辑态）。
    pi_models: config::ModelsConfig,
    /// oh-my-pi 模型配置（编辑态）。
    omp_models: config::ModelsConfig,
    /// opencode 供应商配置（编辑态；与 pi/omp 的 schema 不同，单列一套）。
    opencode_models: config::OcProviders,
    /// opencode 配置读取失败的原因（如 jsonc 里写了注释）。非空时设置页只
    /// 提示、不提供编辑与写回：宁可让人手改，也不能洗掉原文件。
    opencode_load_err: Option<String>,
    /// opencode 顶层 `model`（默认模型，程序不修改，仅展示）。
    opencode_default_model: String,
    /// 模型设置当前页签：0=pi，1=oh-my-pi，2=opencode。
    model_settings_tab: usize,
    /// 设置页当前展开的那一块（手风琴）：None = 全收起。它是互斥的**唯一真相**，
    /// 每次渲染都用它去强制各块的开合（`settings_section`），并持久化在 egui data 里
    /// （重启后还是上次那块开着）。
    open_settings_sec: Option<&'static str>,
    /// 是否已从 egui data 读过一次 `open_settings_sec`（避免每次进设置页都重读，
    /// 把用户刚点开的块拉回旧值）。
    settings_sec_loaded: bool,
    /// 供应商名编辑缓冲（页签, 当前键, 输入缓冲）：失焦前不重命名、不落盘。
    provider_name_edit: Option<(usize, String, String)>,
    /// 模型 context/max 数字编辑缓冲（页签, 供应商键, 模型行号, context, max）：
    /// 失焦前不写回，避免清空后重打拼接出错误数值。
    model_num_edit: Option<(usize, String, usize, String, String)>,
    /// 后台重绘跳帧计数器：后台页签收到 redraw 信号时累计，达到跳帧阈值才真正重绘。
    bg_frame: u64,
    /// 首页项目列表搜索过滤文本。
    search_query: String,
    /// 首页是否显示已隐藏的项目。
    show_hidden: bool,
    /// 首页项目列表排序模式（显示层排序，不修改原始数据）。
    project_sort: ProjectSort,
    /// 待异步重新启动/切换命令的页签。
    pending_relaunch: Vec<PendingRelaunch>,
    /// 本帧的滚轮输入（raw_input_hook 统计原始事件，见 terminal::Wheel）。
    /// 跨帧不保留：指针不在终端上就该丢弃，否则会攒成延迟滚动。
    wheel: terminal::Wheel,
}


/// 页签状态图标（纯函数；参数与 [`Session`] 的输出时间戳一一对应，便于逐项钉住）。
///
/// 一律只看**通用输出启发式**，不按 agent 分家：不追 pi 的会话 JSONL、不查
/// opencode 的 DB、不认 TUI 是不是全屏动画（曾按 pi/opencode 各写一套权威状态
/// 通道，实测两边都判错：pi 会话文件与终端 cwd 对不上就永远读不到状态，opencode
/// 得另起进程查 DB；而图标语义只要求「大概在跑 / 大概跑完了」，输出启发式已经
/// 够用，少一条通道就少一份错乱的来源）。
///
/// 只看两块时间戳，都由 reader 刷：
/// `last_real` = 最近一块**实质内容**（非动画块）；`last_anim` = 最近一块**纯
/// 动画**输出（spinner/时钟/进度条），0 = 从未有过动画。
///
/// 判定链（自上而下，先到先得）：
/// ❌ 进程已退出 / 🔄 启动加载中 → 压过一切。
/// 🔄 最近 3s 内有实质内容 **或** 画面还在高频动 → 运行中。
///   · 内容窗口走 last_real：空闲的全屏 TUI（停在输入框）只刷动画，
///     拿全量输出当「在跑」会让页签永远 🔄（最初那个 bug）。
///   · 动画通道（高频 spinner/时钟/进度条）覆盖「长命令一行字都不吐」：
///     思考/跑命令期间界面在动 ⇒ 准确识别为不是结束状态（见 [`ANIM_BUSY_MS`]）。
///   · ever_output 门挡住「刚 spawn 的新鲜时间戳」假闪；最近 1.5s 打字、
///     500ms 滚轮回显不算（那是用户自己弄出来的，只压内容窗口不压动画通道）。
/// ✅ 有实质输出、用户没查看、已静默 ≥3s、且画面不在高频动 → 完成/待查看。
/// 其余（已查看 / 从无实质输出）→ 空。
///
/// 🔄 与 ✅ 由同一个「画面在动」量分开，互斥且不重叠：动画通道点亮 🔄 的同时
/// 关掉 ✅，所以不会出现「思考中却亮 ✅」也不会「跑完了还常亮 🔄」。
#[allow(clippy::too_many_arguments)]
fn tab_icon(
    exited: bool,
    loading: bool,
    ever_output: bool,
    count: u32,
    viewed: bool,
    last_real: u64,
    now_ms: u64,
    last_input: u64,
    last_scroll: u64,
    last_anim: u64,
) -> Option<&'static str> {
    // 画面还在高频动 ⇒ 界面在动（spinner/时钟/进度条），不是「静默跑完了」。
    // 一次采样定生死：spinner 每 ~100ms 一帧，1s 门限下采样恒落在窗口内；
    // 空闲时光标闪烁稀疏，绝大多数采样落在窗口外 → 照常判完成（见 ANIM_BUSY_MS）。
    let screen_busy = last_anim != 0 && now_ms.saturating_sub(last_anim) < ANIM_BUSY_MS;
    if exited {
        return Some("❌");
    }
    if loading {
        return Some("🔄");
    }
    // 用户驱动例外：最近 1.5s 内键盘输入、或 500ms 内转发滚轮——其直接引发
    // 的回显/整屏重绘是用户操作引起、不是任务在跑 → 跳过运行中判定。命令
    // 真实输出晚于窗口即照常判 🔄（慢命令几乎总是超出窗口）。
    let typing = last_input != 0 && now_ms.saturating_sub(last_input) < INPUT_ACTIVE_MS;
    let scroll_echo = last_scroll != 0 && now_ms.saturating_sub(last_scroll) < SCROLL_ECHO_MS;
    // 有实质内容（最近一块距今 ≤3s）→ 运行中。动画重绘不算「在跑」（那是界面
    // 在动，内容没变——见上面 🔄 的两条通道）；本地滚动/翻页不产生输出，不会
    // 点亮它。ever_output 门：从未收到任何输出的会话（时间戳仍是 spawn 的初始
    // 值）不因「初始即新鲜」假闪 🔄，启动加载由 loading 分支负责。
    let fresh_content = ever_output && now_ms.saturating_sub(last_real) <= OUTPUT_END_MS;
    if !typing && !scroll_echo && fresh_content {
        return Some("🔄");
    }
    // 画面高频动 → 运行中（思考/长工具/静默构建）。不受打字·滚轮例外影响：
    // 那两条只压「人工回显」，而动画不是回显。
    if screen_busy {
        return Some("🔄");
    }
    // 无内容 ≥3s → 完成；有实质输出（count>0）且用户未查看才亮 ✅。
    // 陈旧判据与 🔄 互补：打字期内新鲜回显不落 ✅，只能走空（旧代码语义）。
    // screen_busy 已在上面被 🔄 吃掉，这里自然不会误亮 ✅。
    if count > 0
        && !viewed
        && !screen_busy
        && now_ms.saturating_sub(last_real) > OUTPUT_END_MS
    {
        return Some("✅");
    }
    // 空：无内容可看 / 已查看过。
    None
}

/// [`tab_icon`] 的图标码（存进 Session.state_icon 的低 2 位）。
const ICON_EMPTY: u8 = 0;
/// ✅ 完成/待查看。
const ICON_DONE: u8 = 1;
/// 🔄 运行中（或加载中，渲染层直接给 🔄）。
const ICON_BUSY: u8 = 2;
/// ❌ 已退出（渲染层直接给 ❌，不进快照）。
const ICON_ERR: u8 = 3;
/// state_icon 的位掩码：低 2 位图标码。
const SNAP_CODE_MASK: u8 = 0b11;
/// state_icon 的位：判定那一刻进程是否已退出。
const SNAP_EXITED: u8 = 0b100;
/// state_icon 的位：判定那一刻是否加载中。
const SNAP_LOADING: u8 = 0b1000;

fn icon_code(icon: Option<&'static str>) -> u8 {
    match icon {
        None => ICON_EMPTY,
        Some("✅") => ICON_DONE,
        Some("🔄") => ICON_BUSY,
        Some("❌") => ICON_ERR,
        Some(_) => ICON_EMPTY,
    }
}

fn icon_from_code(code: u8) -> Option<&'static str> {
    match code {
        ICON_DONE => Some("✅"),
        ICON_BUSY => Some("🔄"),
        ICON_ERR => Some("❌"),
        _ => None,
    }
}

/// 「现在该不该重算状态快照」（纯函数，把频率语义钉在测试里）。
///
/// 默认走 [`STATE_CHECK_MS`] 门限（最快 1s 一次）；三条**事件驱动**的例外立即
/// 放行，否则会迟钝到能看出来：
/// ① 首次（`last_check == 0`）；
/// ② 退出/加载态与快照里的位不一致（❌ 与启动 🔄 不能等下一秒）；
/// ③ 出现了新的**实质**内容（命令刚跑起来，🔄 必须立刻亮；动画不算，否则
/// spinner 会把门限彻底顶掉，等于没降频）。
#[allow(clippy::too_many_arguments)]
fn state_due(
    last_check_ms: u64,
    now_ms: u64,
    real_out_ms: u64,
    snap: u8,
    exited: bool,
    loading: bool,
) -> bool {
    if last_check_ms == 0 || now_ms.saturating_sub(last_check_ms) >= STATE_CHECK_MS {
        return true;
    }
    if (snap & SNAP_EXITED != 0) != exited || (snap & SNAP_LOADING != 0) != loading {
        return true;
    }
    real_out_ms > last_check_ms
}

/// App 侧的快照刷新：把图标 + 完成态**一起**算进 Session 的原子里。
///
/// 页签图标（tab_bar）与「任务完成」通知（update_done_states）都只读这份快照，
/// 1s 门限也只在这里生效一次——两个消费者因此永远不会互相矛盾。
fn refresh_tab_state(s: &crate::session::Session, now_ms: u64) {
    let last_check = s.state_check_ms.load(Ordering::Relaxed);
    let snap = s.state_icon.load(Ordering::Relaxed);
    let exited = s.exited.load(Ordering::Acquire);
    let loading = s.loading_active(now_ms);
    let real_out = s.last_real_output_ms.load(Ordering::Relaxed);
    if !state_due(last_check, now_ms, real_out, snap, exited, loading) {
        return;
    }
    s.state_check_ms.store(now_ms, Ordering::Relaxed);
    let last_anim = s.last_anim_ms.load(Ordering::Relaxed);
    let code = icon_code(tab_icon(
        exited,
        loading,
        s.ever_output.load(Ordering::Relaxed),
        s.output_count.load(Ordering::Relaxed),
        !s.has_been_viewed.load(Ordering::Relaxed),
        real_out,
        now_ms,
        s.last_input_ms.load(Ordering::Relaxed),
        s.last_scroll_ms.load(Ordering::Relaxed),
        last_anim,
    ));
    let flags = ((exited as u8) * SNAP_EXITED) | ((loading as u8) * SNAP_LOADING);
    s.state_icon.store(code | flags, Ordering::Relaxed);
    // 完成态（通知判据）与图标同源同刻：退出/加载中不算、动画高频动不算、
    // 实质内容静默 ≥3s 才算。✅ 额外要求「有实质输出 + 未查看」，通知那侧则
    // 另有 out_bytes / viewed / 节流等门槛（见 update_done_states）。
    let done = !exited
        && !loading
        && (last_anim == 0 || now_ms.saturating_sub(last_anim) >= ANIM_BUSY_MS)
        && now_ms.saturating_sub(real_out) > OUTPUT_END_MS;
    s.state_done.store(done, Ordering::Relaxed);
}

/// 读快照出图标：退出/加载中即时纠正（事件驱动，不受 1s 门限约束）；
/// 已查看则把 ✅ 立刻抹掉（用户刚看过就不该还亮着待查看）。
fn tab_icon_from_snap(s: &crate::session::Session, viewed: bool) -> Option<&'static str> {
    let snap = s.state_icon.load(Ordering::Relaxed);
    if snap & SNAP_EXITED != 0 {
        return Some("❌");
    }
    if snap & SNAP_LOADING != 0 {
        return Some("🔄");
    }
    let code = snap & SNAP_CODE_MASK;
    if viewed && code == ICON_DONE {
        return None;
    }
    icon_from_code(code)
}

/// Tab 的轻量投影（Tab 持有 Session，坐标换算只需这四类）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabKind {
    Home,
    /// 未退出的会话：会持久化并恢复。
    Alive,
    /// 已退出的会话 / 重启占位：不恢复，坐标中不占位。
    Gone,
    Settings,
}

/// 计算持久化的 (active, settings_pos)，坐标系 = 恢复后的页签数组：
/// 先推 [Home] + 存活会话，再在 settings_pos 处插入 Settings，最后
/// current = active（见 ui() 恢复流程）。已退出页签不恢复，直接存
/// self.current / 满页签索引会整体左移错位（active 落到隔壁页签）。
/// settings_pos 是**插入前**坐标，active 是**插入后**最终坐标。
///
/// settings_pos 恒为 1：设置页签固定插在首页之后（见 `open_settings`）。
/// 放在会话中间既难找，拖动后的位置还会在重启时丢掉（拖动只改内存，不落盘）。
fn restore_coords(kinds: &[TabKind], current: usize) -> (usize, usize) {
    let settings_pos = 1usize;
    let settings_open = kinds.contains(&TabKind::Settings);
    let mut pre = 1usize; // 下一个「插入前」槽位（1 = Home 之后）
    let mut active = 0usize; // 0 = Home / 无匹配
    for (i, k) in kinds.iter().enumerate() {
        if i == current && *k != TabKind::Home {
            active = if *k == TabKind::Settings {
                settings_pos
            } else if settings_open && pre >= settings_pos {
                pre + 1
            } else {
                pre
            };
        }
        if *k == TabKind::Alive {
            pre += 1;
        }
    }
    (active, settings_pos)
}

/// 页签栏里页签内外的固定空隙（绘制与滚动量算共用一份数字，避免两处漂移）。
const TAB_GAP: f32 = 4.0;

/// 一行页签的横向度量（绘制公式与滚动量算共用）。
#[derive(Clone, Copy)]
struct StripMetrics {
    /// 状态图标槽宽（所有页签相同）。
    slot_w: f32,
    /// 「×」宽（占位页签没有 ×）。
    close_w: f32,
    /// 页签最小内容宽（≈四个汉字）。
    min_width: f32,
    /// 页签**之间**的间距：绘制的 add_space(TAB_GAP) + 父布局 item_spacing.x。
    gap: f32,
    /// 页签 Frame 的左右内边距之和。
    pad: f32,
}

/// 会话页签区的排布结果：每个下标在内容坐标系里的 `[x, x+w]`，以及内容总宽。
struct StripGeom {
    spans: Vec<(usize, f32, f32)>,
    content_w: f32,
}

/// 纯计算：把一行页签排成 `[x, x+w]`（相对内容左缘）。`items` = (下标, 标题宽, 有无 ×)。
///
/// 必须与绘制里 Frame 内的排版一致——差几像素只会让滚动范围略偏，不会错位
/// （真正的定位一直由 egui 布局负责，这里只用来算裁剪范围与跟随位置）。
fn strip_geom(items: &[(usize, f32, bool)], m: StripMetrics) -> StripGeom {
    let mut spans = Vec::with_capacity(items.len());
    let mut x = 0.0f32;
    for &(i, title_w, has_close) in items {
        if !spans.is_empty() {
            x += m.gap;
        }
let inner = m.slot_w + TAB_GAP + title_w
            + if has_close { TAB_GAP + m.close_w } else { 0.0 };
        // 占位页签（无 ×）的补白公式比会话页签少减一个间距，最小宽度也跟着少一个
        // —— 否则滚动范围会比实际画出来的宽，末尾多出一截空白。
        let min_w = m.min_width - if has_close { 0.0 } else { TAB_GAP };
        let w = inner.max(min_w) + m.pad;
        spans.push((i, x, w));
        x += w;
    }
    StripGeom { spans, content_w: x }
}

/// 把 `[x, x+w]` 挪进视口：整体可见就不动；左出界贴左、右出界贴右。
///
/// 单个页签比视口还宽时只能贴左（两端都露不全，贴左保前缀可见）。
fn offset_to_show(off: f32, view_w: f32, content_w: f32, x: f32, w: f32) -> f32 {
    let max_off = (content_w - view_w).max(0.0);
    let off = off.clamp(0.0, max_off);
    if w >= view_w {
        return 0.0;
    }
    let shown = if x < off {
        x
    } else if x + w > off + view_w {
        x + w - view_w
    } else {
        off
    };
    shown.clamp(0.0, max_off)
}

/// 同页签 10s 一条的通知节流（TOAST_MIN_INTERVAL_MS）。通过即占位时间戳——
/// 后续被「用户正盯着」「启动宽限」「已通知」分支静默吞掉也照占，宁可少弹。
fn allow_toast(s: &Session, now_ms: u64) -> bool {
    let last = s.last_toast_ms.load(Ordering::Relaxed);
    let allowed = last == 0 || now_ms.saturating_sub(last) >= TOAST_MIN_INTERVAL_MS;
    if allowed {
        s.last_toast_ms.store(now_ms, Ordering::Relaxed);
    }
    allowed
}

impl ClientApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        // 移除 egui 默认的 Ctrl+Q（Cmd-Q）退出快捷键：它在首页等无消费控件处
        // 会发送 ViewportCommand::Close 关闭整个应用。清空后仅剩系统级关闭
        // （标题栏 × / Alt+F4），终端页签内 Ctrl+Q 仍照常转发给子进程（0x11）。
        cc.egui_ctx
            .options_mut(|o| o.quit_shortcuts.clear());
        let mut config = config::load();
        // 启动时把「本软件同级目录下的 pi / opencode」写进默认配置（路径由
        // current_exe 推得，各机器自用、不写死）；补了内容就顺手落盘一次。
        if fill_default_tool_paths(&mut config) {
            let _ = config::save(&config);
        }
        let initial_dark = config.settings.dark_mode;
        apply_theme(
            &cc.egui_ctx,
            if config.settings.follow_system {
                #[cfg(target_os = "windows")]
                { query_windows_dark_mode() }
                #[cfg(not(target_os = "windows"))]
                { matches!(cc.egui_ctx.system_theme(), None | Some(egui::Theme::Dark)) }
            } else {
                config.settings.dark_mode
            },
        );
        let titlebar_hwnd = hwnd_of(cc);
        // 手动设一次保证首帧即黑。再发一次 SetTheme(Dark)：winit 0.30 的 set_theme
        // 不更新内部 preferred_theme（build 时为 None），WM_SETTINGCHANGE 时它会把
        // 窗口重应用为系统默认浅色——真正的兜底在 logic() 每帧轮询补设（见下）。
        set_titlebar_theme(titlebar_hwnd);
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
        let config_path = config::config_path();
        let settings_command = config.settings.tui_command.clone();
        let settings_commands = config.settings.tui_commands.clone();
        let settings_tool_dirs = config.settings.tool_paths.clone();
        // opencode 配置：读失败（如 jsonc 带注释）时只记错、设置页不提供写回。
        let (opencode_models, opencode_load_err) = match config::load_opencode_providers() {
            Ok(c) => (c, None),
            Err(e) => (config::OcProviders::default(), Some(e)),
        };
        let opencode_default_model = config::opencode_default_model();
        // 恒定帧率渲染，无需唤醒通道；保留 sender 供历史代码 try_send（无 receiver 时直接报错，不阻塞）。
        let redraw_tx = std::sync::mpsc::sync_channel(1).0;
        let ctx = cc.egui_ctx.clone();
        let (check_tx, update_rx) = std::sync::mpsc::channel();
        let (tool_tx, tool_rx) = std::sync::mpsc::channel();
        let saved_tabs = config.tabs.clone();
        let saved_active = config.tabs.active;
        let mut app = Self {
            config,
            tabs: vec![Tab::Home],
            current: 0,
            screen: Screen::Main,
            selected_project: 0,
            settings_command,
            settings_commands,
            settings_new_command: String::new(),
            settings_edit_idx: None,
            settings_edit_buffer: String::new(),
            settings_tool_dirs,
            settings_new_tool_dir: String::new(),
            status: Some("在左侧选择项目并点击「启动」启动内嵌终端页签。".to_string()),
            config_path,
            term_focused: false,
            update_latest: None,
            download_progress_rx: None,
            downloading: false,
            cancel_download: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            update_done: false,
            update_final: None,
            tools: (0..TOOL_SPECS.len()).map(|_| ToolState::new()).collect(),
            tool_cancel: (0..TOOL_SPECS.len())
                .map(|_| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .collect(),
            input: None,
            confirm: None,
            check_tx,
            update_rx,
            tool_tx,
            tool_rx,
            redraw_tx,
            theme_settle_at: None,
            drag_tab: None,
            drag_project: None,
            drag_command: None,
            pending_launch: Vec::new(),
            restore_slots: Vec::new(),
            pending_restore: Vec::new(),
            spawning: Vec::new(),
            spawn_rx: None,
restore_active: None,
        restore_settings: false,
        tab_scroll_x: 0.0,
            last_term_size: (80, 24),
            titlebar_hwnd,
            last_theme_dark: initial_dark,
            titlebar_restore_at: None,
            last_config_save: std::time::Instant::now(),
            config_save_failed: false,

            ctx,
            dir_exists_cache: HashMap::new(),
            title_width_cache: HashMap::new(),
            pi_models: config::load_pi_models(),
            omp_models: config::load_omp_models(),
            opencode_models,
            opencode_load_err,
            opencode_default_model,
            model_settings_tab: 0,
            open_settings_sec: None,
            settings_sec_loaded: false,
            provider_name_edit: None,
            model_num_edit: None,
            bg_frame: 0,
            search_query: String::new(),
            show_hidden: false,
            project_sort: ProjectSort::Default,
            pending_relaunch: Vec::new(),
            wheel: terminal::Wheel::default(),
        };

        // 恢复上次退出时打开中的终端页签：目录仍存在则重新拉起 TUI 会话。
        // 不在构造期 spawn：此时窗口未布局，只有估算尺寸，会走错尺寸启动路径；
        // 收集成 pending_restore，等首帧会话页签布局里按精确尺寸启动（见 ui()）。
        let default_cmd = app.config.settings.tui_command.clone();
        for (i, d) in saved_tabs.dirs.iter().enumerate() {
            if !Path::new(d).is_dir() {
                continue;
            }
            let name = app
                .config
                .projects
                .iter()
                .find(|p| p.path == *d)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| {
                    Path::new(d)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| d.clone())
                });
            let cmd = saved_tabs.cmds.get(i).cloned().filter(|c| !c.is_empty()).unwrap_or_else(|| default_cmd.clone());
            app.pending_restore.push(PendingLaunch {
                title: name,
                dir: d.clone(),
                cmd,
            });
        }
if saved_tabs.settings_open {
            // 位置不再持久化：设置页签固定在首页之后（restore_coords 恒返回 1）。
            app.restore_settings = true;
        }
        if !app.pending_restore.is_empty() || app.restore_settings {
            // 注意：升级前的旧配置坐标含退出页签/设置页签偏移，首启可能落错
            // 一格——旧值无法换算（配置没存退出标记），落错时切回正确页签后
            // 第一次保存即写入 restore_coords 新格式，此后恢复正确（一次性）。
            app.restore_active = Some(saved_active);
        }
        // 每次启动自动检查一次更新。
        app.check_updates(true);
        app
    }

    /// 从当前页签实时计算 TabsState（与 on_exit 同一套规则），
    /// 供「页签变化立即落盘」的崩溃防护使用。
    fn current_tabs_state(&self) -> config::TabsState {
        // active 指向 dirs 数组中的页签（0=Home, 1=dirs[0], …）。
        // 必须按 dirs 实际过滤后的顺序计算索引，不能直接用 self.current：
        // self.current 是 tabs 全数组含 Home/已退出页签的索引，与 dirs 不对应。
        let active_sessions: Vec<&Session> = self
            .tabs
            .iter()
            .skip(1)
            .filter_map(|t| match t {
                Tab::Session(s) if !s.exited.load(Ordering::Acquire) => Some(s.as_ref()),
                _ => None,
            })
            .collect();
        let dirs: Vec<String> = active_sessions.iter().map(|s| s.dir.clone()).collect();
        let cmds: Vec<String> = active_sessions.iter().map(|s| s.cmd.clone()).collect();
// 设置页签也记录：退出时开着则启动时插回首页之后（见 restore_settings）。
        let settings_open = self.tabs.iter().any(|t| matches!(t, Tab::Settings));
        let kinds: Vec<TabKind> = self
            .tabs
            .iter()
            .map(|t| match t {
                Tab::Home => TabKind::Home,
                Tab::Settings => TabKind::Settings,
                Tab::Placeholder { .. } => TabKind::Gone,
                Tab::Session(s) if s.exited.load(Ordering::Acquire) => TabKind::Gone,
                Tab::Session(_) => TabKind::Alive,
            })
            .collect();
        let (active, settings_pos) = restore_coords(&kinds, self.current);
        config::TabsState {
            dirs,
            cmds,
            active,
            settings_open,
            settings_pos,
        }
    }

    /// 静默落盘（不覆盖状态栏已有消息）；同一失败区间只在状态栏提示一次。
    /// 返回是否成功。
    fn persist_config(&mut self) -> bool {
        match config::save(&self.config) {
            Ok(()) => {
                self.config_save_failed = false;
                self.last_config_save = std::time::Instant::now();
                true
            }
            Err(e) => {
                if !self.config_save_failed {
                    self.config_save_failed = true;
                    self.status = Some(format!("保存配置失败: {e}"));
                }
                false
            }
        }
    }

    fn save_config(&mut self, msg: String) {
        match config::save(&self.config) {
            Ok(()) => {
                self.config_save_failed = false;
                self.last_config_save = std::time::Instant::now();
                self.status = Some(msg);
            }
            Err(e) => self.status = Some(format!("保存配置失败: {e}")),
        }
    }


/// 量出本帧会话页签区（首页/设置之外）的横向排布。
    ///
    /// 标题宽走缓存，与绘制共用同一个 `title_width_cache`：这里顺带把未量过的标题
    /// 排版一次，绘制时就零排版成本。
fn strip_geom_for_frame(
        &mut self,
        ui: &egui::Ui,
        font: egui::FontId,
        m: StripMetrics,
    ) -> StripGeom {
        let mut items: Vec<(usize, f32, bool)> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate().skip(1) {
            let (title, has_close) = match tab {
                Tab::Session(s) => (s.title.clone(), true),
                Tab::Placeholder { title } => (title.clone(), false),
                _ => continue, // 首页/设置在固定区，不参与排布
            };
            let w = *self.title_width_cache.entry(title.clone()).or_insert_with(|| {
ui.ctx().fonts_mut(|f| {
                    f.layout_no_wrap(title, font.clone(), Color32::TRANSPARENT)
                        .size()
                        .x
                })
            });
            items.push((i, w, has_close));
        }
        strip_geom(&items, m)
    }

    fn open_settings(&mut self) {
        self.settings_command = self.config.settings.tui_command.clone();
        self.settings_commands = self.config.settings.tui_commands.clone();
        self.settings_new_command.clear();
        self.settings_tool_dirs = self.config.settings.tool_paths.clone();
        self.settings_new_tool_dir.clear();
// 如果已有一个设置页签，跳转过去而不是重复添加。
        if let Some(idx) = self.tabs.iter().position(|t| matches!(t, Tab::Settings)) {
            // 已在位则不动（idx == 1）；旧状态里它可能被拖到了会话中间 → 拉回
            // 首页之后，保证「设置固定在第二个」这条不变量。
            if idx != 1 {
                let t = self.tabs.remove(idx);
                self.tabs.insert(1, t);
            }
            self.current = 1;
        } else {
            // 固定插在首页之后，不追加到末尾：设置是全局页面，不是某个项目的
            // 一部分；夹在一堆会话页签中间既难找，重启恢复也拿不到它的位置
            // （拖动只改内存）。
            self.tabs.insert(1, Tab::Settings);
            self.current = 1;
        }
        self.term_focused = false;
    }

    fn refresh_focus(&mut self) {
        self.term_focused = matches!(self.tabs.get(self.current), Some(Tab::Session(_)));
        if !self.term_focused {
            self.screen = Screen::Main;
        }
    }

    /// 异步检查 GitHub Release 最新版本，结果回到状态栏（不弹窗，只显示在窗口底部）。
    /// silent=true（启动/新开页签自动检查）时不覆盖当前状态栏消息。
    /// 同时顺带检查 pi / opencode（同一份源表、同一线程内串行，避免启动时
    /// 一次拉起 3×12 个 curl 进程），结果各自变成状态栏的下载按钮。
    fn check_updates(&mut self, silent: bool) {
        if !silent {
            self.status = Some("正在检查更新…".to_string());
        }
        let tx = self.check_tx.clone();
        let redraw_tx = self.redraw_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_latest_release());
            // try_send：通道满说明已有待处理重绘，本次唤醒请求可安全丢弃。
            let _ = redraw_tx.try_send(());
        });
        self.check_tool_updates();
    }

    /// 检查 pi / opencode 的新版本（与自更新同一套多源逐个探查）。
    /// 一个后台线程里按 TOOL_SPECS 顺序串行跑：每个都是「先成功先得」，正常
    /// 1~2 秒一个，串行比各起一个线程更温和（启动检查不再瞬时拉起 24 个 curl）。
    fn check_tool_updates(&mut self) {
        let tx = self.tool_tx.clone();
        let redraw_tx = self.redraw_tx.clone();
        // 正在下载/替换中的工具不打断：装到一半再报个「有新版本」纯属噪声。
        let ids: Vec<usize> = (0..TOOL_SPECS.len())
            .filter(|&i| !self.tools[i].downloading)
            .collect();
        let dirs = self.tool_search_dirs();
        let use_path = self.config.settings.tool_search_path;
        std::thread::spawn(move || {
            for idx in ids {
                let ev = check_tool_update(idx, dirs.clone(), use_path);
                let _ = tx.send((idx, ev));
                let _ = redraw_tx.try_send(());
            }
        });
    }

    /// 「检查更新」找 pi / opencode 用的条目列表（不硬编码任何路径）：
    /// 本软件 exe 所在目录（current_exe 的父目录，运行时得出）打头，再接
    /// 配置里的路径（启动时已自动补齐本软件同级目录下的 pi / opencode，
    /// 用户可改成别的机器布局），最后（可选）扫 PATH。
    fn tool_search_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = software_dir().into_iter().collect();
        for d in &self.config.settings.tool_paths {
            let p = PathBuf::from(d.trim());
            if p.as_os_str().is_empty() || dirs.contains(&p) {
                continue;
            }
            dirs.push(p);
        }
        dirs
    }

    /// 后台下载并安装单个工具的新版本（tag 为空 = 本机没装过，装到软件同级
    /// 目录）。流程与自更新同构：候选链下 zip 到 .new → 解压到暂存目录 →
    /// 备份旧 exe 为 .old → install_update 三段式替换 → 同步其余文件（pi）→
    /// 清理暂存。失败 3 秒后自动重试，坏源在 download_chain 内已被剔除；用户
    /// 取消立即退出。
    /// 起一个工具的下载/安装作业。`into_fresh=true` = 强制装进「本软件同级
    /// 目录」（而不管状态里记的 install_dir 在哪）：用于「装到本软件目录」
    /// 那条入口——本机 PATH 里那份不动，这里另装一份自锁版本（pi 进同名子
    /// 目录，opencode 平铺）。取不到可写目录就不开工，不给假入口。
    fn start_tool_download(&mut self, idx: usize, into_fresh: bool) {
        if self.tools[idx].downloading {
            return;
        }
        let spec = &TOOL_SPECS[idx];
        let install_dir = if into_fresh {
            match fresh_tool_dir(spec) {
                Some(d) if !d.as_os_str().is_empty() => d,
                _ => return,
            }
        } else {
            self.tools[idx].install_dir.clone()
        };
        if install_dir.as_os_str().is_empty() {
            return;
        }
        // 目标目录与状态保持一致：装完之后 Done/失败重来都指着同一个位置。
        self.tools[idx].install_dir = install_dir.clone();
        // 本机未安装（missing）时 latest 为空，版本号由作业内现查。
        let tag = self.tools[idx].latest.clone();
        self.tools[idx].downloading = true;
        self.tools[idx].latest = None;
        // 记住本轮在装的 tag：作业结束（失败/取消/被占用）时用它把下载按钮
        // 重新点亮，用户可直接再点一次重试，而不必等下次「检查更新」。
        // 首次安装没有 tag（None）——按钮靠 missing 标记继续显示。
        self.tools[idx].pending_tag = tag.clone();
        self.status = Some(match &tag {
            Some(t) => format!("正在下载 {} {t}…", spec.label),
            None => format!("正在下载安装 {}…", spec.label),
        });
        self.tool_cancel[idx].store(false, Ordering::Relaxed);
        let (ptx, prx) = std::sync::mpsc::channel();
        let cancel = self.tool_cancel[idx].clone();
        let tx = self.tool_tx.clone();
        let redraw_tx = self.redraw_tx.clone();
        // 进度中继：download_chain 往 ptx 写 (已下载, 总数)，这里转成工具事件
        // 走同一条通道（否则要同时管两条通道还得额外唤醒 UI）。prx 的发送端
        // 随作业线程退出而掉落，try_recv 返回断开即收工。
        {
            let tx = self.tool_tx.clone();
            let redraw_tx = self.redraw_tx.clone();
            std::thread::spawn(move || {
                loop {
                    match prx.try_recv() {
                        Ok((d, t)) => {
                            let _ = tx.send((idx, ToolEvent::Progress(d, t)));
                            let _ = redraw_tx.try_send(());
                        }
                        // 发送端还没掉（正在下载）也不能空转：原来这个 while
                        // let Ok(..) = try_recv 是纯忙等，opencode 的 62MB 要下
                        // 几十秒，期间整个核心一直 100% 占着，UI 都被抢。
                        Err(TryRecvError::Empty) => {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        Err(TryRecvError::Disconnected) => return, // 作业结束
                    }
                }
            });
        }
        std::thread::spawn(move || {
            run_tool_update(idx, tag, install_dir, ptx, cancel, tx.clone(), redraw_tx);
            let _ = tx.send((idx, ToolEvent::Finished));
        });
    }

    /// 当前 exe 的正式路径：unlock_exe 启动时把运行映像改名成 {name}.running，
    /// current_exe() 返回的是 .running 锁定路径；更新替换和重启都必须在正式名
    /// 上操作（该名字空闲可写）。
    fn canonical_exe_path(exe: PathBuf) -> PathBuf {
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(base) = name.strip_suffix(".running") {
            exe.with_file_name(base)
        } else {
            exe
        }
    }

    /// 后台下载更新：下载到 .new 文件，完成后替换旧 exe。
    fn start_download(&mut self, tag: &str) {
        if self.downloading {
            return;
        }
        self.downloading = true;
        self.update_done = false; // 新一轮下载，重启按钮回到待完成态。
        self.status = Some(format!("正在下载 {tag}…"));
        // 重置取消信号
        self.cancel_download.store(false, std::sync::atomic::Ordering::Relaxed);
        let tag = tag.to_string();
        let (ptx, prx) = std::sync::mpsc::channel();
        self.download_progress_rx = Some(prx);
        let cancel = self.cancel_download.clone();
        let redraw_tx = self.redraw_tx.clone();
        let status_tx = self.check_tx.clone();
        // 在 UI 线程确定正式名并记住：安装兜底可能把运行映像 rename 成 .old，
        // 之后 current_exe() 返回的将不再是正式名，重启必须用这里记住的路径。
        let final_path = std::env::current_exe()
            .map(Self::canonical_exe_path)
            .unwrap_or_else(|_| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("TUIProjectManager.exe")
            });
        self.update_final = Some(final_path.clone());
        std::thread::spawn(move || {
            let exe_path = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."));
            // 失败后按退避重试，**封顶 MAX_SELF_UPDATE_ATTEMPTS 次**。坏源在
            // download_chain 内已被剔除、install 校验失败（BadDownload）也会
            // 清掉续传残留换源重下；封顶后不再无限循环，收手并提示手动重试。
            let mut attempt = 1u32;
            let mut installed_new: PathBuf; // 成功装入的 .new（供完成日志）
            loop {
                // 用户取消时跳出重试循环
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = status_tx.send(("下载已取消".to_string(), None));
                    let _ = redraw_tx.try_send(());
                    return;
                }
                let new_path = match download_update(&tag, &exe_path, ptx.clone(), &cancel) {
                    Ok(p) => p,
                    Err(e) => {
                        if attempt >= MAX_SELF_UPDATE_ATTEMPTS {
                            let _ = status_tx.send((
                                format!(
                                    "已停止自动重试：连续 {MAX_SELF_UPDATE_ATTEMPTS} 次下载失败（最后一次: {e}）；请检查网络后手动点「⬇ 下载」重试"
                                ),
                                None,
                            ));
                            let _ = redraw_tx.try_send(());
                            return;
                        }
                        let wait = retry_backoff(attempt);
                        let _ = status_tx.send((
                            format!(
                                "下载失败（第 {attempt}/{MAX_SELF_UPDATE_ATTEMPTS} 次）: {e}，{} 秒后自动重试…",
                                wait.as_secs()
                            ),
                            None,
                        ));
                        let _ = redraw_tx.try_send(());
                        // 可中断等待：按「✕ 取消」即刻收手，不用等满退避。
                        if sleep_until_cancel(&cancel, wait) {
                            let _ = status_tx.send(("下载已取消".to_string(), None));
                            let _ = redraw_tx.try_send(());
                            return;
                        }
                        attempt += 1;
                        continue;
                    }
                };
                let new_file = PathBuf::from(&new_path);
                installed_new = new_file.clone();
                // 无论资产名是小写 tui-project-manager.exe 还是历史大写名，最终都落到
                // 正式名（unlock_exe 启动时把运行映像挪成 .running 腾出的空闲名）。
                let old_path = final_path.with_extension("exe.old");
                // copy 目标已存在则直接覆盖（.old 始终保留最新旧版），失败不阻断替换。
                // 注意该提示不能含「失败」字样：UI 按关键字把 downloading 复位，避免干扰安装。
                if let Err(e) = std::fs::copy(&final_path, &old_path) {
                    let _ = status_tx.send((
                        format!("提示: 旧版备份 {old_path:?} 未完成（{e}），不影响替换"),
                        None,
                    ));
                    let _ = redraw_tx.try_send(());
                }
                match install_update(&new_file, &final_path, &old_path, &|msg: &str| {
                    let _ = status_tx.send((msg.to_string(), None));
                    let _ = redraw_tx.try_send(());
                }) {
                    InstallOutcome::Done => break,
                    InstallOutcome::Occupied => {
                        // 安装失败：保留 .new 与 .old 供排查/手动处理，稍后可重新下载。
                        return;
                    }
                    InstallOutcome::BadDownload => {
                        // 清掉全部 .c{i}.new 续传残留，避免下一轮 -C - 把坏文件续传
                        // 拼成残缺 exe；3 秒后整链重新逐链下载（坏源已被剔除）。
                        if let Some(dir) = new_file.parent() {
                            if let Ok(rd) = std::fs::read_dir(dir) {
                                for e in rd.flatten() {
                                    let n = e.file_name().to_string_lossy().into_owned();
                                    if n.ends_with(".new") {
                                        let _ = std::fs::remove_file(e.path());
                                    }
                                }
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        attempt += 1;
                    }
                }
            }
            log_update(&format!(
                "下载 替换完成：{installed_new:?} → {final_path:?}（旧版已备份 .exe.old）"
            ));
            let _ = status_tx.send((
                format!("下载完成！请手动重启应用以使用新版本 {tag}"),
                None,
            ));
            let _ = redraw_tx.try_send(());
        });
    }

    /// 重启应用：以新进程启动当前 exe（透传原参数）后关闭本进程。
    /// 新 exe 已替换到当前路径，重启即加载新版。
    fn restart_app(&mut self) {
        log_update("重启应用：spawn 新进程并退出");
        // 必须启动正式名（新 exe 替换到正式名）。优先用安装时记住的路径：
        // 安装兜底可能把运行映像 rename 成 .old，current_exe() 不再等于正式名，
        // 现算会指向旧版本；否则去掉 .running 后缀现算。
        let exe = self
            .update_final
            .clone()
            .or_else(|| std::env::current_exe().ok().map(Self::canonical_exe_path))
            .unwrap_or_else(|| PathBuf::from("TUIProjectManager.exe"));
        let mut cmd = std::process::Command::new(&exe);
        // 透传启动参数（如 --restore），保持与会话恢复行为一致。
        cmd.args(std::env::args().skip(1));
        match cmd.spawn() {
            Ok(_) => {
                // Windows 下子进程不随父进程退出而终止，先起新进程再关自身。
                // 正常退出路径会触发配置（窗口位置/大小）保存。
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => {
                log_update(&format!("重启应用 启动新进程失败: {e}"));
                self.status = Some(format!("启动新进程失败: {e}"));
                self.update_done = false; // 允许再次点击重试。
            }
        }
    }

    /// 后台 spawn 完成的应用注册：给会话同步深浅主题后原样返回，供插入页签。
    fn apply_theme_to(&self, result: Result<Session, String>) -> Result<Session, String> {
        if let Ok(sess) = &result {
            sess.theme_dark
                .store(self.effective_dark(), std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    fn launch_selected(&mut self) {
        let Some(project) = self.config.projects.get(self.selected_project).cloned() else {
            self.status = Some("请先在左侧选择一个项目".to_string());
            return;
        };
        let exists = Path::new(&project.path).is_dir();
        if !exists {
            self.status = Some(format!("目录不存在，无法启动: {}", project.path));
            return;
        }
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Tab::Session(s) = tab
                && s.dir == project.path
                && !s.exited.load(Ordering::Acquire)
            {
                self.current = i;
                self.term_focused = true;
                self.status = Some(format!("已切换到会话: {}", s.title));
                return;
            }
        }
        // 不在点击帧直接 spawn：此时还在首页布局，拿不到会话页签的真实可用面积；
        // 旧实现的估算（窗口宽-左栏 300px、高-64px）与会话页签全宽实际面积恒差一截，
        // TUI 启动即按估算尺寸整屏画页，首帧 resize 事件偶发丢失 → 页面尺寸对不上。
        // 推迟到下一帧会话页签布局里按精确尺寸启动（见 ui() 的 pending_launch）。
        self.pending_launch.push(PendingLaunch {
            title: project.name.clone(),
            dir: project.path,
            cmd: self.config.settings.tui_command.clone(),
        });
        self.status = Some(format!("正在启动: {}", project.name));
    }

    /// cmd /c 命令串里的目录参数：含空白时交给引号保护，否则 ^ 转义 cmd 特殊字符。
    // 测试仅在内嵌 test 构建中使用 cmd_arg（命令串参数转义规则回归）；
    // 生产构建无任何调用点，属于预期死代码，靠 cfg_attr 在非 test 下放行。
    #[cfg(windows)]
    #[cfg_attr(not(test), allow(dead_code))]
    fn cmd_arg(dir: &str) -> String {
        if dir.chars().any(char::is_whitespace) {
            dir.to_string()
        } else {
            let mut o = String::new();
            for c in dir.chars() {
                match c {
                    '^' => o.push_str("^^"),
                    '&' | '|' | '<' | '>' | '(' | ')' => {
                        o.push('^');
                        o.push(c);
                    }
                    _ => o.push(c),
                }
            }
            o
        }
    }

    /// 用系统文件管理器（Windows 为 explorer）打开目录，结果写入状态栏。
    fn open_explorer(&mut self, dir: impl AsRef<Path>) {
        match std::process::Command::new("explorer").arg(dir.as_ref()).spawn() {
            Ok(_) => self.status = Some(format!("已打开目录: {}", dir.as_ref().display())),
            Err(e) => self.status = Some(format!("打开目录失败: {e}")),
        }
    }

    /// 用 VS Code 打开目录。依赖 code CLI 已安装并加入 PATH。
    fn open_in_vscode(&mut self, dir: &str) {
        #[cfg(windows)]
        let result = {
            use std::os::windows::process::CommandExt;
            use std::io::Write;
            // cmd /c 通过系统代码页（GBK）转码命令行，中文路径会乱码。
            // 写临时 .cmd 文件：先 chcp 65001 切 UTF-8，再 code --new-window。
            // .cmd 本身用 UTF-8 写入，chcp 65001 让 cmd 按 UTF-8 解析后续行。
            let bat = std::env::temp_dir().join("codebuff_open.cmd");
            let content = format!("@chcp 65001 >nul\r\n@code --new-window \"{}\"\r\n", dir);
            if let Ok(mut f) = std::fs::File::create(&bat) {
                let _ = f.write_all(content.as_bytes());
                drop(f);
                let r = std::process::Command::new("cmd")
                    .args(["/c", bat.to_str().unwrap_or("")])
                    .creation_flags(0x0800_0000)
                    .output();
                let _ = std::fs::remove_file(&bat);
                r
            } else {
                // 降级：直接调用 code（路径可能乱码但不至于崩溃）
                std::process::Command::new("code")
                    .args(["--new-window"])
                    .arg(dir)
                    .creation_flags(0x0800_0000)
                    .output()
            }
        };
        #[cfg(not(windows))]
        let result = std::process::Command::new("code").arg(dir).output();
        match result {
            Ok(o) if o.status.success() => {
                self.status = Some(format!("已在 VS Code 中打开: {dir}"));
            }
            Ok(_) => self.status = Some("打开 VS Code 失败：未找到 code 命令（请确认已安装 VS Code 并选择「添加到 PATH」）".to_string()),
            Err(e) => self.status = Some(format!("打开 VS Code 失败: {e}")),
        }
    }

    fn relaunch_session(&mut self, dir: String, title: String) {
        self.pending_relaunch.push(PendingRelaunch {
            tab_index: self.tabs.len(),
            title,
            dir,
            cmd: self.config.settings.tui_command.clone(),
        });
    }

    fn close_session(&mut self, idx: usize) {
        if idx == 0 || idx >= self.tabs.len() {
            return;
        }
        // 将 child 和 master 都移到后台线程异步清理：
        // - Child::drop / Child::kill() 在 Windows 上调用 WaitForSingleObject
        //   等待进程退出，阻塞 UI 线程 100-500ms。
        // - MasterPty::drop 在 Windows 上调用 ClosePseudoConsole，可能阻塞。
        // kill_in_background 取走两者，tabs.remove() 触发的 Session::Drop
        // child=None + master=None，零阻塞，UI 线程立即返回。
        if let Some(Tab::Session(s)) = self.tabs.get_mut(idx) {
            s.kill_in_background();
        }
        self.tabs.remove(idx);
        // 关的是当前页签**前面**的页签 → 后面元素左移，current 须同步减一；
        // 否则选中位/前台标记/输入路由整体错位一格，后台状态判定跟着错。
        if idx < self.current {
            self.current -= 1;
        }
        if self.current >= self.tabs.len() {
            self.current = self.tabs.len().saturating_sub(1);
        }
        if self.current == 0 {
            self.screen = Screen::Main;
        }
        self.refresh_focus();
        self.status = Some("已关闭会话".to_string());
    }

    /// 会话页签拖动落位：把 from 移到「原索引空间」的插入点 target（1..=len）。
    /// 0 是固定的首页，不在可移动范围内。
fn move_tab(&mut self, from: usize, target: usize) {
        let len = self.tabs.len();
        // 固定区不参与重排：首页恒在 0，设置恒在 1（开着时）。落点不得插到它
        // 前面/中间，否则设置页签会被拖离首页之后，下次打开又被拉回去。
        let fixed_end = if self.tabs.get(1).is_some_and(|t| matches!(t, Tab::Settings)) {
            2
        } else {
            1
        };
        if from < fixed_end || from >= len || target == 0 || target > len {
            return;
        }
        let mut new_p = if target > from { target - 1 } else { target };
        if new_p < fixed_end {
            new_p = fixed_end;
        }
        if new_p >= len || new_p == from {
            return;
        }
        let c = self.current;
        let tab = self.tabs.remove(from);
        self.tabs.insert(new_p, tab);
        // 重排后修 current：元素本身落到 new_p；其余位置随移除/插入平移。
        self.current = match c.cmp(&from) {
            std::cmp::Ordering::Equal => new_p,
            std::cmp::Ordering::Greater => {
                let c2 = c - 1;
                if c2 >= new_p {
                    c2 + 1
                } else {
                    c2
                }
            }
            std::cmp::Ordering::Less => {
                if c >= new_p {
                    c + 1
                } else {
                    c
                }
            }
        };
    }

    /// 项目列表中把 from 移到插入点 insert_at（0..=len）。0 无固定项，全列表可移动。
    /// 选中索引随重排修正，之后调用方负责 save_config 持久化。
    fn move_project(&mut self, from: usize, insert_at: usize) {
        let len = self.config.projects.len();
        if from >= len || insert_at > len {
            return;
        }
        let new_p = if insert_at > from { insert_at - 1 } else { insert_at };
        if new_p == from {
            return;
        }
        let sel = self.selected_project;
        let p = self.config.projects.remove(from);
        self.config.projects.insert(new_p, p);
        // 重排后修选中索引：元素本身落到 new_p；其余位置随移除/插入平移。
        self.selected_project = match sel.cmp(&from) {
            std::cmp::Ordering::Equal => new_p,
            std::cmp::Ordering::Greater => {
                let s2 = sel - 1;
                if s2 >= new_p {
                    s2 + 1
                } else {
                    s2
                }
            }
            std::cmp::Ordering::Less => {
                if sel >= new_p {
                    sel + 1
                } else {
                    sel
                }
            }
        };
    }

    /// 「内容停止稳定计时 + 执行完成通知」状态机：原在 tab_bar() 渲染路径，
    /// 挪进 logic() 与退出判定（update_exited）同源同帧执行——不再依赖页签栏
    /// 渲染，最小化/遮挡时也随 IDLE_HEARTBEAT_MS 心跳走，恢复后按已过时长补判。
    /// ponytail: 若系统挂起最小化时的 repaint 心跳，通知会延迟到唤醒后 500ms。
    /// 触发判定**不靠 ✅ 图标**（旧实现以 icon==Some("✅") 为门，而图标要求
    /// !viewed——当前正查看的页签任务完成后图标落空，永进不了完成分支，失焦
    /// 也不弹通知）。改用独立判据：未退出、非加载中、内容停止超 OUTPUT_END_MS、
    /// 有实质输出——当前页签且应用前台（用户正盯着）才清零静默，否则照弹。
    /// 弹窗门槛补「未查看」：启动即 viewed=true，闲置页签（仅 shell 提示符、
    /// 无新输出轮）永不弹「任务完成」+ 任务栏闪烁；只有用户没看过的真任务
    /// 输出轮（阅读循环在加载期后复位 viewed）才提醒。
    fn update_done_states(&mut self, ctx: &egui::Context) {
        let now_ms = crate::now_ms();
        let app_fg = crate::app_is_foreground(self.titlebar_hwnd, ctx.input(|i| i.focused));
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Tab::Session(s) = tab {
                // 完成态 = 进程还活着（exited 由 update_exited 处理「运行结束」）、
                // 非启动加载中、最近一块**实质内容**输出停止 ≥3s、画面不在高频动。
                // last_real_output_ms 只看非动画块：周期转义重绘（tmux 状态栏、
                // 光标/屏幕刷新）不会让它刷新 → 这类会话不会因 done 横跳而循环弹
                // 「任务完成」+ 闪烁；last_anim 负责把「还在思考/跑命令」的高频
                // 动画挡在完成之外（见 ANIM_BUSY_MS）。四条判据与图标**共用同一份
                // 快照**（最快 1s 重算一次，见 refresh_tab_state），两者不会打脸。
                // 未查看门槛在弹窗条件里（viewed 语义：启动即已见，仅新输出轮
                // 复位，见下）。
                refresh_tab_state(s, now_ms);
                let done = s.state_done.load(Ordering::Relaxed);
                // 「执行完成」提醒：进入完成态后需稳定停留 DONE_STABLE_MS（2s）
                // 才弹系统通知 + 任务栏闪烁。稳定窗口过滤误触发：周期输出在
                // 🔄↔边界横跳时（再有输出 → done=false → 清零）重置计时。
                // 静默判据：仅「当前页签且应用在前台」（用户正盯着）才清零
                // 计时不打扰；失焦/切走后重新计时，与 update_exited 的「运行
                // 结束」语义一致——用户报告的现象（当前页签任务完成、窗口
                // 失焦不推通知）即由旧图标门 + viewed 耦合导致，已解耦。
                if done {
                    let since = s.done_since_ms.load(Ordering::Relaxed);
                    if i == self.current && app_fg {
                        // 用户正盯着：视为已知晓，清零计时（切走/失焦后再重新
                        // 计 DONE_STABLE_MS）。done_notified 不动——本轮已看见
                        // 内容，不再弹窗。
                        s.done_since_ms.store(0, Ordering::Relaxed);
                    } else if since == 0 {
                        s.done_since_ms.store(now_ms, Ordering::Relaxed);
                    } else if now_ms.saturating_sub(since) > DONE_STABLE_MS
                        && s.out_bytes.load(Ordering::Relaxed) >= MIN_OUTPUT_BYTES
                        // 未查看门槛：启动即 viewed=true，闲置页签（无新输出轮）
                        // 不提醒；用户没看过的真任务输出轮才弹（阅读循环在加载
                        // 期后每轮新输出复位 viewed）。「任务完成」只属于主页签
                        // 之外、用户还没看过的新内容，消除「没任何输出却弹」误报。
                        && !s.has_been_viewed.load(Ordering::Relaxed)
                        && allow_toast(s, now_ms)
                        && !s.done_notified.swap(true, Ordering::Relaxed)
                        // 启动宽限期：刚启动的会话（含首轮输出）静默；
                        // done_notified 已置位，宽限期结束后不为这一轮补弹。
                        && now_ms.saturating_sub(s.started_ms.load(Ordering::Relaxed))
                            >= STARTUP_GRACE_MS
                    {
                        crate::notify_run_finished(&s.title, "任务完成");
                        crate::flash_taskbar(self.titlebar_hwnd);
                    }
                } else {
                    // 离开完成态（新一轮输出/启动加载中/已退出）→ 清稳定计时并
                    // 复位 done_notified：每一轮完成都可再弹，防轰炸靠同页签 10s
                    // 节流（TOAST_MIN_INTERVAL_MS，含退出/完成共用一条限流）。
                    s.done_since_ms.store(0, Ordering::Relaxed);
                    s.done_notified.store(false, Ordering::Relaxed);
                }
            }
        }
    }

    fn update_exited(&mut self, ctx: &egui::Context) -> bool {
        let now_ms = crate::now_ms();
        let app_fg = crate::app_is_foreground(self.titlebar_hwnd, ctx.input(|i| i.focused));
        let mut changed = false;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let Tab::Session(s) = tab {
                if !s.exited.load(Ordering::Acquire) {
                    // exited 由 try_wait 权威判定（reader 不再自置，见 session.rs
                    // 读循环：瞬时读错曾被当退出置永久 ❌）。前台每帧收割；后台
                    // 每 BG_REAP_MS 一次——孙进程继承 ConPTY 句柄时管道不 EOF，
                    // reader 判不了退出，只能低频轮询进程状态。
                    let fg = s.foreground.load(Ordering::Relaxed);
                    if fg
                        || now_ms.saturating_sub(s.last_reap_ms.load(Ordering::Relaxed))
                            >= BG_REAP_MS
                    {
                        s.last_reap_ms.store(now_ms, Ordering::Relaxed);
                        let child_exited = s.child
                            .as_deref_mut()
                            .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
                        if child_exited {
                            s.exited.store(true, Ordering::Release);
                        }
                    }
                    if s.exited.load(Ordering::Acquire) {
                        changed = true;
                    }
                }
                // 运行结束提醒：不管谁先置位 exited 都只处理一次（notified 去重）。
                // 10s 节流放最前（不通过则每帧重试，不消耗 notified）；用户正盯着
                // 该页签（当前页签且本应用在前台）时不打扰；kill_in_background
                // 已提前置位 notified，程序化终止（重启/切命令/关闭）不弹。
                if s.exited.load(Ordering::Acquire)
                    && allow_toast(s, now_ms)
                    && !s.notified.swap(true, Ordering::Relaxed)
                    && !(i == self.current && app_fg)
                {
                    // 启动宽限期：创建后 10 秒内退出也静默（刚启动就崩/秒退
                    // 不打扰），notified 已置位因此宽限期后也不会补弹。
                    if s.out_bytes.load(Ordering::Relaxed) >= MIN_OUTPUT_BYTES
                        && now_ms.saturating_sub(s.started_ms.load(Ordering::Relaxed))
                            >= STARTUP_GRACE_MS
                    {
                        crate::notify_run_finished(&s.title, "运行结束");
                        crate::flash_taskbar(self.titlebar_hwnd);
                    }
                }
            }
        }
        changed
    }

    fn commit_input(&mut self, dialog: InputDialog) {
        match dialog {
            InputDialog::AddProject { name, path } => {
                let mut name = name.trim().to_string();
                let path = path.trim().to_string();
                if path.is_empty() {
                    self.status = Some("请选择或输入项目路径".to_string());
                    self.input = Some(InputDialog::AddProject {
                        name,
                        path,
                    });
                    return;
                }
                if name.is_empty() {
                    // 名称留空时，默认用路径的最后一段作为项目名。
                    let fallback = Path::new(path.trim_end_matches(['/', '\\']))
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| path.clone());
                    name = fallback;
                }
                self.config.projects.push(config::Project { name, path, hidden: false });
                self.selected_project = self.config.projects.len() - 1;
                self.input = None;
                self.save_config("已添加项目".to_string());
            }
            InputDialog::Rename { value } => {
                let value = value.trim().to_string();
                if let Some(p) = self.config.projects.get_mut(self.selected_project) {
                    p.name = value;
                }
                self.input = None;
                self.save_config("已重命名".to_string());
            }
            InputDialog::EditPath { value } => {
                let value = value.trim().to_string();
                if let Some(p) = self.config.projects.get_mut(self.selected_project) {
                    p.path = value;
                }
                self.input = None;
                self.save_config("已修改路径".to_string());
            }
        }
    }

    fn open_rename(&mut self) {
        let Some(p) = self.config.projects.get(self.selected_project) else {
            self.status = Some("请先选择一个项目".to_string());
            return;
        };
        self.input = Some(InputDialog::Rename {
            value: p.name.clone(),
        });
    }

    fn open_edit_path(&mut self) {
        let Some(p) = self.config.projects.get(self.selected_project) else {
            self.status = Some("请先选择一个项目".to_string());
            return;
        };
        self.input = Some(InputDialog::EditPath {
            value: p.path.clone(),
        });
    }

    fn request_delete(&mut self) {
        let Some(p) = self.config.projects.get(self.selected_project) else {
            self.status = Some("请先选择一个项目".to_string());
            return;
        };
        self.confirm = Some(ConfirmDialog::DeleteProject {
            index: self.selected_project,
            name: p.name.clone(),
        });
    }

    fn confirm_delete(&mut self, index: usize) {
        if index < self.config.projects.len() {
            self.config.projects.remove(index);
            if self.selected_project >= self.config.projects.len() {
                self.selected_project = self.config.projects.len().saturating_sub(1);
            }
            self.save_config("已删除项目".to_string());
        }
    }

    fn shutdown(&mut self) {
        for tab in self.tabs.iter_mut() {
            if let Tab::Session(s) = tab {
                s.kill_in_background();
            }
        }
    }

    // ---- 渲染 ----

    /// 页签块底色：选中 → 实底高亮，悬停 → 半透明浅染，否则透明。
    /// 深色模式下使用深灰底色 + 微弱蓝色点缀，与面板背景区分但不刺眼；
    /// 浅色模式沿用 egui 选中蓝。
    fn tab_bg(sel_fill: Color32, selected: bool, hovered: bool, dark: bool) -> Color32 {
        if dark {
            // 深色模式：深灰底色（比面板背景 #1e1e22 稍亮），带微弱蓝色调
            let accent = Color32::from_rgb(42, 44, 52);   // 深灰偏冷
            let hover = Color32::from_rgb(52, 54, 64);    // 悬停稍亮
            if selected {
                accent
            } else if hovered {
                hover
            } else {
                Color32::TRANSPARENT
            }
        } else {
            if selected {
                sel_fill
            } else if hovered {
                Color32::from_rgba_unmultiplied(sel_fill.r(), sel_fill.g(), sel_fill.b(), 60)
            } else {
                Color32::TRANSPARENT
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let dark = self.effective_dark();
        let mut actions: Vec<TabAction> = Vec::new();
        let sel_fill = ui.visuals().selection.bg_fill;
        // 布局内边距保持紧凑（页签间距小）；背景色块比布局框大：左右各 5px、
        // 上下各 2px（见下方 rect.expand2），色块视觉上
        // 更饱满，但不撑大页签间距。
        let tab_margin = egui::Margin { left: 2, right: 2, top: 2, bottom: 2 };
        // 各会话页签当前帧的矩形（索引 → rect），拖动落位时用来定位插入点。
        let mut tab_rects: Vec<(usize, egui::Rect)> = Vec::new();
        let mut drag_index: Option<usize> = None;

        // 字体度量整帧一次（原实现每个页签内部做三次全量排版测宽）：
        // 图标槽与 × 的宽度对所有页签相同；标题宽度按字符串缓存，标题
        // 不变时零排版成本。
        let tab_font = egui::TextStyle::Body.resolve(ui.style());

        let slot_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap("🔄".to_string(), tab_font.clone(), Color32::TRANSPARENT)
                .size()
                .x
        });
        let close_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap("×".to_string(), tab_font.clone(), Color32::TRANSPARENT)
                .size()
                .x
        });

        ui.horizontal(|ui| {
            // 首页固定最左：不可拖动、不可关闭。
            if let Some(Tab::Home) = self.tabs.first() {
                let selected = self.current == 0;
                // 先占一个 Noop 位置，内容画完后 set 成底色 → 色块盖在面板上、垫在文字后。
                let bg_idx = ui.painter().add(egui::Shape::Noop);
                let resp = egui::Frame::new()
                    .corner_radius(4.0)
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(tab_margin)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new("🏠 首页").strong())
                                .selectable(false),
                        )
                    });
                let rect = resp.response.rect;
                // 交互层注册在内容之后：单击切回首页。出于一致性与可读性，
                // 不用 Label 自带的 sense——其点击不会反映到 Frame 的 response 上
                // （egui 里 Frame 的 response 是另一块无点击感的控件）。
                // 只感 click、不感 drag：首页固定最左，不可拖动、不可关闭。
                let hit = ui.interact(rect, egui::Id::new("home_tab"), egui::Sense::click());
                // 只认指针点击（clicked_by 而非 clicked）：egui 把「控件持有键盘
                // 焦点 + Enter/Space」也算一次点击，页签栏不是键盘可达控件（切页
                // 走点击 / Ctrl+Tab），让回车误切页就是 bug。详见 tab_focus.rs。
                if hit.clicked_by(egui::PointerButton::Primary) && !selected {
                    actions.push(TabAction::Activate(0));
                }
                let hovering = !selected
                    && ui.ctx().pointer_interact_pos().is_some_and(|p| rect.contains(p));
let bg = Self::tab_bg(sel_fill, selected, hovering, dark);
                ui.painter().set(bg_idx, egui::Shape::rect_filled(rect.expand2(egui::vec2(5.0, 2.0)), 0.0, bg));
            }

            // ── 固定区：设置页签（恒在首页之后）──
            // 与首页一样常驻左侧：设置是全局页面，滚走就找不着了。
            // 不可拖动：位置是固定的（open_settings 每次都插回下标 1），能被拖走
            // 又会被拉回来，不如直接不响应拖动。
            if let Some(i) = self.tabs.iter().position(|t| matches!(t, Tab::Settings)) {
                ui.add_space(4.0);
                let title = "⚙ 设置";
                let selected = self.current == i;
                let bg_idx = ui.painter().add(egui::Shape::Noop);
                let (close_rect, frame_resp) = egui::Frame::new()
                    .corner_radius(4.0)
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(tab_margin)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = TAB_GAP;
                        let min_width = ui.text_style_height(&egui::TextStyle::Body) * 4.0;
                        let title_w = *self.title_width_cache.entry(title.to_string()).or_insert_with(|| {
                            ui.ctx().fonts_mut(|f| {
                                f.layout_no_wrap(title.to_string(), tab_font.clone(), Color32::TRANSPARENT)
                                    .size()
                                    .x
                            })
                        });
                        let s = TAB_GAP;
                        let icon_title_w = slot_w + s + title_w;
                        let slack = (min_width - icon_title_w - s - close_w).max(0.0);
                        let (pad_l, pad_m) = if slack > 0.0 && slack >= close_w + s {
                            ((slack + close_w + s) / 2.0, (slack - close_w - s) / 2.0)
                        } else if slack > 0.0 {
                            (slack, 0.0)
                        } else {
                            (0.0, 0.0)
                        };
                        if pad_l > 0.0 {
                            ui.add_space(pad_l);
                        }
                        let row_h = ui.text_style_height(&egui::TextStyle::Body);
                        ui.add_sized(
                            egui::vec2(slot_w, row_h),
                            egui::Label::new(" ").selectable(false),
                        );
                        ui.add(
                            if selected {
                                egui::Label::new(RichText::new(title).strong())
                            } else {
                                egui::Label::new(RichText::new(title))
                            }
                            .selectable(false),
                        );
                        if pad_m > 0.0 {
                            ui.add_space(pad_m);
                        }
                        (ui.add(egui::Label::new("×").selectable(false)).rect, ui.response())
                    })
                    .inner;
                let rect = frame_resp.rect;
                // 只感 click：固定页签不参与重排（同首页）。
                let resp = ui.interact(rect, egui::Id::new("settings_tab"), egui::Sense::click());
                // 只认指针点击：键盘回车不切页/不关页。
                if resp.clicked_by(egui::PointerButton::Primary) {
                    let pos = ui.ctx().pointer_interact_pos();
                    if pos.is_some_and(|p| close_rect.contains(p)) {
                        actions.push(TabAction::Close(i));
                    } else {
                        actions.push(TabAction::Activate(i));
                    }
                }
                if resp.hovered()
                    && ui
                        .ctx()
                        .pointer_interact_pos()
                        .is_some_and(|p| close_rect.contains(p))
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let hovering = !selected
                    && drag_index.is_none()
                    && ui.ctx().pointer_interact_pos().is_some_and(|p| rect.contains(p));
                let bg = Self::tab_bg(sel_fill, selected, hovering, dark);
                ui.painter().set(bg_idx, egui::Shape::rect_filled(rect.expand2(egui::vec2(5.0, 2.0)), 0.0, bg));
                tab_rects.push((i, rect));
            }

            // ── 会话页签区：页签太多时**只滚这一段** ──
            // 首页与设置常驻左侧；首页/设置**页面**上的滚轮也不归它（判定靠指针
            // 是否落在本行矩形内）。不需要真的用 ScrollArea：它靠「测量一遍 + 裁剪
            // 重画一遍」实现，而本函数每帧都在收集点击动作/拖动源，两遍会重复执行
            // 副作用。这里改成：按量出来的宽度把布局起点左移 off，再按视口裁剪。
            let avail = ui.available_rect_before_wrap();
            let view_w = avail.width().max(0.0);
            let geom = self.strip_geom_for_frame(
                ui,
                tab_font.clone(),
                StripMetrics {
                    slot_w,
                    close_w,
                    min_width: ui.text_style_height(&egui::TextStyle::Body) * 4.0,
                    gap: ui.spacing().item_spacing.x + TAB_GAP,
pad: tab_margin.left as f32 + tab_margin.right as f32,
                },
            );
            let view =
                egui::Rect::from_min_size(avail.min, egui::vec2(view_w, avail.height()));
            let mut off = self.tab_scroll_x;
            // 滚轮：竖滚轮 → 横移（鼠标最常见），触控板横扫同样生效。
            let scroll = ui.input(|i| i.smooth_scroll_delta.y + i.smooth_scroll_delta.x);
            if scroll != 0.0
                && ui.ctx().pointer_interact_pos().is_some_and(|p| view.contains(p))
            {
                off -= scroll;
                // 同一份位移不能再流向别的 ScrollArea（首页项目列表等）——照终端
                // show_terminal 里的做法清掉。
                ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
            }
            // 当前页签必须留在视野里：Ctrl+Tab 切页、新建/关闭页签都靠它。
            off = match geom.spans.iter().find(|(i, _, _)| *i == self.current) {
Some((_, x, w)) => offset_to_show(off, view_w, geom.content_w, *x, *w),
                None => off.clamp(0.0, (geom.content_w - view_w).max(0.0)),
            };
            self.tab_scroll_x = off;
            let content_rect = egui::Rect::from_min_max(
                egui::pos2(view.left() - off, view.top()),
                egui::pos2(
                    view.left() - off + geom.content_w.max(view_w),
                    view.bottom(),
                ),
            );
            // 影子 `ui`：布局仍在内容坐标系里排（页签定位继续由 egui 负责），
            // 只是起点左移了 off，并按视口裁剪。下面循环体一个字都不用改。
            let mut ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("tab_strip")
                    .max_rect(content_rect)
.layout(
                        egui::Layout::left_to_right(egui::Align::Center)
                            .with_cross_align(egui::Align::Center),
                    ),
            );
            ui.set_clip_rect(view);

            for (i, tab) in self.tabs.iter().enumerate().skip(1) {
                if matches!(tab, Tab::Settings) {
                    continue; // 固定区已画
                }
                if let Tab::Session(s) = tab {
                    ui.add_space(TAB_GAP);
                    // 状态图标：固定宽度单字符。
                    let viewed = s.has_been_viewed.load(Ordering::Relaxed);
                    let now_ms = crate::now_ms();
                    // ── 状态判定：仅凭终端内容 ──
                    // last_output_ms 由 reader 每收一块输出刷新；滚动/翻页只改
                    // 视口不产生输出 → 不计更新状态。周期重绘/CPU 采样/锁存等
                    // 后台固定刷新全部退出判定（见 tab_icon）：有内容 → 🔄，
                    // 3s 无内容 → 完成（✅/空）。用户驱动例外：最近 1.5s 内
                    // 向终端输过键（last_input_ms，仅键盘/IME/粘贴路径更新）或
                    // 500ms 内转发过滚轮（last_scroll_ms）→ 直接引发的回显不
                    // 算任务在跑 → 跳过 🔄；真实输出晚于窗口照常判 🔄。
                    // 状态快照：最快 1s 重算一次（STATE_CHECK_MS），退出/加载/
                    // 新实质内容这三类事件不等门限（见 state_due）。
                    refresh_tab_state(s, now_ms);
                    let icon = tab_icon_from_snap(s, viewed);
                    let title = s.title.clone();
                    let selected = self.current == i;
                    let dir_key = s.dir.as_str();
                    // 「执行完成」通知状态机已挪入 logic()（update_done_states）：
                    // 渲染路径只算图标、只读状态，不再产生副作用。
                    // 本页签当前启动命令（切换菜单里勾选当前项）。
                    let tab_cmd = s.cmd.clone();
                    // 刚拖起的帧里画底色需要 Noop 在内容之前插入，所以先占位。
                    let bg_idx = ui.painter().add(egui::Shape::Noop);
                    // 整块交互：一个 Sense::click_and_drag 控件同时承担 单击（激活/关闭）
                    // 与 按住拖动（重排）。不用 dnd_drag_source：其内部容器的 dragged 标志
                    // 读取不可靠（egui 0.36 压测见 tests/tab_click.rs），会把点击和拖动都
                    // 搅在一起，还自带 Grab 光标；click_and_drag 由 egui 延迟判定拖动
                    // （指针过了阈值才算拖动），纯点击天然保留，光标也不再变
                    // 成拖拽手势。
                    let (close_rect, frame_resp) = egui::Frame::new()
                        .corner_radius(4.0)
                        .fill(Color32::TRANSPARENT)
.inner_margin(tab_margin)
                        .show(&mut ui, |ui| {
                            ui.spacing_mut().item_spacing.x = TAB_GAP;
                            // 最小宽度≈四个汉字（汉字宽度≈字号）：短标题（如单字项目名）
                            // 不至于把页签缩成一小条，文字与 × 挤在一起、点选/拖拽目标过小。
                            let min_width =
                                ui.text_style_height(&egui::TextStyle::Body) * 4.0;
                            // × 用 U+00D7（Latin-1）而不是 ✕ (U+2715)：后者在 egui 自带字体
                            // 与系统 CJK 字体里都可能缺字形，导致关闭图标不显示。
                            // 点击判定靠帧内对该矩形做命中检查（见下方 clicked 分支）。
                            // egui 横向布局无 flex：先量出标题与 × 的实际宽度，标题在槽内
                            // 居中、× 右对齐——把差额拆成“标题左侧垫白”和“标题/× 之间垫白”
                            // 两部分，等式让标题中心落在槽中心；差额小到撑不开中间空隙时全垫
                            // 在左侧，正文与 × 紧邻（与自然宽标题行为一致）。
                            // slot_w/close_w 整帧已量好；title_w 走缓存（标题不变不排版）。
                            let title_w = *self.title_width_cache.entry(title.clone()).or_insert_with(|| {
                                ui.ctx().fonts_mut(|f| {
                                    f.layout_no_wrap(title.clone(), tab_font.clone(), Color32::TRANSPARENT)
                                        .size()
                                        .x
                                })
                            });
let s = TAB_GAP;
                            let icon_title_w = slot_w + s + title_w;
                            let slack = (min_width - icon_title_w - s - close_w).max(0.0);
                            let (pad_l, pad_m) = if slack > 0.0 && slack >= close_w + s {
                                // 左右空隙对等：pad_l = pad_m + s + close_w → 标题居中。
                                ((slack + close_w + s) / 2.0, (slack - close_w - s) / 2.0)
                            } else if slack > 0.0 {
                                (slack, 0.0)
                            } else {
                                (0.0, 0.0)
                            };
                            if pad_l > 0.0 {
                                ui.add_space(pad_l);
                            }
                            // 状态图标槽：恒定 slot_w 宽度，无图标时渲染透明占位。
                            let row_h = ui.text_style_height(&egui::TextStyle::Body);
                            if let Some(c) = icon {
                                ui.add_sized(
                                    egui::vec2(slot_w, row_h),
                                    egui::Label::new(RichText::new(c.to_string()).strong())
                                        .selectable(false),
                                );
                            } else {
                                ui.add_sized(
                                    egui::vec2(slot_w, row_h),
                                    egui::Label::new(" ").selectable(false),
                                );
                            }
                            // 标题按借用传入，不再每帧 clone 两份 String。
                            ui.add(
                                if selected {
                                    egui::Label::new(RichText::new(title.as_str()).strong())
                                } else {
                                    egui::Label::new(RichText::new(title.as_str()))
                                }
                                // 页签文字不参与文本选择（egui 默认可选中，会在悬停/按下时
                                // 强制 Text 光标覆盖我们设置的小手，见 label selection 插件
                                // 的 on_end_pass），一并关掉。
                                .selectable(false),
                            );
                            if pad_m > 0.0 {
                                ui.add_space(pad_m);
                            }
                            (ui.add(egui::Label::new("×").selectable(false)).rect, ui.response())
                        })
.inner;
                    let rect = frame_resp.rect;
                    // 滚出可见区的页签：整段跳过（绘制本来就被裁掉，但**交互不会**——
                    // 不跳过就变成点右侧空白处命中一个看不见的页签）。
                    if !view.intersects(rect) {
                        continue;
                    }
                    // 交互层注册在内容之后（更上层），点击/拖动都落在它身上。
                    let resp = ui.interact(
                        rect,
                        egui::Id::new(("session_tab", i, dir_key)),
                        egui::Sense::click_and_drag(),
                    );
                    if resp.dragged() {
                        drag_index = Some(i);
                    }
                    // 只认指针点击：键盘（Enter/Space）落在页签上不切页也不关页，
                    // 否则终端里一次误落的回车就能把当前页签切走/关掉。
                    if resp.clicked_by(egui::PointerButton::Primary) {
                        let pos = ui.ctx().pointer_interact_pos();
                        // 指针按在 × 上 —— 关闭；否则 —— 激活。即便已激活也推送 Activate，
                        // 让「输出结束」对号在点击当前页签时也能被清除。
                        if pos.is_some_and(|p| close_rect.contains(p)) {
                            actions.push(TabAction::Close(i));
                        } else {
                            actions.push(TabAction::Activate(i));
                        }
                    }
                    // 右键页签弹出菜单：打开目录 / 在 VSCode 打开（整块右键都响应）。
                    resp.context_menu(|ui| {
                        if ui
                            .button("📂 打开目录")
                            .on_hover_text("在资源管理器中打开该会话目录")
                            .clicked()
                        {
                            actions.push(TabAction::OpenDir(i));
                            ui.close();
                        }
                        if ui
                            .button("⌨ 在 VSCode 打开")
                            .on_hover_text("用 VS Code 打开该会话目录（需安装 code 命令并加入 PATH）")
                            .clicked()
                        {
                            actions.push(TabAction::OpenVSCode(i));
                            ui.close();
                        }
                        if ui
                            .button("🔄 重新启动")
                            .on_hover_text("结束当前会话并重新启动该页签")
                            .clicked()
                        {
                            actions.push(TabAction::Restart(i));
                            ui.close();
                        }
                        ui.separator();
                        // 切换该页签的启动命令：在设置里配置的 TUI 命令列表中选一个，
                        // 选完立即用新命令重启本页签（保持目录与页签位置）。
                        if self.config.settings.tui_commands.is_empty() {
                            ui.add_enabled(
                                false,
                                egui::Button::new("🔧 切换启动命令（无可用命令，请在设置中添加）"),
                            );
                        } else {
                            ui.menu_button("🔧 切换启动命令", |ui| {
                                ui.set_min_width(180.0);
                                for cmd in &self.config.settings.tui_commands {
                                    let cur = cmd.trim() == tab_cmd.trim();
                                    if cur {
                                        ui.label(
                                            RichText::new(format!("✅ {cmd}")).weak(),
                                        );
                                    } else if ui
                                        .button(cmd.clone())
                                        .on_hover_text("用此命令重新启动当前页签（会话内容会清空）")
                                        .clicked()
                                    {
                                        actions.push(TabAction::SwitchCommand(i, cmd.clone()));
                                        ui.close();
                                    }
                                }
                            });
                        }
                    });
                    // × 上悬停 → 小手（其余区域保持普通箭头，暗示可点/可拖）。
                    if resp.hovered()
                        && ui
                            .ctx()
                            .pointer_interact_pos()
                            .is_some_and(|p| close_rect.contains(p))
                    {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let hovering = !selected
                        && drag_index.is_none()
                        && ui.ctx().pointer_interact_pos().is_some_and(|p| rect.contains(p));
                    let bg = Self::tab_bg(sel_fill, selected, hovering, dark);
                    ui.painter().set(bg_idx, egui::Shape::rect_filled(rect.expand2(egui::vec2(5.0, 2.0)), 0.0, bg));
                    tab_rects.push((i, rect));
} else if let Tab::Placeholder { title } = tab {
                    // ── 重启/切换命令占位页签：保持位置与标题可见，不可拖动/关闭。
                    ui.add_space(TAB_GAP);
                    let frame_resp = egui::Frame::new()
                        .corner_radius(4.0)
                        .fill(Color32::TRANSPARENT)
.inner_margin(tab_margin)
                        .show(&mut ui, |ui| {
                            ui.spacing_mut().item_spacing.x = TAB_GAP;
                            let min_width = ui.text_style_height(&egui::TextStyle::Body) * 4.0;
                            let title_w = *self
                                .title_width_cache
                                .entry(title.clone())
                                .or_insert_with(|| {
                                    ui.ctx().fonts_mut(|f| {
                                        f.layout_no_wrap(
                                            title.clone(),
                                            tab_font.clone(),
                                            Color32::TRANSPARENT,
                                        )
                                        .size()
                                        .x
                                    })
                                });
let s = TAB_GAP;
                            let icon_title_w = slot_w + s + title_w;
                            let slack = (min_width - icon_title_w - s).max(0.0);
                            let pad_l = if slack > 0.0 { slack } else { 0.0 };
                            if pad_l > 0.0 {
                                ui.add_space(pad_l);
                            }
                            let row_h = ui.text_style_height(&egui::TextStyle::Body);
                            // 重启中：旋转箭头 + 标题。
                            ui.add_sized(
                                egui::vec2(slot_w, row_h),
                                egui::Label::new(RichText::new("🔄").strong())
                                    .selectable(false),
                            );
                            ui.add(
                                egui::Label::new(RichText::new(title.as_str()).weak())
                                    .selectable(false),
                            );
                            ui.response()
                        })
.inner;
                    let rect = frame_resp.rect;
                    if !view.intersects(rect) {
                        continue;
                    }
                    // 占位页签不响应点击/拖拽：只展示，防止拖动后位置错乱。
                    tab_rects.push((i, rect));
                }
            }
        });

        // 拖动状态跨帧保存在 self.drag_tab：拖动中的每一帧刷新源索引，
        // 松手帧（dragged() 已变 false）靠它仍拿得到 from。
        if let Some(i) = drag_index {
            self.drag_tab = Some(i);
        }
        if let Some(from) = self.drag_tab {
            let pointer = ui.ctx().pointer_interact_pos();
            // 悬停目标：指针所在页签的左半 → 插到它前面，右半 → 后面。
            let mut target: Option<(usize, f32)> = None;
            if let Some(pos) = pointer {
                for (i, rect) in &tab_rects {
                    if rect.contains(pos) {
                        let bx = if pos.x < rect.center().x {
                            rect.left()
                        } else {
                            rect.right()
                        };
                        target = Some((*i, bx));
                        break;
                    }
                }
            }
            // 插入指示条：从页签栏顶部画到底部的细竖线。
            if let Some((_, bx)) = target {
                let area = ui.max_rect();
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(bx - 1.0, area.top()),
                        egui::pos2(bx + 1.0, area.bottom()),
                    ),
                    0.0,
                    sel_fill,
                );
            }
            if ui.input(|i| i.pointer.any_released()) {
                self.drag_tab = None;
                if let Some((hov, _)) = target {
                    let p = match pointer.and_then(|pos| {
                        tab_rects
                            .iter()
                            .find(|(ii, _)| *ii == hov)
                            .map(|(_, r)| (pos, *r))
                    }) {
                        Some((pos, r)) => {
                            if pos.x < r.center().x {
                                hov
                            } else {
                                hov + 1
                            }
                        }
                        None => hov,
                    };
                    self.move_tab(from, p);
                    self.status = Some("已调整页签顺序".to_string());
                }
            }
        }

        for action in actions {
            match action {
                TabAction::Activate(i) => {
                    // 点击页签后清除「输出结束」对号。
                    if let Some(Tab::Session(s)) = self.tabs.get_mut(i) {
                        s.has_been_viewed.store(true, Ordering::Relaxed);
                    }
                    self.current = i;
                    self.refresh_focus();
                }
                TabAction::Close(i) => self.close_session(i),
                TabAction::OpenDir(i) => {
                    if let Some(Tab::Session(s)) = self.tabs.get(i) {
                        let dir = s.dir.clone();
                        if Path::new(&dir).is_dir() {
                            self.open_explorer(&dir);
                        } else {
                            self.status = Some(format!("目录不存在: {dir}"));
                        }
                    }
                }
                TabAction::OpenVSCode(i) => {
                    if let Some(Tab::Session(s)) = self.tabs.get(i) {
                        let dir = s.dir.clone();
                        if Path::new(&dir).is_dir() {
                            self.open_in_vscode(&dir);
                        } else {
                            self.status = Some(format!("目录不存在: {dir}"));
                        }
                    }
                }
                TabAction::SwitchCommand(i, cmd) => self.switch_tab_command(i, cmd),
                TabAction::Restart(i) => self.restart_tab(i),
            }
        }
    }

    fn restart_tab(&mut self, idx: usize) {
        if idx == 0 || idx >= self.tabs.len() {
            return;
        }
        let Some(Tab::Session(s)) = self.tabs.get_mut(idx) else { return };
        let dir = s.dir.clone();
        let title = s.title.clone();
        let cmd = s.cmd.clone();
        // 将 child 和 master 都移到后台线程异步清理，
        // 避免 Child::drop / MasterPty::drop 阻塞 UI 线程。
        s.kill_in_background();
        // 用 Placeholder 替换而非 remove：保持页签位置不变，
        // 防止重启期间页签消失再出现的闪烁。
        self.tabs[idx] = Tab::Placeholder { title: title.clone() };
        self.refresh_focus();
        self.pending_relaunch.push(PendingRelaunch {
            tab_index: idx,
            title,
            dir,
            cmd,
        });
        self.status = Some("正在重新启动...".to_string());
    }

    /// 格式化字节数为人类可读字符串（KB/MB）。
    fn format_bytes(bytes: u64) -> String {
        if bytes < 1024 {
            format!("{bytes} B")
        } else if bytes < 1024 * 1024 {
            format!("{:.1} KB", bytes as f64 / 1024.0)
        } else {
            format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
        }
    }

    /// 通知所有会话当前主题：更新应答器用的标志，并主动广播 OSC 10/11 颜色
    /// （opencode 等 TUI 启动时会查询终端颜色来匹配自己的配色）。
    /// 当前实际生效的主题深浅：跟随系统时取系统偏好，否则取用户设置。
    fn effective_dark(&self) -> bool {
        if !self.config.settings.follow_system {
            return self.config.settings.dark_mode;
        }
        // 直接读注册表：egui system_theme() 依赖 WM_SETTINGCHANGE，
        // 窗口未激活/消息丢失时返回 None 导致跟随系统失效。
        #[cfg(target_os = "windows")]
        {
            query_windows_dark_mode()
        }
        #[cfg(not(target_os = "windows"))]
        {
            matches!(self.ctx.system_theme(), None | Some(egui::Theme::Dark))
        }
    }

    fn broadcast_theme(&mut self) {
        let dark = self.effective_dark();
        // rgb 分量必须是 1~4 位十六进制（X 约定，表示 16 位值）：之前发 6 位
        // （"rgb:ffffff/…"）是非法格式，opencode 解析出错值后用坏调色板重绘，
        // 表现为字体颜色错、文字残缺、栅格乱，且切回主题也不恢复。
        let (fg, bg) = if dark { ("ffff", "1616/1616/1a1a") } else { ("0000", "ffff/ffff/ffff") };
        let msg = format!("\x1b]10;rgb:{fg}/{fg}/{fg}\x1b\\\x1b]11;rgb:{bg}\x1b\\")
            .into_bytes();
        for tab in &mut self.tabs {
            if let Tab::Session(s) = tab {
                s.theme_dark
                    .store(dark, std::sync::atomic::Ordering::Relaxed);
                // 无布局缓存后可免清理：galley 每帧现排（见 terminal.rs 渲染循环），
                // 只作废帧重放与 ANSI 回写缓存。
                s.caret_scan = None;
                s.cached_render_shapes = None;
                s.cached_ansi_rgb = None;
                // 只推给应答过 OSC 10/11/4 颜色查询的会话（opencode 等）。
                // shell/cmd 从不查询这类序列，收到 `ESC]10;...ESC\` 会把 OSC 终止符
                // 的 `\` 直接回显成“自动输入了反斜杠”，不能广播。
                if !s.exited.load(Ordering::Acquire)
                    && s.osc_theme_aware
                        .load(std::sync::atomic::Ordering::Relaxed)
                {
                    let _ = s.writer.try_send(msg.clone());
                }
            }
        }
    }

    fn switch_tab_command(&mut self, idx: usize, cmd: String) {
        if idx == 0 || idx >= self.tabs.len() {
            return;
        }
        let Some(Tab::Session(s)) = self.tabs.get_mut(idx) else { return };
        if s.cmd.trim() == cmd.trim() {
            return;
        }
        let dir = s.dir.clone();
        let title = s.title.clone();
        // 将 child 和 master 都移到后台线程异步清理，
        // 避免 Child::drop / MasterPty::drop 阻塞 UI 线程。
        s.kill_in_background();
        // 用 Placeholder 替换而非 remove：保持页签位置不变。
        self.tabs[idx] = Tab::Placeholder { title: title.clone() };
        self.refresh_focus();
        self.pending_relaunch.push(PendingRelaunch {
            tab_index: idx,
            title,
            dir,
            cmd,
        });
        self.status = Some("正在切换命令...".to_string());
    }

/// 状态栏横向布局的一行：左侧消息（可截断），右侧固定簇（现在只剩「⋯ 更多」
/// 一个按钮）永远可见。设置 / 检查更新 / 打开目录 / 深浅色都收进它的弹出层。
///
/// 右侧固定簇是 `Layout::right_to_left` **贴右边**画的，它不看左边已经占了
/// 多宽 —— 左边放不下时不是换行而是直接盖上去。故先量出右侧簇的宽度，左段
/// （就一条消息）只拿剩下的，且左段画在**横向滚动区**里：还挤不下就横向拨
/// （滚轮即可，滚动条不画），绝不互相盖住。
///
/// 待办按钮都不在这条行里了：pi / opencode 的安装/升级入口与本程序自己的
/// 自更新（下载 / 取消 / 重启）都已搬到「设置 → 工具更新路径」末尾的
/// 「⬇ 下载 / 更新」区（`tool_entry_button_ui` / `self_update_buttons_ui`）——
/// 低频操作挤在最窄的一行里点不准，字小还得悬停才知道干什么；搬走后左段只剩
/// 消息，横向滚动那条兜底几乎用不上、仍留着以防万一。
    /// 状态栏这条消息是不是「发现新版本」提示（可点 → 跳设置页顶部下载）。
    ///
    /// **只看文案，不看 `update_latest` 是否还在**：检查更新回来的那条事件会把
    /// `update_latest` 清掉（下载完成时也会），而提示文本还会在状态栏里待一会儿。
    /// 早先要求 `update_latest.is_some() && …`，于是提示明明还在，却已经既没有
    /// 手型光标、也点不动了。
    fn status_msg_is_update_hint(text: &str) -> bool {
        text.contains("新版本")
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (text, color) = match &self.status {
                Some(s) => (s.clone(), ui_warn(ui)),
                None => match self.tabs.get(self.current) {
                    Some(Tab::Session(s)) => (
                        format!(
                            "会话 {} / {}  项目: {}",
                            self.current,
                            self.tabs.len() - 1,
                            s.title
                        ),
                        ui_gray(ui),
                    ),
                    _ => (
                        "选择项目 → 启动（内嵌终端页签）   |   添加 / 重命名 / 改路径 / 删除   |   右下角 ⋯ 更多信息"
                            .to_string(),
                        ui_gray(ui),
                    ),
                },
            };
            // 渲染时强制计算最终前景色：以状态栏实际背景（panel_fill）为基准，
            // 亮度差不足则翻成黑/白兑底，杜绝背景与文字同色。
            // override_text_color 优先级高于 RichText::color()，因此还要临时清除它，
            // 否则 RichText 颜色被全局覆写，前面的对比兜底失效。
            let bg = ui.visuals().panel_fill;
            let color = forced_contrast_color(color, bg);
            let saved_override = ui.visuals().override_text_color;
            ui.visuals_mut().override_text_color = None;
            let copy_snapshot = text.clone(); // 渲染前快照，供悬停/右键复制（label 会 move text）

            // ================= 宽度预算 =================
            // 右侧固定簇（⋯ 更多）是 right_to_left 贴右边画的，不看左边占了多宽，
            // 放不下就是直接盖上去。故先量它，剩下的才是消息的（封顶行宽的 45%、
            // 下限 120px，超出以省略号截断，悬停看全文）；连最小宽度都腾不出时整条
            // 消息不画 —— 消息可以没有。
            let item_x = ui.spacing().item_spacing.x;
            // 主题：当前状态（弹层里那一项的文案，切完下一帧就变）+ 轮转用。
            let (fs, dark) = (self.config.settings.follow_system, self.effective_dark());
            let theme_label = if fs { "🎨 跟随系统" } else if dark { "🌙 深色" } else { "☀ 浅色" };
            // 右侧固定簇只剩「⋯ 更多」一个按钮（其余全在弹层里）。
            let right_w = button_text_width(ui, "⋯ 更多") + 8.0; // + 测量误差余量
            // 左段能占多宽：行宽先扣掉右侧固定簇（它贴右边画，不让位）。
            let avail = ui.available_width();
            let left_w = (avail - right_w - item_x).max(60.0);
            let msg_w = status_msg_width(avail, left_w, 0.0, item_x);
            let row_h = ui
                .spacing()
                .interact_size
                .y
                .max(ui.text_style_height(&egui::TextStyle::Body));
            // 高度就一行：横向滚动条已隐藏（AlwaysHidden），不再为它预留一条
            // （原来不留就等于内容被自己的滚动条切掉一截）；留 2px 缝给按钮描边。
            // 横向单向滚动区默认只吃水平滚轮（鼠标滚轮是 delta.y，拨不动），开这
            // 个开关让它把垂直滚轮也当横向拨（只在本段生效，不影响设置页）——滚动
            // 条隐藏后这就是唯一的滚动入口，必须留着。
            let saved_scroll_dir = ui.style().always_scroll_the_only_direction;
            ui.style_mut().always_scroll_the_only_direction = true;
            egui::ScrollArea::horizontal()
                .id_salt("status_left_scroll")
                // 只留滚动、不画条：AlwaysHidden 时 egui 既不画横条也不从可用高度
                // 里扣它（ScrollArea::begin 的 show_bars 恒 false → current_bar_use
                // 为 0），滚轮 / Shift+滚轮照常生效（ScrollSource::mouse_wheel 默认开）。
                // 状态栏就这一行高度，露一条 8px 的滚动条既占地方又抢眼。
                .scroll_bar_visibility(egui::containers::scroll_area::ScrollBarVisibility::AlwaysHidden)
                // 宽度就是左段能用的宽度：右侧簇还没排（它贴右边缘），不写死
                // max_width 的话这里会抢走整行，右侧簇又叠回它身上。
                .max_width(left_w)
                .max_height(row_h + 2.0)
                // x 不自动收缩：宽度就这么多，挤不下就横向滚（滚动条已隐藏）；y 收缩到内容。
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                    // ---- 消息 ----
                    if msg_w > 8.0 {
                        // “发现新版本 …” 这类提示点一下直达设置页顶部的更新横幅
                        // （下载按钮在那儿）。其余消息只提供悬停全文 + 右键复制。
                        let is_update_hint = Self::status_msg_is_update_hint(&text);
                        // 不能用 ui.add_sized()：它内部固定用
                        // Layout::centered_and_justified（egui 0.36 ui.rs:1543），
                        // 于是 label 整个被**居中**摆在 msg_w × row_h 的框里——
                        // 文字比框窄就左右各空 (msg_w-宽)/2，看着就是“居中”。
                        // Label::halign 只管 label 自己 rect 内部的文字，管不到它被
                        // 摆在框的哪个位置，两者必须一起钉死：这里用
                        // left_to_right(Center) 固定横向贴左（纵向仍居中，别让文字
                        // 顶到框上沿），Label 再显式 halign(Min) 管住截断行内部。
                        let mut label_resp = ui
                            .allocate_ui_with_layout(
                                egui::vec2(msg_w, row_h),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.add(
                                        egui::Label::new(RichText::new(text).color(color))
                                            .truncate()
                                            .halign(egui::Align::Min)
                                            .sense(if is_update_hint {
                                                egui::Sense::click()
                                            } else {
                                                egui::Sense::hover()
                                            }),
                                    )
                                },
                            )
                            .inner;
                        if is_update_hint {
                            // 再钉一次光标形状：状态栏是整窗最下面一行，egui 的光标
                            // 图标可能被上层容器（面板 / 滚动区）后写覆盖，光靠
                            // Sense::click 不够稳。“能点的东西得长得像能点”。
                            // （egui 0.36 的手型叫 PointingHand。）
                            label_resp =
                                label_resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        }
                        ui.visuals_mut().override_text_color = saved_override;
                        label_resp = label_resp.on_hover_text(if is_update_hint {
                            format!("{}\n（点击跳到设置页顶部下载）", copy_snapshot)
                        } else {
                            copy_snapshot.clone()
                        });
                        // 右键快速复制整条状态栏消息（错误/提示可直接复制去反馈或贴给 AI）。
                        label_resp.context_menu(|ui| {
                            if ui
                                .button("📋 复制")
                                .on_hover_text("复制整条状态栏消息到剪贴板")
                                .clicked()
                            {
                                ui.ctx().copy_text(copy_snapshot.clone());
                                ui.close();
                            }
                        });
                        if is_update_hint && label_resp.clicked() {
                            self.open_settings();
                        }
                    } else {
                        // 连最小宽度都腾不出（窗口极窄到右侧簇就占满了整行）：这条消息
                        // 不画。左段已无待办按钮，挤不出宽度时也就没什么可让的了。
                        ui.visuals_mut().override_text_color = saved_override;
                    }
                    });
                });
            ui.style_mut().always_scroll_the_only_direction = saved_scroll_dir;
            // 右下角固定簇只剩一个「⋯ 更多」按钮：设置 / 检查更新 / 打开目录 /
            // 深浅色全部收进它的弹出层（原先挤在这一行的三个按钮点不准，也把
            // 行宽吃掉了大半）。它是 right_to_left 里**最先添加**的那个 = 最右侧。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 「⋯ 更多」按钮只响应鼠标点击，防止键盘方向键选中后回车误触发。
                let more_id = egui::Id::new("status_more_menu");
                // Sense::CLICK 不含 FOCUSABLE 位：不参与键盘焦点循环（Tab/方向键不会选中它）。
                let more_resp = ui
                    .add(egui::Button::new("⋯ 更多").sense(egui::Sense::CLICK))
                    .on_hover_text("设置 / 检查更新 / 打开目录 / 深浅色切换");
                if more_resp.clicked() && ui.input(|i| i.pointer.any_click()) {
                    egui::Popup::toggle_id(ui.ctx(), more_id);
                }
                // CloseOnClickOutside：egui 的 Popup 默认是 CloseOnClick，**点弹层
                // 里的任何一项都会顺手把菜单关掉**（ComboBox/菜单才是那个语义）。
                // 这里要的是「点了项菜单还在」，故改成只有点在弹层**外面**才关——
                // 菜单项里也就不调 ui.close()（那会立刻置 CLOSE 标记，绕过
                // close_behavior 直接关掉）。连着点两三个项（查完更新接着切主题）
                // 不用反复把菜单点开。再点「⋯ 更多」本身仍是切换开关。
                egui::Popup::from_response(&more_resp)
                    .id(more_id)
                    .open_memory(None)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| {
                        // 弹出层跟随菜单项文字自适应大小：不 set_width（窄了会截断
                        // 「📂 打开用户目录」，宽了右边留白）。
                        // 菜单项左对齐：Align::Min —— 状态栏本身是 right_to_left，
                        // 弹层内沿用 Align::RIGHT 会把文字甩到右边，左边空一大块。
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            if ui.selectable_label(false, "⚙ 设置")
                                .on_hover_text("打开设置页：TUI 启动命令 / 工具更新路径 / 供应商与模型")
                                .clicked()
                            {
                                self.open_settings();
                            }
                            if ui.selectable_label(false, "🔄 检查更新")
                                .on_hover_text("从 GitHub Release 检查本软件 + pi + opencode 的最新版本（启动/新开页签时也会自动检查）")
                                .clicked()
                            {
                                self.check_updates(false);
                            }
                            ui.separator();
                            if ui.selectable_label(false, "📂 打开用户目录")
                                .on_hover_text("打开用户目录（%USERPROFILE%），便于修改 agent 配置")
                                .clicked()
                            {
                                let dir = std::env::var("USERPROFILE")
                                    .or_else(|_| std::env::var("HOME"))
                                    .unwrap_or_else(|_| ".".to_string());
                                self.open_explorer(dir);
                            }
                            if ui.selectable_label(false, "📂 打开软件目录")
                                .on_hover_text("打开本软件 exe 所在的目录（与本软件配置目录同级）")
                                .clicked()
                            {
                                let dir = software_dir().unwrap_or_else(|| PathBuf::from("."));
                                self.open_explorer(dir);
                            }
                            ui.separator();
                            // 深浅色：菜单项本身就是当前状态（theme_label），点一下
                            // 轮转 深色 → 浅色 → 跟随系统 → 深色。菜单不关（close_behavior），
                            // 想连着调就接着点。
                            if ui.selectable_label(false, theme_label)
                                .on_hover_text("点击切换：深色 → 浅色 → 跟随系统（随 Windows 深浅自动切换）")
                                .clicked()
                            {
                                if fs {
                                    // 跟随系统 → 切回固定深色。
                                    self.config.settings.follow_system = false;
                                    self.config.settings.dark_mode = true;
                                } else if dark {
                                    // 深色 → 浅色。
                                    self.config.settings.dark_mode = false;
                                } else {
                                    // 浅色 → 跟随系统。
                                    self.config.settings.follow_system = true;
                                }
                                apply_theme(ui.ctx(), self.effective_dark());
                                self.save_config("已切换主题".to_string());
                                // 通知所有会话新主题：应答 OSC 10/11 查询 + 主动广播颜色
                                // （opencode 等 TUI 会据此匹配自己的配色）。
                                self.broadcast_theme();
                                // 延迟全量重绘：子进程收到广播后重绘需要时间，晚到的输出
                                // 可能在清缓存之后才写入；定时再清一次并强制整帧，兜住
                                // 这类脏状态。
                                self.theme_settle_at = Some(
                                    std::time::Instant::now() + std::time::Duration::from_millis(100),
                                );
                                ui.ctx().request_repaint();
                            }
                        });
                    });
            });
        });
    }

    fn home_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("project_list")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.heading("项目列表");
                ui.separator();
                ui.horizontal(|ui| {
                    // 「⚙ 设置」已挪到右下角「⋯ 更多」弹层里（所有按钮都收在一处），
                    // 左侧面板这一行只留「＋ 添加」。
                    if ui.button("＋ 添加").clicked() {
                        self.input = Some(InputDialog::AddProject {
                            name: String::new(),
                            path: String::new(),
                        });
                    }
                });
                // 搜索框 + 排序文本按钮（点击循环切换：默认 → 升序 → 降序 → 默认）
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search_query)
                            .desired_width(ui.available_width() - 70.0)
                            .hint_text("搜索项目名/目录..."),
                    );
                    let label = match self.project_sort {
                        ProjectSort::Default => "默认顺序",
                        ProjectSort::NameAsc => "名称升序",
                        ProjectSort::NameDesc => "名称降序",
                    };
                    if ui.button(label).clicked() {
                        self.project_sort = match self.project_sort {
                            ProjectSort::Default => ProjectSort::NameAsc,
                            ProjectSort::NameAsc => ProjectSort::NameDesc,
                            ProjectSort::NameDesc => ProjectSort::Default,
                        };
                    }
                });
                // 隐藏项目切换
                let hidden_count = self.config.projects.iter().filter(|p| p.hidden).count();
                if hidden_count > 0 {
                    let label = if self.show_hidden {
                        format!("隐藏项目 ({hidden_count}) - 点击隐藏")
                    } else {
                        format!("显示隐藏项目 ({hidden_count})")
                    };
                    if ui.button(label).clicked() {
                        self.show_hidden = !self.show_hidden;
                    }
                }
                ui.separator();
                if self.config.projects.is_empty() {
                    ui.label(RichText::new("暂无项目，点击「＋ 添加」创建一个。").weak());
                }
                // 过滤项目列表
                let query = self.search_query.trim().to_lowercase();
                let filtered_indices: Vec<usize> = self.config
                    .projects
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        if !self.show_hidden && p.hidden { return false; }
                        if query.is_empty() { return true; }
                        p.name.to_lowercase().contains(&query)
                            || p.path.to_lowercase().contains(&query)
                    })
                    .map(|(i, _)| i)
                    .collect();
                // 升/降序仅作用于显示层：按名称首字排序 filtered_indices（原始索引），
                // 无论何种排序都不改动 config.projects —— 自定义顺序仍由拖拽重排单独完成。
                let mut display_indices = filtered_indices;
                match self.project_sort {
                    ProjectSort::Default => {}
                    ProjectSort::NameAsc => display_indices.sort_by(|&a, &b| {
                        self.config.projects[a]
                            .name
                            .chars()
                            .next()
                            .cmp(&self.config.projects[b].name.chars().next())
                    }),
                    ProjectSort::NameDesc => display_indices.sort_by(|&a, &b| {
                        self.config.projects[b]
                            .name
                            .chars()
                            .next()
                            .cmp(&self.config.projects[a].name.chars().next())
                    }),
                }
                let sel = self.selected_project;
                let sel_fill = ui.visuals().selection.bg_fill;
                let mut actions: Vec<ProjectAction> = Vec::new();
                // 目录存在性批量走 TTL 缓存（每帧全列表 is_dir 是磁盘调用）。
                let exists_list: Vec<bool> = {
                    let cache = &mut self.dir_exists_cache;
                    self.config
                        .projects
                        .iter()
                        .map(|p| dir_exists(cache, &p.path))
                        .collect()
                };
                // 各行矩形（索引 → rect），拖动落位时用来定位插入点。
                let mut row_rects: Vec<(usize, egui::Rect)> = Vec::new();
                let mut drag_index: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for &i in &display_indices {
                            let p = &self.config.projects[i];
                            let exists = exists_list[i];
                            let hidden_mark = if p.hidden { " [隐藏]" } else { "" };
                            let label = if exists {
                                format!("● {}{}", p.name, hidden_mark)
                            } else {
                                format!("○ {}{}  (目录不存在)", p.name, hidden_mark)
                            };
                            let color = if exists {
                                ui.visuals().text_color()
                            } else {
                                ui_warn(ui)
                            };
                            // 整行可点可拖（click_and_drag 由 egui 延迟判定拖动，纯点击天然保留）；
                            // min_size 撑满行宽，整条都能点击/拖起，比只点文字好操作。
                            let resp = ui.add(
                                egui::Button::selectable(sel == i, RichText::new(label).color(color))
                                    .sense(egui::Sense::click_and_drag())
                                    .min_size(egui::vec2(ui.available_width(), 0.0)),
                            );
                            if resp.clicked() {
                                actions.push(ProjectAction::Select(i));
                            }
                            // 双击快速启动（首击已计入 clicked 完成选中，第二击两者同时触发，
                            // 按先后顺序 Select 先于 Launch 执行，选中状态无竞争）。
                            if resp.double_clicked() {
                                actions.push(ProjectAction::Launch(i));
                            }
                            // 右键菜单：目录相关动作在目录不存在时置灰。
                            resp.context_menu(|ui| {
                                if ui
                                    .add_enabled(exists, egui::Button::new("▶ 启动 (内嵌页签)"))
                                    .on_hover_text("启动内嵌终端页签")
                                    .clicked()
                                {
                                    actions.push(ProjectAction::Launch(i));
                                    ui.close();
                                }
                                if ui
                                    .add_enabled(exists, egui::Button::new("📂 打开目录"))
                                    .on_hover_text("在资源管理器中打开该目录")
                                    .clicked()
                                {
                                    actions.push(ProjectAction::OpenDir(i));
                                    ui.close();
                                }
                                if ui
                                    .add_enabled(exists, egui::Button::new("⌨ 在 VSCode 打开"))
                                    .on_hover_text("用 VS Code 打开该目录（需安装 code 命令并加入 PATH）")
                                    .clicked()
                                {
                                    actions.push(ProjectAction::OpenVSCode(i));
                                    ui.close();
                                }
                                ui.separator();
                                let hide_label = if p.hidden { "取消隐藏" } else { "隐藏项目" };
                                if ui.button(hide_label).clicked() {
                                    actions.push(ProjectAction::ToggleHide(i));
                                    ui.close();
                                }
                                if ui.button("重命名").clicked() {
                                    actions.push(ProjectAction::Rename(i));
                                    ui.close();
                                }
                                if ui.button("改路径").clicked() {
                                    actions.push(ProjectAction::EditPath(i));
                                    ui.close();
                                }
                                if ui
                                    .button("删除")
                                    .on_hover_text("从列表中移除该项目（不改动磁盘文件）")
                                    .clicked()
                                {
                                    actions.push(ProjectAction::Delete(i));
                                    ui.close();
                                }
                            });
                            // 拖动悬浮帧：记录源索引；行矩形供落位定位。
                            // 升/降序视图下显示顺序与原始数据不一致，禁用拖拽重排。
                            if resp.dragged() && self.project_sort == ProjectSort::Default {
                                drag_index = Some(i);
                            }
                            row_rects.push((i, resp.rect));
                        }
                        // 拖动到列表上下边缘时自动滚动，让拖拽能到达视野外的项目。
                        if self.drag_project.is_some()
                            && let Some(pos) = ui.ctx().pointer_interact_pos()
                        {
                            let clip = ui.clip_rect();
                            let edge = 28.0;
                            let dy = if pos.y < clip.top() + edge {
                                -24.0
                            } else if pos.y > clip.bottom() - edge {
                                24.0
                            } else {
                                0.0
                            };
                            if dy != 0.0 {
                                ui.scroll_with_delta(egui::vec2(0.0, dy));
                                ui.ctx().request_repaint();
                            }
                        }
                    });
                // 拖动状态跨帧保存在 self.drag_project：拖动中的每一帧刷新源索引，
                // 松手帧（dragged() 已变 false）靠它仍拿得到 from。
                // 升/降序视图下显示顺序与原始数据不一致，禁用拖拽重排。
                if self.project_sort != ProjectSort::Default {
                    drag_index = None;
                }
                if let Some(i) = drag_index {
                    self.drag_project = Some(i);
                }
                if self.project_sort == ProjectSort::Default
                    && let Some(from) = self.drag_project
                {
                    let pointer = ui.ctx().pointer_interact_pos();
                    // 悬停目标：指针所在行的上半 → 插到它前面，下半 → 后面；
                    // 落在最后一行下方 → 末尾，第一行上方 → 开头。
                    let mut target: Option<(usize, f32)> = None;
                    if let Some(pos) = pointer {
                        for (i, rect) in &row_rects {
                            if pos.y >= rect.top() && pos.y <= rect.bottom() {
                                let by = if pos.y < rect.center().y {
                                    rect.top()
                                } else {
                                    rect.bottom()
                                };
                                target = Some((*i, by));
                                break;
                            }
                        }
                        if target.is_none() {
                            if let Some((last_i, last)) = row_rects.last()
                                && pos.y > last.bottom()
                            {
                                target = Some((*last_i, last.bottom()));
                            }
                            if let Some((first_i, first)) = row_rects.first()
                                && pos.y < first.top()
                            {
                                target = Some((*first_i, first.top()));
                            }
                        }
                    }
                    // 插入指示线：横贯列表宽度的细横线。
                    if let Some((_, by)) = target {
                        let area = egui::Rect::from_min_max(
                            egui::pos2(ui.max_rect().left(), by - 1.5),
                            egui::pos2(ui.max_rect().right(), by + 1.5),
                        );
                        ui.painter().rect_filled(area, 0.0, sel_fill);
                    }
                    if ui.input(|i| i.pointer.any_released()) {
                        self.drag_project = None;
                        if let Some((hov, _)) = target {
                            let insert_at = match pointer.and_then(|pos| {
                                row_rects
                                    .iter()
                                    .find(|(ii, _)| *ii == hov)
                                    .map(|(_, r)| (pos, *r))
                            }) {
                                Some((pos, r)) => {
                                    if pos.y < r.center().y {
                                        hov
                                    } else {
                                        hov + 1
                                    }
                                }
                                None => hov + 1,
                            };
                            self.move_project(from, insert_at);
                            self.save_config("已调整项目顺序".to_string());
                        }
                    }
                }
                for action in actions {
                    match action {
                        ProjectAction::Select(i) => {
                            self.selected_project = i;
                            self.screen = Screen::Main;
                        }
                        ProjectAction::Launch(i) => {
                            self.selected_project = i;
                            self.launch_selected();
                        }
                        ProjectAction::OpenDir(i) => {
                            self.selected_project = i;
                            let Some(p) = self.config.projects.get(i) else { continue };
                            let dir = p.path.clone();
                            if Path::new(&dir).is_dir() {
                                self.open_explorer(&dir);
                            } else {
                                self.status = Some(format!("目录不存在: {dir}"));
                            }
                        }
                        ProjectAction::OpenVSCode(i) => {
                            self.selected_project = i;
                            let Some(p) = self.config.projects.get(i) else { continue };
                            let dir = p.path.clone();
                            if Path::new(&dir).is_dir() {
                                self.open_in_vscode(&dir);
                            } else {
                                self.status = Some(format!("目录不存在: {dir}"));
                            }
                        }
                        ProjectAction::Rename(i) => {
                            self.selected_project = i;
                            self.open_rename();
                        }
                        ProjectAction::EditPath(i) => {
                            self.selected_project = i;
                            self.open_edit_path();
                        }
                        ProjectAction::Delete(i) => {
                            self.selected_project = i;
                            self.request_delete();
                        }
                        ProjectAction::ToggleHide(i) => {
                            if let Some(p) = self.config.projects.get_mut(i) {
                                p.hidden = !p.hidden;
                                let msg = if p.hidden {
                                    format!("已隐藏: {}", p.name)
                                } else {
                                    format!("已取消隐藏: {}", p.name)
                                };
                                self.selected_project = i;
                                self.save_config(msg);
                            }
                        }
                    }
                }
            });

        egui::CentralPanel::default().show(ui, |ui| {
            self.project_detail_ui(ui);
        });
    }

    fn project_detail_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        let Some(p) = self.config.projects.get(self.selected_project).cloned() else {
            ui.heading("欢迎使用 TUI 项目管理器");
            ui.add_space(6.0);
            ui.label("在左侧选择或添加一个项目，然后点击「启动」，将在一个内嵌终端页签中于该项目目录运行配置的 TUI 程序。");
            return;
        };
        let exists = dir_exists(&mut self.dir_exists_cache, &p.path);

        ui.heading(&p.name);
        ui.separator();
        ui.label("路径:");
        ui.monospace(&p.path);
        ui.label(
            RichText::new(if exists {
                "✓ 目录存在"
            } else {
                "✗ 目录不存在"
            })
            .color(if exists { ui_ok(ui) } else { ui_warn(ui) }),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let launch = ui.add_enabled(
                exists,
                egui::Button::new(RichText::new("▶ 启动 (内嵌页签)").strong()),
            );
            if launch.clicked() {
                self.launch_selected();
            }
            let open_dir = ui.add_enabled(
                exists,
                egui::Button::new(RichText::new("📂 打开目录")),
            );
            if open_dir.clicked() {
                self.open_explorer(&p.path);
            }
            if ui.button("重命名").clicked() {
                self.open_rename();
            }
            if ui.button("改路径").clicked() {
                self.open_edit_path();
            }
            if ui.button("删除").clicked() {
                self.request_delete();
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "TUI 命令: {}  （在设置中修改）",
                    self.config.settings.tui_command
                ))
                .weak(),
            );
            if ui
                .small_button("复制")
                .on_hover_text("把当前正在使用的 TUI 启动命令复制到剪贴板")
                .clicked()
            {
                self.ctx.copy_text(self.config.settings.tui_command.clone());
                self.status = Some(format!(
                    "已复制启动命令: {}",
                    self.config.settings.tui_command
                ));
            }
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        // 别处（如状态栏的自动安装）改过配置时热更新本页快照。
        self.sync_settings_snapshot();
        ui.add_space(8.0);
        ui.heading("设置");
        ui.separator();
        // 有可更新/可安装的东西时，页顶会摆一个更新区（带下载/取消/重启按钮）。
        // 不占平时的版面：没得下就整块不画，用户不用在一堆设置里找“没得下”。
        self.update_zone_ui(ui);
        // 三大块内容都很长（命令列表 / 路径列表 / 供应商+模型表），平时用不上，
        // 全摊开会把「设置」顶成好几屏。故都做成折叠面板（手风琴，严格互斥：同
        // 一时刻只展开一块，点另一块就自动收起）：收起时只剩标题行 + 一句摘要
        // （条数 / 当前项），展开才画正文。
        self.load_settings_sec_once(ui);
        self.commands_section_ui(ui);
        self.tool_dirs_section_ui(ui);
        self.providers_section_ui(ui);
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        // 帧率设置已整体移除（曾可调 30 FPS）：持续高帧率重绘会干扰 Windows
        // 悬停激活窗口（焦点随鼠标）——30 FPS 输出中实测失效、10 FPS 正常（见
        // 921f062/0ae5904）。根治 = 去掉可调档，锁死 10 FPS（BUSY_FRAME_MS）。
        ui.add_space(12.0);
        ui.label(RichText::new("🔄 = 正在运行（有输出内容 / 进程树在计算），✅ = 输出结束待查看（切到该页签、或在页签内点击/滚动/输入、软件重新获得焦点即消失；TUI 静止等输入不算，显示空），空 = 等待输入或空闲，❌ = 已退出。\n🔄 以是否有输出内容为准，按键/粘贴等人工输入不算输出、保持空不误判 🔄；零输出页签不闪 🔄；✅ 稳定停留 2 秒即弹「任务完成」通知（仅未查看过的真任务输出轮，闲置页签不弹）；周期输出横跳会重置计时。\n快捷键：Ctrl+Tab 循环切换到下一个页签，Ctrl+Shift+Tab 切换到上一个（只在项目页签之间循环，不会切到首页/设置页）。").weak());
        ui.add_space(12.0);
        ui.label(RichText::new(format!("配置文件: {}", self.config_path.display())).weak());
    }

    /// 进设置页时读一次「上次展开哪一块」（egui persisted，与折叠状态一样跨重
    /// 启）。只读一次：`open_settings_sec` 是互斥的唯一真相，每次进来都重读会把
    /// 用户刚点开的块又拉回旧值。
    fn load_settings_sec_once(&mut self, ui: &egui::Ui) {
        if self.settings_sec_loaded {
            return;
        }
        self.settings_sec_loaded = true;
        self.open_settings_sec = match Self::load_open_settings_sec(ui.ctx()) {
            // 首次：默认展开第一块（沿用旧行为），别让设置页一进来空空如也。
            None => SETTINGS_SEC_IDS.first().copied(),
            // 有记录就尊重它（包括用户自己全收起过）。
            Some(sec) => sec,
        };
    }

    /// 设置页页顶的「更新区」：本程序自己的新版本 + pi / opencode 的安装/升级入口。
    /// **只在需要动手时才出现**（本程序有新版可下 / 正在下 / 下完待重启，或某个
    /// 工具可安装/可升级），其余时间整块不画，不占设置页版面。
    ///
    /// 为什么都收在这一处：更新是全局性的（不属于任何一块设置），“有什么可更新”
    /// 一次看完，不必先展开「工具更新路径」才知道 pi 没装；页面一进来就能看到，
    /// 状态栏的「发现新版本 …」提示点一下也正好跳到这里。
    fn update_zone_ui(&mut self, ui: &mut egui::Ui) {
        // 本程序这一路：`update_latest` 单独不够——下载完成事件会把它清回
        // None，那时得靠 update_done 把「重启应用」按钮留在横幅上。
        let self_pending =
            self.update_latest.is_some() || self.downloading || self.update_done;
        // 有任一工具在下 → 也算“有事要办”（工具安装完 exe 就落在 exe 同级目录，
        // 那时 `tool_entry_kind` 会给 None；只看它会让下载中的取消按钮凭空消失）。
        let tools_busy = self.tools.iter().any(|t| t.downloading);
        let tools_pending = tools_busy || self.any_tool_entry_available();
        if !self_pending && !tools_pending {
            return; // 都是最新、工具也都齐了：不画。
        }
        // 描边高亮框：一眼能在这页里找到，且不靠滚动位置。
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(10))
            .stroke(egui::Stroke::new(
                1.0,
                if self_pending {
                    ui.visuals().warn_fg_color
                } else {
                    ui.visuals().text_color()
                },
            ))
            .show(ui, |ui| {
                if self_pending {
                    let tag = self.update_latest.clone();
                    let (icon, head) = if self.update_done {
                        // 下载完成事件会清掉 latest（那条通道不携 tag），故不提版本号。
                        ("✅", "新版本已就绪，重启一下即生效".to_string())
                    } else if self.downloading {
                        (
                            "⬇",
                            match &tag {
                                Some(t) => format!("本程序：正在下载新版本 {t}…"),
                                None => "本程序：正在下载新版本…".to_string(),
                            },
                        )
                    } else {
                        (
                            "⬇",
                            format!(
                                "本程序：发现新版本 {}（当前 {}）",
                                tag.unwrap_or_default(),
                                crate::app_version()
                            ),
                        )
                    };
                    ui.label(RichText::new(format!("{icon} {head}")).strong());
                    ui.horizontal_wrapped(|ui| {
                        self.self_update_buttons_ui(ui);
                    });
                }
                // 工具入口：每个按钮自己判定该不该出现（已装且有新版 / 未装可装 /
                // 装在别处可收进本软件目录），一个都没有时这行不画。
                if tools_pending {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(if self_pending || tools_busy {
                            "工具（pi / opencode）："
                        } else {
                            "⬇ 工具（pi / opencode）可安装 / 升级："
                        })
                        .strong(),
                    );
                    ui.horizontal_wrapped(|ui| {
                        for i in 0..TOOL_SPECS.len() {
                            self.tool_entry_button_ui(ui, i);
                        }
                    });
                }
            });
        ui.add_space(6.0);
    }

    /// 是否至少有一个工具的安装/升级入口按钮会出现（`tool_entry_button_ui` 里
    /// 那套判定的同款，在按钮之外做一次，以便调用方先决定“这一行要不要画”）。
    fn any_tool_entry_available(&self) -> bool {
        self.tools.iter().enumerate().any(|(i, t)| {
            let spec = &TOOL_SPECS[i];
            let fresh = fresh_tool_dir(spec);
            tool_entry_visible(
                t.downloading,
                t.latest.is_some() && !t.missing,
                t.missing,
                fresh.as_ref().is_some_and(|d| d.join(spec.exe_name).is_file()),
                fresh.as_ref().is_some_and(|d| !d.as_os_str().is_empty()),
            )
        })
    }

    /// 折叠块在 egui 里的 id 口径（`settings_section` 要用它驱动展开动画）。
    ///
    /// **必须和 `egui::CollapsingHeader` 内部算出来的那个一模一样**，否则我们动它
    /// 的展开动画它读不到——手风琴就漏了，能同时展开好几块（这坑踩过：egui 先把
    /// salt 压成 u64 再哈希，且 header 还自带一层 `ui.vertical` 子 ui）。
    ///
    /// 三段都要对齐：① `CollapsingHeader::show` 会先包一层 `ui.vertical`，子 ui 的 id
    /// 多一级 `"child"`；② 标题行再用 `IdSalt` 压过的 salt 派生（所以不能图省事直接
    /// `make_persistent_id(id_salt)`，那样哈希的是字符串本身）。口径由测试
    /// `header_id_matches_our_id` 钉住：egui 哪天改了这里会立刻红，不会又悄悄漏成
    /// 「能同时展开两块」。
    fn settings_section_id(ui: &egui::Ui, id_salt: &str) -> egui::Id {
        ui.id()
            .with(egui::IdSalt::new("child"))
            .with(egui::IdSalt::new(id_salt))
    }

    /// 设置页折叠面板 1/3：TUI 启动命令。标题行带摘要（命令数 + 当前选中项），
    /// 收起时只剩这一行。
    fn commands_section_ui(&mut self, ui: &mut egui::Ui) {
        let cur = if self.settings_command.is_empty() {
            "（未选）".to_string()
        } else {
            self.settings_command.clone()
        };
        let title = format!("TUI 启动命令（{} 个 · 当前 {}）", self.settings_commands.len(), cur);
        // 默认展开：三块里启动命令翻得最勤；另外两块默认收起，要用再点开。
        let keep = self.open_settings_sec;
        if let Some(want_open) = Self::settings_section(
            ui,
            "settings_sec_cmds",
            keep == Some("settings_sec_cmds"),
            title,
            |ui| {
                self.commands_body_ui(ui);
            },
        ) {
            self.note_settings_sec_clicked(ui, "settings_sec_cmds", want_open);
        }
    }

    /// 启动命令区正文（折叠面板 1/3 的 body）：选中 / 拖动排序 / 内联编辑 /
    /// 删除 / 复制 / 添加，改动自动保存。
    fn commands_body_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new("点击选择启动时要用的命令，可添加多个，拖动排序，改动自动保存。")
                .weak()
                .small(),
        );
        ui.add_space(4.0);

        let mut dirty = false;
        let mut remove_idx: Option<usize> = None;
        // 各行矩形（索引 → rect），拖动落位时用来定位插入点。
        let mut row_rects: Vec<(usize, egui::Rect)> = Vec::new();
        let mut drag_index: Option<usize> = None;
        let sel_fill = ui.visuals().selection.bg_fill;
        let n_cmds = self.settings_commands.len();
        for i in 0..n_cmds {
            let cmd = self.settings_commands[i].clone();
            ui.horizontal(|ui| {
                let selected = *cmd == self.settings_command;
                let is_editing = self.settings_edit_idx == Some(i);
                if is_editing {
                    // ── 内联编辑模式：TextEdit + 保存/取消 ──
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings_edit_buffer)
                            .desired_width(280.0),
                    );
                    if ui.small_button("保存").clicked() {
                        let new_cmd = self.settings_edit_buffer.trim().to_string();
                        // 改名也按键判重：改成 `nvim.exe` / `NVIM` 不算新命令。
                        if !new_cmd.is_empty()
                            && !self.has_tui_command(&new_cmd, Some(i))
                        {
                            let old_cmd = std::mem::take(&mut self.settings_commands[i]);
                            if self.settings_command == old_cmd {
                                self.settings_command = new_cmd.clone();
                            }
                            self.settings_commands[i] = new_cmd;
                            dirty = true;
                        } else if !new_cmd.is_empty() {
                            self.status = Some(format!("命令已存在，不重复添加: {new_cmd}"));
                        }
                        self.settings_edit_idx = None;
                    }
                    if ui.small_button("取消").clicked() {
                        self.settings_edit_idx = None;
                    }
                    return; // 编辑行不参与拖动/删除等
                }
                // ── 正常显示模式 ──
                let resp = ui.add(
                    egui::Button::selectable(
                        selected,
                        RichText::new(if selected {
                            format!("◉ {cmd}")
                        } else {
                            format!("○ {cmd}")
                        }),
                    )
                    .sense(egui::Sense::click_and_drag()),
                );
                if resp.clicked() {
                    self.settings_command = cmd.clone();
                    dirty = true;
                }
                if resp.dragged() {
                    drag_index = Some(i);
                }
                row_rects.push((i, resp.rect));
                if ui
                    .small_button("编辑")
                    .on_hover_text("内联编辑此命令")
                    .clicked()
                {
                    self.settings_edit_idx = Some(i);
                    self.settings_edit_buffer = cmd.clone();
                }
                if ui
                    .small_button("删除")
                    .on_hover_text("从命令列表中移除")
                    .clicked()
                {
                    remove_idx = Some(i);
                }
                if ui
                    .small_button("复制")
                    .on_hover_text("把此命令复制到系统剪贴板")
                    .clicked()
                {
                    self.ctx.copy_text(cmd.clone());
                    self.status = Some(format!("已复制命令: {cmd}"));
                }
            });
        }
        // 拖动状态跨帧保存在 self.drag_command：拖动中的每一帧刷新源索引，
        // 松手帧（dragged() 已变 false）靠它仍拿得到 from。
        if let Some(i) = drag_index {
            self.drag_command = Some(i);
        }
        if let Some(from) = self.drag_command {
            let pointer = ui.ctx().pointer_interact_pos();
            // 悬停目标：指针所在行的上半 → 插到它前面，下半 → 后面；
            // 落在最后一行下方 → 末尾，第一行上方 → 开头。
            let mut target: Option<(usize, f32)> = None;
            if let Some(pos) = pointer {
                for (i, rect) in &row_rects {
                    if pos.y >= rect.top() && pos.y <= rect.bottom() {
                        let by = if pos.y < rect.center().y {
                            rect.top()
                        } else {
                            rect.bottom()
                        };
                        target = Some((*i, by));
                        break;
                    }
                }
                if target.is_none() {
                    if let Some((last_i, last)) = row_rects.last()
                        && pos.y > last.bottom()
                    {
                        target = Some((*last_i, last.bottom()));
                    }
                    if let Some((first_i, first)) = row_rects.first()
                        && pos.y < first.top()
                    {
                        target = Some((*first_i, first.top()));
                    }
                }
            }
            // 插入指示线：横贯列表宽度的细横线。
            if let Some((_, by)) = target {
                let area = egui::Rect::from_min_max(
                    egui::pos2(ui.max_rect().left(), by - 1.5),
                    egui::pos2(ui.max_rect().right(), by + 1.5),
                );
                ui.painter().rect_filled(area, 0.0, sel_fill);
            }
            if ui.input(|i| i.pointer.any_released()) {
                self.drag_command = None;
                if let Some((hov, _)) = target {
                    let insert_at = match pointer.and_then(|pos| {
                        row_rects
                            .iter()
                            .find(|(ii, _)| *ii == hov)
                            .map(|(_, r)| (pos, *r))
                    }) {
                        Some((pos, r)) => {
                            if pos.y < r.center().y {
                                hov
                            } else {
                                hov + 1
                            }
                        }
                        None => hov + 1,
                    };
                    // 在原索引空间里算新位置，落下后按新序写回；
                    // settings_command 按值关联，重排后选中项自然跟随，无需修索引。
                    let new_p = if insert_at > from { insert_at - 1 } else { insert_at };
                    if new_p != from {
                        let c = self.settings_commands.remove(from);
                        self.settings_commands.insert(new_p, c);
                        dirty = true;
                    }
                }
            }
        }
        if let Some(i) = remove_idx
            && i < self.settings_commands.len()
        {
            let removed = self.settings_commands.remove(i);
            if self.settings_command == removed {
                self.settings_command = self
                    .settings_commands
                    .first()
                    .cloned()
                    .unwrap_or_default();
            }
            dirty = true;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.settings_new_command)
                    .desired_width(280.0)
                    .hint_text("新命令，如 lazygit / htop"),
            );
            if ui
                .button("浏览…")
                .on_hover_text("选择可执行文件")
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("选择 TUI 可执行文件")
                    .add_filter("可执行文件", &["exe", "bat", "cmd", "com"])
                    .pick_file()
            {
                self.settings_new_command = path.to_string_lossy().to_string();
            }
            let clicked = ui.button("添加").clicked();
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if clicked || enter {
                let cmd = self.settings_new_command.trim().to_string();
                if !cmd.is_empty() && !self.has_tui_command(&cmd, None) {
                    self.settings_commands.push(cmd.clone());
                    self.settings_new_command.clear();
                    dirty = true;
                } else if !cmd.is_empty() {
                    // 已存在：只提示、不入列（`nvim` / `NVIM` / `D:\x\nvim.exe` 同义）。
                    self.status = Some(format!("命令已存在，不重复添加: {cmd}"));
                }
            }
        });
        // 边输边对比已有配置：同一条命令再输一遍时就地标黄，省得点了没反应。
        let typed = self.settings_new_command.trim().to_string();
        if !typed.is_empty() && self.has_tui_command(&typed, None) {
            ui.label(
                RichText::new(format!("⚠ 已存在（{typed}），不会重复添加"))
                    .color(ui_warn(ui))
                    .small(),
            );
        }
        ui.label(
            RichText::new("示例: nvim / lazygit / htop / cmd / bash")
                .weak()
                .small(),
        );
        if dirty {
            self.config.settings.tui_command = self.settings_command.trim().to_string();
            self.config.settings.tui_commands = self.settings_commands.clone();
            self.save_config("设置已自动保存".to_string());
        }
    }

    /// 设置页折叠面板 2/3：工具更新路径（检查更新时到哪找 pi / opencode）。
    /// 安装/升级按钮不在这里，工具可装/可升时页顶的「更新区」会亮出来。
    fn tool_dirs_section_ui(&mut self, ui: &mut egui::Ui) {
        let scan = if self.config.settings.tool_search_path {
            "找不到时再扫 PATH"
        } else {
            "不扫 PATH"
        };
        let title = format!(
            "工具更新路径（{} 个位置 · {}）",
            self.settings_tool_dirs.len(),
            scan
        );
        let keep = self.open_settings_sec;
        if let Some(want_open) = Self::settings_section(
            ui,
            "settings_sec_tool_dirs",
            keep == Some("settings_sec_tool_dirs"),
            title,
            |ui| {
                self.tool_dirs_ui(ui);
            },
        ) {
            self.note_settings_sec_clicked(ui, "settings_sec_tool_dirs", want_open);
        }
    }

    /// 设置页折叠面板 3/3：供应商配置（页签 + 供应商/模型表），标题行显示当前
    /// 在配哪一套。安装/升级按钮不在这里，去「工具更新路径」末尾那一区。
    fn providers_section_ui(&mut self, ui: &mut egui::Ui) {
        // oh-my-pi 那张页签已隐藏（PROVIDER_TABS 里没它）：还停在它上面时（老记忆
        // / 老配置里就是 1）落回 pi，否则会显示出一块没有按钮被选中的供应商配置。
        if !PROVIDER_TABS.iter().any(|(_, tab)| *tab == self.model_settings_tab) {
            // 先把那张隐藏页签里未失焦的改名/数字编辑提交掉，别白丢。
            self.flush_provider_rename(self.model_settings_tab);
            self.flush_model_num_edit(self.model_settings_tab);
            self.model_settings_tab = 0;
        }
        let tab = PROVIDER_TABS
            .iter()
            .find(|(_, t)| *t == self.model_settings_tab)
            .map(|(name, _)| *name)
            .unwrap_or("pi");
        let title = format!("供应商配置（当前：{tab}）");
        let keep = self.open_settings_sec;
        if let Some(want_open) = Self::settings_section(
            ui,
            "settings_sec_providers",
            keep == Some("settings_sec_providers"),
            title,
            |ui| {
                self.providers_body_ui(ui);
            },
        ) {
            self.note_settings_sec_clicked(ui, "settings_sec_providers", want_open);
        }
    }

    /// 供应商配置区正文（折叠面板 3/3 的 body）：页签条 + 当前页签的供应商/模型表。
    fn providers_body_ui(&mut self, ui: &mut egui::Ui) {
        // 页签条（PROVIDER_TABS：pi / opencode，oh-my-pi 已隐藏）。窗口窄时自动换行。
        ui.horizontal_wrapped(|ui| {
            for (name, tab) in PROVIDER_TABS {
                if ui
                    .add(egui::Button::selectable(self.model_settings_tab == tab, name))
                    .clicked()
                    && self.model_settings_tab != tab
                {
                    // 切页签前提交原页签里未失焦的供应商改名/数字编辑（失焦事件只在字段被
                    // 渲染的帧里能捕捉，切页签的点击发生在对方页签渲染之前，会漏）避免丢失。
                    self.flush_provider_rename(self.model_settings_tab);
                    self.flush_model_num_edit(self.model_settings_tab);
                    self.model_settings_tab = tab;
                }
            }
        });
        self.model_settings_ui(ui, self.model_settings_tab);
    }

    /// 本程序自己的更新按钮组（下载新版本 / 取消下载 / 装完重启），只出现在设置页
    /// 顶部的更新横幅里（该横幅仅在需要下载时出现）。
    ///
    /// 状态栏右簇只留了「🔄 检查更新」（查有没有新版本，一键）；要下就得有个
    /// 不那么挤的地方点，否则长标签会挤掉右簇的深浅色按钮。状态栏消息里那条
    /// “发现新版本 …” 点一下就能跳到这块。
    fn self_update_buttons_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(tag) = self.update_latest.clone() {
            if self.downloading {
                // 下载中：进度文字 + 取消（具体百分比在状态栏消息里随下载线程刷新）。
                ui.label(
                    RichText::new(format!("⬇ 下载中… {tag}"))
                        .color(ui.visuals().widgets.inactive.text_color()),
                );
                if ui.button("✕ 取消").on_hover_text("取消当前下载").clicked() {
                    self.cancel_download
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    self.status = Some("正在取消下载…".to_string());
                }
            } else if ui
                .button(format!("⬇ 下载 {tag}"))
                .on_hover_text("自动下载新版本到当前目录，完成后替换旧版本")
                .clicked()
            {
                self.start_download(&tag);
            }
        }
        // 下载完成待重启：新 exe 已替换到当前路径，点击重启立刻生效。
        if self.update_done && ui
            .button("🔄 重启应用")
            .on_hover_text("新版本已下载并替换，点击重启使新版本生效")
            .clicked()
        {
            self.restart_app();
        }
    }

    /// 单个工具（pi / opencode）的安装/升级入口按钮，画在设置页**页顶的更新区**
    /// （本程序的新版本 + 工具安装/升级都收在那一块里）。四种情形的判定沿用状态栏
    /// （原代码整段搬过来，只把 `continue` 换成 `return`）：已装且有新版 → 「⬇ pi vX」；
    /// 没装 → 「⬇ 安装 pi」（tag 未知则点下去现查）；装在别处、本软件目录里没有 →
    /// 「⬇ 装 pi 到本软件目录」；拿不到可写目录 → 不画按钮。下载中 → 进度文字 +
    /// 「✕ 取消」。
    ///
    /// 为什么搬过来：装/升级是低频操作，却挤在状态栏最窄的一行里，字小、要点得准，
    /// 挤不下时还得横向滚才看全；而“有什么可更新”一次看完才是顺手的路径。搬走后
    /// 状态栏左段只剩消息，横向滚动那条兜底基本用不上（仍保留，以防消息被顶到
    /// 看不见）。
    fn tool_entry_button_ui(&mut self, ui: &mut egui::Ui, idx: usize) {
        let spec = &TOOL_SPECS[idx];
        // 先把 self.tools 里的值取出来：下面点按钮要 &mut self（start_tool_download）。
        let (downloading, latest, local, missing, dir) = {
            let t = &self.tools[idx];
            (
                t.downloading,
                t.latest.clone(),
                t.local.clone(),
                t.missing,
                t.install_dir.clone(),
            )
        };
        // 本软件同级目录里已经有一份？决定要不要再给「装到本软件目录」。
        let fresh_dir = fresh_tool_dir(spec);
        let fresh_present = fresh_dir
            .as_ref()
            .is_some_and(|d| d.join(spec.exe_name).is_file());
        if downloading {
            ui.label(
                RichText::new(format!("⬇ {} 下载中…", spec.label))
                    .color(ui.visuals().widgets.inactive.text_color()),
            );
            if ui
                .button("✕ 取消")
                .on_hover_text(format!("取消 {} 的下载", spec.label))
                .clicked()
            {
                self.tool_cancel[idx].store(true, Ordering::Relaxed);
                self.status = Some(format!("正在取消 {} 下载…", spec.label));
            }
            return;
        }
        // 按钮存在的四种情形：已装且有新版 / 未装但已拿到 tag /
        // 未装且 tag 未知（点下去时现查）/ **已装在别处、本软件目录
        // 里没有**（装一份进来）。后两种要求目录可用。
        let (label, tip, into_fresh) = match tool_entry_kind(
            latest.is_some() && !missing,
            missing,
            fresh_present,
            fresh_dir.as_ref().is_some_and(|d| !d.as_os_str().is_empty()),
        ) {
            ToolEntryKind::Update => {
                let tag = latest.clone().unwrap_or_default();
                let cur = if local.is_empty() { "未知".to_string() } else { local.clone() };
                (
                    format!("⬇ {} {tag}", spec.label),
                    format!(
                        "{label} 有新版本 {tag}（当前 {cur}）：点击从国内镜像源下载并替换 {exe}",
                        label = spec.label,
                        exe = spec.exe_name
                    ),
                    false,
                )
            }
            ToolEntryKind::InstallFresh if missing => {
                let dir_s = dir.to_string_lossy().into_owned();
                (
                    match &latest {
                        Some(tag) => format!("⬇ 安装 {} {tag}", spec.label),
                        None => format!("⬇ 安装 {}", spec.label),
                    },
                    format!(
                        "未检测到 {exe}（本软件同级目录 / 设置里的工具路径 / PATH 都没有）：点击自动下载并安装到 {dir_s}",
                        exe = spec.exe_name
                    ),
                    true,
                )
            }
            // 本机 PATH / 别的目录里有，但本软件同级目录里没有：
            // “检测到即已装”会让这种机器上一个按钮都没有，用户没
            // 办法把工具收进本软件目录（换机器、PATH 被改、全局
            // 安装被删就断供）。给一条“装到本软件目录”，装好后本
            // 软件目录这份优先被找到（查找顺序里它在最前），按钮自退。
            ToolEntryKind::InstallFresh => {
                let d_s = fresh_dir
                    .as_ref()
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let cur = if local.is_empty() {
                    "未知".to_string()
                } else {
                    local.clone()
                };
                (
                    format!("⬇ 装 {} 到本软件目录", spec.label),
                    format!(
                        "本机在 {found} 找到 {label}（{cur}），但本软件同级目录里没有：点击另装一份到 {d_s}，之后检查更新以本软件目录这份为准",
                        found = dir.join(spec.exe_name).to_string_lossy(),
                        label = spec.label
                    ),
                    true,
                )
            }
            // 拿不到本软件目录（current_exe 失败）→ 无处可装，不给假入口
            ToolEntryKind::None => return,
        };
        if into_fresh {
            if fresh_dir.as_ref().is_none_or(|d| d.as_os_str().is_empty()) {
                return;
            }
        } else if dir.as_os_str().is_empty() {
            return; // 拿不到安装目录 → 无处可装，不给假入口
        }
        if ui.button(label).on_hover_text(tip).clicked() {
            self.start_tool_download(idx, into_fresh);
        }
    }

    /// 折叠面板通用外壳：粗体标题（带摘要）+ 展开才画的 body，**三个分区互斥**
    /// （同一时刻最多只展开一块）。
    ///
    /// **互斥的唯一执行点**：开合只认 `force_open`（= `open_settings_sec` 说的
    /// 「当前该开着的那块」），每帧都用 `CollapsingHeader::open(Some(..))` 强制给
    /// header。所以哪怕底层 `CollapsingState` 里同时存着两个 true（老记忆、手工改
    /// memory），这一帧也只会画出一块——不再依赖「点开自己再去关别人」那种跨控件
    /// 改状态的时序（那种写法一旦 id 口径对不上就会失效）。
    ///
    /// 点标题只回报「这一帧被点了」（`Some(want_open)` = 想切成开还是关），真正的
    /// 开合留给下一帧由调用方改 `open_settings_sec` 统一决定：不在被点的那一帧里
    /// 抢着改状态，就没有「先开的还没关、后开的又开了」的缝。三段都收起也允许
    /// （再点一次当前那块即可），这样「全收起」仍是可达状态。
    ///
    /// 收起时要把展开动画**直接归零**：egui 的收起动画会在其后 ~0.2s 里继续把正文
    /// 画出来（`show_body_unindented` 按 openness 裁剪着画），而本软件锁 10 FPS，
    /// 那就是整整一帧「点开的和刚收的都在」——正是「能同时展开两个」的观感。归零后
    /// 收起是硬切，展开动画只留给刚点开的那一块（此时别的块已归零，不会有第二个）。
    ///
    /// 正文只在 `force_open` 时才会被调用，里面那些输入框 / 拖动排序自然一起停用，
    /// 不会出现「看不见却能改」的幽灵控件。body 缩进一级（`show` 自带），里面是垂直
    /// 流，宽度照旧铺满。
    ///
    /// 注意它是关联函数（不带 &mut self）：body 闭包要借用 self，签名里再带
    /// &mut self 就双重可变借用了。
    fn settings_section(
        ui: &mut egui::Ui,
        id_salt: &'static str,
        force_open: bool,
        title: String,
        body: impl FnOnce(&mut egui::Ui),
    ) -> Option<bool> {
        if !force_open {
            // animation_time = 0.0 → 立刻归零（egui 内部除零有 is_finite 兜底）。
            ui.ctx()
                .animate_bool_with_time(Self::settings_section_id(ui, id_salt), false, 0.0);
        }
        let resp = egui::CollapsingHeader::new(RichText::new(title).strong())
            .id_salt(id_salt)
            // 开合交给 force_open，header 自己不再按点击 toggle（互斥只认 force_open；
            // 点击只用来回报 want_open）。
            .open(Some(force_open))
            .show(ui, |ui| {
                ui.add_space(4.0);
                body(ui);
                ui.add_space(4.0);
            });
        // header_response.clicked() 是「这一帧点在 header 上」，比 fully_open()
        // 及时（后者要等展开动画播完，10 FPS 下慢半拍）。
        resp.header_response.clicked().then(|| !force_open)
    }

    /// 记下刚刚被点的折叠块：want_open = true → 它成为唯一展开项；点的是当前展开的
    /// 那一块（want_open = false）→ 全关（手风琴就该一块都不留）。下一帧其余块就被
    /// `settings_section` 的 force 收掉。状态直接改字段，动画得自己催一帧。
    fn note_settings_sec_clicked(&mut self, ui: &egui::Ui, id_salt: &'static str, want_open: bool) {
        self.open_settings_sec = want_open.then_some(id_salt);
        Self::save_open_settings_sec(ui.ctx(), self.open_settings_sec);
        ui.ctx().request_repaint();
    }

    /// 「当前展开哪一块」的唯一真相存在哪儿：egui 的 persisted data（跟折叠状态
    /// 一样跨重启）。用 `Option<String>`：无记录 = 从没展开过（视为全收起）。
    const OPEN_SEC_KEY: &'static str = "settings_open_sec";

    fn save_open_settings_sec(ctx: &egui::Context, sec: Option<&'static str>) {
        // 值的类型是 `Option<String>`（存 `None` = 记着“全收起”），与读取端的
        // `get_persisted` 泛型参数一致：`get_persisted::<T>` 返回 `Option<T>`，所以
        // `let raw: Option<Option<String>> = …get_persisted(…)` 里的 T 正是
        // `Option<String>`。类型不一致 egui 会当成两个 key（持久化按 TypeId 分桶）。
        ctx.data_mut(|d| {
            d.insert_persisted(
                egui::Id::new(Self::OPEN_SEC_KEY),
                sec.map(|s| s.to_string()),
            )
        });
    }

    /// 读回展开状态：`None` = 从没展开过（首次进设置页）；`Some(None)` = 记着
    /// “全收起”；`Some(Some(id))` = 记着哪块。记录里的 id 已不存在（老版本遗留）
    /// 当全收起。
    ///
    /// `get_persisted::<T>` 返回的是 `Option<T>`：把结果标注成
    /// `Option<Option<String>>` 就等价于 T = `Option<String>`，外层 None = 没这个 key。
    fn load_open_settings_sec(ctx: &egui::Context) -> Option<Option<&'static str>> {
        // persisted 数据的读取也要走 data_mut（get_persisted 要 &mut self）。
        let raw: Option<Option<String>> = ctx
            .data_mut(|d| d.get_persisted(egui::Id::new(Self::OPEN_SEC_KEY)));
        raw.map(|sec| sec.and_then(|s| SETTINGS_SEC_IDS.iter().copied().find(|id| *id == s)))
    }

    /// 启动命令是否已存在（按 config::tui_command_key 等价判重，大小写/`.exe`
    /// 后缀/路径写法不同均视为同一条）。skip_idx = 改名时跳过自己那一行。
    fn has_tui_command(&self, cmd: &str, skip_idx: Option<usize>) -> bool {
        let key = config::tui_command_key(cmd);
        if key.is_empty() {
            return false;
        }
        self.settings_commands
            .iter()
            .enumerate()
            .any(|(i, c)| Some(i) != skip_idx && config::tui_command_key(c) == key)
    }

    /// 装完工具后的本地状态刷新：**不联网**（拿 tag 那部分仍交给「检查更新」），
    /// 只重新定位 exe + 读一次 `--version`。
    ///
    /// 为什么装完要刷一遍：状态栏那个「⬇ 安装 / ⬇ pi vX」按钮是**上次检查**
    /// 的快照。装好 opencode 之后，pi 的状态还停在那次检查的那一刻——中间
    /// 用户自己装了 pi、或改了工具路径，按钮就跟磁盘现状脱节了（明明装好还
    /// 提示安装，或反过来少了一个下载入口）。所以 Done 里就地再定位一次，
    /// 保证每个工具的按钮都对得上现状。
    fn refresh_tool_presence(&mut self) {
        let dirs = self.tool_search_dirs();
        let use_path = self.config.settings.tool_search_path;
        for idx in 0..TOOL_SPECS.len() {
            if self.tools[idx].downloading {
                continue; // 装到一半别去动它
            }
            let spec = &TOOL_SPECS[idx];
            match find_tool_exe(spec, &dirs, use_path) {
                Some(exe) => {
                    let install_dir = exe
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from("."));
                    let local = local_version_with_floor(
                        &local_tool_version(&exe),
                        self.tools[idx].installed_tag.as_deref(),
                    );
                    self.tools[idx].local = local;
                    self.tools[idx].install_dir = install_dir;
                    if self.tools[idx].missing {
                        // 在我们下载期间被装上了：“未安装”作废，不再提示安装。
                        self.tools[idx].missing = false;
                        self.tools[idx].latest = None;
                    }
                }
                None => {
                    // 真的没有：只给「⬇ 安装」（版本号点下去现查），安装目录退回
                    // “软件同级目录”那个可写位置，拿不到就不给假入口。
                    self.tools[idx].missing = true;
                    self.tools[idx].latest = None;
                    self.tools[idx].local = String::new();
                    self.tools[idx].install_dir =
                        fresh_tool_dir(spec).unwrap_or_else(|| self.tools[idx].install_dir.clone());
                }
            }
        }
    }

    /// 设置页的「启动命令 / 工具更新路径」是**快照**（用户可能正在这页上编辑，
    /// 不能每帧被配置覆盖），但快照也得在「配置被本程序别处改过」时热更新：
    /// 比如刚在状态栏自动装好 opencode，Done 里往 tui_commands 追加了一条，
    /// 此时设置页已经开着，不同步的话用户看到的还是旧列表（像是没生效），
    /// 得关掉重开才突然出现。用内容比对当变更标记，不为它给 Config 加字段。
    fn sync_settings_snapshot(&mut self) {
        if self.config.settings.tui_commands != self.settings_commands {
            self.settings_commands = self.config.settings.tui_commands.clone();
        }
        if self.config.settings.tool_paths != self.settings_tool_dirs {
            self.settings_tool_dirs = self.config.settings.tool_paths.clone();
        }
        // 选中项：配置里的 tui_command 才是“用户正在用的那条”，别处改了
        // （首配默认命令、删掉当前项）也得跟上。注意只在它仍存在于列表时
        // 同步，否则会把“列表里没有的当前命令”硬拽成空。
        let tui_command = self.config.settings.tui_command.clone();
        if tui_command != self.settings_command
            && self.settings_commands.iter().any(|c| c == &tui_command)
        {
            self.settings_command = tui_command;
        }
    }

    /// 「启动命令」列表里能被自动安装的绝对路径顶替的那一条下标：与新装 exe
    /// 同义（tui_command_key 相同）且**能整体替换**的条目。
    ///
    /// 能整体替换 = 带路径的（可含空格，整条即路径）或不带参数的裸命令名；
    /// `pi --foo` 这种带参数的替换掉会丢用户参数，不动它（只会被追加一条）。
    /// 纯函数，便于单测。
    fn replaceable_command_idx(list: &[String], key: &str) -> Option<usize> {
        list.iter().position(|c| {
            if config::tui_command_key(c) != key {
                return false;
            }
            let t = c.trim().trim_matches('"');
            t.contains('\\') || t.contains('/') || t.split_whitespace().count() == 1
        })
    }

    /// 启动命令条目现在还能不能用（真能跑起来吗）：带路径的看文件在不在，裸
    /// 命令名（如 `pi`）扫 PATH 找同名 exe。两者都没命中 = 死命令。
    fn tui_command_usable(cmd: &str) -> bool {
        let t = cmd.trim().trim_matches('"').trim();
        if t.is_empty() {
            return false;
        }
        if t.contains('\\') || t.contains('/') {
            return std::path::Path::new(t).is_file();
        }
        let name = t.split_whitespace().next().unwrap_or(t);
        let exe = if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
            name.to_string()
        } else {
            format!("{name}.exe")
        };
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&exe).is_file()))
    }

    /// 工具刚自动装好后把它写进「启动命令」列表：点完「⬇ 安装」就能直接用它
    /// 启动页签，不必再去设置页手敲一遍路径。幂等 + 自愈：已有等价的**可用**
    /// 命令（`nvim` / `D:\x\nvim.exe` 同义）就不动；已有等价但**跑不起来**的
    /// 命令（死路径，或本机没装的裸名 `pi`）就地换成刚装好的绝对路径，不在列表
    /// 里堆死条目；一条都没有则追加。
    /// 不动 `tui_command`（当前选中的启动命令）——那是用户的默认选择，不替他改。
    /// 返回给状态栏的一句话；Err = 落盘失败。
    fn auto_configure_installed_tool(&mut self, exe: &str) -> Result<String, String> {
        let cmd = exe.trim();
        if cmd.is_empty() {
            return Err("安装路径为空".to_string());
        }
        let key = config::tui_command_key(cmd);
        let mut list = self.settings_commands.clone();
        let same = list.iter().position(|c| config::tui_command_key(c) == key);
        let how = match same {
            Some(i) if Self::tui_command_usable(&list[i]) => "启动命令里已有",
            Some(i) if Self::replaceable_command_idx(&list, &key) == Some(i) => {
                list[i] = cmd.to_string();
                "已替换失效的旧命令"
            }
            _ => {
                list.push(cmd.to_string());
                "已加入启动命令"
            }
        };
        self.settings_commands = list.clone();
        self.config.settings.tui_commands = list;
        config::save(&self.config).map_err(|e| e.to_string())?;
        self.config_save_failed = false;
        self.last_config_save = std::time::Instant::now();
        Ok(how.to_string())
    }

    /// 设置页里的「工具更新路径」区：配置「检查更新」到哪里找 pi / opencode。
    /// 默认项（本软件同级目录下的 pi / opencode）启动时由 `current_exe` 自动
    /// 写进配置，跨机器各自一份、**不硬编码**；用户可追加/删除别的位置
    /// （exe 完整路径或目录均可），并决定是否再扫 PATH。改动自动保存。
    ///
    /// 这是折叠面板 2/3 的正文：标题（位置数 / 扫不扫 PATH）在
    /// `tool_dirs_section_ui` 里，这里只管内容。
    fn tool_dirs_ui(&mut self, ui: &mut egui::Ui) {
        let default_dir = software_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "(未知，取不到本软件目录)".to_string());
        ui.label(
            RichText::new(format!(
                "默认查本软件所在目录（{default_dir}）：同级目录里有 pi / opencode 就出现下载按钮，点一下直接装到那里；再查下面配的路径（exe 完整路径或目录均可，目录里也试 pi\\pi.exe），最后{}. 不存在的项不显示按钮。",
                if self.config.settings.tool_search_path {
                    "扫 PATH"
                } else {
                    "不扫 PATH"
                }
            ))
            .weak()
            .small(),
        );
        let mut dirty = false;
        let mut remove_at: Option<usize> = None;
        for i in 0..self.settings_tool_dirs.len() {
            let d = self.settings_tool_dirs[i].clone();
            let exists = PathBuf::from(&d).exists();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!(
                    "{} {d}",
                    if exists { "✅" } else { "⬜" }
                )));
                if ui.small_button("移除").on_hover_text("从查找路径里移除").clicked() {
                    remove_at = Some(i);
                }
                if ui
                    .small_button("打开")
                    .on_hover_text("在资源管理器中打开（不存在则打开所在目录）")
                    .clicked()
                {
                    let p = PathBuf::from(&d);
                    let target = if p.is_dir() {
                        p
                    } else {
                        p.parent().map(|x| x.to_path_buf()).unwrap_or(p)
                    };
                    self.open_explorer(target);
                }
            });
        }
        if let Some(i) = remove_at {
            self.settings_tool_dirs.remove(i);
            dirty = true;
        }
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.settings_new_tool_dir)
                    .desired_width(260.0)
                    .hint_text("路径，如 D:\\Agent\\pi 或 D:\\Agent\\pi.exe"),
            );
            if ui
                .button("浏览目录…")
                .on_hover_text("选择 pi / opencode 所在目录")
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("选择 pi / opencode 所在目录")
                    .pick_folder()
            {
                self.settings_new_tool_dir = path.to_string_lossy().to_string();
            }
            if ui
                .button("选 exe…")
                .on_hover_text("直接选 pi.exe / opencode.exe")
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("选择 pi / opencode 可执行文件")
                    .add_filter("可执行文件", &["exe"])
                    .pick_file()
            {
                self.settings_new_tool_dir = path.to_string_lossy().to_string();
            }
            let clicked = ui.button("添加").clicked();
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if clicked || enter {
                let d = self.settings_new_tool_dir.trim().to_string();
                if !d.is_empty() && !self.settings_tool_dirs.contains(&d) {
                    self.settings_tool_dirs.push(d);
                    self.settings_new_tool_dir.clear();
                    dirty = true;
                }
            }
        });
        if ui
            .button("↺ 补齐同级目录默认项")
            .on_hover_text("把本软件同级目录下的 pi / opencode 路径写回配置")
            .clicked()
        {
            let mut cfg = self.config.clone();
            if fill_default_tool_paths(&mut cfg) {
                self.config.settings.tool_paths = cfg.settings.tool_paths;
                self.settings_tool_dirs = self.config.settings.tool_paths.clone();
                dirty = true;
            }
        }
        let mut use_path = self.config.settings.tool_search_path;
        if ui
            .checkbox(&mut use_path, "找不到时再扫 PATH")
            .on_hover_text("关闭后只看本软件目录 + 上面配的目录")
            .changed()
        {
            dirty = true;
        }
        if dirty {
            self.config.settings.tool_paths = self.settings_tool_dirs.clone();
            self.config.settings.tool_search_path = use_path;
            self.save_config("设置已自动保存".to_string());
        }
    }

    /// provider/models 编辑表单（pi / oh-my-pi 共用），返回是否有改动。
    fn provider_list_ui(
        ui: &mut egui::Ui,
        tab: usize,
        name_edit: &mut Option<(usize, String, String)>,
        num_edit: &mut Option<(usize, String, usize, String, String)>,
        models: &mut config::ModelsConfig,
    ) -> bool {
        let mut dirty = false;
        // 名称编辑缓冲只对当前页签、仍存在的键有效；键被删掉后丢弃。
        // （切到别的页签不清空：那只代表该页签本轮没渲染，缓冲仍在，由切页签处 flush。）
        if let Some((t, k, _)) = name_edit.as_ref() {
            if *t == tab && !models.providers.contains_key(k) {
                *name_edit = None;
            }
        }
        // 数字编辑缓冲同理：供应商/模型行被删除后丢弃，切页签不清。
        if let Some((t, k, i, _, _)) = num_edit.as_ref() {
            let stale = *t != tab
                || models.providers.get(k).map_or(true, |p| p.models.get(*i).is_none());
            if stale {
                *num_edit = None;
            }
        }
        let keys: Vec<String> = models.providers.keys().cloned().collect();
        let mut provider_remove: Option<String> = None;
        let mut provider_rename: Option<(String, String)> = None;
        for key in &keys {
            if let Some(provider) = models.providers.get_mut(key) {
                // 正在编辑该供应商名：沿用缓冲，避免每敲一个字就把行重建导致丢焦点；
                // 否则回填当前键。
                let editing = match name_edit.as_ref() {
                    Some((t, k, _)) => *t == tab && *k == *key,
                    None => false,
                };
                let mut key_name = if editing {
                    name_edit.as_ref().unwrap().2.clone()
                } else {
                    key.clone()
                };
                let mut commit_name = false;
                ui.indent(key, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("供应商:");
                        let resp = ui
                            .add(egui::TextEdit::singleline(&mut key_name).desired_width(120.0));
                        if resp.changed() {
                            // 只更新缓冲不重命名：重命名延迟到失焦才提交（避免每次输入就失焦/落盘）。
                            *name_edit = Some((tab, key.clone(), key_name.clone()));
                        }
                        if resp.lost_focus() {
                            commit_name = true;
                        }
                        if ui.small_button("删除供应商").clicked() {
                            provider_remove = Some(key.clone());
                        }
                    });
                    if commit_name {
                        let new = key_name.trim().to_string();
                        if new.is_empty() || new == *key {
                            // 空名/未变：保留缓冲供继续修改，不落盘。
                            *name_edit = Some((tab, key.clone(), key_name.clone()));
                        } else {
                            provider_rename = Some((key.clone(), new));
                            *name_edit = None;
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.label("baseUrl:");
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut provider.base_url)
                                    .desired_width(320.0),
                            )
                            .changed()
                        {
                            dirty = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("apiKey:");
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut provider.api_key)
                                    .desired_width(320.0),
                            )
                            .changed()
                        {
                            dirty = true;
                        }
                    });
                    // models 列表
                    let mut model_remove: Option<usize> = None;
                    for (mi, model) in provider.models.iter_mut().enumerate() {
                        // context/max 数字编辑缓冲：正在编辑本行则沿用缓冲，失焦才写回，
                        // 避免清空后重打把旧值拼接出新数值。
                        let editing_num = matches!(
                            num_edit.as_ref(),
                            Some((t, k, i, _, _)) if *t == tab && *k == *key && *i == mi
                        );
                        let mut ctx_buf = if editing_num {
                            num_edit.as_ref().unwrap().3.clone()
                        } else {
                            model.context_window.to_string()
                        };
                        let mut max_buf = if editing_num {
                            num_edit.as_ref().unwrap().4.clone()
                        } else {
                            model.max_tokens.to_string()
                        };
                        let mut commit_ctx = false;
                        let mut commit_max = false;
                        ui.horizontal(|ui| {
                            ui.label(format!("模型[{}]:", mi));
                            ui.label("id:");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut model.id).desired_width(60.0),
                                )
                                .changed()
                            {
                                dirty = true;
                            }
                            ui.label("name:");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut model.name)
                                        .desired_width(100.0),
                                )
                                .changed()
                            {
                                dirty = true;
                            }
                            ui.label("context:");
                            let resp_ctx = ui.add(
                                egui::TextEdit::singleline(&mut ctx_buf).desired_width(80.0),
                            );
                            if resp_ctx.changed() {
                                *num_edit = Some((
                                    tab,
                                    key.clone(),
                                    mi,
                                    ctx_buf.clone(),
                                    max_buf.clone(),
                                ));
                            }
                            if resp_ctx.lost_focus() {
                                commit_ctx = true;
                            }
                            ui.label("max:");
                            let resp_max = ui.add(
                                egui::TextEdit::singleline(&mut max_buf).desired_width(80.0),
                            );
                            if resp_max.changed() {
                                *num_edit = Some((
                                    tab,
                                    key.clone(),
                                    mi,
                                    ctx_buf.clone(),
                                    max_buf.clone(),
                                ));
                            }
                            if resp_max.lost_focus() {
                                commit_max = true;
                            }
                            if ui.small_button("删除模型").clicked() {
                                model_remove = Some(mi);
                            }
                        });
                        // 失焦提交：解析成功才写回；失败则丢缓冲、字段回显原值。
                        if commit_ctx {
                            if let Ok(v) = ctx_buf.trim().parse() {
                                model.context_window = v;
                                dirty = true;
                            }
                            if editing_num {
                                *num_edit = None;
                            }
                        }
                        if commit_max {
                            if let Ok(v) = max_buf.trim().parse() {
                                model.max_tokens = v;
                                dirty = true;
                            }
                            if editing_num {
                                *num_edit = None;
                            }
                        }
                    }
                    if let Some(mi) = model_remove {
                        provider.models.remove(mi);
                        dirty = true;
                    }
                    if ui.button("+ 添加模型").clicked() {
                        provider.models.push(config::ModelEntry::default());
                        dirty = true;
                    }
                });
            }
        }
        if let Some(key) = provider_remove {
            models.providers.remove(&key);
            dirty = true;
        }
        if let Some((old, new)) = provider_rename {
            if !new.is_empty()
                && new != old
                && !models.providers.contains_key(&new)
                && let Some(entry) = models.providers.remove(&old)
            {
                models.providers.insert(new, entry);
                dirty = true;
            } else {
                // 重名冲突等：恢复编辑缓冲，保留用户输入供修改。
                *name_edit = Some((tab, old, new));
            }
        }
        if ui.button("+ 添加供应商").clicked() {
            let mut n = 1;
            while models.providers.contains_key(&n.to_string()) {
                n += 1;
            }
            models.providers.insert(n.to_string(), config::ProviderEntry::default());
            dirty = true;
        }
        dirty
    }

    /// 提交某个页签里未落盘的供应商改名（切页签时调用；失焦提交走 provider_list_ui）。
    /// 改名无效/重名冲突时保留编辑缓冲供用户修正。
    fn flush_provider_rename(&mut self, tab: usize) {
        let Some((t, old, new)) = self.provider_name_edit.take() else {
            return;
        };
        if t != tab {
            self.provider_name_edit = Some((t, old, new));
            return;
        }
        let new = new.trim().to_string();
        let ok = if t == 0 {
            if new.is_empty() || new == old || self.pi_models.providers.contains_key(&new) {
                false
            } else if let Some(entry) = self.pi_models.providers.remove(&old) {
                self.pi_models.providers.insert(new.clone(), entry);
                config::save_pi_models(&self.pi_models).is_ok()
            } else {
                false
            }
        } else {
            if new.is_empty() || new == old || self.omp_models.providers.contains_key(&new) {
                false
            } else if let Some(entry) = self.omp_models.providers.remove(&old) {
                self.omp_models.providers.insert(new.clone(), entry);
                config::save_omp_models(&self.omp_models).is_ok()
            } else {
                false
            }
        };
        if !ok {
            // 改名无效/重名冲突/删除失败：保留编辑缓冲供用户修正。
            self.provider_name_edit = Some((t, old, new));
        }
    }

    /// 切页签时提交未失焦的模型 context/max 数字编辑（与改名同一漏帧问题）。
    fn flush_model_num_edit(&mut self, tab: usize) {
        let Some((t, key, idx, ctx, max)) = self.model_num_edit.take() else {
            return;
        };
        if t != tab {
            self.model_num_edit = Some((t, key, idx, ctx, max));
            return;
        }
        let provider = if t == 0 {
            self.pi_models.providers.get_mut(&key)
        } else {
            self.omp_models.providers.get_mut(&key)
        };
        let Some(provider) = provider else {
            return; // 供应商已删除：丢弃缓冲
        };
        let Some(model) = provider.models.get_mut(idx) else {
            return; // 模型行已删：丢弃缓冲
        };
        let mut applied = false;
        if let Ok(v) = ctx.trim().parse::<u64>() {
            model.context_window = v;
            applied = true;
        }
        if let Ok(v) = max.trim().parse::<u64>() {
            model.max_tokens = v;
            applied = true;
        }
        if applied {
            let res = if t == 0 {
                config::save_pi_models(&self.pi_models)
            } else {
                config::save_omp_models(&self.omp_models)
            };
            if let Err(e) = res {
                self.status = Some(format!("供应商配置保存失败: {e}"));
            }
        }
    }

    /// 模型配置页签内容：0=pi，1=oh-my-pi，2=opencode。
    fn model_settings_ui(&mut self, ui: &mut egui::Ui, tab: usize) {
        match tab {
            0 => {
                ui.label(RichText::new("pi 供应商配置").strong());
                ui.label(
                    RichText::new(format!("路径: {}", config::pi_models_path().display()))
                        .weak()
                        .small(),
                );
                if Self::provider_list_ui(
                    ui,
                    0,
                    &mut self.provider_name_edit,
                    &mut self.model_num_edit,
                    &mut self.pi_models,
                ) && let Err(e) = config::save_pi_models(&self.pi_models)
                {
                    self.status = Some(format!("pi 配置保存失败: {e}"));
                }
            }
            1 => {
                ui.label(RichText::new("oh-my-pi 供应商配置").strong());
                ui.label(
                    RichText::new(format!("路径: {}", config::omp_models_path().display()))
                        .weak()
                        .small(),
                );
                if Self::provider_list_ui(
                    ui,
                    1,
                    &mut self.provider_name_edit,
                    &mut self.model_num_edit,
                    &mut self.omp_models,
                ) && let Err(e) = config::save_omp_models(&self.omp_models)
                {
                    self.status = Some(format!("oh-my-pi 配置保存失败: {e}"));
                }
            }
            _ => self.opencode_settings_ui(ui),
        }
    }

    /// opencode 供应商配置页签（页签 2）。与 pi/omp 的 schema 不同：provider 是
    /// `npm` + `options.baseURL` + `models.<id>` map，故单独一套表单。
    /// 写回只 patch provider 子树（见 config::save_opencode_providers）。
    fn opencode_settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("opencode 供应商配置").strong());
        ui.label(
            RichText::new(format!("路径: {}", config::opencode_config_path().display()))
                .weak()
                .small(),
        );
        if let Some(err) = self.opencode_load_err.clone() {
            // 解析不了（jsonc 注释等）就不给编辑入口：宁可让人手改，也不能
            // 把注释和本程序不认识的字段洗掉。
            ui.colored_label(
                egui::Color32::from_rgb(230, 160, 60),
                format!("读取失败，已停用编辑（不会改动原文件）: {err}"),
            );
            return;
        }
        if !self.opencode_default_model.is_empty() {
            ui.label(
                RichText::new(format!(
                    "当前默认模型: {}（本程序不改这一项）",
                    self.opencode_default_model
                ))
                .weak()
                .small(),
            );
        }
        let mut dirty = false;
        let n = self.opencode_models.providers.len();
        for i in 0..n {
            ui.indent(format!("provider[{i}]"), |ui| {
                ui.horizontal(|ui| {
                    ui.label("供应商 id:");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.opencode_models.providers[i].id)
                                .desired_width(120.0),
                        )
                        .changed()
                    {
                        dirty = true;
                    }
                    ui.label("name:");
                    if ui
                        .add(
                            egui::TextEdit::singleline(
                                &mut self.opencode_models.providers[i].name,
                            )
                            .desired_width(120.0),
                        )
                        .changed()
                    {
                        dirty = true;
                    }
                    if ui.small_button("删除供应商").clicked() {
                        self.opencode_models.providers.remove(i);
                        dirty = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("npm:");
                    if ui
                        .add(
                            egui::TextEdit::singleline(
                                &mut self.opencode_models.providers[i].npm,
                            )
                            .desired_width(260.0)
                            .hint_text("@ai-sdk/openai-compatible"),
                        )
                        .changed()
                    {
                        dirty = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("baseURL:");
                    if ui
                        .add(
                            egui::TextEdit::singleline(
                                &mut self.opencode_models.providers[i].base_url,
                            )
                            .desired_width(320.0),
                        )
                        .changed()
                    {
                        dirty = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("apiKey:");
                    if ui
                        .add(
                            egui::TextEdit::singleline(
                                &mut self.opencode_models.providers[i].api_key,
                            )
                            .desired_width(320.0),
                        )
                        .changed()
                    {
                        dirty = true;
                    }
                });
                let n_models = self.opencode_models.providers[i].models.len();
                for mi in 0..n_models {
                    ui.horizontal(|ui| {
                        ui.label("模型 id:");
                        if ui
                            .add(
                                egui::TextEdit::singleline(
                                    &mut self.opencode_models.providers[i].models[mi].id,
                                )
                                .desired_width(120.0),
                            )
                            .changed()
                        {
                            dirty = true;
                        }
                        ui.label("name:");
                        if ui
                            .add(
                                egui::TextEdit::singleline(
                                    &mut self.opencode_models.providers[i].models[mi].name,
                                )
                                .desired_width(160.0),
                            )
                            .changed()
                        {
                            dirty = true;
                        }
                        if ui.small_button("删除模型").clicked() {
                            self.opencode_models.providers[i].models.remove(mi);
                            dirty = true;
                        }
                    });
                }
                if ui.button("+ 添加模型").clicked() {
                    self.opencode_models.providers[i].models.push(config::OcModel {
                        id: String::new(),
                        name: String::new(),
                    });
                    dirty = true;
                }
            });
        }
        if ui.button("+ 添加供应商").clicked() {
            let mut p = config::OcProvider::default();
            // 新供应商 id 自动取未占用的数字名（同 pi 侧习惯）。
            let mut n = 1;
            while self
                .opencode_models
                .providers
                .iter()
                .any(|q| q.id == n.to_string())
            {
                n += 1;
            }
            p.id = n.to_string();
            p.name = n.to_string();
            self.opencode_models.providers.push(p);
            dirty = true;
        }
        // 写回前先查两处容易踩的坑：id 重复（同键会互相覆盖）、id 为空（写不进去）。
        let ids: Vec<&str> = self
            .opencode_models
            .providers
            .iter()
            .map(|p| p.id.trim())
            .collect();
        let uniq = {
            let mut u = ids.clone();
            u.sort_unstable();
            u.dedup();
            u.len()
        };
        let empty = ids.iter().any(|s| s.is_empty());
        if ids.len() != uniq || empty {
            ui.colored_label(
                egui::Color32::from_rgb(230, 160, 60),
                if empty {
                    "存在空 id 的供应商（不会写入文件）"
                } else {
                    "存在重复的供应商 id（后写的会覆盖前一个）"
                },
            );
        }
        if dirty {
            if let Err(e) = config::save_opencode_providers(&self.opencode_models) {
                self.status = Some(format!("opencode 配置保存失败: {e}"));
            } else {
                self.opencode_default_model = config::opencode_default_model();
            }
        }
    }

    fn input_dialog(&mut self, ui: &mut egui::Ui) {
        let mut dialog = match self.input.take() {
            Some(d) => d,
            None => return,
        };
        let title = dialog.title();
        let is_rename = matches!(dialog, InputDialog::Rename { .. });
        let is_edit_path = matches!(dialog, InputDialog::EditPath { .. });
        let mut commit = false;
        let mut cancel = false;

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                match &mut dialog {
                    InputDialog::AddProject { name, path } => {
                        ui.label(
                            RichText::new("项目名称（留空则使用路径最后一段作为名称）")
                                .weak()
                                .small(),
                        );
                        let name_resp =
                            ui.add(egui::TextEdit::singleline(name).desired_width(340.0));
                        ui.add_space(4.0);
                        ui.label(RichText::new("项目路径").weak().small());
                        ui.horizontal(|ui| {
                            let path_resp = ui.add(
                                egui::TextEdit::singleline(path)
                                    .desired_width(340.0)
                                    .hint_text("选择或输入文件夹路径"),
                            );
                            let browse = ui.button("浏览…").clicked();
                            if browse
                                && let Some(dir) = rfd::FileDialog::new()
                                    .set_title("选择项目文件夹")
                                    .pick_folder()
                            {
                                *path = dir.to_string_lossy().to_string();
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && (name_resp.has_focus() || path_resp.has_focus())
                            {
                                commit = true;
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Escape))
                                && (name_resp.has_focus() || path_resp.has_focus())
                            {
                                cancel = true;
                            }
                        });
                    }
                    InputDialog::Rename { value } | InputDialog::EditPath { value } => {
                        let hint = if is_rename { "新名称" } else { "新路径" };
                        ui.label(RichText::new(hint).weak().small());
                        ui.horizontal(|ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(value)
                                    .desired_width(340.0)
                                    .hint_text(hint),
                            );
                            if !resp.has_focus() {
                                resp.request_focus();
                            }
                            if is_edit_path
                                && ui.button("浏览…").clicked()
                                && let Some(dir) = rfd::FileDialog::new()
                                    .set_title("选择项目文件夹")
                                    .pick_folder()
                            {
                                *value = dir.to_string_lossy().to_string();
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) && resp.has_focus() {
                                commit = true;
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Escape)) && resp.has_focus() {
                                cancel = true;
                            }
                        });
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("确定").clicked() {
                        commit = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            });

        if commit {
            self.commit_input(dialog);
        } else if cancel {
            self.input = None;
        } else {
            self.input = Some(dialog);
        }
    }

    fn confirm_dialog(&mut self, ui: &mut egui::Ui) {
        let dialog = match self.confirm.take() {
            Some(d) => d,
            None => return,
        };
        let confirm_label;
        let (message, _index) = match &dialog {
            ConfirmDialog::DeleteProject { index, name } => {
                confirm_label = "确定";
                (format!("确定删除项目「{name}」吗？"), *index)
            }
            ConfirmDialog::RelaunchSession { title, reason, .. } => {
                confirm_label = "重新打开";
                (
                    format!("终端页签「{title}」已崩溃（{reason}），已隔离并关闭。要重新打开它吗？"),
                    0,
                )
            }
        };
        let mut yes = false;
        let mut no = false;
        egui::Window::new("确认")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label(message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(confirm_label).clicked() {
                        yes = true;
                    }
                    if ui.button("取消").clicked() {
                        no = true;
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    yes = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    no = true;
                }
            });
        if yes {
            match dialog {
                ConfirmDialog::DeleteProject { index, .. } => self.confirm_delete(index),
                ConfirmDialog::RelaunchSession { dir, title, .. } => {
                    self.relaunch_session(dir, title)
                }
            }
        } else if !no {
            self.confirm = Some(dialog);
        }
    }
}

impl Drop for ClientApp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl eframe::App for ClientApp {
    /// egui 消费输入之前的钩子：在这里统计滚轮档位。
    ///
    /// 终端滚动必须按「档位」而不是 egui 的 `smooth_scroll_delta` 走：
    /// 后者把一次拨轮摊到 ~50 帧渐进下发（WheelState::after_events），
    /// 按每帧位移换算行数会把一次拨轮放大成几十行（滚一下翻过好几屏）。
    /// 原始事件里档位是干净的：winit 给鼠标滚轮的 Line 单位一格正好 1.0。
    ///
    /// 每帧重置：本字段是「本帧输入」而非队列，没被终端消费的部分直接丢弃
    /// （滚轮不排队——指针在页签栏/设置页上滚就不该留到终端上）。
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.wheel = terminal::collect_wheel(&raw_input.events);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 清理启动时创建的 .running 标记文件。
        if let Ok(exe) = std::env::current_exe()
            && let Some(name) = exe.file_name().and_then(|n| n.to_str())
        {
            let _ = std::fs::remove_file(exe.with_file_name(format!("{name}.running")));
        }
        // 记录打开中的终端页签（退出后下次启动自动重新拉起）。
        // 规则与运行期随时落盘共用 current_tabs_state()，行为保持一致。
        self.config.tabs = self.current_tabs_state();
        // 窗口状态已在每帧 logic 中记录；运行期已随时落盘，这里退出时保底一次。
        let _ = config::save(&self.config);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 标题栏固定黑色：启动时设一次 + 主题切换时补设（见下），不每帧轮询
        // （DwmSetWindowAttribute 触发 DWM 重算非客户区，重置 winit 的 hover 跟踪
        // → 「鼠标悬停激活窗口」失效）。补设走延迟时刻：egui 深浅切换会在帧尾经
        // ViewportCommand::SetTheme 让 winit SetWindowTheme("") 复位标题栏为浅色，
        // 立即补设压不住，延迟一拍再钉回黑。
        // 跟随系统：系统深浅变化时重应用主题并广播到所有会话。
        let cur_dark = self.effective_dark();
        if cur_dark != self.last_theme_dark {
            self.last_theme_dark = cur_dark;
            apply_theme(ctx, cur_dark);
            self.broadcast_theme();
            self.theme_settle_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
            ctx.request_repaint();
            set_dwm_dark(self.titlebar_hwnd);
            self.titlebar_restore_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
        }
        // 延迟补设黑色标题栏：等 winit 应用完帧尾的 SetTheme 命令后再压一次。
        if let Some(t) = self.titlebar_restore_at
            && std::time::Instant::now() >= t
        {
            self.titlebar_restore_at = None;
            set_dwm_dark(self.titlebar_hwnd);
        }

        // ── Ctrl+(Shift+)Tab 循环切换页签 ──
        // 在 logic() 用 consume_key 消费：事件从流里移除，ui() 里终端输入循环
        // 收不到 → 不会当作 \t/\x1b[Z 转发给子进程。egui matches_logically 忽略
        // 多余的 Shift/Alt，必须先判 Ctrl+Shift+Tab（后退）再判 Ctrl+Tab
        // （前进），否则 Ctrl+Shift+Tab 会被前进分支吞掉（见 egui 0.36
        // input_state::consume_key 文档）。
        let (tab_back, tab_fwd) = ctx.input_mut(|i| {
            let back = i.consume_key(
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                egui::Key::Tab,
            );
            let fwd = !back && i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab);
            (back, fwd)
        });
        if tab_back || tab_fwd {
            if let Some(target) = tab_cycle_target(&self.tabs, self.current, tab_fwd) {
                self.current = target;
                self.refresh_focus();
                // 切入口即视为已查看（✅/`任务完成`通知清零，与点击页签一致；
                // 前台每帧同步与 update_done_states 随本帧随后生效）。
                if let Some(Tab::Session(s)) = self.tabs.get(self.current) {
                    s.has_been_viewed.store(true, Ordering::Relaxed);
                }
            }
        }

        // 前台标记每帧同步（覆盖所有切换路径：点击/Ctrl+Tab/关闭/拖拽/恢复）。
        // 当前页签同时置「已查看」→ ✅ 图标只属于后台页签，一处覆盖所有切换
        // 路径。done_notified 的复位/武装在 update_done_states 的 ✅ 分支。
        for (i, t) in self.tabs.iter().enumerate() {
            if let Tab::Session(s) = t {
                if i == self.current {
                    s.has_been_viewed.store(true, Ordering::Relaxed);
                    s.foreground.store(true, Ordering::Relaxed);
                } else {
                    s.foreground.store(false, Ordering::Relaxed);
                }
            }
        }

        // ✅ 稳定计时 / 「执行完成」通知状态机（原在 tab_bar 渲染路径）：
        // 与退出判定同在 logic() 跑，UI 只读图标。
        self.update_done_states(ctx);

        // 主题切换后的延迟全量重绘：到点后把所有会话缓存再清一遍并强制整帧。
        if let Some(t) = self.theme_settle_at {
            if std::time::Instant::now() >= t {
                self.theme_settle_at = None;
                for tab in &mut self.tabs {
                    if let Tab::Session(s) = tab {
                        s.caret_scan = None;
                        s.cached_render_shapes = None;
                        s.cached_ansi_rgb = None;
                    }
                }
                ctx.request_repaint();
            } else {
                ctx.request_repaint_after(
                    t.saturating_duration_since(std::time::Instant::now()),
                );
            }
        }

        // ── 帧率调度（cmd/conhost 式：有脏区才持续刷新，静止降为慢心跳）──
        // 有活干（输出在途/加载/交互/下载/后台 spawn）→ 按配置帧率；
        // 全部静止 → IDLE_HEARTBEAT_MS 慢心跳兜底，省 ~95% 空闲重绘。
        // 输出唤醒不再由 reader 线程直接 request_repaint（持续输出会把整窗
        // 帧率顶到 CPU 全速，干扰 winit hover 跟踪 → 悬停激活失效）：
        // SessionListener 只发合并信号（容量 1），此处消费后按配置帧率唤醒。
        // 后台页签不消费信号（画面不可见，切回那帧自然重绘）。
        let mut busy = self.downloading
            || !self.spawning.is_empty()
            || self.theme_settle_at.is_some()
            || ctx.input(|i| i.pointer.any_down());
        if let Some(Tab::Session(s)) = self.tabs.get(self.current) {
            // 仅前台页签消费合并信号：解析线程有新输出待画时按配置帧率刷新。
            if s.redraw_rx.try_recv().is_ok() {
                busy = true;
            }
            if !busy {
                let now_ms = crate::now_ms();
                // 仅前台页签的近期输出/加载态拉高整窗帧率；后台页签输出只更新
                // 图标（IDLE_HEARTBEAT_MS=500ms 心跳轮询），不再连带全窗刷新
                // （连带刷=悬停激活干扰源）。
                if now_ms.saturating_sub(s.last_output_ms.load(Ordering::Relaxed)) < 300
                    || s.loading_active(now_ms)
                {
                    busy = true;
                }
            }
        }
        let delay_ms = if busy {
            // 固定 10 FPS（100ms）。ponytail: 曾开放 30/60 FPS 档，但高帧率持续
            // 重绘实测干扰 Windows 悬停激活窗口（10 FPS 正常、30 失效）；如需
            // 更高帧率先根治该干扰源，再放开这里。
            BUSY_FRAME_MS
        } else {
            IDLE_HEARTBEAT_MS
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(delay_ms));
        self.bg_frame = self.bg_frame.wrapping_add(1);

        // 更新检查结果回到状态栏。
        if let Ok((msg, latest)) = self.update_rx.try_recv() {
            // 完成消息只在下载线程成功替换 exe 后发出：置标记，状态栏改显
            // 「重启应用」按钮。失败消息（“下载失败…重试”）不含该前缀。
            if msg.contains("下载完成") {
                self.update_done = true;
            }
            // 取消/失败时立即重置 downloading 状态，UI 即时恢复。
            // **中间态的「第 n/5 次…自动重试」不能复位**：复位会让「✕ 取消」
            // 按钮消失，下载线程却还在后台重试 → 用户按不到停止（无限循环体感）。
            if msg.contains("已取消") || msg.contains("已停止重试") || msg.contains("已停止自动重试") {
                self.downloading = false;
                self.download_progress_rx = None;
            }
            self.status = Some(msg);
            self.update_latest = latest;
            ctx.request_repaint();
        }
        // 下载进度实时更新状态栏。
        if let Some(rx) = &self.download_progress_rx
            && let Ok((downloaded, total)) = rx.try_recv()
        {
            if total > 0 {
                let pct = downloaded as f64 / total as f64 * 100.0;
                self.status = Some(format!("下载中… {pct:.2}% ({}/{})",
                    Self::format_bytes(downloaded),
                    Self::format_bytes(total)));
            } else {
                self.status = Some(format!("下载中… {}",
                    Self::format_bytes(downloaded)));
            }
            ctx.request_repaint();
        }
        // 下载线程结束后清理通道：仅当发送端已全部掉落（线程退出）才清理，
        // 重试等待期（3 秒休眠无进度消息）不误判为结束。
        if self.downloading
            && self
                .download_progress_rx
                .as_ref()
                .is_some_and(|rx| matches!(rx.try_recv(), Err(TryRecvError::Disconnected)))
        {
            self.downloading = false;
            self.download_progress_rx = None;
            ctx.request_repaint();
        }
        // pi / opencode 的检查/下载事件：排空通道逐条消费（一次 try_recv
        // 只取一条，检查+下载进度混在一个通道里，逐帧多条才不会漏）。
        while let Ok((idx, ev)) = self.tool_rx.try_recv() {
            if idx >= self.tools.len() {
                continue;
            }
            match ev {
                ToolEvent::Checked { local, latest, install_dir, missing } => {
                    // 本地版本认不出时以「我们刚装上的 tag」兜底。
                    let local = local_version_with_floor(
                        &local,
                        self.tools[idx].installed_tag.as_deref(),
                    );
                    self.tools[idx].local = local.clone();
                    self.tools[idx].install_dir = install_dir;
                    self.tools[idx].missing = missing;
                    // 检查结果以本次为准：正在下载/重试中不抢按钮（点了会
                    // 重复起一个线程），只更新版本号与安装目录。
                    if self.tools[idx].downloading {
                        continue;
                    }
                    if missing {
                        // 未检测到：不抢已有状态栏文案（检查是静默触发的，不扰民），
                        // 但工具状态要收干净——由 missing 标记在状态栏持续给出
                        // 「⬇ 安装」按钮（版本号在点下去时现查）。
                        self.tools[idx].latest = None;
                        self.tools[idx].downloading = false;
                        let dir = self.tools[idx].install_dir.to_string_lossy().into_owned();
                        log_update(&format!(
                            "检查更新 {} 未安装，下载入口装到 {dir}",
                            TOOL_SPECS[idx].label
                        ));
                        if self.status.as_deref().is_none_or(|s| s.is_empty()) {
                            // 点下去才发现按钮不在（多条时收进了「⬇ 工具 N」菜单）。
                            self.status = Some(if dir.is_empty() {
                                format!("未检测到 {}，请手动安装", TOOL_SPECS[idx].label)
                            } else {
                                format!(
                                    "未检测到 {label}，点右侧按钮装到本软件目录",
                                    label = TOOL_SPECS[idx].label
                                )
                            });
                        }
                    } else {
                        self.tools[idx].latest = latest.clone();
                        // 找到新版本才占用状态栏（已是最新时保持现状，不扰民）。
                        if let Some(tag) = latest {
                            self.status = Some(format!(
                                "发现 {} 新版本 {tag}（当前 {}），点击状态栏按钮下载",
                                TOOL_SPECS[idx].label,
                                if local.is_empty() { "未知".to_string() } else { local }
                            ));
                        }
                    }
                }
                ToolEvent::Status(msg) => {
                    self.status = Some(msg);
                }
                ToolEvent::Progress(downloaded, total) => {
                    let label = TOOL_SPECS[idx].label;
                    self.status = Some(if total > 0 {
                        format!(
                            "下载 {} 中… {:.2}% ({}/{})",
                            label,
                            downloaded as f64 / total as f64 * 100.0,
                            Self::format_bytes(downloaded),
                            Self::format_bytes(total)
                        )
                    } else {
                        format!("下载 {} 中… {}", label, Self::format_bytes(downloaded))
                    });
                }
                ToolEvent::Done { msg, version } => {
                    // 首次安装（was_missing）顺手把新 exe 写进「启动命令」列表：
                    // 用户点完「⬇ 安装」就该能直接用它启动页签，不必再去设置页手加。
                    let was_missing = self.tools[idx].missing;
                    // 版本号直接采信刚装上的 tag（见 run_tool_update 的说明）。
                    self.tools[idx].local = version.clone();
                    self.tools[idx].installed_tag = Some(version);
                    self.tools[idx].latest = None;
                    self.tools[idx].pending_tag = None;
                    // 装完 exe 已在安装目录里，不再是「未安装」：状态栏那个
                    // 「⬇ 安装」按钮自行退场。
                    self.tools[idx].missing = false;
                    let mut msg = msg;
                    if was_missing {
                        let exe = self.tools[idx]
                            .install_dir
                            .join(TOOL_SPECS[idx].exe_name)
                            .to_string_lossy()
                            .into_owned();
                        match self.auto_configure_installed_tool(&exe) {
                            Ok(how) => msg = format!("{msg}，{how}"),
                            Err(e) => msg = format!("{msg}，但写入启动命令失败: {e}"),
                        }
                    }
                    self.status = Some(msg);
                    // 另一个工具（如 pi）的状态重新对一下盘：装完 opencode 后它
                    // 到底还是“未安装”还是已经被人装好了，磁盘说了算。
                    self.refresh_tool_presence();
                }
                ToolEvent::Finished => {
                    self.tools[idx].downloading = false;
                    // 非成功结束（取消/被占用/重试到顶）：把下载按钮重新点亮，
                    // 用户可直接再点一次重试。Done 已在上面清了 pending_tag，
                    // 所以不会在这里把刚装好的版本又标成“有新版本”。未安装
                    // （missing）时按钮本就常在，pending_tag 为空也不影响。
                    if self.tools[idx].pending_tag.is_some() {
                        self.tools[idx].latest = self.tools[idx].pending_tag.clone();
                    }
                    self.tools[idx].pending_tag = None;
                }
            }
            ctx.request_repaint();
        }
        let exited = self.update_exited(ctx);
        if exited {
            self.status = Some("有会话已退出".to_string());
            ctx.request_repaint();
        }

        // 记录窗口状态（每帧刷新内存态，运行期随时落盘，退出时再保底一次）。
        // 只读需要的三个字段，不整份 clone ViewportInfo（内含多个 String 字段，每帧一次）。
        let prev_window = self.config.window.clone();
        ctx.input(|i| {
            let vp = i.viewport();
            self.config.window.maximized = vp.maximized.unwrap_or(false);
            if !self.config.window.maximized {
                if let Some(r) = vp.outer_rect {
                    self.config.window.pos = Some([r.min.x, r.min.y]);
                }
                if let Some(r) = vp.inner_rect {
                    self.config.window.size = Some([r.width(), r.height()]);
                }
            }
        });
        let window_changed = prev_window != self.config.window;

        // 配置随时落盘（防闪退丢配置）：
        // - 页签结构变化（打开/关闭/切换/拖拽/会话退出）→ 立即保存；
        // - 仅窗口位置/尺寸变化（拖动/缩放每帧连续变）→ 节流到
        //   CONFIG_SAVE_INTERVAL 写一次，避免拖动窗口时写盘风暴。
        // 恢复/启动中的会话还没长成真实页签前不落盘：此时 current_tabs_state()
        // 会算出空列表，提前写盘会把磁盘上待恢复的页签列表清空。
        let tabs_state = self.current_tabs_state();
        let tabs_changed = tabs_state != self.config.tabs;
        let settled = self.pending_launch.is_empty()
            && self.pending_restore.is_empty()
            && self.pending_relaunch.is_empty()
            && self.spawning.is_empty();
        let window_debounce_ok = self.last_config_save.elapsed() >= CONFIG_SAVE_INTERVAL;
        if settled && (tabs_changed || (window_changed && window_debounce_ok)) {
            self.config.tabs = tabs_state;
            self.persist_config();
        }

        // 窗口标题保持固定，不根据等待输入状态动态修改。
        // 动态标题会频繁调用 send_viewport_cmd，可能干扰 winit 的 hover 跟踪，
        // 导致 Windows「鼠标悬停激活窗口」失效。等待输入的状态由页签空图标表达。
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("tab_bar").show(ui, |ui| self.tab_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            // 待启动会话在此 spawn：中央面板布局已定，ui.available_size() 即终端真实
            // 面积，按它算行列数启动，TUI 首帧即按最终尺寸画整屏页，无需 resize 纠正。
            // spawn 移到后台线程：ConPTY 初始化 + 子进程创建会阻塞 UI 数百毫秒，
            // 导致首帧卡死、点击/键盘事件全部丢失。
            let pending = std::mem::take(&mut self.pending_launch);
            let pending_rel = std::mem::take(&mut self.pending_relaunch);
            let has_new_spawns = !pending.is_empty() || !pending_rel.is_empty();
            if has_new_spawns {
                let geom = terminal::term_grid_size(ui);
                let (cols, rows) = geom.unwrap_or((80, 24));
                let (tx, rx) = std::sync::mpsc::channel();
                self.spawn_rx = Some(rx);
                let history_lines = self.config.settings.history_lines;
                for p in pending {
                    let title = p.title.clone();
                    let ctx = self.ctx.clone();
                    let wake_ctx = ctx.clone();
                    let tx = tx.clone();
                    self.spawning.push((title, false, None));
                    std::thread::spawn(move || {
                        let result = session::spawn(
                            &p.title, &p.dir, &p.cmd,
                            cols as u16, rows as u16,
                            history_lines,
                            ctx,
                        );
                        let _ = tx.send((result, false, None));
                        // spawn 完成立即唤醒 UI 应用页签（恒定帧率下最多省掉
                        // 一帧 ~100ms 的等待；恢复/重启路径行为一致）。
                        wake_ctx.request_repaint();
                    });
                }
                for p in pending_rel {
                    let title = p.title.clone();
                    let tab_idx = p.tab_index;
                    // spawn 完成后唤醒 UI：占位页签不是前台终端，不唤醒的话新会话
                    // 出现要等到下一帧恒定帧（最多 ~100ms），明显延迟。
                    let ctx = self.ctx.clone();
                    let wake_ctx = ctx.clone();
                    let tx = tx.clone();
                    self.spawning.push((title, false, Some(tab_idx)));
                    std::thread::spawn(move || {
                        let result = session::spawn(
                            &p.title, &p.dir, &p.cmd,
                            cols as u16, rows as u16,
                            history_lines,
                            ctx,
                        );
                        let _ = tx.send((result, false, Some(tab_idx)));
                        wake_ctx.request_repaint();
                    });
                }
                self.screen = Screen::Main;
                self.check_updates(true);
            }
            // 轮询后台 spawn 结果：先收进临时列表，再逐条处理，避免 &rx 与 &mut self 借用冲突。
            let mut spawn_results: Vec<SpawnResult> = Vec::new();
            if let Some(rx) = &self.spawn_rx {
                while let Ok(item) = rx.try_recv() {
                    self.spawning.pop();
                    spawn_results.push(item);
                }
            }
            for (result, is_restore, saved_index) in spawn_results {
                if is_restore {
                    let themed = self.apply_theme_to(result);
                    if let Some(idx) = saved_index
                        && let Some(slot) = self.restore_slots.get_mut(idx)
                    {
                        *slot = Some(themed);
                    }
                } else if let Some(relaunch_idx) = saved_index {
                    match self.apply_theme_to(result) {
                        Ok(sess) => {
                            // 优先按标题找到对应占位页签（重启期间用户可能拖拽重排
                            // 导致 relaunch_idx 不再指向它），避免幽灵占位页签永驻。
                            let idx = self
                                .tabs
                                .iter()
                                .position(|t| {
                                    matches!(t, Tab::Placeholder { title } if title == &sess.title)
                                })
                                .unwrap_or_else(|| {
                                    relaunch_idx.min(self.tabs.len().saturating_sub(1))
                                });
                            // 如果目标位置是 Placeholder（重启/切换命令），直接替换；
                            // 否则 insert（崩溃恢复等场景）。
                            if matches!(self.tabs.get(idx), Some(Tab::Placeholder { .. })) {
                                self.tabs[idx] = Tab::Session(Box::new(sess));
                            } else {
                                self.tabs.insert(idx, Tab::Session(Box::new(sess)));
                            }
                            self.current = idx;
                            self.term_focused = true;
                            self.screen = Screen::Main;
                            self.refresh_focus();
                            self.status = Some("已启动".to_string());
                        }
                        Err(e) => {
                            // 启动失败：移除 Placeholder，恢复到之前的状态。
                            if matches!(self.tabs.get(relaunch_idx), Some(Tab::Placeholder { .. })) {
                                self.tabs.remove(relaunch_idx);
                                if self.current >= self.tabs.len() {
                                    self.current = self.tabs.len().saturating_sub(1);
                                }
                            }
                            self.status = Some(format!("启动失败: {e}"));
                            self.refresh_focus();
                        }
                    }
                } else {
                    match self.apply_theme_to(result) {
                        Ok(sess) => {
                            self.tabs.push(Tab::Session(Box::new(sess)));
                            self.current = self.tabs.len() - 1;
                            self.term_focused = true;
                            self.screen = Screen::Main;
                            self.status = Some("已启动".to_string());
                        }
                        Err(e) => {
                            self.status = Some(format!("启动失败: {e}"));
                        }
                    }
                }
            }
            // 启动时恢复上次的会话：后台线程 spawn，避免阻塞首帧。
            let pending = std::mem::take(&mut self.pending_restore);
            if !pending.is_empty() {
                let geom = terminal::term_grid_size(ui);
                let (cols, rows) = geom.unwrap_or((80, 24));
                let (tx, rx) = std::sync::mpsc::channel();
                self.spawn_rx = Some(rx);
                self.restore_slots = (0..pending.len()).map(|_| None).collect();
                for (save_i, p) in pending.into_iter().enumerate() {
                    let title = p.title;
                    let dir = p.dir;
                    let cmd = p.cmd;
                    let ctx = self.ctx.clone();
                    let wake_ctx = ctx.clone();
                    let tx = tx.clone();
                    let history_lines = self.config.settings.history_lines;
                    self.spawning.push((title.clone(), true, Some(save_i)));
                    std::thread::spawn(move || {
                        let result = session::spawn(
                            &title, &dir, &cmd,
                            cols as u16, rows as u16,
                            history_lines,
                            ctx,
                        );
                        // saved_index：恢复时按保存时的原序插入，保证页签顺序不因
                        // 后台 spawn 完成先后而打乱。
                        let _ = tx.send((result, true, Some(save_i)));
                        // 恢复完成立即唤醒 UI，页签尽快出现（spawn 慢的页签不拖累
                        // 其他页签应用，全部完成后统一追加）。
                        wake_ctx.request_repaint();
                    });
                }
            }
            // 全部恢复 spawn 完成后（若尚无恢复任务则 skips 返回），按保存序一次性
            // 追加到页签，再应用上次激活页签。避免逐条插入的越界/顺序错乱。
            // 恢复 spawn 必须在本块之前执行：首帧先把 spawning 填上，否则单独恢复
            // 设置页签（无会话）会在会话 spawn 前误触发，导致设置插到会话前面。
if self.spawning.is_empty()
                && (!self.restore_slots.is_empty() || self.restore_settings)
            {
                let slots = std::mem::take(&mut self.restore_slots);
                let mut restored = 0usize;
                for slot in slots {
                    match slot {
                        Some(Ok(sess)) => {
                            self.tabs.push(Tab::Session(Box::new(sess)));
                            restored += 1;
                        }
                        Some(Err(e)) => {
                            self.status = Some(format!("启动失败: {e}"));
                        }
                        None => {}
                    }
                }
// 退出时设置页签开着：固定插回首页之后（位置不再持久化）。
                if self.restore_settings {
                    self.restore_settings = false;
                    self.tabs.insert(1, Tab::Settings);
                }
                if let Some(active) = self.restore_active.take() {
                    self.current = active.min(self.tabs.len() - 1);
                    self.term_focused =
                        matches!(self.tabs.get(self.current), Some(Tab::Session(_)));
                }
                if restored > 0 {
                    self.status = Some(format!("已恢复上次的 {restored} 个终端页签"));
                }
            }
            if self.spawning.is_empty() {
                self.spawn_rx = None;
            }

            match self.tabs.get(self.current) {
                Some(Tab::Session(_)) => {
                    // 记录本次终端真实网格尺寸（show_terminal 每帧同步进 grid_size，
                    // 直接读，省去再查一次字体度量）：重开崩溃页签/切换启动命令时按它
                    // spawn，避免 80x24 起步等首帧 resize 的错尺寸启动路径。
                    self.last_term_size = {
                        if let Some(Tab::Session(s)) = self.tabs.get(self.current) {
                            s.grid_size
                        } else {
                            unreachable!()
                        }
                    };
                    // 页签崩溃隔离：渲染闭包可能 panic（egui 绘制/term 越界等）。
                    // catch_unwind 捕获后关闭该页签，整个软件继续运行。
                    let (dark, status, term_focused) = (
                        self.effective_dark(),
                        &mut self.status,
                        &mut self.term_focused,
                    );
                    let wheel = &self.wheel;
                    let sess = match self.tabs.get_mut(self.current) {
                        Some(Tab::Session(s)) => s,
                        _ => unreachable!(),
                    };
                    let crashed = match std::panic::catch_unwind(
                        std::panic::AssertUnwindSafe(|| {
                            terminal::show_terminal(ui, sess, dark, status, term_focused, wheel);
                        }),
                    ) {
                        Ok(()) => None,
                        Err(payload) => Some(
                            payload
                                .downcast_ref::<&str>()
                                .map(|s| s.to_string())
                                .or_else(|| payload.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "未知错误".to_string()),
                        ),
                    };
                    if let Some(reason) = crashed {
                        let idx = self.current;
                        let (dir, title) = match self.tabs.get(idx) {
                            Some(Tab::Session(s)) => (s.dir.clone(), s.title.clone()),
                            _ => (String::new(), String::new()),
                        };
                        self.close_session(idx);
                        self.confirm = Some(ConfirmDialog::RelaunchSession { dir, title, reason });
                        self.status =
                            Some("该终端页签发生崩溃，已隔离并关闭（其他页签不受影响）。".to_string());
                    }
                }
                Some(Tab::Placeholder { title }) => {
                    // 重启/切换命令期间：显示加载提示，不渲染终端。
                    let title_clone = title.clone();
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() * 0.3);
                            ui.label(RichText::new("🔄 正在重启...").strong().size(20.0));
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(format!("该页签（{title_clone}）正在后台重新启动，请稍候"))
                                    .weak(),
                            );
                        });
                    });
                }
                Some(Tab::Settings) => {
                    // 设置页很长（启动命令列表 / 工具更新路径 / 供应商配置三个页签 /
                    // 深浅主题…），窗口不够高时下面的区块直接被面板裁掉且滚不到。
                    // 整页套竖向滚动区：auto_shrink([false,false]) = 宽高都撑满可用
                    // 空间（否则内容短时滚动区会缩成内容宽，右侧留白）。
                    egui::ScrollArea::vertical()
                        .id_salt("settings_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.settings_ui(ui));
                }
                _ => {
                    self.home_ui(ui);
                }
            }
        });

        self.input_dialog(ui);
        self.confirm_dialog(ui);
    }
}
#[cfg(test)]
mod tab_icon_tests {
    use super::{
        state_due, tab_icon, ANIM_BUSY_MS, ICON_EMPTY, SNAP_EXITED, STATE_CHECK_MS,
    };

    fn icon(ever: bool, count: u32, viewed: bool, silent_ms: u64) -> Option<&'static str> {
        let now = 100_000u64;
        tab_icon(false, false, ever, count, viewed, now.saturating_sub(silent_ms), now, 0, 0, 0)
    }

    // 最近 3s 内有内容 → 🔄。动画块同样刷新 last_output_ms → 周期重绘/旋转
    // 期间保持运行；count=0（纯动画会话）只要有输出照样 🔄。
    #[test]
    fn content_within_3s_shows_running() {
        assert_eq!(icon(true, 10, false, 2_000), Some("🔄"));
        assert_eq!(icon(true, 0, false, 2_000), Some("🔄"));
    }

    // 边界：恰好 3s 内仍有内容 → 🔄；超过 3s → 完成。
    #[test]
    fn three_sec_boundary() {
        assert_eq!(icon(true, 10, false, 3_000), Some("🔄"));
        assert_eq!(icon(true, 10, false, 3_001), Some("✅"));
    }

    // 3s 无内容 + 有实质输出 + 未查看 → ✅（完成/待查看）。
    #[test]
    fn content_stopped_3s_shows_done() {
        assert_eq!(icon(true, 10, false, 10_000), Some("✅"));
    }

    // 已查看（点击过页签/切走前台同步）→ ✅ 消失；viewed 不复位，后续不再重复亮 ✅。
    #[test]
    fn done_clears_after_viewed() {
        assert_eq!(icon(true, 10, true, 10_000), None);
    }

    // 从未有实质输出（count=0，纯动画/零输出会话）→ 3s 无内容后落空，不亮 ✅。
    #[test]
    fn no_content_never_shows_done() {
        assert_eq!(icon(true, 0, false, 10_000), None);
    }

    // 零输出会话（spawn 后从未读到任何数据块）不假闪 🔄：last_output_ms 仍是
    // spawn 时刻（对 now 而言“新鲜”），若没有 ever_output 门会在前 3s 闪 🔄。
    #[test]
    fn never_output_never_flashes_running() {
        let now = 100_000u64;
        // loading 未结束 → 🔄 由加载态负责（正常）；加载结束后零输出 → 空。
        assert_eq!(tab_icon(false, false, false, 0, false, now - 500, now, 0, 0, 0), None);
        assert_eq!(tab_icon(false, false, false, 0, false, now - 10_000, now, 0, 0, 0), None);
    }

    // 已退出 / 启动加载具有最高优先级。
    #[test]
    fn exited_and_loading_override() {
        let now = 100_000u64;
        assert_eq!(tab_icon(true, false, false, 10, false, now - 10_000, now, 0, 0, 0), Some("❌"));
        assert_eq!(tab_icon(false, true, false, 10, false, now - 10_000, now, 0, 0, 0), Some("🔄"));
    }

    // 输入驱动例外：最近 1.5s 内用户输过键，回显即使刷新 last_output_ms
    // 也不判运行中 → 空；窗口过期（或从未输入）后按内容判 🔄。
    #[test]
    fn typing_echo_not_running() {
        let now = 100_000u64;
        // 1s 前刚输入过（回显新鲜）→ 不亮 🔄，落空。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 1_000, now, now - 1_000, 0, 0), None);
        // 输入窗口边界：不敢 1.5s 整（< INPUT_ACTIVE_MS 才算），恰过期即恢复。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 1_000, now, now - 1_501, 0, 0), Some("🔄"));
        // 无输入历史（last_input=0，鼠标选择/拖拽等）→ 正常判 🔄。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 1_000, now, 0, 0, 0), Some("🔄"));
    }

    // 输入窗口不吞 ✅：任务完成后开始敲新命令（输入窗口内、但内容已停
    // ≥3s）→ ✅ 保持可见，不因打字闪空。
    #[test]
    fn typing_keeps_done_visible() {
        let now = 100_000u64;
        assert_eq!(tab_icon(false, false, true, 10, false, now - 10_000, now, now - 500, 0, 0), Some("✅"));
        assert_eq!(tab_icon(false, false, true, 10, true, now - 10_000, now, now - 500, 0, 0), None);
    }

    /// 动画活性判定：思考/跑命令期间界面在高频动（spinner/时钟/进度条，帧间隔
    /// ~100ms）→ 判**运行中**（🔄），绝不判完成。这是用户要求「思考或执行命令
    /// 时也能准确识别到不是结束状态」的落点，且不依赖任何 agent 私有协议
    /// （不追 JSONL、不查 DB）——只看屏幕还在不在动。
    #[test]
    fn screen_animation_means_running_not_done() {
        let now = 100_000u64;
        // 静默 10s、动画 100ms 前刚来（spinner 在动）→ 🔄 而不是 ✅。
        assert_eq!(
            tab_icon(false, false, true, 10, false, now - 10_000, now, 0, 0, now - 100),
            Some("🔄"),
            "画面还在高频动 = 还在思考/跑命令，不能判完成"
        );
        // 动画停 300ms（超过 ANIM_BUSY_MS）→ 照常亮 ✅（回合真跑完了）。
        assert_eq!(
            tab_icon(false, false, true, 10, false, now - 10_000, now, 0, 0, now - 300),
            Some("✅")
        );
        // 边界：恰好 250ms（= ANIM_BUSY_MS）算「不忙」，靠 < 而非 <=。
        assert_eq!(
            tab_icon(false, false, true, 10, false, now - 10_000, now, 0, 0, now - ANIM_BUSY_MS),
            Some("✅")
        );
        // 1s 采样点上的 spinner 相位无关：任意 0-150ms 的动画年龄都判「在动」。
        for ago in [0u64, 40, 99, 150] {
            assert_eq!(
                tab_icon(false, false, true, 10, false, now - 10_000, now, 0, 0, now - ago),
                Some("🔄"),
                "spinner 每 ~100ms 一帧，1s 门限下采样必落在 ANIM_BUSY_MS 内（ago={ago}）"
            );
        }
        // 「画面在动」也压掉 ✅ 这条通道之外的误判：动画+已查看仍不亮 ✅。
        assert_eq!(
            tab_icon(false, false, true, 10, true, now - 10_000, now, 0, 0, now - 100),
            Some("🔄")
        );
    }

    /// 从未出现动画输出的普通 shell 命令（last_anim=0）不受否决影响：语义与
    /// 纯静默判据完全一致（不能让普通命令的 ✅ 判据变严）。
    #[test]
    fn never_seen_animation_keeps_silent_done() {
        let now = 100_000u64;
        assert_eq!(
            tab_icon(false, false, true, 10, false, now - 10_000, now, 0, 0, 0),
            Some("✅")
        );
    }

    /// 活性否决不越过更高优先级：动画在动但刚有实质内容 → 仍是 🔄（运行中）。
    #[test]
    fn screen_animation_does_not_override_running() {
        let now = 100_000u64;
        assert_eq!(
            tab_icon(false, false, true, 10, false, now - 500, now, 0, 0, now - 100),
            Some("🔄")
        );
        // 加载中/已退出同理压过一切。
        assert_eq!(
            tab_icon(false, true, true, 10, false, now - 10_000, now, 0, 0, now - 100),
            Some("🔄")
        );
        assert_eq!(
            tab_icon(true, false, true, 10, false, now - 10_000, now, 0, 0, now - 100),
            Some("❌")
        );
    }

    /// 1s 门限：默认最快一秒重算一次快照（降频），但三条事件通道立即放行——
    /// 首次、退出/加载态翻转、新实质内容（动画不算，否则 spinner 把门限顶掉）。
    #[test]
    fn state_snapshot_rechecks_at_most_once_a_second() {
        let now = 100_000u64;
        let idle = ICON_EMPTY;
        // 首次：必算。
        assert!(state_due(0, now, 0, idle, false, false));
        // 刚算过（200ms 前）且什么都没变 → 不算。
        assert!(!state_due(now - 200, now, now - 10_000, idle, false, false));
        // 差一毫秒到 1s → 还差一口气。
        assert!(!state_due(now - STATE_CHECK_MS + 1, now, now - 10_000, idle, false, false));
        // 满 1s → 算。
        assert!(state_due(now - STATE_CHECK_MS, now, now - 10_000, idle, false, false));
        // 事件通道：新实质内容晚于上次判定 → 立即算（命令刚跑起来 🔄 要立刻亮）。
        assert!(state_due(now - 200, now, now - 100, idle, false, false));
        // 事件通道：退出/加载态翻转（快照里没有这两位）→ 立即算（❌ 不能等下一秒）。
        assert!(state_due(now - 200, now, now - 10_000, idle, true, false));
        assert!(state_due(now - 200, now, now - 10_000, idle, false, true));
        // 快照里已是退出态、现在仍退出 → 不重复算（没有无谓的每帧重算）。
        let snap_exit = idle | SNAP_EXITED;
        assert!(!state_due(now - 200, now, now - 10_000, snap_exit, true, false));
    }

    // 滚动/翻页只改视口、不改 last_output_ms → 不计更新状态：滚动后无新内容
    // 仍按内容判据落 ✅/空，滚动本身不点亮 🔄（见 app.rs 图标循环注释）。
    #[test]
    fn scrolling_does_not_count_as_update() {
        assert_eq!(icon(true, 10, false, 10_000), Some("✅")); // 未查看：滚动后仍是完成
        assert_eq!(icon(true, 10, true, 10_000), None); // 已查看：滚动后仍空
    }

    // 滚动转发回显例外（last_scroll_ms 专用 500ms 短窗）：滚动转发 TUI 后立即
    // 到达的重绘回显（last_out 新）→ 不亮 🔄（滚动查看历史不误亮运行中）；
    // 窗口过期后真实输出照常判 🔄（持续滚动看日志时页签仍实时刷新运行状态）。
    // 本地缓冲滚动不写 last_scroll（=0）→ 不产生例外，滚不滚动都不影响判定。
    #[test]
    fn scroll_echo_window_only_swallows_prompt_redraw() {
        let now = 100_000u64;
        // 500ms 内转发过滚轮 + 输出新鲜（TUI 重绘回显）→ 吞掉，不亮 🔄。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 100, now, 0, now - 100, 0), None);
        // 滚动窗口边界：恰过期（501ms）即按内容恢复 🔄。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 100, now, 0, now - 501, 0), Some("🔄"));
        // 无滚动记录（本地缓冲滚动 / 从未转发，last_scroll=0）→ 输出新鲜照常 🔄。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 100, now, 0, 0, 0), Some("🔄"));
        // 滚动例外不吞 ✅：滚动时内容早已停、未查看 → 仍按内容判完成。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 10_000, now, 0, now - 100, 0), Some("✅"));
        // 滚动窗口与输入窗口互不干扰：输入回声例外只由 last_input 触发。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 100, now, now - 100, now - 100, 0), None);
    }
}

#[cfg(test)]
mod settings_accordion_tests {
    use super::{ClientApp as App, SETTINGS_SEC_IDS};
    use eframe::egui;

    /// 状态栏里“能点”的那条提示必须只靠文案识别：不能因为 update_latest 被清掉
    /// （下载完成 / 事件回来）就变成不能点，否则提示还在、手型却没了。
    #[test]
    fn update_hint_depends_on_text_only() {
        assert!(App::status_msg_is_update_hint("发现新版本 v0.2.0（当前 0.1.0）…"));
        assert!(App::status_msg_is_update_hint("新版本已就绪，重启一下即生效"));
        assert!(!App::status_msg_is_update_hint("正在检查更新…"));
        assert!(!App::status_msg_is_update_hint("已启动 2 个会话 · pi v0.1.0"));
    }

    /// 一帧的结果：这一帧真正画了正文的块 + 各块标题行可点的位置。
    struct FrameOut {
        drawn: Vec<&'static str>,
        hits: Vec<(&'static str, egui::Pos2)>,
    }

    /// 直接改某块的底层 `CollapsingState`（走 header 用的同一个 id 口径）。
    fn force_state(ui: &egui::Ui, id_salt: &str, open: bool) {
        let mut st = egui::containers::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            App::settings_section_id(ui, id_salt),
            false,
        );
        st.set_open(open);
        st.store(ui.ctx());
    }

    fn raw_input(t: f64, click: Option<(egui::Pos2, bool)>) -> egui::RawInput {
        let mut raw = egui::RawInput {
            time: Some(t),
            predicted_dt: 0.1, // 本软件锁 10 FPS：一帧 100ms
            ..Default::default()
        };
        if let Some((pos, pressed)) = click {
            raw.events.push(egui::Event::PointerMoved(pos));
            raw.events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            });
        }
        raw
    }

    /// 模拟一帧设置页：按 keep 渲染三块，并把「点了标题」的记账做了（同
    /// `note_settings_sec_clicked`：那个要 `&mut ClientApp`，这里只能照它的语义改
    /// 同一个 keep 变量）。
    fn frame(
        ctx: &egui::Context,
        keep: &mut Option<&'static str>,
        input: egui::RawInput,
    ) -> FrameOut {
        let mut out = FrameOut {
            drawn: vec![],
            hits: vec![],
        };
        let o = ctx.run_ui(input, |ui| {
            for id in SETTINGS_SEC_IDS {
                let top = ui.cursor().min;
                let want = App::settings_section(
                    ui,
                    id,
                    *keep == Some(id),
                    format!("标题 {id}"),
                    |ui| {
                        out.drawn.push(id);
                        ui.label("这一块正文");
                    },
                );
                if let Some(want_open) = want {
                    *keep = want_open.then_some(id);
                    ui.ctx().request_repaint();
                }
                // 标题行就是这块的顶部一行：测试用空字体，标题行高 18px，往里 3px
                // 必落在标题行里。
                out.hits.push((id, top + egui::vec2(3.0, 3.0)));
            }
        });
        o.drop_without_applying_deltas();
        out
    }

    fn test_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(egui::FontDefinitions::empty()); // 不加载字体，测试快
        ctx
    }

    /// 我们算的 id 必须**就是** `CollapsingHeader` 内部用的那个。口径一旦对不上，
    /// 我们动它的展开动画它读不到，「只留一块」就漏了（能同时展开好几块）。旧实现
    /// 栽在这：`make_persistent_id(id_salt)` 哈希的是字符串本身，而 egui 是先把 salt
    /// 压成 u64（`IdSalt`）再哈希，且 header 还自带一层 `ui.vertical` 子 ui。
    #[test]
    fn header_id_matches_our_id() {
        egui::__run_test_ui(|ui| {
            for id in SETTINGS_SEC_IDS {
                let resp = egui::CollapsingHeader::new("标题").id_salt(id).show(ui, |_| {});
                assert_eq!(
                    resp.header_response.id,
                    App::settings_section_id(ui, id),
                    "settings_section_id 与 header 的 id 对不上（egui 改了 id 口径？）"
                );
            }
        });
    }

    /// 手风琴的唯一真相是 keep：不管底层 `CollapsingState` 里同时存着几个 true
    /// （老记忆、手工改 memory），这一帧也只能画出一块；收起要硬切，不留动画尾巴。
    #[test]
    fn only_keep_section_body_is_drawn() {
        let ctx = test_ctx();
        let mut keep: Option<&'static str> = Some(SETTINGS_SEC_IDS[2]);
        // 先人为把前两块都置成展开（模拟状态残留）。
        let o = ctx.run_ui(raw_input(0.1, None), |ui| {
            for id in &SETTINGS_SEC_IDS[..2] {
                force_state(ui, id, true);
            }
        });
        o.drop_without_applying_deltas();

        let mut t = 0.1;
        let out = frame(&ctx, &mut keep, raw_input(t, None));
        assert_eq!(out.drawn, [SETTINGS_SEC_IDS[2]], "残留的两块没收掉");

        // keep = None（三块全收起）→ 一个都不许画。
        keep = None;
        t += 0.1;
        let out = frame(&ctx, &mut keep, raw_input(t, None));
        assert!(out.drawn.is_empty(), "全收起时仍画着：{:?}", out.drawn);

        // keep = 第一块 → 只有第一块，且后续几帧也不许冒出第二块。
        keep = Some(SETTINGS_SEC_IDS[0]);
        for i in 0..4 {
            t += 0.1;
            let out = frame(&ctx, &mut keep, raw_input(t, None));
            assert!(
                out.drawn.len() <= 1,
                "第 {i} 帧画了 {:?}（同一时刻只能有一块）",
                out.drawn
            );
            assert_eq!(out.drawn, [SETTINGS_SEC_IDS[0]]);
        }
    }

    /// 端到端回归：A 开着时点 B 的标题，**任何一帧都只能画出一块正文**；再点一次
    /// 当前那块 → 全收起。正好钉住两个坑：① id 口径对不上（收起的块照旧画着，能同时
    /// 展开好几块）；② 收起的块跟着 egui 的收起动画再画 ~0.2s（10 FPS 下整整一帧
    /// 「刚收的和刚开的都在」）。
    #[test]
    fn clicking_header_never_shows_two_bodies() {
        let ctx = test_ctx();
        let (a, b) = (SETTINGS_SEC_IDS[0], SETTINGS_SEC_IDS[1]);
        let mut keep: Option<&'static str> = Some(a);
        let mut t = 0.0;
        // 先跑几帧把布局和展开动画稳住，并记下标题行的位置（布局稳了才点得准）。
        let mut out = FrameOut {
            drawn: vec![],
            hits: vec![],
        };
        for _ in 0..4 {
            t += 0.1;
            out = frame(&ctx, &mut keep, raw_input(t, None));
            assert_eq!(out.drawn, [a], "稳帧只应有 A 展开");
        }
        let hit = |o: &FrameOut, id: &'static str| o.hits.iter().find(|(i, _)| *i == id).unwrap().1;

        // 点 B：egui 的「点击」是松手那一帧才成立，所以按下/松开分两帧。
        for pressed in [true, false] {
            t += 0.1;
            let o = frame(&ctx, &mut keep, raw_input(t, Some((hit(&out, b), pressed))));
            assert!(
                o.drawn.len() <= 1,
                "点 B 的第 {pressed} 帧画了 {:?}（只能有一块）",
                o.drawn
            );
        }
        assert_eq!(keep, Some(b), "点 B 之后该由 B 独占");
        // 接下来几帧：A 必须立刻收掉，只剩 B 的正文。
        for i in 0..4 {
            t += 0.1;
            out = frame(&ctx, &mut keep, raw_input(t, None));
            assert!(
                out.drawn.len() <= 1,
                "切到 B 后的第 {i} 帧画了 {:?}（只能有一块）",
                out.drawn
            );
            assert_eq!(out.drawn, [b]);
        }

        // 再点 B（当前展开的那块）→ 全收起，之后不许再有正文。
        for pressed in [true, false] {
            t += 0.1;
            let o = frame(&ctx, &mut keep, raw_input(t, Some((hit(&out, b), pressed))));
            assert!(o.drawn.len() <= 1, "收起 B 时画了 {:?}", o.drawn);
        }
        assert_eq!(keep, None, "再点一次当前块 = 全收起");
        for i in 0..3 {
            t += 0.1;
            let o = frame(&ctx, &mut keep, raw_input(t, None));
            assert!(o.drawn.is_empty(), "全收起后第 {i} 帧仍画了 {:?}", o.drawn);
        }
    }


    /// 「哪块开着」要跨重启记住：写进去再读出来是同一块；全收起也能记。
    #[test]
    fn open_sec_persists() {
        egui::__run_test_ctx(|ctx| {
            // 首次：没有任何记录（外层 None = 从没展开过）。
            assert_eq!(App::load_open_settings_sec(ctx), None);
            App::save_open_settings_sec(ctx, Some(SETTINGS_SEC_IDS[1]));
            assert_eq!(
                App::load_open_settings_sec(ctx),
                Some(Some(SETTINGS_SEC_IDS[1]))
            );
            // 全收起也要能存（否则每次进来都回退成上次那块）。
            App::save_open_settings_sec(ctx, None);
            assert_eq!(App::load_open_settings_sec(ctx), Some(None));
            // 非法 id（老版本遗留）当全收起，不 panic。
            ctx.data_mut(|d| {
                d.insert_persisted(egui::Id::new(App::OPEN_SEC_KEY), Some("旧区块".to_string()))
            });
            assert_eq!(App::load_open_settings_sec(ctx), Some(None));
        });
    }
}

#[cfg(test)]
mod restore_coords_tests {
    use super::{offset_to_show, restore_coords, strip_geom, tab_cycle_target, StripMetrics, Tab, TabKind};

    /// 回归：Ctrl+(Shift+)Tab 只在页签之间循环，不得落到首页 / 设置页。
    ///
    /// 旧实现是 `(current ± 1) % tabs.len()`，候选集里混进了 `Home`（下标 0）
    /// 和 `Settings`（末尾），于是「从项目 A 按 Ctrl+Tab」直接回到首页，
    /// 再按又进设置——两个页面都不是页签，想回项目得连按几下，且极易在
    /// 首页/设置之间来回弹。设置页里还嵌着模型配置等大量控件，一进去
    /// 手感就变了。
    #[test]
fn tab_cycle_never_lands_on_home_or_settings() {
        // 真实布局：Home 在 0、Settings 固定在 1，其余是会话/占位页签。
        let tabs = vec![
            Tab::Home,
            Tab::Settings,
            Tab::Placeholder { title: "A 重启中".into() },
        ];
        // 首页 → 前进进页签区第一个，后退进最后一个（都不是首页/设置）
        assert_eq!(tab_cycle_target(&tabs, 0, true), Some(2));
        assert_eq!(tab_cycle_target(&tabs, 0, false), Some(2));
        // 在页签区里循环：2 → 2（只有一个候选，原地不动），且绝不去 0/1
        assert_eq!(tab_cycle_target(&tabs, 2, true), Some(2));
        assert_eq!(tab_cycle_target(&tabs, 2, false), Some(2));
        // 设置页 → 同样进页签区
        assert_eq!(tab_cycle_target(&tabs, 1, true), Some(2));
        assert_eq!(tab_cycle_target(&tabs, 1, false), Some(2));

        // 多个页签：在**页签之间**环回，不经过首页/设置
        let tabs = vec![
            Tab::Home,
            Tab::Settings,
            Tab::Placeholder { title: "A".into() },
            Tab::Placeholder { title: "B".into() },
            Tab::Placeholder { title: "C".into() },
        ];
        assert_eq!(tab_cycle_target(&tabs, 2, true), Some(3));
        assert_eq!(tab_cycle_target(&tabs, 3, true), Some(4));
        assert_eq!(tab_cycle_target(&tabs, 4, true), Some(2), "末尾要回卷到第一个页签，而不是设置页");
        assert_eq!(tab_cycle_target(&tabs, 2, false), Some(4), "后退回卷也不落设置页");
        assert_eq!(tab_cycle_target(&tabs, 4, false), Some(3));

        // 满页遍历一圈，一次都不许出现 0（首页）或 1（设置）
        let mut cur = 0usize;
        for _ in 0..12 {
            cur = tab_cycle_target(&tabs, cur, true).unwrap();
            assert!((2..=4).contains(&cur), "Ctrl+Tab 落到了非页签下标 {cur}");
        }
    }

/// 没有项目页签时（只有首页 + 设置）→ 按键不做事，不能 panic 也不能乱跳。
    #[test]
    fn tab_cycle_noop_without_session_tabs() {
        let tabs = vec![Tab::Home];
        assert_eq!(tab_cycle_target(&tabs, 0, true), None);
        assert_eq!(tab_cycle_target(&tabs, 0, false), None);
        let tabs = vec![Tab::Home, Tab::Settings];
        assert_eq!(tab_cycle_target(&tabs, 0, true), None);
        assert_eq!(tab_cycle_target(&tabs, 1, false), None);
        // 越界的 current（理论上不会出现）也不许 panic
        assert_eq!(tab_cycle_target(&tabs, 99, true), None);
    }

    /// 页签栏横向排布：宽度公式 = 图标槽 + 间距 + 标题 + 间距 + ×（占位页签无 ×），
    /// 不足最小宽度的按最小宽度算，页签之间再加一个 gap。
    ///
    /// 这份数字同时决定「内容有多宽」（裁剪范围）和「当前页签在哪」（跟随滚动），
    /// 与绘制共用一份常量 TAB_GAP，改一处就够。
    #[test]
    fn strip_geom_lays_tabs_out_left_to_right() {
        let m = StripMetrics { slot_w: 16.0, close_w: 8.0, min_width: 60.0, gap: 8.0, pad: 4.0 };
        // 两个会话页签（标题宽 30）+ 间隔。
        let g = strip_geom(&[(2, 30.0, true), (3, 30.0, true)], m);
        // 单页签 = 16+4+30+4+8 = 62 ≥ 最小 60 → 62 + pad 4 = 66。
        assert_eq!(g.spans, vec![(2, 0.0, 66.0), (3, 74.0, 66.0)]);
        assert_eq!(g.content_w, 140.0);
        // 短标题按最小宽度兜底（32+4=36 < 60 → 60+4 = 64），不缩成一小条。
        let g = strip_geom(&[(2, 4.0, true)], m);
        assert_eq!(g.spans, vec![(2, 0.0, 64.0)]);
        assert_eq!(g.content_w, 64.0);
// 占位页签没有 ×：自然宽度少「一个间距 + ×」，但最小宽度只少一个间距。
        let g = strip_geom(&[(2, 30.0, false)], m);
        assert_eq!(g.spans, vec![(2, 0.0, 60.0)]);
        // 空会话区 → 没有页签、内容宽 0（滚动范围自然为 0）。
        let g = strip_geom(&[], m);
        assert!(g.spans.is_empty());
        assert_eq!(g.content_w, 0.0);
    }

    /// 滚动跟随：当前页签必须留在视口里；已经装得下时不得乱动。
    #[test]
    fn offset_to_show_keeps_active_tab_visible() {
        // 视口 100、内容 300 → 偏移范围 [0, 200]。
        let f = |off, x, w| offset_to_show(off, 100.0, 300.0, x, w);
        // 已经可见 → 不动。
        assert_eq!(f(50.0, 60.0, 60.0), 50.0);
        // 在左边之外 → 贴左（当前页签紧跟左缘）。
        assert_eq!(f(150.0, 40.0, 60.0), 40.0);
        // 右缘出界 → 右缘贴视口右缘（180 + 60 - 100）。
        assert_eq!(f(10.0, 180.0, 60.0), 140.0);
        // 越界的旧偏移先夹进范围再判断（页签被关掉后偏移可能超界）：夹到 200 后
        // 当前页签落在左边之外 → 贴它的左缘（可见性优先）。
        assert_eq!(f(9999.0, 60.0, 60.0), 60.0);
        // 负偏移先夹到 0；右缘仍超界时只能贴到最大偏移（末尾那点露不全是
        // 内容比视口只多一点的必然结果，不许给出越界偏移）。
        assert_eq!(f(-50.0, 250.0, 60.0), 200.0);
        // 内容比视口窄 → 无处可滚，恒为 0。
        assert_eq!(offset_to_show(80.0, 100.0, 60.0, 0.0, 60.0), 0.0);
        // 单个页签比视口还宽 → 贴左（前缀可见），不许算出越界偏移。
        assert_eq!(f(100.0, 0.0, 300.0), 0.0);
    }

    #[test]
fn restore_indices_skip_exited_and_settings() {
        use TabKind::*;
        // settings_pos 恒为 1：设置页签固定插在首页之后（位置不再持久化）。
        // 恢复数组 = [Home, Settings, B, D]（A 已退出不恢复）。
        let kinds = [Home, Settings, Gone, Alive, Alive];
        assert_eq!(restore_coords(&kinds, 3), (2, 1)); // B → 2
        assert_eq!(restore_coords(&kinds, 4), (3, 1)); // D → 3
assert_eq!(restore_coords(&kinds, 1), (1, 1)); // Settings 自身 → 1
        assert_eq!(restore_coords(&kinds, 2), (2, 1)); // 已退出 → 邻位（正好是 B）
        assert_eq!(restore_coords(&kinds, 0), (0, 1)); // Home → 0
        // 无设置页签：坐标 = Home + 存活会话序（不因没设置而错位）。
        let kinds = [Home, Alive, Alive];
        assert_eq!(restore_coords(&kinds, 2), (2, 1));
        assert_eq!(restore_coords(&kinds, 1), (1, 1));
        // 旧配置里设置页签夹在会话中间（拖动过的老状态）：激活会话仍要落在它自己
        // 上——会话一律排在固定的设置页签之后。
        let kinds = [Home, Alive, Settings, Alive];
        assert_eq!(restore_coords(&kinds, 1), (2, 1));
        assert_eq!(restore_coords(&kinds, 3), (3, 1));
        assert_eq!(restore_coords(&kinds, 2), (1, 1));
    }
}


#[cfg(all(test, windows))]
mod vscode_tests {
    use super::ClientApp;

    #[test]
    fn spaced_dir_passes_through_for_quotes() {
        assert_eq!(ClientApp::cmd_arg(r"D:\AI\with space dir\test"), r"D:\AI\with space dir\test");
        assert_eq!(ClientApp::cmd_arg(r"D:\AI\with space\foo&bar"), r"D:\AI\with space\foo&bar");
    }

    #[test]
    fn specials_escaped_only_without_whitespace() {
        assert_eq!(ClientApp::cmd_arg(r"D:\AI\foo&bar"), r"D:\AI\foo^&bar");
        assert_eq!(ClientApp::cmd_arg(r"D:\AI\a|b<c>d(x)"), r"D:\AI\a^|b^<c^>d^(x^)");
        assert_eq!(ClientApp::cmd_arg(r"D:\AI\caret^char"), r"D:\AI\caret^^char");
        assert_eq!(ClientApp::cmd_arg(r"D:\p"), r"D:\p");
    }
}

#[cfg(all(test, windows))]
mod update_tests {
    use super::{
        asset_table_from_json, assets_from_html, candidate_chains, direct, exe_asset_from_html,
        extract_zip, fetch_release_assets, fetch_release_list, find_exe_in_stage,
        is_plausible_tag, parse_release_list, parse_tag, download_tool_archive,
        parse_version_token, pick_release_tag_with_assets, pick_tool_assets, purge_other_shards,
        shard_fp, sync_tree, tag_probes, tar_bin, tool_asset_names, tool_exe_candidates,
        tool_fresh_dir_in,
        version_newer, win_arch, AssetTable, ClientApp, Probe, ProbeKind, ReleaseEntry, ToolErr,
        GH_MIRRORS, TOOL_SPECS,
    };
    use std::path::PathBuf;

    /// 临时探针：跑真实网络下的完整工具下载管线，定位「下载失败」到底断在哪一环。
    #[test]
    #[ignore = "要真联网，手动跑"]
    fn live_tool_download_pipeline() {
        for spec in TOOL_SPECS.iter() {
        println!("\n##### 工具 = {} repo={} #####", spec.label, spec.repo);

        let probes = tag_probes(spec.repo);
        println!("tag 源数 = {}（池 {} 个）", probes.len(), probes.iter().filter(|p| p.pool).count());

        let tag = String::from(match spec.id {
            "pi" => "v0.99.2",
            _ => "v1.18.34",
        });
        println!("\n--- 1) fetch_release_assets({}) ---", tag);
        match fetch_release_assets(spec.repo, &tag) {
            Ok((src, t)) => println!("  OK 来源={} 资产 {} 项", src, t.len()),
            Err(e) => println!("  ERR {}", e),
        }

        println!("\n--- 2) fetch_release_list（tag 自愈兼底）---");
        match fetch_release_list(spec.repo) {
            Ok(rs) => {
                println!("  OK {} 个 release", rs.len());
                let names = tool_asset_names(spec);
                match pick_release_tag_with_assets(&rs, &names) {
                    Some(t) => println!("  锚定 tag = {}", t),
                    None => println!("  锚定失败：没有任何 release 带 Windows 压缩包"),
                }
            }
            Err(e) => println!("  ERR {}", e),
        }

        println!("\n--- 3) download_tool_archive 真下载 ---");
        let dir = std::env::temp_dir().join(format!("tuipm_live_probe_{}", spec.id));
        let _ = std::fs::create_dir_all(&dir);
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::spawn(move || {
            let (a, b) = rx.recv().unwrap_or((0, 0));
            println!("  进度回执收到: {} / {}", a, b);
        });
        match download_tool_archive(spec, &tag, &dir, &tx, &cancel) {
            Ok((zip, size, url)) => {
                println!("  OK 下载 {} 字节 tag={}", size, url);
                // 4) 解压 + 找主 exe + 安装（后半段管线）
                let stage = dir.join("stage");
                let _ = std::fs::remove_dir_all(&stage);
                println!("\n--- 4) extract_zip ---");
                match extract_zip(&zip, &stage) {
                    Ok(()) => println!("  OK 解压完成"),
                    Err(e) => {
                        println!("  ERR 解压失败: {}", e);
                        let _ = std::fs::remove_dir_all(&dir);
                        return;
                    }
                }
                println!("\n--- 5) find_exe_in_stage({}) ---", spec.exe_name);
                match find_exe_in_stage(&stage, spec.exe_name) {
                    Some(exe) => println!(
                        "  OK {}  ({} 字节)",
                        exe.display(),
                        std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0)
                    ),
                    None => {
                        println!("  ERR 暂存目录里找不到 {}", spec.exe_name);
                        let mut names = Vec::new();
                        if let Ok(rd) = std::fs::read_dir(&stage) {
                            for e in rd.flatten() {
                                names.push(e.file_name().to_string_lossy().into_owned());
                            }
                        }
                        println!("  stage 顶层: {:?}", &names[..names.len().min(12)]);
                    }
                }
            }
            Err(e) => println!("  ERR {} (structural={})", e.msg, e.structural),
        }
        let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 回归锁：下载/探查一律**不带任何代理**（用户要求）。两层都要在：
    /// curl 侧的 `--noproxy *`，以及环境变量被清干净（否则改个传输层/换个
    /// 工具就又走代理了）。
    #[test]
    fn downloads_never_use_proxy() {
        let mut c = std::process::Command::new("curl");
        direct(&mut c);
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let i = args.iter().position(|a| a == "--noproxy").expect("必须带 --noproxy");
        assert_eq!(args.get(i + 1).map(String::as_str), Some("*"));
        for k in [
            "http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
        ] {
            // env_remove 的效果是「键仍在、值被抹成 None」。
            assert!(
                c.get_envs()
                    .filter(|(k2, _)| k2.to_string_lossy() == k)
                    .all(|(_, v)| v.is_none()),
                "环境变量 {k} 必须从子进程环境里去掉"
            );
        }
    }

    /// unlock_exe 把运行映像改名成 {name}.running，替换/重启目标必须去掉后缀
    /// 落到正式名，否则 remove/rename 打在锁定映像上 → 更新失败只剩 .new/.old。
    #[test]
    fn canonical_exe_path_strips_running() {
        assert_eq!(
            ClientApp::canonical_exe_path(PathBuf::from(r"D:\a\TUIProjectManager.exe.running")),
            PathBuf::from(r"D:\a\TUIProjectManager.exe")
        );
        assert_eq!(
            ClientApp::canonical_exe_path(PathBuf::from(r"D:\a\TUIProjectManager.exe")),
            PathBuf::from(r"D:\a\TUIProjectManager.exe")
        );
    }

    #[test]
    fn html_exe_extracted() {
        let html = r#"<a href="/qq458249269/TUIProjectManager/releases/download/v2025.06.30.0001/TUIProjectManager.exe">TUIProjectManager.exe</a>"#;
        let a =
            exe_asset_from_html(html, "qq458249269/TUIProjectManager", "v2025.06.30.0001").unwrap();
        assert_eq!(a.name, "TUIProjectManager.exe");
        // URL 必须带 owner/repo：旧实现拼的是 github.com/releases/download/…，
        // 少了这两段，HTML 源拿到的直链 100% 是死链。
        assert_eq!(
            a.url,
            "https://github.com/qq458249269/TUIProjectManager/releases/download/v2025.06.30.0001/TUIProjectManager.exe"
        );
    }

    /// **url 与 name 不得装反**（历史事故：自更新拿文件名当 URL 请求，10 条链全 404；
    /// 又拿整条 URL 当文件名，Windows 开不出输出文件 → 进度永远 0 + 下载必失败）。
    /// 这里按 `download_update` 的用法钉住两个字段各自的形状。
    #[test]
    fn html_exe_fields_not_swapped() {
        let html = r#"<a href="/q/q/releases/download/v1/tui-project-manager.exe">x</a>"#;
        let a = exe_asset_from_html(html, "q/q", "v1").unwrap();
        assert!(a.url.starts_with("https://github.com/") && a.url.ends_with(".exe"), "url 字段必须是可请求的完整直链，实际 {}", a.url);
        assert!(!a.url.contains(' '), "url 字段不得是文件名");
        // name 要能直接当文件名落盘：不得含分隔符 / 盘符冒号。
        for bad in ['/', '\\', ':', '?', '*', '?', '"', '<', '>', '|'] {
            assert!(!a.name.contains(bad), "name 字段含路径分隔符 {bad:?}：{}", a.name);
        }
        assert!(a.name.ends_with(".exe") && !a.name.contains("github.com"));
        // 拼镜像前缀后必须仍是「镜像 + 直链」，而不是「镜像 + 文件名」。
        let chained = candidate_chains(&a.url);
        assert_eq!(chained[0].url, format!("{}{}", GH_MIRRORS[0], a.url));
    }

    #[test]
    fn html_wrong_tag_ignored() {
        let html = r#"<a href="/q/q/releases/download/vother/Other.exe">x</a>"#;
        assert!(
            exe_asset_from_html(html, "q/q", "v2025.06.30.0001").is_none(),
            "tag 不匹配的资产不得被收下"
        );
    }

    #[test]
    fn html_other_repo_ignored() {
        let html = r#"<a href="/a/b/releases/download/v1/x.exe">x</a>"#;
        assert!(assets_from_html(html, "c/d", "v1").is_empty(), "别的仓库的资产不得混入");
    }

    #[test]
    fn html_query_stripped() {
        let html = r#"<a href="/q/q/releases/download/v1/TUIProjectManager.exe?download=1">x</a>"#;
        let a = exe_asset_from_html(html, "q/q", "v1").unwrap();
        assert_eq!(a.name, "TUIProjectManager.exe");
        assert!(!a.url.contains('?'));
    }

    #[test]
    fn html_no_exe_returns_none() {
        assert!(exe_asset_from_html("<html>nothing</html>", "q/q", "v1").is_none());
    }

    #[test]
    fn looks_like_exe_checks_mz_header() {
        let dir = std::env::temp_dir();
        let p = dir.join("tpm_test_mz_check.bin");
        // 真实 exe ≥256KB 才过体积门（volume gate 2026-09 后加的，旧 6 字节桩恒败）；
        // 构造带 DOS 头（0x3C 处 e_lfanew=0x40）→ "PE\0\0" 的合法最小夹具。
        let mut ok = vec![0u8; 256 * 1024 + 8];
        ok[0..2].copy_from_slice(b"MZ");
        ok[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        ok[0x40..0x44].copy_from_slice(b"PE\0\0");
        std::fs::write(&p, &ok).unwrap();
        assert!(super::looks_like_exe(&p));
        // 同体积但纯 HTML：不判 exe。
        std::fs::write(&p, vec![b'<'; 256 * 1024]).unwrap();
        assert!(!super::looks_like_exe(&p));
        // 体积不够（历史 6 字节桩场景）恒判否。
        std::fs::write(&p, b"MZ\x90\x00\x03\x00").unwrap();
        assert!(!super::looks_like_exe(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn looks_like_zip_checks_magic_and_tail() {
        let p = std::env::temp_dir().join("tpm_test_zip_check.zip");
        // 合法最小桩：头 "PK\x03\x04"、尾 "PK\x05\x06"、体积过 1MB 门。
        let mut ok = vec![0u8; 1024 * 1024 + 16];
        ok[0..4].copy_from_slice(b"PK\x03\x04");
        let n = ok.len();
        ok[n - 4..].copy_from_slice(b"PK\x05\x06");
        std::fs::write(&p, &ok).unwrap();
        assert!(super::looks_like_zip(&p));
        // 截断包：头在、尾缺（镜像断流/代理改写的典型形态）→ 判否。
        let mut truncated = ok.clone();
        let n = truncated.len();
        truncated[n - 4..].fill(0);
        std::fs::write(&p, &truncated).unwrap();
        assert!(!super::looks_like_zip(&p));
        // 错误页 HTML：头不对 → 判否。
        std::fs::write(&p, vec![b'<'; 1024 * 1024 + 16]).unwrap();
        assert!(!super::looks_like_zip(&p));
        // 体积不够（小体积资产/空包）→ 判否。
        std::fs::write(&p, b"PK\x03\x04PK\x05\x06").unwrap();
        assert!(!super::looks_like_zip(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn version_token_from_cli_output() {
        // pi --version / opencode --version 的实际形态
        assert_eq!(parse_version_token("0.87.1\n"), "0.87.1");
        assert_eq!(parse_version_token("1.18.32"), "1.18.32");
        // 带 v 前缀 / 前后杂音
        assert_eq!(parse_version_token("v0.88.0"), "0.88.0");
        assert_eq!(parse_version_token("pi version 0.87.1 (windows-x64)"), "0.87.1");
        // 非版本 token 不能误认（opencode 的资产名 windows-x64 之类）
        assert_eq!(parse_version_token("opencode-windows-x64"), "");
        assert_eq!(parse_version_token(""), "");
    }

    #[test]
    fn tool_versions_compare_by_dotted_numbers() {
        // pi: 0.87.1 → 0.88.0；opencode: 1.18.32 → 1.19.0
        assert!(version_newer("0.88.0", "0.87.1"));
        assert!(version_newer("1.19.0", "1.18.32"));
        assert!(!version_newer("0.87.1", "0.87.1"));
        assert!(!version_newer("0.87.1", "0.88.0"));
    }

    /// 工具 exe 查找顺序：传入的目录（默认 = 本软件所在目录）优先于 PATH，
    /// 且同路径去重。
    #[test]
    fn tool_exe_candidates_prefer_app_dir() {
        let app = PathBuf::from(r"D:\soft\TUIProjectManager");
        let path = std::ffi::OsString::from(r"D:\Agent;D:\Agent\pi");
        let c = tool_exe_candidates("pi.exe", "pi", &[app.clone()], Some(path.clone()));
        assert_eq!(c[0], app.join("pi.exe"));
        assert_eq!(c[1], app.join("pi").join("pi.exe"));
        assert_eq!(c[2], PathBuf::from(r"D:\Agent\pi.exe"));
        assert_eq!(c[3], PathBuf::from(r"D:\Agent\pi\pi.exe"));
        // 多个目录（软件目录 + 用户配的额外目录）按传入顺序。
        let c2 = tool_exe_candidates(
            "pi.exe",
            "pi",
            &[app.clone(), PathBuf::from(r"D:\Agent\pi")],
            None,
        );
        assert_eq!(c2[2], PathBuf::from(r"D:\Agent\pi\pi.exe"));
        // 同路径去重：额外目录与 PATH 命中同一条 → 只留一份。
        let same = tool_exe_candidates(
            "pi.exe",
            "pi",
            &[PathBuf::from(r"D:\Agent\pi")],
            Some(path),
        );
        assert_eq!(
            same,
            vec![
                PathBuf::from(r"D:\Agent\pi\pi.exe"),
                PathBuf::from(r"D:\Agent\pi\pi\pi.exe"),
                PathBuf::from(r"D:\Agent\pi.exe"),
            ]
        );
        // 不给目录也不给 PATH（如 current_exe 失败且关了 PATH）→ 无候选。
        assert!(tool_exe_candidates("pi.exe", "pi", &[], None).is_empty());
    }

    /// 未安装时的新装目录：pi 进 <软件目录>\pi\（zip 含整棵程序树，平铺会把
    /// 软件目录弄脏），opencode 平铺在软件同级目录（zip 只含 exe）。且新装位置
    /// 必须是查找顺序能命中的摆法，否则装完下次检查仍报「未安装」。
    #[test]
    fn fresh_install_dir_keeps_app_dir_clean() {
        let app = PathBuf::from(r"D:\soft\TUIProjectManager");
        let pi = TOOL_SPECS.iter().find(|s| s.id == "pi").unwrap();
        let oc = TOOL_SPECS.iter().find(|s| s.id == "opencode").unwrap();
        assert_eq!(
            tool_fresh_dir_in(&app, pi),
            PathBuf::from(r"D:\soft\TUIProjectManager\pi")
        );
        assert_eq!(tool_fresh_dir_in(&app, oc), app);
        for spec in [pi, oc] {
            let dir = tool_fresh_dir_in(&app, spec);
            let cands = tool_exe_candidates(spec.exe_name, spec.id, &[dir], None);
            assert!(
                cands.iter().any(|c| *c == app.join(spec.id).join(spec.exe_name)),
                "{id}: 新装位置不在查找候选里",
                id = spec.id
            );
        }
    }

    /// 本地版本兜底：`--version` 认不出（或比亲手装的还旧）→ 用装上的 tag，
    /// 免得同一个 tag 反复被报成“有新版本”。认得出且不旧 → 原样用。
    #[test]
    fn local_version_falls_back_to_installed_tag() {
        use super::local_version_with_floor;
        assert_eq!(local_version_with_floor("", Some("0.9.1")), "0.9.1");
        assert_eq!(local_version_with_floor("0.1.0", Some("0.9.1")), "0.9.1");
        assert_eq!(local_version_with_floor("0.9.1", Some("0.9.1")), "0.9.1");
        assert_eq!(local_version_with_floor("1.0.0", Some("0.9.1")), "1.0.0");
        // 没装过（installed=None）就认 --version 的，哪怕它读不出来。
        assert_eq!(local_version_with_floor("", None), "");
    }

    /// TEMP-DIAG-REMOVED
    /// button_text_width 的估算要和 egui Button 的真实宽度对得上（否则宽度
    /// 预算算错，按钮照样会被右侧簇盖住）。单测里用 egui 的测试 Ui 真加一个
    /// 按钮，拿 Response 的宽做对照，容忍 1px 排版误差。
    #[test]
    fn button_text_width_matches_egui() {
        use super::button_text_width;
        eframe::egui::__run_test_ui(|ui| {
            for l in [
                "⋯ 更多",
                "🔄 检查更新",
                "🌙 深色",
                "⬇ 下载 2026.09.13",
                "⬇ 装 pi 到本软件目录",
                "🔄 重启应用",
                "✕ 取消",
            ] {
                let actual = ui.add(eframe::egui::Button::new(l)).rect.width();
                let pred = button_text_width(ui, l);
                assert!(
                    (actual - pred).abs() <= 1.0,
                    "「{l}」宽度估偏：预估 {pred:.1} vs 实际 {actual:.1}"
                );
            }
        });
    }

    /// 状态栏消息宽度：先扣右侧固定簇（它贴右边画，不看左边、不换行、只盖上
    /// 去），再扣待办按钮；不够就不画这条消息，溢出交给横向滚动区。
    #[test]
    fn status_msg_width_reserves_buttons() {
        use super::status_msg_width;
        // 现场：行宽 1400，右侧簇约 270 → 左段 1120；一条工具入口 + 自更新约
        // 315。宽裕时消息拿满 45% 上限。
        let msg = status_msg_width(1400.0, 1120.0, 315.0, 8.0);
        assert!((msg - 630.0).abs() < 0.5, "宽裕时取上限，实际 {msg}");

        // 行宽 900（左段 620）：工具按钮占 315，剩 ~297 给消息 → 缩到剩余宽度，
        // 不用滚动条就能一行摆下。
        let msg = status_msg_width(900.0, 620.0, 315.0, 8.0);
        assert!(msg > 40.0 && msg < 405.0, "应缩到剩余宽度，实际 {msg}");

        // 行宽 560（左段 280）：按钮自己就装不下了 → 消息整条不画（0），剩下
        // 交给滚动条，否则画出来就是一条几个字的碎片。
        assert_eq!(status_msg_width(560.0, 280.0, 315.0, 8.0), 0.0);

        // 没有按钮时消息拿满上限；窗口极窄（<80px）时全给消息但也不超上限。
        let msg = status_msg_width(1400.0, 1120.0, 0.0, 8.0);
        assert!((msg - 630.0).abs() < 0.5, "无按钮时取上限，实际 {msg}");
        assert!(status_msg_width(60.0, 60.0, 0.0, 8.0) <= 60.0);
    }

    /// 状态栏工具入口的判定：升级优先；已是最新但本软件目录里没有那份时，
    /// 仍给「装到本软件目录」（否则 PATH 里那份会让这台机器一个入口都没有，
    /// 实测就是“pi 在 PATH 上 → 永远不出现安装按钮”）；同级目录不可写时不
    /// 给假入口。
    #[test]
    fn tool_entry_kind_covers_install_fresh() {
        use super::{tool_entry_kind, ToolEntryKind as K};
        // 有新版 → 更新当前所在目录那份
        assert_eq!(tool_entry_kind(true, false, false, true), K::Update);
        assert_eq!(tool_entry_kind(true, true, false, true), K::Update);
        // 完全没装 → 装到本软件目录
        assert_eq!(tool_entry_kind(false, true, false, true), K::InstallFresh);
        // 装在别处（PATH / 别的配置路径），本软件目录里没有 → 也给入口
        assert_eq!(tool_entry_kind(false, false, false, true), K::InstallFresh);
        // 本软件目录里已经有了且无新版 → 不给
        assert_eq!(tool_entry_kind(false, false, true, true), K::None);
        assert_eq!(tool_entry_kind(false, true, true, true), K::InstallFresh);
        // 拿不到本软件目录（current_exe 失败）→ 无处可装，不给假入口
        assert_eq!(tool_entry_kind(false, true, false, false), K::None);
        assert_eq!(tool_entry_kind(false, false, false, false), K::None);
    }

    /// 回归锁：**下载中必须还画得出入口**（否则「✕ 取消」按钮跟着整块消失，
    /// 用户没法取消）。
    /// 本软件同级目录（has_update=false、missing=false、fresh_present=true）
    /// → `tool_entry_kind` 判 None，只按它判就会把更新区收起来。
    #[test]
    fn tool_entry_visible_keeps_entry_during_download() {
        use super::tool_entry_visible;
        // 已装在同级目录、无新版的工具：平时无入口（不占版面）
        assert!(!tool_entry_visible(false, false, false, true, true));
        // 但下载中一定要有——否则取消按钮没处画
        assert!(tool_entry_visible(true, false, false, true, true));
        // 其余情形不受影响
        assert!(tool_entry_visible(false, true, false, true, true));
        assert!(tool_entry_visible(false, false, true, false, true));
        assert!(!tool_entry_visible(false, false, true, false, false));
    }

    /// 自动配置启动命令时挑哪一条顶替：同义（按 tui_command_key）里只挑能**整体
    /// 替换**的；`pi --foo` 这种带参数的替换掉会丢用户参数，只能追加不能顶替。
    #[test]
    fn replaceable_command_picks_whole_entry_only() {
        let f = ClientApp::replaceable_command_idx;
        let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(f(&list(&["pi --foo"]), "pi"), None);
        assert_eq!(f(&list(&["pi --foo", "pi"]), "pi"), Some(1));
        assert_eq!(f(&list(&[r"D:\old\pi-windows-x64\pi.exe"]), "pi"), Some(0));
        // 带空格的路径整条就是路径，也能整体替换
        assert_eq!(f(&list(&[r"C:\Program Files\pi\pi.exe"]), "pi"), Some(0));
        assert_eq!(f(&list(&[r"C:\Program Files\pi\pi.exe"]), "opencode"), None);
    }

    #[test]
    fn tool_asset_names_follow_machine_arch() {
        for tpl in TOOL_SPECS.iter().flat_map(|s| s.asset_tpls) {
            let name = tpl.replace("{arch}", win_arch());
            assert!(!name.contains("{arch}"), "架构占位符未替换: {name}");
            assert!(name.ends_with(".zip"), "工具资产应为 zip: {name}");
        }
        // x64 机器上 pi 的真实产物名。
        if win_arch() == "x64" {
            assert_eq!(TOOL_SPECS[0].asset_tpls[0].replace("{arch}", win_arch()), "pi-windows-x64.zip");
            assert_eq!(
                TOOL_SPECS[1].asset_tpls[0].replace("{arch}", win_arch()),
                "opencode-windows-x64.zip"
            );
        }
    }

    #[test]
    fn html_zip_asset_extracted() {
        // 工具 release 的 zip 资产也走同一个 HTML 解析器（expanded_assets 片段）
        let html = r#"<a href="/earendil-works/pi/releases/download/v0.88.0/pi-windows-x64.zip">pi</a>"#;
        let all = assets_from_html(html, "earendil-works/pi", "v0.88.0");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "pi-windows-x64.zip");
        assert_eq!(
            all[0].1,
            "https://github.com/earendil-works/pi/releases/download/v0.88.0/pi-windows-x64.zip"
        );
        // tag 不匹配 / 无资产
        assert!(assets_from_html(html, "earendil-works/pi", "v0.87.1").is_empty());
        assert!(assets_from_html("<html>nothing</html>", "earendil-works/pi", "v1").is_empty());
    }

    #[test]
    fn expanded_assets_fragment_parsed() {
        // expanded_assets 片段里链接带 <a href> + 换行/引号收尾，属性顺序也不定，
        // 解析必须只认路径本身，别把 host/tag 对错仓库的收进来。
        let html = r#"
            <li><a href="/anomalyco/opencode/releases/download/v1.2.3/opencode-windows-x64.zip" data-view-component="true">opencode-windows-x64.zip</a></li>
            <li><a href="/anomalyco/opencode/releases/download/v1.2.3/opencode-linux-x64.zip">linux</a></li>
        "#;
        let all = assets_from_html(html, "anomalyco/opencode", "v1.2.3");
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|(_, u)| u.contains("/anomalyco/opencode/")));
        assert!(all[0].1.ends_with("/opencode-windows-x64.zip"));
    }

    #[test]
    fn asset_table_from_json_reads_size_and_url() {
        let j = r#"{"assets":[
            {"name":"opencode-windows-x64.zip","browser_download_url":"https://github.com/anomalyco/opencode/releases/download/v1.2.3/opencode-windows-x64.zip","size":65000000},
            {"name":"checksums.txt","browser_download_url":"https://x/checksums.txt","size":10}
        ]}"#;
        let v: serde_json::Value = serde_json::from_str(j).unwrap();
        let t = asset_table_from_json(&v).unwrap();
        assert_eq!(t["opencode-windows-x64.zip"].1, 65_000_000);
        // 空表当失败：否则「这版没有资产」会被当成拿到了表，后面一步也不走
        assert!(asset_table_from_json(&serde_json::json!({"assets":[]})).is_err());
    }

    #[test]
    fn pick_tool_assets_exact_first_then_discovery() {
        let names = vec!["opencode-windows-x64.zip".to_string()];
        let mut t: AssetTable = AssetTable::new();
        for n in [
            "opencode-windows-x64.zip",
            "opencode-windows-x64-baseline.zip",
            "opencode-linux-x64.zip",
            "opencode-windows-arm64.zip",
            "checksums.txt",
            "source.zip",
        ] {
            t.insert(n.into(), (format!("https://u/{n}"), 1));
        }
        let got: Vec<String> = pick_tool_assets(&t, &names)
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        // 模板精确命中第一；发现项按「普通版 → 兼容版」排；别的架构/校验文件不进
        assert_eq!(
            got,
            vec![
                "opencode-windows-x64.zip",
                "opencode-windows-x64-baseline.zip"
            ]
        );
    }

    #[test]
    fn pick_tool_assets_survives_renamed_product() {
        // 上游改名（pi-windows-x64-gnu.zip）时仍能发现并下载，不至于全线 404。
        let names = vec!["pi-windows-x64.zip".to_string()];
        let mut t: AssetTable = AssetTable::new();
        t.insert("pi-windows-x64-gnu.zip".into(), ("https://u/gnu".into(), 42));
        let got = pick_tool_assets(&t, &names);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "pi-windows-x64-gnu.zip");
        assert_eq!(got[0].2, 42, "字节数要带出来，进度才有百分比");
    }

    /// 回归：tag 探查源里**不得**再出现 jsDelivr。
    /// 事故二（与 jsDelivr 同形、换了个马甲）：**tag 探查的 HTML 源咬到了
    /// GitHub 页面里的路由元数据**。
    ///
    /// `tag_probes` 里的 `PS HTML` 源把整页正文交给 `parse_tag`，而旧实现是
    /// `body.find("/releases/tag/")` 取**第一个**。GitHub 是 React 页，`<head>`
    /// 里这行排在真 tag 链接**前面**：
    ///
    /// ```html
    /// <meta name="route-pattern" content="/:user_id/:repository/releases/tag/*name">
    /// ```
    ///
    /// 于是抠出来的是 `/*name" data-turbo-transient>`：非空、能过「返回空 tag」
    /// 检查、PS 通道 8s 就能连通还常常抢在镜像前面 → 状态栏冒出一个鬼版本号，
    /// 点下去拼出的 `…/releases/download/*name"…/opencode-windows-x64.zip` 全 404。
    /// 现象与 jsDelivr 那次一模一样（异常版本号 + 下载必失败），但根因在解析器。
    ///
    /// 真实标签页正文里 13 处 `/releases/tag/` 依次是：`*name`（模板，1 处）、
    /// `v1.18.33`（真 tag，6 处）、`v1.18.33&quot;,…`（内嵌 JSON，被 `;` 截断后
    /// 仍不合法）。只有第 2 类能过闸——下面把现场存成夹具钉住。
    #[test]
    fn parse_tag_skips_route_pattern_meta_in_release_page() {
        // GitHub 标签页正文骨架（按真实顺序：route-pattern 在 <title>/真链接之前）
        let page = concat!(
            r#"<meta name="route-pattern" content="/:user_id/:repository/releases/tag/*name">"#,
            "<title>Release v1.18.33 - anomalyco/opencode - GitHub</title>",
            r#"<a href="/anomalyco/opencode/releases/tag/v1.18.33">v1.18.33</a>"#,
            r#"<a href="/anomalyco/opencode/releases/tag/v1.18.33&quot;,&quot;user_id&quot;:null}}"#,
        );
        let p = Probe::new("PS HTML", String::new(), ProbeKind::HtmlTag, 8, 15).via_ps();
        assert_eq!(parse_tag(&p, page).unwrap(), "v1.18.33");

        // 旧实现的形状：只认第一个 → 必须换掉（这条断言就是本次回归的根因）
        let naive = &page[page.find("/releases/tag/").unwrap() + "/releases/tag/".len()..];
        assert!(
            !is_plausible_tag(naive.trim()),
            "第一个 /releases/tag/ 仍是路由模板碎片，不能直接当 tag 用：{naive:?}"
        );

        // curl 的 HtmlTag 源给的是重定向 URL（整段就一个链接）→ 照旧能用
        let red = "https://github.com/anomalyco/opencode/releases/tag/v1.18.33";
        assert_eq!(parse_tag(&p, red).unwrap(), "v1.18.33");

        // 一个合法 tag 都找不到（错误页 / 只有模板）→ 判失败让这一源退场，
        // 绝不能把 HTML 碎片当 tag 放行进下载链
        let err_page = "<html><body>503 Service Unavailable</body></html>";
        assert!(parse_tag(&p, err_page).is_err(), "错误页不该解析出 tag");
        let only_meta =
            r#"<meta name="route-pattern" content="/:user_id/:repository/releases/tag/*name">"#;
        assert!(parse_tag(&p, only_meta).is_err(), "只有路由模板时必须判失败");
    }

    /// tag 合法性闸：拒掉 HTML 碎片 / 路径 / 带引号的一坨，放行正常 tag。
    /// 这道闸对 JSON 源同样生效——历史上就混进过一个返回 npm 包版本的源。
    #[test]
    fn is_plausible_tag_rejects_html_fragments() {
        for ok in ["v1.18.33", "1.18.33", "v0.99.1", "1.0.0-rc.1", "v2.0.20"] {
            assert!(is_plausible_tag(ok), "{ok} 应当合法");
        }
        for bad in [
            r#"/*name" data-turbo-transient"#,
            "*name",
            "",
            "v1.18.33&quot;,&quot;user_id&quot;:null",
            "../../evil",
            "v1 18 33",
            "1.18.33\nX-Injected: 1",
        ] {
            assert!(!is_plausible_tag(bad), "{bad:?} 应当判非法");
        }
    }

    /// 事故：`data.jsdelivr.com/v1/packages/gh/{repo}` 返回的是仓库发布到 npm 的
    /// 包版本，不是 GitHub Release 的 tag。opencode 的 npm 版是 `2.0.20`（Release
    /// tag 是 `v1.18.33`），`.../releases/download/2.0.20/…` 必然 404，且
    /// `version_newer("2.0.20", "1.18.32")` 为真 → 装完也永远显示「有新版本」。
    /// jsDelivr 又是抢跑极快的 CDN，排前面就总是它先赢，其余源没机会纠正。
    #[test]
    fn tag_probes_exclude_npm_version_sources() {
        let urls: Vec<String> = tag_probes("anomalyco/opencode").into_iter().map(|p| p.url).collect();
        assert!(
            !urls.iter().any(|u| u.contains("jsdelivr")),
            "tag 探查源里不能有 jsDelivr：npm 包版本 ≠ Release tag\n{urls:?}"
        );
// 兜底：剩下的源都得是「以 GitHub Release 为准」的
        assert!(
            urls.iter().all(|u| u.contains("github.com")),
            "tag 探查只应走 GitHub 系源\n{urls:?}"
        );
    }

    /// 事故回归：下载/探查都不能再「一源一线程全部并发」——并发扇出会撞
    /// api.github.com 匿名限流、把带宽平分到每条都跌破速度地板，于是整轮
    /// 「所有下载源下载失败」。钉住两件事：
    ///  1. 探查源表里 GH_MIRRORS 的那些条目标了 pool（预算可跳过），直连与
    ///     PS 通道一条都不能标（它们是保底，必须试到底）；
    ///  2. 候选链铺开仍是「镜像池在前、保底在后」的顺序。
    #[test]
    fn probe_and_download_sources_are_sequential_pool_then_backup() {
        let probes = tag_probes("anomalyco/opencode");
        let pool: Vec<&str> = probes.iter().filter(|p| p.pool).map(|p| p.desc.as_str()).collect();
        assert_eq!(
            pool.len(),
            GH_MIRRORS.len(),
            "tag 源表里的镜像条数应与 GH_MIRRORS 一一对应\n{pool:?}"
        );
        assert!(
            probes.iter().any(|p| !p.pool && p.url.starts_with("https://api.github.com/")),
            "GitHub API 直连必须是非池源（预算不能砍掉它）"
        );
        #[cfg(windows)]
        assert!(
            probes.iter().any(|p| !p.pool && p.ps),
            "PS 通道必须是非池源（预算不能砍掉它）"
        );
        let chains = candidate_chains("https://github.com/o/r/releases/download/v1/x.zip");
        let first_backup = chains.iter().position(|c| !c.pool).expect("必须有非池保底链");
        assert!(
            chains[..first_backup].iter().all(|c| c.pool),
            "池内链必须排在保底链前面（顺序即优先级）"
        );
        assert!(
            chains.iter().all(|c| c.url.ends_with("x.zip")),
            "候选链 URL 必须以资产直链结尾（前缀只该加在前面）\n{chains:?}"
        );
        // 保底链至少要有 curl 直连；Windows 上另有 PS 通道。
        assert!(chains.last().map(|c| !c.ps).unwrap_or(false), "最后一条应是 curl 直连保底");
    }

    #[test]
    fn parse_release_list_keeps_order_and_skips_draft_prerelease() {
        let body = serde_json::json!([
            {
                "tag_name": "v1.18.33",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {"name": "opencode-windows-x64.zip",
                     "browser_download_url": "https://u/x64.zip", "size": 62127126},
                    {"name": "opencode-linux-x64.tar.gz",
                     "browser_download_url": "https://u/l.tgz", "size": 60}
                ]
            },
            {"tag_name": "v2.0.0-rc1", "draft": false, "prerelease": true,
             "assets": [{"name": "opencode-windows-x64.zip",
                         "browser_download_url": "https://u/rc.zip", "size": 1}]},
            {"tag_name": "v1.18.32", "draft": false, "prerelease": false,
             "assets": [{"name": "opencode-windows-x64.zip",
                         "browser_download_url": "https://u/old.zip", "size": 62}]}
        ]);
        let got = parse_release_list(&body.to_string()).unwrap();
        let tags: Vec<&str> = got.iter().map(|(t, _)| t.as_str()).collect();
        // 预发布不进候选；新 → 旧的原序必须保留（不自己比版本号）
        assert_eq!(tags, vec!["v1.18.33", "v1.18.32"]);
        assert_eq!(got[0].1["opencode-windows-x64.zip"].1, 62127126, "字节数要带出来");
        assert!(parse_release_list("[]").is_err(), "空列表当失败");
        assert!(parse_release_list("not json").is_err());
    }

    /// 回归：错 tag 会被发布列表换成「真的带产物」的那个。
    ///
    /// 事故现场就是 tag=`2.0.20`（npm 版）打不进 opencode 的 release；这里用
    /// 纯函数钉住「只要列表里有带 Windows 压缩包的 release，就该选最新的它」。
    #[test]
    fn pick_release_tag_anchors_on_assets_not_probe() {
        let mk = |tag: &str, with_zip: bool| -> ReleaseEntry {
            let mut t: AssetTable = AssetTable::new();
            if with_zip {
                t.insert(
                    "opencode-windows-x64.zip".into(),
                    ("https://u/x64.zip".into(), 62),
                );
            } else {
                t.insert("opencode-linux-x64.tar.gz".into(), ("https://u/l.tgz".into(), 60));
            }
            (tag.to_string(), t)
        };
        let names = vec!["opencode-windows-x64.zip".to_string()];
        // 最新那个没发 Windows 包 → 往下一个找，而不是直接用最新
        let rel = vec![mk("v1.18.33", false), mk("v1.18.32", true), mk("v1.18.31", true)];
        assert_eq!(
            pick_release_tag_with_assets(&rel, &names).as_deref(),
            Some("v1.18.32")
        );
        // 全都没有 → None（交给上层报「上游改了产物名」）
        let rel = vec![mk("v1.18.33", false), mk("v1.18.32", false)];
        assert_eq!(pick_release_tag_with_assets(&rel, &names), None);
        // 空列表不 panic
        assert_eq!(pick_release_tag_with_assets(&[], &names), None);
    }

    /// 错 tag 自愈后，完成文案用的 tag 必须是**实际下载**的那个，不能是探查来的
    /// 错 tag（否则装的是 1.18.33、状态栏却报 2.0.20，下次又判「有新版本」）。
    #[test]
    fn corrected_tag_is_not_compared_as_version() {
        // 装好 1.18.33 后，探查若仍报 npm 版 2.0.20，必须认为「无新版本」。
        assert!(!version_newer("1.18.33", "2.0.20"));
    }

    #[test]
    fn shard_fp_separates_tags_and_assets() {
        // 同一工具的不同 tag / 不同资产名必须落在不同分片上，否则 -C - 续传
        // 会把两个不同的包首尾拼起来。
        let a = shard_fp("v1.2.3/opencode-windows-x64.zip");
        let b = shard_fp("v1.2.3/opencode-windows-x64-baseline.zip");
        let c = shard_fp("v1.2.4/opencode-windows-x64.zip");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, shard_fp("v1.2.3/opencode-windows-x64.zip"), "指纹要稳定");
        assert_eq!(a.len(), 8);
    }

    #[test]
    fn purge_other_shards_keeps_current() {
        let dir = std::env::temp_dir().join("tpm_test_shards");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let keep = shard_fp("v2/pi-windows-x64.zip");
        std::fs::write(dir.join(format!(".pi-update.zip.{keep}.c0.new")), b"keep").unwrap();
        std::fs::write(dir.join(format!(".pi-update.zip.{keep}.new")), b"done").unwrap();
        std::fs::write(dir.join(".pi-update.zip.deadbeef.c0.new"), b"old").unwrap();
        std::fs::write(dir.join("other-tool.zip.c0.new"), b"unrelated").unwrap();
        let n = purge_other_shards(&dir, ".pi-update.zip", &keep);
        assert_eq!(n, 1);
        assert!(dir.join(format!(".pi-update.zip.{keep}.c0.new")).exists());
        assert!(dir.join(format!(".pi-update.zip.{keep}.new")).exists());
        assert!(!dir.join(".pi-update.zip.deadbeef.c0.new").exists());
        // 别的工具的分片不许被牵连
        assert!(dir.join("other-tool.zip.c0.new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_exe_in_stage_handles_wrapper_dir() {
        let dir = std::env::temp_dir().join("tpm_test_stage_find");
        let _ = std::fs::remove_dir_all(&dir);
        let stage = dir.join("stage");
        std::fs::create_dir_all(stage.join("pi-windows-x64/assets")).unwrap();
        // 套了一层同名目录（发布方常见做法）：原实现只认 stage/pi.exe，
        // 认不到就从「包结构不符」一路重下 5 次 44MB。
        std::fs::write(stage.join("pi-windows-x64/pi.exe"), b"MZ").unwrap();
        assert_eq!(
            find_exe_in_stage(&stage, "pi.exe"),
            Some(stage.join("pi-windows-x64/pi.exe"))
        );
        // 平铺（无套壳）也要认得
        std::fs::write(stage.join("pi.exe"), b"MZ").unwrap();
        assert_eq!(find_exe_in_stage(&stage, "pi.exe"), Some(stage.join("pi.exe")));
        // 确实没有 → None（上层据此判定包结构不符，停止重试）
        assert_eq!(find_exe_in_stage(&dir.join("empty"), "pi.exe"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_err_classifies_structural() {
        // 结构性失败不该再触发「换源重下 60MB」
        let e = ToolErr::structural("包结构与预期不符");
        assert!(e.structural);
        assert!(!ToolErr::net("连接超时").structural);
        // From<String> 默认按网络类处理（保守：宁可多重下一次）
        let e: ToolErr = "下载失败".to_string().into();
        assert!(!e.structural);
    }

    #[test]
    fn staged_exe_reuse_detected_and_consumed() {
        // 「被占用」后重试时应复用暂存里已解好的 exe，而不是重下 60MB 包。
        let dir = std::env::temp_dir().join("tpm_test_reuse");
        let _ = std::fs::remove_dir_all(&dir);
        let stage = dir.join(".pi-update-stage");
        std::fs::create_dir_all(&stage).unwrap();
        // 有效 exe（DOS 头 + PE 签名 + 过体积门）→ 可复用
        let mut pe = vec![0u8; 256 * 1024 + 8];
        pe[0..2].copy_from_slice(b"MZ");
        pe[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        pe[0x40..0x44].copy_from_slice(b"PE\0\0");
        std::fs::write(stage.join("pi.exe"), &pe).unwrap();
        assert!(stage.join("pi.exe").is_file() && super::looks_like_exe(&stage.join("pi.exe")));
        // 残留的坏文件不得被复用（否则会拿损坏产物去替换正式名）
        std::fs::write(stage.join("bad.exe"), b"not an exe").unwrap();
        assert!(!(stage.join("bad.exe").is_file() && super::looks_like_exe(&stage.join("bad.exe"))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_update_rejects_bad_exe_keeps_final() {
        // 工具链复用 install_update：替换前仍会做 PE 头校验，坏产物不得换上。
        let dir = std::env::temp_dir().join("tpm_test_install");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let newf = dir.join("pi.exe.new");
        let finalp = dir.join("pi.exe");
        let oldp = dir.join("pi.exe.old");
        std::fs::write(&newf, b"not a pe file").unwrap();
        std::fs::write(&finalp, b"old").unwrap();
        let msgs = std::sync::Mutex::new(Vec::new());
        let got = super::install_update(&newf, &finalp, &oldp, &|m| {
            msgs.lock().unwrap().push(m.to_string())
        });
        let msgs = msgs.into_inner().unwrap();
        assert!(matches!(got, super::InstallOutcome::BadDownload));
        // 正式名保持原样，坏产物已被删（避免被 -C - 续传拼坏）
        assert_eq!(std::fs::read(&finalp).unwrap(), b"old");
        assert!(!newf.exists());
        // 提示不能含「失败」字样：自更新 UI 按该关键字复位 downloading 状态
        assert!(msgs.iter().all(|m| !m.contains("失败")), "{msgs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_zip_roundtrip() {
        // 用系统 bsdtar 打个真 zip，再解出来验证解压链路（含暂存目录清理）。
        let dir = std::env::temp_dir().join("tpm_test_extract");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip = dir.join("t.zip");
        std::fs::write(dir.join("hello.txt"), b"hello zip").unwrap();
        let mut cmd = std::process::Command::new(tar_bin());
        cmd.arg("-a").arg("-cf").arg(&zip).arg("-C").arg(&dir).arg("hello.txt");
        assert!(cmd.output().unwrap().status.success(), "bsdtar 打 zip 应成功");
        let dest = dir.join("out");
        extract_zip(&zip, &dest).expect("解压应成功");
        assert_eq!(
            std::fs::read_to_string(dest.join("hello.txt")).unwrap(),
            "hello zip"
        );
        // 再解一次：暂存目录应先被清空（不留上一次的残留）
        std::fs::write(dest.join("stale.txt"), b"x").unwrap();
        extract_zip(&zip, &dest).expect("二次解压应成功");
        assert!(!dest.join("stale.txt").exists(), "暂存目录应先清空");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_tree_copies_all_but_exe_and_keeps_extra() {
        let dir = std::env::temp_dir().join("tpm_test_sync");
        let _ = std::fs::remove_dir_all(&dir);
        let stage = dir.join("stage");
        let inst = dir.join("inst");
        std::fs::create_dir_all(stage.join("native/win32")).unwrap();
        std::fs::create_dir_all(inst.join("node_modules")).unwrap();
        std::fs::write(stage.join("pi.exe"), b"MZ").unwrap();
        std::fs::write(stage.join("package.json"), b"{}").unwrap();
        std::fs::write(stage.join("native/win32/a.node"), b"node").unwrap();
        // 安装目录里已有同名文件（要覆盖）与自装目录（不能删）
        std::fs::write(inst.join("package.json"), b"old").unwrap();
        std::fs::write(inst.join("node_modules/keep.txt"), b"keep").unwrap();
        let fails = sync_tree(&stage, &inst, "pi.exe");
        assert_eq!(fails, 0);
        assert_eq!(std::fs::read_to_string(inst.join("package.json")).unwrap(), "{}");
        assert_eq!(
            std::fs::read_to_string(inst.join("native/win32/a.node")).unwrap(),
            "node"
        );
        // 主 exe 由 install_update 负责，sync_tree 不碰
        assert!(!inst.join("pi.exe").exists());
        // 用户自装内容保留
        assert!(inst.join("node_modules/keep.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 替换链路（install_update）的真机验证。`cargo test -- --ignored live_install_ --nocapture`
///
/// 为什么必须真机：`install_update` 的每条分支都踩 Windows 的文件语义，而单测里
/// 一个都造不出来——
///  - **运行映像禁止覆盖写、却允许 rename**（unlock_exe 能腾出正式名全靠这条），
///    std 的 `File::open` 句柄（带 FILE_SHARE_DELETE）模拟不出来，只能真起一个进程；
///  - **独占句柄挡住 rename 源**（Defender 扫 .new 的等效场景），只能用
///    `share_mode(0)` 真锁；
///  - **回滚要 20s 重试窗口耗尽才触发**，也造不出来。
/// 沙箱全在 target/live_install/ 下，不碰真实安装目录；唯一例外是
/// `live_install_into_real_path`，需显式给环境变量才动真文件（见该用例）。
#[cfg(all(test, windows))]
mod live_install_tests {
    use super::{install_update, looks_like_exe, InstallOutcome};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// 每例一个独立沙箱（跑之前清空，避免上一轮的残留把结论带偏）。
    fn sandbox(name: &str) -> PathBuf {
        let root = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_owned()))
            .unwrap_or_else(std::env::temp_dir)
            .join("live_install");
        let dir = root.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 「刚下载好的 .new」夹具：测试二进制自身就是一枚 10MB+ 的合法 exe（MZ+PE
    /// 头齐全），不必造假字节——体积门、e_lfanew 对齐这些只有真产物才有意义。
    /// 文件名照生产格式 `{资产名}.{指纹}.new`，顺手验证指纹路径没被写坏。
    fn fresh_new(dir: &Path, fp: &str) -> PathBuf {
        let dst = dir.join(format!("tui-project-manager.exe.{fp}.new"));
        std::fs::copy(std::env::current_exe().unwrap(), &dst).unwrap();
        dst
    }

    /// 内容指纹（fnv1a）：只用于「是不是同一个文件」的相等判断，不做安全用途。
    fn fnv(p: &Path) -> String {
        use std::io::Read;
        let mut f = std::fs::File::open(p).unwrap();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            for b in &buf[..n] {
                h ^= *b as u64;
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
        }
        format!("{h:016x}（{} 字节）", std::fs::metadata(p).unwrap().len())
    }

    /// 收集 sink 消息（生产里是状态栏通道），断言文案与实际分支对得上。
    struct Msgs(Mutex<Vec<String>>);
    impl Msgs {
        fn new() -> Self {
            Msgs(Mutex::new(Vec::new()))
        }
        fn push(&self, m: &str) {
            println!("  状态栏: {m}");
            self.0.lock().unwrap().push(m.to_owned());
        }
        fn joined(&self) -> String {
            self.0.lock().unwrap().join(" | ")
        }
    }

    /// 把沙箱里那枚 exe 变成「另一个正在运行的实例」：ping 的副本 + `-t` 长跑。
    /// 这样正式名就是一个真·运行映像，覆盖写必被拒、rename 却允许——与生产里
    /// 「用户手滑开了第二个实例」的情形同构。
    fn spawn_running_instance(exe: &Path) -> std::process::Child {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new(exe);
        c.args(["-t", "127.0.0.1"]);
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW，测试别往桌面弹黑窗
        let child = c.spawn().expect("起「另一个实例」失败");
        std::thread::sleep(std::time::Duration::from_millis(600)); // 等映像映射完成
        child
    }

    /// 快路径：正式名空闲 → 一次 rename 就装上（自更新最常见的分支）。
    #[test]
    #[ignore = "要真动文件，手动跑"]
    fn live_install_fast_path() {
        let dir = sandbox("fast_path");
        let final_path = dir.join("tui-project-manager.exe");
        let old_path = dir.join("tui-project-manager.exe.old");
        let new_file = fresh_new(&dir, "fastpath");
        std::fs::copy(std::env::current_exe().unwrap(), &final_path).unwrap();
        let new_fp = fnv(&new_file);

        let msgs = Msgs::new();
        let outcome = install_update(&new_file, &final_path, &old_path, &|m| msgs.push(m));

        assert!(matches!(outcome, InstallOutcome::Done), "快路径应装上");
        assert_eq!(fnv(&final_path), new_fp, "正式名必须是新版本");
        assert!(!new_file.exists(), ".new 应被 rename 走，不留残骸");
        println!("正式名 = {}", fnv(&final_path));
    }

    /// 慢路径：正式名是**另一个正在运行的实例**。真机实测（Windows 11）：覆盖写 /
    /// 覆盖 rename 打在运行映像上是 os error 5「拒绝访问」且**永不**释放，但把运行
    /// 映像 rename 走是放行的 → 腾名 → 放入 .new。这条分支以前只在注释里被论证过，
    /// 从没在真机上跑通过。
    ///
    /// 别断言「正在挪开旧版本」这类进度文案：它只在 rename **失败、等系统释放**时
    /// 才发（report_every），慢路径一次成功时是静默的。走没走慢路径的真指纹是
    /// `.old` 里躺着旧映像的字节。
    #[test]
    #[ignore = "要真动文件 + 起子进程，手动跑"]
    fn live_install_slow_path_when_final_is_running_image() {
        let dir = sandbox("slow_path");
        let final_path = dir.join("tui-project-manager.exe");
        let old_path = dir.join("tui-project-manager.exe.old");
        let new_file = fresh_new(&dir, "slowpath");
        // 正式名 = ping 的副本并真的跑起来。不用真实应用 exe：那 14MB 的东西会
        // 带出窗口/子进程把沙箱搅浑，而这里要的只是「运行映像」这一个文件语义。
        std::fs::copy(r"C:\Windows\System32\ping.exe", &final_path).unwrap();
        // 生产流程（start_download）会先把正式名 copy 成 .old。这里放一份**陈旧**
        // .old：慢路径必须先清掉它，否则 rename 目标被占、白耗 8s 窗口。
        std::fs::write(&old_path, b"stale backup from last time").unwrap();
        let old_fp = fnv(&final_path);
        let new_fp = fnv(&new_file);
        let mut child = spawn_running_instance(&final_path);

        let msgs = Msgs::new();
        let outcome = install_update(&new_file, &final_path, &old_path, &|m| msgs.push(m));
        let _ = child.kill();
        let _ = child.wait();

        assert!(matches!(outcome, InstallOutcome::Done), "慢路径也应装上");
        assert_eq!(fnv(&final_path), new_fp, "腾出的正式名要放新版本");
        assert_eq!(fnv(&old_path), old_fp, "旧映像应完整挪进 .old（兼备份）——这就是慢路径的真指纹");
        assert!(!new_file.exists());
        println!("状态栏: {}", msgs.joined());
    }

    /// 回滚：`.new` 被独占句柄占住（Defender 实时扫描的等效场景）→ 快路径与最后
    /// 一步 rename 全部失败 → 必须把 .old 挪回正式名，绝不能让用户落到
    /// 「正式名没了、只剩 .running」的空档。
    ///
    /// 锁必须在 install_update 跑起来**之后**才拿：step 0 的 looks_like_exe 要
    /// 读 .new，抢在它前面锁会被判成损坏产物（BadDownload），测的就不是回滚了。
    #[test]
    #[ignore = "要真动文件 + 约 35s 重试窗口，手动跑"]
    fn live_install_rolls_back_when_new_file_is_locked() {
        let dir = sandbox("rollback");
        let final_path = dir.join("tui-project-manager.exe");
        let old_path = dir.join("tui-project-manager.exe.old");
        let new_file = fresh_new(&dir, "rollback");
        std::fs::copy(std::env::current_exe().unwrap(), &final_path).unwrap();
        let old_fp = fnv(&final_path);
        let new_fp = fnv(&new_file);

        // 锁形态很讲究：只放行**读**（share_mode = FILE_SHARE_READ），不放行
        // delete/write —— 与杀软扫描句柄同构。真用 share_mode(0)（谁都不许碰）
        // 会把 step 0 的 looks_like_exe 也挡掉，结果判成 BadDownload，测的就不是
        // 回滚了；这条测试要的正是「读得了、改不动」。
        const FILE_SHARE_READ: u32 = 0x1;
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&new_file)
            .expect("按杀软形态打开 .new 失败");
        assert!(looks_like_exe(&new_file), "锁只挡改名，读仍应放行");

        let msgs = Msgs::new();
        let outcome = install_update(&new_file, &final_path, &old_path, &|m| msgs.push(m));
        drop(holder); // 先放锁，再断言

        assert!(
            matches!(outcome, InstallOutcome::Occupied),
            "被占住只能报 Occupied 等用户重试"
        );
        assert_eq!(fnv(&final_path), old_fp, "回滚后正式名必须还是旧版且可用");
        assert_eq!(fnv(&new_file), new_fp, ".new 要保留供稍后重试，不能删");
        let log = msgs.joined();
        assert!(log.contains("已回滚保留旧版本"), "文案应说明已回滚：{log}");
        assert!(log.contains("持续被其他进程占用"), "应点名 .new 被占：{log}");
        println!("回滚后正式名 = {}", fnv(&final_path));
    }

    /// 坏产物：镜像返错误页 / 截断文件 → BadDownload，当场删掉（否则残留会被
    /// 下一轮 `-C -` 续传拼成更坏的 exe），正式名分毫不动。
    #[test]
    #[ignore = "要真动文件，手动跑"]
    fn live_install_rejects_corrupt_new() {
        let dir = sandbox("corrupt");
        let final_path = dir.join("tui-project-manager.exe");
        let old_path = dir.join("tui-project-manager.exe.old");
        std::fs::copy(std::env::current_exe().unwrap(), &final_path).unwrap();
        let old_fp = fnv(&final_path);
        // 体积过得去、头不对：镜像的错误页就是这样（几百 KB 的 HTML）。
        let junk = dir.join("tui-project-manager.exe.corrupt.new");
        std::fs::write(&junk, vec![b'<'; 512 * 1024]).unwrap();

        let msgs = Msgs::new();
        let outcome = install_update(&junk, &final_path, &old_path, &|m| msgs.push(m));

        assert!(matches!(outcome, InstallOutcome::BadDownload));
        assert!(!junk.exists(), "坏产物必须当场删掉，不能被续传拼坏");
        assert_eq!(fnv(&final_path), old_fp, "正式名不受影响");
        assert!(msgs.joined().contains("已自动换源重新下载"));
    }

    /// 端到端：真联网跑生产用的 `download_update`（当前代码里的下载链），把产物
    /// 装进沙箱的正式名。覆盖「tag → 资产直链 → 分片下载 → 校验 → 替换」整条，
    /// 且完全不动真实安装目录。
    #[test]
    #[ignore = "要真联网 + 真动文件，手动跑"]
    fn live_self_update_download_then_install() {
        use super::{download_update, fetch_latest_tag, SELF_REPO};
        let dir = sandbox("e2e");
        println!("--- 1) fetch_latest_tag({SELF_REPO}) ---");
        let tag = fetch_latest_tag(SELF_REPO).expect("取最新 tag 失败");

        println!("--- 2) download_update({tag}) ---");
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let printer = std::thread::spawn(move || {
            let mut last = 0u64;
            while let Ok((done, _total)) = rx.recv() {
                if done / (1 << 20) != last / (1 << 20) {
                    last = done;
                    println!("  进度 {}.{} MB", done >> 20, (done & 0xFF_FFFF) >> 16);
                }
            }
        });
        let new_file = PathBuf::from(download_update(&tag, &dir, tx, &cancel).expect("下载失败"));
        let _ = printer.join().unwrap();

        println!("  产物 = {} → {}", new_file.display(), fnv(&new_file));
        // 留一份产物：真实目录的替换验证（live_install_into_real_path）要拿它当
        // .new，而 install_update 会把 .new rename 走，不留就白下一趟。
        let kept = dir.parent().unwrap().join("downloaded_release.exe");
        std::fs::copy(&new_file, &kept).unwrap();
        println!("  产物留档 = {}", kept.display());
        assert!(looks_like_exe(&new_file), "下载产物必须是合法 exe");
        assert!(
            new_file.to_string_lossy().ends_with(".new"),
            "产物名必须是 .new 续传分片，实际 {}",
            new_file.display()
        );
        // 装反字段的历史坑：url 字段拿到纯文件名、name 字段拿到整条 URL，于是续传
        // 分片名长成「…/https://github.com/….exe.<fp>.c0.new」——带 '/' 与 ':'，
        // Windows 开不出这个文件（curl error 23，分片恒 0 字节 → 进度永远卡 0）。
        // 只查文件名：整条路径本来就带盘符冒号。
        let fname = new_file.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !fname.contains('/') && !fname.contains('\\') && !fname.contains(':'),
            "产物文件名不得含路径分隔符/盘符冒号，实际 {fname}"
        );
        assert!(!fname.contains("github.com"), "产物文件名不得含 URL 片段，实际 {fname}");

        println!("--- 3) install_update 装进沙箱正式名 ---");
        let final_path = dir.join("tui-project-manager.exe");
        let old_path = dir.join("tui-project-manager.exe.old");
        std::fs::copy(std::env::current_exe().unwrap(), &final_path).unwrap();
        let new_fp = fnv(&new_file);
        let msgs = Msgs::new();
        let outcome = install_update(&new_file, &final_path, &old_path, &|m| msgs.push(m));
        assert!(matches!(outcome, InstallOutcome::Done));
        assert_eq!(fnv(&final_path), new_fp, "正式名必须是刚下的那个 exe");
        println!("沙箱正式名 = {}", fnv(&final_path));
    }

    /// 对**真实安装目录**跑一遍替换（默认跳过）。要动真文件必须显式给两个环境
    /// 变量，且会在同目录留一份带时间戳的备份：
    ///   TUIPM_LIVE_INSTALL_FINAL=D:\agent\tui-project-manager.exe \
    ///   TUIPM_LIVE_INSTALL_NEW=<刚下载的 exe> \
    ///   cargo test --release -- --ignored live_install_into_real_path --nocapture
    /// 走的是生产同一函数（同样先 copy 正式名成 .old），失败自动回滚。
    #[test]
    #[ignore = "要真动真实安装目录，手动跑并显式给环境变量"]
    fn live_install_into_real_path() {
        let (Ok(final_env), Ok(new_env)) = (
            std::env::var("TUIPM_LIVE_INSTALL_FINAL"),
            std::env::var("TUIPM_LIVE_INSTALL_NEW"),
        ) else {
            println!("跳过：未给 TUIPM_LIVE_INSTALL_FINAL / TUIPM_LIVE_INSTALL_NEW");
            return;
        };
        let final_path = PathBuf::from(final_env);
        let new_file = PathBuf::from(new_env);
        assert!(looks_like_exe(&new_file), "指定的 .new 不是合法 exe");
        let old_path = final_path.with_extension("exe.old");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let backup = final_path.with_extension(format!("exe.bak{stamp}"));
        // 生产流程（start_download）同款备份，且多留一份带时间戳的（.old 会被
        // 下一次更新覆盖，验证期间不动它）。
        std::fs::copy(&final_path, &old_path).unwrap();
        std::fs::copy(&final_path, &backup).unwrap();
        println!("正式名 {} → {}", fnv(&final_path), fnv(&backup));
        let new_fp = fnv(&new_file);

        let msgs = Msgs::new();
        let outcome = install_update(&new_file, &final_path, &old_path, &|m| msgs.push(m));

        match outcome {
            InstallOutcome::Done => {
                assert_eq!(fnv(&final_path), new_fp);
                println!(
                    "装上成功，正式名 = {}；旧版备份 {}",
                    fnv(&final_path),
                    backup.display()
                );
            }
            _ => {
                // 没装上：正式名必须仍是旧版（回滚），否则从 backup 手工恢复。
                assert_eq!(
                    fnv(&final_path),
                    fnv(&backup),
                    "未装上且正式名不是旧版，请用 {} 手工恢复",
                    backup.display()
                );
                println!("未装上，正式名仍是旧版（已回滚）：{}", msgs.joined());
            }
        }
    }
}
