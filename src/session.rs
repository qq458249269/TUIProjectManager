use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;


use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::{Selection as TermSelection, SelectionRange};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// 渲染快照格子：只保留逐格渲染需要的字段。
/// 不携带 `Cell.extra`（Arc 超链接/稀有下划线色）：渲染路径零读取，
/// 且 `Cell` 因此不能 Copy，`build_snapshot` 每格多一次 Arc clone/drop。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellSnap {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: alacritty_terminal::term::cell::Flags,
}

impl Default for CellSnap {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: alacritty_terminal::term::cell::Flags::empty(),
        }
    }
}
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Processor};
use eframe::egui;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::term_gl::{hash_mix, raw_color_key};

// ── 无锁快照：reader 线程生成，UI 线程通过 AtomicPtr 加载，零锁竞争 ──

/// 终端渲染快照：包含 UI 渲染一帧所需的全部数据。
#[allow(dead_code)]
/// reader 线程在处理完 PTY 输出后生成，通过 AtomicPtr 原子交换给 UI 线程。
pub struct TermSnapshot {
/// 可见格子 (point, cell)。
    pub cells: Vec<(Point, CellSnap)>,
    /// 每可视行（vline = line + offset）的行指纹，下标即 vline。
    /// 在构建快照时顺手算（cells 本来就要逐格 clone）：渲染端据此判定行级
    /// 增量，不用再重扫 rows×cols 个格子。内容相同的行指纹必相同，反之不必。
    pub row_hashes: Vec<u64>,
    /// 显示偏移。
    pub offset: usize,
    /// 光标位置。
    pub cursor_point: Point,
    /// 光标形状。
    pub cursor_shape: CursorShape,
    /// 选区（用于复制和渲染）。
    pub selection: Option<TermSelection>,
/// 选区范围（用于渲染高亮）。
    pub sel_range: Option<SelectionRange>,
    // 原 selected_text 字段已删：全代码零读取，但 build_snapshot 每块输出都
    // 跑一次 selection_to_string()——长回滚区上选着东西时每 64KiB 输出重建
    // 整段选中文本。复制路径另有实现，不依赖这里。
    /// 光标是否可见。
    pub show_cursor: bool,
    /// 调色盘。
    pub colors: Colors,
    /// 终端模式标志。
    pub mode: TermMode,
    /// 光标格的字符和标志（Block 光标反色重绘用）。
    pub cursor_cell_char: char,
    pub cursor_cell_flags: alacritty_terminal::term::cell::Flags,
    /// 产出这份快照时的 parse_gen。UI 用它而不是读当前的 parse_gen：
    /// reader 先推进 gen 再解析（此期间 UI 可能拿到「gen 已变、快照还是旧的」
    /// 的组合），拿 ambient gen 当「我渲染的就是这个」会把旧画面钉成静止帧。
    pub snap_gen: u64,
}

/// 从 term 当前状态构造完整快照（可见格+ 元数据）。
/// reader 线程与 UI 线程共用：后台页签解析时跳过整屏克隆，页签切回前台那帧
/// 由 UI 线程就地重建（reader 阻塞在 read() 上，收不到命令）。
pub fn build_snapshot(t: &Term<SessionListener>, snap_gen: u64) -> TermSnapshot {
    let content = t.renderable_content();
    let offset = content.display_offset;
    let selection = t.selection.clone();
    let sel = selection.as_ref().and_then(|s| s.to_range(t));
    let show = t.mode().contains(TermMode::SHOW_CURSOR);
    let mode = *t.mode();
    let cpoint = content.cursor.point;
    let (cc, cf) = {
        let cell = &t.grid()[cpoint];
        (cell.c, cell.flags)
    };
// 预分配：display_iter 恰好吐 screen_lines×columns 个格。原先两个 Vec 从
    // 零长起，每次重建要 log2(2400)≈11 次 doubling（重分配 + 全量拷贝）——
    // 滚动时每帧一次，是白付的。
    let cap = t.grid().screen_lines() * t.grid().columns();
    let mut cells = Vec::with_capacity(cap);
    // 行指纹：cells 按 vline 升序（display_iter 自下而上，vline = line+offset），
    // 遇行号变化就封口上一行。种子带 vline，滚动换位也算内容变化。
    let mut row_hashes: Vec<u64> = Vec::with_capacity(t.grid().screen_lines());
    let mut cur_line = i32::MIN;
    let mut rh = 0u64;
    for indexed in content.display_iter {
        let p = indexed.point;
        let v = p.line.0 + offset as i32;
        if v < 0 {
            continue;
        }
        if v != cur_line {
            if cur_line != i32::MIN {
                row_hashes.push(rh);
            }
            cur_line = v;
            rh = 0xcbf2_9ce4_8422_2325 ^ v as u64;
        }
        let cell = indexed.cell;
        hash_mix(&mut rh, raw_color_key(cell.fg));
        hash_mix(&mut rh, raw_color_key(cell.bg));
        hash_mix(&mut rh, cell.flags.bits() as u64);
        hash_mix(&mut rh, cell.c as u64 ^ (p.column.0 as u64) << 24);
        cells.push((
            p,
            CellSnap { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags },
        ));
    }
    if cur_line != i32::MIN {
        row_hashes.push(rh);
    }
    TermSnapshot {
        cells,
        row_hashes,
        offset,
        cursor_point: content.cursor.point,
        cursor_shape: content.cursor.shape,
        selection,
sel_range: sel,
        show_cursor: show,
        colors: *content.colors,
        mode,
        cursor_cell_char: cc,
        cursor_cell_flags: cf,
        snap_gen,
    }
}

#[allow(dead_code)]
/// UI → reader 线程的命令：滚动、调整大小、选区更新。
/// reader 线程在处理 PTY 输出的间隙消费这些命令。
pub enum TermCommand {
    Scroll(Scroll),
    Resize { cols: usize, rows: usize },
    UpdateSelection(Option<TermSelection>),
}

/// 回显延迟探针统计（TUIPM_LATENCY_DEBUG=1 启用）：总耗时/次数/峰值，全局共享。
static ECHO_SUM_US: AtomicU64 = AtomicU64::new(0);
static ECHO_CNT: AtomicU64 = AtomicU64::new(0);
static ECHO_MAX_MS: AtomicU32 = AtomicU32::new(0);

fn latency_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("TUIPM_LATENCY_DEBUG").is_ok())
}

/// 终端一行的渲染缓存（指纹不变则整行重放）。
#[derive(Default)]
pub struct RowCache {
    /// 行内容指纹（仅原始字段；不含主题/几何/选区，那些在 row_cache_sig 里）。
    pub hash: u64,
    /// 本行的背景 Shape（已按 run 合并）。
    pub bg: Vec<egui::Shape>,
    /// 本行 galley 回落字形的 Shape（GPU quad 不在此处）。
    pub fg: Vec<egui::Shape>,
}

/// 一个在应用内页签中运行的终端会话。
pub struct Session {
    /// 页签标题（默认取项目名）。
    pub title: String,
    /// 启动目录。
    pub dir: String,
    /// 本页签启动用的 TUI 命令（切命令后用于标记当前项/重载）。
    pub cmd: String,
    /// 终端仿真状态（仅 reader 线程写入）。
    pub term: Arc<RwLock<Term<SessionListener>>>,
    /// 渲染快照：reader 线程生成新 Arc 替换（旧 Arc 由持引用方释放，UI 渲染期
    /// 全程持有自己的 Arc，旧快照不会被并发 drop——替代旧 AtomicPtr 方案的每块
    /// 输出 O(rows×cols) clone + 双份常驻 cells。
    pub snapshot: Arc<std::sync::Mutex<Arc<TermSnapshot>>>,
    /// UI → reader 命令通道：滚动/调整大小/选区更新。
    #[allow(dead_code)]
    pub cmd_tx: std::sync::mpsc::Sender<TermCommand>,
    /// 向 PTY 写入输入的通道发送端（实际写由专用后台线程执行，
    /// 避免子进程不读取输入时阻塞 UI/解析线程）。
    pub writer: std::sync::mpsc::SyncSender<Vec<u8>>,
    /// PTY 主句柄（用于 resize）。Option 包装：kill_in_background() 后取走，
    /// 避免 MasterPty::drop 在 UI 线程阻塞（Windows 上 ClosePseudoConsole 可能卡住）。
    pub master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    /// 子进程（Option 包装：take() 后移入后台线程异步 kill，避免 Child::drop 在 UI 线程阻塞）。
    pub child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// 上次渲染的网格尺寸，用于检测是否需要 resize。
    pub grid_size: (u16, u16),
    /// 新会话首次帧需要强制 resize：解决 ConPTY 初始化时序问题。
    /// spawn 后 ConPTY 管道可能未就绪，子进程输出被阻塞；
    /// 自动 resize 强制 ConPTY 重新同步，无需用户手动调整窗口。
    pub needs_resize: bool,
    /// 当前深浅主题（应答 OSC 10/11 颜色查询用，由 UI 线程更新）。
    pub theme_dark: Arc<AtomicBool>,
    /// 是否应答过 OSC 10/11/4 颜色查询（一旦应答，说明该子进程能理解 OSC 颜色，
    /// 主题切换时的主动颜色广播才安全；shell 等从不查询，收到广播只会乱码）。
    pub osc_theme_aware: Arc<AtomicBool>,
    /// 本会话是否为前台页签（UI 线程每帧同步 self.current）。后台会话的
    /// 输出仍照常解析（管道不能停读），但不唤醒 UI 重绘。
    pub foreground: Arc<AtomicBool>,
    /// UI → 解析线程无信号：解析线程每消费一块前台输出后发一个合并信号，
    /// logic() 每帧消费一次并据此决定是否按配置帧率唤醒 UI。
    pub redraw_rx: std::sync::mpsc::Receiver<()>,
    /// 子进程是否已退出（reader 线程写、UI 线程读，无需 term 锁）。
    pub exited: Arc<AtomicBool>,
    /// 是否已发送过「运行结束」提醒：正常退出置位后只提醒一次；
    /// kill_in_background 提前置位，重启/切命令/关闭页签等程序化终止不弹通知。
    pub notified: Arc<AtomicBool>,
    /// 会话创建时刻（单调毫秒，见 crate::now_ms）。UI 用它做启动宽限期判定：
    /// 启动后 10 秒内（STARTUP_GRACE_MS）不弹通知，过滤启动瞬间输出高峰的误报。
    pub started_ms: Arc<AtomicU64>,
    /// 是否已发送过本轮 ✅ 的「任务完成」提醒：进入 ✅ 稳定后提示一次，
    /// 离开 ✅ 复位（每轮完成都可再弹，见 app.rs update_done_states）。
    pub done_notified: Arc<AtomicBool>,
    /// 本页签最近一次系统通知的单调毫秒（0 = 从未）。同页签 10s 内最多一条
    /// （完成/退出共用，TOAST_MIN_INTERVAL_MS）。
    pub last_toast_ms: Arc<AtomicU64>,
    /// 后台上次收割子进程（try_wait）的时刻：后台每 BG_REAP_MS 轮一次退出。
    pub last_reap_ms: Arc<AtomicU64>,
    /// 进入 ✅ 状态的时刻（毫秒）。0 = 不在该状态。UI 页签检测到状态后需
    /// 稳定停留 DONE_STABLE_MS 才提醒，过滤 top/watch/编译间歇输出等周期性
    /// 进程在 🔄↔✅ 间横跳造成的重复误报。
    pub done_since_ms: Arc<AtomicU64>,
    /// 上次认领的剪贴板序列号（复制文件后 Ctrl+V 的兜底识别，见 show_terminal）。
    pub last_clipboard_seq: Option<std::num::NonZeroU32>,
    /// 最近一次有输出的绝对时间戳（毫秒），供 UI 精确判定连续输出是否已停。
    /// 含转义/动画块：页签 🔄 图标用——动画重绘也算在跑（spinner/状态栏
    /// 刷新保持旋转）。
    pub last_output_ms: Arc<AtomicU64>,
    /// 累计输出次数（读取线程写、UI 线程读），用于判断是否有持续输出活动。
    pub output_count: Arc<AtomicU32>,
    /// 累计实质输出字节数（非动画块的可打印字节，读取线程写、UI 线程读）。
    /// 通知过滤判据：会话从未显示过实质内容（纯零输出/秒退/仅 spinner 动画，
    /// 动画不属于可打印字节）时，「运行结束」/「任务完成」都不弹通知
    /// （见 app.rs MIN_OUTPUT_BYTES）。
    pub out_bytes: Arc<AtomicU64>,
/// 「已查看」标记：true = 用户已经看过当前内容（只影响 `任务完成` 系统
    /// 通知，不影响 ✅ 图标——激活页签跑完也照亮）。
    /// 启动即 true（spawn 产物不需要提醒）；读取线程在「加载期之后」的每轮
    /// 新内容输出时复位 false → 完成通知只属于用户没看过的真输出轮；
    /// 点击/切换到该页签（TabAction::Activate、Ctrl+Tab）、在终端里做任何
    /// 交互（点击/拖选/滚轮/打字，terminal.rs show_terminal 入口统一记账）
    /// 置回 true。
    ///
    /// 语义是「**有没有交互过**」，不是「是不是当前页签」：当前页签跑完同样
    /// 亮 ✅（用户要求），一碰就消。系统通知另有 watched() 管「正盯着不打扰」。
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
    /// 最近一次**屏幕可见字符真的变了**的时刻（reader 逐块比对可见格快照）。
    /// 「在动」的唯一可靠信号：spinner/时钟/进度条都改字符，而纯转义重绘
    /// （整屏重画同样的字）不改——后者正是字节启发式把静止画面误判成「在跑」
    /// 的根源。0 = 从未变过。
    pub last_screen_change_ms: Arc<AtomicU64>,
    /// 最近一次「**成规模**内容变化」的时刻：单块变化 ≥ [`MIN_CONTENT_CELLS`]
    /// 个可见格。完成/🔄 内容窗口只认它——秒表、时钟、动画小数位这类每次只改
    /// 1~3 格的周期性刷新不算内容（否则永远「有内容」→ 🔄 常驻、✅ 亮不出来）。
    pub last_content_ms: Arc<AtomicU64>,
    /// 「运行/完成」状态快照的上次重算时刻（毫秒）。门限 STATE_CHECK_MS（1s）：
    /// 每个页签最快一秒重算一次，其余帧复用快照——降低检测频率，也让图标不再
    /// 亚秒抖动（🔄↔✅ 来回闪）。
    pub state_check_ms: Arc<AtomicU64>,
    /// 状态快照（app.rs 写）：低 2 位 = 图标码，高 2 位 = 判定时的 exited /
    /// loading。渲染与「任务完成」通知共用这一份，保证两者不各判各的。
    pub state_icon: Arc<AtomicU8>,
    /// 状态快照（app.rs 写）：是否处于「完成态」（通知判据用，与图标同源同刻）。
    pub state_done: Arc<AtomicBool>,
    /// 回显延迟探针：最近一次向 PTY 写入输入字节的毫秒时间戳。
    /// 读取线程据此计算「按键 → 首块回显」延迟（TUIPM_LATENCY_DEBUG=1 打印）。
    pub last_input_ms: Arc<AtomicU64>,
    /// 最近一次**转发**滚轮给子进程的毫秒时间戳（鼠标上报/备用屏路径才写）。
    /// TUI 收到滚轮后立即整屏重绘回显 → reader 刷新 last_output_ms → 页签
    /// 误亮 🔄。记入专用短窗口（app.rs SCROLL_ECHO_MS，仅 500ms）：只吞滚动
    /// 驱动的这一下重绘回显；真实任务输出晚于窗口即照常判 🔄。
    /// 本地缓冲滚动（普通 shell）不产生 PTY 输出，不写此字段。
    pub last_scroll_ms: Arc<AtomicU64>,
    /// 主键按下时的位置（仅 UI 线程用）：快速拖选兜底判定用。
    /// 低帧率下按下/拖动/释放全落在同一帧时，egui 既不判 click 也不判
    /// drag，drag_started_by 永不触发 —— 这里自己记按下点。
    pub drag_press_pos: Option<egui::Pos2>,
    /// 鼠标按下时的位置（纯点击判定用）：press_origin() 在释放帧返回 None，
    /// 自己从 raw events 捕获按下坐标，用于区分点击与拖动。
    pub click_press_pos: Option<egui::Pos2>,
    /// 已转发给子进程的鼠标按下（code, col, row, sgr）：等释放帧分类后补发配对释放。
    /// None = 没有待配对的按下（普通点击手势已立即转发）。
    pub mouse_press_pending: Option<(u16, usize, usize, bool)>,
    /// 本次主键手势被判为本地选区手势（拖选/快速拖选兜底建出选区）。
    /// 释放帧据此吞掉配对的按下/释放，不给子进程发幽灵点击。
    pub mouse_gesture_sel: bool,
    /// 上一帧 IME 预编辑文本（拼音等）：非空时强制刷新快照，避免跳过 clone
    /// 导致输入法组合/提交时内容不同步。
    pub last_preedit: String,
    /// 静止帧缓存：跳过 clone 时复用上一帧的 ANSI 查找表（256 项 Color32）。
    pub cached_ansi_rgb: Option<[egui::Color32; 256]>,
    /// 索引色「解析 + 主题适配」预烘焙表（fg/bg 各 256 项）。
    pub cached_color_lut: Option<([egui::Color32; 256], [egui::Color32; 256])>,
    /// 上一帧快照对应的 parse_gen：用于检测 snapshot 是否有新内容，跳过无变化帧的重渲染。
    pub last_snapshot_gen: u64,
    /// 上一帧快照偏移：仅 offset 变化时做 O(rows) 的 point.line 平移，
    /// 避免 O(rows×cols) 的全量 clone。
    pub last_snapshot_offset: usize,
    /// 缓存的字体度量：(cell_w, cell_h, ppp)。字号/DPI 不变时跳过 fonts_mut 查询。
    pub cached_metrics: Option<(f32, f32, f32)>,
    /// 静止帧缓存：完整渲染结果（bg_shapes + GPU mesh + fg_shapes），
    /// snapshot_changed=false 时直接重放，跳过逐格渲染循环。
pub cached_render_shapes: Option<Vec<egui::Shape>>,
    /// 行级渲染缓存：每行内容指纹 + 该行的背景 Shape + galley 回落字形。
    /// 流式输出时只有一两行变，其余行直接重放，不再重跑逐格循环。
    pub row_cache: Vec<RowCache>,
    /// 生成 row_cache 时的帧参数指纹（主题/几何/选区/字体图集世代）；与当前
    /// 帧不一致则整份缓存作废。
    pub row_cache_sig: u64,
    /// GPU 字形批渲染状态：None = 未初始化或初始化失败（整格走 galley 回落）。
    /// 字形位图与纹理全进程共享（见 term_gl::SHARED_ATLASES），本字段只留
    /// 本页签私有的帧缓冲，多页签不再各占一份 4MB 图集。
    pub gpu: Option<crate::term_gl::TermGpu>,
    /// 会话是否仍在启动中（首次有实际输出后置 false）。
    /// UI 线程据此显示旋转 ⚙️ 加载动画。
    pub loading: Arc<AtomicBool>,
    /// 快照里的 cells 已过期：后台页签解析时 reader 只刷时间戳、不重建整屏
    /// 格子（画面不可见）。该页签切回前台时 UI 用当前网格重建一帧并清此标记。
    pub cells_stale: Arc<AtomicBool>,
}

/// 把字节转义成可打印日志片段（保留 ESC/CSI 原形，其余不可打印字节转义）。
fn escape_for_log(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len().min(600) * 2);
    for &b in bytes.iter().take(300) {
        match b {
            0x1b => s.push_str("\\e"),
            0x07 => s.push_str("\\a"),
            0x0d => s.push_str("\\r"),
            0x0a => s.push_str("\\n"),
            0x09 => s.push_str("\\t"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

/// 跑一次可能 panic 的闭包（reader 线程唯一的存活保证）。
/// Err(()) = panic 已被拦下，调用方负责收尾：置 exited + 跳出读循环。
/// 原始 panic 仍会先经 main.rs 全局钩子写进 crash.log（先 hook 后 unwind）。
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, ()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|_| ())
}

/// 连续读到几次 EOF 才退出读循环（纯函数，见 [`eof_should_exit`]）。
const EOF_EXIT_STREAK: u32 = 3;

/// 读到 EOF 时是否该退出读循环。`is_eof=false`（有数据/读错）清零计数：
/// 瞬时读错不得当成退出（曾因此置永久 ❌）。
fn eof_should_exit(is_eof: bool, streak: &mut u32) -> bool {
    if !is_eof {
        *streak = 0;
        return false;
    }
    *streak += 1;
    *streak >= EOF_EXIT_STREAK
}

/// 取写锁；已中毒时照旧拿（`into_inner`）。
/// 解析 panic 被 `guarded` 拦下后锁即中毒：若此后各处走 `unwrap()` /
/// `if let Ok(..)` 的失败分支，快照也永远刷新不了 = 画面继续冻结在最后一帧。
fn wlock<'a, T>(r: std::sync::LockResult<std::sync::RwLockWriteGuard<'a, T>>) -> std::sync::RwLockWriteGuard<'a, T> {
    match r {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// 取读锁；已中毒时照旧拿（见 `wlock`）。
/// 取读锁；已中毒时照旧拿（`into_inner`）。
pub fn rlock<'a, T>(r: std::sync::LockResult<std::sync::RwLockReadGuard<'a, T>>) -> std::sync::RwLockReadGuard<'a, T> {
    match r {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// UI 线程版的取锁：**繁忙就拿不到，绝不排队等**。
///
/// 与 `wlock`/`rlock` 同一条中毒策略（`into_inner`），但对「锁被占」返回
/// `None` 而不是阻塞：reader 每解析一块 PTY 输出就抢一次锁，而 `bf7b4d6` 的
/// 16ms 交互帧间隔让滚动/拖选这类每帧都碰 term 的操作跑到 60 次/秒 ——
/// 阻塞版直接把 UI 线程焊在锁上（滚轮拖死窗口就是这条路径）。
/// 拿不到时调用方要么把活丢给 reader 的指令队列，要么跳过本帧。
pub fn try_wlock<T>(m: &std::sync::RwLock<T>) -> Option<std::sync::RwLockWriteGuard<'_, T>> {
    match m.try_write() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}
/// 读侧同 [`try_wlock`]。
pub fn try_rlock<T>(m: &std::sync::RwLock<T>) -> Option<std::sync::RwLockReadGuard<'_, T>> {
    match m.try_read() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
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
        // 后台会话不唤醒 UI：画面反正不可见，切回该页签的那一帧自然会重绘；
        // 页签标题/活动点由基线轮询更新。解析照常（管道不能停读）。
        // 前台会话：仅发合并信号（容量 1，满则丢弃），由 logic() 统一按帧调度
        // request_repaint——避免每个输出块直接抢唤醒，持续输出期间把整窗
        // 帧率顶到 CPU 全速，干扰 winit 的 hover 跟踪（悬停激活失效）。
        if self.foreground.load(Ordering::Relaxed) {
            let _ = self.redraw.try_send(());
        }
    }
}

/// 响应终端能力探测序列（TUI 启动时常用），返回要写回 PTY 的应答字节。
/// 返回值（字节, 是否应答过 OSC 10/11/4 颜色查询）：后者供主题广播判断
/// 该会话是否 OS 色，避免向 cmd 等不响 OSC 的 shell 推颜色序列。
/// 应答项：
/// - DSR 光标位置（ESC[6n / ESC[?6n → ESC[r;cR）
/// - 主 DA（ESC[c / ESC[0c → ESC[?62;c，VT220）
/// - DECRQM 模式查询（ESC[?...$p → ESC[?...;m$y）
/// - kitty 键盘协议查询（ESC[?u → 回同样 ESC[?u 表示不支持）
/// - 主键增强查询（ESC[?2;1;0S → ESC[?0u）
/// - XTWINOPS 像素尺寸（ESC[14t，未知时回 0）
/// - OSC 10/11/4 颜色查询（ESC]10;? 等 → rgb 值，随当前主题）
///
/// XTVERSION（ESC[>0q）不答：没有对端消费它，泄漏为键盘输入纯是噪声。
///
/// DSR/DA 必须答：本机实测（裸 ConPTY + cmd.exe，捆绑 1.25 与系统内置
/// 行为一致）ConPTY **不会**替子进程应答这两条，cmd.exe 发出
/// `ESC[6n ESC[c` 后就停在初始化等应答，既不打横幅也不出提示符——按键、
/// 拖选、Ctrl+C/V 全都像失灵。92d9235 曾以「多回一份 CPR 会变成 4 个 C」
/// 为由停答，那是本末倒置：不答直接换来「终端永不启动」。
/// 返回 (应答字节, 是否应答了 OSC 颜色查询)。
/// 查询序列可能跨块被截断：OMP 等程序的查询序列可能被 read() 切分到
/// 相邻块中，扫描单块可能漏掉。调用方在外层已做跨块拼接（leftover 缓冲），
/// 因此此函数只需处理完整/不完整序列即可。
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
            } else if osc_num == 4
                && let Some(rest) = body.strip_suffix(b";?")
                && let Ok(idx) = String::from_utf8_lossy(rest).parse::<u32>()
                && idx <= 15
            {
                osc_color = true;
                out.extend_from_slice(
                    format!("\x1b]4;{idx};rgb:000000/000000/000000\x1b\\").as_bytes(),
                );
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
        if (params.is_empty() || params == b"0") && fin == b'c' {
            // Primary Device Attributes (DA)：报 VT220 + 常见能力位。
            // 不答的话 cmd/pwsh/node 系会卡在启动探测上（见函数文档实测）。
            out.extend_from_slice(b"\x1b[?62;1;2;6;9;15;22c");
} else if (params.is_empty() || params == b"6") && fin == b'n' {
            // DSR 光标位置（CPR）：回当前可见光标的 1-based 行;列。
            // 滚动缓冲偏移要减掉——子进程问的是「屏幕上哪」，不是「历史上哪」。
            let content = term.renderable_content();
            let row = (content.cursor.point.line.0 - content.display_offset as i32 + 1).max(1);
            let col = (content.cursor.point.column.0 + 1).max(1);
            out.extend_from_slice(format!("\x1b[{row};{col}R").as_bytes());
        } else if params == b"?6" && fin == b'n' {
            // ESC[?6n（DECXCPR）同 CPR，按私有形态再答一份。
            let content = term.renderable_content();
            let row = (content.cursor.point.line.0 - content.display_offset as i32 + 1).max(1);
            let col = (content.cursor.point.column.0 + 1).max(1);
            out.extend_from_slice(format!("\x1b[?{row};{col};1R").as_bytes());
        } else if params.starts_with(b"?") && fin == b'u' {
            // kitty 键盘协议不支持：按协议回同样的 CSI ? u。
            out.extend_from_slice(b"\x1b[?u");
        } else if params.starts_with(b"?") && fin == b'S' {
            // 主键增强查询（OMP/ink 启动时发 \x1b[?2;1;0S）：
            // 应答"不支持主键增强"(\x1b[?0u)，强制宿主回退传统按键编码。
            // 若不答，OMP 判定键盘协议未确认，可能丢弃未按协议编码的裸字符 `1`
            //（深色下不回显、浅色 OSC 后才恢复即为该态）。
            out.extend_from_slice(b"\x1b[?0u");
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

/// 去掉 PTY 输出字节流中的孤儿 CSI-u 残片（kitty 键盘协议回显时
/// ESC 前缀被 ConPTY 吞掉，只剩 `[13;5u`、`[57442;1:3u` 等可见文本）。
/// 不影响真正的 ESC 转义序列；只处理无 ESC 前缀的 `[数字;数字u` 残片。
/// 替换规则与 `strip_ansi` 的孤儿逻辑一致：每个连续残片段替换为
/// 单个空格（残片后已有空白则吞掉多余空白）。
/// 结果覆盖写入调用方缓冲（reader 每块复用同一份，避免每块新分配；不清空就会
/// 逐块累积，历史字节被反复重解析/重应答 → 子进程收到无穷多条重复应答）。
fn strip_orphan_csi_u_bytes_into(out: &mut Vec<u8>, bytes: &[u8]) {
    out.clear();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // 真正的 ESC 转义序列：原样复制。
            out.push(b);
            i += 1;
            if i < bytes.len() {
                out.push(bytes[i]);
                let next = bytes[i];
                i += 1;
                if next == b'[' {
                    // CSI：跳过参数字节 (0x30..=0x3F) + 中间字节 (0x20..=0x2F) + 终止字节
                    while i < bytes.len()
                        && ((0x20..=0x2f).contains(&bytes[i]) || (0x30..=0x3f).contains(&bytes[i]))
                    {
                        out.push(bytes[i]);
                        i += 1;
                    }
                    if i < bytes.len() && (0x40..=0x7e).contains(&bytes[i]) {
                        out.push(bytes[i]);
                        i += 1;
                    }
                } else if next == b']' {
                    // OSC：消耗到 BEL 或 ESC \
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            out.push(bytes[i]);
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            out.push(bytes[i]);
                            out.push(bytes[i + 1]);
                            i += 2;
                            break;
                        }
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
        } else if b == b'[' {
            // 孤儿 CSI-u 探测：`[` + 纯参数(数字/;/:) + `u` → 整段丢弃；
            // 末尾截断残片 `[57442;1`（有参数但无 u）也丢弃。
            let mut j = i + 1;
            let mut n = 0usize;
            let mut seps = 0usize;
            while j < bytes.len() {
                let pb = bytes[j];
                if pb.is_ascii_digit() {
                    j += 1;
                    n += 1;
                } else if pb == b';' || pb == b':' {
                    j += 1;
                    n += 1;
                    seps += 1;
                } else if pb == b'u' && n > 0 {
                    // 完整孤儿 `[数字;数字:数字u`
                    j += 1; // 包含 u
                    break;
                } else {
                    break;
                }
            }
            let skip = if (j > i + 1 && bytes.get(j - 1) == Some(&b'u') && n > 1)
                || (n > 0 && seps > 0 && j >= bytes.len())
            {
                j - i
            } else {
                0
            };
            if skip == 0 {
                out.push(b'[');
                i += 1;
            } else {
                i += skip;
                // 吞掉残片后紧跟的空白（断词位置原有空白）
                while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                    i += 1;
                }
                // 补一个空格还原断词，但不重复（前一字符已是空白则跳过）
                if !out.last().is_some_and(|&c| c == b' ' || c == b'\t' || c == b'\n') {
                    out.push(b' ');
                }
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
}

/// 测试用包装：一次性缓冲版本（生产路径走 into 版复用缓冲）。
#[cfg(test)]
fn strip_orphan_csi_u_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    strip_orphan_csi_u_bytes_into(&mut out, bytes);
    out
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

// 页签状态判定已改纯内容制（见 app.rs tab_icon）：进程树 CPU 采样与后台
// 锁存全量移除——间隔输出/静默间隙一律按「3s 无内容即完成」判定。

/// loading 标志的墙钟上限（见 Session::loading_active）。reader 线程只在「有
/// 输出」时才会清 loading（首个非动画块 / 3s 兜底）；零输出会话（sleep、静默
/// 命令、后台 daemon）永远不会触发清理 → 页签 🔄 常驻、误报「加载中」。消费侧
/// 统一走 loading_active()：超过窗口后无论标志是否仍为 true 都不再显示加载态。
const LOADING_MAX_MS: u64 = 5_000;

/// 一块输出里至少这么多可见格变了，才算「内容变化」（刷 last_content_ms）。
///
/// 分档理由：命令输出/回答正文一次变几十上百格；秒表、时钟、动画小数位一次只
/// 变 1~3 格，却每秒都来。若把它们也算内容，画面再静止 `last_content` 也永远
/// 新鲜 → 🔄 常驻、✅ 亮不出来。4 格是分界：词级流式输出（4~6 格）算内容，
/// 单字符 tick 不算。
/// ponytail: 逐字动画（如逐格点阵 loader）会漏判成「静止」——真出现再降到 2。
const MIN_CONTENT_CELLS: usize = 4;

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
    history_lines: u32,
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
    // TERM_PROGRAM 让 oh-my-posh 等工具识别终端身份，跳过 DA 查询。
    // 不答 DA（ConPTY 时序错位时响应会泄漏为键盘输入，出现杂字 C）。
    cmd.env("TERM_PROGRAM", "mintty");
    cmd.env("TERM_PROGRAM_VERSION", "3.7.5");
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
    let writer_alive = Arc::new(AtomicBool::new(true));
    let writer_alive_clone = writer_alive.clone();
    std::thread::spawn(move || {
        let mut writer = writer;
        let mut write_count: u64 = 0;
        // ponytail: TUIPM_LOG_WRITES=1 时把所有写入 PTY 的字节打到 stderr，排查杂散输入用。
        let log_writes = std::env::var("TUIPM_LOG_WRITES").is_ok();
        while let Ok(bytes) = writer_rx.recv() {
            if log_writes {
                eprintln!("[pty-write] {:?}", String::from_utf8_lossy(&bytes));
            }
            match writer.write_all(&bytes) {
                Ok(()) => {
                    let _ = writer.flush();
                    write_count += 1;
                }
Err(e) => {
                    // ── writer 线程死亡诊断 ──
                    // write_all 失败 = PTY 写入管道断裂（子进程退出、读侧被丢，
                    // 或 reader 线程 panic 后 drop 掉了管道）。此后所有 try_send
                    // 仍成功但数据永远不会到达 PTY —— 必须记进 crash.log：
                    // GUI 无控制台，丢在这里就只剩「终端莫名其妙不动了」。
                    writer_alive_clone.store(false, Ordering::Relaxed);
                    crate::log_crash(
                        "writer-died",
                        &format!("write_all failed: {e} (after {write_count} writes, {} bytes)\n    last_bytes: {}",
                            bytes.len(),
                            escape_for_log(&bytes)),
                    );
                    break;
                }
            }
        }
    });

    // 合并重绘信号：解析线程每消费一块输出发一个（容量 1，满则丢弃合并）。
    // logic() 每帧消费一次并按配置帧率唤醒 UI——替代旧的每块直接
    // request_repaint（持续输出会把整窗帧率顶到 CPU 全速，干扰 winit
    // hover 跟踪 → 悬停激活失效）。
    let (redraw_tx, redraw_rx) = std::sync::mpsc::sync_channel::<()>(1);
    // 退出时唤醒 UI：reader 线程设 exited 后立即 request_repaint（线程安全），
    // 停帧空闲时 `update_exited()` 才能在本帧发现退出（页签✔/状态栏/通知）。
    // 仅退出这一次，无常耗——后台输出/空闲不唤醒。
let reader_ctx = ctx.clone();
    // 前台标记：UI 线程每帧同步 self.current；后台会话输出不唤醒 UI。
    let foreground = Arc::new(AtomicBool::new(false));
    let listener_fg = foreground.clone();
    let listener = SessionListener {
        redraw: redraw_tx.clone(),
        ctx,
        foreground: listener_fg,
    };

    // 开启 kitty 键盘协议跟踪：应用推 CSI > flags u 时仿真器记下 DISAMBIGUATE 位，
    // terminal.rs 据此决定组合回车是否发 CSI-u（协商过了才发，避免对端不认识
    // 被当字面文本插进输入框）。
    // 滚动历史行数：默认 10000 → 2000。历史是每页签内存大头：
    // 列宽 × 行数 × ~32B/格，120 列时 10000 行 ≈ 38MB，多页签线性翻倍。
    // 2000 行（≈7.7MB/页签）对本工具场景（nvim/lazygit/htop/回看日志）足够；
    // ponytail: 需要更长的历史时，把 scrolling_history 移入 config.json 设置项。
    let term_config = Config {
        kitty_keyboard: true,
        scrolling_history: history_lines.clamp(100, 5000) as usize,
        ..Default::default()
    };
    let term = Term::new(
        term_config,
        &TermSize::new(cols as usize, rows as usize),
        listener,
    );    // 注意：不在 spawn 里调 pair.master.resize(size)。
    // 子进程已通过 pair.slave.spawn_command() 拿到正确尺寸；
    // ConPTY 内部会把该尺寸同步给子进程。
    // 若此处再 resize，会与 show_terminal() 首帧 resize 竞争，
    // 导致 ConPTY 管道状态不一致（输入卡死）。
    // 首帧 resize 由 Session.needs_resize 标记触发，保证只发生一次。

    // 终端能力查询应答（opencode/opentui 启动时会探测终端交互能力：DSR、
    // DECRQM、XTWINOPS 等）。不回会导致 app 判定“非交互终端”，从而不启用
    // 鼠标捕获——滚轮只能滚仿真器自己的滚动缓冲，而这类 TUI 的滚动缓冲里是
    // 整屏重画的原始字节，内容必然错行。按真实终端行为回复即可。
    let reply_tx = writer_tx.clone();
    let theme_dark = Arc::new(AtomicBool::new(true));
    let osc_theme_aware = Arc::new(AtomicBool::new(false));

    // OMP/opencode 等 TUI 启动后若不收到一次外部写入，其输入事件循环
    // 不进入就绪态：按键字节已写入 ConPTY 但 OMP 不回显、不消费，一直
    // 到切主题（发 OSC10/11）后才恢复。此处 spawn 后立即推一次同款 OSC，
    // 让输入从一开始就正常。经验证有效（alt-screen 触发点更晚反而失灵，
    // 必须赶在 OMP 首次渲染前送达）。
    {
        let d = theme_dark.load(Ordering::Relaxed);
        let (fg, bg) = if d { ("ffff", "1616/1616/1a1a") } else { ("0000", "ffff/ffff/ffff") };
        let msg = format!("\x1b]10;rgb:{fg}/{fg}/{fg}\x1b\\\x1b]11;rgb:{bg}\x1b\\").into_bytes();
        let _ = writer_tx.try_send(msg);
    }
    // 快照 + 命令通道
    let snapshot = Arc::new(std::sync::Mutex::new(Arc::new(TermSnapshot {
        cells: Vec::new(),
        row_hashes: Vec::new(),
        offset: 0,
        cursor_point: Point::new(Line(0), Column(0)),
        cursor_shape: CursorShape::Block,
        selection: None,
sel_range: None,
        show_cursor: true,
        colors: Colors::default(),
        mode: TermMode::empty(),
        cursor_cell_char: ' ',
        cursor_cell_flags: alacritty_terminal::term::cell::Flags::empty(),
        snap_gen: 0,
    })));
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TermCommand>();
    let cells_stale = Arc::new(AtomicBool::new(false));

    let output_count = Arc::new(AtomicU32::new(0));
    let out_bytes = Arc::new(AtomicU64::new(0));
    let loading = Arc::new(AtomicBool::new(true));
    // 启动即「已查看」：启动属下的 shell 提示符/横幅不算需要提醒的新内容，
    // 之后每轮新实质输出由 reader 复位成未查看（见读循环 loading 分支）。
    let has_been_viewed = Arc::new(AtomicBool::new(true));
    let now_ts = crate::now_ms();
    let last_output_ms = Arc::new(AtomicU64::new(now_ts));
    let last_input_ms = Arc::new(AtomicU64::new(0));
    let last_scroll_ms = Arc::new(AtomicU64::new(0));
    let last_screen_change_ms = Arc::new(AtomicU64::new(0));
    let last_content_ms = Arc::new(AtomicU64::new(0));
    // 状态快照：首帧由 app.rs 的 refresh_tab_state 填（门限 1s）。
    let state_check_ms = Arc::new(AtomicU64::new(0));
    let state_icon = Arc::new(AtomicU8::new(0));
    let state_done = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    // 读取子进程输出的线程。
    let term = Arc::new(RwLock::new(term));
    let parse_gen = Arc::new(AtomicU64::new(0));
    // TUI 状态（备用屏/隐藏光标）由 reader 每次输出后实时写，供页签图标/
    // latch 判定：后台页签不渲染，terminal.rs 的写入只发生在切到前台时。
    let alt_screen = Arc::new(AtomicBool::new(false));
    let cursor_hidden = Arc::new(AtomicBool::new(true));
    {
        let term = term.clone();
        let theme_dark = theme_dark.clone();
        let reader_exited = exited.clone();
        let osc_theme_aware = osc_theme_aware.clone();
        let output_count = output_count.clone();
        let reader_out_bytes = out_bytes.clone();
        let last_output_ms = last_output_ms.clone();
    let reader_last_screen_change = last_screen_change_ms.clone();
    let reader_last_content = last_content_ms.clone();
        let reader_input_ms = last_input_ms.clone();
        let reader_fg = foreground.clone();
        let reader_loading = loading.clone();
        let reader_viewed = has_been_viewed.clone();
        let reader_alt_screen = alt_screen.clone();
        let reader_cursor_hidden = cursor_hidden.clone();
        let parse_gen = parse_gen.clone();
        let reader_snapshot = snapshot.clone();
        let reader_cells_stale = cells_stale.clone();
        let reader_cmd_rx = cmd_rx;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("获取 PTY 读取句柄失败: {e}"))?;
// 读循环整体包一层 catch_unwind：除解析外的 panic（快照/命令处理/维度计算）
        // 同样会静默带走 reader 线程 → PTY 无人读 + 快照停更 = 画面永久冻结，
        // 子进程随后写管道报错（os error 232）＝“终端被异常终止”。一律收敛成
        // 「会话异常结束」：记 crash.log + 置 exited，页签变可重开态。
        let guard_exited = exited.clone();
        let guard_ctx = reader_ctx.clone();
        std::thread::spawn(move || {
        let reader_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut parser: Processor = Processor::default();
            let mut buf = [0u8; 0x10_000];
            let mut last_counted_ms: u64 = 0;
            // 动画分类累计窗口：一轮连续输出（空闲 >500ms 重置），不看单次
            // read 块大小（块切分抖动是旧分类误判源）。
            let mut class_esc: u64 = 0;
            let mut class_bytes: u64 = 0;
let mut last_chunk_ms: u64 = 0;
            // 上一次的可见格字符（快照同序）：逐块比对得出「屏幕真的变了没」。
            let mut prev_screen: Vec<char> = Vec::new();
            // 跨块拼接缓冲：OMP 等程序的终端查询序列可能被 read() 切分到相邻块，
            // reply_to_queries 逐块扫描会漏掉。保留尾部未完成的转义序列，
            // 下一块拼接后重扫。
            let mut query_leftover: Vec<u8> = Vec::new();
            // 每块输出复用的缓冲（原来每块新分配：merged/清理后的副本/字符快照），
            // rows×cols 级别的小对象循环里反复 malloc 是后台多页签下的主要开销。
            let mut merged: Vec<u8> = Vec::with_capacity(0x10_000);
            let mut stripped: Vec<u8> = Vec::with_capacity(0x10_000);
            let mut now_chars: Vec<char> = Vec::new();

            // 连续 EOF 计数：子进程已退出时管道立刻 EOF，不重试就是每50ms 空转
            // 一条线程到进程结束（每个死页签漏一条）。真 EOF 连着来几次即认定
            // 管道已关，退出读循环；读错（Err）仍按原样退避重试（瞬时读错曾被
            // 当退出置永久 ❌，这条不能碰）。不置 exited：退出判定归 UI 的
            // try_wait，reader 自行宣布退出会和它抢跑。
let mut eof_streak: u32 = 0;
            // 是否至少发布过一次整屏格子：后台页签在首次发布前也必须先给一份，
            // 否则 tab 切回来既无格子也无重建触发点（renderable_content 迭代需 term）。
            let mut published_once = false;
            // VT 解析守护：alacritty 内部 panic（用户实况 crash.log：
            // term/mod.rs:1296 `removal index (is 0) should be < len (is 0)`）
            // 会带走整条 reader 线程 → PTY 无人读（子进程写满管道后自己阻塞）
            // + 快照永不更新（UI 每帧走静止帧重放）＝画面永久冻结、程序看着还活着。
            // 这里把 panic 收敛成「会话异常结束」：置 exited 让页签显示可重开的
            // 结束态，break 后 drop reader 关管道，不静默冻屏。原始 panic 仍由
            // main.rs 全局钩子记进 crash.log。
let reader_writer_alive = writer_alive.clone();
'read: loop {
        // 写入侧已死（管道断）→ 本会话输入永远到不了子进程，等同会话终止：
        // 置 exited 让页签显示可重开的结束态，而不是“看着活着、敲字没反应”。
        if !reader_writer_alive.load(Ordering::Relaxed) {
            crate::log_crash("reader", "PTY 写入管道已断，结束会话读循环");
            reader_exited.store(true, Ordering::Release);
            reader_ctx.request_repaint();
            break 'read;
        }
let read_res = reader.read(&mut buf);
                // EOF 累计：连续三次读到 0 = 读侧已关（子进程退出），退出读循环，
                // 不留一条每 50ms 醒一次、永远读到 EOF 的空转线程。读到数据或
                // 读错即清零（读错走下方退避重试，瞬时读错不得当退出）。
                // 不置 exited：退出判定归 UI 的 try_wait，reader 自行宣布退出会
                // 和它抢跑。
                if eof_should_exit(matches!(read_res, Ok(0)), &mut eof_streak) {
                    break 'read;
                }
                match read_res {
                    Ok(0) | Err(_) => {
                        // 跨块残留一并刷入。
                        if !query_leftover.is_empty() {
                            parse_gen.fetch_add(1, Ordering::Relaxed);
                            let mut t = term.write().unwrap_or_else(|e| e.into_inner());
                            let r = guarded(|| {
                                parser.advance(&mut *t, &query_leftover);
                                reply_to_queries(&t, &query_leftover, theme_dark.load(Ordering::Relaxed))
                            });
                            drop(t);
                            match r {
                                Ok(Some((reply, osc_color))) => {
                                    if osc_color { osc_theme_aware.store(true, Ordering::Relaxed); }
                                    let _ = reply_tx.try_send(reply);
                                }
                                Ok(None) => {}
                                Err(()) => {
crate::log_crash("reader-panic", &format!("VT 解析 panic（残留块），会话中断；尾部字节: {}", escape_for_log(&query_leftover)));
                                    reader_exited.store(true, Ordering::Release);
                                    reader_ctx.request_repaint();
                                    break 'read;
                                }
                            }
                            query_leftover.clear();
                        }
                        // 读失败/EOF 不再直接置 exited：瞬时读错曾被当退出
                        // （永久 ❌），子进程活着时管道异常也误判。退出与否由
                        // UI 线程 try_wait 权威判定（前台每帧、后台每 BG_REAP_MS，
                        // 见 app.rs update_exited）；这里退避重试，exited 置位后
                        // 下一轮退出读循环。
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        if reader_exited.load(Ordering::Acquire) {
                            reader_ctx.request_repaint();
                            break 'read;
                        }
                        continue;
                    },
                    Ok(n) => {
                        parse_gen.fetch_add(1, Ordering::Relaxed);
                        let now_ms = crate::now_ms();
                        if latency_debug() {
                            let since = now_ms.saturating_sub(reader_input_ms.load(Ordering::Relaxed));
                            if since > 0 && since <= 500 {
                                ECHO_SUM_US.fetch_add(since * 1000, Ordering::Relaxed);
                                ECHO_CNT.fetch_add(1, Ordering::Relaxed);
                                ECHO_MAX_MS.fetch_max(since as u32, Ordering::Relaxed);
                                let cnt = ECHO_CNT.load(Ordering::Relaxed);
                                if cnt.is_multiple_of(20) {
                                    eprintln!(
                                        "[latency] echo avg={}ms max={}ms n={cnt}",
                                        ECHO_SUM_US.load(Ordering::Relaxed) / cnt / 1000,
                                        ECHO_MAX_MS.load(Ordering::Relaxed),
                                    );
                                }
                            }
                        }
                        last_output_ms.store(now_ms, Ordering::Relaxed);
                        // 兜底：启动超时后强制退出加载态。TUI 首屏若整块几乎全是
                        // 转义序列（ConPTY 握手/清屏/定位），启发式会把它误判为
                        // 「动画」而永不置 false → 页签 🔄 常驻，即使终端画面早已
                        // 静止。3 秒后无论内容分类如何都结束加载态（now_ts 即
                        // spawn 时刻）。
                        if reader_loading.load(Ordering::Relaxed)
                            && now_ms.saturating_sub(now_ts) > 3000
                        {
                            reader_loading.store(false, Ordering::Relaxed);
                        }
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
                        // 动画分类按「一轮连续输出」累计（空闲 >500ms 重置）：
                        // 旧规则 (ratio>0.5 && n<200) 依赖单次 read 块大小，同一
                        // 程序前块判动画、后块判文本，纯 ANSI 页签被误判成动画 →
                        // out_bytes 不累计 → 永远不亮 ✅/不弹通知。>0.5 覆盖旧的
                        // 0.8 强动画分支。ponytail: 高转义率非动画内容（ANSI 图）
                        // 仍会当动画跳过计数，升级到显式帧头判定才能根治。
                        if now_ms.saturating_sub(last_chunk_ms) > 500 {
                            class_esc = 0;
                            class_bytes = 0;
                        }
                        last_chunk_ms = now_ms;
                        class_esc += esc_bytes as u64;
                        class_bytes += (esc_bytes + printable) as u64;
                        let is_animation = class_bytes == 0
                            || class_esc as f64 / class_bytes as f64 > 0.5;
                        if is_animation {
                            // 纯动画块（转义重绘/spinner/时钟）：刷「画面还在动」
                            // 时间戳。高频来 ⇒ 还在思考/跑命令，完成判定据此否决
                            // （空闲时的光标闪烁稀疏，不会误否决，见 ANIM_BUSY_MS）。
                        } else {
                            // 实质内容块：刷「最近实质内容」时间戳——完成/通知判据
                            // 只认它（周期转义重绘不产生实质内容 → 永不误判完成）。
                            // 累计实质输出字节（非动画块的可打印字节）：
                            // 「任务完成」/「运行结束」通知的过滤判据。
                            reader_out_bytes.fetch_add(printable as u64, Ordering::Relaxed);
// 新一轮实质输出 → 清「已查看」：本轮输出停止 3s 后才允许
                            // 再弹 `任务完成`。done_notified 在离开 ✅ 时复位
                            // （app.rs update_done_states），重复弹窗由同页签
                            // 10s 节流兜底（TOAST_MIN_INTERVAL_MS）。
                            // 加载期内不清：启动即 viewed=true，首块提示符若也复位
                            // 成 false，纯闲置页签会永远「未查看」→ 无人查看也弹
                            // 「任务完成」通知+闪烁（用户报告：没任何输出却弹）。
                            // 加载结束（首次实质输出清 loading）后的输出才复位。
                            if !reader_loading.load(Ordering::Relaxed) {
                                reader_viewed.store(false, Ordering::Relaxed);
                            }
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
                        // 跨块拼接：把上次残留的未完成转义序列拼到本次块前面，
                        // 保证 reply_to_queries 能识别被 read() 切断的查询。
                        merged.clear();
                        merged.extend_from_slice(&query_leftover);
                        merged.extend_from_slice(&buf[..n]);
                        // 提取尾部未完成的转义序列：从最后一个 ESC 开始到块尾
                        // 若该段不含 CSI/OSC 终结字节，则是不完整的，保留到下块。
                        query_leftover.clear();
                        if let Some(last_esc) = merged.iter().rposition(|&b| b == 0x1b) {
                            let tail = &merged[last_esc..];
                            let complete = if tail.len() >= 2 && tail[1] == b'[' {
                                tail[2..].iter().any(|&b| (0x40..=0x7e).contains(&b))
                            } else if tail.len() >= 2 && tail[1] == b']' {
                                tail[2..].contains(&0x07)
                                    || tail.windows(2).any(|w| w[0] == 0x1b && w[1] == b'\\')
                            } else {
                                tail.len() >= 2
                            };
                            if !complete {
                                query_leftover.extend_from_slice(tail);
                            }
                        }
                        // 清理孤儿 CSI-u 残片：kitty 键盘协议回显的 `[13;5u`、
                        // `[57442;1:3u` 等无 ESC 前缀的残片会被 VT parser 当字面文本
                        // 渲染成可见乱码。在喂给 parser 前整段清理。
                        let merged: &[u8] = {
strip_orphan_csi_u_bytes_into(&mut stripped, &merged);
                            &stripped
                        };
                        // ── 分块处理：每次最多 CHUNK_SIZE 字节后释放 term 锁，
                        //    让 UI 线程有机会获取读锁做渲染/响应输入。
                        //    VT parser 内部状态（部分序列缓冲）独立于 term 锁，
                        //    分块不会破坏解析连续性。
                        // ── 分块处理：前台 4KB 块保证回显低延迟，
                        //    后台一次性解析整块数据，降低锁竞争频率。
                        //    前台：4KB 块间隙足够 UI 拿读锁渲染/响应输入。
                        //    后台：不可见，无需频繁释放锁给 UI，一次加锁解析整块。
                        if is_fg {
                            const CHUNK_SIZE: usize = 4096;
                            let mut offset = 0usize;
                            while offset < merged.len() {
                                let end = (offset + CHUNK_SIZE).min(merged.len());
                                let chunk = &merged[offset..end];
                                let reply = {
                                    let mut t = term.write().unwrap_or_else(|e| e.into_inner());
                                let r = guarded(|| {
                                parser.advance(&mut *t, chunk);
                                reply_to_queries(
                                    &t,
                                    chunk,
                                    theme_dark.load(Ordering::Relaxed),
                                )
                            });
                                    match r {
                                        Ok(v) => v,
                                        Err(()) => {
crate::log_crash("reader-panic", &format!("VT 解析 panic，前台页签会话中断；触发字节: {}", escape_for_log(chunk)));
                                            reader_exited.store(true, Ordering::Release);
                                            reader_ctx.request_repaint();
                                            break 'read;
                                        }
                                    }
}; // ← term 锁在此释放
                                if let Some((reply, osc_color)) = reply {
                                    if osc_color {
                                        osc_theme_aware.store(true, Ordering::Relaxed);
                                    }
                                    let _ = reply_tx.try_send(reply);
                                }
                                offset = end;
                            }
                        } else {
                            // 后台会话：一次加锁解析整块数据（不可见，
                            // 无需频繁释放锁给 UI，降低锁竞争频率）。
                            let reply = {
                                let mut t = term.write().unwrap_or_else(|e| e.into_inner());
                                let r = guarded(|| {
                                    parser.advance(&mut *t, merged);
                                    reply_to_queries(
                                        &t,
                                        merged,
                                        theme_dark.load(Ordering::Relaxed),
                                    )
                                });
                                match r {
                                    Ok(v) => v,
                                    Err(()) => {
crate::log_crash("reader-panic", &format!("VT 解析 panic（后台页签），会话中断；触发字节: {}", escape_for_log(merged)));
                                        reader_exited.store(true, Ordering::Release);
                                        break 'read;
                                    }
                                }
                            }; // ← term 锁在此释放
                            if let Some((reply, osc_color)) = reply {
                                if osc_color {
                                    osc_theme_aware.store(true, Ordering::Relaxed);
                                }
                                let _ = reply_tx.try_send(reply);
                            }
                        }
                        // ── 处理 UI 命令（滚动/调整大小/选区）──
                        while let Ok(cmd) = reader_cmd_rx.try_recv() {
                            let mut t = wlock(term.write());
                            match cmd {
                                TermCommand::Scroll(s) => t.scroll_display(s),
                                TermCommand::Resize { cols, rows } => {
                                    t.resize(TermSize::new(cols, rows));
                                }
                                TermCommand::UpdateSelection(sel) => {
                                    t.selection = sel;
                                }
                            }
                        }
                        // ── 生成无锁快照：短暂加锁克隆可见格子 + 元数据，
                        //    然后通过 Arc 原子交换给 UI 线程。UI 线程
                        //    load() 时零锁竞争，彻底消除渲染卡顿。
                        {
                            let t = rlock(term.read());
                            {
                                // 后台页签不重建整屏格子（每块 rows×cols 次Cell 克隆
                                // + 分配，画面根本不可见）：只刷元数据/时间戳并标
                                // cells_stale，UI 切回该页签那帧就地重建。
                                // 只做屏幕变化检测（图标/通知判据）与元数据发布。
                                let publish = is_fg || !published_once;
                                if publish {
                                    let cur_gen = parse_gen.load(Ordering::Relaxed);
                                    let snap = build_snapshot(&t, cur_gen);
                                    reader_alt_screen
                                        .store(snap.mode.contains(TermMode::ALT_SCREEN), Ordering::Relaxed);
                                    reader_cursor_hidden.store(!snap.show_cursor, Ordering::Relaxed);
                                    // ── 屏幕变化检测
                                    // 逐格比字符。变化 ≥MIN_CONTENT_CELLS 格 = 成规模内容
                                    // （命令输出、回答正文）；1~3 格的周期刷新（秒表/时钟/
                                    // spinner 帧）只刷新「画面在动」，不刷新内容时间戳。
                                    // 整屏重画同样的字 = 零变化，一个时间戳都不刷——
                                    // 这正是「明明静止却常亮 🔄、✅ 永远不亮」的成因。
                                    {
                                        now_chars.clear();
                                        now_chars.extend(snap.cells.iter().map(|(_, c)| {
                                            if c.c == '\0' { ' ' } else { c.c }
                                        }));
                                        let diff = if prev_screen.len() == now_chars.len() {
                                            now_chars
                                                .iter()
                                                .zip(&prev_screen)
                                                .filter(|(a, b)| a != b)
                                                .count()
                                        } else {
                                            // 尺寸变了（resize / 换页）：整体按变化算。
                                            now_chars.len()
                                        };
                                        std::mem::swap(&mut prev_screen, &mut now_chars);
                                        if diff > 0 {
                                            reader_last_screen_change.store(now_ms, Ordering::Relaxed);
                                            if diff >= MIN_CONTENT_CELLS {
                                                reader_last_content.store(now_ms, Ordering::Relaxed);
                                            }
                                        }
                                    }
                                    published_once = true;
                                    reader_cells_stale.store(false, Ordering::Relaxed);
                                    *reader_snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(snap);
                                } else {
                                    let content = t.renderable_content();
                                    let offset_val = content.display_offset;
                                    let mode = *t.mode();
                                    let show = mode.contains(TermMode::SHOW_CURSOR);
                                    reader_alt_screen
                                        .store(mode.contains(TermMode::ALT_SCREEN), Ordering::Relaxed);
                                    reader_cursor_hidden.store(!show, Ordering::Relaxed);
                                    // 只比字符，不克隆格子：prev_screen 就地推进。
                                    {
                                        now_chars.clear();
                                        now_chars.extend(content.display_iter.filter_map(|ix| {
                                            // 与 build_snapshot 同一过滤（负行不入快照）：
                                            // 两端口径不一致会让切页时误判整屏变化。
                                            if (ix.point.line.0 + offset_val as i32) < 0 {
                                                return None;
                                            }
                                            Some(if ix.cell.c == '\0' { ' ' } else { ix.cell.c })
                                        }));
                                        let diff = if prev_screen.len() == now_chars.len() {
                                            now_chars
                                                .iter()
                                                .zip(&prev_screen)
                                                .filter(|(a, b)| a != b)
                                                .count()
                                        } else {
                                            // 尺寸变了（resize / 换页）：整体按变化算。
                                            now_chars.len()
                                        };
                                        std::mem::swap(&mut prev_screen, &mut now_chars);
                                        if diff > 0 {
                                            reader_last_screen_change.store(now_ms, Ordering::Relaxed);
                                            if diff >= MIN_CONTENT_CELLS {
                                                reader_last_content.store(now_ms, Ordering::Relaxed);
                                            }
}
                                    }
                                    reader_cells_stale.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                        if is_fg {
                            // 前台会话发合并信号：logic() 每帧消费，按配置帧率唤醒。
                            let _ = redraw_tx.send(());
                        } else {
                            // 后台页签攒批：攒满阈值或超时才处理，降低加锁频率。
                            // 分块已在上方完成，此处仅控制处理时机。
                            // （后台路径上面的 while 循环已直接处理完 data，
                            //    这里不再额外 batch——后台输出量通常较小，
                            //    分块本身就足够轻量。）
                        }
}
                }
            }
        }));
        if reader_result.is_err() {
            crate::log_crash("reader-panic", "读循环 panic（解析/快照/命令），会话已标记异常结束");
            guard_exited.store(true, Ordering::Release);
guard_ctx.request_repaint();
        }
    });
    }

    let session = Session {
        title: title.to_string(),
        dir: dir.to_string(),
        cmd: tui_command.to_string(),
        term,
        snapshot: snapshot.clone(),
        cmd_tx: cmd_tx.clone(),
        writer: writer_tx,
        master: Some(pair.master),
        child: Some(child),
        grid_size: (cols, rows),
        needs_resize: true,
        theme_dark,
        osc_theme_aware,
        foreground,
        redraw_rx,
        exited: exited.clone(),
        notified: Arc::new(AtomicBool::new(false)),
        started_ms: Arc::new(AtomicU64::new(now_ts)),
        done_notified: Arc::new(AtomicBool::new(false)),
        last_toast_ms: Arc::new(AtomicU64::new(0)),
        last_reap_ms: Arc::new(AtomicU64::new(0)),
        done_since_ms: Arc::new(AtomicU64::new(0)),
        last_clipboard_seq: None,
        output_count,
        out_bytes,
        last_output_ms,
        has_been_viewed,
        alt_screen,
        cursor_hidden,
        parse_gen: parse_gen.clone(),
        cells_stale,
        caret_scan: None,
        gpu: None,
        last_input_ms,
        last_scroll_ms,
        last_screen_change_ms,
        last_content_ms,
        state_check_ms,
        state_icon,
        state_done,
        drag_press_pos: None,
        click_press_pos: None,
        mouse_press_pending: None,
        mouse_gesture_sel: false,
        last_preedit: String::new(),
        cached_ansi_rgb: None,
        cached_color_lut: None,
        cached_metrics: None,
cached_render_shapes: None,
        row_cache: Vec::new(),
        row_cache_sig: 0,
        last_snapshot_gen: 0,
        last_snapshot_offset: 0,
        loading,
    };
    Ok(session)
}
impl Session {
    /// 页签「加载中」判定：loading 标志 + 墙钟双条件。reader 只在「有输出」时
    /// 才自清 loading（首个非动画块 / 3s 兜底），零输出会话（sleep、静默命令）
    /// 会常驻 true → 墙钟兜底，超时后不再显示加载态，杜绝「🔄常驻但没在跑」。
    /// 页签图标 / 终端区启动占位 / 帧率调度三处统一走这里，口径一致。
    pub fn loading_active(&self, now_ms: u64) -> bool {
        self.loading.load(Ordering::Relaxed)
            && now_ms.saturating_sub(self.started_ms.load(Ordering::Relaxed)) < LOADING_MAX_MS
    }

    /// 将 child 和 master 都移到后台线程异步清理，避免 Child::drop / MasterPty::drop
    /// 在 UI 线程阻塞（Windows 上调用 WaitForSingleObject 等待进程退出，
    /// 100-500ms 冻结 UI）。
    /// 调用方在 tabs.remove() 之前调用：Session::drop 时 child=None + master=None，
    /// 零阻塞。
    pub fn kill_in_background(&mut self) {
        // 程序化终止不弹「运行结束」提醒（首次启动的会话此刻未必被 update_exited 处理过）。
        self.notified.store(true, Ordering::Relaxed);
        let child = self.child.take();
        let master = self.master.take();
        if child.is_some() || master.is_some() {
            std::thread::spawn(move || {
                if let Some(mut c) = child {
                    let _ = c.kill();
                }
                // master 在此 drop：PTY 伪控制台在此释放，
                // reader 线程感知到管道断裂后自然退出。
                drop(master);
            });
        }
    }
}

/// UI 线程滚动后立即刷新快照：reader 线程在无 PTY 输出时不会生成新快照，
/// 不更新的话下一帧渲染仍用旧 offset，滚动无可见效果。
///
/// **全程不阻塞 UI**（fix: 滚轮拖死窗口）：原来 `term.read()` 是阻塞式，
/// reader 每解析一块 PTY 输出就抢一次写锁，于是滚动（每帧必跑）和
/// 持续输出的会话正面撞车，UI 线程排队等锁 → 窗口未响应。这里改 `try_read`：
/// 抢不到就整帧跳过，`last_snapshot_offset` 保持陈旧 → 下一帧 `offset_changed`
/// 仍为真会重试；reader 解析完本身也会 publish 一份带新 offset 的快照。
/// 另：快照自带它真实的 parse_gen，用 ambient cur_gen 会在
/// 「gen 已推进、reader 尚未发布」的瞬间把旧快照标成最新，
/// 下一帧 terminal.rs 的静止帧快路径就永久钉住旧画面。
pub fn refresh_snapshot(sess: &mut Session) {
let cur_gen = sess.parse_gen.load(Ordering::Relaxed);
    // 抢不到（reader 正在解析）→ 整帧跳过，下一帧 offset_changed 仍为真会重试。
    let Some(t) = try_rlock(&sess.term) else { return };

    // 从终端 grid 直接读 display_offset：旧 snapshot 的 offset 未随滚动更新，
    // 用它检测不到纯滚动变化。
    let cur_offset = t.grid().display_offset();
    let gen_changed = cur_gen != sess.last_snapshot_gen;
    let offset_changed = cur_offset != sess.last_snapshot_offset;
    let stale = sess.cells_stale.load(Ordering::Relaxed);
    if !gen_changed && !offset_changed && !stale {
        return;
    }

    // 任何变化（gen 或 offset）都需从 term grid 重建 cells：
    // - gen 变化：新 PTY 输出改变了内容
    // - offset 变化：滚动改变了可见区域
    // - cells_stale：后台页签期间 reader 跳过了整屏克隆，切回前台就地补一帧
    //
    // 单份数据进 Arc 快照：UI 渲染期持 Arc，零额外副本（旧 AtomicPtr 方案
    // 这里同时持有 snapshot_scratch + snapshot.cells 两份全屏格）。
    let snap = build_snapshot(&t, cur_gen);
    let offset_val = snap.offset;
    drop(t);
    *sess.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(snap);
    sess.cells_stale.store(false, Ordering::Relaxed);
    sess.last_snapshot_gen = cur_gen;
    sess.last_snapshot_offset = offset_val;
sess.cached_render_shapes = None;
}

#[cfg(test)]
mod tests {
use super::*;
use alacritty_terminal::term::cell::Flags;

    /// 卡死回归（出现过多次）：VT 解析 panic 会带走整条 reader 线程 →
    /// PTY 无人读 + 快照永不更新 → 画面永久冻结、进程还活着。
    /// 三条约定：`guarded` 拦下 panic；`wlock/rlock` 中毒后照旧可用
    /// （否则快照同样停更 = 冻屏换个姿势复现）。
/// reader 退出的判定：子进程退出后管道一直 EOF，原实现每 50ms 空转一次
    /// 到进程结束（每个死页签漏一条线程）。约定：
    /// 1) 连续 EOF 达阈值 → 退出读循环；
    /// 2) 任何一次非 EOF（有数据 / 读错）→ 计数清零重来（瞬时读错不是退出）。
#[test]
    fn eof_streak_exits_reader() {
        let mut s = 0;
        assert!(!eof_should_exit(true, &mut s));
        assert!(!eof_should_exit(true, &mut s));
        assert!(eof_should_exit(true, &mut s), "连续 EOF 达阈值应退出");

        // 读错/有数据打断后重新计数，不会因历史 EOF 提前退出。
        let mut s = 0;
        assert!(!eof_should_exit(true, &mut s));
        assert!(!eof_should_exit(false, &mut s));
        assert!(!eof_should_exit(true, &mut s));
        assert!(!eof_should_exit(true, &mut s));
        assert!(eof_should_exit(true, &mut s));
    }

/// `try_wlock`/`try_rlock` 两条约定（roll: UI 线程不再阻塞取锁）：
    /// 1) 锁被占 → None，**不得阻塞**（等锁=滚轮卡死那条路）；
    /// 2) 锁中毒 → 照旧拿（into_inner）；否则解析 panic 毒过锁后
    ///    UI 侧永远拿不到，快照停更 = 画面永久冻在最后一帧。
    #[test]
    fn try_locks_never_block_and_survive_poison() {
        use std::sync::{Arc, RwLock};
        let m = Arc::new(RwLock::new(7u32));

        // 空闲：拿到。
        assert_eq!(*try_wlock(&m).unwrap(), 7);
        assert_eq!(*try_rlock(&m).unwrap(), 7);

        // 被占：无锁 → None，不阻塞。
        let w = m.write().unwrap();
        assert!(try_wlock(&m).is_none(), "锁被占时不得等待");
        assert!(try_rlock(&m).is_none());
        drop(w);

// 中毒：仍拿得到。
        let m2 = {
            let m2 = Arc::new(RwLock::new(1u32));
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
                let g = m2.write().unwrap();
                move || panic!("毒化")
            }));
            m2
        };
        assert!(try_wlock(&m2).is_some(), "中毒锁必须照旧可用");
        assert!(try_rlock(&m2).is_some());
    }

    #[test]
    fn parse_panic_is_contained_and_term_stays_usable() {
        let listener = SessionListener {
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let term = std::sync::Arc::new(std::sync::RwLock::new(Term::new(
            Config::default(),
            &TermSize::new(8, 3),
            listener,
        )));

        // 1) panic 被收敛成 Err（调用方据此置 exited、退出读循环）
        let r: Result<(), ()> = guarded(|| {
            let _t = wlock(term.write());
            panic!("模拟 alacritty 解析 panic");
        });
        assert!(r.is_err(), "guarded 必须拦下 panic");

        // 2) 锁已中毒但仍可取用：快照照常刷新，不会二次冻屏
        {
            let mut t = wlock(term.write());
            let mut p: Processor = Default::default();
            p.advance(&mut *t, b"hi");
        }
        let snap = build_snapshot(&rlock(term.read()), 1);
        assert!(snap.cells.iter().any(|(_, c)| c.c == 'i'));
    }

/// 行级增量渲染的前置假设：快照 cells 必须按 vline（point.line.0 + offset）
    /// 升序排列，且 vline 0 就是首行。terminal.rs 按行切分区间的 while 循环
    /// 只靠这个序，序乱 = 整屏错位。
    #[test]
    fn snapshot_cells_are_vline_ascending() {
        let listener = SessionListener {
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let mut term = Term::new(Config::default(), &TermSize::new(4, 3), listener);
        let mut p: Processor = Default::default();
        for (i, row) in ["AAA", "BBB", "CCC"].iter().enumerate() {
            let mut bytes = row.as_bytes().to_vec();
            if i < 2 {
                bytes.extend_from_slice(b"\r\n");
            }
            p.advance(&mut term, &bytes);
        }
        let snap = build_snapshot(&term, 7);
        let mut prev = i32::MIN;
        for (pt, _) in &snap.cells {
            let v = pt.line.0 + snap.offset as i32;
            assert!(v >= prev, "cells 行序降序/乱序: {prev} -> {v}");
            prev = v;
        }
        let mut heads: std::collections::BTreeMap<i32, char> = std::collections::BTreeMap::new();
        for (pt, c) in &snap.cells {
            if pt.column.0 == 0 {
                heads.insert(pt.line.0 + snap.offset as i32, c.c);
            }
        }
        let got: Vec<(i32, char)> = heads.iter().map(|(k, ch)| (*k, *ch)).collect();
assert_eq!(got, vec![(0, 'A'), (1, 'B'), (2, 'C')], "vline0 必须是首行");
    }

    /// 快照自带的行指纹：渲染端按它判行级增量（不再自己扫全屏格）。守卫性质：
    /// 内容不变 → 指纹逐行不变；改一格 → 只影响那一行。
    #[test]
    fn snapshot_row_hashes_track_content() {
        fn mk() -> Term<SessionListener> {
            let listener = SessionListener {
                redraw: std::sync::mpsc::sync_channel(1).0,
                ctx: eframe::egui::Context::default(),
                foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            };
            let mut term = Term::new(Config::default(), &TermSize::new(4, 3), listener);
            let mut p: Processor = Default::default();
            for row in ["AAA", "BBB", "CCC"] {
                p.advance(&mut term, row.as_bytes());
                p.advance(&mut term, b"\r\n");
            }
            term
        }
        let a = build_snapshot(&mk(), 1);
        assert_eq!(a.row_hashes.len(), 3, "行数应等于可视行数");
        let b = build_snapshot(&mk(), 2);
        assert_eq!(a.row_hashes, b.row_hashes, "内容相同 → 指纹相同");
        let mut c = mk();
        let mut p: Processor = Default::default();
        p.advance(&mut c, b"\x1b[2;2HX"); // 只改第 2 行第 2 列
        let s = build_snapshot(&c, 3);
        assert_eq!(s.row_hashes[0], a.row_hashes[0], "未变行不应被牵连");
        assert_ne!(s.row_hashes[1], a.row_hashes[1], "改动行指纹必须变");
        assert_eq!(s.row_hashes[2], a.row_hashes[2], "未变行不应被牵连");
    }

    /// DSR/DA 必须应答：本机实测（裸 ConPTY + cmd.exe，捆绑 conpty 1.25 与系统
    /// 内置行为一致）ConPTY 不替子进程应答这两条，cmd 发出 `ESC[6n ESC[c` 后
    /// 停在初始化等应答——不打横幅、不出提示符、按键无回显、拖选/Ctrl+C/V 全废。
    #[test]
    fn reply_to_queries_answers_dsr_and_da() {
        let listener = SessionListener {
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let term = Term::new(Config::default(), &TermSize::new(80, 24), listener);
        assert!(reply_to_queries(&term, b"\x1b[6n", true).is_some(), "DSR 必须应答");
        let r = reply_to_queries(&term, b"\x1b[6n\x1b[c", true).expect("DSR+DA 必须应答").0;
        let s = String::from_utf8_lossy(&r);
        assert!(s.contains("\x1b[1;1R"), "CPR 回 1;1: {s:?}");
        assert!(s.contains("\x1b[?62;"), "DA 回 VT220: {s:?}");
        // DECXCPR 私有形态同样应答。
        let r2 = reply_to_queries(&term, b"\x1b[?6n", true).unwrap().0;
        let s2 = String::from_utf8_lossy(&r2);
        assert!(s2.starts_with("\x1b[?1;1;"), "DECXCPR: {s2:?}");
    }

    #[test]
    fn reply_to_queries_standard() {
        let listener = SessionListener {
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let term = Term::new(Config::default(), &TermSize::new(80, 24), listener);
// DA 不应答（ConPTY 自己应答）；DSR/DECRQM/kitty/像素尺寸/OSC 颜色照常应答。
        let bytes = b"\x1b[6n\x1b[?2026$p\x1b[?1000$p\x1b[?u\x1b[14t\x1b]11;?\x1b\\";
        let r = reply_to_queries(&term, bytes, true).unwrap().0;
        let s = String::from_utf8_lossy(&r);
        assert!(s.contains("\x1b[1;1R"), "DSR CPR: {s}");
        assert!(s.contains("\x1b[?2026;1$y"), "DECRQM 2026: {s}");
        assert!(s.contains("\x1b[?1000;1$y"), "DECRQM 1000: {s}");
        assert!(s.contains("\x1b[?u"), "kitty 键盘: {s}");
        assert!(s.contains("\x1b[4;0;0t"), "XTWINOPS: {s}");
        assert!(s.contains("\x1b]11;rgb:16161a/16161a/16161a"), "OSC11 深色底: {s}");
        // XTVERSION 不应答（无对端消费的查询）。
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

    /// 回归：滚动历史上限必须生效——默认 scrollback 10000 行时每页签
    /// 占用 ≈列宽×10000×32B（120 列 ≈ 38MB），页签开多内存线性爆炸。
    /// 收紧后历史行数 ≤ scrolling_history，总占用降至 ~7.7MB/页签。
    #[test]
    fn scrollback_history_bounded() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::term::Config as TermConfig;
        let cfg = TermConfig { scrolling_history: 1000, ..Default::default() };
        let mut term = Term::new(cfg, &TermSize::new(120, 40), VoidListener);
        let mut p: alacritty_terminal::vte::ansi::Processor = Default::default();
// 灌入远超历史的输出：120 列 × 12000 行（每行 120 个「A」+ 换行）。
        let mut line = vec![b'A'; 120];
        line.push(b'\r');
        line.push(b'\n');
        for _ in 0..12000 {
            p.advance(&mut term, &line);
        }
        // 历史行数 = 总行数 − 屏高，必须被 scrolling_history 钳住。
        let hist = term.grid().total_lines() - 40;
        assert!(hist <= 1000, "历史行数未受 scrolling_history 限制: {hist}");
        assert!(hist >= 900, "历史应钳到接近上限（{hist}/1000）");
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
    #[test]
    fn sanitize_strips_bidi_and_nul() {
        let s = "\u{202a}D:\\tools\\app.exe\u{0} --flag";
        assert_eq!(sanitize(s), "D:\\tools\\app.exe --flag");
        assert_eq!(split_command(s), vec!["D:\\tools\\app.exe", "--flag"]);
        assert_eq!(sanitize("D:\\projects\\PG数据库性能测试\u{202e}"), "D:\\projects\\PG数据库性能测试");
    }

    /// 孤儿 CSI-u 残片替换为单个空格（与 strip_ansi 同规则，字节级）。
    #[test]
    fn strip_orphan_csi_u_bytes_works() {
        // 用户实况：多个连续残片 → 单个空格。
        assert_eq!(
            strip_orphan_csi_u_bytes(b"[13;5u[57442;1:3u[13;5u[57442;1:3u"),
            b" "
        );
        // 前后有正常文本：残片位置补空格断词。
        assert_eq!(strip_orphan_csi_u_bytes(b"a[13;5ub"), b"a b");
        assert_eq!(strip_orphan_csi_u_bytes(b"x[57442;1:3uy"), b"x y");
        // 末尾截断残片丢弃。
        assert_eq!(strip_orphan_csi_u_bytes(b"ok[123;45"), b"ok ");
        // 残片后已有空格不重复补。
        assert_eq!(strip_orphan_csi_u_bytes(b"T[13;5u X"), b"T X");
        // 真正的 ESC 转义序列原样保留。
        let s = b"\x1b[31mred\x1b[0m[13;5u";
        assert_eq!(strip_orphan_csi_u_bytes(s), b"\x1b[31mred\x1b[0m ");
        // 普通文本不受影响。
        assert_eq!(strip_orphan_csi_u_bytes(b"arr[0] = [1, 2]"), b"arr[0] = [1, 2]");
        assert_eq!(strip_orphan_csi_u_bytes(b"[abc]"), b"[abc]");
        assert_eq!(strip_orphan_csi_u_bytes(b"[123"), b"[123");
        // 冒号分隔但无 u 结尾且后接其它字符 → 保留。
        assert_eq!(strip_orphan_csi_u_bytes(b"a[1:2b"), b"a[1:2b");
    }

    /// 上游 bug（已在 vendor 版修复）回归：不经 strip 的 push 洪水在第 4096 次
    /// push 曾触发 alacritty panic（title_stack 为空 → remove(0) 越界），即
    /// crash.log 里那串 panic 引发的一连串「终端被异常终止」。vendor 修完后
    /// 同样的洪水不能再 panic。
    #[test]
    fn unfiltered_kitty_push_flood_no_longer_panics() {
        let old = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // 测试内不要污染 crash.log
        let listener = SessionListener {
            redraw: std::sync::mpsc::sync_channel(1).0,
            ctx: eframe::egui::Context::default(),
            foreground: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let cfg = Config { kitty_keyboard: true, ..Default::default() };
        let mut term = Term::new(cfg, &TermSize::new(80, 24), listener);
        let mut parser: Processor = Default::default();
        let push = b"\x1b[>1u";
        let mut fed = 0;
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..5000 {
                fed += push.len();
                parser.advance(&mut term, push);
            }
        }))
        .is_err();
        std::panic::set_hook(old);
        assert!(!panicked, "vendor 已修复 push 洪水 panic；若再次 panic 说明 vendor patch 被破坏");
        assert!(fed <= 5000 * push.len());
    }

}
