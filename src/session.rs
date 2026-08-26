use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;


use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use eframe::egui;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// 一个在应用内页签中运行的终端会话。
pub struct Session {
    /// 页签标题（默认取项目名）。
    pub title: String,
    /// 启动目录。
    pub dir: String,
    /// 本页签启动用的 TUI 命令（切命令后用于标记当前项/重载）。
    pub cmd: String,
    /// 终端仿真状态。
    pub term: Arc<Mutex<Term<SessionListener>>>,
    /// 向 PTY 写入输入的通道发送端（实际写由专用后台线程执行，
    /// 避免子进程不读取输入时阻塞 UI/解析线程）。
    pub writer: std::sync::mpsc::SyncSender<Vec<u8>>,
    /// PTY 主句柄（用于 resize）。
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    /// 子进程。
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    /// 上次渲染的网格尺寸，用于检测是否需要 resize。
    pub grid_size: (u16, u16),
    /// 当前深浅主题（应答 OSC 10/11 颜色查询用，由 UI 线程更新）。
    pub theme_dark: Arc<AtomicBool>,
    /// 是否应答过 OSC 10/11/4 颜色查询（一旦应答，说明该子进程能理解 OSC 颜色，
    /// 主题切换时的主动颜色广播才安全；shell 等从不查询，收到广播只会乱码）。
    pub osc_theme_aware: Arc<AtomicBool>,
    /// 本会话是否为前台页签（UI 线程每帧同步 self.current）。后台会话的
    /// 输出仍照常解析（管道不能停读），但不唤醒 UI 重绘。
    pub foreground: Arc<AtomicBool>,
    /// 子进程是否已退出。
    pub exited: bool,
    /// 上次认领的剪贴板序列号（复制文件后 Ctrl+V 的兜底识别，见 show_terminal）。
    pub last_clipboard_seq: Option<std::num::NonZeroU32>,
    /// 最近一次有输出的绝对时间戳（毫秒），供 UI 精确判定连续输出是否已停。
    pub last_output_ms: Arc<AtomicU64>,
    /// 最近一次有「实际内容」输出的时间戳（毫秒）。排除纯 escape 序列的
    /// TUI 动画帧（光标移动、屏幕重绘），只在有可打印字符输出时更新。
    /// UI 用它区分「TUI 自带动画」（content 静默）vs「正在产生内容」（content 活跃）。
    pub last_content_ms: Arc<AtomicU64>,
    /// 累计输出次数（读取线程写、UI 线程读），用于判断是否有持续输出活动。
    pub output_count: Arc<AtomicU32>,
    /// 该页签是否已显示过「输出结束」对号（点击页签后清除）。
    pub has_been_viewed: Arc<AtomicBool>,
    /// 终端是否处于备用屏（ALT_SCREEN / DECSET 1049）。
    /// htop/vim/opencode/nano 等全屏 TUI 启用，普通 shell 不启用。
    /// 由 terminal::show_terminal 每帧写入，供 app 页签检测 TUI 模式。
    pub alt_screen: Arc<AtomicBool>,
    /// 终端光标是否隐藏（DECSET 25 关闭 / TUI 自行管理光标）。
    /// 每帧渲染时由 terminal::show_terminal 写入，供 app 页签图标判断
    /// TUI 是否处于交互模式（光标可见 = 等待用户输入/选择）。
    pub cursor_hidden: Arc<AtomicBool>,
    /// 解析代数：读取线程每消费一块子进程输出 +1。渲染侧据此判断
    /// caret_scan 缓存是否过期（内容只在解析线程变化）。
    pub parse_gen: Arc<AtomicU64>,
    /// 隐藏光标（DECSET 25 关）时的自绘光标格扫描缓存：
    /// (解析代数, 行, 列)。代数没变就直接复用，
    /// 免去每帧 rows×cols 的全屏网格扫描。offset 恒为 0。
    pub caret_scan: Option<(u64, Line, Column)>,
    /// 渲染帧的网格快照缓冲：跨帧复用避免每帧 rows×cols 次 Vec 分配。
    pub snapshot_scratch: Vec<(Point, Cell)>,
    /// 逐格渲染的 Galley 缓存：键 = (字符, 前景色, 窄格下划线, 宽字符)。
    /// 同一格式每帧只排版一次；上限 8192 条，超出整体清空防膨胀。
    pub galley_cache:
        HashMap<(char, egui::Color32, bool, bool), std::sync::Arc<egui::epaint::Galley>>,
    /// GPU 字形批渲染状态：None = 未初始化或初始化失败（整格走 galley 回落）。
    pub gpu: Option<crate::term_gl::TermGpu>,
}

/// 终端事件监听器：把终端要求的写回 PTY、处理 OSC 52 剪贴板，并通知界面重绘。
#[derive(Clone)]
pub struct SessionListener {
    writer: std::sync::mpsc::SyncSender<Vec<u8>>,
    redraw: std::sync::mpsc::SyncSender<()>,
    ctx: eframe::egui::Context,
    /// 本会话是否为前台页签：后台会话的输出不唤醒 UI，刷新靠基线轮询。
    foreground: std::sync::Arc<AtomicBool>,
}

impl EventListener for SessionListener {
    fn send_event(&self, event: Event) {
        // Event::PtyWrite：仿真器对探测的自发应答默认丢弃（应答权归 reply_to_queries），
        // 但主 DA 应答 \x1b[?6c 必须放行——新版 conpty 启动握手会等它，不给就不吐输出。
        // （其余应答经 ConPTY 输入引擎时序错位时会被当键盘文本打进子进程。）
        if let Event::PtyWrite(text) = &event {
            if text == "\x1b[?6c" {
                let _ = self.writer.try_send(text.as_bytes().to_vec());
            }
        } else if let Event::ClipboardStore(_, text) = &event {
            // opencode 等 TUI 的 OSC 52 复制：直接写系统剪贴板。egui Context 线程安全。
            self.ctx.copy_text(text.clone());
        }
        // Event::PtyWrite（仿真器对 DA/DSR/DECRQM/键盘模式查询的自发应答）一律丢弃：
        // 应答权统一归 reply_to_queries。否则双重应答，且这些应答经 ConPTY 输入
        // 引擎时序错位时会被当键盘文本打进子进程（实测 cmd 提示符后多出 ?6c 尾巴）。
        // 其余 PtyWrite 一律不投递。try_send：通道容量 1，满则丢弃，刷新信号合并为一个。
        // 必须非阻塞：解析线程持有 term 锁时回调这里，若 send 阻塞，
        // 会与 UI 线程的 term.lock() 渲染互相等待而死锁。
        // 后台会话不唤醒 UI：画面反正不可见，切回该页签的那一帧自然会重绘；
        // 页签标题/活动点由 1s 基线轮询更新。解析照常（管道不能停读）。
        if self.foreground.load(Ordering::Relaxed) {
            let _ = self.redraw.try_send(());
            // 直接请求重绘（Context 线程安全）：空闲基线是 1s 轮询，若只靠
            // logic() 里消费通道，PTY 输出刚错过一帧就要等最多 1s 才显示——
            // 打字回显也是 PTY 输出，会明显发粘。这里即时唤醒下一帧。
            self.ctx.request_repaint();
        }
    }
}

/// 响应终端能力探测序列（TUI 启动时常用），返回要写回 PTY 的应答字节。
/// 返回值（字节, 是否应答过 OSC 10/11/4 颜色查询）：后者供主题广播判断
/// 该会话是否 OS 色，避免向 cmd 等不响 OSC 的 shell 推颜色序列。
/// 实测（examples/conpty_probe，conpty.dll 1.25）：宿主写回的应答会被
/// ConPTY 输入引擎消费、从不转发给子进程；DSR 应答总能被干净识别，而
/// 主 DA/XTVERSION 应答在时序错位时会被当键盘文本打进子进程（cmd 提示符
/// 后出现 ^[[?1;2c）。所以只答 DSR/DECRQM/kitty/像素尺寸这类被干净消费的：
/// - DSR 光标位置（ESC[6n → ESC[r;cR）
/// - DECRQM 模式查询（ESC[?...$p → ESC[?...;m$y）
/// - kitty 键盘协议查询（ESC[?u → 回同样 ESC[?u 表示不支持）
/// - XTWINOPS 像素尺寸（ESC[14t，未知时回 0）
/// - OSC 10/11/4 颜色查询（ESC]10;? 等 → rgb 值，随当前主题）
/// 不答主 DA 与 XTVERSION。返回 (应答字节, 是否应答了 OSC 颜色查询)。
/// 查询序列可能跨块被截断，扫描单块即可——探测都发生在启动后的单次写入里。
fn reply_to_queries(term: &Term<SessionListener>, bytes: &[u8], dark: bool) -> Option<(Vec<u8>, bool)> {
    // 高吞吐输出（AI 回答流）的绝大多数块根本没有转义序列：
    // 先做一次快速扫描，无 ESC 字节直接返回，省掉逐字节状态扫描。
    if !bytes.contains(&0x1b) {
        return None;
    }
    let mut out = Vec::new();
    let mut osc_color = false;
    let mut i = 0usize;
    while i + 2 <= bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if bytes[i + 1] == b']' {
            // OSC：\x1b] 数字 ; 内容 终止符(BEL 或 ESC\)。内容以 ? 结尾=查询。
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let osc_num: u32 = bytes[i + 2..j].iter().map(|b| *b as char).collect::<String>().parse().unwrap_or(0);
            if j < bytes.len() && bytes[j] == b';' {
                j += 1;
            }
            let body_start = j;
            while j < bytes.len() && bytes[j] != 0x07 && !(bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\') {
                j += 1;
            }
            let body = &bytes[body_start..j];
            i = (j + 1).min(bytes.len());
            // 颜色查询：\x1b]10;? 前景 / \x1b]11;? 背景 / \x1b]4;N;? 调色板。
            let (fg, bg) = if dark { ("ffffff", "16161a") } else { ("000000", "ffffff") };
            if (osc_num == 10 || osc_num == 11) && body == b"?" {
                osc_color = true;
                let c = if osc_num == 10 { fg } else { bg };
                out.extend_from_slice(
                    format!("\x1b]{osc_num};rgb:{c}/{c}/{c}\x1b\\").as_bytes(),
                );
            } else if osc_num == 4 {
                if let Some(rest) = body.strip_suffix(b";?") {
                    if let Ok(idx) = String::from_utf8_lossy(rest).parse::<u32>() {
                        if idx <= 15 {
                            osc_color = true;
                            out.extend_from_slice(
                                format!("\x1b]4;{idx};rgb:000000/000000/000000\x1b\\").as_bytes(),
                            );
                        }
                    }
                }
            }
            continue;
        }
        if bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        let mut params = String::new();
        while j < bytes.len()
            && (bytes[j].is_ascii_digit() || bytes[j] == b';' || bytes[j] == b'?' || bytes[j] == b'>')
        {
            params.push(bytes[j] as char);
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'$' {
            j += 1; // DECRQM 的中间字节：\x1b[?N$p
        }
        if j >= bytes.len() {
            break; // 序列不完整，等下一块
        }
        let fin = bytes[j];
        i = j + 1;
        if params == "6" && fin == b'n' {
            // DSR 光标位置：汇报视口内光标行列（1-based）。
            let cursor = term.renderable_content().cursor.point;
            out.extend_from_slice(
                format!("\x1b[{};{}R", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes(),
            );
        } else if params.starts_with('?') && fin == b'u' {
            // kitty 键盘协议不支持：按协议回同样的 CSI ? u。
            out.extend_from_slice(b"\x1b[?u");
        } else if params.starts_with('?') && fin == b'p' {
            // DECRQM：1 = 支持且处于该模式，0 = 不识别。
            // 我们能转发 SGR 滚轮（1000/1006）；括号粘贴、同步输出、断字簇按启用答复；
            // 像素鼠标（1016）我们根本不发像素数据，回不识别以免 app 改用像素坐标。
            let n: i64 = params[1..].parse().unwrap_or(-1);
            let state = match n {
                1000 | 1006 | 2004 | 2026 | 2027 => 1,
                _ => 0,
            };
            out.extend_from_slice(format!("\x1b[?{};{}$y", n, state).as_bytes());
        } else if params == "14" && fin == b't' {
            // XTWINOPS 14：像素尺寸未知，回 0 表示知道但无数据。
            out.extend_from_slice(b"\x1b[4;0;0t");
        }
    }
    (!out.is_empty()).then_some((out, osc_color))
}

/// 去掉路径/命令里可能混入的不可见 Unicode 控制符（如复制粘贴带进来的
/// U+202A 双向嵌入符）以及 NUL 字节，避免 CreateProcessW 因非法字符失败。
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(*c, '\0' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

/// 把一条命令字符串拆成程序与参数（支持双引号）。
fn split_command(cmd: &str) -> Vec<String> {
    let cmd = sanitize(cmd);
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in cmd.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            ' ' | '\t' if !in_quote => {
                if !cur.is_empty() {
                    parts.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

#[cfg(windows)]
unsafe extern "system" {
    fn SetDllDirectoryW(lppathname: *const u16) -> i32;
}

/// 捆绑的新版 ConPTY（assets/conpty，取自 VS Code 内置 node-pty 同源构建，1.25 版）：
/// Win10 内置老版 conpty 会吞掉备用屏/鼠标模式声明（?1049h/?1000h），并把宿主写入
/// 的 SGR 滚轮序列改写成乱码，导致全屏 TUI（opencode 等）滚轮转发失效。
/// 首次启动会话时解包到临时目录并用 SetDllDirectoryW 加入 DLL 搜索路径——
/// portable-pty 侧载逻辑按名字 LoadLibrary("conpty.dll") 就会命中它；解包失败
/// 则回退系统内置 conpty（老系统上滚轮转发不可用，其余功能不受影响）。
#[cfg(windows)]
static CONPTY_DLL: &[u8] = include_bytes!("../assets/conpty/conpty.dll");
#[cfg(windows)]
static OPENCONSOLE_EXE: &[u8] = include_bytes!("../assets/conpty/OpenConsole.exe");

#[cfg(windows)]
fn ensure_bundled_conpty() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // 按版本分目录：升级后旧文件不冲突。旧版本残留不清理（TEMP 会自清）。
        let dir = std::env::temp_dir()
            .join("tui-pm-conpty")
            .join(crate::app_version());
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!("解包内置 ConPTY 失败，回退系统内置 conpty");
            return;
        }
        let put = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            // 已存在且大小一致就复用（同版本内容不变），否则重写；失败回退内置。
            if std::fs::metadata(&p).map_or(true, |m| m.len() != bytes.len() as u64)
                && std::fs::write(&p, bytes).is_err()
            {
                return None;
            }
            Some(p)
        };
        if put("conpty.dll", CONPTY_DLL).is_none()
            || put("OpenConsole.exe", OPENCONSOLE_EXE).is_none()
        {
            eprintln!("解包内置 ConPTY 失败，回退系统内置 conpty");
            return;
        }
        #[cfg(windows)]
        unsafe {
            let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
            SetDllDirectoryW(wide.as_ptr());
        }
    });
}

/// 在项目目录下启动一个会话，运行配置的 TUI 命令。
pub fn spawn(
    title: &str,
    dir: &str,
    tui_command: &str,
    cols: u16,
    rows: u16,
    redraw: std::sync::mpsc::SyncSender<()>,
    ctx: eframe::egui::Context,
) -> Result<Session, String> {
    #[cfg(windows)]
    ensure_bundled_conpty();
    let pty_system = native_pty_system();
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system
        .openpty(size)
        .map_err(|e| format!("打开 PTY 失败: {e}"))?;

    let parts = split_command(tui_command);
    if parts.is_empty() {
        return Err("TUI 启动命令为空，请在设置中配置".to_string());
    }
    let mut cmd = CommandBuilder::new(parts[0].clone());
    for arg in &parts[1..] {
        cmd.arg(arg.clone());
    }
    // opencode/node 系 TUI 依据 TERM/COLORTERM 决定是否输出颜色；
    // 不设的话会退化成无彩色渲染。
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let dir = sanitize(dir);
    if !dir.is_empty() {
        cmd.cwd(Path::new(&dir));
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("启动进程失败: {e}"))?;
    drop(pair.slave);

    let writer: Box<dyn Write + Send> = pair
        .master
        .take_writer()
        .map_err(|e| format!("获取 PTY 写入句柄失败: {e}"))?;

    // 专用后台写入线程：UI/解析线程只把字节投递进通道，真正的 write_all
    // 在这里执行。子进程不读取输入（管道缓冲满）时，write_all 只会阻塞
    // 这个后台线程，不会拖死 UI 线程或持有 term 锁的解析线程。
    // 通道有界，写满时 try_send 丢弃新输入（比阻塞整个程序好）。
    let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1024);
    std::thread::spawn(move || {
        let mut writer = writer;
        // ponytail: TUIPM_LOG_WRITES=1 时把所有写入 PTY 的字节打到 stderr，排查杂散输入用。
        let log_writes = std::env::var("TUIPM_LOG_WRITES").is_ok();
        while let Ok(bytes) = writer_rx.recv() {
            if log_writes {
                eprintln!("[pty-write] {:?}", String::from_utf8_lossy(&bytes));
            }
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let redraw_reader = redraw.clone();
    // 前台标记：UI 线程每帧同步 self.current；后台会话输出不唤醒 UI。
    let foreground = Arc::new(AtomicBool::new(false));
    let listener_fg = foreground.clone();
    let listener = SessionListener {
        writer: writer_tx.clone(),
        redraw,
        ctx,
        foreground: listener_fg,
    };

    // 开启 kitty 键盘协议跟踪：应用推 CSI > flags u 时仿真器记下 DISAMBIGUATE 位，
    // terminal.rs 据此决定组合回车是否发 CSI-u（协商过了才发，避免对端不认识
    // 被当字面文本插进输入框）。
    let term_config = Config {
        kitty_keyboard: true,
        ..Default::default()
    };
    let term = Term::new(
        term_config,
        &TermSize::new(cols as usize, rows as usize),
        listener,
    );

    // 通知子进程终端尺寸（部分程序需要）。
    let _ = pair.master.resize(size);

    // 终端能力查询应答（opencode/opentui 启动时会探测终端交互能力：DSR、
    // DECRQM、XTWINOPS 等）。不回会导致 app 判定“非交互终端”，从而不启用
    // 鼠标捕获——滚轮只能滚仿真器自己的滚动缓冲，而这类 TUI 的滚动缓冲里是
    // 整屏重画的原始字节，内容必然错行。按真实终端行为回复即可。
    let reply_tx = writer_tx.clone();
    let theme_dark = Arc::new(AtomicBool::new(true));
    let osc_theme_aware = Arc::new(AtomicBool::new(false));

    let output_count = Arc::new(AtomicU32::new(0));
    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let last_output_ms = Arc::new(AtomicU64::new(now_ts));
    let last_content_ms = Arc::new(AtomicU64::new(now_ts));
    // 读取子进程输出的线程。
    let term = Arc::new(Mutex::new(term));
    let parse_gen = Arc::new(AtomicU64::new(0));
    {
        let term = term.clone();
        let theme_dark = theme_dark.clone();
        let osc_theme_aware = osc_theme_aware.clone();
        let output_count = output_count.clone();
        let last_output_ms = last_output_ms.clone();
        let last_content_ms = last_content_ms.clone();
        let reader_fg = foreground.clone();
        let parse_gen = parse_gen.clone();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("获取 PTY 读取句柄失败: {e}"))?;
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::default();
            let mut buf = [0u8; 0x10_000];
            // 上次计入 output_count 的时间戳，用于去抖：
            // 输入回显是短时间内的突发小块，AI 回答是持续的稳定流。
            let mut last_counted_ms: u64 = 0;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = redraw_reader.send(());
                        break;
                    },
                    Ok(n) => {
                        // 解析代数 +1：渲染侧的自绘光标扫描缓存随之失效。
                        parse_gen.fetch_add(1, Ordering::Relaxed);
                        // 记录输出时间戳（供 UI 判断是否持续输出）。
                        let now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        last_output_ms.store(now_ms, Ordering::Relaxed);
                        // 分析输出内容：区分 TUI 自带动画 vs 实际回答文本。
                        let chunk = &buf[..n];
                        let mut esc_bytes: u32 = 0;
                        let mut printable: u32 = 0;
                        let mut i = 0;
                        while i < chunk.len() {
                            if chunk[i] == 0x1b && i + 1 < chunk.len() && chunk[i + 1] == b'[' {
                                esc_bytes += 2;
                                let mut j = i + 2;
                                while j < chunk.len() && !(0x40..=0x7e).contains(&chunk[j]) {
                                    esc_bytes += 1;
                                    j += 1;
                                }
                                if j < chunk.len() {
                                    esc_bytes += 1;
                                }
                                i = j + 1;
                            } else if (0x20..=0x7E).contains(&chunk[i]) || chunk[i] >= 0xC0 {
                                printable += 1;
                                i += 1;
                            } else {
                                i += 1;
                            }
                        }
                        let total = (esc_bytes + printable) as f64;
                        // TUI 动画帧特征：escape 序列占比高 + 块较小（光标移动/屏幕重绘）。
                        // 着色文本的 escape 占比也可能高（颜色码），但块通常较大。
                        let esc_ratio = if total > 0.0 { esc_bytes as f64 / total } else { 0.0 };
                        let is_animation = total == 0.0
                            || (esc_ratio > 0.5 && n < 200)
                            || esc_ratio > 0.8;
                        if !is_animation {
                            // 非动画输出：更新「实际内容」时间戳，UI 用它区分
                            // TUI 自带动画（光标移动/屏幕重绘）vs 真正的内容输出。
                            last_content_ms.store(now_ms, Ordering::Relaxed);
                            if now_ms.saturating_sub(last_counted_ms) > 300 || printable >= 20 {
                                output_count.fetch_add(1, Ordering::Relaxed);
                                last_counted_ms = now_ms;
                            }
                        }
                        // 分片推进解析器，每片释放一次锁：整块 64KB 全程持锁时，
                        // 高吞吐输出的 TUI 会让每帧来取锁的 UI 渲染线程排队卡顿；
                        // 切成 8KB 小片后渲染线程可在片间插空拿到锁。
                        // vte 的状态机本就支持序列跨 advance 调用续解析，切片安全。
                        const PARSE_SLICE: usize = 8192;
                        for start in (0..n).step_by(PARSE_SLICE) {
                            let end = (start + PARSE_SLICE).min(n);
                            let mut t = term.lock().unwrap();
                            parser.advance(&mut *t, &buf[start..end]);
                        }
                        if let Some((reply, osc_color)) = reply_to_queries(
                            &mut term.lock().unwrap(),
                            &buf[..n],
                            theme_dark.load(Ordering::Relaxed),
                        ) {
                            if osc_color {
                                osc_theme_aware.store(true, Ordering::Relaxed);
                            }
                            let _ = reply_tx.try_send(reply);
                        }
                        // 前台才唤醒 UI 重绘（见 SessionListener::send_event 同款逻辑）。
                        if reader_fg.load(Ordering::Relaxed) {
                            let _ = redraw_reader.send(());
                        }
                    }
                }
            }
        });
    }

    Ok(Session {
        title: title.to_string(),
        dir: dir.to_string(),
        cmd: tui_command.to_string(),
        term,
         writer: writer_tx,
        master: pair.master,
        child,
        grid_size: (cols, rows),
        theme_dark,
        osc_theme_aware,
        foreground,
        exited: false,
        last_clipboard_seq: None,
        output_count,
        last_output_ms,
        last_content_ms,
        has_been_viewed: Arc::new(AtomicBool::new(false)),
        alt_screen: Arc::new(AtomicBool::new(false)),
        cursor_hidden: Arc::new(AtomicBool::new(true)),
        parse_gen,
        caret_scan: None,
        snapshot_scratch: Vec::new(),
        galley_cache: HashMap::new(),
        gpu: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::cell::Flags;

    /// 终端能力应答器：标准 VT 序列的应答都要对（opencode 等 TUI 靠它判定
    /// 终端是否交互）。
    #[test]
    fn reply_to_queries_standard() {
        let listener = SessionListener {
            writer: std::sync::mpsc::sync_channel(1).0,
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let term = Term::new(Config::default(), &TermSize::new(80, 24), listener);
        // 主 DA 与 XTVERSION 有意不应答（会泄漏成键盘输入，见函数注释）；
        // DSR/DECRQM/kitty/像素尺寸照常应答。
        let bytes = b"\x1b[6n\x1b[?2026$p\x1b[?1000$p\x1b[?u\x1b[14t\x1b]11;?\x1b\\";
        let r = reply_to_queries(&term, bytes, true).unwrap().0;
        let s = String::from_utf8_lossy(&r);
        assert!(s.contains("\x1b[1;1R") || s.contains(";1R"), "DSR 光标应答: {s}");
        assert!(s.contains("\x1b[?2026;1$y"), "DECRQM 2026: {s}");
        assert!(s.contains("\x1b[?1000;1$y"), "DECRQM 1000: {s}");
        assert!(s.contains("\x1b[?u"), "kitty 键盘: {s}");
        assert!(s.contains("\x1b[4;0;0t"), "XTWINOPS: {s}");
        assert!(s.contains("\x1b]11;rgb:16161a/16161a/16161a"), "OSC11 深色底: {s}");
        // 主 DA 与 XTVERSION 不再应答。
        assert!(reply_to_queries(&term, b"\x1b[c", true).is_none(), "主 DA 不应答");
        assert!(reply_to_queries(&term, b"\x1b[>0q", true).is_none(), "XTVERSION 不应答");
        // 浅色主题下 OSC 11 回白底。
        let r3 = reply_to_queries(&term, b"\x1b]11;?\x1b\\", false).unwrap();
        assert!(String::from_utf8_lossy(&r3.0).contains("rgb:ffffff/ffffff/ffffff"));
        // 非查询内容不回。
        assert!(reply_to_queries(&term, b"hello", true).is_none());
        // 应答过 OSC 颜色查询 → 第二返回值标记 true（供主题广播判断是否安全）。
        let (_, osc) = reply_to_queries(&term, b"\x1b]10;?\x1b\\", true).unwrap();
        assert!(osc, "OSC 10 颜色查询应答应标记 osc_theme_aware");
        // DSR 应答不应标记 OSC 颜色（否则主题广播会误推给不响 OSC 的会话）。
        let (_, osc2) = reply_to_queries(&term, b"\x1b[6n", true).unwrap();
        assert!(!osc2, "DSR 应答不应标记 OSC 颜色");
    }

    /// 回归：仿真器自发 PtyWrite 应答只放行主 DA（\x1b[?6c，conpty 握手必需），
    /// 其余（DSR/DECRQM/键盘模式等）必须丢弃——应答权归 reply_to_queries，
    /// 否则经 ConPTY 输入引擎时序错位会被当键盘文本打进子进程。
    #[test]
    fn listener_drops_emulator_pty_writes() {
        let (wtx, wrx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
        let listener = SessionListener {
            writer: wtx,
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        use alacritty_terminal::event::Event;
        // 主 DA 应答：放行（conpty 启动握手等它，不给则不吐输出）。
        listener.send_event(Event::PtyWrite("\x1b[?6c".to_string()));
        assert!(
            wrx.recv_timeout(std::time::Duration::from_millis(100)).is_ok(),
            "主 DA 应答 \\x1b[?6c 必须放行"
        );
        // 其余应答：丢弃。
        listener.send_event(Event::PtyWrite("\x1b[1;1R".to_string()));
        listener.send_event(Event::PtyWrite("\x1b[?0u".to_string()));
        assert!(
            wrx.recv_timeout(std::time::Duration::from_millis(100)).is_err(),
            "DSR/键盘模式等仿真器自发应答应被丢弃，不应写入 PTY"
        );
    }

    /// 宽字符由前导格+随空格两格组成；程序（如 nvim）把光标左移一格时光标会
    /// 落在随空格上。若渲染侧按“非 WIDE_CHAR 即 1 格宽”画光标方块，白色方块
    /// 正好盖住汉字右半 → “只显示一半汉字”。渲染代码对随空格按 2 格宽处理。
    #[test]
    fn cursor_can_sit_on_wide_spacer() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::Config as TermConfig;
        let mut term = Term::new(TermConfig::default(), &TermSize::new(80, 24), VoidListener);
        let mut p: alacritty_terminal::vte::ansi::Processor = Default::default();
        // 写一个汉字（2 格：前导+随空格），再左移 1 格 → 光标停在随空格的列。
        p.advance(&mut term, "\u{4f60}\u{1b}[D".as_bytes());
        let pt = term.grid().cursor.point;
        let cell = &term.grid()[pt];
        assert!(
            cell.flags.contains(Flags::WIDE_CHAR_SPACER),
            "光标应停在随空格上 (flags={:?}), 渲染若按 1 格宽画方块就会盖住半个汉字",
            cell.flags
        );
    }
    use std::time::Duration;

    #[test]
    fn sanitize_strips_bidi_and_nul() {
        let s = "\u{202a}D:\\tools\\app.exe\u{0} --flag";
        assert_eq!(sanitize(s), "D:\\tools\\app.exe --flag");
        assert_eq!(split_command(s), vec!["D:\\tools\\app.exe", "--flag"]);
        assert_eq!(sanitize("D:\\projects\\PG数据库性能测试\u{202e}"), "D:\\projects\\PG数据库性能测试");
    }

    #[test]
    fn spawn_run_and_render() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let ctx = eframe::egui::Context::default();
        let mut sess = spawn("test", ".", "cmd", 80, 24, tx, ctx).expect("spawn 失败");
        sess.writer.try_send(b"echo HELLO123\r".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(1500));

        let term = sess.term.lock().unwrap();
        let content = term.renderable_content();
        let mut text = String::new();
        for idx in content.display_iter {
            let cell = idx.cell;
            if cell.c != '\0' && !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                text.push(cell.c);
            }
        }
        assert!(
            text.contains("HELLO123"),
            "终端内容中没有 HELLO123, 实际: {text}"
        );

        let _ = sess.child.kill();
    }

    /// 复现“启动/切换主题自动输入反斜杠”的根因：OSC 应答/广播的 ST 终止符必须是
    /// `\x1b\`（ESC+单个反斜杠）。写成 `\x1b\\\\` 会多出一个 0x5c，被不识 OSC
    /// 的 shell 直接回显（启动答一次=一个 `\`，主题广播两个序列=`\\`）。
    /// 另：不认 OSC 的 shell（cmd）根本不应收到颜色广播（osc_theme_aware 守卫）。
    #[test]
    fn shell_gets_no_stray_backslashes() {
        fn term_text(sess: &Session) -> String {
            let term = sess.term.lock().unwrap();
            let content = term.renderable_content();
            let mut text = String::new();
            for idx in content.display_iter {
                let cell = idx.cell;
                if cell.c != '\0' && !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    text.push(cell.c);
                }
            }
            text
        }

        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let ctx = eframe::egui::Context::default();
        let mut sess = spawn("t", ".", "cmd", 80, 24, tx, ctx).expect("spawn cmd");
        std::thread::sleep(Duration::from_millis(1500));
        let text = term_text(&sess);
        // cmd 提示符本身含反斜杠（D:\…>），只检“提示符后被自动输入的内容”：
        // 最后一行应恰好结束在 `>` 上，之后不得有任何回显字符。
        let last = text.lines().rev().find(|l| !l.trim().is_empty());
        assert!(
            last.is_some_and(|l| l.trim_end().ends_with('>')),
            "启动后 shell 被自动输入了内容（最后一行不是空提示符）:\n{text}"
        );

        // OSC 颜色查询应答的 ST 终止符必须恰好是 ESC+单个反斜杠，
        // 不能多出第二个 0x5c（那就是“自动输入的反斜杠”）。
        let listener = SessionListener {
            writer: std::sync::mpsc::sync_channel(1).0,
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let term = Term::new(Config::default(), &TermSize::new(80, 24), listener);
        let (reply, aware) =
            reply_to_queries(&term, b"\x1b]11;?\x1b\\", true).expect("OSC11 查询应答");
        assert!(aware, "OSC 11 查询应标记 osc_theme_aware");
        let s = String::from_utf8_lossy(&reply);
        assert!(
            s.ends_with("\x1b\\") && !s[..s.len() - 2].contains('\\'),
            "OSC 应答 ST 必须是 ESC+单个反斜杠，实际: {s:?}"
        );
        let _ = sess.child.kill();
    }

    // 捆绑的新版 ConPTY（assets/conpty，取自 VS Code 内置 node-pty 同源构建）
    // 必须双向透传：子进程的备用屏/SGR 鼠标声明要到达仿真器（滚轮转发分支的
    // 判定依据），宿主写入的 SGR 滚轮序列要原样到达子进程。Win10 内置老版
    // conpty 会吞掉 ?1049h/?1000h 声明并把 SGR 滚轮改写成乱码（实测变
    // \x1b[[C）——那正是全屏 TUI（opencode）滚轮翻不动页的根因。
    // node 兼任子进程：把收到的每个输入按 hex 追加到 _sgr_echo.log。
    #[test]
    fn conpty_sgr_wheel_passthrough() {
        // node 未安装时跳过（Windows GUI 机器一般都有 dev 环境）。
        if std::process::Command::new("node")
            .args(["--version"])
            .output()
            .is_err()
        {
            return;
        }
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let ctx = eframe::egui::Context::default();
        // node 的 -e 参数经 portable-pty 命令行拼接后会被空格拆散（脚本含空格），
        // 改用无空格路径的脚本文件：与真实 TUI 的启动方式（单文件可执行）一致。
        let echo_file = "_sgr_echo.mjs";
        let log_file = "_sgr_echo.log";
        let script = concat!(
            "import fs from 'fs';\n",
            "process.stdin.setRawMode(true);\n",
            "process.stdout.write('READY');\n",
            "process.stdout.write('\\u001b[?1049h');\n",
            "process.stdout.write('\\u001b[?1000h\\u001b[?1006h');\n",
            "process.stdin.on('data',d=>fs.appendFileSync('_sgr_echo.log',Buffer.from(d).toString('hex')+'\\n'));\n",
        );
        std::fs::write(echo_file, script).unwrap();
        let _ = std::fs::remove_file(log_file);
        let mut sess = spawn("t", ".", "node _sgr_echo.mjs", 80, 24, tx, ctx).expect("spawn node");
        std::thread::sleep(Duration::from_millis(2000));

        // 输出方向：备用屏与 SGR 鼠标编码位必须穿透到仿真器。
        // 新版 conpty 会自行跟踪并吞掉 ?1000h/?1002h 本体，但透传 ?1006h 编码位。
        let m = sess.term.lock().map(|t| *t.mode()).unwrap_or_default();
        assert!(
            m.contains(alacritty_terminal::term::TermMode::ALT_SCREEN),
            "备用屏切换应穿透到仿真器（bundled conpty.dll 未加载？）"
        );
        assert!(
            m.contains(alacritty_terminal::term::TermMode::SGR_MOUSE),
            "SGR 鼠标编码位应穿透到仿真器（bundled conpty.dll 未加载？）"
        );

        // 输入方向：宿主写入的 SGR 滚轮必须原样到达子进程。
        sess.writer.try_send(b"\x1b[<64;3;12M".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        let log = std::fs::read_to_string(log_file).unwrap_or_default();
        assert!(
            log.contains("1b5b3c36343b333b31324d"),
            "宿主写入的 SGR 滚轮应原样到达子进程（bundled conpty.dll 未加载？）:\n{log}"
        );
        let _ = sess.child.kill();
        let _ = std::fs::remove_file(echo_file);
        let _ = std::fs::remove_file(log_file);
    }

    /// 真实链路验证：本机装有 opencode 时，用它跑「启动 → 仿真器看到鼠标上报
    /// 模式」的完整过程。opencode（opentui）启用 SGR 鼠标上报是 terminal.rs
    /// 滚轮转发分支的触发条件；若这里看不到模式位，说明声明被 ConPTY 吞掉或
    /// 捆绑的 conpty.dll 没生效。
    #[test]
    fn opencode_enables_mouse_reporting() {
        if std::process::Command::new("opencode")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let ctx = eframe::egui::Context::default();
        let mut sess = spawn("t", ".", "opencode", 100, 30, tx, ctx).expect("spawn opencode");
        // opencode 是 node 打包的大程序，冷启动可能要几秒。
        let mut saw_mouse = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(250));
            if sess.child.try_wait().map_or(false, |s| s.is_some()) {
                break;
            }
            if let Ok(t) = sess.term.lock() {
                if t.mode().intersects(
                    alacritty_terminal::term::TermMode::MOUSE_MODE
                        | alacritty_terminal::term::TermMode::SGR_MOUSE,
                ) {
                    saw_mouse = true;
                    break;
                }
            }
        }
        let _ = sess.child.kill();
        assert!(saw_mouse, "10 秒内未观察到 opencode 开启鼠标上报模式");
    }

    #[test]
    fn scroll_display_offset_semantics() {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::grid::Scroll;
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let ctx = eframe::egui::Context::default();
        let mut sess = spawn("test", ".", "cmd", 80, 24, tx, ctx).expect("spawn 失败");
        // 输出 50 行，超出 24 行屏幕后产生历史缓冲。
        sess.writer
            .try_send(b"for /L %i in (1,1,50) do @echo line%i\r".to_vec())
            .unwrap();
        std::thread::sleep(Duration::from_millis(2000));

        let mut term = sess.term.lock().unwrap();
        assert!(
            term.history_size() > 0,
            "输出后应有滚动缓冲, history_size={}",
            term.history_size()
        );
        let top = term.history_size();

        // 向上滚 5 行 -> offset=5。
        term.scroll_display(Scroll::Delta(5));
        assert_eq!(term.grid().display_offset(), 5);

        // 向下滚 3 行 -> offset=2。
        term.scroll_display(Scroll::Delta(-3));
        assert_eq!(term.grid().display_offset(), 2);

        // 滚到底部 -> 实时视图。
        term.scroll_display(Scroll::Bottom);
        assert_eq!(term.grid().display_offset(), 0);

        // 滚到最顶 -> 全部历史。
        term.scroll_display(Scroll::Top);
        assert_eq!(term.grid().display_offset(), top);

        // 超出边界被 clamp。
        term.scroll_display(Scroll::Delta(1000));
        assert_eq!(term.grid().display_offset(), top);
        term.scroll_display(Scroll::Delta(-1000));
        assert_eq!(term.grid().display_offset(), 0);

        let _ = sess.child.kill();
    }
}


