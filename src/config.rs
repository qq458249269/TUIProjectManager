use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一个保存的项目条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: String,
}

/// 程序设置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// 已配置的 TUI 命令列表。
    #[serde(default)]
    pub tui_commands: Vec<String>,
    /// 当前选中的 TUI 命令（启动项目时使用）。
    pub tui_command: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tui_commands: vec!["nvim".to_string()],
            tui_command: "nvim".to_string(),
        }
    }
}

/// 应用配置，保存到与程序同级目录下的 config/config.json。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub projects: Vec<Project>,
    pub settings: Settings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            settings: Settings::default(),
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