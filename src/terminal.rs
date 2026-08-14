use std::io::Write;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, Rgb};
use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, Stroke, Vec2};
use portable_pty::PtySize;

use crate::session::Session;

/// 终端内嵌页面使用的等宽字号。
pub const TERM_FONT_SIZE: f32 = 14.0;

/// 在终端中按 Ctrl+B 前缀后产生的应用级命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermCommand {
    GoHome,
    NextTab,
    PrevTab,
    CloseTab,
    SendCtrlB,
}

/// 把 alacritty 颜色解析为 egui 颜色。缺省色时用前景黑/背景白兜底（白色终端页面）。
fn resolve_color(color: Color, colors: &Colors, is_fg: bool) -> Color32 {
    let rgb = match color {
        Color::Spec(rgb) => Some(rgb),
        Color::Indexed(i) => colors[i as usize],
        Color::Named(n) => colors[n],
    };
    match rgb {
        Some(Rgb { r, g, b }) => Color32::from_rgb(r, g, b),
        None if is_fg => Color32::BLACK,
        None => Color32::WHITE,
    }
}

/// 计算 csi 修饰参数：无修饰则用普通 \x1b[A 形式。
fn csi_mod(mod_param: Option<u8>, letter: u8) -> Vec<u8> {
    match mod_param {
        Some(m) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
        None => format!("\x1b[{}", letter as char).into_bytes(),
    }
}

/// F1..F12 编码。
fn fkey(key: egui::Key, mod_param: Option<u8>) -> Vec<u8> {
    let n = key as u8 - egui::Key::F1 as u8 + 1;
    if let Some(m) = mod_param {
        let digits = match n {
            5 => "15",
            6 => "17",
            7 => "18",
            8 => "19",
            9 => "20",
            10 => "21",
            11 => "23",
            _ => "24",
        };
        return format!("\x1b[1;{m}{digits}~").into_bytes();
    }
    match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        _ => b"\x1b[24~".to_vec(),
    }
}

/// 把单个可打印键（含被 shift 烘焙进的符号键）转为 ASCII 字节。
fn key_byte(key: egui::Key, shift: bool) -> Option<u8> {
    let base = key as u8;
    if base >= egui::Key::A as u8 && base <= egui::Key::Z as u8 {
        let i = base - egui::Key::A as u8;
        return Some(if shift { i + b'A' } else { i + b'a' });
    }
    if base >= egui::Key::Num0 as u8 && base <= egui::Key::Num9 as u8 {
        let i = base - egui::Key::Num0 as u8;
        return Some(if shift { b")!@#$%^&*("[i as usize] } else { b'0' + i });
    }
    let b = match key {
        egui::Key::Space => b' ',
        egui::Key::Minus => if shift { b'_' } else { b'-' },
        egui::Key::Equals => if shift { b'+' } else { b'=' },
        egui::Key::Comma => if shift { b'<' } else { b',' },
        egui::Key::Period => if shift { b'>' } else { b'.' },
        egui::Key::Slash => if shift { b'?' } else { b'/' },
        egui::Key::Backslash => if shift { b'|' } else { b'\\' },
        egui::Key::Backtick => if shift { b'~' } else { b'`' },
        egui::Key::OpenBracket => if shift { b'{' } else { b'[' },
        egui::Key::CloseBracket => if shift { b'}' } else { b']' },
        egui::Key::Quote => if shift { b'"' } else { b'\'' },
        egui::Key::Semicolon => if shift { b':' } else { b';' },
        egui::Key::Pipe => b'|',
        egui::Key::Questionmark => b'?',
        egui::Key::Exclamationmark => b'!',
        egui::Key::OpenCurlyBracket => b'{',
        egui::Key::CloseCurlyBracket => b'}',
        egui::Key::Colon => b':',
        egui::Key::Plus => b'+',
        _ => return None,
    };
    Some(b)
}

/// 编码普通字符键（字母/数字/标点）上的 ctrl/alt 组合。
fn encode_char_key(key: egui::Key, ctrl: bool, alt: bool, shift: bool) -> Option<Vec<u8>> {
    let b = key_byte(key, shift)?;
    if ctrl && !alt {
        if b == b' ' {
            return Some(vec![0]);
        }
        let lc = b.to_ascii_lowercase();
        if lc.is_ascii() && (b'a'..=b'z').contains(&lc) {
            return Some(vec![lc - b'a' + 1]);
        }
        return Some(match lc {
            b'[' => vec![0x1b],
            b'\\' => vec![0x1c],
            b']' => vec![0x1d],
            b'^' => vec![0x1e],
            b'_' => vec![0x1f],
            _ => vec![b],
        });
    }
    if alt && !ctrl {
        let mut v = vec![0x1b];
        v.push(b);
        return Some(v);
    }
    if ctrl && alt {
        let mut v = vec![0x1b];
        let lc = b.to_ascii_lowercase();
        if lc.is_ascii() && (b'a'..=b'z').contains(&lc) {
            v.push(lc - b'a' + 1);
        } else {
            v.push(b);
        }
        return Some(v);
    }
    None
}

/// 把 egui 按键编码为发送给 PTY 的字节序列。
/// 纯可打印字符（无 ctrl/alt）返回 None，由 Event::Text 处理，避免重复输入。
fn encode_key(key: egui::Key, ctrl: bool, alt: bool, shift: bool, _repeat: bool) -> Option<Vec<u8>> {
    let mod_param = if shift && !ctrl && !alt {
        Some(2)
    } else if alt && !shift && !ctrl {
        Some(3)
    } else if shift && alt && !ctrl {
        Some(4)
    } else if ctrl && !shift && !alt {
        Some(5)
    } else if ctrl && shift && !alt {
        Some(6)
    } else if ctrl && alt && !shift {
        Some(7)
    } else if ctrl && alt && shift {
        Some(8)
    } else {
        None
    };

    match key {
        egui::Key::Enter => Some(vec![b'\r']),
        egui::Key::Backspace => Some(vec![0x7f]),
        egui::Key::Escape => Some(vec![0x1b]),
        egui::Key::Tab => Some(if shift { b"\x1b[Z".to_vec() } else { vec![b'\t'] }),
        egui::Key::Home => Some(vec![0x1b, b'[', b'H']),
        egui::Key::End => Some(vec![0x1b, b'[', b'F']),
        egui::Key::PageUp => Some(b"\x1b[5~".to_vec()),
        egui::Key::PageDown => Some(b"\x1b[6~".to_vec()),
        egui::Key::Insert => Some(b"\x1b[2~".to_vec()),
        egui::Key::Delete => Some(b"\x1b[3~".to_vec()),
        egui::Key::ArrowUp => Some(csi_mod(mod_param, b'A')),
        egui::Key::ArrowDown => Some(csi_mod(mod_param, b'B')),
        egui::Key::ArrowRight => Some(csi_mod(mod_param, b'C')),
        egui::Key::ArrowLeft => Some(csi_mod(mod_param, b'D')),
        egui::Key::Copy | egui::Key::Cut | egui::Key::Paste => None,
        _ => {
            let base = key as u8;
            if base >= egui::Key::F1 as u8 && base <= egui::Key::F20 as u8 {
                Some(fkey(key, mod_param))
            } else if ctrl || alt {
                encode_char_key(key, ctrl, alt, shift)
            } else {
                None
            }
        }
    }
}

/// 渲染一个终端会话（网格 + 光标），并把终端获得焦点时的键盘输入写回 PTY。
/// 返回需要由应用层执行的前缀命令（Ctrl+B 之后按下的 h/n/p/x/b 等）。
pub fn show_terminal(
    ui: &mut egui::Ui,
    sess: &mut Session,
    prefix_active: &mut bool,
    status: &mut Option<String>,
    term_focused: &mut bool,
) -> Vec<TermCommand> {
    let mut cmds = Vec::new();

    let font_id = FontId::monospace(TERM_FONT_SIZE);
    let cell_w = ui.ctx().fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let cell_h = ui.ctx().fonts_mut(|f| f.row_height(&font_id));
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return cmds;
    }

    let avail = ui.available_size();
    let cols = (avail.x / cell_w).floor().max(1.0) as usize;
    let rows = (avail.y / cell_h).floor().max(1.0) as usize;

    if sess.grid_size != (cols as u16, rows as u16) {
        if let Ok(mut t) = sess.term.lock() {
            t.resize(TermSize::new(cols, rows));
        }
        let _ = sess.master.resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
        sess.grid_size = (cols as u16, rows as u16);
    }

    let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click());
    if resp.clicked() {
        *term_focused = true;
        resp.request_focus();
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::WHITE);

    let mut bytes_out: Vec<Vec<u8>> = Vec::new();

    if *term_focused {
        let events = ui.input(|i| i.events.clone());
        let alt_down = ui.input(|i| i.modifiers.alt);
        for ev in &events {
            match ev {
                egui::Event::Text(text) => {
                    if *prefix_active || alt_down || text.is_empty() {
                        continue;
                    }
                    bytes_out.push(text.as_bytes().to_vec());
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let ctrl = modifiers.ctrl || modifiers.command;
                    let alt = modifiers.alt;
                    let shift = modifiers.shift;

                    if *prefix_active {
                        match key {
                            egui::Key::Escape => {
                                *prefix_active = false;
                                *status = Some("已退出前缀模式".to_string());
                            }
                            egui::Key::H | egui::Key::Num0 => {
                                cmds.push(TermCommand::GoHome);
                                *prefix_active = false;
                            }
                            egui::Key::N => {
                                cmds.push(TermCommand::NextTab);
                                *prefix_active = false;
                            }
                            egui::Key::P => {
                                cmds.push(TermCommand::PrevTab);
                                *prefix_active = false;
                            }
                            egui::Key::X => {
                                cmds.push(TermCommand::CloseTab);
                                *prefix_active = false;
                            }
                            egui::Key::B => {
                                cmds.push(TermCommand::SendCtrlB);
                                *prefix_active = false;
                            }
                            _ => {
                                *prefix_active = false;
                                *status = Some("已退出前缀模式".to_string());
                            }
                        }
                        continue;
                    }

                    if ctrl && !alt && *key == egui::Key::B {
                        *prefix_active = true;
                        *status = Some(
                            "Ctrl+B 前缀: h 首页  n 下一个  p 上一个  x 关闭  b 发送Ctrl+B  Esc 取消"
                                .to_string(),
                        );
                        continue;
                    }

                    if let Some(bytes) = encode_key(*key, ctrl, alt, shift, false) {
                        bytes_out.push(bytes);
                    }
                }
                egui::Event::Copy => {
                    if !*prefix_active {
                        bytes_out.push(vec![0x03]); // Ctrl+C
                    }
                }
                egui::Event::Cut => {
                    if !*prefix_active {
                        bytes_out.push(vec![0x18]); // Ctrl+X
                    }
                }
                egui::Event::Paste(text) => {
                    if !*prefix_active && !text.is_empty() {
                        bytes_out.push(text.as_bytes().to_vec());
                    }
                }
                _ => {}
            }
        }
    }

    if !bytes_out.is_empty() {
        if let Ok(mut w) = sess.writer.lock() {
            for b in &bytes_out {
                let _ = w.write_all(b);
            }
        }
    }

    // ---- 渲染网格 ----
    let term = match sess.term.lock() {
        Ok(t) => t,
        Err(_) => return cmds,
    };
    let content = term.renderable_content();
    let offset = content.display_offset;
    let colors = &content.colors;
    let cursor = content.cursor;

    let mut job = egui::text::LayoutJob::default();
    job.break_on_newline = true;
    job.wrap.max_width = cols as f32 * cell_w;
    job.wrap.max_rows = rows;

    let mut last_vline: Option<i32> = None;
    for indexed in content.display_iter {
        let point = indexed.point;
        let vline = point.line.0 - offset as i32;
        if vline < 0 || vline >= rows as i32 {
            continue;
        }
        if let Some(prev) = last_vline {
            if prev != vline {
                job.append("\n", 0.0, egui::TextFormat::default());
            }
        }
        last_vline = Some(vline);

        let cell = indexed.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let ch = if cell.c == '\0' { ' ' } else { cell.c };

        let fg = resolve_color(cell.fg, colors, true);
        let bg = resolve_color(cell.bg, colors, false);
        let (fg, bg) = if cell.flags.contains(Flags::INVERSE) {
            (bg, fg)
        } else {
            (fg, bg)
        };

        let mut format = egui::TextFormat {
            font_id: font_id.clone(),
            color: fg,
            background: bg,
            underline: Stroke::NONE,
            ..Default::default()
        };
        if cell.flags.contains(Flags::UNDERLINE) {
            format.underline = Stroke::new(1.0, fg);
        }
        job.append(&ch.to_string(), 0.0, format);
    }

    let galley = painter.layout_job(job);
    painter.galley(rect.left_top(), galley, Color32::WHITE);

    // 光标（方块）
    if term.mode().contains(TermMode::SHOW_CURSOR)
        && !matches!(
            cursor.shape,
            alacritty_terminal::vte::ansi::CursorShape::Hidden
        )
    {
        let p = cursor.point;
        let vline = p.line.0 - offset as i32;
        if vline >= 0 && vline < rows as i32 {
            let col = p.column.0 as usize;
            if col < cols {
                let cursor_cell = &term.grid()[cursor.point];
                let fg = resolve_color(cursor_cell.fg, colors, true);
                let x = rect.left() + col as f32 * cell_w;
                let y = rect.top() + vline as f32 * cell_h;
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h)),
                    0.0,
                    fg,
                );
            }
        }
    }

    // 会话活跃时由 SessionListener 的后台解析线程通过 redraw 信号触发重绘；
    // 这里不需要无条件 request_repaint，避免空闲时满帧空转。

    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_bytes(k: egui::Key, ctrl: bool, alt: bool, shift: bool) -> Option<Vec<u8>> {
        encode_key(k, ctrl, alt, shift, false)
    }

    #[test]
    fn plain_printable_is_none() {
        assert_eq!(key_bytes(egui::Key::A, false, false, false), None);
        assert_eq!(key_bytes(egui::Key::A, false, false, true), None);
        assert_eq!(key_bytes(egui::Key::Num3, false, false, true), None);
    }

    #[test]
    fn special_keys() {
        assert_eq!(key_bytes(egui::Key::Enter, false, false, false), Some(vec![b'\r']));
        assert_eq!(key_bytes(egui::Key::Backspace, false, false, false), Some(vec![0x7f]));
        assert_eq!(key_bytes(egui::Key::Escape, false, false, false), Some(vec![0x1b]));
        assert_eq!(key_bytes(egui::Key::Tab, false, false, false), Some(vec![b'\t']));
        assert_eq!(key_bytes(egui::Key::Tab, false, false, true), Some(b"\x1b[Z".to_vec()));
        assert_eq!(key_bytes(egui::Key::Home, false, false, false), Some(b"\x1b[H".to_vec()));
        assert_eq!(key_bytes(egui::Key::PageUp, false, false, false), Some(b"\x1b[5~".to_vec()));
        assert_eq!(key_bytes(egui::Key::F1, false, false, false), Some(b"\x1bOP".to_vec()));
        assert_eq!(key_bytes(egui::Key::F5, false, false, false), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn arrows_with_modifiers() {
        assert_eq!(key_bytes(egui::Key::ArrowUp, false, false, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes(egui::Key::ArrowUp, true, false, false), Some(b"\x1b[1;5A".to_vec()));
        assert_eq!(key_bytes(egui::Key::ArrowUp, false, false, true), Some(b"\x1b[1;2A".to_vec()));
        assert_eq!(key_bytes(egui::Key::ArrowUp, false, true, false), Some(b"\x1b[1;3A".to_vec()));
        assert_eq!(key_bytes(egui::Key::ArrowDown, true, false, false), Some(b"\x1b[1;5B".to_vec()));
        assert_eq!(key_bytes(egui::Key::ArrowRight, true, false, false), Some(b"\x1b[1;5C".to_vec()));
        assert_eq!(key_bytes(egui::Key::ArrowLeft, true, false, false), Some(b"\x1b[1;5D".to_vec()));
    }

    #[test]
    fn ctrl_letters() {
        assert_eq!(key_bytes(egui::Key::A, true, false, false), Some(vec![0x01]));
        assert_eq!(key_bytes(egui::Key::C, true, false, false), Some(vec![0x03]));
        assert_eq!(key_bytes(egui::Key::Z, true, false, false), Some(vec![0x1a]));
        assert_eq!(key_bytes(egui::Key::Space, true, false, false), Some(vec![0x00]));
        // ctrl+shift+M -> \r
        assert_eq!(key_bytes(egui::Key::M, true, false, true), Some(vec![0x0d]));
    }

    #[test]
    fn alt_letters() {
        assert_eq!(key_bytes(egui::Key::A, false, true, false), Some(b"\x1ba".to_vec()));
        assert_eq!(key_bytes(egui::Key::A, false, true, true), Some(b"\x1bA".to_vec()));
        assert_eq!(key_bytes(egui::Key::Num5, false, true, false), Some(b"\x1b5".to_vec()));
        // alt+ctrl+a -> ESC + 0x01
        assert_eq!(key_bytes(egui::Key::A, true, true, false), Some(b"\x1b\x01".to_vec()));
    }
}
