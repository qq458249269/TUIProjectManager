use std::io::{Read, Write};
use std::path::Path;
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

    // 读取子进程输出的线程。
    let term = Arc::new(Mutex::new(term));
    {
        let term = term.clone();
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
        exited: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::cell::Flags;

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


