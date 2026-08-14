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
    /// 终端仿真状态。
    pub term: Arc<Mutex<Term<SessionListener>>>,
    /// 向 PTY 写入输入的句柄。
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// PTY 主句柄（用于 resize）。
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    /// 子进程。
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    /// 上次渲染的网格尺寸，用于检测是否需要 resize。
    pub grid_size: (u16, u16),
    /// 子进程是否已退出。
    pub exited: bool,
}

/// 终端事件监听器：把终端要求的写回 PTY，并通知界面重绘。
#[derive(Clone)]
pub struct SessionListener {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    redraw: std::sync::mpsc::Sender<()>,
}

impl EventListener for SessionListener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = &event {
            if let Ok(mut w) = self.writer.lock() {
                let _ = w.write_all(text.as_bytes());
            }
        }
        let _ = self.redraw.send(());
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
    redraw: std::sync::mpsc::Sender<()>,
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
    let writer = Arc::new(Mutex::new(writer));

    let listener = SessionListener {
        writer: writer.clone(),
        redraw,
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
        term,
        writer,
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
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut sess = spawn("test", ".", "cmd", 80, 24, tx).expect("spawn 失败");
        {
            let mut w = sess.writer.lock().unwrap();
            let _ = w.write_all(b"echo HELLO123\r");
        }
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
}