use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection as TermSelection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, Rgb};
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
/// 深背景 → 白，亮灰/白前景 → 黑（仅近似无色相的亮色，避免误杀黄色等亮色语法高亮）。
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
    }
    (f, b)
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
            if let Ok(mut t) = sess.term.lock() {
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
        if resp.secondary_clicked() {
            // 有选区 → 右键复制；无选区 → 右键粘贴（下一帧 egui 投递 Event::Paste）。
            let copied = copy_selection(&t, ui.ctx(), status);
            if !copied {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                // 右键粘贴也算与终端交互：聚焦终端，下一帧的粘贴事件才能写入。
                *term_focused = true;
            }
            t.selection = None;
        }
    }

    let mut bytes_out: Vec<Vec<u8>> = Vec::new();
    let mut preedit = String::new();

    if *term_focused {
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

    let mut job = egui::text::LayoutJob::default();
    job.break_on_newline = true;
    // max_width 留 0.5px 余量：行宽恰好等于 cols*cell_w 时避免 egui 因舍入多换一行。
    job.wrap.max_width = cols as f32 * cell_w + 0.5;
    job.wrap.max_rows = rows;

    let mut last_vline: Option<i32> = None;
    for indexed in content.display_iter {
        let point = indexed.point;
        // display_iter 返回绝对行号（历史区为负行），视口顶对应 line=-offset，
        // 所以屏幕行号是 line + offset（写成减法会把历史行全部过滤成空白）。
        let vline = point.line.0 + offset as i32;
        if vline < 0 || vline >= rows as i32 {
            continue;
        }
        if let Some(prev) = last_vline {
            if prev != vline {
                job.append(
                    "\n",
                    0.0,
                    egui::TextFormat {
                        line_height: Some(cell_h),
                        ..Default::default()
                    },
                );
            }
        }
        last_vline = Some(vline);

        let cell = indexed.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let ch = if cell.c == '\0' { ' ' } else { cell.c };

        let fg = resolve_color(cell.fg, colors, true, dark);
        let bg = resolve_color(cell.bg, colors, false, dark);
        let (fg, bg) = if cell.flags.contains(Flags::INVERSE) {
            (bg, fg)
        } else {
            (fg, bg)
        };
        // 浅色主题：把暗色主题 TUI 的深背景/浅前景适配成浅色界面可读组合。
        let (fg, bg) = if dark { (fg, bg) } else { adapt_to_light(fg, bg) };
        // 选中格用蓝底白字高亮。
        let (fg, bg) = if sel_range
            .as_ref()
            .is_some_and(|r| cell_selected(r, indexed.point, cell))
        {
            (Color32::WHITE, Color32::from_rgb(0, 110, 210))
        } else {
            (fg, bg)
        };

        let mut format = egui::TextFormat {
            font_id: font_id.clone(),
            color: fg,
            background: bg,
            underline: Stroke::NONE,
            // 强制统一行高：CJK fallback 字体（msyh 等）行高与默认等宽字体不同，
            // 不指定会让含中文的行变高，导致整块内容逐行漂移、与光标/选区错位。
            line_height: Some(cell_h),
            ..Default::default()
        };
        if cell.flags.contains(Flags::UNDERLINE) {
            format.underline = Stroke::new(1.0, fg);
        }
        job.append(&ch.to_string(), 0.0, format);
    }

    let galley = painter.layout_job(job);
    painter.galley(rect.left_top(), galley, Color32::WHITE);

    // 光标：支持方块/下划线/竖线三种形状，带描边；失焦时画空心边框；
    // 闪烁光标（DECSET 12）按约 1.1s 周期半显半隐。
    let mut cursor_rect: Option<Rect> = None;
    if term.mode().contains(TermMode::SHOW_CURSOR) && cursor.shape != CursorShape::Hidden {
        let p = cursor.point;
        let vline = p.line.0 + offset as i32;
        if vline >= 0 && vline < rows as i32 {
            let col = p.column.0 as usize;
            if col < cols {
                let cursor_cell = &term.grid()[cursor.point];
                let fg = resolve_color(cursor_cell.fg, colors, true, dark);
                let x = rect.left() + col as f32 * cell_w;
                let y = rect.top() + vline as f32 * cell_h;
                let cursor_cell_rect =
                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h));

                let blinking = term.cursor_style().blinking;
                let phase_on = (ui.input(|t| t.time).rem_euclid(1.1)) < 0.55;
                if !blinking || phase_on {
                    let border = color_for(dark, Color32::from_gray(200), Color32::from_gray(70));
                    if *term_focused {
                        // 光标一律用高对比绘制，避免暗色字符时看不见。
                        match cursor.shape {
                            CursorShape::Block => {
                                painter.rect_filled(cursor_cell_rect, 0.0, fg);
                                painter.rect_stroke(
                                    cursor_cell_rect,
                                    0.0,
                                    Stroke::new(1.0, border),
                                    egui::StrokeKind::Inside,
                                );
                                // 块内反色重绘格内字符（宽字符按整字宽画出），保证光标内内容可见。
                                let ch = cursor_cell.c;
                                if ch != '\0'
                                    && !cursor_cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                                {
                                    let cell_bg =
                                        resolve_color(cursor_cell.bg, colors, false, dark);
                                    painter.text(
                                        Pos2::new(x, y + cell_h / 2.0),
                                        egui::Align2::LEFT_CENTER,
                                        ch.to_string(),
                                        font_id.clone(),
                                        cell_bg,
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
                                painter.rect_filled(
                                    Rect::from_min_size(
                                        Pos2::new(x, y + cell_h - h),
                                        Vec2::new(cell_w, h),
                                    ),
                                    0.0,
                                    color_for(dark, Color32::WHITE, Color32::BLACK),
                                );
                            }
                            CursorShape::Beam => {
                                let w = (cell_w * 0.2).max(2.5);
                                painter.rect_filled(
                                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, cell_h)),
                                    0.0,
                                    color_for(dark, Color32::WHITE, Color32::BLACK),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key_bytes(k: egui::Key, ctrl: bool, alt: bool, shift: bool) -> Option<Vec<u8>> {
        encode_key(k, ctrl, alt, shift, false)
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
