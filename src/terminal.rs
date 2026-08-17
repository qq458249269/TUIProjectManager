use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection as TermSelection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, Stroke, Vec2};
use portable_pty::PtySize;

use crate::session::{Session, SessionListener};

/// 终端内嵌页面使用的等宽字号。
pub const TERM_FONT_SIZE: f32 = 14.0;

/// 深浅主题下的终端画布底色。
const TERM_BG_DARK: Color32 = Color32::from_rgb(22, 22, 26);
const TERM_BG_LIGHT: Color32 = Color32::WHITE;

fn color_for(dark: bool, dark_c: Color32, light_c: Color32) -> Color32 {
    if dark { dark_c } else { light_c }
}

/// 把 alacritty 颜色解析为 egui 颜色。缺省色时按主题兜底：
/// 深色=白字深底，浅色=黑字白底（与深浅切换同步）。
fn resolve_color(color: Color, colors: &Colors, is_fg: bool, dark: bool) -> Color32 {
    let rgb = match color {
        Color::Spec(rgb) => Some(rgb),
        Color::Indexed(i) => colors[i as usize],
        Color::Named(n) => colors[n],
    };
    match rgb {
        Some(Rgb { r, g, b }) => Color32::from_rgb(r, g, b),
        None if is_fg => color_for(dark, Color32::WHITE, Color32::BLACK),
        None => color_for(dark, TERM_BG_DARK, TERM_BG_LIGHT),
    }
}

/// 浅色主题下把暗色主题 TUI 的配色映射为可读组合：
/// 深背景 → 白；亮灰/白前景 → 黑；过亮的饱和前景（亮黄、亮青、亮绿、亮紫…）
/// 在白底上对比不足，按通道等比压暗（保留色相）到半亮度保证可读。
fn adapt_to_light(fg: Color32, bg: Color32) -> (Color32, Color32) {
    let lum = |c: Color32| {
        0.2126 * c.r() as f32 / 255.0
            + 0.7152 * c.g() as f32 / 255.0
            + 0.0722 * c.b() as f32 / 255.0
    };
    let neutral = |c: Color32| {
        let max = c.r().max(c.g()).max(c.b()) as f32;
        let min = c.r().min(c.g()).min(c.b()) as f32;
        max - min < 32.0
    };
    let mut f = fg;
    let mut b = bg;
    if lum(b) < 0.30 {
        b = TERM_BG_LIGHT;
    }
    if lum(f) > 0.72 && neutral(f) {
        f = Color32::BLACK;
    } else if lum(f) > 0.55 {
        // 亮饱和色（如 ANSI 亮黄 #FFFF00）白底上几乎看不见，压到半亮度：
        // 色相不变，对比足够，且不会被误打成黑色。
        f = Color32::from_rgb(
            (f.r() as f32 * 0.5) as u8,
            (f.g() as f32 * 0.5) as u8,
            (f.b() as f32 * 0.5) as u8,
        );
    }
    (f, b)
}

/// 终端右键菜单动作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TermAction {
    /// 复制选中文本到剪贴板。
    Copy,
    /// 从剪贴板粘贴到终端。
    Paste,
    /// 清空当前输入行内容（等效按住退格键直到清空）。
    ClearInput,
}

/// 有选中文本时复制到系统剪贴板（并清除选区），返回是否复制了内容。
fn copy_selection(
    term: &alacritty_terminal::term::Term<SessionListener>,
    ctx: &egui::Context,
    status: &mut Option<String>,
) -> bool {
    // 复制前先取字符串再释放锁（selection_to_string 内部也会拿锁）。
    let text = term.selection_to_string();
    match text {
        Some(text) if !text.trim().is_empty() => {
            let text = text
                .lines()
                .map(|l| l.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
            ctx.copy_text(text);
            *status = Some("已复制选中的文本".to_string());
            true
        }
        _ => false,
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

/// 宽字符视觉槽：返回（槽起始列, 槽宽格数）。宽字符前导格 2 格从自身列开始；
/// 随空格连回前导格占满 2 格；其余窄格 1 格。背景/选中高亮按下槽绘制，保证选
/// 区边界落在宽字符任何一列时整个汉字同色（不出现半字异色）。
fn cjk_slot(col: usize, wide: bool, spacer: bool) -> (usize, usize) {
    if wide {
        (col, 2)
    } else if spacer {
        (col.saturating_sub(1), 2)
    } else {
        (col, 1)
    }
}

/// 判断单元格是否在选区内（宽字符前导格在选区右边界落在其空格上时也算选中）。
fn cell_selected(range: &SelectionRange, point: Point, cell: &Cell) -> bool {
    range.contains(point)
        || (cell.flags.contains(Flags::WIDE_CHAR)
            && range.contains(Point::new(point.line, point.column + 1)))
}

/// 渲染一个终端会话（网格 + 光标），并把终端获得焦点时的键盘输入写回 PTY。
pub fn show_terminal(
    ui: &mut egui::Ui,
    sess: &mut Session,
    dark: bool,
    status: &mut Option<String>,
    term_focused: &mut bool,
) {
    let font_id = FontId::monospace(TERM_FONT_SIZE);
    let cell_w = ui.ctx().fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let cell_h = ui.ctx().fonts_mut(|f| f.row_height(&font_id));
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
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

    let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
    let term_id = resp.id;
    if resp.clicked() {
        *term_focused = true;
        resp.request_focus();
    }

    // IME 归属：终端聚焦，且没有其他 egui 控件（如输入弹窗里的 TextEdit）
    // 持有键盘焦点时，才把系统输入法交给终端。
    let owns_ime = *term_focused
        && ui.memory(|m| m.focused().map_or(true, |id| id == term_id));

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, color_for(dark, TERM_BG_DARK, TERM_BG_LIGHT));

    // 鼠标滚轮查看滚动缓冲：只需悬停在终端区域，不需要终端获得焦点。
    // 直接复用 egui 的平滑滚动量（与 ScrollArea 方向一致：正值=向上滚=更早内容），
    // 并消费掉避免被后续控件重复使用；滚动后立即重绘，不等 0.3s 基线。
    if resp.hovered() {
        let delta = ui.input(|i| i.smooth_scroll_delta.y);
        if delta != 0.0 {
            ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
            // 平滑滚动量常 <1 行，按行高折算并保证至少 1 行，否则触摸板/精密滚轮无反应。
            let mut lines = (delta / cell_h).round() as i32;
            if lines == 0 {
                lines = if delta > 0.0 { 1 } else { -1 };
            }
            // 应用开启了鼠标上报（opencode 聊天列表等）→ 把滚轮转成 SGR 事件发给
            // 应用让它自己滚；否则滚动仿真器自身的历史缓冲（普通 shell / vim）。
            // 注意：opencode 在备用屏（alt screen）下无历史缓冲，这条分支是它唯一的滚屏途径。
            let mouse_on = sess
                .term
                .lock()
                .map(|t| t.mode().intersects(TermMode::MOUSE_MODE))
                .unwrap_or(false);
            if mouse_on {
                let pos = ui.input(|i| i.pointer.latest_pos()).unwrap_or(rect.center());
                let col = (((pos.x - rect.left()).max(0.0) / cell_w) as usize + 1).clamp(1, cols);
                let row = (((pos.y - rect.top()).max(0.0) / cell_h) as usize + 1).clamp(1, rows);
                let up = lines > 0;
                for _ in 0..lines.abs().clamp(1, 32) {
                    let b = if up { 64 } else { 65 };
                    let _ = sess.writer.try_send(format!("\x1b[<{b};{col};{row}M").into_bytes());
                }
            } else if let Ok(mut t) = sess.term.lock() {
                t.scroll_display(Scroll::Delta(lines));
            }
            ui.ctx().request_repaint();
        }
    }

    // 文本选择：左键拖拽选中（选区由 alacritty 核心维护，随内容滚动/换行自动
    // 跟随），左键单击清除选择，右键复制选中文本。
    if let Ok(mut t) = sess.term.lock() {
        let offset = t.grid().display_offset();
        let point_at = |pos: Pos2| -> Option<Point> {
            let col = ((pos.x - rect.left()) / cell_w).floor() as i64;
            let row = ((pos.y - rect.top()) / cell_h).floor() as i64;
            if row < 0 || col < 0 {
                return None;
            }
            Some(Point::new(
                Line(row as i32 - offset as i32),
                Column((col as usize).min(cols.saturating_sub(1))),
            ))
        };
        if resp.drag_started_by(egui::PointerButton::Primary) {
            if let Some(pos) = resp.interact_pointer_pos().and_then(point_at) {
                t.selection =
                    Some(TermSelection::new(SelectionType::Simple, pos, Side::Left));
            }
        } else if resp.dragged_by(egui::PointerButton::Primary) {
            if let Some(pos) = resp.interact_pointer_pos().and_then(point_at) {
                if let Some(sel) = t.selection.as_mut() {
                    sel.update(pos, Side::Left);
                }
            }
        }
        if resp.clicked() {
            t.selection = None;
        }
    }

    // 右键弹菜单：复制 / 粘贴 / 清空输入（不再右键直接粘贴，避免误触）。
    // context_menu 闭包内再拿 term 锁；必须在上面 lock 块之外调用，否则同一
    // 帧先锁后闭包再锁会死锁。
    let mut menu_action: Option<TermAction> = None;
    resp.context_menu(|ui| {
        let has_selection = sess
            .term
            .lock()
            .map(|t| t.selection.is_some())
            .unwrap_or(false);
        if ui
            .add_enabled(has_selection, egui::Button::new("📋 复制"))
            .on_hover_text("复制选中的文本到剪贴板")
            .clicked()
        {
            menu_action = Some(TermAction::Copy);
            ui.close();
        }
        if ui
            .button("📥 粘贴")
            .on_hover_text("从剪贴板粘贴到终端")
            .clicked()
        {
            menu_action = Some(TermAction::Paste);
            ui.close();
        }
        ui.separator();
        if ui
            .button("🧹 清空输入")
            .on_hover_text("清除当前输入行内容（等效按住退格键直到清空）")
            .clicked()
        {
            menu_action = Some(TermAction::ClearInput);
            ui.close();
        }
    });
    match menu_action {
        Some(TermAction::Copy) => {
            let copied = sess
                .term
                .lock()
                .map(|mut t| {
                    let copied = copy_selection(&t, ui.ctx(), status);
                    if copied {
                        t.selection = None;
                    }
                    copied
                })
                .unwrap_or(false);
            if !copied {
                *status = Some("没有可复制的选中文本".to_string());
            }
        }
        Some(TermAction::Paste) => {
            // 剪贴板有文件（Explorer 复制/剪切）→ 直接填路径；否则走系统文本
            // 粘贴（清掉选区，下一帧 egui 投递 Event::Paste，聚焦终端后可写入）。
            let files = crate::clipboard::clipboard_files();
            if files.is_empty() {
                if let Ok(mut t) = sess.term.lock() {
                    t.selection = None;
                }
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                *term_focused = true;
            } else {
                let _ = sess.writer.try_send(files.join(" ").into_bytes());
            }
        }
        Some(TermAction::ClearInput) => {
            // 等效按住退格键直到清空：从视口顶行到光标处整块统计字符数，再发送
            // 等量退格（0x7f，与应用内 Backspace 键编码一致）。
            // - 空格、换行/折行到上一行的输入都计入，全部清除；
            // - 宽字符占位格跳过，CJK 按字符数而非显示格数计；
            // - 空单元格(' ')多算无害：多余退格在空输入行上不被 shell 处理，
            //   宁可多删（安全方向）也不可少删（漏空格/漏上一行正是此前的 bug）；
            // - 光标各列不重叠：光标所在行数到光标列为止，其余行数整行。
            let count = sess
                .term
                .lock()
                .map(|t| {
                    let cur = t.renderable_content().cursor.point;
                    let offset = t.grid().display_offset();
                    let mut n = 0usize;
                    for r in 0..rows {
                        let line = Line(r as i32 - offset as i32);
                        let max_col = if line == cur.line {
                            cur.column.0.min(cols)
                        } else {
                            cols
                        };
                        for col in 0..max_col {
                            let cell = &t.grid()[Point::new(line, Column(col))];
                            if cell.c != '\0'
                                && !cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                            {
                                n += 1;
                            }
                        }
                    }
                    n
                })
                .unwrap_or(0);
            if count > 0 {
                let _ = sess.writer.try_send(vec![0x7f; count]);
                *status = Some("已清空当前输入".to_string());
            } else {
                *status = Some("当前输入为空，无需清空".to_string());
            }
        }
        None => {}
    }

    let mut bytes_out: Vec<Vec<u8>> = Vec::new();
    let mut preedit = String::new();

    if *term_focused {
        // 剪贴板文件粘贴（Ctrl+V，winit 消息钩子置位，见 clipboard.rs）：
        // 用户复制/剪切了文件 → 把路径空格拼接直接填入，不走系统文本粘贴。
        if crate::clipboard::FILES_PASTE_REQUESTED.swap(false, std::sync::atomic::Ordering::Relaxed) {
            let paths = crate::clipboard::clipboard_files();
            if !paths.is_empty() {
                bytes_out.push(paths.join(" ").into_bytes());
            }
        }

        // 拖拽文件入终端：egui 把 dropped_files 派发给每个面板，只有鼠标
        // 落在本终端矩形内才收下（多个终端各取各的，不重复）。
        let dropped = ui.input(|i| i.raw.dropped_files.clone());
        if !dropped.is_empty() {
            let pointer = ui.input(|i| i.pointer.latest_pos());
            if pointer.is_some_and(|p| rect.contains(p)) {
                let paths: Vec<String> = dropped
                    .iter()
                    .filter_map(|f| f.path().to_str().map(|s| s.to_string()))
                    .collect();
                if !paths.is_empty() {
                    bytes_out.push(paths.join(" ").into_bytes());
                }
            }
        }

        let events = ui.input(|i| i.events.clone());
        let alt_down = ui.input(|i| i.modifiers.alt);
        for ev in &events {
            match ev {
                egui::Event::Text(text) => {
                    if alt_down || text.is_empty() {
                        continue;
                    }
                    bytes_out.push(text.as_bytes().to_vec());
                }
                egui::Event::Ime(ime) if owns_ime => match ime {
                    egui::ImeEvent::Commit(text) => {
                        if !alt_down && !text.is_empty() {
                            bytes_out.push(text.as_bytes().to_vec());
                        }
                    }
                    egui::ImeEvent::Preedit { text, .. } => {
                        preedit.clone_from(text);
                    }
                    _ => {}
                },
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let ctrl = modifiers.ctrl || modifiers.command;
                    let alt = modifiers.alt;
                    let shift = modifiers.shift;

                    // Esc 清除文本选择（按键仍转发给终端，兼容 vim 等应用）。
                    if *key == egui::Key::Escape {
                        if let Ok(mut t) = sess.term.lock() {
                            t.selection = None;
                        }
                    }

                    if let Some(bytes) = encode_key(*key, ctrl, alt, shift, false) {
                        bytes_out.push(bytes);
                    }
                }
                egui::Event::Copy => {
                    // 有选区时 Ctrl+C 复制选区；无选区才发 SIGINT（0x03）。
                    let mut copied = false;
                    if let Ok(mut t) = sess.term.lock() {
                        copied = copy_selection(&t, ui.ctx(), status);
                        if copied {
                            t.selection = None;
                        }
                    }
                    if !copied {
                        bytes_out.push(vec![0x03]);
                    }
                }
                egui::Event::Cut => {
                    bytes_out.push(vec![0x18]); // Ctrl+X
                }
                egui::Event::Paste(text) => {
                    if !text.is_empty() {
                        // 多行粘贴：支持括号粘贴的应用（nvim 等）按字面插入，
                        // 否则把换行转成 \r，让 shell 逐行执行。
                        let bracketed = sess
                            .term
                            .lock()
                            .map(|t| t.mode().contains(TermMode::BRACKETED_PASTE))
                            .unwrap_or(false);
                        if bracketed {
                            let mut v = b"\x1b[200~".to_vec();
                            v.extend_from_slice(text.as_bytes());
                            v.extend_from_slice(b"\x1b[201~");
                            bytes_out.push(v);
                        } else {
                            bytes_out.push(text.replace('\n', "\r").into_bytes());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // 投��用户输入到后台写入线程（非��塞）。
    // 有输入时先回到实时视图底部：正在回看历史时输入，应回到最新内容。
    if !bytes_out.is_empty() {
        if let Ok(mut t) = sess.term.lock() {
            if t.grid().display_offset() != 0 {
                t.scroll_display(Scroll::Bottom);
            }
        }
        let mut all = Vec::new();
        for b in &bytes_out {
            all.extend_from_slice(b);
        }
        let _ = sess.writer.try_send(all);
    }

    // ---- 渲染网格 ----
    let term = match sess.term.lock() {
        Ok(t) => t,
        Err(_) => return,
    };
    let content = term.renderable_content();
    let offset = content.display_offset;
    let colors = &content.colors;
    let cursor = content.cursor;
    // 当前选区范围（供高亮与复制共用；alacritty 核心随内容滚动自动维护）。
    let sel_range = term.selection.as_ref().and_then(|s| s.to_range(&term));
    let canvas_bg = color_for(dark, TERM_BG_DARK, TERM_BG_LIGHT);

    // 逐格渲染：每格钉在 col*cell_w 的精确位置，宽字符画满 2 格。
    // 不能再用整行 LayoutJob 排版：CJK fallback 字体（msyh）的字形宽度实测
    // 14pt，不等于等宽字体 M 的 2 倍（约 16.86pt），整行排版时每个宽字都会
    // 让后面所有格子向左漂移约 2.9pt，光标/选区位置全部错位。
    for indexed in content.display_iter {
        let point = indexed.point;
        // display_iter 返回绝对行号（历史区为负行），视口顶对应 line=-offset，
        // 所以屏幕行号是 line + offset（写成减法会把历史行全部过滤成空白）。
        let vline = point.line.0 + offset as i32;
        if vline < 0 || vline >= rows as i32 {
            continue;
        }
        let cell = indexed.cell;
        let col = point.column.0 as usize;
        let x = rect.left() + col as f32 * cell_w;
        let y = rect.top() + vline as f32 * cell_h;
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        // 视觉槽：宽字符前导格占 2 格；其随空格（SPACER）连回前导格占满 2 格；
        // 窄格 1 格。背景/选区/光标下划线统一按槽绘制——选区边界落在宽字符的
        // 任意一列（含随空格）时整个汉字同色，不会再出现左半正常色、右半被高亮
        // 盖住的“半字”效果。字形仍左对齐画在前导格起点（spacer 无字形）。
        let (slot_col, slot_cells) =
            cjk_slot(col, wide, cell.flags.contains(Flags::WIDE_CHAR_SPACER));
        let x_slot = rect.left() + slot_col as f32 * cell_w;
        let slot_w = slot_cells as f32 * cell_w;

        let (mut fg, mut bg) = (
            resolve_color(cell.fg, colors, true, dark),
            resolve_color(cell.bg, colors, false, dark),
        );
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        // 浅色主题：把暗色主题 TUI 的深背景/浅前景适配成浅色界面可读组合。
        let (fg, bg) = if dark { (fg, bg) } else { adapt_to_light(fg, bg) };
        // 选中格用白字深蓝灰底高亮（宽字符的占位格也按同一规则涂底）。
        // 底不用饱和蓝 (0,110,210)：14px 汉字笔划细，白字与蓝底的抗锯齿中间色
        // 是浅蓝（如 #2080D8），与底色同色系 → 汉字看着“没变白、看不清”；
        // 深蓝灰 (48,58,80) 下中间色变成深灰，字形轮廓清晰。
        let (fg, bg) = if sel_range
            .as_ref()
            .is_some_and(|r| cell_selected(r, indexed.point, cell))
        {
            (Color32::WHITE, Color32::from_rgb(48, 58, 80))
        } else {
            (fg, bg)
        };

        // 背景与画布底色不同（选中/反色/自定义底色）时整格涂背景；宽字槽宽
        // 占满 2 格（含随空格连回前导格），只靠字形的背景（仅 14pt 宽）会露出
        // 右半格，选区边界落在随空格时也能盖满整个汉字。
        if bg != canvas_bg {
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(x_slot, y), Vec2::new(slot_w, cell_h)),
                0.0,
                bg,
            );
        }
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let ch = if cell.c == '\0' { ' ' } else { cell.c };

        let mut format = egui::TextFormat {
            font_id: font_id.clone(),
            color: fg,
            underline: Stroke::NONE,
            // 强制统一行高：CJK fallback 字体（msyh 等）行高与默认等宽字体不同，
            // 不指定会让含中文的行变高，导致整块内容逐行漂移、与光标/选区错位。
            line_height: Some(cell_h),
            ..Default::default()
        };
        if cell.flags.contains(Flags::UNDERLINE) && !wide {
            format.underline = Stroke::new(1.0, fg);
        }
        let text = ch.to_string();
        if wide {
            // 宽字左对齐画在 2 格位起点（与终端惯例一致：字身贴槽左沿，
            // 槽宽仍按 2 格，选区/光标块/下划线盖满整槽；若居中则每个汉字
            // 左右各内缩 1.43px，字与相邻 ASCII、行首字都显出偏移）。
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = slot_w;
            job.append(&text, 0.0, format);
            painter.galley(Pos2::new(x, y), painter.layout_job(job), Color32::WHITE);
            if cell.flags.contains(Flags::UNDERLINE) {
                painter.hline(
                    x..=(x + slot_w),
                    y + cell_h - 1.0,
                    Stroke::new(1.0, fg),
                );
            }
        } else {
            let mut job = egui::text::LayoutJob::default();
            job.append(&text, 0.0, format);
            painter.galley(Pos2::new(x, y), painter.layout_job(job), Color32::WHITE);
        }
    }

    // 光标：支持方块/下划线/竖线三种形状，带描边；失焦时画空心边框。
    // 定位策略分两种：
    // 1) SHOW_CURSOR 开启的常规应用（cmd/nvim 等）：按 grid 光标位置精确绘制，
    //    方向键移动光标时 grid 光标跟着走，位置永远正确。
    // 2) pi 等 TUI 常驻 DECSET 25 隐藏光标（shape=Hidden、show_cursor=false），
    //    并把终端光标停靠在输入框末尾的固定列——直接照画会停在错误位置，且不随
    //    方向键移动（实测 pi 停靠 (11,79)，输入在 (11,5)）。这类 TUI 会用
    //    “空白格 + 非默认前后景色”的单元格自绘真实输入光标（实测随方向键移动），
    //    所以先在该行找这种自绘光标格、画在那里；找不到才退回停靠位置。
    let mut cursor_rect: Option<Rect> = None;
    {
        // 光标位置：
        // - SHOW_CURSOR 开启（cmd/nvim 等）：按 grid 光标位置，精确跟随方向键。
        // - pi 等 TUI 常驻 DECSET 25 隐藏光标，并把终端光标停靠在最后写入行的
        //   末尾（实测列固定不动，不随方向键走），改用自绘光标：pi 用
        //   (Black,White) 反色格画输入光标，格内是字符或空格，随方向键移动。
        //   自底向上整屏找反色格：输入行在 UI 最下方，必然优先命中；停靠行
        //   是灰色状态行时（启动瞬间）也不会误判。找不到才退回停靠位置。
        let show = term.mode().contains(TermMode::SHOW_CURSOR);
        let mut cpoint = cursor.point;
        if !show {
            let is_caret = |cell: &Cell| {
                matches!(
                    (cell.fg, cell.bg),
                    (Color::Named(NamedColor::Black), Color::Named(NamedColor::White))
                        | (Color::Named(NamedColor::White), Color::Named(NamedColor::Black))
                )
            };
            'outer: for r in (0..rows).rev() {
                let line = Line(r as i32 - offset as i32);
                for col in 0..cols {
                    let cell = &term.grid()[Point::new(line, Column(col))];
                    if is_caret(cell) {
                        cpoint = Point::new(line, Column(col));
                        break 'outer;
                    }
                }
            }
        }
        let p = cpoint;
        let vline = p.line.0 + offset as i32;
        if vline >= 0 && vline < rows as i32 {
            let col = p.column.0 as usize;
            if col < cols {
                let cursor_cell = &term.grid()[cpoint];
                // 宽字符光标：方块/下划线/空心块都按 2 格宽画，避免只盖住半个汉字。
                // 光标本身停在宽字符的随空格（后一半）上也一样：若只按 1 格宽画，
                // 白色方块会正好盖住汉字右半，看起来就是“只显示一半汉字”。
                let on_spacer = cursor_cell.flags.contains(Flags::WIDE_CHAR_SPACER);
                let cursor_wide =
                    cursor_cell.flags.contains(Flags::WIDE_CHAR) || on_spacer;
                let col0 = if on_spacer { col.saturating_sub(1) } else { col };
                let x = rect.left() + col0 as f32 * cell_w;
                let y = rect.top() + vline as f32 * cell_h;
                let w = cell_w * if cursor_wide { 2.0 } else { 1.0 };
                let cursor_cell_rect =
                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, cell_h));

                // 光标填充色与画布底色强对比：深色画布→白色光标、浅色画布→
                // 黑色光标，深浅主题下都清晰可见（不能沿用单元格前景色：浅色
                // 主题下暗色配色 TUI 的前景色接近白，白块落在白底上就是光标消失）。
                let (fill, ink) = if dark {
                    (Color32::WHITE, Color32::BLACK)
                } else {
                    (Color32::BLACK, Color32::WHITE)
                };
                let border = fill;
                // Hidden 形状兜底成 Block：隐藏 = 不画，那就强制画个方块。
                let shape = if cursor.shape == CursorShape::Hidden {
                    CursorShape::Block
                } else {
                    cursor.shape
                };
                if *term_focused {
                        match shape {
                            CursorShape::Block => {
                                painter.rect_filled(cursor_cell_rect, 0.0, fill);
                                painter.rect_stroke(
                                    cursor_cell_rect,
                                    0.0,
                                    Stroke::new(1.0, fill),
                                    egui::StrokeKind::Inside,
                                );
                                // 反色重绘格内字符（宽字符居中画在整块内），保证光标内内容可读。
                                // 光标停在随空格时自身无字形，取前导格的宽字符重绘。
                                let ch = if on_spacer {
                                    term.grid()[Point::new(
                                        cpoint.line,
                                        Column(cpoint.column.0.saturating_sub(1)),
                                    )]
                                    .c
                                } else {
                                    cursor_cell.c
                                };
                                if ch != '\0'
                                {
                                    // 字身左对齐，与格内字形一致；宽字符光标块仍盖满 2 格槽。
                                    painter.text(
                                        Pos2::new(x, y + cell_h / 2.0),
                                        egui::Align2::LEFT_CENTER,
                                        ch.to_string(),
                                        font_id.clone(),
                                        ink,
                                    );
                                }
                            }
                            CursorShape::HollowBlock => {
                                painter.rect_stroke(
                                    cursor_cell_rect,
                                    0.0,
                                    Stroke::new(2.0, border),
                                    egui::StrokeKind::Inside,
                                );
                            }
                            CursorShape::Underline => {
                                let h = (cell_h * 0.25).max(3.0);
                                // 用 w（按 cursor_wide 已算 2 格）：下划线光标停在
                                // 宽字符上时盖满 2 格槽，只画 1 格会只盖住半个汉字。
                                painter.rect_filled(
                                    Rect::from_min_size(
                                        Pos2::new(x, y + cell_h - h),
                                        Vec2::new(w, h),
                                    ),
                                    0.0,
                                    fill,
                                );
                            }
                            CursorShape::Beam => {
                                let w = (cell_w * 0.2).max(2.5);
                                painter.rect_filled(
                                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, cell_h)),
                                    0.0,
                                    fill,
                                );
                            }
                            CursorShape::Hidden => {}
                        }
    } else {
        // 未聚焦：空心边框指示光标位置，不遮挡文本。
        painter.rect_stroke(
            cursor_cell_rect,
            0.0,
            Stroke::new(1.5, border),
            egui::StrokeKind::Inside,
        );
    }
    cursor_rect = Some(cursor_cell_rect);
            }
        }
    }

    // 启用系统输入法：egui 只有在有控件写入 PlatformOutput::ime 时才会调用
    // winit 的 set_ime_allowed（目前只有 TextEdit 走这条路径），否则 Windows
    // 上根本不会创建 IME 上下文，输入法事件（含中文提交）永远到不了终端。
    if owns_ime {
        let cursor_rect = cursor_rect.unwrap_or(Rect::from_min_size(
            rect.left_top(),
            Vec2::new(cell_w, cell_h),
        ));
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Terminal,
                rect,
                cursor_rect,
                should_interrupt_composition: false,
            });
        });

        // 把输入法组合（拼音预编辑）画在光标处，带下划线，便于确认候选内容。
        if !preedit.is_empty() {
            let preedit_color = color_for(dark, Color32::WHITE, Color32::BLACK);
            let preedit_bg = color_for(dark, Color32::from_gray(64), Color32::from_gray(225));
            let fmt = egui::TextFormat {
                font_id: font_id.clone(),
                color: preedit_color,
                background: preedit_bg,
                underline: Stroke::new(1.0, preedit_color),
                ..Default::default()
            };
            let mut preedit_job = egui::text::LayoutJob::default();
            preedit_job.append(&preedit, 0.0, fmt);
            let preedit_galley = painter.layout_job(preedit_job);
            let row_rect = Rect::from_min_max(
                rect.left_top(),
                Pos2::new(rect.right(), cursor_rect.min.y + cell_h),
            );
            painter
                .with_clip_rect(row_rect)
                .galley(cursor_rect.min, preedit_galley, Color32::WHITE);
        }
    }

    // 会话活跃时由 SessionListener 的后台解析线程通过 redraw 信号触发重绘；
    // 这里不需要无条件 request_repaint，避免空闲时满帧空转。
}

/// 浅色主题颜色映射：亮黄等亮饱和色白底上必须被压暗到可读、色相保留；
/// 中性亮色仍打黑；中低亮度颜色（本就够对比）不得被改动。
#[cfg(test)]
#[test]
fn light_adapt_keeps_bright_colors_readable() {
    let yellow = Color32::from_rgb(255, 255, 0);
    let (f, _) = adapt_to_light(yellow, Color32::from_rgb(30, 30, 30));
    let bright = (f.r() as u16) + (f.g() as u16) + (f.b() as u16);
    assert!(
        bright < 510,
        "亮黄应从 #FFFF00 压暗（当前 {f:?}），白底上才看得见"
    );
    assert!(
        f.r() == f.g(),
        "压暗应保留黄色色相（红绿等高、蓝低），当前 {f:?}"
    );
    assert!(f.b() < f.r().min(f.g()), "黄色不应混入蓝色通道，当前 {f:?}");

    // 白前景（中性亮色）按原规则直接打黑。
    let (f2, _) = adapt_to_light(Color32::WHITE, Color32::from_rgb(30, 30, 30));
    assert_eq!(f2, Color32::BLACK);

    // 中亮度绿（如 0,128,0）白底对比已够，不得被改动。
    let green = Color32::from_rgb(0, 128, 0);
    let (f3, _) = adapt_to_light(green, Color32::from_rgb(30, 30, 30));
    assert_eq!(f3, green);

    // 深背景仍映射为白。
    let (_, b) = adapt_to_light(yellow, Color32::BLACK);
    assert_eq!(b, TERM_BG_LIGHT);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_bytes(k: egui::Key, ctrl: bool, alt: bool, shift: bool) -> Option<Vec<u8>> {
        encode_key(k, ctrl, alt, shift, false)
    }

    /// 几何不变量：任一台机器加载的 CJK fallback 字体下，汉字字形必须占宽槽
    /// （>1 格）且能并入 2 格槽（≤2 格）。只要成立，逐格渲染 + 左对齐就不会错位。
    #[test]
    fn cjk_glyph_fits_wide_slot() {
        let ctx = egui::Context::default();
        // 与 crate::app::setup_fonts 相同的候选字体加载逻辑。
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
                    vec![egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Monospace,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    }],
                ));
                break;
            }
        }
        let font_id = FontId::monospace(super::TERM_FONT_SIZE);
        ctx.begin_pass(egui::RawInput::default());
        ctx.fonts_mut(|f| {
            let cell_w = f.glyph_width(&font_id, 'M');
            let cjk = f.glyph_width(&font_id, '你');
            assert!(
                cjk > cell_w,
                "汉字字形 {cjk:.3}px 必须宽于单格 {cell_w:.3}px，否则与 ASCII 同宽会错位"
            );
            assert!(
                cjk <= 2.0 * cell_w + 0.01,
                "汉字字形 {cjk:.3}px 必须能并入 2 格槽 {:.3}px",
                2.0 * cell_w
            );
            // 整行 LayoutJob 排版按字形累加宽度：若汉字≠2 格宽，每个汉字都会让
            // 后续内容向左漂移 (2*cell_w - cjk)，逐字累加——这就是逐格渲染的原因。
            let drift = 2.0 * cell_w - cjk;
            println!("cell_w={cell_w:.3} cjk={cjk:.3} drift/汉字={drift:.3}px");
        });
        let mut out = ctx.end_pass();
        out.textures_delta.clear(); // 不把纹理增量交给渲染器，避免 Drop 断言
    }

    #[test]
    fn cjk_slot_geometry() {
        // 窄格：1 格，起点自身。
        assert_eq!(cjk_slot(3, false, false), (3, 1));
        // 宽字符前导格：2 格，起点自身。
        assert_eq!(cjk_slot(5, true, false), (5, 2));
        // 随空格：连回前导格仍 2 格——选区/背景不会截半字。
        assert_eq!(cjk_slot(6, false, true), (5, 2));
        // 随空格排到第 0 列（理论上不会发生）也不越界 panic。
        assert_eq!(cjk_slot(0, false, true), (0, 2));
    }

    #[test]
    fn selection_highlight_predicate() {
        let cell = Cell::default();
        let range = SelectionRange::new(
            Point::new(Line(-2), Column(1)),
            Point::new(Line(-2), Column(3)),
            false,
        );
        assert!(cell_selected(&range, Point::new(Line(-2), Column(2)), &cell));
        assert!(!cell_selected(&range, Point::new(Line(-1), Column(2)), &cell));
        assert!(!cell_selected(&range, Point::new(Line(-2), Column(4)), &cell));

        // 宽字符前导格：选区右边界落在其空格（column+1）上时仍高亮。
        let mut wide = Cell::default();
        wide.flags.insert(Flags::WIDE_CHAR);
        let edge = SelectionRange::new(
            Point::new(Line(0), Column(0)),
            Point::new(Line(0), Column(5)),
            false,
        );
        assert!(cell_selected(&edge, Point::new(Line(0), Column(4)), &wide));
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
