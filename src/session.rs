use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
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
    /// 子进程是否已退出。
    pub exited: bool,
}

/// 终端事件监听器：把终端要求的写回 PTY、处理 OSC 52 剪贴板，并通知界面重绘。
#[derive(Clone)]
pub struct SessionListener {
    writer: std::sync::mpsc::SyncSender<Vec<u8>>,
    redraw: std::sync::mpsc::SyncSender<()>,
    ctx: eframe::egui::Context,
}

impl EventListener for SessionListener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = &event {
            // 只投递到后台写入线程，绝不在解析线程里阻塞写
            //（解析线程持有 term 锁时会回调这里）。
            let _ = self.writer.try_send(text.as_bytes().to_vec());
        } else if let Event::ClipboardStore(_, text) = &event {
            // opencode 等 TUI 的 OSC 52 复制：直接写系统剪贴板。egui Context 线程安全。
            self.ctx.copy_text(text.clone());
        }
        // try_send：通道容量 1，满则丢弃，刷新信号合并为一个。
        // 必须非阻塞：解析线程持有 term 锁时回调这里，若 send 阻塞，
        // 会与 UI 线程的 term.lock() 渲染互相等待而死锁。
        let _ = self.redraw.try_send(());
    }
}

/// 响应终端能力探测序列（TUI 启动时常用），返回要写回 PTY 的应答字节。
/// 返回值（字节, 是否应答过 OSC 10/11/4 颜色查询）：后者供主题广播判断
/// 该会话是否 OS 色，避免向 cmd 等不响 OSC 的 shell 推颜色序列。
/// 全部是标准 VT/xterm 行为，对任意 TUI（cmd/shell/nvim/opencode 等）通用：
/// - DSR 光标位置（ESC[6n → ESC[r;cR）
/// - 主 DA（ESC[c → ESC[?1;2c）与 XTVERSION（ESC[>0q → ESC[>0;136;0c）
/// - DECRQM 模式查询（ESC[?...$p → ESC[?...;m$y）
/// - kitty 键盘协议查询（ESC[?u → 回同样 ESC[?u 表示不支持）
/// - XTWINOPS 像素尺寸（ESC[14t，未知时回 0）
/// - OSC 10/11/4 颜色查询（ESC]10;? 等 → rgb 值，随当前主题）
/// 返回 (应答字节, 是否应答了 OSC 颜色查询)。查询序列可能跨块被截断，扫描
/// 单块即可——探测都发生在启动后的单次写入里。
fn reply_to_queries(term: &Term<SessionListener>, bytes: &[u8], dark: bool) -> Option<(Vec<u8>, bool)> {
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
                    format!("\x1b]{osc_num};rgb:{c}/{c}/{c}\x1b\\\\").as_bytes(),
                );
            } else if osc_num == 4 {
                if let Some(rest) = body.strip_suffix(b";?") {
                    if let Ok(idx) = String::from_utf8_lossy(rest).parse::<u32>() {
                        if idx <= 15 {
                            osc_color = true;
                            out.extend_from_slice(
                                format!("\x1b]4;{idx};rgb:000000/000000/000000\x1b\\\\").as_bytes(),
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
        } else if params.is_empty() && fin == b'c' {
            // 主 DA（VT100 标识）：报支持高级视频属性的终端。
            out.extend_from_slice(b"\x1b[?1;2c");
        } else if params.starts_with('>') && fin == b'q' {
            // XTVERSION（ESC[>0q 等）：报成 xterm 风格，让 TUI 识别为交互终端。
            out.extend_from_slice(b"\x1b[>0;136;0c");
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
        while let Ok(bytes) = writer_rx.recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let listener = SessionListener {
        writer: writer_tx.clone(),
        redraw,
        ctx,
    };

    let term = Term::new(
        Config::default(),
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

    // 读取子进程输出的线程。
    let term = Arc::new(Mutex::new(term));
    {
        let term = term.clone();
        let theme_dark = theme_dark.clone();
        let osc_theme_aware = osc_theme_aware.clone();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("获取 PTY 读取句柄失败: {e}"))?;
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::default();
            let mut buf = [0u8; 0x10_000];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut term = term.lock().unwrap();
                        parser.advance(&mut *term, &buf[..n]);
                        if let Some((reply, osc_color)) = reply_to_queries(
                            &mut *term,
                            &buf[..n],
                            theme_dark.load(Ordering::Relaxed),
                        ) {
                            // 应答过 OSC 颜色查询 = 该子进程懂 OSC 颜色，之后主题
                            // 切换时才有资格收到主动颜色广播（见 app::broadcast_theme）。
                            if osc_color {
                                osc_theme_aware.store(true, Ordering::Relaxed);
                            }
                            // try_send 非阻塞：解析线程持 term 锁，绝不可阻塞等回车。
                            let _ = reply_tx.try_send(reply);
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
        exited: false,
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
        };
        let term = Term::new(Config::default(), &TermSize::new(80, 24), listener);
        let bytes = b"\x1b[6n\x1b[?2026$p\x1b[?1000$p\x1b[?u\x1b[14t\x1b]11;?\x1b\\";
        let r = reply_to_queries(&term, bytes, true).unwrap().0;
        let s = String::from_utf8_lossy(&r);
        assert!(s.contains("\x1b[1;1R"), "DSR 光标应答: {s}");
        assert!(s.contains("\x1b[?2026;1$y"), "DECRQM 2026: {s}");
        assert!(s.contains("\x1b[?1000;1$y"), "DECRQM 1000: {s}");
        assert!(s.contains("\x1b[?u"), "kitty 键盘: {s}");
        assert!(s.contains("\x1b[4;0;0t"), "XTWINOPS: {s}");
        assert!(s.contains("\x1b]11;rgb:16161a/16161a/16161a"), "OSC11 深色底: {s}");
        // 主 DA 与 XTVERSION。
        let r2 = reply_to_queries(&term, b"\x1b[c\x1b[>0q", true).unwrap().0;
        let s2 = String::from_utf8_lossy(&r2);
        assert!(s2.contains("\x1b[?1;2c"), "主 DA: {s2}");
        assert!(s2.contains("\x1b[>0;136;0c"), "XTVERSION: {s2}");
        // 浅色主题下 OSC 11 回白底。
        let r3 = reply_to_queries(&term, b"\x1b]11;?\x1b\\", false).unwrap();
        assert!(String::from_utf8_lossy(&r3.0).contains("rgb:ffffff/ffffff/ffffff"));
        // 非查询内容不回。
        assert!(reply_to_queries(&term, b"hello", true).is_none());
        // 应答过 OSC 颜色查询 → 第二返回值标记 true（供主题广播判断是否安全）。
        let (_, osc) = reply_to_queries(&term, b"\x1b]10;?\x1b\\", true).unwrap();
        assert!(osc, "OSC 10 颜色查询应答应标记 osc_theme_aware");
        let (_, osc2) = reply_to_queries(&term, b"\x1b[c", true).unwrap();
        assert!(!osc2, "主 DA 应答不应标记 OSC 颜色");
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


