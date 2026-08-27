use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection as TermSelection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, Stroke, Vec2};
use portable_pty::PtySize;

use crate::session::{Session, SessionListener};
use crate::term_gl::{hash_mix, CellQuad, GlyphAtlas, TermGpu};
// 读 Windows 剪贴板 CF_HDROP（资源管理器复制/剪切的文件列表）。
use clipboard_win::{formats::FileList, get_clipboard, raw as clip_raw};

/// 终端内嵌页面使用的等宽字号。
pub const TERM_FONT_SIZE: f32 = 14.0;

/// Color32 压成 u64 供哈希搅拌（sRGB 四字节）。
#[inline]
fn color_key(c: Color32) -> u64 {
    ((c.r() as u64) << 24) | ((c.g() as u64) << 16) | ((c.b() as u64) << 8) | c.a() as u64
}

/// 渲染持锁段耗时统计（TUIPM_LATENCY_DEBUG=1 启用）。
static SNAP_ACC_US: AtomicU64 = AtomicU64::new(0);
static SNAP_MAX_US: AtomicU32 = AtomicU32::new(0);
static SNAP_FRAMES: AtomicU32 = AtomicU32::new(0);

fn latency_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("TUIPM_LATENCY_DEBUG").is_ok())
}

/// 深浅主题下的终端画布底色。
const TERM_BG_DARK: Color32 = Color32::from_rgb(22, 22, 26);
const TERM_BG_LIGHT: Color32 = Color32::WHITE;

fn color_for(dark: bool, dark_c: Color32, light_c: Color32) -> Color32 {
    if dark { dark_c } else { light_c }
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

/// 深色主题下提亮过暗的颜色（色相不变），保证与深画布底色有足够对比。
/// 过亮的颜色在深底上一般对比足够，不额外压暗；浅色主题压暗见 `adapt_to_light`。
fn adapt_to_dark(fg: Color32, bg: Color32) -> (Color32, Color32) {
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
    // 背景过浅将沉暗，维持深色主题纯度。
    if lum(b) > 0.75 {
        b = TERM_BG_DARK;
    }
    // 前景过暗（ANSI 黑/深灰/深蓝…）在深底几近不可见：混合向白提亮到目标
    // 亮度 0.4（色相比不变：灰保持灰、蓝变浅蓝，而非通道放大把纯蓝吹成 255
    // 却仍只有 0.07 亮度）。过亮中性前景归一为白。
    let fl = lum(f);
    if fl > 0.72 && neutral(f) {
        f = Color32::WHITE;
    } else if fl < 0.22 {
        let t = ((0.4 - fl) / (1.0 - fl)).clamp(0.0, 0.6);
        let mix = |v: u8| (v as f32 + (255.0 - v as f32) * t).round() as u8;
        f = Color32::from_rgb(mix(f.r()), mix(f.g()), mix(f.b()));
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

/// 从剪贴板读文件绝对路径列表（CF_HDROP，资源管理器 Ctrl+C 复制/剪切的文件）。
/// 剪贴板被占用或无文件时返回空。
fn clipboard_files() -> Vec<String> {
    match get_clipboard(FileList) {
        Ok(files) => files,
        Err(_) => Vec::new(),
    }
}

/// 把绝对路径转成相对会话启动目录的路径（Windows 路径不区分大小写）：
/// 在目录内 → 相对路径；目录外 → 保留绝对路径。前缀必须落在分隔符上，
/// 避免 `D:\code` 误把 `D:\code2\a.txt` 裁成 `2\a.txt`。
fn rel_to_cwd(abs: &str, cwd: &str) -> String {
    let abs = abs.replace('/', "\\");
    let cwd_owned = cwd.replace('/', "\\");
    let cwd = cwd_owned.trim_end_matches('\\');
    let a = abs.to_lowercase();
    let c = cwd.to_lowercase();
    if !c.is_empty()
        && a.len() > c.len()
        && a.starts_with(&c)
        && abs.get(c.len()..).is_some_and(|rest| rest.starts_with('\\'))
    {
        abs[c.len()..].trim_start_matches('\\').to_string()
    } else {
        abs
    }
}

/// 粘到终端的单条路径：含空格/引号时加双引号，避免 shell 把路径拆词。
/// ponytail: cmd 里引号内无法转义引号，所以含引号直接删掉（极端少见，够用为止）。
fn path_for_input(p: &str) -> String {
    if p.chars().any(|c| c == ' ' || c == '"') {
        format!("\"{}\"", p.replace('"', ""))
    } else {
        p.to_string()
    }
}

/// 把剪贴板/拖放的文件列表转成相对路径并写入终端输入行。
/// 返回是否写入了路径（入参为空时返回 false，调用方回退到文本粘贴）。
fn paste_file_paths(sess: &Session, files: &[String], status: &mut Option<String>) -> bool {
    if files.is_empty() {
        return false;
    }
    let mapped: Vec<String> = files
        .iter()
        .map(|p| path_for_input(&rel_to_cwd(p, &sess.dir)))
        .collect();
    let _ = sess.writer.try_send(mapped.join(" ").into_bytes());
    *status = Some(if mapped.len() == 1 {
        format!("已粘贴文件相对路径：{}", mapped[0])
    } else {
        format!("已粘贴 {} 个文件路径", mapped.len())
    });
    true
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

/// 组合回车（Shift/Alt/Ctrl+Enter）编码。full=true 时按 CSI-u 发送
/// （kitty 键盘协议 / modifyOtherKeys 同款格式），opencode 等全屏 TUI 靠它区分
/// 「换行」与「提交」；full=false 回退普通 \r（普通 shell 收到转义串会当文本回显）。
fn encode_modified_enter(shift: bool, alt: bool, ctrl: bool, full: bool) -> Vec<u8> {
    if !full {
        return vec![b'\r'];
    }
    // kitty 修饰符编码：1 + shift(1) + alt(2) + ctrl(4)；13 = Enter 的键值。
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    format!("\x1b[13;{m}u").into_bytes()
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

/// 按当前可用面积与等宽字体计算终端网格行列数。
/// 会话页签里终端占满整个中央面板（无左侧项目栏）；启动会话时用它取精确尺寸，
/// 避免用窗口减固定余量的估算值 spawn（那会让 TUI 启动时按错误尺寸画页面）。
pub fn term_grid_size(ui: &egui::Ui) -> Option<(usize, usize)> {
    let font_id = FontId::monospace(TERM_FONT_SIZE);
    let cell_w = ui.ctx().fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let cell_h = ui.ctx().fonts_mut(|f| f.row_height(&font_id));
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }
    let avail = ui.available_size();
    Some((
        (avail.x / cell_w).floor().max(1.0) as usize,
        (avail.y / cell_h).floor().max(1.0) as usize,
    ))
}

/// 按子进程启用的鼠标编码（SGR ?1006 或默认 X10）把一次鼠标事件写入 PTY。
/// 新版 ConPTY 对宿主写入的鼠标序列只透传、不重编码，所以编码必须与子进程
/// 声明的一致；code 为 xterm 事件码：按键 0/1/2，移动 +32，滚轮上 64 下 65，
/// 释放固定 0（X10 载荷里自动变 3）。col/row 已是 1-based。
fn send_mouse_event(
    writer: &std::sync::mpsc::SyncSender<Vec<u8>>,
    sgr: bool,
    code: u16,
    col: usize,
    row: usize,
    release: bool,
) {
    let bytes = if sgr {
        format!("\x1b[<{code};{col};{row}{}", if release { 'm' } else { 'M' }).into_bytes()
    } else {
        // X10 三字节载荷：事件码+32、列+32、行+32；列/行上限 223 防 u8 回绕。
        vec![
            0x1b,
            b'[',
            b'M',
            (code.min(191) as u8).wrapping_add(32),
            (col.clamp(1, 223) as u8).wrapping_add(32),
            (row.clamp(1, 223) as u8).wrapping_add(32),
        ]
    };
    let _ = writer.try_send(bytes);
}

/// 渲染一个终端会话（网格 + 光标），并把终端获得焦点时的键盘输入写回 PTY。
pub fn show_terminal(
    ui: &mut egui::Ui,
    sess: &mut Session,
    dark: bool,
    status: &mut Option<String>,
    term_focused: &mut bool,
) {
    // 字体度量缓存：字号/DPI 不变时跳过 fonts_mut 锁查询。
    let font_id = FontId::monospace(TERM_FONT_SIZE);
    let ppp = ui.ctx().pixels_per_point();
    let (cell_w, cell_h) = match sess.cached_metrics {
        Some((cw, ch, cached_ppp)) if (cached_ppp - ppp).abs() < 0.01 => (cw, ch),
        _ => {
            let m = ui.ctx().fonts_mut(|f| {
                (f.glyph_width(&font_id, 'M'), f.row_height(&font_id))
            });
            sess.cached_metrics = Some((m.0, m.1, ppp));
            m
        }
    };
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }
    // ── GPU 字形批渲染：首次帧初始化；DPI/字号变化时重建图集。失败则整格回落 galley。
    match sess.gpu.as_mut() {
        Some(g) => g.ensure_params(TERM_FONT_SIZE, ppp),
        None => sess.gpu = TermGpu::new(ui.ctx(), TERM_FONT_SIZE, ppp),
    }
    // 首帧懒预热：图集为空时一次性光栅化 ASCII 可打印字符，
    // 避免首帧逐字光栅化的卡顿峰值。只在 map 为空时执行（首次或 DPI 变化后）。
    if let Some(g) = sess.gpu.as_mut() {
        if g.atlas.is_empty() {
            for ch in (32u8..=126).map(|b| b as char) {
                g.atlas.glyph(ch);
            }
        }
    }
    let avail = ui.available_size();
    let cols = ((avail.x / cell_w).floor().max(1.0)) as usize;
    let rows = ((avail.y / cell_h).floor().max(1.0)) as usize;

    // 哈希缓冲对齐可见区尺寸（rows×cols）。
    if let Some(g) = sess.gpu.as_mut() {
        g.begin_frame(rows, cols);
    }

    if sess.grid_size != (cols as u16, rows as u16) {
        if let Ok(mut t) = sess.term.write() {
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

    // ── 读取终端鼠标模式（一次性锁，后续所有分支复用） ──
    let term_mode_snapshot = sess.term.read().map(|t| *t.mode()).ok();
    // 应用要鼠标事件 = 任一上报模式（?1000/?1002/?1003）。注意捆绑的新版
    // conpty.dll 会自行跟踪并吞掉 ?1000h/?1002h，只向仿真器透传编码位 ?1006h，
    // 所以 SGR_MOUSE 单独出现也代表子进程开了鼠标上报（实测 opencode 即此形态）。
    let mouse_reporting = term_mode_snapshot.map_or(false, |m| {
        m.intersects(TermMode::MOUSE_MODE | TermMode::SGR_MOUSE)
    });
    // 转发用的编码开关：新版 ConPTY 对宿主写入的鼠标序列只透传不重编码，
    // 必须与应用声明一致（SGR ?1006 vs 默认 X10）；老版 ConPTY 则把一切鼠标
    // 序列改写成乱码，捆绑 conpty.dll 是全屏 TUI 滚轮可用的前提。
    let sgr_mouse = term_mode_snapshot.map_or(false, |m| m.contains(TermMode::SGR_MOUSE));
    let alt_screen = term_mode_snapshot
        .unwrap_or(TermMode::empty())
        .contains(TermMode::ALT_SCREEN);

    // ── 鼠标滚轮 ──
    // 用 pointer_hover_pos() + latest_pos() 双重检测：
    // hover_pos() 在 click_and_drag 感知下某些帧返回 None；
    // latest_pos() 在纯滚轮操作（无鼠标移动）时也可能返回 None。
    // 合并为单次 input 调用，减少锁获取。
    let (scroll_delta, over_term) = ui.input(|i| {
        let sd = i.smooth_scroll_delta.y;
        let over = sd != 0.0 && (
            i.pointer.hover_pos().map_or(false, |p| rect.contains(p))
                || i.pointer.latest_pos().map_or(false, |p| rect.contains(p))
        );
        (sd, over)
    });
    if over_term {
        let delta = scroll_delta;
        ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
        let mut lines = (delta / cell_h).round() as i32;
        if lines == 0 {
            lines = if delta > 0.0 { 1 } else { -1 };
        }
        if mouse_reporting {
            // 子进程开了鼠标上报（opencode/nvim 等）→ 滚轮作为真实滚轮事件转发，
            // 编码与应用声明一致（SGR/X10）。优先于 alt_screen PgUp/PgDn 分支。
            let pos = ui
                .input(|i| i.pointer.latest_pos())
                .unwrap_or(rect.center());
            let col = (((pos.x - rect.left()).max(0.0) / cell_w) as usize + 1)
                .clamp(1, cols);
            let row = (((pos.y - rect.top()).max(0.0) / cell_h) as usize + 1)
                .clamp(1, rows);
            let b = if lines > 0 { 64u16 } else { 65 }; // xterm 滚轮上/下事件码
            for _ in 0..lines.abs().clamp(1, 32) {
                send_mouse_event(&sess.writer, sgr_mouse, b, col, row, false);
            }
        } else if alt_screen {
            // ALT_SCREEN 无鼠标上报 → PgUp/PgDn 翻页。
            let count = (lines.abs() as usize).div_ceil(3).clamp(1, 8);
            let key: &[u8] = if lines > 0 { b"\x1b[5~" } else { b"\x1b[6~" };
            for _ in 0..count {
                let _ = sess.writer.try_send(key.to_vec());
            }
        } else {
            // 普通 shell / 主屏 TUI（pi 等）→ 滚仿真器缓冲。
            if let Ok(mut t) = sess.term.write() {
                t.scroll_display(Scroll::Delta(lines));
            }
        }
        ui.ctx().request_repaint();
    } // end over_term
    // ── 鼠标点击 / 拖拽：按下/释放转发给子进程，拖拽与右键留给本地 ──
    // 通用策略（对任何子进程一致，不区分是否 mouse reporting）：
    // - 左/中键按下、释放照常转发 → TUI 内点击按钮/切换焦点仍可用；
    // - 左键拖拽不转发 → 一律做本地文本选择（选中→复制是终端的通用刚需）；
    // - 右键永不转发 → 固定弹本地菜单（复制/粘贴/清空输入）。
    if mouse_reporting {
        let to_col_row = |pos: Pos2| -> (usize, usize) {
            let col = (((pos.x - rect.left()).max(0.0) / cell_w) as usize + 1)
                .clamp(1, cols);
            let row = (((pos.y - rect.top()).max(0.0) / cell_h) as usize + 1)
                    .clamp(1, rows);
            (col, row)
        };

        // --- 按下/释放：直接从 egui 原始指针状态检测，不依赖 resp.clicked()——
        //    Sense::click_and_drag() 下有时序问题（drag_started 同帧 clicked=false）。
        let ptr_pressed = ui.input(|i| {
            i.pointer.button_pressed(egui::PointerButton::Primary)
                || i.pointer.button_pressed(egui::PointerButton::Middle)
        });
        // 仅过滤主/中键释放：右键释放不应转发给子进程（否则 TUI 收到
        // 意外左键释放事件，回显杂字或清除选区）。
        let ptr_released = ui.input(|i| {
            i.pointer.button_released(egui::PointerButton::Primary)
                || i.pointer.button_released(egui::PointerButton::Middle)
        });

        if resp.hovered() && ptr_pressed {
            // 按下：检测哪个按钮
            if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                let (col, row) = to_col_row(pos);
                let btn = ui.input(|i| {
                    if i.pointer.button_pressed(egui::PointerButton::Primary) {
                        0
                    } else {
                        1
                    }
                });
                send_mouse_event(&sess.writer, sgr_mouse, btn, col, row, false);
                ui.ctx().request_repaint();
            }
        }

        // --- 释放 ---
        if ptr_released {
            if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                let (col, row) = to_col_row(pos);
                send_mouse_event(&sess.writer, sgr_mouse, 0, col, row, true);
                ui.ctx().request_repaint();
            }
        }
    }

    // 本地文本选择 + 右键菜单选区检查合并为一次加锁：
    // 原来选区处理和 has_selection 各自加锁，两段锁之间读者线程可能
    // 处理 PTY 输出清掉选区 → 右键菜单「复制」恒灰。
    let mut has_selection = false;
    {
        let primary_released = ui.input(|i| i.pointer.button_released(egui::PointerButton::Primary));
        let latest_pos = ui.input(|i| i.pointer.latest_pos());
        if let Ok(mut t) = sess.term.write() {
            let disp_off = t.grid().display_offset();
            let point_at = |pos: Pos2| -> Option<Point> {
                let col = ((pos.x - rect.left()) / cell_w).floor() as i64;
                let row = ((pos.y - rect.top()) / cell_h).floor() as i64;
                if row < 0 || col < 0 {
                    return None;
                }
                Some(Point::new(
                    Line(row as i32 - disp_off as i32),
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
            if primary_released && sess.drag_press_pos.is_none() {
                sess.drag_press_pos = ui.input(|i| {
                    i.raw.events.iter().find_map(|e| match e {
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            ..
                        } => Some(*pos),
                        _ => None,
                    })
                });
            }
            if primary_released {
                if let (Some(p0), Some(p1)) = (sess.drag_press_pos.take(), latest_pos) {
                    let moved = p0.distance(p1) > 4.0;
                    if moved && rect.contains(p1) && t.selection.is_none() {
                        let disp_off = t.grid().display_offset();
                        let point_at = |pos: Pos2| -> Option<Point> {
                            let col = ((pos.x - rect.left()) / cell_w).floor() as i64;
                            let row = ((pos.y - rect.top()) / cell_h).floor() as i64;
                            if row < 0 || col < 0 {
                                return None;
                            }
                            Some(Point::new(
                                Line(row as i32 - disp_off as i32),
                                Column((col as usize).min(cols.saturating_sub(1))),
                            ))
                        };
                        if let Some(start) = point_at(p0) {
                            t.selection =
                                Some(TermSelection::new(SelectionType::Simple, start, Side::Left));
                            if let (Some(end), Some(sel)) = (point_at(p1), t.selection.as_mut()) {
                                sel.update(end, Side::Left);
                            }
                            ui.ctx().request_repaint();
                        }
                    }
                }
            }
            // 在同一次加锁内完成选区状态读取，消除与右键菜单之间的竞态窗口。
            has_selection = t.selection.is_some();
        }
    }

    // 右键弹菜单：复制 / 粘贴 / 清空输入（不再右键直接粘贴，避免误触）。
    // 任何模式下都可用——右键不再转发给子进程。
    let mut menu_action: Option<TermAction> = None;
    {
    resp.context_menu(|ui| {
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
                .write()
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
            // 剪贴板里是文件（复制过文件）→ 直接粘贴相对路径；
            // 否则清掉选区触发系统文本粘贴，下一帧 egui 投递 Event::Paste。
            let files = clipboard_files();
            if let Ok(mut t) = sess.term.write() {
                t.selection = None;
            }
            if !paste_file_paths(sess, &files, status) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            }
            *term_focused = true;
        }
        Some(TermAction::ClearInput) => {
            // 清空当前输入行：End（光标到行尾）+ Ctrl+U（清除到行首）。
            // Ctrl+U（0x15）是 bash/zsh/cmd 通用的「清空行」快捷键，
            // 单次发送即可清空整行，无需发送大量退格。
            let _ = sess.writer.try_send(b"\x1b[F\x15".to_vec());
            *status = Some("已清空输入".to_string());
        }
        None => {}
    }
    }

    let mut bytes_out: Vec<Vec<u8>> = Vec::new();
    let mut preedit = String::new();

    // 拖放文件到终端区域：把文件的相对路径粘贴到当前输入行（不要求终端已聚焦）。
    // 合并 dropped_files / hovered_files / latest_pos 为单次 input 调用。
    let (dropped, has_hovered, pointer_pos) = ui.input(|i| {
        let d = i.raw.dropped_files.clone();
        let h = !i.raw.hovered_files.is_empty();
        let p = i.pointer.latest_pos();
        (d, h, p)
    });
    if !dropped.is_empty() {
        let pos = pointer_pos.unwrap_or(rect.center());
        if rect.contains(pos) {
            *term_focused = true;
            resp.request_focus();
            let files: Vec<String> = dropped
                .iter()
                .map(|f| f.path().to_string_lossy().into_owned())
                .collect();
            if paste_file_paths(sess, &files, status) {
                if let Ok(mut t) = sess.term.write() {
                    t.selection = None;
                }
            }
        }
    } else if has_hovered {
        // 拖拽悬停时的提示：还没松手，先告诉用户会粘什么。
        let pos = pointer_pos.unwrap_or(rect.center());
        if rect.contains(pos) {
            *status = Some("释放鼠标以把文件相对路径粘贴到终端".to_string());
        }
    }

    if *term_focused {
        // 剪贴板里只有文件（资源管理器复制）时，egui 的 Ctrl+V 不产生任何事件
        // （它只读文本剪贴板，没文本就直接吞掉按键）。用剪贴板序列号兜底：
        // 序列变化 + Ctrl 按下 + 指针在终端上 + 无文本选区，视为一次“粘贴文件”
        // 手势。数字签变化只认领一次，避免每次按住 Ctrl 都重复注入。
        if ui.input(|i| i.modifiers.ctrl || i.modifiers.command) {
            let seq = clip_raw::seq_num();
            if sess.last_clipboard_seq != seq {
                sess.last_clipboard_seq = seq;
                let has_sel = sess
                    .term
                    .read()
                    .map(|t| t.selection.is_some())
                    .unwrap_or(false);
                let over_term = ui
                    .input(|i| i.pointer.latest_pos().map_or(false, |p| rect.contains(p)));
                if !has_sel && over_term {
                    paste_file_paths(sess, &clipboard_files(), status);
                }
            }
        }

        // 事件在单次 input 闭包内就地处理：原实现先 clone 整个事件列表再遍历，
        // 每帧多一次分配 + 全量拷贝（egui Event 含 String/IME 文本）。
        ui.input(|i| {
            let alt_down = i.modifiers.alt;
            for ev in &i.events {
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
                            if let Ok(mut t) = sess.term.write() {
                                t.selection = None;
                            }
                        }

                        // 组合回车：仅当应用真的推过 kitty 键盘协议（协商成功）
                        // 才发 CSI-u（Shift+Enter = opencode 输入框换行）；否则退回 \r。
                        // 不能拿备用屏当信号——ConPTY 会吞掉协议协商，对端不认识
                        // CSI-u 时会把它当字面文本插进输入框（实测 opencode 出乱码）。
                        if *key == egui::Key::Enter && (shift || alt || ctrl) {
                            let full = term_mode_snapshot.map_or(false, |m| {
                                m.intersects(TermMode::DISAMBIGUATE_ESC_CODES)
                            });
                            bytes_out.push(encode_modified_enter(shift, alt, ctrl, full));
                        } else if let Some(bytes) = encode_key(*key, ctrl, alt, shift, false) {
                            bytes_out.push(bytes);
                        }
                    }
                    egui::Event::Copy => {
                        // 有选区时 Ctrl+C 复制选区；无选区才发 SIGINT（0x03）。
                        // 注意：Ctrl+C 会同时触发 Event::Key(C,ctrl) 和 Event::Copy。
                        // Key 处理器里 Ctrl+字母已经通过 encode_char_key 发了 0x03，
                        // 这里只在「无选区且 Key 处理器未发过」时才补发，避免双发。
                        let key_already_sent = !bytes_out.is_empty()
                            && bytes_out.last().map_or(false, |b| b.as_slice() == [0x03]);
                        let mut copied = false;
                        if let Ok(mut t) = sess.term.write() {
                            copied = copy_selection(&t, ui.ctx(), status);
                            if copied {
                                t.selection = None;
                            }
                        }
                        if !copied && !key_already_sent {
                            bytes_out.push(vec![0x03]);
                        }
                    }
                    egui::Event::Cut => {
                        bytes_out.push(vec![0x18]); // Ctrl+X
                    }
                    egui::Event::Paste(text) => {
                        // 剪贴板中是文件 → 粘贴文件相对路径（优先于文本粘贴）。
                        if paste_file_paths(sess, &clipboard_files(), status) {
                            continue;
                        }
                        if !text.is_empty() {
                            // 多行粘贴：支持括号粘贴的应用（nvim 等）按字面插入，
                            // 否则把换行转成 \r，让 shell 逐行执行。
                            let bracketed = sess
                                .term
                                .read()
                                .map(|t| t.mode().contains(TermMode::BRACKETED_PASTE))
                                .unwrap_or(false);
                            if bracketed {
                                let mut v = b"\x1b[200~".to_vec();
                                v.extend_from_slice(text.as_bytes());
                                v.extend_from_slice(b"\x1b[201~");
                                bytes_out.push(v);
                            } else {
                                // 换行替换为空格：多行粘贴保留词间距，
                                // 不让 shell 把每行当独立命令执行。
                                let cleaned: String = text
                                    .replace("\r\n", " ")
                                    .replace('\r', " ")
                                    .replace('\n', " ");
                                if !cleaned.is_empty() {
                                    bytes_out.push(cleaned.into_bytes());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // 投递用户输入到后台写入线程（非阻塞）。
    if !bytes_out.is_empty() {
        let mut all = Vec::new();
        for b in &bytes_out {
            all.extend_from_slice(b);
        }
        let sent = sess.writer.try_send(all).is_ok();
        // 回显延迟探针：记录输入时间戳（读取线程比对首块回显）。
        if sent {
            sess.last_input_ms.store(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                Ordering::Relaxed,
            );
            // 标记下一帧需要强制刷新快照：PTY 回显会在下一帧的 parse_gen 里体现，
            // 但输入发送和 parse_gen 更新之间有 1 帧窗口期，不标记会导致
            // 回显字符被跳过（快照复用旧数据、parse_gen 还没变）。
            sess.pending_input = true;
        }
    }

    // ---- 渲染网格 ----
    // 快照终端状态后立即释放锁：渲染循环遍历全部格子（rows×cols）开销大，
    // 期间解析线程被阻塞无法喂入新输出 → 打字回显延迟。快照后渲染纯读
    // 快照数据，解析线程可在片间插空更新终端状态。
    // 静止帧优化：parse_gen 未变时跳过 cell clone（最大开销），复用上一帧快照；
    // 光标/选区/颜色等元数据仍每帧读取（它们可能独立变化）。
    // 注意：IME 活跃时不跳过，因为输入法组合/提交会改变终端内容。
    let snap_started = std::time::Instant::now();
    let term_arc = sess.term.clone();
    let cur_gen = sess.parse_gen.load(Ordering::Relaxed);
    let snapshot_changed = cur_gen != sess.snapshot_gen
        || !sess.last_preedit.is_empty()
        || std::mem::replace(&mut sess.pending_input, false);
    let (
        offset,
        colors_vec,
        cursor,
        sel_range,
        show_cursor,
        snapshot_cells,
        cursor_cell_char,
        cursor_cell_flags,
    ) = {
        let term = match term_arc.read() {
            Ok(t) => t,
            Err(_) => return,
        };
        let content = term.renderable_content();
        let offset = content.display_offset;
        let colors_vec = content.colors.clone();
        let cursor = content.cursor;
        let sel_range = term.selection.as_ref().and_then(|s| s.to_range(&term));
        let show_cursor = term.mode().contains(TermMode::SHOW_CURSOR);
        // 光标格快照：Block 光标反色重绘需要字符和标志。
        let cpoint = cursor.point;
        let (cursor_cell_char, cursor_cell_flags) = {
            let cell = &term.grid()[cpoint];
            (cell.c, cell.flags)
        };
        if snapshot_changed {
            // 内容变化或 IME 活跃：重新克隆所有可见格子。
            // 缓冲跨帧复用（从 Session 取出、渲染尾归还）：省掉每帧 rows×cols 的
            // Vec 分配/回收，全屏 TUI 动画 60fps 时每秒少几万次分配。
            let mut snapshot_cells = std::mem::take(&mut sess.snapshot_scratch);
            snapshot_cells.clear();
            snapshot_cells.reserve(rows * cols);
            for indexed in content.display_iter {
                let point = indexed.point;
                let vline = point.line.0 + offset as i32;
                if vline >= 0 && vline < rows as i32 {
                    snapshot_cells.push((point, indexed.cell.clone()));
                }
            }
            (
                offset, colors_vec, cursor, sel_range, show_cursor,
                snapshot_cells, cursor_cell_char, cursor_cell_flags,
            )
        } else {
            // 静止帧：复用上一帧快照，跳过 rows×cols 次 cell clone。
            // term 锁仍需获取（读 cursor/selection/colors），但不再遍历 display_iter。
            let snapshot_cells = std::mem::take(&mut sess.snapshot_scratch);
            (
                offset, colors_vec, cursor, sel_range, show_cursor,
                snapshot_cells, cursor_cell_char, cursor_cell_flags,
            )
        }
        // term 锁在此释放：解析线程可立即处理积压输出。
    };
    if latency_debug() {
        let us = snap_started.elapsed().as_micros() as u32;
        SNAP_ACC_US.fetch_add(us as u64, Ordering::Relaxed);
        SNAP_MAX_US.fetch_max(us, Ordering::Relaxed);
        let n = SNAP_FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 300 == 0 {
            eprintln!(
                "[latency] snapshot avg={}us max={}us frames={n}",
                SNAP_ACC_US.load(Ordering::Relaxed) / n as u64,
                SNAP_MAX_US.load(Ordering::Relaxed),
            );
        }
    }
    // 记录本次 snapshot 对应的 parse_gen，下帧比对决定是否跳过 clone。
    if snapshot_changed {
        sess.snapshot_gen = sess.parse_gen.load(Ordering::Relaxed);
    }
    let canvas_bg = color_for(dark, TERM_BG_DARK, TERM_BG_LIGHT);

    // ── 预计算 ANSI 256 色查找表：把每格 resolve_color 的 match+from_rgb
    //    降为一次数组索引。静止帧复用上一帧的查找表，跳过 256 次循环。
    let ansi_rgb = if snapshot_changed {
        let mut rgb: [Color32; 256] = [Color32::TRANSPARENT; 256];
        for i in 0..256usize {
            if let Some(Rgb { r, g, b }) = colors_vec[i] {
                rgb[i] = Color32::from_rgb(r, g, b);
            }
        }
        sess.cached_ansi_rgb = Some(rgb);
        rgb
    } else {
        sess.cached_ansi_rgb.unwrap_or([Color32::TRANSPARENT; 256])
    };
    let default_fg = color_for(dark, Color32::WHITE, Color32::BLACK);
    let default_bg = canvas_bg;

    // ── 网格层绘制：背景矩形与字形分两列收集、背景在前拼接。
    // 曾做过「静止帧形状缓存」（跨帧存 Shape 列表，内容未变直接重放），
    // 但主题/页签切换后重放的旧字形引用失效，出现栅格化乱码，已删除；
    // 保留的收益：同底色相邻槽合并成一个大矩形（TUI 状态栏/选中高亮从
    // cols 个矩形降到 1 个）+ 背景层与字形层分离后的批量提交。
            // 背景与字形分两列收集：同底色相邻槽先在 bg_run 里合并成一个大矩形，
            // 合并跨格进行、只能循环结束后落笔——若与字形同列表会盖住字形。
            // 格子矩形互不重叠，全局「背景层在下」与逐格交错绘制结果一致。
            let mut bg_shapes: Vec<egui::Shape> = Vec::new();
            let mut bg_run: Option<(Rect, Color32)> = None;
            let mut fg_shapes: Vec<egui::Shape> = Vec::new();

    // ── 静止帧快速路径：渲染结果未变时直接重放缓存，跳过逐格循环。
    //    缓存形状不含光标，光标在函数末尾统一绘制（两种路径共享）。
    let skip_render = !snapshot_changed && sess.cached_render_shapes.is_some();
    if skip_render {
        if let Some(cached) = sess.cached_render_shapes.take() {
            painter.add(egui::Shape::Vec(cached));
        }
    }

    if !skip_render {
    // 逐格渲染：每格钉在 col*cell_w 的精确位置，宽字符画满 2 格。
    // 不能再用整行 LayoutJob 排版：CJK fallback 字体（msyh）的字形宽度实测
    // 14pt，不等于等宽字体 M 的 2 倍（约 16.86pt），整行排版时每个宽字都会
    // 让后面所有格子向左漂移约 2.9pt，光标/选区位置全部错位。
    //
    for (point, cell) in &snapshot_cells {
        let vline = point.line.0 + offset as i32;
        if vline < 0 || vline >= rows as i32 {
            continue;
        }
        // 随空格（宽字符占位格）直接跳过：它的 2 格槽矩形与前导格完全重叠，
        // 槽底色已由前导格涂满（cell_selected 对 WIDE_CHAR 右扩 1 格，选区盖住
        // 随空格时前导格亦命中选中）。若在这里再 rect_filled，会在字形画完后
        // 盖住它 —— 就是选中时汉字“消失”的根因。它自身永远无字形，先走先跳。
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let col = point.column.0 as usize;
        let x = rect.left() + col as f32 * cell_w;
        let y = rect.top() + vline as f32 * cell_h;
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        // 视觉槽：宽字符前导格占 2 格；其随空格（SPACER）连回前导格占满 2 格；
        // 窄格 1 格。背景/选区/光标下划线统一按槽绘制——选区边界落在宽字符的
        // 任意一列（含随空格）时整个汉字同色，不会再出现左半正常色、右半被高亮
        // 盖住的“半字”效果。字形仍左对齐画在前导格起点（spacer 无字形）。
        let (slot_col, slot_cells) = cjk_slot(col, wide, false);
        let x_slot = rect.left() + slot_col as f32 * cell_w;
        let slot_w = slot_cells as f32 * cell_w;

        let underlined = cell.flags.contains(Flags::UNDERLINE);
        let selected = sel_range.as_ref().is_some_and(|r| cell_selected(r, *point, cell));

        // 用预计算查找表替换 resolve_color：Indexed/Named 走数组，Spec 直转。
        let resolve = |c: Color, default: Color32| -> Color32 {
            match c {
                Color::Spec(Rgb { r, g, b }) => Color32::from_rgb(r, g, b),
                Color::Indexed(i) => ansi_rgb.get(i as usize).copied().unwrap_or(default),
                Color::Named(n) => {
                    use alacritty_terminal::vte::ansi::NamedColor;
                    match n {
                        // Foreground/Background 是逻辑色：用主题默认色，
                        // 不走 ansi_rgb（那里存的是终端配置的浅灰色默认值）。
                        NamedColor::Foreground => default_fg,
                        NamedColor::Background => default_bg,
                        _ => ansi_rgb.get(n as usize).copied().unwrap_or(default),
                    }
                }
            }
        };
        let (mut fg, mut bg) = (resolve(cell.fg, default_fg), resolve(cell.bg, default_bg));
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        // 浅/深主题下各取所需：浅色压暗过浅颜色、深色提亮过暗颜色（色相不变），
        // 其余沿用 ANSI 原色 —— 保留终端原本的配色，只做可读性微调。
        let (fg, bg) = if dark { adapt_to_dark(fg, bg) } else { adapt_to_light(fg, bg) };
        // 选中格统一为浅灰底深字，深浅一致。
        // （蓝底白字在旧代码里曾遮盖汉字：因为随空格在字形后涂背景，盖住了
        // 前导格刚画的宽字 —— 见上文随空格在背景前跳过。）
        let (fg, bg) = if selected {
            (Color32::from_gray(24), Color32::from_gray(176))
        } else {
            (fg, bg)
        };

        let ch = if cell.c == '\0' { ' ' } else { cell.c };

        // 哈希先行：空白快速跳过的格子也要入表，保证 rows×cols 全覆盖可 diff。
        // GPU 路径未启用时跳过，省下每格的哈希开销。
        if sess.gpu.is_some() {
            let g = sess.gpu.as_mut().unwrap();
            let idx = vline as usize * cols + col;
            let mut h = 0xcbf2_9ce4_8422_2325u64 ^ (idx as u64);
            hash_mix(&mut h, ch as u64);
            hash_mix(&mut h, color_key(fg));
            hash_mix(&mut h, color_key(bg));
            hash_mix(
                &mut h,
                underlined as u64 | ((wide as u64) << 1) | ((selected as u64) << 2),
            );
            g.hash_scratch[idx] = h;
        }

        // 空白格快速跳过：默认底色、未选中（否则底色已是高亮灰）、无下划线的
        // 空格无任何可见输出，连 LayoutJob 都不用建 —— 空屏帧的主要开销就在这。
        if ch == ' ' && bg == canvas_bg && !underlined {
            continue;
        }

        // 背景与画布底色不同（选中/反色/自定义底色）时整格涂背景。宽字槽宽占满
        // 2 格（随空格已跳过，此处只画前导格），只靠字形背景（14pt）会露右半格。
        // 同行相邻同底色槽并入当前 run（x 坐标由同一算式产生，相邻列精确相等）。
        if bg != canvas_bg {
            let slot = Rect::from_min_size(Pos2::new(x_slot, y), Vec2::new(slot_w, cell_h));
            match bg_run.as_mut() {
                Some((run, run_bg)) if *run_bg == bg && run.right() == slot.left() && run.top() == slot.top() => {
                    run.max.x = slot.max.x;
                }
                _ => {
                    if let Some((run, c)) = bg_run.take() {
                        bg_shapes.push(egui::Shape::rect_filled(run, 0.0, c));
                    }
                    bg_run = Some((slot, bg));
                }
            }
        } else if let Some((run, c)) = bg_run.take() {
            bg_shapes.push(egui::Shape::rect_filled(run, 0.0, c));
        }

        // GPU 批渲染优先：图集命中（含本次成功入库）直推 quad；空槽回落下方
        // galley 路径（emoji 等缺字形格子逐格混合，不整屏切换）。
        if let Some(g) = sess.gpu.as_mut() {
            let slot = g.atlas.glyph(ch);
            if slot.w > 0.0 {
                let baseline = y + g.atlas.baseline_rel(cell_h);
                let uv_solid = GlyphAtlas::solid_uv();
                // 位图与显示 1:1，但落点若是小数设备像素，LINEAR 采样会混入
                // 邻素发虚 —— 原点对齐设备像素网格保证锐利。
                let snap = |v: f32| (v * ppp).round() / ppp;
                g.quads.push(CellQuad {
                    rect: Rect::from_min_size(
                        Pos2::new(snap(x + slot.dx), snap(baseline + slot.dy)),
                        Vec2::new(slot.w, slot.h),
                    ),
                    uv0: Pos2::new(slot.u0, slot.v0),
                    uv1: Pos2::new(slot.u1, slot.v1),
                    color: fg,
                });
                if underlined {
                    g.quads.push(CellQuad {
                        rect: Rect::from_min_size(
                            Pos2::new(x_slot, y + g.atlas.underline_rel(cell_h)),
                            Vec2::new(slot_w, 1.0),
                        ),
                        uv0: uv_solid,
                        uv1: uv_solid,
                        color: fg,
                    });
                }
                continue;
            }
        }

        // Galley 缓存查找/回填。键里的“窄格下划线”只影响 TextFormat.underline；
        // 宽字符下划线是事后手画的横线，不进排版，故宽字该位恒 false。
        let key = (ch, fg, underlined && !wide, wide);
        let galley = match sess.galley_cache.get(&key) {
            Some((g, _)) => g.clone(),
            None => {
                // LRU 淘汰：超限时按 generation 淘汰最旧 25%，
                // 避免整表清空后首帧全量重建的卡顿峰值。
                if sess.galley_cache.len() >= 8192 {
                    let cutoff = sess.galley_gen.saturating_sub(sess.galley_gen / 4);
                    sess.galley_cache.retain(|_, (_, g)| *g > cutoff);
                    sess.galley_gen += 1;
                }
                let mut format = egui::TextFormat {
                    font_id: font_id.clone(),
                    color: fg,
                    underline: Stroke::NONE,
                    // 强制统一行高：CJK fallback 字体（msyh 等）行高与默认等宽字体不同，
                    // 不指定会让含中文的行变高，导致整块内容逐行漂移、与光标/选区错位。
                    line_height: Some(cell_h),
                    ..Default::default()
                };
                if key.2 {
                    format.underline = Stroke::new(1.0, fg);
                }
                let mut job = egui::text::LayoutJob::default();
                if wide {
                    // 宽字左对齐画在 2 格位起点（与终端惯例一致：字身贴槽左沿，
                    // 槽宽仍按 2 格，选区/光标块/下划线盖满整槽；若居中则每个汉字
                    // 左右各内缩 1.43px，字与相邻 ASCII、行首字都显出偏移）。
                    job.wrap.max_width = slot_w;
                }
                let text = ch.to_string();
                job.append(&text, 0.0, format);
                let g = painter.layout_job(job);
                sess.galley_cache.insert(key, (g.clone(), sess.galley_gen));
                g
            }
        };
        fg_shapes.push(egui::Shape::galley(Pos2::new(x, y), galley, Color32::WHITE));

        if wide && underlined {
            fg_shapes.push(egui::Shape::line_segment(
                [Pos2::new(x, y + cell_h - 1.0), Pos2::new(x + slot_w, y + cell_h - 1.0)],
                Stroke::new(1.0, fg),
            ));
        }
    }

    // 收尾：冲掉最后一个背景 run，背景层拼在字形层前面，整层一次性提交。
    if let Some((run, c)) = bg_run.take() {
        bg_shapes.push(egui::Shape::rect_filled(run, 0.0, c));
    }
    // GPU 字形层：内容变化才重建网格；静止帧直接重放上一帧的同一 Arc<Mesh>，
    // 跳过全部 quad 重建（egui 每帧仍会重画它，省的是 CPU 侧组装）。
    if let Some(g) = sess.gpu.as_mut() {
        if let Some(mesh) = g.end_frame(ui.ctx()) {
            bg_shapes.push(egui::Shape::Mesh(mesh));
        }
    }
    bg_shapes.extend(fg_shapes);
    // 缓存完整渲染结果供静止帧重放。
    sess.cached_render_shapes = Some(bg_shapes.clone());
    painter.add(egui::Shape::Vec(bg_shapes));
    } // end if !skip_render

    // 光标：支持方块/下划线/竖线三种形状，带描边；失焦时画空心边框。
    // 定位策略分两种：
    // 1) SHOW_CURSOR 开启的常规应用（cmd/nvim 等）：按 grid 光标位置精确绘制，
    //    方向键移动光标时 grid 光标跟着走，位置永远正确。
    // 2) pi 等 TUI 常驻 DECSET 25 隐藏光标（shape=Hidden、show_cursor=false），
    //    并把终端光标停靠在输入框末尾的固定列——直接照画会停在错误位置，且不随
    //    方向键移动（实测 pi 停靠 (11,79)，输入在 (11,5)）。这类 TUI 会用
    //    “空白格 + 非默认前后景色”的单元格自绘真实输入光标（实测随方向键移动），
    //    所以先在该行找这种自绘光标格、画在那里；找不到才退回停靠位置。
    // 用 HashMap 做 O(1) 查找，替代旧版 snapshot_cells.iter().find() 的 O(n) 线性扫描，
    // 消除 rows×cols×snapshot_cells 的平方级开销。
    use std::collections::HashMap;
    // 只有关闭光标（pi 等 TUI 自绘光标）才需要网格查找表；cmd/nvim 等
    // SHOW_CURSOR 应用跳过构建，省去每帧 rows×cols 次 HashMap 插入。
    let cell_map: Option<HashMap<(i32, usize), &Cell>> = if show_cursor {
        None
    } else {
        Some(snapshot_cells.iter()
            .map(|(p, c)| ((p.line.0, p.column.0), c))
            .collect())
    };
    let mut cursor_rect: Option<Rect> = None;
    {
        // 光标位置：
        // - SHOW_CURSOR 开启（cmd/nvim 等）：按 grid 光标位置，精确跟随方向键。
        // - pi 等 TUI 常驻 DECSET 25 隐藏光标，并把终端光标停靠在最后写入行的
        //   末尾（实测列固定不动，不随方向键走），改用自绘光标：pi 用
        //   (Black,White) 反色格画输入光标，格内是字符或空格，随方向键移动。
        //   自底向上整屏找反色格：输入行在 UI 最下方，必然优先命中；停靠行
        //   是灰色状态行时（启动瞬间）也不会误判。找不到才退回停靠位置。
        let mut cpoint = cursor.point;
        if !show_cursor {
            // 全屏扫描代价 rows×cols，每帧都扫不划算：内容只在解析线程变化，
            // 用解析代数做缓存键——没变就直接复用上次找到的自绘光标位置；
            // 任一变了才重扫并回填缓存。
            let cur_gen = sess.parse_gen.load(Ordering::Relaxed);
            let disp_off = offset;
            let cached = match sess.caret_scan {
                Some((g, line, col)) if g == cur_gen => {
                    Some(Point::new(line, col))
                }
                _ => None,
            };
            let found = cached.or_else(|| {
                let is_caret = |cell: &Cell| {
                    matches!(
                        (cell.fg, cell.bg),
                        (Color::Named(NamedColor::Black), Color::Named(NamedColor::White))
                            | (Color::Named(NamedColor::White), Color::Named(NamedColor::Black))
                    )
                };
                // 自底向上找反色格：输入行在 UI 最下方，必然优先命中。
                let mut hit = None;
                'outer: for r in (0..rows).rev() {
                    let line = Line(r as i32 - disp_off as i32);
                    for col in 0..cols {
                        // O(1) HashMap 查找，替代 O(n) 线性扫描。
                        if let Some(cell) = cell_map.as_ref().and_then(|m| m.get(&(line.0, col))) {
                            if is_caret(cell) {
                                hit = Some(Point::new(line, Column(col)));
                                break 'outer;
                            }
                        }
                    }
                }
                if let Some(p) = hit {
                    sess.caret_scan = Some((cur_gen, p.line, p.column));
                }
                hit
            });
            if let Some(p) = found {
                cpoint = p;
            }
        }
        let p = cpoint;
        let vline = p.line.0 + offset as i32;
        if vline >= 0 && vline < rows as i32 {
            let col = p.column.0 as usize;
            if col < cols {
                // 从快照获取光标格数据（不碰 term 锁）。
                let on_spacer = cursor_cell_flags.contains(Flags::WIDE_CHAR_SPACER);
                let cursor_wide =
                    cursor_cell_flags.contains(Flags::WIDE_CHAR) || on_spacer;
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
                                    // 从快照 HashMap O(1) 获取前导格字符。
                                    let prev = Column(cpoint.column.0.saturating_sub(1));
                                    cell_map.as_ref().and_then(|m| m.get(&(cpoint.line.0, prev.0)))
                                        .map(|c| c.c)
                                        .unwrap_or(cursor_cell_char)
                                } else {
                                    cursor_cell_char
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

    // 主题调色诊断（TUIPM_THEME_DEBUG=1 时启用）：抽样打印前几行非空格格子的
    // 原始 fg/bg 与适配后的最终色，用于定位切题后「黑底黑字」类问题出在
    // 哪一环（原始解析错 or 适配错）。另附每帧缓存命中状态。
    if std::env::var("TUIPM_THEME_DEBUG").as_deref() == Ok("1") {
        static FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let f = FRAME.fetch_add(1, Ordering::Relaxed);
        if f % 30 == 0 {
            eprintln!(
                "[frame] dark={dark} galleys={} gen={}",
                sess.galley_cache.len(),
                sess.parse_gen.load(Ordering::Relaxed),
            );
        }
        let mut dumped = 0;
        for (point, cell) in &snapshot_cells {
            if cell.c == '\0' || cell.c == ' ' || dumped >= 12 {
                continue;
            }
            dumped += 1;
            eprintln!(
                "[theme-debug] ({},{}) ch={:?} flags={:?} raw_fg={:?} raw_bg={:?} dark={}",
                point.line.0,
                point.column.0,
                cell.c,
                cell.flags,
                cell.fg,
                cell.bg,
                dark
            );
        }
        eprintln!(
            "[theme-debug] palette[0..8]={:?} palette_fg={:?} palette_bg={:?} cursor={:?} show_cursor={}",
            (0..8).map(|i| colors_vec[i]).collect::<Vec<_>>(),
            colors_vec[256],
            colors_vec[257],
            cursor.shape,
            show_cursor
        );
    }

    // 归还快照缓冲供下一帧复用（见上方 take 处注释）。
    sess.snapshot_scratch = snapshot_cells;

    // 记录终端状态标志，供 app 页签图标判定 TUI 运行状态。
    sess.alt_screen.store(alt_screen, Ordering::Relaxed);
    sess.cursor_hidden
        .store(cursor.shape == CursorShape::Hidden, Ordering::Relaxed);

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

    // 记录本帧 preedit 状态，供下一帧 snapshot skip 判断是否需要刷新。
    sess.last_preedit.clone_from(&preedit);

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

/// 深色主题：过暗前景在深底提亮到可读（色相比不变）；过亮中性前景归白。
#[cfg(test)]
#[test]
fn dark_adapt_lightens_dark_colors() {
    let lum = |c: Color32| {
        0.2126 * c.r() as f32 / 255.0
            + 0.7152 * c.g() as f32 / 255.0
            + 0.0722 * c.b() as f32 / 255.0
    };
    // ANSI 深蓝 #000080(0,0,139)：向白提亮到 >0.28 亮度，且蓝通道仍最高。
    let navy = Color32::from_rgb(0, 0, 139);
    let (f, _) = adapt_to_dark(navy, TERM_BG_DARK);
    assert!(lum(f) > 0.28, "深蓝应提亮到 >0.28，当前 {f:?} lum={:.3}", lum(f));
    assert!(f.b() > f.r().max(f.g()), "提亮不应改变色相（蓝仍最高），当前 {f:?}");
    // 纯黑前景 → 提亮为中性浅灰，仍可读。
    let (fb, _) = adapt_to_dark(Color32::BLACK, TERM_BG_DARK);
    assert!(lum(fb) > 0.25, "黑色应提亮，当前 {fb:?} lum={:.3}", lum(fb));
    // 过浅背景沉暗为深色画布。
    assert_eq!(adapt_to_dark(Color32::WHITE, Color32::WHITE).1, TERM_BG_DARK);
    // 已亮前景不被压暗。
    assert_eq!(adapt_to_dark(Color32::WHITE, TERM_BG_DARK).0, Color32::WHITE);
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

    #[test]
    fn modified_enter() {
        // 主屏 shell：组合回车回退普通 \r。
        assert_eq!(encode_modified_enter(true, false, false, false), vec![b'\r']);
        // 备屏/kitty：Shift/Alt/Ctrl+Enter 按 CSI-u，修饰符 = 1+s+2a+4c。
        assert_eq!(
            encode_modified_enter(true, false, false, true),
            b"\x1b[13;2u".to_vec()
        );
        assert_eq!(
            encode_modified_enter(false, true, false, true),
            b"\x1b[13;3u".to_vec()
        );
        assert_eq!(
            encode_modified_enter(true, false, true, true),
            b"\x1b[13;6u".to_vec()
        );
    }

    /// 相对路径转换：大小写不敏感、目录边界必须落分隔符、目录外保留绝对。
    #[test]
    fn rel_path_vs_cwd() {
        // 目录内 → 相对（cwd 大小写不同也成立）。
        assert_eq!(
            rel_to_cwd(r"D:\code\my-project\src\main.rs", r"D:\CODE\my-project"),
            r"src\main.rs"
        );
        // 前缀歧义：D:\code 不能把 D:\code2 裁掉。
        assert_eq!(
            rel_to_cwd(r"D:\code\my-project2\a.txt", r"D:\code\my-project"),
            r"D:\code\my-project2\a.txt"
        );
        // 就是目录本身 → 绝对。
        assert_eq!(
            rel_to_cwd(r"D:\code\my-project", r"D:\code\my-project"),
            r"D:\code\my-project"
        );
        // 目录外 → 绝对。
        assert_eq!(
            rel_to_cwd(r"E:\other\a.txt", r"D:\code\my-project"),
            r"E:\other\a.txt"
        );
        // 正斜杠统一为反斜杠。
        assert_eq!(rel_to_cwd("D:/code/my-project/a.txt", r"D:\code\my-project"), r"a.txt");
        // 空 cwd（相对目录启动的会话）→ 绝对。
        assert_eq!(rel_to_cwd(r"D:\a.txt", ""), r"D:\a.txt");
    }

    #[test]
    fn path_quote_when_space() {
        assert_eq!(path_for_input(r"src\main.rs"), r"src\main.rs");
        assert_eq!(path_for_input(r"my dir\a.txt"), "\"my dir\\a.txt\"");
        // 含引号直接删掉（cmd 引号内无法转义），仍按含特殊字符加双引号。
        assert_eq!(path_for_input("say\"hi.txt"), "\"sayhi.txt\"");
    }
}
