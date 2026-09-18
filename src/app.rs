use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui::{Color32, RichText};

use crate::config;
use crate::session::{self, Session};
use crate::terminal;

/// 启动宽限期：会话创建后一分钟内不弹「运行结束」/「执行完成」系统通知与
/// 任务栏闪烁。刚启动的会话 shell 初始化/命令首屏输出会制造大量看似「完成」
/// 的瞬间，宽限期过滤误报（进度型命令会在宽限期后再按正常规则提示）。
const STARTUP_GRACE_MS: u64 = 60_000;

/// 配置节流落盘间隔：仅窗口位置/尺寸变化（拖动/resize 每帧连续变化）时
/// 最多每 CONFIG_SAVE_INTERVAL 写盘一次；页签结构变化、显式保存都立即落盘。
const CONFIG_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// 全部静止时的慢心跳帧间隔（cmd/conhost 式节能：无脏区不持续重绘）。
/// 输出到达、鼠标/键盘事件都走 request_repaint() 即时唤醒，心跳只负责
/// 兜底捕获无事件干系的状态推进（任务完成通知稳定窗口、状态栏倒计时等）。
const IDLE_HEARTBEAT_MS: u64 = 500;

/// TUI 状态检测阈值（页签图标 / 完成通知判定）。准确性优先，但拒绝
/// 「拉长静默阈值」式消误报——那会把真实完成的通知推迟到十几秒。
/// 「还在运行」的正确证据是进程树在消耗 CPU（session::tree_cpu_active）：
/// Agent 静默思考/编译/搜索/打包都是 CPU 活跃，只是终端没输出。
///
/// 误报成因复盘：
/// - 旧 ✅ 分支只有 `count>0 && !viewed`，无「输出确实停止」时间判据，
///   字节一静 500ms 就亮 ✅。
/// - 旧 DONE_STABLE_MS=2s：周期输出间隙 >2s 就弹「任务完成」，实为还在跑。
/// - 旧 content_fresh=3s：TUI 静默 >3s（思考/链接）掉出「活跃」落 ✅。
///
/// 现方案：
/// - 🔄 判定扩为「网格在变 或 进程树近 3s 有 CPU 增量」——静默思考期间
///   保持 🔄，绝不落 ✅/✏️（CPU 判据见 session.rs tree_cpu_active）。
/// - OUTPUT_END_MS=3s：✅ 需连续 3s 零字节输出 + CPU 静默 + 网格静止。
/// - DONE_STABLE_MS=2s：通知/闪烁需在 ✅/✏️ 稳定停留 2 秒；实际通知延迟：
///   ✏️ 路径约 2.5s（0.5s 静默入场 + 2s 稳定），✅ 路径约 5s。
/// - CONTENT_FRESH_MS=30s：✏️「等待选择」需网格近 30s 有过实质变化，
///   CPU 静默挡静默思考，时长只影响图标形态、不影响通知延迟。
const CONTENT_FRESH_MS: u64 = 30_000;
const OUTPUT_END_MS: u64 = 3_000;
const DONE_STABLE_MS: u64 = 2_000;

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
fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\msyhbd.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
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

/// 用 curl 请求 URL 并解析 JSON 中的 tag_name。
/// 返回 Ok(tag) 或 Err(错误描述)。
fn fetch_tag_from_url(url: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-s", "-f", "--connect-timeout", "8", "--ssl-no-revoke",
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
        serde_json::from_str(&text).map_err(|_| "无法解析 GitHub 响应".to_string())?;
    v["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "GitHub 返回错误响应".to_string())
}

/// 拉取最新版本号。多源级联避免 GitHub API 限流（60 次/时）导致误报：
/// ① /releases/latest 的 302 重定向目标（HTML 端点，无限流）取最新 tag；
/// ② 重定向失败时回退 api.github.com 的 releases/latest JSON；
/// ③ 直连失败时依次尝试 GH_MIRRORS 镜像代理 API。
/// 返回（状态栏消息, 有新版本时的 tag）。
fn fetch_latest_release() -> (String, Option<String>) {
    // 源①：HTML 重定向。curl 不加 -L，从 redirect_url 里取 tag；
    // HTTP 非 2xx/3xx 时 -f 会报错 → 链路不通 → 回退源②。
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-s", "-f",
        "-o", "NUL", // 丢弃响应体，只要重定向头
        "-w", "%{redirect_url}",
        "--connect-timeout", "8",
        "--ssl-no-revoke",
        "-H", "User-Agent: TUIProjectManager",
        "https://github.com/qq458249269/TUIProjectManager/releases/latest",
    ]);
    // GUI 程序 spawn 控制台程序（curl.exe）会闪一个黑窗口：
    // CREATE_NO_WINDOW 让子进程不分配控制台，彻底消除。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd.output();
    if let Ok(o) = &out
        && o.status.success()
    {
        let url = String::from_utf8_lossy(&o.stdout);
        if let Some(pos) = url.find("/releases/tag/") {
            let tag = url[pos + "/releases/tag/".len()..].trim().to_string();
            if !tag.is_empty() {
                log_update(&format!("检查更新 源① HTML 重定向 → tag {tag}"));
                let msg = version_message(&tag);
                return if msg.contains("发现新版本") { (msg, Some(tag)) } else { (msg, None) };
            }
        }
        log_update(&format!("检查更新 源① HTML 返回但未解析出 tag（redirect={url}）"));
    } else if let Err(e) = &out {
        log_update(&format!("检查更新 源① 启动 curl 失败: {e}"));
    } else if let Ok(o) = &out {
        log_update(&format!(
            "检查更新 源① HTML 非 2xx/3xx（HTTP {}）",
            o.status.code().unwrap_or(0)
        ));
    }
    // 源②：回退 GitHub API（可能触发限流，此时会明确报错而非误报已最新）。
    let api_url =
        "https://api.github.com/repos/qq458249269/TUIProjectManager/releases/latest";
    match fetch_tag_from_url(api_url) {
        Ok(tag) => {
            log_update(&format!("检查更新 源② API 成功 → tag {tag}"));
            let msg = version_message(&tag);
            return if msg.contains("发现新版本") { (msg, Some(tag)) } else { (msg, None) };
        }
        Err(e) => {
            log_update(&format!("检查更新 源② API 失败: {e}"));
        }
    }
    // 源③：直连 GitHub 全部失败，依次尝试加速镜像代理 API。
    // 镜像前缀 + 原始 API URL 组成代理地址，适用于中国大陆等 GitHub 受限网络。
    for mirror in GH_MIRRORS {
        let proxy_url = format!("{mirror}{api_url}");
        match fetch_tag_from_url(&proxy_url) {
            Ok(tag) => {
                log_update(&format!("检查更新 镜像 {mirror} 成功 → tag {tag}"));
                let msg = version_message(&tag);
                return if msg.contains("发现新版本") { (msg, Some(tag)) } else { (msg, None) };
            }
            Err(e) => {
                log_update(&format!("检查更新 镜像 {mirror} 失败: {e}"));
            }
        }
    }
    ("检查更新失败：网络错误，请检查网络连接或代理设置".to_string(), None)
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
    let mut page_cmd = std::process::Command::new("curl");
    page_cmd.args([
        "-s", "-L", "-f", "--connect-timeout", "8", "--ssl-no-revoke",
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
        let mut api_cmd = std::process::Command::new("curl");
        api_cmd.args([
            "-s", "-f", "--connect-timeout", "8", "--ssl-no-revoke",
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
    // ── 候选下载链：原始直链 + 常见加速镜像，逐个尝试 ──
    // 每个文件名候选先打原始直链，再打各镜像；全部失败才报错交给上层
    // 3 秒重试。GitHub 直链对未知文件名返回 404，先试真实名（必中）。
    let mut attempts: Vec<(String, String)> = Vec::new();
    for (url, name) in fallback {
        attempts.push((url.clone(), name.clone()));
        for mirror in GH_MIRRORS {
            attempts.push((format!("{mirror}{url}"), name.clone()));
        }
    }
    log_update(&format!(
        "下载 开始尝试 {} 个候选链（tag={tag}，total={total}）：{}",
        attempts.len(),
        attempts
            .iter()
            .map(|(u, _)| u.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let mut errors: Vec<String> = Vec::new();
    for (url, name) in attempts {
        // 用户取消时立即停止所有候选链
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("下载已取消".to_string());
        }
        match download_one(&url, &name, total, dest_dir, &progress_tx, cancel) {
            Ok(p) => {
                log_update(&format!("下载 成功：{url} → {p}"));
                return Ok(p);
            }
            Err(e) => {
                log_update(&format!("下载 失败：{url}：{e}"));
                errors.push(format!("{url}: {e}"));
            }
        }
    }
    // API 失败的真实原因（限流 403 等）并入汇总，不再被直拼兜底掩盖。
    let mut parts = errors;
    if let Some(e) = &api_err {
        parts.push(format!("源① API: {e}"));
    }
    Err(format!("所有下载源失败：{}", parts.join("；")))
}

/// GitHub release 下载加速镜像（前缀拼接原始 github.com 直链，如
/// 下载单个文件：通过 curl 下载到 dest_dir/{asset_name}.new，支持断点续传和取消。
/// 列表换成当前可用的即可，补一个零成本、挂了自动跳过。
const GH_MIRRORS: &[&str] = &[
    "https://ghfast.top/",
    "https://gh-proxy.com/",
    "https://ghproxy.net/",
];

/// 用 curl 把单个 URL 下载到 dest_dir/{asset_name}.new，轮询文件大小报告进度。
/// 返回 Ok(下载文件路径) 或 Err(具体失败原因)。
fn download_one(
    url: &str,
    asset_name: &str,
    total: u64,
    dest_dir: &Path,
    progress_tx: &std::sync::mpsc::Sender<(u64, u64)>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<String, String> {
    // 下载到 .new 文件，完成后由调用方替换旧 exe。
    let new_name = format!("{asset_name}.new");
    let dest_path = dest_dir.join(&new_name);

    // 断点续传：若 .new 文件已存在，记录已下载字节数，用 curl -C - 续传。
    let downloaded_before = std::fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0);

    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-L", "-f", "--connect-timeout", "8", "--ssl-no-revoke",
        "-H", "User-Agent: TUIProjectManager",
        "-o", dest_path.to_str().unwrap_or("update.exe.new"),
    ]);
    // 已有部分文件时续传；否则从头下载。
    if downloaded_before > 0 {
        cmd.arg("-C").arg("-");
    }
    cmd.arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动下载失败: {e}"))?;

    // 轮询文件大小报告进度：每 200ms 检查一次。
    loop {
        // 检查取消信号
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = child.kill();
            let _ = std::fs::remove_file(&dest_path);
            return Err("下载已取消".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        let downloaded = std::fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0);
        let _ = progress_tx.send((downloaded, total));
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    let final_size = std::fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0);
                    let _ = progress_tx.send((final_size, total));
                    return Ok(dest_path.to_string_lossy().into_owned());
                } else {
                    // 失败时保留 .new 文件以供下次续传，不删除
                    return Err(format!("curl 退出码 {}", status.code().unwrap_or(-1)));
                }
            }
            Ok(None) => continue,
            Err(e) => {
                let _ = std::fs::remove_file(&dest_path);
                return Err(format!("{e}"));
            }
        }
    }
}

/// 把已下载的 .new 文件安装到正式名 exe，处理 Windows 下目标被占用的场景。
///
/// Windows 规则：运行中的映像文件允许 rename（Vista+），但禁止原地替换/删除。
/// 因此当 rename(.new → 正式名) 因目标被占用而失败（拒绝访问 os error 5 ——
/// 可能是本进程映像未被 unlock_exe 挪走、双开实例、或杀软瞬时句柄）时：
/// 1) 快路径：直接替换并带重试，等杀软/Defender 释放目标；
/// 2) 慢路径：把正式名先 rename 到 .old 腾出名字（运行中的映像也可 rename，
///    .old 兼作旧版备份），再放入新文件；失败自动回滚，正式名始终可用。
/// 返回是否安装成功。
fn install_update(
    new_file: &Path,
    final_path: &Path,
    old_path: &Path,
    status_tx: &std::sync::mpsc::Sender<(String, Option<String>)>,
    redraw_tx: &std::sync::mpsc::SyncSender<()>,
) -> bool {
    // 0) 校验下载产物（MZ 头）：镜像偶发返回错误页/空文件，装上就无法启动。
    if !looks_like_exe(new_file) {
        let _ = std::fs::remove_file(new_file); // 删掉坏的，下次重新下载
        let msg = format!(
            "替换失败: 下载文件损坏或不是可执行文件（{new_file:?}），已删除，请重新下载"
        );
        let _ = status_tx.send((msg, None));
        let _ = redraw_tx.try_send(());
        return false;
    }
    // 1) 快路径：正式名空闲 → 直接替换。带重试：杀软/Defender 扫描时会短暂
    //    占用正式名或 .new（拒绝访问 os error 5），等它释放。
    for i in 0..5 {
        match std::fs::rename(new_file, final_path) {
            Ok(()) => return true,
            Err(e) => {
                log_update(&format!("替换 直接替换第 {} 次失败: {e}", i + 1));
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
    // 2) 慢路径：正式名仍被占用。先清掉旧 .old（避免 rename 目标被占），
    //    再把正式名 rename 走腾出名字；运行中的映像也允许 rename。
    let _ = std::fs::remove_file(old_path);
    let mut moved = false;
    for _ in 0..5 {
        match std::fs::rename(final_path, old_path) {
            Ok(()) => {
                moved = true;
                break;
            }
            Err(e) => {
                log_update(&format!("替换 挪走占用目标失败: {e}"));
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
    if !moved {
        let msg = format!(
            "替换失败: 正式名 {final_path:?} 一直被其他进程占用（多为杀软扫描或另一个正在运行的实例），新文件保留在 {new_file:?}，请稍后重试"
        );
        let _ = status_tx.send((msg, None));
        let _ = redraw_tx.try_send(());
        return false;
    }
    match std::fs::rename(new_file, final_path) {
        Ok(()) => true,
        Err(e) => {
            // 回滚：把挪走的旧映像放回正式名，确保目录里始终有可用 exe。
            let _ = std::fs::rename(old_path, final_path);
            let msg = format!(
                "替换失败: {e}（已自动回滚，正式名保留旧版本；新文件仍在 {new_file:?}）"
            );
            let _ = status_tx.send((msg, None));
            let _ = redraw_tx.try_send(());
            false
        }
    }
}

/// 粗略校验文件是否为 Windows PE 可执行文件（MZ 头）。
fn looks_like_exe(p: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(p) else {
        return false;
    };
    let mut buf = [0u8; 2];
    let n = f.read(&mut buf).unwrap_or(0);
    n == 2 && &buf == b"MZ"
}

/// 从 GitHub Release 标签页 HTML 里找 exe 下载直链（API 限流/被墙时兜底）。
/// 返回 (exe 文件名, 下载 URL, 0)；HTML 不含字节数，进度按已下载字节算。
fn exe_asset_from_html(html: &str, tag: &str) -> Option<(String, String, u64)> {
    const NEEDLE: &str = "releases/download/";
    let mut from = 0;
    while let Some(rel) = html[from..].find(NEEDLE) {
        let start = from + rel + NEEDLE.len();
        let rest = &html[start..];
        let end = rest.find(['\'', '\"', '<', '?', '\n']).unwrap_or(rest.len());
        // 路径形如 {tag}/{xxx.exe}
        let parts: Vec<&str> = rest[..end].split('/').collect();
        if parts.len() == 2 && parts[0] == tag && parts[1].ends_with(".exe") {
            return Some((
                parts[1].to_string(),
                format!("https://github.com/{NEEDLE}{}/{}", parts[0], parts[1]),
                0,
            ));
        }
        from = start;
    }
    None
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
    pub settings_refresh_fps: String,
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
    /// 模型设置当前页签：0=pi 模型配置，1=oh-my-pi 模型配置。
    model_settings_tab: usize,
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
}

impl ClientApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let config = config::load();
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
        // 恒定帧率渲染，无需唤醒通道；保留 sender 供历史代码 try_send（无 receiver 时直接报错，不阻塞）。
        let redraw_tx = std::sync::mpsc::sync_channel(1).0;
        let ctx = cc.egui_ctx.clone();
        let (check_tx, update_rx) = std::sync::mpsc::channel();
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
            settings_refresh_fps: config::DEFAULT_REFRESH_FPS.to_string(),
            status: Some("在左侧选择项目并点击「启动」启动内嵌终端页签。".to_string()),
            config_path,
            term_focused: false,
            update_latest: None,
            download_progress_rx: None,
            downloading: false,
            cancel_download: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        update_done: false,
        update_final: None,
            input: None,
            confirm: None,
            check_tx,
            update_rx,
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
            model_settings_tab: 0,
            bg_frame: 0,
            search_query: String::new(),
            show_hidden: false,
            project_sort: ProjectSort::Default,
            pending_relaunch: Vec::new(),
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
                Tab::Session(s) if !s.exited.load(Ordering::Relaxed) => Some(s.as_ref()),
                _ => None,
            })
            .collect();
        let dirs: Vec<String> = active_sessions.iter().map(|s| s.dir.clone()).collect();
        let cmds: Vec<String> = active_sessions.iter().map(|s| s.cmd.clone()).collect();
        let active = self.current;
        // 设置页签也记录：退出时开着则启动时在相同位置恢复（见构造器 restore_settings_pos）。
        let settings_open = self.tabs.iter().any(|t| matches!(t, Tab::Settings));
        let settings_pos = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Settings))
            .unwrap_or(1);
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
        self.settings_refresh_fps = self.config.settings.refresh_fps.to_string();
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
            // 失败后 3 秒自动重试，直到下载成功为止。
            let mut attempt = 1u32;
            let new_path = loop {
                // 用户取消时跳出重试循环
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = status_tx.send(("下载已取消".to_string(), None));
                    let _ = redraw_tx.try_send(());
                    return;
                }
                match download_update(&tag, &exe_path, ptx.clone(), &cancel) {
                    Ok(p) => break p,
                    Err(e) => {
                        let _ = status_tx.send((
                            format!("下载失败（第 {attempt} 次）: {e}，3 秒后自动重试…"),
                            None,
                        ));
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        attempt += 1;
                    }
                }
            };
            let new_file = PathBuf::from(&new_path);
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
            if !install_update(&new_file, &final_path, &old_path, &status_tx, &redraw_tx) {
                // 安装失败：保留 .new 与 .old 供排查/手动处理，稍后可重新下载。
                return;
            }
            log_update(&format!("下载 替换完成：{new_file:?} → {final_path:?}（旧版本备份 → {old_path:?}）"));
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
                && !s.exited.load(Ordering::Relaxed)
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

    fn update_exited(&mut self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut changed = false;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let Tab::Session(s) = tab {
                if !s.exited.load(Ordering::Relaxed) {
                    // reader 线程退出时设置 exited 标志（无需 term 锁）。
                    // 兜底：子进程也已退出时同样标记。
                    // 后台会话跳过 try_wait()：不可见的会话不需要每帧 syscall,
                    // reader 线程会在 PTY 管道断裂时设置 exited 标志。
                    if s.foreground.load(Ordering::Relaxed) {
                        let child_exited = s.child
                            .as_deref_mut()
                            .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
                        if child_exited {
                            s.exited.store(true, Ordering::Relaxed);
                        }
                    }
                    if s.exited.load(Ordering::Relaxed) {
                        changed = true;
                    }
                }
                // 运行结束提醒：不管谁先置位 exited 都只处理一次（notified 去重）。
                // 用户正盯着该页签（当前页签且本应用在前台）时不打扰；
                // kill_in_background 已提前置位 notified，程序化终止（重启/切命令/关闭）不弹。
                if s.exited.load(Ordering::Relaxed)
                    && !s.notified.swap(true, Ordering::Relaxed)
                    && !(i == self.current && crate::app_is_foreground(self.titlebar_hwnd))
                {
                    // 启动宽限期：创建后一分钟内退出也静默（刚启动就崩/秒退
                    // 不打扰），notified 已置位因此宽限期后也不会补弹。
                    if now_ms.saturating_sub(s.started_ms.load(Ordering::Relaxed))
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
        // 应用是否前台：每帧取一次（update_exited 的「运行结束」通知同样用它判断）。
        let app_fg = crate::app_is_foreground(self.titlebar_hwnd);
        let slot_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap("✏️".to_string(), tab_font.clone(), Color32::TRANSPARENT)
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
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    // ── TUI 状态检测（准确性优先：宁可晚判定，不误报） ──
                    let is_tui = s.alt_screen.load(Ordering::Relaxed);
                    let cursor_vis = !s.cursor_hidden.load(Ordering::Relaxed);
                    let last_content = s.last_grid_change_ms.load(Ordering::Relaxed);
                    // 网格内容级静止：reader 每块输出后比较新旧可见格子（见
                    // session.rs），动画/spinner/周期重绘（top/watch）都改变格子
                    // → 内容新鲜；只有真正静止等待输入时才静默。比字节级可打印内容
                    // 判据更接近真值，思考动画期不再被误判为「等待你的选择」。
                    let content_silent = now_ms.saturating_sub(last_content) > 500;
                    // 内容新鲜度：最近 CONTENT_FRESH_MS 内网格有过实质变化 → 仍活跃。
                    // 拉长到 30s：TUI 静默思考/链接/等网络期间不掉出「活跃」，
                    // 避免静默 >3s 就被误判成「输出结束」。
                    let content_fresh = now_ms.saturating_sub(last_content) < CONTENT_FRESH_MS;
                    let count = s.output_count.load(Ordering::Relaxed);
                    let last_out = s.last_output_ms.load(Ordering::Relaxed);
                    let any_silent = now_ms.saturating_sub(last_out) > 500;
                    // ── 进程树 CPU 活动：Agent 静默思考/编译/搜索等「无输出但
                    //    仍在计算」的硬判据（session::tree_cpu_active，Toolhelp32
                    //    快照采样会话进程树 CPU 增量）。它才是「还在运行」的
                    //    证据——有它就不需要靠十几秒的静默阈值换准确性。 ──
                    let cpu_busy = session::tree_cpu_active(s.pid, now_ms);
                    // 图标逻辑：
                    //   ❌ 已退出
                    //   🔄 会话启动中 / 正在输出 / 进程树在计算（静默思考等）
                    //   ✏️ TUI 空闲等待用户输入
                    //   ✅ 输出结束（本轮对话完成，点击页签后消失）
                    let icon: Option<&str> = if s.exited.load(Ordering::Relaxed) {
                        Some("❌")
                    } else if s.loading.load(Ordering::Relaxed) {
                        Some("🔄")
                    } else if count > 0 && (!content_silent || cpu_busy) {
                        // 网格内容在变化，或进程树最近 3s 在消耗 CPU
                        // （Agent 静默思考/长编译/搜索）→ 正在运行 🔄。
                        // 旧版只认网格变化：静默期（思考/链接）被误判完成。
                        Some("🔄")
                    } else if is_tui
                        && cursor_vis
                        && any_silent     // 字节级静止：最近 500ms 无任何输出
                        && content_silent  // 网格级静止：格子 500ms 无变化
                        && content_fresh
                        && !cpu_busy      // CPU 也静默：真在等用户，不是在算
                        && count > 0
                    {
                        // TUI 空闲 + 光标可见 + 字节/网格/CPU 三静止 = 等待用户输入。
                        // any_silent 挡动画与周期重绘：进程只要还在输出（无论内容
                        // 是否重复）就不算等待；content_silent 挡字节稀疏但网格在
                        // 变的慢速输出；cpu_busy 挡静默思考（此刻 TUI 恰好不画动画）。
                        // （排除会话结束后 shell 空闲停在提示符的情况）
                        Some("✏️")
                    } else if count > 0
                        && !viewed
                        && content_silent
                        && !cpu_busy
                        // 输出真正结束判据：最近 OUTPUT_END_MS 内没有任何字节。
                        // CPU 判据已在前置分支挡住静默思考，这里 3s 就够判定
                        // 「输出确实停了」→ 真实完成的通知延迟压回 3 秒级。
                        && now_ms.saturating_sub(last_out) > OUTPUT_END_MS
                    {
                        // 输出已结束（连续 3s 零输出且 CPU 静默）+ 未查看 → ✅
                        Some("✅")
                    } else {
                        None
                    };
                    let title = s.title.clone();
                    let selected = self.current == i;
                    let dir_key = s.dir.as_str();
                    // 「执行完成」提醒：页签进入 ✅（输出结束待查看）/ ✏️（TUI
                    // 等待选择）状态后需稳定停留 DONE_STABLE_MS（2s）才弹系统通知
                    // + 任务栏闪烁（done_notified 去重，只提示一次）。稳定窗口过滤
                    // 误触发：top/watch/编译间歇输出等进程在 🔄↔✏️/✅ 间横跳时
                    // 重置计时；「还在跑」由前置的进程树 CPU 判据（cpu_busy）挡在
                    // 🔄，所以短稳定窗口就够区分真实完成与周期性输出——通知延迟保持
                    // 在 3 秒级，不再用十几秒的静默阈值换准确性。仅当
                    // 「当前页签且应用在前台」（用户正盯着）才静默；当前页签但应用
                    // 在后台（焦点在别的窗口）→ 用户没在看，照常计时弹通知 + 闪烁，
                    // 与 update_exited 的「运行结束」语义一致。图标离开这两个状态
                    // 时重置，下一轮输出完成再提示。
                    if matches!(icon, Some("✅") | Some("✏️")) {
                        let since = s.done_since_ms.load(Ordering::Relaxed);
                        if i == self.current && app_fg {
                            s.done_since_ms.store(0, Ordering::Relaxed);
                            // 用户正看着 ✅/✏️（当前页签且前台），视为已知晓，
                            // 切走时不弹重复通知。
                            s.done_notified.store(true, Ordering::Relaxed);
                        } else if since == 0 {
                            s.done_since_ms.store(now_ms, Ordering::Relaxed);
                        } else if now_ms.saturating_sub(since) > DONE_STABLE_MS
                            && !s.done_notified.swap(true, Ordering::Relaxed)
                        {
                            // 启动宽限期：刚启动的会话（含其首轮输出）不弹通知/闪烁，
                            // done_notified 已置位，宽限期结束后不会为这一轮补弹。
                            if now_ms.saturating_sub(s.started_ms.load(Ordering::Relaxed))
                                >= STARTUP_GRACE_MS
                            {
                                let heading =
                                    if matches!(icon, Some("✅")) { "任务完成" } else { "等待你的选择" };
                                crate::notify_run_finished(&title, heading);
                                crate::flash_taskbar(self.titlebar_hwnd);
                            }
                        }
                    } else {
                        s.done_since_ms.store(0, Ordering::Relaxed);
                        s.done_notified.store(false, Ordering::Relaxed);
                    }
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
                // 强制全量重绘：galley 里烘焙的是旧主题适配后的字形颜色；
                // 自绘光标扫描缓存也一并作废。否则出现汉字颜色错乱、
                // 光标块停在旧位置的残留。
                s.galley_cache.clear();
                // ASCII 快捷 galley 槽用 ver==galley_gen 判断有效性，
                // 主题切换必须递增使其全部过期，否则浅色下文本残留白色。
                s.galley_gen = s.galley_gen.wrapping_add(1);
                s.caret_scan = None;
                s.cached_render_shapes = None;
                s.cached_ansi_rgb = None;
                // 只推给应答过 OSC 10/11/4 颜色查询的会话（opencode 等）。
                // shell/cmd 从不查询这类序列，收到 `ESC]10;...ESC\` 会把 OSC 终止符
                // 的 `\` 直接回显成“自动输入了反斜杠”，不能广播。
                if !s.exited.load(Ordering::Relaxed)
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
            ui.label(RichText::new(text).color(color));
            ui.visuals_mut().override_text_color = saved_override;
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
                                let dir = std::env::current_exe()
                                    .ok()
                                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                                    .unwrap_or_else(|| PathBuf::from("."));
                                self.open_explorer(dir);
                                ui.close();
                            }
                        });
                    });
                // 「检查更新」：放在 ⋯ 更多 左边，同样只响应鼠标点击。
                let check_upd = ui
                    .add(egui::Button::new("🔄 检查更新").sense(egui::Sense::CLICK))
                    .on_hover_text("从 GitHub Release 检查最新版本（启动/新开页签时也会自动检查）");
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
                        if !new_cmd.is_empty() && !self.settings_commands.contains(&new_cmd) {
                            let old_cmd = std::mem::take(&mut self.settings_commands[i]);
                            if self.settings_command == old_cmd {
                                self.settings_command = new_cmd.clone();
                            }
                            self.settings_commands[i] = new_cmd;
                            dirty = true;
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
            if ui.button("浏览…").on_hover_text("选择可执行文件").clicked()
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
                if !cmd.is_empty() && !self.settings_commands.contains(&cmd) {
                    self.settings_commands.push(cmd.clone());
                    self.settings_new_command.clear();
                    dirty = true;
                }
            }
        });
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
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        // ── 模型配置（页签：pi / oh-my-pi） ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("模型配置:").strong());
            for (i, name) in ["pi 模型配置", "oh-my-pi 模型配置"].iter().enumerate() {
                if ui
                    .add(egui::Button::selectable(self.model_settings_tab == i, *name))
                    .clicked()
                {
                    self.model_settings_tab = i;
                }
            }
        });
        self.model_settings_ui(ui, self.model_settings_tab);
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        // ── 帧率预设 ──
        ui.label(RichText::new("终端页签刷新帧率（10–60 FPS）").strong());
        ui.add_space(4.0);
        let presets = [10u64, 30, 60];
        let cur_fps = self.config.settings.refresh_fps;
        ui.horizontal(|ui| {
            for &preset in &presets {
                let label = format!("{preset} FPS");
                let selected = cur_fps == preset;
                if ui
                    .add(egui::Button::selectable(selected, &label))
                    .clicked()
                {
                    self.config.settings.refresh_fps = preset;
                    self.settings_refresh_fps = preset.to_string();
                    self.save_config(format!("帧率已设为 {preset} FPS"));
                }
            }
            ui.separator();
            ui.label("自定义:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.settings_refresh_fps)
                    .desired_width(50.0)
                    .hint_text("10-60"),
            );
            if resp.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && let Ok(v) = self.settings_refresh_fps.parse::<u64>()
            {
                let clamped = v.clamp(10, 60);
                self.config.settings.refresh_fps = clamped;
                self.settings_refresh_fps = clamped.to_string();
                self.save_config(format!("帧率已设为 {clamped} FPS"));
            }
        });
        ui.label(
            RichText::new("有输出/交互时按此帧率刷新，全部静止自动降为 2 FPS 慢心跳省电")
                .weak()
                .small(),
        );
        ui.add_space(12.0);
        ui.label(RichText::new("✏️ = TUI 近期有输出且等待选择（会话结束后不显示），✅ = 输出结束待查看，点击页签后消失。\n✅/✏️ 稳定停留 2 秒即弹「任务完成/等待选择」通知；进程树 CPU 活跃（静默思考/编译）会保持 🔄 不误报，周期输出横跳会重置计时。").weak());
        ui.add_space(12.0);
        ui.label(RichText::new(format!("配置文件: {}", self.config_path.display())).weak());
    }

    /// provider/models 编辑表单（pi / oh-my-pi 共用），返回是否有改动。
    fn provider_list_ui(ui: &mut egui::Ui, models: &mut config::ModelsConfig) -> bool {
        let mut dirty = false;
        let keys: Vec<String> = models.providers.keys().cloned().collect();
        for key in &keys {
            if let Some(provider) = models.providers.get_mut(key) {
                ui.indent(key, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Provider:");
                        ui.label(RichText::new(key).strong());
                    });
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
                        ui.horizontal(|ui| {
                            ui.label(format!("Model[{}]:", mi));
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
                            let mut ctx_str = model.context_window.to_string();
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut ctx_str)
                                        .desired_width(80.0),
                                )
                                .changed()
                                && let Ok(v) = ctx_str.parse()
                            {
                                model.context_window = v;
                                dirty = true;
                            }
                            ui.label("max:");
                            let mut max_str = model.max_tokens.to_string();
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut max_str)
                                        .desired_width(80.0),
                                )
                                .changed()
                                && let Ok(v) = max_str.parse()
                            {
                                model.max_tokens = v;
                                dirty = true;
                            }
                            if ui.small_button("×").clicked() {
                                model_remove = Some(mi);
                            }
                        });
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
        dirty
    }

    /// 模型配置页签内容：0=pi，1=oh-my-pi。
    fn model_settings_ui(&mut self, ui: &mut egui::Ui, tab: usize) {
        if tab == 0 {
            ui.label(RichText::new("pi 模型配置").strong());
            ui.label(
                RichText::new(format!("路径: {}", config::pi_models_path().display()))
                    .weak()
                    .small(),
            );
            if Self::provider_list_ui(ui, &mut self.pi_models)
                && let Err(e) = config::save_pi_models(&self.pi_models)
            {
                self.status = Some(format!("pi 配置保存失败: {e}"));
            }
        } else {
            ui.label(RichText::new("oh-my-pi 模型配置").strong());
            ui.label(
                RichText::new(format!("路径: {}", config::omp_models_path().display()))
                    .weak()
                    .small(),
            );
            if Self::provider_list_ui(ui, &mut self.omp_models)
                && let Err(e) = config::save_omp_models(&self.omp_models)
            {
                self.status = Some(format!("oh-my-pi 配置保存失败: {e}"));
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

        // 前台标记每帧同步（覆盖所有切换路径：点击/Ctrl+Tab/关闭/拖拽/恢复）。
        for (i, t) in self.tabs.iter().enumerate() {
            if let Tab::Session(s) = t {
                s.foreground.store(i == self.current, Ordering::Relaxed);
            }
        }

        // 主题切换后的延迟全量重绘：到点后把所有会话缓存再清一遍并强制整帧。
        if let Some(t) = self.theme_settle_at {
            if std::time::Instant::now() >= t {
                self.theme_settle_at = None;
                for tab in &mut self.tabs {
                    if let Tab::Session(s) = tab {
                        s.galley_cache.clear();
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
        }
        if !busy {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            for tab in &self.tabs {
                if let Tab::Session(s) = tab {
                    // 最近 300ms 有输出或仍在启动 → 持续刷新（TUI 动画/top/watch 等
                    // 周期性进程依赖持续帧；加载态也需帧渲染启动画面）。
                    if now_ms.saturating_sub(s.last_output_ms.load(Ordering::Relaxed)) < 300
                        || s.loading.load(Ordering::Relaxed)
                    {
                        busy = true;
                        break;
                    }
                }
            }
        }
        let delay_ms = if busy {
            1000 / self.config.settings.refresh_fps.clamp(10, 60)
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
        let exited = self.update_exited();
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
        // 导致 Windows「鼠标悬停激活窗口」失效。等待输入提示改用页签 ✏️ 图标。
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
                    self.spawning.push((title.clone(), true, Some(save_i)));
                    std::thread::spawn(move || {
                        let result = session::spawn(
                            &title, &dir, &cmd,
                            cols as u16, rows as u16,
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
                    let sess = match self.tabs.get_mut(self.current) {
                        Some(Tab::Session(s)) => s,
                        _ => unreachable!(),
                    };
                    let crashed = match std::panic::catch_unwind(
                        std::panic::AssertUnwindSafe(|| {
                            terminal::show_terminal(ui, sess, dark, status, term_focused);
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
                    self.settings_ui(ui);
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
    use super::{exe_asset_from_html, ClientApp};
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
        std::fs::write(&p, b"MZ\x90\x00\x03\x00").unwrap();
        assert!(super::looks_like_exe(&p));
        std::fs::write(&p, b"<html>404 Not Found</html>").unwrap();
        assert!(!super::looks_like_exe(&p));
        std::fs::write(&p, b"MXZ").unwrap();
        assert!(!super::looks_like_exe(&p));
        let _ = std::fs::remove_file(&p);
    }
}

