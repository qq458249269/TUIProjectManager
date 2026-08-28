use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一个保存的项目条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: String,
}

/// 界面基线刷新帧率的默认值（帧/秒），对应默认 10 帧/秒。
pub(crate) const DEFAULT_REFRESH_FPS: u64 = 10;

fn default_refresh_fps() -> u64 {
    DEFAULT_REFRESH_FPS
}

fn default_dark_mode() -> bool {
    true
}

fn default_follow_system() -> bool {
    false
}

/// 程序设置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// 已配置的 TUI 命令列表。
    #[serde(default)]
    pub tui_commands: Vec<String>,
    /// 当前选中的 TUI 命令（启动项目时使用）。
    pub tui_command: String,
    /// 界面基线刷新帧率（10..=60；数字越大越流畅、CPU 占用越高）。默认 10。
    #[serde(default = "default_refresh_fps")]
    pub refresh_fps: u64,
    /// 深浅主题：true=深色（默认），false=浅色。
    #[serde(default = "default_dark_mode")]
    pub dark_mode: bool,
    /// 跟随系统主题：true 时按系统深浅动态切换（覆盖 dark_mode）。
    #[serde(default = "default_follow_system")]
    pub follow_system: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tui_commands: vec!["nvim".to_string()],
            tui_command: "nvim".to_string(),
            refresh_fps: DEFAULT_REFRESH_FPS,
            dark_mode: true,
            follow_system: false,
        }
    }
}

/// 应用配置，保存到与程序同级目录下的 config/config.json。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub projects: Vec<Project>,
    pub settings: Settings,
    /// 窗口位置/大小，下次启动时恢复。
    #[serde(default)]
    pub window: WindowState,
    /// 上次打开中的页签，下次启动时重新拉起。
    #[serde(default)]
    pub tabs: TabsState,
}

/// 上次的窗口状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WindowState {
    pub pos: Option<[f32; 2]>,
    pub size: Option<[f32; 2]>,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            pos: None,
            size: None,
            maximized: false,
        }
    }
}

/// 上次退出时打开中的终端页签（启动时重新拉起）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TabsState {
    /// 打开的会话目录，按页签顺序。
    pub dirs: Vec<String>,
    /// 上次激活的页签索引（0 = 首页）。
    pub active: usize,
}

impl Default for TabsState {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            active: 0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            settings: Settings::default(),
            window: WindowState::default(),
            tabs: TabsState::default(),
        }
    }
}

/// 与程序可执行文件同级的 config 目录。
pub fn config_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("config");
        }
    }
    PathBuf::from("config")
}

/// 配置文件路径。
pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// 加载配置；文件不存在或解析失败时返回默认配置。
pub fn load() -> Config {
    let path = config_path();
    let mut config = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => Config::default(),
    };
    // 旧版本只有 tui_command，迁移到 tui_commands 列表。
    if config.settings.tui_commands.is_empty() {
        let cmd = config.settings.tui_command.trim().to_string();
        if !cmd.is_empty() {
            config.settings.tui_commands.push(cmd);
        }
    }
    config
}

/// 保存配置到 config/config.json。
pub fn save(config: &Config) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(config_path(), json).map_err(|e| e.to_string())
}

// ── 模型配置（pi / oh-my-pi） ──────────────────────────────────────────

/// 单个模型条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub max_tokens: u64,
}

impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            id: "1".into(),
            name: "1".into(),
            context_window: 200_000,
            max_tokens: 8192,
        }
    }
}

/// 单个 provider 条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEntry {
    pub base_url: String,
    #[serde(default)]
    pub api: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

impl Default for ProviderEntry {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:20128/v1".into(),
            api: "openai-completions".into(),
            api_key: "sk-18d904d21da7b328-s15pfo-8f0e32c3".into(),
            models: vec![ModelEntry::default()],
        }
    }
}

/// 模型配置（pi 的 JSON / oh-my-pi 的 YAML 共用此结构）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelsConfig {
    #[serde(default)]
    pub providers: std::collections::HashMap<String, ProviderEntry>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        let mut providers = std::collections::HashMap::new();
        providers.insert("1".into(), ProviderEntry::default());
        Self { providers }
    }
}

/// 用户 home 目录。
fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// pi 模型配置文件路径：~/.pi/agent/models.json
pub fn pi_models_path() -> PathBuf {
    home_dir().join(".pi").join("agent").join("models.json")
}

/// oh-my-pi 模型配置文件路径：~/.omp/agent/models.yml
pub fn omp_models_path() -> PathBuf {
    home_dir().join(".omp").join("agent").join("models.yml")
}

/// 读取 pi 模型配置；文件不存在时创建默认配置。
pub fn load_pi_models() -> ModelsConfig {
    let path = pi_models_path();
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => {
            let cfg = ModelsConfig::default();
            let _ = save_pi_models(&cfg);
            cfg
        }
    }
}

/// 保存 pi 模型配置。
pub fn save_pi_models(cfg: &ModelsConfig) -> Result<(), String> {
    let path = pi_models_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// 读取 oh-my-pi 模型配置；文件不存在时创建默认配置。
pub fn load_omp_models() -> ModelsConfig {
    let path = omp_models_path();
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_yaml::from_str(&raw).unwrap_or_default(),
        Err(_) => {
            let cfg = ModelsConfig::default();
            let _ = save_omp_models(&cfg);
            cfg
        }
    }
}

/// 保存 oh-my-pi 模型配置。
pub fn save_omp_models(cfg: &ModelsConfig) -> Result<(), String> {
    let path = omp_models_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let yaml = serde_yaml::to_string(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, yaml).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_json_roundtrip() {
        let cfg = ModelsConfig::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        // 验证字段名是 camelCase
        assert!(json.contains("baseUrl"), "JSON 应含 camelCase baseUrl: {json}");
        assert!(json.contains("apiKey"), "JSON 应含 camelCase apiKey: {json}");
        assert!(json.contains("contextWindow"), "JSON 应含 camelCase contextWindow: {json}");
        assert!(json.contains("maxTokens"), "JSON 应含 camelCase maxTokens: {json}");
        // 回环验证
        let parsed: ModelsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn models_yaml_roundtrip() {
        let cfg = ModelsConfig::default();
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        assert!(yaml.contains("baseUrl"), "YAML 应含 camelCase baseUrl: {yaml}");
        assert!(yaml.contains("apiKey"), "YAML 应含 camelCase apiKey: {yaml}");
        assert!(yaml.contains("contextWindow"), "YAML 应含 camelCase contextWindow: {yaml}");
        assert!(yaml.contains("maxTokens"), "YAML 应含 camelCase maxTokens: {yaml}");
        let parsed: ModelsConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, cfg);
    }
}