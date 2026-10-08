// 探针：ConPTY 输入引擎把哪些 VT 字节序列识别成 End/Ctrl+End。
// cargo test --test conpty_end -- --nocapture --ignored
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::time::Duration;

#[test]
#[ignore]
fn conpty_parses_end_sequences() {
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        unsafe extern "system" {
            fn SetDllDirectoryW(lp_path_name: *const u16) -> i32;
        }
        // 与 session::ensure_bundled_conpty 相同的加载方式（portable-pty 会 LoadLibrary conpty.dll）。
        let dir = std::env::temp_dir().join("tui-pm-conpty-probe");
        std::fs::create_dir_all(&dir).unwrap();
        let base = ["../assets/conpty", "assets/conpty"]
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .copied()
            .expect("assets/conpty not found");
        for f in ["conpty.dll", "OpenConsole.exe"] {
            let bytes = std::fs::read(std::path::Path::new(base).join(f)).unwrap();
            std::fs::write(dir.join(f), &bytes).unwrap();
        }
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        SetDllDirectoryW(wide.as_ptr());
    }

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })
        .unwrap();
    let mut cmd = CommandBuilder::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-Command",
        // 就绪标记 + 连读 8 个键，逐行打印 键名+修饰键
        "Write-Output 'READY'; $i=0; while($i -lt 8){ try { $k=[Console]::ReadKey($true); Write-Output ('GOT '+$k.Key+' mod='+$k.Modifiers) } catch { Write-Output ('ERR '+$_.Exception.Message); break }; $i++ }",
    ]);
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut tmp = [0u8; 4096];
        loop {
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(tmp[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut buf: Vec<u8> = Vec::new();
    let mut drain = |buf: &mut Vec<u8>, ms: u64| {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(chunk) = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            buf.extend_from_slice(&chunk);
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
    };

    let start = std::time::Instant::now();
    loop {
        drain(&mut buf, 2000);
        if buf.windows(5).any(|w| w == b"READY") {
            break;
        }
        if start.elapsed() > Duration::from_secs(40) {
            panic!("powershell 未就绪，输出: {:?}", String::from_utf8_lossy(&buf));
        }
    }

    let seqs: [&[u8]; 8] = [
        b"\x1b[F",     // 我们现在发的 End
        b"\x1b[1;5F",  // xterm 式 Ctrl+End
        b"\x1b[4~",    // xterm 式 End（备选）
        b"\x1b[1;5~",  // xterm 式 Ctrl+End（备选，F1 风格）
        b"\x1b[H",     // 我们现在发的 Home
        b"\x1b[1;5H",  // xterm 式 Ctrl+Home
        b"\x1b[1~",    // xterm 式 Home（备选）
        b"\x1b[5;5~",  // Ctrl+PageUp
    ];
    for s in seqs {
        writer.write_all(s).unwrap();
        writer.flush().unwrap();
        drain(&mut buf, 1200);
    }
    drain(&mut buf, 2000);
    let out = String::from_utf8_lossy(&buf);
    println!("==== raw output ====\n{out}\n====================");
    for got in out.lines().filter(|l| l.contains("GOT")) {
        println!("parsed: {got}");
    }
    let _ = child.kill();
    std::mem::forget(child);
}
