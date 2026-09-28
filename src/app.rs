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
/// 用户驱动回显例外（✂ 不吞任务真实输出）：键盘/IME/粘贴写 last_input_ms
/// （terminal.rs 投递 bytes_out 时记录）→ 直接引发的回显是用户驱动、不是任务
/// 在跑，其窗口内跳过 🔄 判定；滚动转发的 TUI 重绘回显记 last_scroll_ms 走
/// 自己的 500ms 短窗（见 SCROLL_ECHO_MS）。真实输出晚于各自窗口仍按
/// last_out 正常判 🔄——输入/滚动看日志期间页签照常实时刷新运行状态。
const INPUT_ACTIVE_MS: u64 = 1_500;
/// 滚动转发回显例外窗口：鼠标上报/备用屏路径滚轮转发 TUI 后，TUI 立即整屏
/// 重绘回显 → 刷新 last_output_ms → 若不加例外会误亮 🔄 3s。记 last_scroll_ms
/// 专用短窗（terminal.rs 滚动处理处写），只吞滚动驱动的这一下重绘；真实任务
/// 输出晚于窗口即照常判 🔄——持续滚动看日志时页签仍实时显示运行中。
/// 本地缓冲滚动（普通 shell）不产生 PTY 输出，不写此字段，完全不影响图标。
const SCROLL_ECHO_MS: u64 = 500;
/// 「执行完成」通知/闪烁需在 ✅ 稳定停留 2s（过滤 🔄↔✅ 间隙横跳）。
const DONE_STABLE_MS: u64 = 2_000;
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

/// 用 curl 请求 URL 并解析 JSON 中的 tag。extract 决定取哪个字段：GitHub
/// API 用 tag_name，jsDelivr 数据 API 用 versions[0].version。
/// 通用性说明：`-q` 让 curl 完全不读 ~/.curlrc（曾有残留 Clash 127.0.0.1:7897
/// 代理配置导致所有 curl 走指定端口、检查更新一律网络错误）——任何机器上的
/// 残留配置都不影响；环境 http_proxy/https_proxy 代理仍读（用户明确配置的
/// 代理放行，配合 PS/WinHTTP 系统代理通道，直连/代理双栈互补——全禁代理
/// 曾导致有加速器的机器下载不了）。connect_timeout / max_time（秒）由调用方
/// 决定。
fn fetch_tag_from_url(
    url: &str,
    connect_timeout: u64,
    max_time: u64,
    extract: fn(&serde_json::Value) -> Option<String>,
) -> Result<String, String> {
    let mut cmd = std::process::Command::new(curl_bin());
    let ct = connect_timeout.to_string();
    let mt = max_time.to_string();
    cmd.args([
        "-q", // 忽略 .curlrc / _curlrc，防用户机器上的残留代理端口
        "-s", "-f", "--connect-timeout", &ct, "--max-time", &mt, "--ssl-no-revoke",
        "-H", "User-Agent: TUIProjectManager",
        url,
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output().map_err(|e| format!("启动 curl 失败: {e}"))?;
    if !output.status.success() {
        return Err(format!("HTTP {}", output.status.code().unwrap_or(0)));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "无法解析 JSON 响应".to_string())?;
    extract(&v).ok_or_else(|| "返回结构不符合预期".to_string())
}

/// curl 拉取 URL 到内存（-q 忽略 .curlrc 残留代理，参数与 fetch_tag_from_url 一致）。
/// 新增的资产解析（工具更新的 API/HTML 源）复用此通道，不重复造 curl 命令。
fn curl_get(url: &str, connect_timeout: u64, max_time: u64) -> Result<Vec<u8>, String> {
    let mut cmd = std::process::Command::new(curl_bin());
    let ct = connect_timeout.to_string();
    let mt = max_time.to_string();
    cmd.args([
        "-q", "-s", "-f", "-L", "--connect-timeout", &ct, "--max-time", &mt, "--ssl-no-revoke",
        "-H", "User-Agent: TUIProjectManager",
        url,
    ]);
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

/// HTML 302 重定向取 tag（github.com /releases/latest 重定向到
/// /releases/tag/<tag>，免 API 限流）。同样 -q 直连、无代理无端口。
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
        url,
    ]);
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
    let redirect = String::from_utf8_lossy(&o.stdout);
    match redirect.find("/releases/tag/") {
        Some(pos) => {
            let tag = redirect[pos + "/releases/tag/".len()..].trim().to_string();
            if tag.is_empty() {
                Err("HTML 返回空 tag".to_string())
            } else {
                Ok(tag)
            }
        }
        None => Err(format!("HTML 未解析出 tag（redirect={redirect}）")),
    }
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

/// PowerShell Invoke-RestMethod 拉 JSON 取 tag。独立网络栈（WinHTTP/Schannel），
/// 吃系统代理（Steam++/加速器系统代理模式可救直连被墙；不设 DefaultWebProxy=$null）
/// ——curl 在这个目标机上会 ACCESS_VIOLATION 启动即崩，PS 通道是「别的办法绕过」
/// 的主力源（实测直连 api.github.com ~1.1s 返回 tag）。
#[cfg(windows)]
fn ps_fetch_tag(url: &str, timeout_secs: u64) -> Result<String, String> {
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         $ErrorActionPreference='Stop'; \
         $r = Invoke-RestMethod -Uri '{url}' -Headers @{{'User-Agent'='TUIProjectManager'}} -TimeoutSec {t}; \
         $r | ConvertTo-Json -Depth 10",
        url = url,
        t = timeout_secs,
    );
    let out = ps_run(&script)?;
    let v: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("PS 响应解析失败: {e}"))?;
    v["tag_name"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "PS 返回结构不符合预期".to_string())
}

/// PowerShell Invoke-WebRequest 取 GitHub releases/latest 页（自动跟随 302）后
/// 从响应体里挖 /releases/tag/<tag>。
#[cfg(windows)]
fn ps_fetch_html_tag(url: &str, timeout_secs: u64) -> Result<String, String> {
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         (Invoke-WebRequest -Uri '{url}' -Headers @{{'User-Agent'='TUIProjectManager'}} -TimeoutSec {t} -UseBasicParsing).Content",
        url = url,
        t = timeout_secs,
    );
    let body = ps_run(&script)?;
    match body.find("/releases/tag/") {
        Some(pos) => {
            let tag = body[pos + "/releases/tag/".len()..]
                .trim()
                .to_string();
            if tag.is_empty() {
                Err("PS HTML 返回空 tag".to_string())
            } else {
                Ok(tag)
            }
        }
        None => Err("PS HTML 未解析出 tag".to_string()),
    }
}

/// 本软件自身的 GitHub 仓库（检查自身更新用）。
const SELF_REPO: &str = "qq458249269/TUIProjectManager";

/// 拉取某个 repo 的最新 tag。所有源**并发**探查、先到先得：GH_MIRRORS 国内镜像、
/// jsDelivr 数据 API（Fastly CDN，大陆友好、不依赖 GitHub 可达性）、
/// GitHub HTML 302（免 API 限流）、GitHub API（可能限流）同时发起，
/// 任一源在自身超时内返回有效 tag 即胜出——坏源零成本跳过，总耗时封顶在
/// 最快源的超时内（≈6s），不再逐源串行、最坏吃满全表，也顺带防限流误报。
/// 全程 -q 直连、不读任何代理配置与端口。自更新与 pi/opencode 检查共用此源表。
fn fetch_latest_tag(repo: &str) -> Result<String, String> {
    let api_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let html_url = format!("https://github.com/{repo}/releases/latest");
    let jd_url = format!("https://data.jsdelivr.com/v1/packages/gh/{repo}");

    // 每个源：描述、URL、取 tag 方式（JSON 提取器或 HTML 302）、超时。
    struct Src {
        desc: &'static str,
        url: String,
        html: bool,
        ps: bool, // 走 PowerShell WinHTTP 通道（curl 崩溃/失败时的绕过源）
        extract: fn(&serde_json::Value) -> Option<String>,
        ct: u64,
        mt: u64,
    }
    let gh_api: fn(&serde_json::Value) -> Option<String> =
        |v| v["tag_name"].as_str().map(str::to_string);
    let mut sources: Vec<Src> = Vec::new();
    for m in GH_MIRRORS {
        sources.push(Src {
            desc: m,
            url: format!("{m}{api_url}"),
            html: false,
            ps: false,
            extract: gh_api,
            ct: 3,
            mt: 6,
        });
    }
    // jsDelivr 数据 API：实返回 {"tags":{},"versions":[{version,…}]}——tags 恒为空
    // 对象（只收 semver 标签），最新版在 versions[0].version。旧实现读 tags[]
    // 永远取不到 → 该源静默必败，等于少一个 CDN 主力源。
    sources.push(Src {
        desc: "jsDelivr",
        url: jd_url,
        html: false,
        ps: false,
        extract: |v: &serde_json::Value| -> Option<String> {
            v["versions"]
                .as_array()?
                .first()?["version"]
                .as_str()
                .map(str::to_string)
                .or_else(|| v["tags"].as_array()?.first()?.as_str().map(str::to_string))
        },
        ct: 4,
        mt: 8,
    });
    sources.push(Src { desc: "GitHub HTML", url: html_url.clone(), html: true, ps: false, extract: gh_api, ct: 6, mt: 12 });
    sources.push(Src { desc: "GitHub API", url: api_url.clone(), html: false, ps: false, extract: gh_api, ct: 6, mt: 12 });

    // PowerShell 通道（独立 WinHTTP 网络栈）：curl 失败/崩溃时仍可检查更新。
    // 实测本机直连 api.github.com 1.1s 可达；不读任何代理端口。
    #[cfg(windows)]
    {
        sources.push(Src {
            desc: "PS API",
            url: api_url.clone(),
            html: false,
            ps: true,
            extract: gh_api,
            ct: 8,
            mt: 15,
        });
    }
    #[cfg(windows)]
    {
        sources.push(Src {
            desc: "PS HTML",
            url: html_url.clone(),
            html: true,
            ps: true,
            extract: gh_api,
            ct: 8,
            mt: 15,
        });
    }

    let n = sources.len();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<Result<(String, String), String>>();
    for s in sources {
        let tx = tx.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            if done.load(Ordering::Relaxed) {
                return; // 已有源胜出，本线程不再发消息
            }
            let tag = if s.ps {
                #[cfg(windows)]
                {
                    if s.html {
                        ps_fetch_html_tag(&s.url, s.mt)
                    } else {
                        ps_fetch_tag(&s.url, s.mt)
                    }
                }
                #[cfg(not(windows))]
                {
                    // 非 Windows 无 PS 源（构造时不会插入）。
                    Err("PS 源仅在 Windows 可用".to_string())
                }
            } else if s.html {
                fetch_tag_html(&s.url, s.ct, s.mt)
            } else {
                fetch_tag_from_url(&s.url, s.ct, s.mt, s.extract)
            };
            let msg = match &tag {
                Ok(t) => format!("检查更新 {} 成功 → tag {t}", s.desc),
                Err(e) => format!("检查更新 {} 失败: {e}", s.desc),
            };
            log_update(&msg);
            if tag.is_ok() {
                done.store(true, Ordering::Relaxed);
            }
            let _ = tx.send(tag.map(|t| (s.desc.to_string(), t)));
        });
    }
    let mut errors: Vec<String> = Vec::new();
    for _ in 0..n {
        match rx.recv() {
            Ok(Ok((_desc, tag))) => {
                done.store(true, Ordering::Relaxed);
                log_update(&format!("检查更新 {repo} 最新 tag: {tag}"));
                return Ok(tag);
            }
            Ok(Err(e)) => errors.push(e),
            Err(_) => break,
        }
    }
    // 具体失败原因已逐条 log_update；这里只给用户一句可行动的提示
    //（逐源错误已写日志，展开只会把状态栏撑成一条长串）。
    log_update(&format!("检查更新 {repo} 全部源失败: {}", errors.join("；")));
    Err("网络错误（镜像与直连、jsDelivr 均失败，请检查网络连接或加速工具如 Steam++）".to_string())
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
        format!("发现新版本 {tag}，点击下方按钮下载")
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
    // 从 GitHub Release 里找 exe 直链。优先级：HTML 页面（github.com CDN，
    // 无限流、比 api.github.com 更易连通）→ 直拼直链（零请求保底）→ GitHub
    // API 最低（仅 HTML 拿不到时兜底，成功可补字节数）。
    // 旧实现 API 优先：限流/被墙时每次下载都先撞 API 失败（HTTP 22），错误
    // 汇总里「源① API」长期打头误导；现在 API 降为最低优先级，curl 带 -f
    // 失败时透出真实报错，HTML 兜底成功则照常下载。
    let mut exe_info: Option<(String, String, u64)> = None; // (url, 文件名, 字节数)
    let mut api_err: Option<String> = None;
    // 源②主路径：HTML 页面（github.com CDN 无限流）。HTML 成功即用，
    // total=0（页面不含字节数，进度按已下载字节显示）。
    // ponytail: 要百分比进度可另发一次 HEAD 取 Content-Length，或 API 仅补 size。
    let page_url = format!(
        "https://github.com/qq458249269/TUIProjectManager/releases/tag/{tag}"
    );
    let mut page_cmd = std::process::Command::new(curl_bin());
    page_cmd.args([
        "-q", "-s", "-L", "-f", "--connect-timeout", "8", "--ssl-no-revoke",
        "-H", "User-Agent: TUIProjectManager",
        &page_url,
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        page_cmd.creation_flags(0x08000000);
    }
    match page_cmd.output() {
        Ok(o) if o.status.success() => {
            let html = String::from_utf8_lossy(&o.stdout);
            if let Some(info) = exe_asset_from_html(&html, tag) {
                exe_info = Some(info);
            } else {
                // GitHub 资产列表是 lazy-load 的 expanded_assets fragment，
                // 初始 HTML 无下载链接 → 解析必失败，转 API/直拼。
                log_update(&format!(
                    "下载 源② HTML 成功但未解析出 exe 直链（页面 {} 字节，资产懒加载），转 API/直拼",
                    html.len()
                ));
            }
        }
        Ok(o) => {
            log_update(&format!(
                "下载 源② HTML HTTP {} 失败，转 API/直拼",
                o.status.code().unwrap_or(0)
            ));
        }
        Err(e) => log_update(&format!("下载 源② HTML: 启动 curl 失败: {e}")),
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
            &api_url,
        ]);
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
                        exe_info = Some((
                            url.to_string(),
                            name.to_string(),
                            a["size"].as_u64().unwrap_or(0),
                        ));
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
        Some((url, name, size)) => (vec![(url, name)], size),
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
    // ── 候选下载链：国内镜像 + PS WinHTTP + curl 直链，每个文件名下全量并发 ──
    // 竞速第一个到达（见 download_race）：坏源/停滞源零成本跳过，速度地板判死
    // 僵尸源；旧实现逐链串行（镜像×8 → PS → curl 直连按序等待），8 个死镜像
    // 的 connect 超时（8s 各）累计 64s+ 才轮到直链——正是「镜像源下载缓慢」
    // 的根因，已由并发取代。单个 fallback 失败再试下一个候选文件名。
    let mut parts: Vec<String> = Vec::new();
    for (url, name) in fallback {
        let mut candidates: Vec<(String, bool)> = Vec::new(); // (url, 走 PS)
        for mirror in GH_MIRRORS {
            candidates.push((format!("{mirror}{url}"), false));
        }
        #[cfg(windows)]
        candidates.push((url.clone(), true));
        #[cfg(not(windows))]
        candidates.push((url.clone(), false));
        // Windows 下 PS 失败（无 PowerShell 等）时仍有 curl 直链保底：
        #[cfg(windows)]
        candidates.push((url.clone(), false));
        log_update(&format!(
            "下载 {} 并发尝试 {} 个候选链（tag={tag}，total={total}）",
            name,
            candidates.len()
        ));
        // 用户取消时立即停止本轮竞速。
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("下载已取消".to_string());
        }
        match download_race(
            &name,
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
/// 慢/被墙，镜像 CDN 缓存、延迟低；检查更新时所有镜像**并发**探查、先到
/// 先得（挂了零成本跳过）；下载仍逐链尝试、挂了自动跳到下一个，全挂才回退
/// 原始直链。列表换成当前可用即可，多放几个零成本、坏节点自动跳过。
// 镜像列表为「加速前缀池」而非命运列表：检查更新/下载全部**并发**批量发起、
// 先到先得，连接失败与坏文件（错误页/截断）都会被即时剔除记错并继续等其余
// 源——所以越多越稳，坏节点零成本，谁响应快谁胜出，天然满足「实时性高」。
// 池内包含踩点验证过数量级的常见国内加速：热门前缀代理、jsDelivr CDN 之外的
// 各家 gh-proxy 系。个别历史 403/超时/证书过期的源保留在池里：连通状态随时
// 变化，竞速机制下不必人工汰换，哪家活了立即自动启用。
const GH_MIRRORS: &[&str] = &[
    "https://gh-proxy.com/",   // 热门前缀代理
    "https://gh-proxy.net/",   // 同系备用
    "https://ghps.cc/",        // 极速代理
    "https://ghfast.top/",     // gh-proxy 系，状态多变，竞速下自动甄别
    "https://mirror.ghproxy.com/", // ghproxy 系老牌
    "https://ghproxy.net/",    // ghproxy 系
    "https://gh.llkk.cc/",     // 备用代理
    "https://github.moeyy.xyz/", // moeyy 加速
];

/// 用 curl（或 PowerShell WinHTTP）把单个 URL 下载到 dest_dir/{asset_name}.new，
/// 轮询文件大小报告进度。返回 Ok(下载文件路径) 或 Err(具体失败原因)。
/// 单个文件名下的候选链**并发竞速**下载（与 fetch_latest_release 同思路）：
/// 全部候选（国内镜像 + PS WinHTTP + curl 直链）同时发起，各自写独立临时
/// 文件 dest_dir/{name}.c{idx}.new，第一个成功完成的胜出并 promote 为
/// {name}.new，其余就地 kill。坏源/停滞源零成本跳过：--connect-timeout 8 挡
/// 连接挂死，--speed-limit 4096 --speed-time 8 判死持续 <4KB/s 达 8s 的僵尸
/// 源（不再让一个死镜像独占整条串行下载）。失败/取消保留 .c{idx}.new 供
/// 下次 -C - 续传；胜出 promote 若被杀软短持有则退避重试。validate 是产物
/// 校验（自更新 = looks_like_exe，pi/opencode = looks_like_zip），错误页/
/// 截断文件永不胜出。
/// 返回 Ok(下载文件路径) 或 Err(所有候选失败的聚合)。
/// ponytail: 若 release 数日后镜像纷纷清缓存变慢，可给镜像档位降权或按历史
/// 延迟排序重试；触及率低，暂不加。
fn download_race(
    asset_name: &str,
    candidates: Vec<(String, bool)>, // (url, 走 PS WinHTTP)
    total: u64,
    dest_dir: &Path,
    progress_tx: &std::sync::mpsc::Sender<(u64, u64)>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    validate: fn(&Path) -> bool,
) -> Result<String, String> {
    let new_name = format!("{asset_name}.new");
    let dest_path = dest_dir.join(&new_name);
    let mut children: Vec<Option<std::process::Child>> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for (i, (url, ps)) in candidates.iter().enumerate() {
        let tmp = dest_dir.join(format!("{asset_name}.c{i}.new"));
        let tmp_str = tmp.to_str().unwrap_or("update.exe.new").replace('\'', "''");
        let mut cmd = if *ps {
            let mut c = std::process::Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command"]);
            // PS/WinHTTP 通道吃系统代理（Steam++/Clash 系统代理模式可救大陆
            // 直连被墙；不设 DefaultWebProxy=$null——远端曾禁代理导致下载不了）。
            let script = format!(
                "$ErrorActionPreference='Stop'; \
                 Invoke-WebRequest -Uri '{url}' -Headers @{{'User-Agent'='TUIProjectManager'}} \
                 -TimeoutSec 120 -OutFile '{dest}' -UseBasicParsing",
                url = url.replace('\'', "''"),
                dest = tmp_str,
            );
            c.arg(script);
            c
        } else {
            let mut c = std::process::Command::new(curl_bin());
            c.args([
                "-q", "-L", "-f", "--connect-timeout", "8", "--ssl-no-revoke",
                // 传输停滞判死：持续 <4KB/s 达 8s 中止本候选，让位其余候选。
                "--speed-limit", "4096", "--speed-time", "8",
                // 不带 --noproxy：-q 已禁 .curlrc 残留代理（历史坑 3a70473），
                // 依仍读环境 http_proxy/https_proxy 与 PS 系统代理互补。
                "-H", "User-Agent: TUIProjectManager",
                "-o", tmp.to_str().unwrap_or("update.exe.new"),
            ]);
            // 断点续传：上次遗留的 .c{i}.new 非空则续传（PS 无续传，直接覆盖重下）。
            if std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) > 0 {
                c.arg("-C").arg("-");
            }
            c.arg(url);
            c
        };
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW，不闪黑窗
        }
        match cmd.spawn() {
            Ok(ch) => children.push(Some(ch)),
            Err(e) => {
                errs.push(format!("{url}: 启动下载失败 {e}"));
                children.push(None);
            }
        }
    }
    loop {
        // 取消：kill 全部，保留 .c{idx}.new 供下次续传。
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            for c in children.iter_mut().flatten() {
                let _ = c.kill();
            }
            return Err("下载已取消".to_string());
        }
        // 进度 = 各临时文件当前大小的最大值（领先者即竞速胜出者的身形）。
        let mut reported = 0u64;
        for i in 0..children.len() {
            reported = reported.max(
                std::fs::metadata(dest_dir.join(format!("{asset_name}.c{i}.new")))
                    .map(|m| m.len())
                    .unwrap_or(0),
            );
        }
        let _ = progress_tx.send((reported, total));
        std::thread::sleep(std::time::Duration::from_millis(200));
        // 扫描已结束的子进程：首个成功者胜出。
        for (i, slot) in children.iter_mut().enumerate() {
            let Some(ch) = slot.as_mut() else { continue };
            match ch.try_wait() {
                Ok(Some(st)) if st.success() => {
                    let tmp = dest_dir.join(format!("{asset_name}.c{i}.new"));
                    // 源返回了非 exe 产物（错误页 HTML / 截断文件）：视作该候选失败，
                    // 删其临时文件后继续等其余候选——坏源永不胜出，杜绝
                    // 「替换失败: 下载文件损坏」反复出现（原本错误页体积小、下载最快，
                    // 总是抢在真源前面 promote 成功）。
                    if !validate(&tmp) {
                        let _ = std::fs::remove_file(&tmp);
                        errs.push(format!("{}: 文件损坏（产物校验失败，疑似错误页或截断）", candidates[i].0));
                        *slot = None;
                        continue;
                    }
                    // kill 其余所有候选，删除其残留临时文件（本次已废弃）。
                    for (j, other) in children.iter_mut().enumerate() {
                        if j == i { continue; }
                        if let Some(o) = other.as_mut() {
                            let _ = o.kill();
                        }
                        other.take();
                        if let Some(p) = dest_dir
                            .join(format!("{asset_name}.c{j}.new"))
                            .to_str()
                        {
                            let _ = cleanup_file(p);
                        }
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
                    let final_size = std::fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0);
                    let _ = progress_tx.send((final_size, total));
                    return Ok(dest_path.to_string_lossy().into_owned());
                }
                Ok(Some(st)) => {
                    // 失败（HTTP 非零、限流、停滞判死）→ 记错误，临时文件保留供续传。
                    let code = st.code().map(|c| c.to_string()).unwrap_or_else(|| "信号终止".to_string());
                    errs.push(if candidates[i].1 {
                        format!("PS 通道失败（{code}）")
                    } else {
                        format!("镜像/直链失败（{code}）")
                    });
                    slot.take();
                }
                Ok(None) => {}
                Err(e) => {
                    errs.push(format!("{e}"));
                    slot.take();
                }
            }
        }
        if children.iter().all(Option::is_none) {
            return Err(if errs.is_empty() {
                "无候选可启动".to_string()
            } else {
                errs.join("；")
            });
        }
    }
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
    //    竞速层已前置剔除坏源，这里双保险；失败上层会自动换源重下，无需用户手动干预。
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

/// 从 GitHub Release 标签页 HTML 里抽全部 assets 直链，返回 (文件名, 下载 URL)。
/// github.com 的资产列表是 lazy-load 的 expanded_assets fragment，标签页初始
/// HTML 往往一个链接都没有（实测 pi/opencode 均如此）→ 解析必失败，只能当兜底源。
fn assets_from_html(html: &str, tag: &str) -> Vec<(String, String)> {
    const NEEDLE: &str = "releases/download/";
    let mut from = 0;
    let mut out: Vec<(String, String)> = Vec::new();
    while let Some(rel) = html[from..].find(NEEDLE) {
        let start = from + rel + NEEDLE.len();
        let rest = &html[start..];
        let end = rest.find(['\'', '\"', '<', '?', '\n']).unwrap_or(rest.len());
        // 路径形如 {tag}/{xxx.exe}
        let parts: Vec<&str> = rest[..end].split('/').collect();
        if parts.len() == 2 && parts[0] == tag {
            out.push((
                parts[1].to_string(),
                format!("https://github.com/{NEEDLE}{}/{}", parts[0], parts[1]),
            ));
        }
        from = start;
    }
    out
}

/// 从 GitHub Release 标签页 HTML 里找 exe 下载直链（API 限流/被墙时兜底）。
/// 返回 (exe 文件名, 下载 URL, 0)；HTML 不含字节数，进度按已下载字节算。
fn exe_asset_from_html(html: &str, tag: &str) -> Option<(String, String, u64)> {
    assets_from_html(html, tag)
        .into_iter()
        .find(|(_, name)| name.ends_with(".exe"))
        .map(|(name, url)| (name, url, 0))
}

// ── 外部工具（pi / opencode）更新 ──────────────────────────────────────
//
// 自更新那套流水线（多源并发探查 → 镜像竞速下载 .new → 解压 → install_update
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

/// 拉 GitHub Release 资产表：name → (browser_download_url, 字节数)。
/// 带 size 是为了下载进度能显示百分比（HTML 源拿不到字节数）。
fn gh_assets(
    repo: &str,
    tag: &str,
) -> Result<std::collections::HashMap<String, (String, u64)>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
    let body = curl_get(&url, 8, 15)?;
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("API 响应解析失败: {e}"))?;
    let mut out = std::collections::HashMap::new();
    for a in v["assets"].as_array().into_iter().flatten() {
        if let (Some(n), Some(u)) = (a["name"].as_str(), a["browser_download_url"].as_str()) {
            out.insert(
                n.to_string(),
                (u.to_string(), a["size"].as_u64().unwrap_or(0)),
            );
        }
    }
    Ok(out)
}

/// 下载工具 zip 产物到 {install_dir}/.{id}-update.zip.new。直链解析优先级：
/// 源①GitHub API（资产表完整 + 字节数）→ 源②标签页 HTML（API 被墙/限流时捡
/// releases/download/{tag}/{候选名}；实测这两个仓库的资产列表懒加载，HTML
/// 多半解析不出来，属于兜底）→ 源③直拼直链（零请求，永远可用）。解析出直链
/// 后按「国内镜像 + PS 通道 + curl 直链」全量并发竞速（与自更新同一套
/// download_race，只是校验函数换 looks_like_zip）。
/// 返回 (zip 路径, 资产字节数)。
fn download_tool_archive(
    spec: &ToolSpec,
    tag: &str,
    dest_dir: &Path,
    progress_tx: &std::sync::mpsc::Sender<(u64, u64)>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(PathBuf, u64), String> {
    let names: Vec<String> = spec
        .asset_tpls
        .iter()
        .map(|t| t.replace("{arch}", win_arch()))
        .collect();
    // 源①API：一次请求拿整张资产表，筛出候选名（保留模板顺序）。
    let mut resolved: Vec<(String, String, u64)> = Vec::new();
    let mut api_err: Option<String> = None;
    match gh_assets(spec.repo, tag) {
        Ok(table) => {
            for n in &names {
                if let Some((url, size)) = table.get(n) {
                    resolved.push((n.clone(), url.clone(), *size));
                }
            }
        }
        Err(e) => {
            log_update(&format!("工具下载 API 失败: {e}"));
            api_err = Some(e);
        }
    }
    // 源②HTML 兜底：只补 API 没解析出的候选名。
    let missing: Vec<String> = names
        .iter()
        .filter(|n| !resolved.iter().any(|(rn, _, _)| rn == *n))
        .cloned()
        .collect();
    if !missing.is_empty() {
        let page = format!("https://github.com/{}/releases/tag/{}", spec.repo, tag);
        match curl_get(&page, 8, 15) {
            Ok(b) => {
                let html = String::from_utf8_lossy(&b);
                let all = assets_from_html(&html, tag);
                for n in &missing {
                    if let Some((_, url)) = all.iter().find(|(hn, _)| hn == n) {
                        resolved.push((n.clone(), url.clone(), 0));
                    }
                }
            }
            Err(e) => log_update(&format!("工具下载 HTML 源失败: {e}")),
        }
    }
    // 源③直拼：仍缺的候选名直接按约定 URL 拼出来（零请求保底）。
    for n in &names {
        if !resolved.iter().any(|(rn, _, _)| rn == n) {
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
    // 逐个候选文件名试：每个文件名下的所有下载链（镜像 × 8 + PS + 直链）
    // 并发竞速，全部失败再换下一个文件名。
    let mut errs: Vec<String> = Vec::new();
    for (name, url, total) in resolved {
        if cancel.load(Ordering::Relaxed) {
            return Err("下载已取消".to_string());
        }
        let mut candidates: Vec<(String, bool)> = Vec::new();
        for mirror in GH_MIRRORS {
            candidates.push((format!("{mirror}{url}"), false));
        }
        #[cfg(windows)]
        candidates.push((url.clone(), true));
        #[cfg(not(windows))]
        candidates.push((url.clone(), false));
        // Windows 下 PS 失败（无 PowerShell 等）时仍有 curl 直链保底：
        #[cfg(windows)]
        candidates.push((url.clone(), false));
        let asset_name = format!(".{}-update.zip", spec.id);
        log_update(&format!(
            "工具下载 {} {name} 并发尝试 {} 个候选链（tag={tag}）",
            spec.label,
            candidates.len()
        ));
        match download_race(
            &asset_name,
            candidates,
            total,
            dest_dir,
            progress_tx,
            cancel,
            looks_like_zip,
        ) {
            Ok(p) => {
                log_update(&format!("工具下载 成功：{} {name} → {p}", spec.label));
                return Ok((PathBuf::from(p), total));
            }
            Err(e) => {
                log_update(&format!("工具下载 失败：{} {name}：{e}", spec.label));
                errs.push(format!("{name}: {e}"));
            }
        }
    }
    if let Some(e) = api_err {
        errs.push(format!("源① API: {e}"));
    }
    Err(format!("所有下载源失败：{}", errs.join("；")))
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

/// 检查单个工具（pi / opencode）的新版本：按 tool_dirs（默认本软件所在目录）
/// → PATH 定位 exe → `--version` 读本地版本 → 复用自更新的多源并发探查拿
/// 最新 tag → version_newer 比较。找不到 exe（未安装）时**仍然给出下载入口**：
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
    let mut reuse = {
        let staged = stage.join(spec.exe_name);
        staged.is_file() && looks_like_exe(&staged)
    };
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
                        "查询最新版本号未成功（第 {attempt} 次）: {e}，3 秒后自动重试…"
                    ));
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    attempt += 1;
                    continue;
                }
            }
        }
        let tag = tag.as_deref().unwrap_or_default();
        // 1) 下载 zip（镜像并发竞速，产物 looks_like_zip 校验）；复用暂存时跳过。
        let zip: Option<std::path::PathBuf> = if reuse {
            reuse = false;
            sink("复用上次已解压的文件，直接重试替换…");
            None
        } else {
            match download_tool_archive(spec, tag, &install_dir, &progress_tx, &cancel) {
                Ok((zip, _total)) => Some(zip),
                Err(e) => {
                    if cancel.load(Ordering::Relaxed) {
                        sink("下载已取消");
                        return;
                    }
                    sink(&format!(
                        "下载失败（第 {attempt} 次）: {e}，3 秒后自动重试…"
                    ));
                    std::thread::sleep(std::time::Duration::from_secs(3));
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
                sink(&format!(
                    "解压失败（第 {attempt} 次）: {e}，3 秒后自动重试…"
                ));
                std::thread::sleep(std::time::Duration::from_secs(3));
                attempt += 1;
                continue;
            }
        }
        // 3) 替换：备份旧 exe 为 .old（copy，运行中的映像也能读），再走
        //    install_update 的快路径/慢路径/回滚三段式。
        let staged_exe = stage.join(spec.exe_name);
        if !staged_exe.is_file() {
            let _ = std::fs::remove_dir_all(&stage);
            drop_zip(&zip);
            sink(&format!(
                "压缩包内没有 {}（包结构与预期不符），3 秒后换源重试…",
                spec.exe_name
            ));
            std::thread::sleep(std::time::Duration::from_secs(3));
            attempt += 1;
            continue;
        }
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
                    let fails = sync_tree(&stage, &install_dir, spec.exe_name);
                    if fails > 0 {
                        log_update(&format!("工具 {label} 同步 {fails} 个文件失败"));
                    }
                }
                // 5) 清理暂存（下载包已装完，不再需要续传）。
                let _ = std::fs::remove_dir_all(&stage);
                drop_zip(&zip);
                log_update(&format!(
                    "工具 {label} 更新完成：{tag} → {final_exe:?}（旧版已备份 {old_exe:?}）"
                ));
                let _ = tx.send((
                    idx,
                    ToolEvent::Done {
                        msg: if first_install {
                            // 具体路径/是否已入启动命令由 UI 拼（它才知道配置改动
                            // 结果）；这里只给一句短消息。
                            format!("{label} {tag} 已安装")
                        } else {
                            format!("{label} 已更新到 {tag}，重新启动 {label} 即可生效")
                        },
                        // 直接采信装上去的 tag，不再去跑 `--version`：某些版本
                        // 改了输出格式抠不出数字，那样按钮会永远停在“有新版本”。
                        version: tag.trim_start_matches('v').to_string(),
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
                // 解压出的 exe 不合法：清掉 .new/暂存残留，避免 -C - 续传拼坏，
                // 3 秒后整链重来（坏源已在竞速层剔除）。
                drop_zip(&zip);
                let _ = std::fs::remove_dir_all(&stage);
                reuse = false;
                sink(&format!(
                    "下载到损坏文件，已自动换源重新下载（第 {attempt} 次）"
                ));
                std::thread::sleep(std::time::Duration::from_secs(3));
                attempt += 1;
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
    /// 退出时设置页签开着：启动时在原位置插回（满页签栏索引，仅恢复时消费一次）。
    restore_settings_pos: Option<usize>,
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


/// 页签状态图标判定（纯函数，便于测试）。仅凭终端内容判定：
/// 已退出 → ❌；启动加载中 → 🔄；最近 OUTPUT_END_MS（3s）内有输出 → 🔄；
/// 输出停止 ≥3s 且有可查看内容且未查看 → ✅（完成/待查看）；否则空。
/// 滚动/翻页只改视口、不写 last_output_ms → 不计更新状态；周期重绘、CPU
/// 采样、锁存等后台「固定刷新」全部退出判定，3 秒无内容即完成。
/// 唯一例外：用户驱动回显（last_input/last_scroll 距现在窗口内）——最近
/// 1.5s 内输过键（last_input_ms）或 500ms 内转发过滚轮（last_scroll_ms）给
/// TUI，其直接引发的重绘回显不算任务在跑，跳过 🔄；✅ 判定（基于 ≥3s 无新
/// 内容）与空不受影响，任务完成后开始敲下一行命令时 ✅ 保持可见。
/// 两个窗口互不兼容：键盘输入回显给 1.5s（打字可能持续），滚动重绘是
/// 一次性输出给 500ms；真实输出晚于各自窗口即照常判 🔄。
/// ever_output：是否有任何输出块（含动画）。零输出会话不因 last_output_ms
/// 初始化为 spawn 时刻而假闪 🔄，🔄 只属于真实内容驱动/加载态。
fn tab_icon(
    exited: bool,
    loading: bool,
    ever_output: bool,
    count: u32,
    viewed: bool,
    last_out: u64,
    now_ms: u64,
    last_input: u64,
    last_scroll: u64,
) -> Option<&'static str> {
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
    // 有内容（最近一块输出距今 ≤3s）→ 运行中。动画块也刷新 last_output_ms，
    // spinner/周期重绘期间保持 🔄；本地滚动/翻页不产生输出，不会点亮它。
    // ever_output 门：从未收到任何输出的会话（last_output_ms 仍是 spawn 的
    // 初始值）不因「初始即新鲜」假闪 🔄，启动加载由 loading 分支负责。
    if !typing
        && !scroll_echo
        && ever_output
        && now_ms.saturating_sub(last_out) <= OUTPUT_END_MS
    {
        return Some("🔄");
    }
    // 无内容 ≥3s → 完成；有实质输出（count>0）且用户未查看才亮 ✅。
    // 陈旧判据与 🔄 互补：打字期内新鲜回显不落 ✅，只能走空（旧代码语义）。
    if count > 0 && !viewed && now_ms.saturating_sub(last_out) > OUTPUT_END_MS {
        return Some("✅");
    }
    // 空：无内容可看 / 已查看过。
    None
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
fn restore_coords(kinds: &[TabKind], current: usize) -> (usize, usize) {
    let settings_pos = kinds
        .iter()
        .position(|k| *k == TabKind::Settings)
        .map(|pos| {
            1 + kinds
                .iter()
                .skip(1)
                .take(pos.saturating_sub(1))
                .filter(|k| **k == TabKind::Alive)
                .count()
        })
        .unwrap_or(1);
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
            restore_settings_pos: None,
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
            app.restore_settings_pos = Some(saved_tabs.settings_pos.max(1));
        }
        if !app.pending_restore.is_empty() || app.restore_settings_pos.is_some() {
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
        // 设置页签也记录：退出时开着则启动时在相同位置恢复（见构造器 restore_settings_pos）。
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


    fn open_settings(&mut self) {
        self.settings_command = self.config.settings.tui_command.clone();
        self.settings_commands = self.config.settings.tui_commands.clone();
        self.settings_new_command.clear();
        self.settings_tool_dirs = self.config.settings.tool_paths.clone();
        self.settings_new_tool_dir.clear();
        // 如果已有一个设置页签，跳转过去而不是重复添加。
        if let Some(idx) = self.tabs.iter().position(|t| matches!(t, Tab::Settings)) {
            self.current = idx;
        } else {
            self.tabs.push(Tab::Settings);
            self.current = self.tabs.len() - 1;
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

    /// 检查 pi / opencode 的新版本（与自更新同一套多源并发探查）。
    /// 一个后台线程里按 TOOL_SPECS 顺序串行跑：每个都是「先到先得」，正常
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
    /// 目录）。流程与自更新同构：镜像竞速下 zip 到 .new → 解压到暂存目录 →
    /// 备份旧 exe 为 .old → install_update 三段式替换 → 同步其余文件（pi）→
    /// 清理暂存。失败 3 秒后自动重试，坏源在 download_race 内已被剔除；用户
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
        // 进度中继：download_race 往 ptx 写 (已下载, 总数)，这里转成工具事件
        // 走同一条通道（否则要同时管两条通道还得额外唤醒 UI）。prx 的发送端
        // 随作业线程退出而掉落，try_recv 返回断开即收工。
        {
            let tx = self.tool_tx.clone();
            let redraw_tx = self.redraw_tx.clone();
            std::thread::spawn(move || {
                while let Ok((d, t)) = prx.try_recv() {
                    let _ = tx.send((idx, ToolEvent::Progress(d, t)));
                    let _ = redraw_tx.try_send(());
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
            // 失败后 3 秒自动重试，直到下载成功为止。坏源在 download_race 内已被
            // 剔除、install 校验失败（BadDownload）也会清掉续传残留换源重下，
            // 不会再出现「替换失败: 下载文件损坏…请重新下载」的僵局。
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
                        let _ = status_tx.send((
                            format!("下载失败（第 {attempt} 次）: {e}，3 秒后自动重试…"),
                            None,
                        ));
                        std::thread::sleep(std::time::Duration::from_secs(3));
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
                        // 拼成残缺 exe；3 秒后整链重新并发下载（坏源已被剔除）。
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
        if from == 0 || from >= len || target == 0 || target > len {
            return;
        }
        let new_p = if target > from { target - 1 } else { target };
        if new_p == from {
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
                // 非启动加载中、最近一块**实质内容**输出停止 ≥3s。last_real_output_ms
                // 只看非动画块：周期转义重绘（tmux 状态栏/光标/屏幕刷新）不会让它
                // 刷新 → 这类会话不会因 done 横跳而循环弹「任务完成」+ 闪烁。
                // 未查看门槛在弹窗条件里（viewed 语义：启动即已见，仅新输出轮
                // 复位，见下）。
                let done = !s.exited.load(Ordering::Acquire)
                    && !s.loading_active(now_ms)
                    && now_ms.saturating_sub(s.last_real_output_ms.load(Ordering::Relaxed))
                        > OUTPUT_END_MS;
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
                if hit.clicked() && !selected {
                    actions.push(TabAction::Activate(0));
                }
                let hovering = !selected
                    && ui.ctx().pointer_interact_pos().is_some_and(|p| rect.contains(p));
                let bg = Self::tab_bg(sel_fill, selected, hovering, dark);
                ui.painter().set(bg_idx, egui::Shape::rect_filled(rect.expand2(egui::vec2(5.0, 2.0)), 0.0, bg));
            }

            for (i, tab) in self.tabs.iter().enumerate().skip(1) {
                if let Tab::Session(s) = tab {
                    ui.add_space(4.0);
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
                    let count = s.output_count.load(Ordering::Relaxed);
                    let last_out = s.last_output_ms.load(Ordering::Relaxed);
                    let last_input = s.last_input_ms.load(Ordering::Relaxed);
                    let last_scroll = s.last_scroll_ms.load(Ordering::Relaxed);
                    let icon = tab_icon(
                        s.exited.load(Ordering::Acquire),
                        s.loading_active(now_ms),
                        s.ever_output.load(Ordering::Relaxed),
                        count,
                        viewed,
                        last_out,
                        now_ms,
                        last_input,
                        last_scroll,
                    );
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
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
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
                            let s = ui.spacing().item_spacing.x;
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
                    // 交互层注册在内容之后（更上层），点击/拖动都落在它身上。
                    let resp = ui.interact(
                        rect,
                        egui::Id::new(("session_tab", i, dir_key)),
                        egui::Sense::click_and_drag(),
                    );
                    if resp.dragged() {
                        drag_index = Some(i);
                    }
                    if resp.clicked() {
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
                    ui.add_space(4.0);
                    let frame_resp = egui::Frame::new()
                        .corner_radius(4.0)
                        .fill(Color32::TRANSPARENT)
                        .inner_margin(tab_margin)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
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
                            let s = ui.spacing().item_spacing.x;
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
                    // 占位页签不响应点击/拖拽：只展示，防止拖动后位置错乱。
                    tab_rects.push((i, rect));
                } else if let Tab::Settings = tab {
                    // ── 设置页签 ──
                    ui.add_space(4.0);
                    let title = "⚙ 设置";
                    let selected = self.current == i;
                    let bg_idx = ui.painter().add(egui::Shape::Noop);
                    let (close_rect, frame_resp) = egui::Frame::new()
                        .corner_radius(4.0)
                        .fill(Color32::TRANSPARENT)
                        .inner_margin(tab_margin)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let min_width =
                                ui.text_style_height(&egui::TextStyle::Body) * 4.0;
                            let title_w = *self.title_width_cache.entry(title.to_string()).or_insert_with(|| {
                                ui.ctx().fonts_mut(|f| {
                                    f.layout_no_wrap(title.to_string(), tab_font.clone(), Color32::TRANSPARENT)
                                        .size()
                                        .x
                                })
                            });
                            let s = ui.spacing().item_spacing.x;
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
                    let resp = ui.interact(
                        rect,
                        egui::Id::new(("settings_tab", i)),
                        egui::Sense::click_and_drag(),
                    );
                    if resp.dragged() {
                        drag_index = Some(i);
                    }
                    if resp.clicked() {
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
                        "选择项目 → 启动（内嵌终端页签）   |   添加 / 重命名 / 改路径 / 删除 / 设置"
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
            // 状态消息**不许把右侧按钮挤出可视区**：宽度封顶（行宽的 45%，下限
            // 120px），超出以省略号截断。以前一条长消息（未检测到 pi 的安装提示
            // 带路径，能有一屏宽）会把自更新 / pi / opencode / 检查更新按钮全挤
            // 没了，用户连“检查更新”都点不着。悬停看全文、右键复制全文。
            let msg_w = {
                let avail = ui.available_width();
                if avail < 80.0 {
                    avail // 窗口极窄：全给消息，右侧按钮反正也放不下
                } else {
                    (avail * 0.45).max(120.0)
                }
            };
            let mut label_resp = ui.add_sized(
                [
                    msg_w,
                    ui.spacing()
                        .interact_size
                        .y
                        .max(ui.text_style_height(&egui::TextStyle::Body)),
                ],
                egui::Label::new(RichText::new(text).color(color)).truncate(),
            );
            ui.visuals_mut().override_text_color = saved_override;
            label_resp = label_resp.on_hover_text(copy_snapshot.clone());
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
            if let Some(tag) = self.update_latest.clone() {
                ui.separator();
                if self.downloading {
                    // 下载中：显示进度文本 + 取消按钮
                    ui.label(RichText::new(format!("⬇ 下载中… {tag}")).color(
                        ui.visuals().widgets.inactive.text_color()));
                    if ui.button("✕ 取消").on_hover_text("取消当前下载").clicked() {
                        self.cancel_download.store(true, std::sync::atomic::Ordering::Relaxed);
                        self.status = Some("正在取消下载…".to_string());
                    }
                } else {
                    if ui
                        .button(format!("⬇ 下载 {tag}"))
                        .on_hover_text("自动下载新版本到当前目录，完成后替换旧版本")
                        .clicked()
                    {
                        self.start_download(&tag);
                    }
                }
            }
            // pi / opencode 的安装/升级入口。条目有两个来源：有新版（⬇ pi vX.Y.Z）
            // 与**本机未检测到**（⬇ 安装 pi，点一下装到软件同级目录）；下载中则是
            // 进度文字 + ✕ 取消。
            //
            // 状态栏一行宽度有限，自更新 + pi + opencode 三个按钮同时出现会把
            // 右侧的「检查更新 / ⋯ 更多」挤出可视区（横向一排到底，没有第二行）。
            // 故：**只有 1 条时**照样摊在状态栏上（一眼可点），**≥2 条时**收成
            // 一颗「⬇ 工具 · N」按钮，点了在弹出菜单里逐条列 —— 与「⋯ 更多」
            // 同一套交互，横向只占一颗按钮的宽度。
            // 先收集待点击的工具下标再统一下载：本循环持 self.tools 的不可变
            // 借用，start_tool_download 要 &mut self。
            let mut tool_click: Option<(usize, bool)> = None;
            let mut tool_cancel_click: Option<usize> = None;
            // (工具下标, 是否下载中, 按钮文案, 悬停说明, 是否装进“本软件目录”)
            let mut tool_rows: Vec<(usize, bool, String, String, bool)> = Vec::new();
            for (i, t) in self.tools.iter().enumerate() {
                let spec = &TOOL_SPECS[i];
                let (downloading, latest, local, missing, dir) = (
                    t.downloading,
                    t.latest.clone(),
                    t.local.clone(),
                    t.missing,
                    t.install_dir.clone(),
                );
                // 本软件同级目录里已经有一份？决定要不要再给「装到本软件目录」。
                let fresh_dir = fresh_tool_dir(spec);
                let fresh_present = fresh_dir
                    .as_ref()
                    .is_some_and(|d| d.join(spec.exe_name).is_file());
                if downloading {
                    tool_rows.push((
                        i,
                        true,
                        format!("⬇ {} 下载中…", spec.label),
                        format!("正在从镜像源下载并安装 {}", spec.label),
                        false,
                    ));
                } else {
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
                        ToolEntryKind::None => continue,
                    };
                    if into_fresh {
                        if fresh_dir.as_ref().is_none_or(|d| d.as_os_str().is_empty()) {
                            continue;
                        }
                    } else if dir.as_os_str().is_empty() {
                        continue; // 拿不到安装目录 → 无处可装，不给假入口
                    }
                    tool_rows.push((i, false, label, tip, into_fresh));
                }
            }
            // 渲染：1 条直接摊开；≥2 条收进「⋯」式弹出菜单。
            match tool_rows.len() {
                0 => {}
                1 => {
                    let (i, downloading, label, tip, into_fresh) = tool_rows.remove(0);
                    let spec = &TOOL_SPECS[i];
                    ui.separator();
                    if downloading {
                        ui.label(RichText::new(label).color(
                            ui.visuals().widgets.inactive.text_color(),
                        ));
                        if ui
                            .button("✕ 取消")
                            .on_hover_text(format!("取消 {} 的下载", spec.label))
                            .clicked()
                        {
                            tool_cancel_click = Some(i);
                        }
                    } else if ui.button(label).on_hover_text(tip).clicked() {
                        tool_click = Some((i, into_fresh));
                    }
                }
                n => {
                    ui.separator();
                    let busy = tool_rows.iter().filter(|(_, d, ..)| *d).count();
                    let text = if busy > 0 {
                        format!("⬇ 工具 {n} · {busy} 下载中")
                    } else {
                        format!("⬇ 工具 {n}")
                    };
                    let tip = format!(
                        "{n} 个工具有安装/升级待办（点开逐条选，与「⋯ 更多」同一套交互）：{}",
                        tool_rows
                            .iter()
                            .map(|(_, _, l, _, _)| l.as_str())
                            .collect::<Vec<_>>()
                            .join(" / ")
                    );
                    let tool_id = egui::Id::new("status_tool_menu");
                    let mut resp = ui.add(egui::Button::new(text).sense(egui::Sense::CLICK));
                    resp = resp.on_hover_text(tip);
                    if resp.clicked() && ui.input(|i| i.pointer.any_click()) {
                        egui::Popup::toggle_id(ui.ctx(), tool_id);
                    }
                    egui::Popup::from_response(&resp)
                        .id(tool_id)
                        .open_memory(None)
                        .show(|ui| {
                            ui.label(RichText::new("pi / opencode 安装与升级").strong().small());
                            ui.separator();
                            for (i, downloading, label, tip, into_fresh) in &tool_rows {
                                ui.horizontal(|ui| {
                                    let spec = &TOOL_SPECS[*i];
                                    if *downloading {
                                        ui.label(RichText::new(label).color(
                                            ui.visuals().widgets.inactive.text_color(),
                                        ));
                                        if ui
                                            .small_button("✕ 取消")
                                            .on_hover_text(format!("取消 {} 的下载", spec.label))
                                            .clicked()
                                        {
                                            tool_cancel_click = Some(*i);
                                            ui.close();
                                        }
                                    } else if ui
                                        .button(label.clone())
                                        .on_hover_text(tip.clone())
                                        .clicked()
                                    {
                                        tool_click = Some((*i, *into_fresh));
                                        ui.close();
                                    }
                                });
                            }
                        });
                }
            }
            if let Some(i) = tool_cancel_click {
                self.tool_cancel[i].store(true, Ordering::Relaxed);
                self.status = Some(format!("正在取消 {} 下载…", TOOL_SPECS[i].label));
            }
            if let Some((i, into_fresh)) = tool_click {
                self.start_tool_download(i, into_fresh);
            }
            // 下载完成待重启：新 exe 已替换到当前路径，点击重启立刻生效。
            if self.update_done {
                ui.separator();
                if ui
                    .button("🔄 重启应用")
                    .on_hover_text("新版本已下载并替换，点击重启使新版本生效")
                    .clicked()
                {
                    self.restart_app();
                }
            }
            // 右下角：⋯ 更多折叠菜单（打开用户目录 / 软件目录）+「检查更新」+ 深浅色切换（右侧第一个 = 最右）。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 「⋯ 更多」按钮只响应鼠标点击，防止键盘方向键选中后回车误触发。
                let more_id = egui::Id::new("status_more_menu");
                // Sense::CLICK 不含 FOCUSABLE 位：不参与键盘焦点循环（Tab/方向键不会选中它）。
                // 最先添加 = 最右侧：⋯ 更多 固定在最右边。
                let more_resp = ui
                    .add(egui::Button::new("⋯ 更多").sense(egui::Sense::CLICK))
                    .on_hover_text("打开用户目录 / 软件目录");
                if more_resp.clicked() && ui.input(|i| i.pointer.any_click()) {
                    egui::Popup::toggle_id(ui.ctx(), more_id);
                }
                egui::Popup::from_response(&more_resp)
                    .id(more_id)
                    .open_memory(None)
                    .show(|ui| {
                        ui.set_width(110.0);
                        ui.with_layout(egui::Layout::top_down(egui::Align::RIGHT), |ui| {
                            if ui.selectable_label(false, "📂 打开用户目录")
                                .on_hover_text("打开用户目录（%USERPROFILE%），便于修改 agent 配置")
                                .clicked()
                            {
                                let dir = std::env::var("USERPROFILE")
                                    .or_else(|_| std::env::var("HOME"))
                                    .unwrap_or_else(|_| ".".to_string());
                                self.open_explorer(dir);
                                ui.close();
                            }
                            ui.separator();
                            if ui.selectable_label(false, "📂 打开软件目录")
                                .on_hover_text("打开本软件 exe 所在的目录（与本软件配置目录同级）")
                                .clicked()
                            {
                                let dir = software_dir().unwrap_or_else(|| PathBuf::from("."));
                                self.open_explorer(dir);
                                ui.close();
                            }
                        });
                    });
                // 「检查更新」：放在 ⋯ 更多 左边，同样只响应鼠标点击。
                let check_upd = ui
                    .add(egui::Button::new("🔄 检查更新").sense(egui::Sense::CLICK))
                    .on_hover_text("从 GitHub Release 检查本软件 + pi + opencode 的最新版本（启动/新开页签时也会自动检查）");
                if check_upd.clicked() && ui.input(|i| i.pointer.any_click()) {
                    self.check_updates(false);
                }
                // 主题切换按钮：深色 → 浅色 → 跟随系统 → 深色 轮转。
                // 只响应鼠标点击，防止键盘方向键选中后回车误触发。
                let (fs, dark) = (
                    self.config.settings.follow_system,
                    self.effective_dark(),
                );
                let label = if fs {
                    "🎨 跟随系统"
                } else if dark {
                    "🌙 深色"
                } else {
                    "☀ 浅色"
                };
                // Sense::CLICK 不含 FOCUSABLE 位：主题切换按钮同样只响应鼠标，
                // 不参与键盘焦点循环（方向键不会选中它，回车不会误触发）。
                let theme_btn = ui
                    .add(egui::Button::new(label).sense(egui::Sense::CLICK))
                    .on_hover_text("点击切换：深色 → 浅色 → 跟随系统（随 Windows 深浅自动切换）");
                if theme_btn.clicked() && ui.input(|i| i.pointer.any_click()) {
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
                    // 通知所有会话新主题：应答 OSC 10/11 查询 + 主动广播颜色（
                    // opencode 等 TUI 会据此匹配自己的配色）。
                    self.broadcast_theme();
                    // 延迟全量重绘：子进程收到广播后重绘需要时间，晚到的输出可能
                    // 在清缓存之后才写入；定时再清一次并强制整帧，兜住这类脏状态。
                    self.theme_settle_at =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
                    ui.ctx().request_repaint();
                }
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
                    if ui.button("＋ 添加").clicked() {
                        self.input = Some(InputDialog::AddProject {
                            name: String::new(),
                            path: String::new(),
                        });
                    }
                    if ui.button("⚙ 设置").clicked() {
                        self.open_settings();
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
        ui.label("TUI 启动命令（点击选择启动时要用的命令，可添加多个，拖动排序，改动自动保存）:");
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
        self.tool_dirs_ui(ui);
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        // ── 模型配置（页签：pi / oh-my-pi） ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("供应商配置:").strong());
            for (i, name) in ["pi 供应商配置", "oh-my-pi 供应商配置", "opencode 供应商配置"]
                .iter()
                .enumerate()
            {
                if ui
                    .add(egui::Button::selectable(self.model_settings_tab == i, *name))
                    .clicked()
                    && self.model_settings_tab != i
                {
                    // 切页签前提交原页签里未失焦的供应商改名/数字编辑（失焦事件只在字段被
                    // 渲染的帧里能捕捉，切页签的点击发生在对方页签渲染之前，会漏）避免丢失。
                    self.flush_provider_rename(self.model_settings_tab);
                    self.flush_model_num_edit(self.model_settings_tab);
                    self.model_settings_tab = i;
                }
            }
        });
        self.model_settings_ui(ui, self.model_settings_tab);
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        // 帧率设置已整体移除（曾可调 30 FPS）：持续高帧率重绘会干扰 Windows
        // 悬停激活窗口（焦点随鼠标）——30 FPS 输出中实测失效、10 FPS 正常（见
        // 921f062/0ae5904）。根治 = 去掉可调档，锁死 10 FPS（BUSY_FRAME_MS）。
        ui.add_space(12.0);
        ui.label(RichText::new("🔄 = 正在运行（有输出内容 / 进程树在计算），✅ = 输出结束待查看（切到该页签、或在页签内点击/滚动/输入、软件重新获得焦点即消失；TUI 静止等输入不算，显示空），空 = 等待输入或空闲，❌ = 已退出。\n🔄 以是否有输出内容为准，按键/粘贴等人工输入不算输出、保持空不误判 🔄；零输出页签不闪 🔄；✅ 稳定停留 2 秒即弹「任务完成」通知（仅未查看过的真任务输出轮，闲置页签不弹）；周期输出横跳会重置计时。\n快捷键：Ctrl+Tab 循环切换到下一个页签，Ctrl+Shift+Tab 切换到上一个。").weak());
        ui.add_space(12.0);
        ui.label(RichText::new(format!("配置文件: {}", self.config_path.display())).weak());
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
    fn tool_dirs_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label("工具更新路径（检查更新时到哪找 pi / opencode）:");
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
            let n = self.tabs.len();
            if n > 1 {
                // 后退 = 逆序一步（(current + n - 1) % n）。
                let step = if tab_fwd { 1 } else { n - 1 };
                self.current = (self.current + step) % n;
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
            // 取消/失败时立即重置 downloading 状态，UI 即时恢复
            if msg.contains("已取消") || msg.contains("失败") {
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
                && (!self.restore_slots.is_empty() || self.restore_settings_pos.is_some())
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
                // 退出时设置页签开着：按满页签栏索引插回原位置（首页后、会话之间）。
                if let Some(pos) = self.restore_settings_pos.take() {
                    self.tabs.insert(pos.min(self.tabs.len()), Tab::Settings);
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
    use super::tab_icon;

    fn icon(ever: bool, count: u32, viewed: bool, silent_ms: u64) -> Option<&'static str> {
        let now = 100_000u64;
        tab_icon(false, false, ever, count, viewed, now.saturating_sub(silent_ms), now, 0, 0)
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
        assert_eq!(tab_icon(false, false, false, 0, false, now - 500, now, 0, 0), None);
        assert_eq!(tab_icon(false, false, false, 0, false, now - 10_000, now, 0, 0), None);
    }

    // 已退出 / 启动加载具有最高优先级。
    #[test]
    fn exited_and_loading_override() {
        let now = 100_000u64;
        assert_eq!(tab_icon(true, false, false, 10, false, now - 10_000, now, 0, 0), Some("❌"));
        assert_eq!(tab_icon(false, true, false, 10, false, now - 10_000, now, 0, 0), Some("🔄"));
    }

    // 输入驱动例外：最近 1.5s 内用户输过键，回显即使刷新 last_output_ms
    // 也不判运行中 → 空；窗口过期（或从未输入）后按内容判 🔄。
    #[test]
    fn typing_echo_not_running() {
        let now = 100_000u64;
        // 1s 前刚输入过（回显新鲜）→ 不亮 🔄，落空。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 1_000, now, now - 1_000, 0), None);
        // 输入窗口边界：不敢 1.5s 整（< INPUT_ACTIVE_MS 才算），恰过期即恢复。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 1_000, now, now - 1_501, 0), Some("🔄"));
        // 无输入历史（last_input=0，鼠标选择/拖拽等）→ 正常判 🔄。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 1_000, now, 0, 0), Some("🔄"));
    }

    // 输入窗口不吞 ✅：任务完成后开始敲新命令（输入窗口内、但内容已停
    // ≥3s）→ ✅ 保持可见，不因打字闪空。
    #[test]
    fn typing_keeps_done_visible() {
        let now = 100_000u64;
        assert_eq!(tab_icon(false, false, true, 10, false, now - 10_000, now, now - 500, 0), Some("✅"));
        assert_eq!(tab_icon(false, false, true, 10, true, now - 10_000, now, now - 500, 0), None);
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
        assert_eq!(tab_icon(false, false, true, 10, false, now - 100, now, 0, now - 100), None);
        // 滚动窗口边界：恰过期（501ms）即按内容恢复 🔄。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 100, now, 0, now - 501), Some("🔄"));
        // 无滚动记录（本地缓冲滚动 / 从未转发，last_scroll=0）→ 输出新鲜照常 🔄。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 100, now, 0, 0), Some("🔄"));
        // 滚动例外不吞 ✅：滚动时内容早已停、未查看 → 仍按内容判完成。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 10_000, now, 0, now - 100), Some("✅"));
        // 滚动窗口与输入窗口互不干扰：输入回声例外只由 last_input 触发。
        assert_eq!(tab_icon(false, false, true, 10, false, now - 100, now, now - 100, now - 100), None);
    }
}
#[cfg(test)]
mod restore_coords_tests {
    use super::{restore_coords, TabKind};

    #[test]
    fn restore_indices_skip_exited_and_settings() {
        use TabKind::*;
        // 恢复数组 = [Home, B, Settings, D]（A 已退出不恢复）。
        let kinds = [Home, Gone, Alive, Settings, Alive];
        assert_eq!(restore_coords(&kinds, 4), (3, 2)); // D → 3
        assert_eq!(restore_coords(&kinds, 2), (1, 2)); // B → 1
        assert_eq!(restore_coords(&kinds, 3), (2, 2)); // Settings 自身 → 2
        assert_eq!(restore_coords(&kinds, 1), (1, 2)); // 已退出 → 邻位
        assert_eq!(restore_coords(&kinds, 0), (0, 2)); // Home → 0
        // 无设置页签：坐标 = Home + 存活会话序。
        let kinds = [Home, Alive, Alive];
        assert_eq!(restore_coords(&kinds, 2), (2, 1));
        // 设置页签排在会话前：插入后会话右移一格。
        let kinds = [Home, Settings, Alive];
        assert_eq!(restore_coords(&kinds, 2), (2, 1));
        assert_eq!(restore_coords(&kinds, 1), (1, 1));
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
        assets_from_html, exe_asset_from_html, extract_zip, parse_version_token, sync_tree, tar_bin,
        tool_exe_candidates, tool_fresh_dir_in, version_newer, win_arch, ClientApp, TOOL_SPECS,
    };
    use std::path::PathBuf;

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
        let (name, url, _size) = exe_asset_from_html(html, "v2025.06.30.0001").unwrap();
        assert_eq!(name, "TUIProjectManager.exe");
        assert_eq!(
            url,
            "https://github.com/releases/download/v2025.06.30.0001/TUIProjectManager.exe"
        );
    }

    #[test]
    fn html_wrong_tag_ignored() {
        let html = r#"<a href="/q/q/releases/download/vother/Other.exe">x</a>"#;
        assert!(exe_asset_from_html(html, "v2025.06.30.0001").is_none());
    }

    #[test]
    fn html_query_stripped() {
        let html = r#"<a href="/q/q/releases/download/v1/TUIProjectManager.exe?download=1">x</a>"#;
        let (name, url, _) = exe_asset_from_html(html, "v1").unwrap();
        assert_eq!(name, "TUIProjectManager.exe");
        assert!(!url.contains('?'));
    }

    #[test]
    fn html_no_exe_returns_none() {
        assert!(exe_asset_from_html("<html>nothing</html>", "v1").is_none());
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
        // 工具 release 的 zip 资产也走同一个 HTML 解析器
        let html = r#"<a href="/earendil-works/pi/releases/download/v0.88.0/pi-windows-x64.zip">pi</a>"#;
        let all = assets_from_html(html, "v0.88.0");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "pi-windows-x64.zip");
        assert_eq!(
            all[0].1,
            "https://github.com/releases/download/v0.88.0/pi-windows-x64.zip"
        );
        // tag 不匹配 / 无资产
        assert!(assets_from_html(html, "v0.87.1").is_empty());
        assert!(assets_from_html("<html>nothing</html>", "v1").is_empty());
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

