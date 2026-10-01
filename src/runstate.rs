//! 页签「任务运行态」的抽象接口。
//!
//! # 为什么要有这层
//! 页签图标（🔄/✅/空）原先只有一个判据：**终端最近 3s 内有没有输出**。那是
//! **保底启发式**，对普通 shell 命令够用，对 pi / opencode 这类 agent 不够：
//! 模型思考、请求 API、跑长工具时终端可以十几秒不出一个字节，页签就误判成
//! 「完成」，弹「任务完成」通知；反过来 pi 静止等输入时界面动画/光标重绘又
//! 会一直有输出，页签常亮 🔄。两种都错。
//!
//! agent 自己知道准确状态，而且都有**外部可读**的接口，不需要我们逆向终端：
//!
//! - **pi / oh-my-pi**：会话 JSONL `~/.pi/agent/sessions/<项目slug>/<会话>.jsonl`，
//!   追尾最后一条 message 记录。
//! - **opencode**：库 `~/.local/share/opencode/opencode.db`，经它自带的
//!   `opencode db "SQL" --format json` CLI 读（不必自己解 SQLite）。
//!
//! 两者都是**只读旁路**：读不到、读坏了、工具没装 → 一律返回
//! [`RunState::Unknown`]，调用方（`app.rs` 的 `tab_icon` / `update_done_states`）
//! 原样回退到 3s 输出启发式。**任何时候都不因为这层读不到而丢状态**。
//!
//! # 用法
//! `session::spawn` 末尾调 [`start_tracking`]：命中已知 agent 就起一个后台
//! 线程，把状态写进 `Session::run_state`（`Arc<AtomicU8>`，0=Unknown）；
//! 会话退出（`exited` 置位）线程自行结束。UI 侧只读那个原子量，永不加锁等待，
//! 所以这层**不影响帧率**（opencode 一次 `db` 查询约 1s，跑在自己的线程 + 全局
//! 缓存里，同一时刻最多一条查询）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 权威运行态。`Unknown` = 本源不适用/读不到 → 调用方回退输出启发式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunState {
    /// 工具没装/没识别/文件读不到：必须回退启发式。
    Unknown = 0,
    /// 正在干活（模型生成中、工具执行中、回合未结束）。
    Busy = 1,
    /// 回合已结束，轮到用户输入。
    Idle = 2,
}

impl RunState {
    /// 从 `Session::run_state` 里的 u8 还原；0 → Unknown（触发回退）。
    pub fn from_slot(v: u8) -> RunState {
        match v {
            1 => RunState::Busy,
            2 => RunState::Idle,
            _ => RunState::Unknown,
        }
    }

    /// 存进 `Session::run_state`。
    pub fn to_slot(self) -> u8 {
        self as u8
    }
}

/// 本页签跑的是哪种 agent（都不认识 = 不跟踪，走启发式）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AgentKind {
    /// pi / oh-my-pi / omp（同一套会话 JSONL）。
    Pi,
    /// opencode（TUI 落 SQLite，经其 `opencode db` CLI 读）。
    Opencode,
}

/// 轮询节奏：pi 只是 stat + 读文件尾 64KB；opencode 命中全局缓存（见 `oc_state`）。
const POLL_MS: u64 = 1_200;
/// opencode 查询结果缓存时长。`opencode db` 冷启约 1s，多个 opencode 页签各查
/// 一次会把 CPU 顶满；缓存期内同目录直接复用，锁内串行 → 全局同时只有一条查询。
const OC_CACHE_MS: u64 = 2_500;
/// opencode 可执行文件路径的缓存时长（配置读/遍历目录不算便宜，但也不必每次
/// 查询都做；装完新工具最多一分钟认得）。
const EXE_CACHE_MS: u64 = 60_000;
/// 读会话文件尾多少字节。末行可能被写了一半，最多也就一行（几 KB）。
const TAIL_BYTES: u64 = 64 * 1024;

/// [`PI_BUSY_FRESH_MS`]：Busy 判定的时效闸门。会话文件超过这么久没被写过，
/// 就不认为这一轮还在跑（残留的 toolUse/toolResult 尾记录而已）。
/// 依据：19189 段真实 Busy 的持续时长 p99.9 = 238s，300s 留足余量。
const PI_BUSY_FRESH_MS: u64 = 300 * 1000;

// ── agent 识别 ────────────────────────────────────────────────────────────

/// 从启动命令里认出 agent。取**每个 token 的文件名部分**做全等匹配：
/// 不用 `contains("pi")`——`D:\agent\pi\pi.exe`、`clip` 之类都会误命中。
/// `cd D:\x && pi` 这种复合命令也能中（扫所有 token）。
fn detect_kind(cmd: &str) -> Option<AgentKind> {
    for tok in cmd.split_whitespace() {
        let tok = tok.trim_matches(|c| c == '"' || c == '\'');
        let base = tok
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(tok)
            .to_ascii_lowercase();
        let base = base
            .strip_suffix(".exe")
            .or_else(|| base.strip_suffix(".cmd"))
            .or_else(|| base.strip_suffix(".bat"))
            .unwrap_or(&base)
            .to_string();
        match base.as_str() {
            "pi" | "omp" | "oh-my-pi" | "pi-tui" => return Some(AgentKind::Pi),
            "opencode" => return Some(AgentKind::Opencode),
            _ => {}
        }
    }
    None
}

// ── pi：会话 JSONL 追尾 ──────────────────────────────────────────────────

/// 启动命令里是否含已知 agent（pi / opencode…）。
///
/// 供 UI 区分「Unknown 是因为这不是 agent」与「Unknown 是 agent 状态读不出来」：
/// 后者不能拿输出窗口判运行中——全屏 TUI 空闲时也一直在刷光标/动画，
/// 3s 输出窗口会被永远顶满 → 终端一个字节新内容都没有，页签却常亮 🔄
/// （用户报告「pi 没有任何输出但显示的是刷新」）。
/// 未装 / 没识别 / 读失败仍返回 Unknown，只是 UI 额外知道了「这是个 agent」。
pub fn is_agent_cmd(cmd: &str) -> bool {
    detect_kind(cmd).is_some()
}

/// pi 的项目目录 slug：`D:\AI\TUIProjectManager` → `--D--AI-TUIProjectManager--`
/// （`:` 和 `\` 各换成 `-`，两端再各包一层 `-`，即 `--` + 内层 + `--`）。
fn pi_project_slug(dir: &str) -> String {
    let inner: String = dir
        .chars()
        .map(|c| match c {
            ':' | '\\' | '/' => '-',
            c => c,
        })
        .collect();
    format!("--{inner}--")
}

/// 用户主目录（`%USERPROFILE%` → `HOME` → 兜底 `.`）。
fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// pi 会话根目录：`~/.pi/agent/sessions`。
fn pi_sessions_root() -> PathBuf {
    home_dir().join(".pi").join("agent").join("sessions")
}

/// 路径等价比较：统一 `/`→`\`、去尾部斜杠、Windows 下大小写不敏感。
///
/// 必须做：官方的关联字段（pi 会话头 `cwd`、opencode `session.directory`）
/// 与页签启动目录**不一定逐字相同**——正斜杠/反斜杠、结尾斜杠、大小写都可能差。
/// 直接字符串相等会把「能读到数据」判成「没这个会话」，静默退回启发式。
fn path_eq(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        let s = s.replace('/', "\\");
        let s = s.trim_end_matches('\\').to_string();
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    }
    norm(a) == norm(b)
}

/// pi 会话文件的第一条记录就是 `{"type":"session","cwd":…,"id":…}`——**官方
/// 给出的「这个会话属于哪个项目」**，比目录名反推的 slug 可靠。只读文件头
/// 2KB 就够（那条记录很短）。
fn pi_session_cwd(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 2048];
    let n = f.read(&mut head).ok()?;
    let text = String::from_utf8_lossy(&head[..n]);
    let first = text.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(first).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("session") {
        return None;
    }
    v.get("cwd")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
}

/// 挑出属于该 cwd 的会话文件：候选按 mtime 从新到旧，**逐个用官方 `cwd` 字段
/// 校验**，第一个对上的就是它。
///
/// 目录 slug 只是索引（照本机目录名反推的，pi 改了 slug 就失效），所以 slug
/// 目录不存在/对不上时退回「扫全部项目目录」；两种路径最终都以会话头里的
/// `cwd` 为准，不会认到别的项目的会话去。候选数封顶 [`PI_SCAN_LIMIT`]：
/// 目录里堆了几百个旧会话时不必全扫。
const PI_SCAN_LIMIT: usize = 40;

fn pi_session_file(dir: &str) -> Option<PathBuf> {
    let root = pi_sessions_root();
    let mut dirs = vec![root.join(pi_project_slug(dir))];
    if !dirs[0].is_dir() {
        dirs.clear();
        let Ok(rd) = std::fs::read_dir(&root) else {
            return None;
        };
        dirs.extend(
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir()),
        );
    }
    let mut cands: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let Ok(t) = md.modified() else { continue };
            cands.push((t, p));
        }
    }
    // 候选取最新的在前（mtime 降序），再逐个用官方 cwd 校验。
    cands.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    cands
        .into_iter()
        .take(PI_SCAN_LIMIT)
        .map(|(_, p)| p)
        .find(|p| pi_session_cwd(p).is_some_and(|c| path_eq(&c, dir)))
}

/// pi 一条 `{"type":"message",...}` 记录 → 运行态。
///
/// 判定规则（对着本机真实会话文件归纳）：
/// - `assistant` + `stopReason == "toolUse"` → **Busy**：工具调用已发出、结果
///   还没写回，工具正在跑。
/// - `toolResult` → **Busy**：这一轮还没收尾，assistant 紧接着还要生成下一段。
/// - `assistant` + 其它 `stopReason`（`stop`/无）→ **Idle**：回合完结，等用户。
/// - `user` → **Idle**：轮到用户了。
/// - 非 message 记录（`custom`/`session`/`model_change`…）→ 跳过，看更早一条。
pub fn pi_state_from_record(line: &str) -> Option<RunState> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("message") {
        return None;
    }
    let m = v.get("message")?;
    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
    match role {
        "toolResult" => Some(RunState::Busy),
        "assistant" => {
            let stop = m.get("stopReason").and_then(|s| s.as_str()).unwrap_or("");
            match stop {
                "toolUse" | "tool_call" | "toolCalls" => Some(RunState::Busy),
                // `aborted` = 用户按 Esc 打断 → 本轮已结束，必须判 Idle。
                // 若在这里返回 None，pi_state_from_tail 会越过它去看更早的
                // `assistant/toolUse`，把一个早就收工的会话钉成 Busy → 页签
                // 永远 🔄（实测末条为 aborted 的会话有 3 个）。
                "stop" | "end_turn" | "finished" | "complete" | "max_tokens" | "length" | "error" | "aborted" => {
                    Some(RunState::Idle)
                }
                // 未知/空：不能确定是否结束 → 不判定，往前找更早一条确定的状态。
                // （实测 694 个会话无一落到此分支，纯属版本前向兼容兜底。）
                _ => None,
            }
        }
        "user" => Some(RunState::Idle),
        _ => None,
    }
}

/// 读会话文件尾部若干条记录（从后往前），第一条能判定出状态的即返回。
/// 末行可能写了一半（JSONL 追加非原子）→ 解析失败就跳过它看上一条。
fn pi_state_from_tail(text: &str) -> Option<RunState> {
    for line in text.lines().rev() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(st) = pi_state_from_record(t) {
            return Some(st);
        }
        // 非 message 记录（custom 等）合法，继续往前找。
        if serde_json::from_str::<serde_json::Value>(t).is_err() {
            continue;
        }
    }
    None
}

fn pi_state(dir: &str) -> Option<RunState> {
    let path = pi_session_file(dir)?;
    let meta = std::fs::metadata(&path).ok()?;
    let len = meta.len();
    let take = TAIL_BYTES.min(len.max(1));
    let mut f = std::fs::File::open(&path).ok()?;
    use std::io::{Read, Seek};
    f.seek(std::io::SeekFrom::Start(len - take)).ok()?;
    let mut buf = Vec::with_capacity(take as usize);
    f.read_to_end(&mut buf).ok()?;
    let st = pi_state_from_tail(&String::from_utf8_lossy(&buf))?;
    // 时效闸门：Busy 必须「此刻仍在发生」才作数。pi 只在一轮起止时落盘会话
    // 文件，工具运行期间文件不动；实测 19189 段 Busy 里 p99.9 = 238s、
    // p100 = 1683s，故 300s 内仍可能是真在跑（长 bash / 编译）。
    // 超过 PI_BUSY_FRESH_MS 文件一动不动 = 这一轮其实早已收工（用户 Ctrl+C、
    // 关窗、崩溃），尾记录只是没被收尾的残留。此时还报 Busy，页签会在零输出
    // 情况下永远 🔄——实测末条 Busy 的 63 个会话里 62 个静默超 120s，多半是
    // 几天前的死会话。降级成 Idle，把判定交回输出窗口/✅。
    if st == RunState::Busy {
        let fresh = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| crate::now_ms().saturating_sub(d.as_millis() as u64) <= PI_BUSY_FRESH_MS)
            .unwrap_or(false);
        if !fresh {
            return Some(RunState::Idle);
        }
    }
    Some(st)
}

// ── opencode：SQLite（经其 `opencode db` CLI） ────────────────────────────

/// 找到的 opencode 可执行（None = 确认没装）；旁边是找到它的时刻。
type ExeCache = Option<(Option<PathBuf>, u64)>;

/// 缓存命中判定：**必须先有记录**（`None` = 从没找过）。不能拿「(None, 0)」
/// 当初值——`now_ms()` 首次调用返回 0，那样会命中「新鲜」而永远返回 None。
/// 命中时返回记住的值（可能是 None = 确认没有 opencode）。
fn cache_hit(cache: &ExeCache, now: u64) -> Option<Option<PathBuf>> {
    let (p, at) = cache.as_ref()?;
    (now.saturating_sub(*at) < EXE_CACHE_MS).then(|| p.clone())
}

/// opencode 可执行文件：先找本 exe 同级目录（更新器装哪儿就找哪儿），
/// 再走**与检查更新/一键安装同款**的查找（设置页配的工具路径 → PATH，
/// app::find_opencode_exe），最后才自己扫 PATH。结果缓存 60s：工具装完后
/// 最多一分钟认得，不必每次查询都重读配置。
fn opencode_exe() -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<ExeCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let now = crate::now_ms();
    if let Ok(g) = cache.lock()
        && let Some(hit) = cache_hit(&g, now)
    {
        return hit;
    }
    let name = if cfg!(windows) { "opencode.exe" } else { "opencode" };
    let mut found = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file());
    if found.is_none() {
        found = crate::app::find_opencode_exe();
    }
    if found.is_none()
        && let Some(path) = std::env::var_os("PATH")
    {
        found = std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file());
    }
    if let Ok(mut g) = cache.lock() {
        *g = Some((found.clone(), now));
    }
    found
}

/// 全局 opencode 缓存：目录 → (状态, 查询时刻)。锁内串行查询，保证同一时刻
/// 只有一条 `opencode db` 在跑（它自己冷启就要 ~1s）。
struct OcCache {
    map: HashMap<String, (Option<RunState>, u64)>,
}

fn oc_cache() -> &'static Mutex<OcCache> {
    static C: OnceLock<Mutex<OcCache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(OcCache { map: HashMap::new() }))
}

/// 该目录下 opencode 的运行态。表结构（`sqlite_master` 实测）：
/// `session(id, directory, …)` + `part(session_id, time_updated, data JSON)`，
/// `data.type` ∈ text/reasoning/tool/step-start/step-finish/…，
/// `step-finish` 带 `reason`（`stop` = 本回合收尾，`tool-calls` = 还要继续）。
/// 故：**最后一条 part 是 `step-finish`+`stop` → Idle，其余 → Busy**。
const OC_SQL: &str = "SELECT s.directory AS dir, \
     (SELECT json_extract(p.data,'$.type') FROM part p WHERE p.session_id=s.id ORDER BY p.time_updated DESC LIMIT 1) AS last_type, \
     (SELECT json_extract(p.data,'$.reason') FROM part p WHERE p.session_id=s.id ORDER BY p.time_updated DESC LIMIT 1) AS last_reason, \
     (SELECT MAX(p.time_updated) FROM part p WHERE p.session_id=s.id) AS last_ts \
     FROM session s WHERE s.time_archived IS NULL";

fn oc_state(dir: &str) -> Option<RunState> {
    let now = crate::now_ms();
    let mut g = oc_cache().lock().ok()?;
    if let Some((st, at)) = g.map.get(dir)
        && now.saturating_sub(*at) < OC_CACHE_MS
    {
        return *st;
    }
    let st = oc_query(dir);
    g.map.insert(dir.to_string(), (st, now));
    st
}

/// Windows `CREATE_NO_WINDOW`：不分配控制台窗口。
///
/// **必须带**：本程序是 `#![windows_subsystem = "windows"]` 的 GUI 进程，自己
/// 没有控制台。直接 spawn 控制台子进程（`opencode.exe` 就是）会**给它新分配
/// 一个控制台窗口并抢焦点**——表现就是开着 opencode 页签时每隔 2.5s（缓存期）
/// 在前台闪一个「opencode」黑窗。
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 构造 `opencode db` 查询命令（抽出来只为能单测钉住 creation flags）。
///
/// 窗口标志之外，stdin/stdout/stderr 全由 `output()` 接管道，子进程也不会
/// 去碰本进程的（不存在的）控制台。
fn oc_db_cmd(exe: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["db", OC_SQL, "--format", "json"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 跑一次查询（home 目录为 cwd：opencode 的库是全局的，别被项目目录带偏）。
fn oc_query(dir: &str) -> Option<RunState> {
    let exe = opencode_exe()?;
    let out = oc_db_cmd(&exe).current_dir(home_dir()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let rows = v.as_array()?;
    // 同目录可能有多条会话（新建/恢复/子会话）→ 取最后一条 part 最新的那条。
    let mut best: Option<(u64, Option<RunState>)> = None;
    for r in rows {
        if !r
            .get("dir")
            .and_then(|d| d.as_str())
            .is_some_and(|d| path_eq(d, dir))
        {
            continue;
        }
        let ts = r.get("last_ts").and_then(|t| t.as_u64()).unwrap_or(0);
        let ty = r.get("last_type").and_then(|t| t.as_str());
        let reason = r.get("last_reason").and_then(|t| t.as_str());
        let st = match ty {
            None => None,
            Some("step-finish") if reason == Some("stop") => Some(RunState::Idle),
            Some(_) => Some(RunState::Busy),
        };
        if best.as_ref().is_none_or(|(bt, _)| ts > *bt) {
            best = Some((ts, st));
        }
    }
    best.and_then(|(_, st)| st)
}

// ── 跟踪线程 ──────────────────────────────────────────────────────────────

/// 启动跟踪（幂等由调用方保证：每个会话只在 spawn 时调一次）。
///
/// `slot` 存状态（`RunState::to_slot`），`done` 置位即退出线程——复用
/// `Session::exited`，会话关了跟踪自动收尾，不留后台线程。
pub fn start_tracking(cmd: &str, dir: &str, slot: Arc<AtomicU8>, done: Arc<AtomicBool>) {
    let Some(kind) = detect_kind(cmd) else {
        return; // 不认识的命令（shell/vim/…）→ 永远 Unknown，走输出启发式
    };
    let dir = dir.to_string();
    std::thread::Builder::new()
        .name("run-state".into())
        .spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let st = match kind {
                    AgentKind::Pi => pi_state(&dir),
                    AgentKind::Opencode => oc_state(&dir),
                };
                slot.store(st.unwrap_or(RunState::Unknown).to_slot(), Ordering::Relaxed);
                // 分片睡眠：done 一置位最多 100ms 就退出，不用等满一个周期。
                let mut waited = 0;
                while waited < POLL_MS {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    waited += 100;
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已发工具调用、结果未回 → 正在跑工具。
    #[test]
    fn pi_pending_tool_call_is_busy() {
        let l = r#"{"type":"message","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","name":"bash"}]}}"#;
        assert_eq!(pi_state_from_record(l), Some(RunState::Busy));
    }

    /// 工具结果已回 → assistant 还要接着生成，仍算忙。
    #[test]
    fn pi_tool_result_still_busy() {
        let l = r#"{"type":"message","message":{"role":"toolResult","content":[{"type":"text"}],"isError":false}}"#;
        assert_eq!(pi_state_from_record(l), Some(RunState::Busy));
    }

    /// assistant 收尾（stopReason=stop）→ 轮到用户 → 空闲。
    #[test]
    fn pi_finished_turn_is_idle() {
        let l = r#"{"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text"}]}}"#;
        assert_eq!(pi_state_from_record(l), Some(RunState::Idle));
    }

    /// 刚提交 prompt（末条 user）→ 空闲，不能误判成「完成」。
    #[test]
    fn pi_user_message_is_idle() {
        let l = r#"{"type":"message","message":{"role":"user","content":[{"type":"text"}]}}"#;
        assert_eq!(pi_state_from_record(l), Some(RunState::Idle));
    }

    /// stopReason 未知时不强制判定，交给上游往前找确定状态。
    #[test]
    fn pi_unknown_stopreason_skips() {
        let l = r#"{"type":"message","message":{"role":"assistant","stopReason":"thought","content":[]}}"#;
        assert_eq!(pi_state_from_record(l), None);
        let l2 = r#"{"type":"message","message":{"role":"assistant","stopReason":"","content":[]}}"#;
        assert_eq!(pi_state_from_record(l2), None);
    }

    /// 回归锁：`aborted`（用户按 Esc 打断）必须判 Idle，绝不能返回 None。
    /// 返回 None 会让 tail 越过它去看更早的 `assistant/toolUse`，把已收工的
    /// 会话钉成 Busy → 终端零输出而页签永远 🔄。
    #[test]
    fn pi_aborted_is_idle_not_skipped() {
        let l = r#"{"type":"message","message":{"role":"assistant","stopReason":"aborted","content":[]}}"#;
        assert_eq!(pi_state_from_record(l), Some(RunState::Idle));
        // 带一条更早的 toolUse：aborted 必须在它前面拦下，判 Idle 而非 Busy。
        let tail = concat!(
            r#"{"type":"message","message":{"role":"assistant","stopReason":"toolUse"}}"#,
            "\n",
            r#"{"type":"message","message":{"role":"assistant","stopReason":"aborted"}}"#,
            "\n"
        );
        assert_eq!(pi_state_from_tail(tail), Some(RunState::Idle));
    }

    /// custom/session 等非 message 记录要被跳过，看更早的 message 记录。
    #[test]
    fn pi_tail_skips_non_message_records() {
        let tail = concat!(
            r#"{"type":"message","message":{"role":"assistant","stopReason":"toolUse"}}"#,
            "\n",
            r#"{"type":"custom","customType":"compact-thinking-duration","data":{"durationMs":42}}"#,
            "\n"
        );
        assert_eq!(pi_state_from_tail(tail), Some(RunState::Busy));
    }

    /// 末行写了一半（截断 JSON）不能炸，也不能误判：跳过它看上一条。
    #[test]
    fn pi_tail_tolerates_torn_last_line() {
        let tail = concat!(
            r#"{"type":"message","message":{"role":"assistant","stopReason":"toolUse"}}"#,
            "\n",
            r#"{"type":"message","message":{"role":"assistant","stopReason":"sto"#
        );
        assert_eq!(pi_state_from_tail(tail), Some(RunState::Busy));
    }

    /// 空文件/全是非消息记录 → Unknown（回退启发式）。
    #[test]
    fn pi_tail_without_message_is_unknown() {
        assert_eq!(pi_state_from_tail(""), None);
        assert_eq!(pi_state_from_tail(r#"{"type":"session","id":"x"}"#), None);
    }

    /// 命令识别：认程序名，不认路径里的 pi 字样。
    #[test]
    fn detect_kind_by_program_name() {
        assert_eq!(detect_kind(r#"D:\agent\pi\pi.exe"#), Some(AgentKind::Pi));
        assert_eq!(detect_kind("pi --mode rpc"), Some(AgentKind::Pi));
        assert_eq!(detect_kind("omp"), Some(AgentKind::Pi));
        assert_eq!(detect_kind(r#"cd D:\x && opencode"#), Some(AgentKind::Opencode));
        // 不能被路径/子串误伤
        assert_eq!(detect_kind(r#"D:\tools\clip.exe"#), None);
        assert_eq!(detect_kind("powershell.exe"), None);
        assert_eq!(detect_kind("python api.py"), None);
    }

    /// slug 与本机 pi 实际目录名一致（实测 `D:\AI\TUIProjectManager`
    /// → `--D--AI-TUIProjectManager--`）。
    #[test]
    fn pi_slug_matches_real_layout() {
        assert_eq!(
            pi_project_slug(r"D:\AI\TUIProjectManager"),
            "--D--AI-TUIProjectManager--"
        );
        assert_eq!(pi_project_slug(r"C:\Users\yxh"), "--C--Users-yxh--");
    }

    /// opencode 结果解析：step-finish+stop = 空闲，其余 part = 忙。
    #[test]
    fn opencode_part_rule() {
        let rows = r#"[{"dir":"D:\\AI\\x","last_type":"step-finish","last_reason":"stop","last_ts":9},
                        {"dir":"D:\\AI\\x","last_type":"tool","last_reason":null,"last_ts":3}]"#;
        let v: serde_json::Value = serde_json::from_str(rows).unwrap();
        // 最新那条（ts 更大）胜出
        let newest = v
            .as_array()
            .unwrap()
            .iter()
            .max_by_key(|r| r["last_ts"].as_u64().unwrap_or(0))
            .unwrap();
        assert_eq!(newest["last_type"].as_str(), Some("step-finish"));
        assert_eq!(newest["last_reason"].as_str(), Some("stop"));
    }

    /// 路径等价：正/反斜杠、结尾斜杠、大小写差异都得当成同一个目录，
    /// 否则官方字段里明明是这个项目，我们却判定「没这个会话」而静默退回启发式。
    #[test]
    fn path_eq_tolerates_separator_and_case() {
        assert!(path_eq(r"D:\AI\x", "D:/AI/x"));
        assert!(path_eq(r"D:\AI\x\", r"d:\ai\x"));
        assert!(path_eq(r"D:\AI\x", r"D:\AI\x\"));
        assert!(!path_eq(r"D:\AI\x", r"D:\AI\y"));
        assert!(!path_eq(r"D:\AI\x", r"D:\AI\x\sub"));
    }

    /// 会话头里的 `cwd` 是官方给的归属字段；解析它才能把文件认到页签上。
    #[test]
    fn pi_session_cwd_reads_official_header() {
        let dir = std::env::temp_dir().join("tpm-pi-hdr");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("s.jsonl");
        std::fs::write(
            &p,
            "{\"type\":\"session\",\"version\":3,\"id\":\"01a0\",\"timestamp\":\"2026-09-30T23:17:01.862Z\",\"cwd\":\"D:\\\\AI\\\\TUIProjectManager\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\"}}\n",
        )
        .unwrap();
        assert_eq!(pi_session_cwd(&p).as_deref(), Some(r"D:\AI\TUIProjectManager"));
        // 非会话文件（直接是 message）→ None，不能硬当归属字段用
        let q = dir.join("q.jsonl");
        std::fs::write(&q, "{\"type\":\"message\"}\n").unwrap();
        assert_eq!(pi_session_cwd(&q), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存语义回归锁：没有记录 = 没找过，**不能当命中**（曾经用 (None, 0)
    /// 当初值，now_ms() 首次返回 0 → 永远命中「新鲜」→ 永远返回 None）。
    #[test]
    fn exe_cache_needs_a_record_before_hitting() {
        let none: ExeCache = None;
        assert_eq!(cache_hit(&none, 0), None, "没记录就不是命中");
        assert_eq!(cache_hit(&none, 1_000_000), None);
        // 有记录但已记住「没有」→ 命中并返回 None（省掉重复查找）
        let known_absent = Some((None, 1_000u64));
        assert_eq!(cache_hit(&known_absent, 1_000), Some(None));
        // 过期 → 重新找
        assert_eq!(cache_hit(&known_absent, 1_000 + EXE_CACHE_MS), None);
        // 命中且确有 exe → 原样返回
        let found = Some((Some(PathBuf::from("opencode.exe")), 500u64));
        assert_eq!(
            cache_hit(&found, 600),
            Some(Some(PathBuf::from("opencode.exe")))
        );
    }

/// 回归锁：状态查询**必须无窗口跑**。本程序是 GUI 子系统进程，自己没控制台，
    /// spawn 控制台子进程会新分配控制台并抢焦点 → 开 opencode 页签时每隔 2.5s
    /// 在前台闪一个「opencode」黑窗。
    ///
    /// 参数与标志两层都锁。标志只能拿源码断言：`Command` 在 stable 上**没有
    /// `get_creation_flags` 这种读取口**，设完就看不回来，唯一能回归锁住
    /// 「真的在 spawn 处钉上了」的办法就是把本文件源码当测试数据（自引用
    /// `include_str!`，改动即重编）。
    #[test]
    fn oc_query_runs_windowless_with_exact_args() {
        let c = oc_db_cmd(std::path::Path::new("opencode.exe"));
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
assert_eq!(args.len(), 4, "只应有 db / SQL / --format / json 四段");
        assert_eq!(args[0], "db");
        assert_eq!(args[1], OC_SQL);
        assert_eq!(&args[2..], &["--format".to_string(), "json".to_string()]);
        #[cfg(windows)]
        {
            let src = include_str!("runstate.rs");
            let body = src
                .split_once("fn oc_db_cmd")
                .and_then(|(_, r)| r.split_once("fn oc_query"))
                .map(|(f, _)| f)
                .expect("得找得到 oc_db_cmd");
            assert!(
                body.contains("creation_flags(CREATE_NO_WINDOW)"),
                "spawn 处必须钉 CREATE_NO_WINDOW，否则每 2.5s 在前台闪一次黑窗"
            );
        }
    }

    /// 标志值本身锁死（0x0800_0000 = CREATE_NO_WINDOW）。
    #[test]
    fn create_no_window_value_is_locked() {
        #[cfg(windows)]
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
    }

    /// 状态槽位往返；0 必须回到 Unknown（触发回退）。
    #[test]
    fn slot_roundtrip() {
        assert_eq!(RunState::from_slot(0), RunState::Unknown);
        assert_eq!(RunState::from_slot(RunState::Busy.to_slot()), RunState::Busy);
        assert_eq!(RunState::from_slot(RunState::Idle.to_slot()), RunState::Idle);
    }

    /// 非 agent 命令不启动跟踪（槽位恒为 Unknown）。
    #[test]
    fn plain_shell_not_tracked() {
        let slot = Arc::new(AtomicU8::new(0));
        let done = Arc::new(AtomicBool::new(false));
        start_tracking("cmd.exe", "C:\\", slot.clone(), done);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(slot.load(Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// 手工验证用（`cargo test -- --ignored live_`）：对着本机真实数据跑一遍
    /// 两个官方状态源，确认能认到会话并给出状态。CI/别的机器上没有这些
    /// 文件，默认不跑。
    #[test]
    #[ignore]
    fn live_probe_against_real_files() {
        let cwd = std::env::current_dir().unwrap().display().to_string();
        println!("cwd = {cwd}");
        match pi_session_file(&cwd) {
            Some(p) => {
                println!("pi 会话文件 = {}", p.display());
                println!("pi 官方 cwd = {:?}", pi_session_cwd(&p));
                println!("pi 状态 = {:?}", pi_state(&cwd));
            }
            None => println!("pi：没找到属于该 cwd 的会话（回退启发式）"),
        }
        println!("config_path exists = {}", crate::config::config_path().exists());
        println!("find_opencode_exe = {:?}", crate::app::find_opencode_exe());
        println!("opencode 可执行 = {:?}", opencode_exe());
        println!("opencode 状态 = {:?}", oc_state(&cwd));
    }
}
