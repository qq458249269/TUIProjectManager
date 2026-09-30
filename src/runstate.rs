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
/// 读会话文件尾多少字节。末行可能被写了一半，最多也就一行（几 KB）。
const TAIL_BYTES: u64 = 64 * 1024;

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

/// 该 cwd 下最近改动的会话文件。目录 slug 对不上就退回「扫全部项目目录、
/// 取 mtime 最新的 jsonl」——slug 规则是照着本机目录名反推的，pi 改了也不至于
/// 整个功能失效（最多认到别的项目，同目录同名冲突本来就极少）。
fn pi_session_file(dir: &str) -> Option<PathBuf> {
    let root = pi_sessions_root();
    let mut dirs = vec![root.join(pi_project_slug(dir))];
    if !dirs[0].is_dir() {
        dirs.clear();
        let Ok(rd) = std::fs::read_dir(&root) else {
            return None;
        };
        dirs.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()));
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
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
            if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                best = Some((t, p));
            }
        }
    }
    best.map(|(_, p)| p)
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
            if stop == "toolUse" {
                Some(RunState::Busy)
            } else {
                Some(RunState::Idle)
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
    let len = std::fs::metadata(&path).ok()?.len();
    let take = TAIL_BYTES.min(len.max(1));
    let mut f = std::fs::File::open(&path).ok()?;
    use std::io::{Read, Seek};
    f.seek(std::io::SeekFrom::Start(len - take)).ok()?;
    let mut buf = Vec::with_capacity(take as usize);
    f.read_to_end(&mut buf).ok()?;
    pi_state_from_tail(&String::from_utf8_lossy(&buf))
}

// ── opencode：SQLite（经其 `opencode db` CLI） ────────────────────────────

/// opencode 可执行文件：先找本 exe 同级目录（更新器装哪儿就找哪儿），
/// 再退 PATH。只在开跟踪线程时找一次。
fn opencode_exe() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let p = dir.join(if cfg!(windows) { "opencode.exe" } else { "opencode" });
        if p.is_file() {
            return Some(p);
        }
    }
    let name = if cfg!(windows) { "opencode.exe" } else { "opencode" };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
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

/// 跑一次查询（home 目录为 cwd：opencode 的库是全局的，别被项目目录带偏）。
fn oc_query(dir: &str) -> Option<RunState> {
    let exe = opencode_exe()?;
    let out = std::process::Command::new(exe)
        .args(["db", OC_SQL, "--format", "json"])
        .current_dir(home_dir())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let rows = v.as_array()?;
    // 同目录可能有多条会话（新建/恢复/子会话）→ 取最后一条 part 最新的那条。
    let mut best: Option<(u64, Option<RunState>)> = None;
    for r in rows {
        if r.get("dir").and_then(|d| d.as_str()) != Some(dir) {
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
