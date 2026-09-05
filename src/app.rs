use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui::{Color32, RichText};

use crate::config;
use crate::session::{self, Session};
use crate::terminal;

/// 首页里的两个子页。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
}

/// 顶部页签：第一个永远是首页，后面每个对应一个终端会话。
pub enum Tab {
    Home,
    Session(Session),
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
    fn DwmGetWindowAttribute(
        hwnd: isize,
        attr: u32,
        attr_value: *mut std::ffi::c_void,
        attr_size: u32,
    ) -> i32;
}

/// 标题栏当前是否已是深色（DWM 属性 20 读回是否为 1）。
#[cfg(target_os = "windows")]
fn is_titlebar_dark(hwnd: isize) -> bool {
    if hwnd == 0 { return false; }
    let mut v: i32 = 0;
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            20, // DWMWA_USE_IMMERSIVE_DARK_MODE
            &mut v as *mut i32 as *mut std::ffi::c_void,
            4,
        )
    };
    hr >= 0 && v == 1
}

/// 只设 DWM 深色属性，不调 refresh_titlebar（避免 SWP_FRAMECHANGED 重置 hover 跟踪）。
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

/// 从 GitHub Release 拉取最新版本号，返回（状态栏消息, tag, 下载 URL）。
fn fetch_latest_release() -> (String, Option<String>, Option<String>) {
    const URL: &str =
        "https://api.github.com/repos/qq458249269/TUIProjectManager/releases/latest";
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-s",
        "--connect-timeout",
        "8",
        "--ssl-no-revoke",
        "-H",
        "User-Agent: TUIProjectManager",
        URL,
    ]);
    // GUI 程序 spawn 控制台程序（curl.exe）会闪一个黑窗口：
    // CREATE_NO_WINDOW 让子进程不分配控制台，彻底消除。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd.output();
    match out {
        Ok(o) if o.status.success() => {
            match serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&o.stdout)) {
                Ok(v) => {
                    let tag = v["tag_name"].as_str().unwrap_or("?").to_string();
                    let latest = tag.trim_start_matches('v');
                    if version_newer(latest, crate::app_version()) {
                        // 从 release assets 中查找 exe 下载链接
                        let download_url = v["assets"].as_array().and_then(|assets| {
                            assets.iter().find_map(|a| {
                                let name = a["name"].as_str().unwrap_or("");
                                if name.ends_with(".exe") {
                                    a["browser_download_url"].as_str().map(String::from)
                                } else {
                                    None
                                }
                            })
                        });
                        (format!("发现新版本 {tag}，点击下方按钮下载"), Some(tag), download_url)
                    } else {
                        (format!("已是最新版本 ({tag})"), None, None)
                    }
                }
                Err(_) => ("检查更新失败：无法解析 GitHub 响应".to_string(), None, None),
            }
        }
        Ok(_) => ("检查更新失败：网络错误".to_string(), None, None),
        Err(e) => (format!("检查更新失败：{e}"), None, None),
    }
}

/// 探测可用的系统代理：读注册表 ProxyServer（如 127.0.0.1:10808），端口可达才返回。
/// 代理软件没运行时端口连不通，返回 None 让 curl 直连。
fn system_proxy() -> Option<String> {
    let out = std::process::Command::new("reg")
        .args([
            "query",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
            "/v",
            "ProxyServer",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // 输出形如： ProxyServer    REG_SZ    127.0.0.1:10808
    let hp = text.lines().find_map(|l| {
        let l = l.trim();
        if !l.contains("REG_SZ") {
            return None;
        }
        let v = l["REG_SZ".len()..].trim();
        v.split([';', '=', ',']).find_map(|seg| {
            let seg = seg.trim();
            if seg.split(':').count() == 2 {
                Some(seg.to_string())
            } else {
                None
            }
        })
    })?;
    // 探测端口是否活着
    use std::net::{TcpStream, ToSocketAddrs};
    let addr = hp.to_socket_addrs().ok()?.next()?;
    if TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).is_err() {
        return None;
    }
    Some(format!("http://{hp}"))
}

/// 下载新版本 exe 到应用目录，完成后替换运行中的 exe（不退出、不重启）。
/// 在后台线程执行，通过 channel 回报进度。
fn download_and_replace(
    url: &str,
    progress_tx: std::sync::mpsc::Sender<DownloadEvent>,
) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let app_dir = exe.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();

    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy().to_string();
    let new_exe = app_dir.join(format!("{exe_name}.new"));

    // 用 curl 下载，--progress-meter 把表格进度写到 stderr，-o 指定输出文件；自动使用可用系统代理
    let proxy = system_proxy();
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-L",
        "--connect-timeout",
        "15",
        "--ssl-no-revoke",
        "--progress-meter",
        "-o",
        new_exe.to_str().unwrap_or(""),
        url,
    ])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::piped());
    if let Some(p) = &proxy {
        cmd.args(["--proxy", p]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = progress_tx.send(DownloadEvent::Failed(format!("下载失败: {e}")));
            return;
        }
    };
    let _ = progress_tx.send(DownloadEvent::Started(child.id()));

    // 逐段读 stderr：curl 每次进度更新以 \r 覆盖，段内按 \r 切分，取每段首列数字（百分比）
    // 非数字段收集末尾文本（curl 错误行），失败时带给用户
    let mut last_err = String::new();
    if let Some(stderr) = child.stderr.take() {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(stderr);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    for part in buf.split(|&b| b == b'\r') {
                        let text = String::from_utf8_lossy(part);
                        if let Some(pct) = text
                            .split_whitespace()
                            .next()
                            .and_then(|s| s.parse::<f64>().ok())
                        {
                            let _ = progress_tx.send(DownloadEvent::Progress(pct));
                        } else if text.starts_with("curl:") {
                            last_err = text.trim().to_string();
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    match child.wait() {
        Ok(status) if status.success() => {
            // 验证下载的文件非空
            match std::fs::metadata(&new_exe) {
                Ok(m) if m.len() > 1024 => {
                    let _ = progress_tx.send(DownloadEvent::Downloaded(new_exe));
                }
                _ => {
                    let _ = std::fs::remove_file(&new_exe).ok();
                    let _ = progress_tx.send(DownloadEvent::Failed("下载文件异常（可能不完整）".to_string()));
                }
            }
        }
        Ok(_) => {
            let msg = if last_err.is_empty() {
                "下载失败：curl 返回非零状态".to_string()
            } else {
                format!("下载失败：{last_err}")
            };
            let _ = progress_tx.send(DownloadEvent::Failed(msg));
        }
        Err(e) => {
            let _ = progress_tx.send(DownloadEvent::Failed(format!("下载失败: {e}")));
        }
    }
}

/// 下载完成后的事件。
enum DownloadEvent {
    /// curl 下载进程已启动，携带 pid（供退出时强制清理）。
    Started(u32),
    /// 下载进度百分比（0.0 ~ 100.0）。
    Progress(f64),
    Downloaded(PathBuf),
    Failed(String),
}

/// 热替换运行中的 exe（不退出、不重启）：旧 exe 改名 → 新 exe 移入原名。
/// 必须由外部 PowerShell 进程执行：主进程自己 rename 到自己 exe 原名会被拒绝访问（os error 5）。
/// 旧文件 exe.old 因运行中句柄暂时删不掉，残留到下次替换时再清。
fn schedule_replace(new_exe: &Path, app_dir: &Path, exe_name: &str) -> bool {
    let exe = app_dir.join(exe_name);
    let old = app_dir.join(format!("{exe_name}.old"));
    // 单引号包路径；路径含单引号极罕见，忽略。
    let ps = format!(
        "Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue; Rename-Item -LiteralPath '{}' '{}' -Force; Move-Item -LiteralPath '{}' '{}' -Force",
        old.display(),
        exe.display(),
        format!("{exe_name}.old"),
        new_exe.display(),
        exe.display(),
    );
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &ps]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.spawn().is_ok()
}

/// 用系统默认浏览器打开 URL。
/// egui 的 Hyperlink 依赖 eframe 的 links 特性（webbrowser crate），本项目为减
/// 体积禁用了该特性，OpenUrl 命令是空操作——所以自己调 cmd start 交给浏览器。
fn open_url(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW，避免闪黑窗
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
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

/// 项目目录存在性（带 TTL 缓存）。每帧 UI 都要显示目录状态，直接 is_dir()
/// 是每帧每项目一次文件系统调用；2 秒内复用上次结果，过期才重新采样。
/// 独立成函数拿 cache 参数而非 &mut self：调用点在遍历 self.config 的
/// 循环里，避免借用冲突。
fn dir_exists(cache: &mut HashMap<String, (bool, Instant)>, path: &str) -> bool {
    const TTL: std::time::Duration = std::time::Duration::from_secs(2);
    let now = Instant::now();
    if let Some(&(ok, at)) = cache.get(path) {
        if now.duration_since(at) < TTL {
            return ok;
        }
    }
    let ok = Path::new(path).is_dir();
    cache.insert(path.to_string(), (ok, now));
    ok
}

pub struct ClientApp {
    pub config: config::Config,
    pub tabs: Vec<Tab>,
    pub current: usize,
    pub screen: Screen,
    pub selected_project: usize,
    pub settings_command: String,
    pub settings_commands: Vec<String>,
    pub settings_new_command: String,
    pub settings_refresh_fps: String,
    pub status: Option<String>,
    pub config_path: PathBuf,
    pub term_focused: bool,
    update_latest: Option<String>,
    update_download_url: Option<String>,
    check_tx: Sender<(String, Option<String>, Option<String>)>,
    update_rx: Receiver<(String, Option<String>, Option<String>)>,
    /// 下载进度（Some = 正在下载）。
    download_progress: Option<String>,
    /// 下载进程 pid（应用退出时强制结束，避免残留 curl）。
    download_pid: Option<u32>,
    /// 下载事件接收通道。
    download_rx: Option<std::sync::mpsc::Receiver<DownloadEvent>>,
    pub input: Option<InputDialog>,
    pub confirm: Option<ConfirmDialog>,
    redraw_tx: std::sync::mpsc::SyncSender<()>,
    redraw_rx: Receiver<()>,
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
    spawn_rx: Option<Receiver<(Result<Session, String>, bool, Option<usize>)>>,
    /// 启动时待恢复的会话（同样推迟到首帧会话页签布局）。
    pending_restore: Vec<PendingLaunch>,
    /// 恢复的会话处理完后一次性应用上次激活页签（消费一次）。
    restore_active: Option<usize>,
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
    /// 模型设置是否展开编辑。
    model_settings_open: bool,
    /// 后台重绘跳帧计数器：后台页签收到 redraw 信号时累计，达到跳帧阈值才真正重绘。
    bg_frame: u64,
    /// 首页项目列表搜索过滤文本。
    search_query: String,
    /// 首页是否显示已隐藏的项目。
    show_hidden: bool,
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
        // 重绘信号用容量 1 的有界通道：任意多个终端会话/后台线程并发投递时，
        // 通道满即丢弃新信号（try_send），刷新请求被合并——不会出现消息堆积。
        let (redraw_tx, redraw_rx) = std::sync::mpsc::sync_channel(1);
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
            settings_refresh_fps: config::DEFAULT_REFRESH_FPS.to_string(),
            status: Some("在左侧选择项目并点击「启动」启动内嵌终端页签。".to_string()),
            config_path,
            term_focused: false,
            update_latest: None,
            update_download_url: None,
            input: None,
            confirm: None,
            check_tx,
            update_rx,
            redraw_tx,
            redraw_rx,
            download_progress: None,
            download_pid: None,
            download_rx: None,
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
            last_term_size: (80, 24),
            titlebar_hwnd,
            last_theme_dark: initial_dark,

            ctx,
            dir_exists_cache: HashMap::new(),
            title_width_cache: HashMap::new(),
            pi_models: config::load_pi_models(),
            omp_models: config::load_omp_models(),
            model_settings_open: false,
            bg_frame: 0,
            search_query: String::new(),
            show_hidden: false,
            pending_relaunch: Vec::new(),
        };

        // 恢复上次退出时打开中的终端页签：目录仍存在则重新拉起 TUI 会话。
        // 不在构造期 spawn：此时窗口未布局，只有估算尺寸，会走错尺寸启动路径；
        // 收集成 pending_restore，等首帧会话页签布局里按精确尺寸启动（见 ui()）。
        for d in &saved_tabs.dirs {
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
            app.pending_restore.push(PendingLaunch {
                title: name,
                dir: d.clone(),
                cmd: app.config.settings.tui_command.clone(),
            });
        }
        if !app.pending_restore.is_empty() {
            app.restore_active = Some(saved_active);
        }
        // 每次启动自动检查一次更新。
        app.check_updates(true);
        app
    }

    fn save_config(&mut self, msg: String) {
        match config::save(&self.config) {
            Ok(()) => self.status = Some(msg),
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
            if let Tab::Session(s) = tab {
                if s.dir == project.path && !s.exited.load(Ordering::Relaxed) {
                    self.current = i;
                    self.term_focused = true;
                    self.status = Some(format!("已切换到会话: {}", s.title));
                    return;
                }
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
    #[cfg(windows)]
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
        // 将 child 移到后台线程异步清理：Child::drop 在 Windows 上调用
        // WaitForSingleObject 等待进程退出，会阻塞 UI 线程 100-500ms。
        // take() 后 tabs.remove() 触发的 Session::Drop 不再包含 child，
        // UI 线程立即返回，进程终止在后台完成。
        if let Some(Tab::Session(s)) = self.tabs.get_mut(idx) {
            if let Some(mut child) = s.child.take() {
                std::thread::spawn(move || {
                    let _ = child.kill();
                });
            }
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
        let mut changed = false;
        for tab in self.tabs.iter_mut() {
            if let Tab::Session(s) = tab {
                if !s.exited.load(Ordering::Relaxed) {
                    // reader 线程退出时设置 exited 标志（无需 term 锁）。
                    // 兜底：子进程也已退出时同样标记。
                    // 后台会话跳过 try_wait()：不可见的会话不需要每帧 syscall,
                    // reader 线程会在 PTY 管道断裂时设置 exited 标志。
                    if s.foreground.load(Ordering::Relaxed) {
                        let child_exited = s.child
                            .as_deref_mut()
                            .map_or(false, |c| matches!(c.try_wait(), Ok(Some(_))));
                        if child_exited {
                            s.exited.store(true, Ordering::Relaxed);
                        }
                    }
                    if s.exited.load(Ordering::Relaxed) {
                        changed = true;
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
                if let Some(mut child) = s.child.take() {
                    let _ = child.kill();
                }
            }
        }
        // 结束残留的下载进程（curl）
        if let Some(pid) = self.download_pid.take() {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
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
                    // ── TUI 状态检测 ──
                    let is_tui = s.alt_screen.load(Ordering::Relaxed);
                    let cursor_vis = !s.cursor_hidden.load(Ordering::Relaxed);
                    let last_content = s.last_content_ms.load(Ordering::Relaxed);
                    let content_silent = now_ms.saturating_sub(last_content) > 500;
                    // 内容新鲜度：最近 3 秒内有过可打印内容输出 → TUI 活跃。
                    let content_fresh = now_ms.saturating_sub(last_content) < 3000;
                    let count = s.output_count.load(Ordering::Relaxed);
                    let last_out = s.last_output_ms.load(Ordering::Relaxed);
                    let any_silent = now_ms.saturating_sub(last_out) > 500;
                    // 图标逻辑：
                    //   ❌ 已退出
                    //   🔄 会话启动中 / 正在输出
                    //   ✏️ TUI 空闲等待用户输入
                    //   ✅ 输出结束（本轮对话完成，点击页签后消失）
                    let icon: Option<&str> = if s.exited.load(Ordering::Relaxed) {
                        Some("❌")
                    } else if s.loading.load(Ordering::Relaxed) {
                        Some("🔄")
                    } else if !any_silent && count > 0 {
                        // 正在输出 → 🔄
                        Some("🔄")
                    } else if is_tui && cursor_vis && content_silent && content_fresh && count > 0 {
                        // TUI 空闲 + 光标可见 + 近期有内容输出 = 等待用户输入
                        // （排除会话结束后 shell 空闲停在提示符的情况）
                        Some("✏️")
                    } else if count > 0 && !viewed {
                        // 输出已结束 + 未查看 → ✅
                        Some("✅")
                    } else {
                        None
                    };
                    let title = s.title.clone();
                    let selected = self.current == i;
                    let dir_key = s.dir.as_str();
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
        // 将 child 移到后台线程异步清理，避免 Child::drop 阻塞 UI 线程。
        if !s.exited.load(Ordering::Relaxed) {
            if let Some(mut child) = s.child.take() {
                std::thread::spawn(move || {
                    let _ = child.kill();
                });
            }
        }
        self.tabs.remove(idx);
        if self.current >= self.tabs.len() {
            self.current = self.tabs.len().saturating_sub(1);
        }
        if self.current == 0 {
            self.screen = Screen::Main;
        }
        self.refresh_focus();
        self.pending_relaunch.push(PendingRelaunch {
            tab_index: idx,
            title,
            dir,
            cmd,
        });
        self.status = Some("正在重新启动...".to_string());
    }

    /// 通知所有会话当前主题：更新应答器用的标志，并主动广播 OSC 10/11 颜色
    /// （opencode 等 TUI 启动时会查询终端颜色来匹配自己的配色）。
    /// 当前实际生效的主题深浅：跟随系统时取系统偏好，否则取用户设置。
    fn effective_dark(&self) -> bool {
        if self.config.settings.follow_system {
            // 直接读注册表：egui system_theme() 依赖 WM_SETTINGCHANGE，
            // 窗口未激活/消息丢失时返回 None 导致跟随系统失效。
            #[cfg(target_os = "windows")]
            { return query_windows_dark_mode(); }
            #[cfg(not(target_os = "windows"))]
            { matches!(self.ctx.system_theme(), None | Some(egui::Theme::Dark)) }
        } else {
            self.config.settings.dark_mode
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
        // 将 child 移到后台线程异步清理，避免 Child::drop 阻塞 UI 线程。
        if !s.exited.load(Ordering::Relaxed) {
            if let Some(mut child) = s.child.take() {
                std::thread::spawn(move || {
                    let _ = child.kill();
                });
            }
        }
        self.tabs.remove(idx);
        if self.current >= self.tabs.len() {
            self.current = self.tabs.len().saturating_sub(1);
        }
        if self.current == 0 {
            self.screen = Screen::Main;
        }
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
            ui.label(RichText::new(text).color(color));
            if let Some(tag) = &self.update_latest {
                ui.separator();
                if self.download_progress.is_some() {
                    // 正在下载，显示进度
                    let progress = self.download_progress.clone().unwrap_or_default();
                    ui.label(RichText::new(format!("⬇ 下载中 {progress}")).color(ui_warn(ui)));
                } else if let Some(download_url) = &self.update_download_url {
                    // 有下载链接，点击直接下载替换
                    if ui
                        .button(format!("⬇ 下载 {tag}"))
                        .on_hover_text("下载新版本并自动替换（需重启应用）")
                        .clicked()
                    {
                        let url = download_url.clone();
                        let (dl_tx, dl_rx) = std::sync::mpsc::channel();
                        self.download_progress = Some("0.00%".to_string());
                        self.download_rx = Some(dl_rx);
                        std::thread::spawn(move || {
                            download_and_replace(&url, dl_tx);
                        });
                    }
                } else {
                    // 无下载链接，降级到浏览器打开
                    let url = format!(
                        "https://github.com/qq458249269/TUIProjectManager/releases/tag/{tag}"
                    );
                    if ui
                        .button(format!("⬇ 下载 {tag}"))
                        .on_hover_text("用系统默认浏览器打开 GitHub Release 下载页")
                        .clicked()
                    {
                        open_url(&url);
                    }
                }
            }
            // 右下角：⋯ 更多折叠菜单（打开用户目录 / 软件目录 / 检查更新）+ 深浅色切换（右侧第一个 = 最右）。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 「⋯ 更多」按钮只响应鼠标点击，防止键盘方向键选中后回车误触发。
                let more_id = egui::Id::new("status_more_menu");
                let more_resp = ui.add(egui::Button::new("⋯ 更多"))
                    .on_hover_text("打开用户目录 / 软件目录 / 检查更新");
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
                            ui.separator();
                            if ui.selectable_label(false, "🔄 检查更新")
                                .on_hover_text("从 GitHub Release 检查最新版本（启动/新开页签时也会自动检查）")
                                .clicked()
                            {
                                self.check_updates(false);
                                ui.close();
                            }
                        });
                    });
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
                let theme_btn = ui
                    .button(label)
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
                // 搜索框
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .desired_width(ui.available_width())
                        .hint_text("搜索项目名/目录..."),
                );
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
                        for &i in &filtered_indices {
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
                            if resp.dragged() {
                                drag_index = Some(i);
                            }
                            row_rects.push((i, resp.rect));
                        }
                        // 拖动到列表上下边缘时自动滚动，让拖拽能到达视野外的项目。
                        if self.drag_project.is_some() {
                            if let Some(pos) = ui.ctx().pointer_interact_pos() {
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
                        }
                    });
                // 拖动状态跨帧保存在 self.drag_project：拖动中的每一帧刷新源索引，
                // 松手帧（dragged() 已变 false）靠它仍拿得到 from。
                if let Some(i) = drag_index {
                    self.drag_project = Some(i);
                }
                if let Some(from) = self.drag_project {
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
                            if let Some((last_i, last)) = row_rects.last() {
                                if pos.y > last.bottom() {
                                    target = Some((*last_i, last.bottom()));
                                }
                            }
                            if let Some((first_i, first)) = row_rects.first() {
                                if pos.y < first.top() {
                                    target = Some((*first_i, first.top()));
                                }
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
        for (i, cmd) in self.settings_commands.iter().enumerate() {
            ui.horizontal(|ui| {
                let selected = *cmd == self.settings_command;
                // 整行可点可拖（click_and_drag 由 egui 延迟判定拖动，纯点击天然保留）；
                // 不设 min_size：让「删除/复制」排在右侧不顶行，否则全宽会把按钮顶到下一行。
                // selectable_label 即 Button::selectable。
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
                // 拖动悬浮帧：记录源索引；行矩形供落位定位。
                if resp.dragged() {
                    drag_index = Some(i);
                }
                row_rects.push((i, resp.rect));
                if ui
                    .small_button("删除")
                    .on_hover_text("从命令列表中移除")
                    .clicked()
                {
                    remove_idx = Some(i);
                }
                // 复制命令到系统剪贴板：可粘贴到终端/配置别处使用。
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
                    if let Some((last_i, last)) = row_rects.last() {
                        if pos.y > last.bottom() {
                            target = Some((*last_i, last.bottom()));
                        }
                    }
                    if let Some((first_i, first)) = row_rects.first() {
                        if pos.y < first.top() {
                            target = Some((*first_i, first.top()));
                        }
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
        if let Some(i) = remove_idx {
            if i < self.settings_commands.len() {
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
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.settings_new_command)
                    .desired_width(280.0)
                    .hint_text("新命令，如 lazygit / htop"),
            );
            if ui.button("浏览…").on_hover_text("选择可执行文件").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("选择 TUI 可执行文件")
                    .add_filter("可执行文件", &["exe", "bat", "cmd", "com"])
                    .pick_file()
                {
                    self.settings_new_command = path.to_string_lossy().to_string();
                }
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
        // ── 模型配置 ──
        ui.horizontal(|ui| {
            let arrow = if self.model_settings_open { "▼" } else { "▶" };
            if ui
                .button(format!("{arrow} 模型配置"))
                .on_hover_text("配置 pi / oh-my-pi 的模型参数")
                .clicked()
            {
                self.model_settings_open = !self.model_settings_open;
            }
        });
        if self.model_settings_open {
            self.model_settings_ui(ui);
        }
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        // ── 帧率预设 ──
        ui.label(RichText::new("前台终端页签帧率（后台页签始终不渲染，不影响 CPU）").strong());
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
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Ok(v) = self.settings_refresh_fps.parse::<u64>() {
                    let clamped = v.clamp(10, 60);
                    self.config.settings.refresh_fps = clamped;
                    self.settings_refresh_fps = clamped.to_string();
                    self.save_config(format!("帧率已设为 {clamped} FPS"));
                }
            }
        });
        ui.label(
            RichText::new("默认 30 FPS，10 省电 / 30 均衡 / 60 流畅")
                .weak()
                .small(),
        );
        ui.add_space(12.0);
        ui.label(RichText::new("界面刷新：空闲时每秒 1 次，进程有输出时 300ms 一帧动画（页签柱状指示），降低 CPU 占用。\n✏️ = TUI 近期有输出且等待选择（会话结束后不显示），✅ = 输出结束待查看，点击页签后消失。").weak());
        ui.add_space(12.0);
        ui.label(RichText::new(format!("配置文件: {}", self.config_path.display())).weak());
    }

    fn model_settings_ui(&mut self, ui: &mut egui::Ui) {
        // ── pi 配置 ──
        ui.label(RichText::new("pi 模型配置").strong());
        ui.label(
            RichText::new(format!("路径: {}", config::pi_models_path().display()))
                .weak()
                .small(),
        );
        let mut pi_dirty = false;
        // providers 列表
        let pi_keys: Vec<String> = self.pi_models.providers.keys().cloned().collect();
        for key in &pi_keys {
            if let Some(provider) = self.pi_models.providers.get_mut(key) {
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
                            pi_dirty = true;
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
                            pi_dirty = true;
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
                                pi_dirty = true;
                            }
                            ui.label("name:");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut model.name)
                                        .desired_width(100.0),
                                )
                                .changed()
                            {
                                pi_dirty = true;
                            }
                            ui.label("context:");
                            let mut ctx_str = model.context_window.to_string();
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut ctx_str)
                                        .desired_width(80.0),
                                )
                                .changed()
                            {
                                if let Ok(v) = ctx_str.parse() {
                                    model.context_window = v;
                                    pi_dirty = true;
                                }
                            }
                            ui.label("max:");
                            let mut max_str = model.max_tokens.to_string();
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut max_str)
                                        .desired_width(80.0),
                                )
                                .changed()
                            {
                                if let Ok(v) = max_str.parse() {
                                    model.max_tokens = v;
                                    pi_dirty = true;
                                }
                            }
                            if ui.small_button("×").clicked() {
                                model_remove = Some(mi);
                            }
                        });
                    }
                    if let Some(mi) = model_remove {
                        provider.models.remove(mi);
                        pi_dirty = true;
                    }
                    if ui.button("+ 添加模型").clicked() {
                        provider
                            .models
                            .push(config::ModelEntry::default());
                        pi_dirty = true;
                    }
                });
            }
        }
        ui.add_space(4.0);
        ui.separator();
        // ── oh-my-pi 配置 ──
        ui.label(RichText::new("oh-my-pi 模型配置").strong());
        ui.label(
            RichText::new(format!("路径: {}", config::omp_models_path().display()))
                .weak()
                .small(),
        );
        let mut omp_dirty = false;
        let omp_keys: Vec<String> = self.omp_models.providers.keys().cloned().collect();
        for key in &omp_keys {
            if let Some(provider) = self.omp_models.providers.get_mut(key) {
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
                            omp_dirty = true;
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
                            omp_dirty = true;
                        }
                    });
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
                                omp_dirty = true;
                            }
                            ui.label("name:");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut model.name)
                                        .desired_width(100.0),
                                )
                                .changed()
                            {
                                omp_dirty = true;
                            }
                            ui.label("context:");
                            let mut ctx_str = model.context_window.to_string();
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut ctx_str)
                                        .desired_width(80.0),
                                )
                                .changed()
                            {
                                if let Ok(v) = ctx_str.parse() {
                                    model.context_window = v;
                                    omp_dirty = true;
                                }
                            }
                            ui.label("max:");
                            let mut max_str = model.max_tokens.to_string();
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut max_str)
                                        .desired_width(80.0),
                                )
                                .changed()
                            {
                                if let Ok(v) = max_str.parse() {
                                    model.max_tokens = v;
                                    omp_dirty = true;
                                }
                            }
                            if ui.small_button("×").clicked() {
                                model_remove = Some(mi);
                            }
                        });
                    }
                    if let Some(mi) = model_remove {
                        provider.models.remove(mi);
                        omp_dirty = true;
                    }
                    if ui.button("+ 添加模型").clicked() {
                        provider
                            .models
                            .push(config::ModelEntry::default());
                        omp_dirty = true;
                    }
                });
            }
        }
        // 自动保存
        if pi_dirty {
            if let Err(e) = config::save_pi_models(&self.pi_models) {
                self.status = Some(format!("pi 配置保存失败: {e}"));
            }
        }
        if omp_dirty {
            if let Err(e) = config::save_omp_models(&self.omp_models) {
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
                            if browse {
                                if let Some(dir) = rfd::FileDialog::new()
                                    .set_title("选择项目文件夹")
                                    .pick_folder()
                                {
                                    *path = dir.to_string_lossy().to_string();
                                }
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
                            if is_edit_path {
                                if ui.button("浏览…").clicked() {
                                    if let Some(dir) = rfd::FileDialog::new()
                                        .set_title("选择项目文件夹")
                                        .pick_folder()
                                    {
                                        *value = dir.to_string_lossy().to_string();
                                    }
                                }
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
        if let Ok(exe) = std::env::current_exe() {
            if let Some(name) = exe.file_name().and_then(|n| n.to_str()) {
                let _ = std::fs::remove_file(exe.with_file_name(format!("{name}.running")));
            }
        }
        // 记录打开中的终端页签（退出后下次启动自动重新拉起）。
        // active 指向 dirs 数组中的页签（0=Home, 1=dirs[0], …）。
        // 必须按 dirs 实际过滤后的顺序计算索引，不能直接用 self.current：
        // self.current 是 tabs 全数组含 Home/已退出页签的索引，与 dirs 不对应。
        let dirs: Vec<String> = self
            .tabs
            .iter()
            .skip(1)
            .filter_map(|t| match t {
                Tab::Session(s) if !s.exited.load(Ordering::Relaxed) => Some(s.dir.clone()),
                _ => None,
            })
            .collect();
        let active = self.current;
        self.config.tabs = config::TabsState { dirs, active };
        // 窗口状态已在每帧 logic 中记录，退出时落盘。
        let _ = config::save(&self.config);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 固定深色标题栏：每帧检查 DWM 属性，被系统重置（WM_SETTINGCHANGE）时补设。
        // 只调 DwmSetWindowAttribute（不调 refresh_titlebar），不干扰 hover 跟踪。
        if !is_titlebar_dark(self.titlebar_hwnd) {
            set_dwm_dark(self.titlebar_hwnd);
        }

        // 跟随系统：系统深浅变化时重应用主题并广播到所有会话。
        let cur_dark = self.effective_dark();
        if cur_dark != self.last_theme_dark {
            self.last_theme_dark = cur_dark;
            apply_theme(ctx, cur_dark);
            self.broadcast_theme();
            self.theme_settle_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
            ctx.request_repaint();
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

        // 帧率控制：前台页签有输出时按配置 FPS，空闲时 1 FPS 基线（保持光标闪烁、
        // 页签图标更新），后台页签/首页/设置页不调度。
        let is_foreground_term = matches!(self.tabs.get(self.current), Some(Tab::Session(_)));
        let redraw_count = self.redraw_rx.try_iter().count();
        if is_foreground_term {
            if redraw_count > 0 {
                // 有 PTY 输出 → 按配置帧率持续刷新（打字回显、TUI 动画）。
                let fps = self.config.settings.refresh_fps.clamp(10, 60);
                ctx.request_repaint_after(std::time::Duration::from_millis(1000 / fps));
            } else {
                // 空闲 → 10 FPS 基线：保持光标闪烁、页签状态图标更新流畅。
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        // 首页/设置页不调度轮询：egui 输入处理会自动唤醒渲染循环。
        // 更新检查结果在用户交互时自然被消费。
        self.bg_frame = self.bg_frame.wrapping_add(1);

        // 更新检查结果回到状态栏。
        if let Ok((msg, latest, download_url)) = self.update_rx.try_recv() {
            self.status = Some(msg);
            self.update_latest = latest;
            self.update_download_url = download_url;
            ctx.request_repaint();
        }
        // 处理下载进度事件：先 take() 取出 rx，处理完再决定是否放回。
        if let Some(rx) = self.download_rx.take() {
            let mut keep_rx = true;
            while let Ok(event) = rx.try_recv() {
                match event {
                    DownloadEvent::Started(pid) => {
                        self.download_pid = Some(pid);
                        ctx.request_repaint();
                    }
                    DownloadEvent::Progress(pct) => {
                        self.download_progress = Some(format!("{pct:.2}%"));
                        ctx.request_repaint();
                    }
                    DownloadEvent::Downloaded(new_exe) => {
                        // 下载完成，写 bat 替换脚本，应用运行中直接热替换（不退出、不重启）
                        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
                        let app_dir = exe.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
                        let exe_name = exe.file_name().unwrap_or_default().to_string_lossy().to_string();
                        if schedule_replace(&new_exe, &app_dir, &exe_name) {
                            self.status = Some("正在替换为新版本，重启应用后生效".to_string());
                            self.download_progress = None;
                            self.download_pid = None;
                            ctx.request_repaint();
                        } else {
                            self.status = Some("替换进程启动失败".to_string());
                            self.download_progress = None;
                            self.download_pid = None;
                            ctx.request_repaint();
                        }
                        keep_rx = false;
                    }
                    DownloadEvent::Failed(msg) => {
                        self.status = Some(msg);
                        self.download_progress = None;
                        self.download_pid = None;
                        ctx.request_repaint();
                        keep_rx = false;
                    }
                }
            }
            if keep_rx {
                self.download_rx = Some(rx);
            }
        }
        let exited = self.update_exited();
        if exited {
            self.status = Some("有会话已退出".to_string());
            ctx.request_repaint();
        }

        // 记录窗口状态，退出时保存。只读需要的三个字段，
        // 不整份 clone ViewportInfo（内含多个 String 字段，每帧一次）。
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
                    let redraw = self.redraw_tx.clone();
                    let ctx = self.ctx.clone();
                    let tx = tx.clone();
                    self.spawning.push((title, false, None));
                    std::thread::spawn(move || {
                        let result = session::spawn(
                            &p.title, &p.dir, &p.cmd,
                            cols as u16, rows as u16,
                            redraw, ctx,
                        );
                        let _ = tx.send((result, false, None));
                    });
                }
                for p in pending_rel {
                    let title = p.title.clone();
                    let tab_idx = p.tab_index;
                    let redraw = self.redraw_tx.clone();
                    let ctx = self.ctx.clone();
                    let tx = tx.clone();
                    self.spawning.push((title, false, Some(tab_idx)));
                    std::thread::spawn(move || {
                        let result = session::spawn(
                            &p.title, &p.dir, &p.cmd,
                            cols as u16, rows as u16,
                            redraw, ctx,
                        );
                        let _ = tx.send((result, false, Some(tab_idx)));
                    });
                }
                self.screen = Screen::Main;
                self.check_updates(true);
            }
            // 轮询后台 spawn 结果：先收进临时列表，再逐条处理，避免 &rx 与 &mut self 借用冲突。
            let mut spawn_results: Vec<(Result<Session, String>, bool, Option<usize>)> = Vec::new();
            if let Some(rx) = &self.spawn_rx {
                while let Ok(item) = rx.try_recv() {
                    self.spawning.pop();
                    spawn_results.push(item);
                }
            }
            for (result, is_restore, saved_index) in spawn_results {
                if is_restore {
                    let themed = self.apply_theme_to(result);
                    if let Some(idx) = saved_index {
                        if let Some(slot) = self.restore_slots.get_mut(idx) {
                            *slot = Some(themed);
                        }
                    }
                } else if let Some(relaunch_idx) = saved_index {
                    match self.apply_theme_to(result) {
                        Ok(sess) => {
                            let idx = relaunch_idx.min(self.tabs.len());
                            self.tabs.insert(idx, Tab::Session(sess));
                            self.current = idx;
                            self.term_focused = true;
                            self.screen = Screen::Main;
                            self.refresh_focus();
                            self.status = Some("已启动".to_string());
                        }
                        Err(e) => {
                            self.status = Some(format!("启动失败: {e}"));
                            self.refresh_focus();
                        }
                    }
                } else {
                    match self.apply_theme_to(result) {
                        Ok(sess) => {
                            self.tabs.push(Tab::Session(sess));
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
            // 全部恢复 spawn 完成后（若尚无恢复任务则 skips 返回），按保存序一次性
            // 追加到页签，再应用上次激活页签。避免逐条插入的越界/顺序错乱。
            if self.spawning.is_empty() && !self.restore_slots.is_empty() {
                let slots = std::mem::take(&mut self.restore_slots);
                let mut restored = 0usize;
                for slot in slots {
                    match slot {
                        Some(Ok(sess)) => {
                            self.tabs.push(Tab::Session(sess));
                            restored += 1;
                        }
                        Some(Err(e)) => {
                            self.status = Some(format!("启动失败: {e}"));
                        }
                        None => {}
                    }
                }
                if restored > 0 {
                    if let Some(active) = self.restore_active.take() {
                        self.current = active.min(self.tabs.len() - 1);
                        self.term_focused = self.current != 0;
                    }
                    self.status = Some(format!("已恢复上次的 {restored} 个终端页签"));
                }
            }
            if self.spawning.is_empty() {
                self.spawn_rx = None;
            }

            // 启动时恢复上次的会话：后台线程 spawn，避免阻塞首帧。
            let pending = std::mem::take(&mut self.pending_restore);
            if !pending.is_empty() {
                let geom = terminal::term_grid_size(ui);
                let (cols, rows) = geom.unwrap_or((80, 24));
                let (tx, rx) = std::sync::mpsc::channel();
                self.spawn_rx = Some(rx);
                let restore_count = pending.len();
                self.restore_slots = (0..restore_count).map(|_| None).collect();
                for (save_i, p) in pending.into_iter().enumerate() {
                    let title = p.title;
                    let dir = p.dir;
                    let cmd = p.cmd;
                    let redraw = self.redraw_tx.clone();
                    let ctx = self.ctx.clone();
                    let tx = tx.clone();
                    self.spawning.push((title.clone(), true, Some(save_i)));
                    std::thread::spawn(move || {
                        let result = session::spawn(
                            &title, &dir, &cmd,
                            cols as u16, rows as u16,
                            redraw, ctx,
                        );
                        // saved_index：恢复时按保存时的原序插入，保证页签顺序不因
                        // 后台 spawn 完成先后而打乱。
                        let _ = tx.send((result, true, Some(save_i)));
                    });
                }
                // 恢复的会话数已知，等后台线程完成后一次性应用 restore_active。
                // 先记下待恢复数，spawn_rx 轮询时消费。
                let _ = restore_count;
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
