use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
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

/// 回显延迟探针统计（TUIPM_LATENCY_DEBUG=1 启用）：总耗时/次数/峰值，全局共享。
static ECHO_SUM_US: AtomicU64 = AtomicU64::new(0);
static ECHO_CNT: AtomicU64 = AtomicU64::new(0);
static ECHO_MAX_MS: AtomicU32 = AtomicU32::new(0);

fn latency_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("TUIPM_LATENCY_DEBUG").is_ok())
}

/// 一个在应用内页签中运行的终端会话。
pub struct Session {
    /// 页签标题（默认取项目名）。
    pub title: String,
    /// 启动目录。
    pub dir: String,
    /// 本页签启动用的 TUI 命令（切命令后用于标记当前项/重载）。
    pub cmd: String,
    /// 终端仿真状态。
    pub term: Arc<RwLock<Term<SessionListener>>>,
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
    /// 回显延迟探针：最近一次向 PTY 写入输入字节的毫秒时间戳。
    /// 读取线程据此计算「按键 → 首块回显」延迟（TUIPM_LATENCY_DEBUG=1 打印）。
    pub last_input_ms: Arc<AtomicU64>,
    /// 主键按下时的位置（仅 UI 线程用）：快速拖选兜底判定用。
    /// 低帧率下按下/拖动/释放全落在同一帧时，egui 既不判 click 也不判
    /// drag，drag_started_by 永不触发 —— 这里自己记按下点。
    pub drag_press_pos: Option<egui::Pos2>,
    /// 上一帧 IME 预编辑文本（拼音等）：非空时强制刷新快照，避免跳过 clone
    /// 导致输入法组合/提交时内容不同步。
    pub last_preedit: String,
    /// 静止帧缓存：跳过 clone 时复用上一帧的 ANSI 查找表（256 项 Color32）。
    pub cached_ansi_rgb: Option<[egui::Color32; 256]>,
    /// 渲染帧的网格快照缓冲：跨帧复用避免每帧 rows×cols 次 Vec 分配。
    pub snapshot_scratch: Vec<(Point, Cell)>,
    /// 上次 snapshot 时的 parse_gen 值：未变化时跳过 clone，复用上一帧快照。
    pub snapshot_gen: u64,
    /// 本帧有输入发送到 PTY：下一帧强制刷新快照，避免字符回显被跳过。
    pub pending_input: bool,
    /// 缓存的字体度量：(cell_w, cell_h, ppp)。字号/DPI 不变时跳过 fonts_mut 查询。
    pub cached_metrics: Option<(f32, f32, f32)>,
    /// 静止帧缓存：完整渲染结果（bg_shapes + GPU mesh + fg_shapes），
    /// snapshot_changed=false 时直接重放，跳过逐格渲染循环。
    pub cached_render_shapes: Option<Vec<egui::Shape>>,
    /// 逐格渲染的 Galley 缓存：键 = (字符, 前景色, 窄格下划线, 宽字符)。
    /// 同一格式每帧只排版一次；上限 8192 条，超出按 generation 淘汰最旧 25%，
    /// 避免整表清空后首帧全量重建的卡顿峰值。
    pub galley_cache:
        HashMap<(char, egui::Color32, bool, bool), (std::sync::Arc<egui::epaint::Galley>, u64)>,
    /// galley_cache 淘汰代数：每次淘汰 +1，新插入继承当前代数。
    pub galley_gen: u64,
    /// GPU 字形批渲染状态：None = 未初始化或初始化失败（整格走 galley 回落）。
    pub gpu: Option<crate::term_gl::TermGpu>,
    /// 会话是否仍在启动中（首次有实际输出后置 false）。
    /// UI 线程据此显示旋转 ⚙️ 加载动画。
    pub loading: Arc<AtomicBool>,
    /// galley_cache ASCII 快速路径：键 = (char as u8, fg_key, underline)。
    /// 95 个可打印 ASCII × 8 种颜色量化 × 2 种下划线 = 最多 1520 条，
    /// 固定数组 O(1) 查找，跳过 HashMap 的哈希+比较开销。
    pub ascii_galley_slots:
        Option<[(u64, Option<std::sync::Arc<egui::epaint::Galley>>); 1520]>,
}

/// 终端事件监听器：把终端要求的写回 PTY、处理 OSC 52 剪贴板，并通知界面重绘。
#[derive(Clone)]
pub struct SessionListener {
    redraw: std::sync::mpsc::SyncSender<()>,
    ctx: eframe::egui::Context,
    /// 本会话是否为前台页签：后台会话的输出不唤醒 UI，刷新靠基线轮询。
    foreground: std::sync::Arc<AtomicBool>,
}

impl EventListener for SessionListener {
    fn send_event(&self, event: Event) {
        // Event::PtyWrite：仿真器对探测的自发应答一律丢弃。
        // 旧版曾放行主 DA 应答 \x1b[?6c（认为 conpty 握手需要），
        // 但实测 ConPTY 输入引擎处理 DA 时会把 'c' 字符回显到 PTY 输出，
        // 导致 pi/vim 等启动时光标位置多出一个 'C'。ConPTY 自身已能
        // 处理 DA 握手，宿主无需代答。应答权统一归 reply_to_queries。
        if let Event::ClipboardStore(_, text) = &event {
            // opencode 等 TUI 的 OSC 52 复制：直接写系统剪贴板。egui Context 线程安全。
            self.ctx.copy_text(text.clone());
        }
        // Event::PtyWrite（仿真器对 DA/DSR/DECRQM/键盘模式查询的自发应答）一律丢弃：
        // 应答权统一归 reply_to_queries。否则双重应答，且这些应答经 ConPTY 输入
        // 引擎时序错位时会被当键盘文本打进子进程（实测 cmd 提示符后多出 ?6c 尾巴）。
        // 其余 PtyWrite 一律不投递。try_send：通道容量 1，满则丢弃，刷新信号合并为一个。
        // 必须非阻塞：解析线程持有 term 锁时回调这里，若 send 阻塞，
        // 会与 UI 线程的 term 读锁渲染互相等待而死锁。
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
        // 栈上缓冲替代 String::new() + push：CSI 参数通常 <20 字节，
        // 避免每次转义序列触发一次堆分配。
        let mut params_buf = [0u8; 32];
        let mut params_len = 0usize;
        while j < bytes.len()
            && (bytes[j].is_ascii_digit() || bytes[j] == b';' || bytes[j] == b'?' || bytes[j] == b'>')
        {
            if params_len < params_buf.len() {
                params_buf[params_len] = bytes[j];
            }
            params_len += 1;
            j += 1;
        }
        let params = &params_buf[..params_len.min(params_buf.len())];
        if j < bytes.len() && bytes[j] == b'$' {
            j += 1; // DECRQM 的中间字节：\x1b[?N$p
        }
        if j >= bytes.len() {
            break; // 序列不完整，等下一块
        }
        let fin = bytes[j];
        i = j + 1;
        if params == b"6" && fin == b'n' {
            // DSR 光标位置：汇报视口内光标行列（1-based）。
            let cursor = term.renderable_content().cursor.point;
            out.extend_from_slice(
                format!("\x1b[{};{}R", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes(),
            );
        } else if params.starts_with(b"?") && fin == b'u' {
            // kitty 键盘协议不支持：按协议回同样的 CSI ? u。
            out.extend_from_slice(b"\x1b[?u");
        } else if params.starts_with(b"?") && fin == b'p' {
            // DECRQM：1 = 支持且处于该模式，0 = 不识别。
            // 我们能转发 SGR 滚轮（1000/1006）；括号粘贴、同步输出、断字簇按启用答复；
            // 像素鼠标（1016）我们根本不发像素数据，回不识别以免 app 改用像素坐标。
            let n: i64 = std::str::from_utf8(&params[1..])
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1);
            let state = match n {
                1000 | 1006 | 2004 | 2026 | 2027 => 1,
                _ => 0,
            };
            out.extend_from_slice(format!("\x1b[?{};{}$y", n, state).as_bytes());
        } else if params == b"14" && fin == b't' {
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
    let loading = Arc::new(AtomicBool::new(true));
    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let last_output_ms = Arc::new(AtomicU64::new(now_ts));
    let last_content_ms = Arc::new(AtomicU64::new(now_ts));
    let last_input_ms = Arc::new(AtomicU64::new(0));
    // 读取子进程输出的线程。
    let term = Arc::new(RwLock::new(term));
    let parse_gen = Arc::new(AtomicU64::new(0));
    {
        let term = term.clone();
        let theme_dark = theme_dark.clone();
        let osc_theme_aware = osc_theme_aware.clone();
        let output_count = output_count.clone();
        let last_output_ms = last_output_ms.clone();
        let last_content_ms = last_content_ms.clone();
        let reader_input_ms = last_input_ms.clone();
        let reader_fg = foreground.clone();
        let reader_loading = loading.clone();
        let parse_gen = parse_gen.clone();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("获取 PTY 读取句柄失败: {e}"))?;
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::default();
            let mut buf = [0u8; 0x10_000];
            let mut last_counted_ms: u64 = 0;
            // 后台页签攒批缓冲：降低加锁频率，减少对前台的锁竞争。
            let mut pending: Vec<u8> = Vec::new();
            let mut last_flush = std::time::Instant::now();
            const BG_BATCH_BYTES: usize = 4096;
            const BG_BATCH_MS: u64 = 5;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        // 后台攒批剩余数据刷入：单次加锁完成解析。
                        if !pending.is_empty() {
                            parse_gen.fetch_add(1, Ordering::Relaxed);
                            let mut t = term.write().unwrap();
                            parser.advance(&mut *t, &pending);
                            pending.clear();
                        }
                        let _ = redraw_reader.send(());
                        break;
                    },
                    Ok(n) => {
                        parse_gen.fetch_add(1, Ordering::Relaxed);
                        let now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        if latency_debug() {
                            let since = now_ms.saturating_sub(reader_input_ms.load(Ordering::Relaxed));
                            if since > 0 && since <= 500 {
                                ECHO_SUM_US.fetch_add(since * 1000, Ordering::Relaxed);
                                ECHO_CNT.fetch_add(1, Ordering::Relaxed);
                                ECHO_MAX_MS.fetch_max(since as u32, Ordering::Relaxed);
                                let cnt = ECHO_CNT.load(Ordering::Relaxed);
                                if cnt % 20 == 0 {
                                    eprintln!(
                                        "[latency] echo avg={}ms max={}ms n={cnt}",
                                        ECHO_SUM_US.load(Ordering::Relaxed) / cnt / 1000,
                                        ECHO_MAX_MS.load(Ordering::Relaxed),
                                    );
                                }
                            }
                        }
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
                        let esc_ratio = if total > 0.0 { esc_bytes as f64 / total } else { 0.0 };
                        let is_animation = total == 0.0
                            || (esc_ratio > 0.5 && n < 200)
                            || esc_ratio > 0.8;
                        if !is_animation {
                            last_content_ms.store(now_ms, Ordering::Relaxed);
                            // 首次有实际内容输出 → 标记加载完成，停止旋转动画。
                            if reader_loading.load(Ordering::Relaxed) {
                                reader_loading.store(false, Ordering::Relaxed);
                            }
                            if now_ms.saturating_sub(last_counted_ms) > 300 || printable >= 20 {
                                output_count.fetch_add(1, Ordering::Relaxed);
                                last_counted_ms = now_ms;
                            }
                        }
                        // 后台页签：攒批后再加锁解析，降低加锁频率。
                        // 前台页签：立即解析，保证打字回显低延迟。
                        // 关键优化：解析 + 应答合并为单次加锁，原来 N 个 PARSE_SLICE
                        // 分片各加锁一次 + reply_to_queries 再加一次，共 N+1 次；
                        // 合并后只需 1 次，UI 线程 snapshot 等锁的阻塞时间大幅缩短。
                        let is_fg = reader_fg.load(Ordering::Relaxed);
                        if is_fg {
                            let reply = {
                                let mut t = term.write().unwrap();
                                // 整块一次性喂入解析器（parser 内部自己处理块边界）。
                                parser.advance(&mut *t, &buf[..n]);
                                let r = reply_to_queries(
                                    &mut t,
                                    &buf[..n],
                                    theme_dark.load(Ordering::Relaxed),
                                );
                                r // t 在此作用域结束时 drop，释放 term 锁
                            }; // ← term 锁在此释放
                            if let Some((reply, osc_color)) = reply {
                                if osc_color {
                                    osc_theme_aware.store(true, Ordering::Relaxed);
                                }
                                let _ = reply_tx.try_send(reply);
                            }
                            // 锁已释放，send 不会与 UI 线程 term.read() 死锁
                            let _ = redraw_reader.send(());
                        } else {
                            // 后台攒批：攒满 4KB 或超时 5ms 才加锁喂一次。
                            pending.extend_from_slice(&buf[..n]);
                            let elapsed = last_flush.elapsed().as_millis() as u64;
                            if pending.len() >= BG_BATCH_BYTES || elapsed >= BG_BATCH_MS {
                                let reply = {
                                    let mut t = term.write().unwrap();
                                    parser.advance(&mut *t, &pending);
                                    let r = reply_to_queries(
                                        &mut t,
                                        &pending,
                                        theme_dark.load(Ordering::Relaxed),
                                    );
                                    r
                                }; // ← term 锁在此释放
                                if let Some((reply, osc_color)) = reply {
                                    if osc_color {
                                        osc_theme_aware.store(true, Ordering::Relaxed);
                                    }
                                    let _ = reply_tx.try_send(reply);
                                }
                                pending.clear();
                                last_flush = std::time::Instant::now();
                            }
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
        parse_gen: parse_gen.clone(),
        caret_scan: None,
        snapshot_scratch: Vec::new(),
        snapshot_gen: 0,
        pending_input: false,
        galley_cache: HashMap::new(),
        galley_gen: 0,
        gpu: None,
        last_input_ms,
        drag_press_pos: None,
        last_preedit: String::new(),
        cached_ansi_rgb: None,
        cached_metrics: None,
        cached_render_shapes: None,
        loading,
        ascii_galley_slots: None,
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
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let term = Term::new(Config::default(), &TermSize::new(80, 24), listener);
        // 主 DA 与 XTVERSION 有意不应答（泄漏为键盘输入，见函数注释）；
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
    fn listener_drops_all_pty_writes() {
        let listener = SessionListener {
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        use alacritty_terminal::event::Event;
        // 所有 PtyWrite 一律丢弃：主 DA、DSR、键盘模式等。
        // ConPTY 自身处理 DA 握手，宿主代答会导致 'c' 字符回显泄漏。
        listener.send_event(Event::PtyWrite("\x1b[?6c".to_string()));
        listener.send_event(Event::PtyWrite("\x1b[1;1R".to_string()));
        listener.send_event(Event::PtyWrite("\x1b[?0u".to_string()));
        // 没有 writer 通道，PtyWrite 事件不应触发任何写入。
        // 此处验证不 panic 即可（无 writer 字段，事件被完全忽略）。
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
}


