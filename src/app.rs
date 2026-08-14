use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;
use egui::{Color32, RichText};

use crate::config;
use crate::session::{self, Session};
use crate::terminal;

/// 首页里的两个子页。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Settings,
}

/// 顶部页签：第一个永远是首页，后面每个对应一个终端会话。
pub enum Tab {
    Home,
    Session(Session),
}

/// 输入弹窗的类型与当前文本。
pub enum InputDialog {
    AddProject { name: String, path: String },
    Rename { value: String },
    EditPath { value: String },
}

impl InputDialog {
    fn title(&self) -> &'static str {
        match self {
            InputDialog::AddProject { .. } => "添加项目",
            InputDialog::Rename { .. } => "重命名项目",
            InputDialog::EditPath { .. } => "修改项目路径",
        }
    }
}

/// 确认弹窗。
pub enum ConfirmDialog {
    DeleteProject { index: usize, name: String },
}

/// 页签栏点击产生的动作。
enum TabAction {
    Activate(usize),
    Close(usize),
}

/// 加载中文字体作为 Proportional 与 Monospace 的 fallback。
fn setup_fonts(ctx: &egui::Context) {
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
                vec![
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Proportional,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Monospace,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                ],
            ));
            break;
        }
    }
}

/// 应用深浅主题（egui 全部控件/字体颜色随之切换）。
fn apply_theme(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
}

/// 深浅主题下可读的提示色。
fn ui_warn(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(220, 170, 60)
    } else {
        Color32::from_rgb(160, 125, 15)
    }
}

fn ui_ok(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(90, 200, 90)
    } else {
        Color32::from_rgb(35, 150, 45)
    }
}

fn ui_gray(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::GRAY
    } else {
        Color32::from_gray(90)
    }
}

/// 从 GitHub Release 拉取最新版本号，返回（状态栏消息, 有新版本时的 tag）。
fn fetch_latest_release() -> (String, Option<String>) {
    const URL: &str =
        "https://api.github.com/repos/qq458249269/TUIProjectManager/releases/latest";
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-s",
        "--connect-timeout",
        "8",
        "--ssl-no-revoke",
        "-H",
        "User-Agent: TUIProjectManager",
        URL,
    ]);
    // GUI 程序 spawn 控制台程序（curl.exe）会闪一个黑窗口：
    // CREATE_NO_WINDOW 让子进程不分配控制台，彻底消除。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd.output();
    match out {
        Ok(o) if o.status.success() => {
            match serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&o.stdout)) {
                Ok(v) => {
                    let tag = v["tag_name"].as_str().unwrap_or("?").to_string();
                    let latest = tag.trim_start_matches('v');
                    if version_newer(latest, crate::app_version()) {
                        (format!("发现新版本 {tag}，可在下方点击下载"), Some(tag))
                    } else {
                        (format!("已是最新版本 ({tag})"), None)
                    }
                }
                Err(_) => ("检查更新失败：无法解析 GitHub 响应".to_string(), None),
            }
        }
        Ok(_) => ("检查更新失败：网络错误".to_string(), None),
        Err(e) => (format!("检查更新失败：{e}"), None),
    }
}

/// 点分数字版本比较（如 2025.06.30.0001），a > b 返回 true。
fn version_newer(a: &str, b: &str) -> bool {
    let pa: Vec<u64> = a.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    let pb: Vec<u64> = b.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

pub struct ClientApp {
    pub config: config::Config,
    pub tabs: Vec<Tab>,
    pub current: usize,
    pub screen: Screen,
    pub selected_project: usize,
    pub settings_command: String,
    pub settings_commands: Vec<String>,
    pub settings_new_command: String,
    pub status: Option<String>,
    pub config_path: PathBuf,
    pub term_focused: bool,
    update_latest: Option<String>,
    check_tx: Sender<(String, Option<String>)>,
    update_rx: Receiver<(String, Option<String>)>,
    pub input: Option<InputDialog>,
    pub confirm: Option<ConfirmDialog>,
    redraw_tx: std::sync::mpsc::SyncSender<()>,
    redraw_rx: Receiver<()>,
}

impl ClientApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let config = config::load();
        apply_theme(&cc.egui_ctx, config.settings.dark_mode);
        let config_path = config::config_path();
        let settings_command = config.settings.tui_command.clone();
        let settings_commands = config.settings.tui_commands.clone();
        // 重绘信号用容量 1 的有界通道：任意多个终端会话/后台线程并发投递时，
        // 通道满即丢弃新信号（try_send），刷新请求被合并——不会出现消息堆积。
        let (redraw_tx, redraw_rx) = std::sync::mpsc::sync_channel(1);
        let (check_tx, update_rx) = std::sync::mpsc::channel();
        let saved_tabs = config.tabs.clone();
        let saved_active = config.tabs.active;
        let mut app = Self {
            config,
            tabs: vec![Tab::Home],
            current: 0,
            screen: Screen::Main,
            selected_project: 0,
            settings_command,
            settings_commands,
            settings_new_command: String::new(),
            status: Some("在左侧选择项目并点击「启动」启动内嵌终端页签。".to_string()),
            config_path,
            term_focused: false,
            update_latest: None,
            input: None,
            confirm: None,
            check_tx,
            update_rx,
            redraw_tx,
            redraw_rx,
        };

        // 恢复上次退出时打开中的终端页签：目录仍存在则重新拉起 TUI 会话。
        let mut restored = 0usize;
        for d in &saved_tabs.dirs {
            if !Path::new(d).is_dir() {
                continue;
            }
            let name = app
                .config
                .projects
                .iter()
                .find(|p| p.path == *d)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| {
                    Path::new(d)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| d.clone())
                });
            match session::spawn(
                &name,
                d,
                &app.config.settings.tui_command,
                80,
                24,
                app.redraw_tx.clone(),
            ) {
                Ok(sess) => {
                    app.tabs.push(Tab::Session(sess));
                    restored += 1;
                }
                Err(_) => {}
            }
        }
        if restored > 0 {
            app.current = saved_active.min(app.tabs.len() - 1);
            app.term_focused = app.current != 0;
            app.status = Some(format!("已恢复上次的 {restored} 个终端页签"));
        }
        app
    }

    fn save_config(&mut self, msg: String) {
        match config::save(&self.config) {
            Ok(()) => self.status = Some(msg),
            Err(e) => self.status = Some(format!("保存配置失败: {e}")),
        }
    }


    fn open_settings(&mut self) {
        self.settings_command = self.config.settings.tui_command.clone();
        self.settings_commands = self.config.settings.tui_commands.clone();
        self.settings_new_command.clear();
        self.screen = Screen::Settings;
        self.term_focused = false;
    }

    fn refresh_focus(&mut self) {
        self.term_focused = !matches!(self.tabs.get(self.current), Some(Tab::Home));
        if !self.term_focused {
            self.screen = Screen::Main;
        }
    }

    /// 异步检查 GitHub Release 最新版本，结果回到状态栏。
    fn check_updates(&mut self) {
        self.status = Some("正在检查更新…".to_string());
        let tx = self.check_tx.clone();
        let redraw_tx = self.redraw_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_latest_release());
            // try_send：通道满说明已有待处理重绘，本次唤醒请求可安全丢弃。
            let _ = redraw_tx.try_send(());
        });
    }

    fn launch_selected(&mut self) {
        let Some(project) = self.config.projects.get(self.selected_project).cloned() else {
            self.status = Some("请先在左侧选择一个项目".to_string());
            return;
        };
        let exists = Path::new(&project.path).is_dir();
        if !exists {
            self.status = Some(format!("目录不存在，无法启动: {}", project.path));
            return;
        }
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Tab::Session(s) = tab {
                if s.dir == project.path && !s.exited {
                    self.current = i;
                    self.term_focused = true;
                    self.status = Some(format!("已切换到会话: {}", s.title));
                    return;
                }
            }
        }
        let cols = 80u16;
        let rows = 24u16;
        match session::spawn(
            &project.name,
            &project.path,
            &self.config.settings.tui_command,
            cols,
            rows,
            self.redraw_tx.clone(),
        ) {
            Ok(sess) => {
                self.tabs.push(Tab::Session(sess));
                self.current = self.tabs.len() - 1;
                self.term_focused = true;
                self.screen = Screen::Main;
                self.status = Some(format!("已启动: {}", project.name));
            }
            Err(e) => self.status = Some(format!("启动失败: {e}")),
        }
    }

    fn close_session(&mut self, idx: usize) {
        if idx == 0 || idx >= self.tabs.len() {
            return;
        }
        if let Some(Tab::Session(s)) = self.tabs.get_mut(idx) {
            let _ = s.child.kill();
        }
        self.tabs.remove(idx);
        if self.current >= self.tabs.len() {
            self.current = self.tabs.len().saturating_sub(1);
        }
        if self.current == 0 {
            self.screen = Screen::Main;
        }
        self.refresh_focus();
        self.status = Some("已关闭会话".to_string());
    }

    fn update_exited(&mut self) -> bool {
        let mut changed = false;
        for tab in self.tabs.iter_mut() {
            if let Tab::Session(s) = tab {
                if !s.exited {
                    if let Ok(Some(_)) = s.child.try_wait() {
                        s.exited = true;
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    fn commit_input(&mut self, dialog: InputDialog) {
        match dialog {
            InputDialog::AddProject { name, path } => {
                let mut name = name.trim().to_string();
                let path = path.trim().to_string();
                if path.is_empty() {
                    self.status = Some("请选择或输入项目路径".to_string());
                    self.input = Some(InputDialog::AddProject {
                        name,
                        path,
                    });
                    return;
                }
                if name.is_empty() {
                    // 名称留空时，默认用路径的最后一段作为项目名。
                    let fallback = Path::new(path.trim_end_matches(['/', '\\']))
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| path.clone());
                    name = fallback;
                }
                self.config.projects.push(config::Project { name, path });
                self.selected_project = self.config.projects.len() - 1;
                self.input = None;
                self.save_config("已添加项目".to_string());
            }
            InputDialog::Rename { value } => {
                let value = value.trim().to_string();
                if let Some(p) = self.config.projects.get_mut(self.selected_project) {
                    p.name = value;
                }
                self.input = None;
                self.save_config("已重命名".to_string());
            }
            InputDialog::EditPath { value } => {
                let value = value.trim().to_string();
                if let Some(p) = self.config.projects.get_mut(self.selected_project) {
                    p.path = value;
                }
                self.input = None;
                self.save_config("已修改路径".to_string());
            }
        }
    }

    fn open_rename(&mut self) {
        let Some(p) = self.config.projects.get(self.selected_project) else {
            self.status = Some("请先选择一个项目".to_string());
            return;
        };
        self.input = Some(InputDialog::Rename {
            value: p.name.clone(),
        });
    }

    fn open_edit_path(&mut self) {
        let Some(p) = self.config.projects.get(self.selected_project) else {
            self.status = Some("请先选择一个项目".to_string());
            return;
        };
        self.input = Some(InputDialog::EditPath {
            value: p.path.clone(),
        });
    }

    fn request_delete(&mut self) {
        let Some(p) = self.config.projects.get(self.selected_project) else {
            self.status = Some("请先选择一个项目".to_string());
            return;
        };
        self.confirm = Some(ConfirmDialog::DeleteProject {
            index: self.selected_project,
            name: p.name.clone(),
        });
    }

    fn confirm_delete(&mut self, index: usize) {
        if index < self.config.projects.len() {
            self.config.projects.remove(index);
            if self.selected_project >= self.config.projects.len() {
                self.selected_project = self.config.projects.len().saturating_sub(1);
            }
            self.save_config("已删除项目".to_string());
        }
    }

    fn shutdown(&mut self) {
        for tab in self.tabs.iter_mut() {
            if let Tab::Session(s) = tab {
                let _ = s.child.kill();
            }
        }
    }

    // ---- 渲染 ----

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut actions: Vec<TabAction> = Vec::new();
        ui.horizontal(|ui| {
            if let Some(Tab::Home) = self.tabs.first() {
                let selected = self.current == 0;
                if ui
                    .selectable_label(selected, RichText::new("🏠 首页").strong())
                    .clicked()
                    && !selected
                {
                    actions.push(TabAction::Activate(0));
                }
            }
            for (i, tab) in self.tabs.iter().enumerate().skip(1) {
                if let Tab::Session(s) = tab {
                    ui.add_space(4.0);
                    // 标题 + 关闭按钮一组，收紧间距让 ✕ 紧贴页签。
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        let title = if s.exited {
                            format!("{} (已退出)", s.title)
                        } else {
                            s.title.clone()
                        };
                        let selected = self.current == i;
                        if ui.selectable_label(selected, title).clicked() && !selected {
                            actions.push(TabAction::Activate(i));
                        }
                        // × 用 U+00D7（Latin-1）而不是 ✕ (U+2715)：后者在 egui 自带字体
                        // 与系统 CJK 字体里都可能缺字形，导致关闭图标不显示。
                        if ui.add(egui::Button::new("×").small().frame(false))
                            .on_hover_text("关闭会话")
                            .clicked()
                        {
                            actions.push(TabAction::Close(i));
                        }
                    });
                }
            }

            // 右侧：深浅主题切换 + 快速打开用户目录。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("🔄 检查更新")
                    .on_hover_text("从 GitHub Release 检查最新版本")
                    .clicked()
                {
                    self.check_updates();
                }
                let dark = self.config.settings.dark_mode;
                let theme_btn = if dark {
                    ui.button("☀ 浅色").on_hover_text("切换到浅色主题，字体与颜色同步切换")
                } else {
                    ui.button("🌙 深色").on_hover_text("切换到深色主题，字体与颜色同步切换")
                };
                if theme_btn.clicked() {
                    self.config.settings.dark_mode = !dark;
                    apply_theme(ui.ctx(), self.config.settings.dark_mode);
                    self.save_config("已切换主题".to_string());
                    ui.ctx().request_repaint();
                }
                if ui
                    .button("📂 用户目录")
                    .on_hover_text("打开用户目录（%USERPROFILE%），便于修改 agent 配置")
                    .clicked()
                {
                    let dir = std::env::var("USERPROFILE")
                        .or_else(|_| std::env::var("HOME"))
                        .unwrap_or_else(|_| ".".to_string());
                    match std::process::Command::new("explorer").arg(&dir).spawn() {
                        Ok(_) => {
                            self.status = Some(format!("已打开用户目录: {dir}"));
                        }
                        Err(e) => self.status = Some(format!("打开用户目录失败: {e}")),
                    }
                }
            });
        });
        for action in actions {
            match action {
                TabAction::Activate(i) => {
                    self.current = i;
                    self.refresh_focus();
                }
                TabAction::Close(i) => self.close_session(i),
            }
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (text, color) = match &self.status {
                Some(s) => (s.clone(), ui_warn(ui)),
                None => match self.tabs.get(self.current) {
                    Some(Tab::Session(s)) => (
                        format!(
                            "会话 {} / {}  项目: {}",
                            self.current,
                            self.tabs.len() - 1,
                            s.title
                        ),
                        ui_gray(ui),
                    ),
                    _ => (
                        "选择项目 → 启动（内嵌终端页签）   |   添加 / 重命名 / 改路径 / 删除 / 设置"
                            .to_string(),
                        ui_gray(ui),
                    ),
                },
            };
            ui.label(RichText::new(text).color(color));
            if let Some(tag) = &self.update_latest {
                ui.separator();
                ui.hyperlink_to(
                    format!("⬇ 下载 {tag} (GitHub Release)"),
                    format!("https://github.com/qq458249269/TUIProjectManager/releases/tag/{tag}"),
                );
            }
        });
    }

    fn home_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("project_list")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.heading("项目列表");
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("＋ 添加").clicked() {
                        self.input = Some(InputDialog::AddProject {
                            name: String::new(),
                            path: String::new(),
                        });
                    }
                    if ui.button("⚙ 设置").clicked() {
                        self.open_settings();
                    }
                });
                ui.separator();
                if self.config.projects.is_empty() {
                    ui.label(RichText::new("暂无项目，点击「＋ 添加」创建一个。").weak());
                }
                let sel = self.selected_project;
                let mut clicked: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (i, p) in self.config.projects.iter().enumerate() {
                            let exists = Path::new(&p.path).is_dir();
                            let label = if exists {
                                format!("● {}", p.name)
                            } else {
                                format!("○ {}  (目录不存在)", p.name)
                            };
                            let color = if exists {
                                ui.visuals().text_color()
                            } else {
                                ui_warn(ui)
                            };
                            if ui
                                .selectable_label(sel == i, RichText::new(label).color(color))
                                .clicked()
                            {
                                clicked = Some(i);
                            }
                        }
                    });
                if let Some(i) = clicked {
                    self.selected_project = i;
                    self.screen = Screen::Main;
                }
            });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.screen == Screen::Settings {
                self.settings_ui(ui);
            } else {
                self.project_detail_ui(ui);
            }
        });
    }

    fn project_detail_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        let Some(p) = self.config.projects.get(self.selected_project).cloned() else {
            ui.heading("欢迎使用 TUI 项目管理器");
            ui.add_space(6.0);
            ui.label("在左侧选择或添加一个项目，然后点击「启动」，将在一个内嵌终端页签中于该项目目录运行配置的 TUI 程序。");
            return;
        };
        let exists = Path::new(&p.path).is_dir();

        ui.heading(&p.name);
        ui.separator();
        ui.label("路径:");
        ui.monospace(&p.path);
        ui.label(
            RichText::new(if exists {
                "✓ 目录存在"
            } else {
                "✗ 目录不存在"
            })
            .color(if exists { ui_ok(ui) } else { ui_warn(ui) }),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let launch = ui.add_enabled(
                exists,
                egui::Button::new(RichText::new("▶ 启动 (内嵌页签)").strong()),
            );
            if launch.clicked() {
                self.launch_selected();
            }
            let open_dir = ui.add_enabled(
                exists,
                egui::Button::new(RichText::new("📂 打开目录")),
            );
            if open_dir.clicked() {
                if let Err(e) = std::process::Command::new("explorer")
                    .arg(&p.path)
                    .spawn()
                {
                    self.status = Some(format!("打开目录失败: {e}"));
                }
            }
            if ui.button("重命名").clicked() {
                self.open_rename();
            }
            if ui.button("改路径").clicked() {
                self.open_edit_path();
            }
            if ui.button("删除").clicked() {
                self.request_delete();
            }
        });
        ui.separator();
        ui.label(
            RichText::new(format!(
                "TUI 命令: {}  （在设置中修改）",
                self.config.settings.tui_command
            ))
            .weak(),
        );
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading("设置");
        ui.separator();
        ui.label("TUI 启动命令（点击选择启动时要用的命令，可添加多个，改动自动保存）:");
        ui.add_space(4.0);

        let mut dirty = false;
        let mut remove_idx: Option<usize> = None;
        for (i, cmd) in self.settings_commands.iter().enumerate() {
            ui.horizontal(|ui| {
                let selected = *cmd == self.settings_command;
                let resp = ui.selectable_label(
                    selected,
                    RichText::new(if selected { format!("◉ {cmd}") } else { format!("○ {cmd}") }),
                );
                if resp.clicked() {
                    self.settings_command = cmd.clone();
                    dirty = true;
                }
                if ui
                    .small_button("删除")
                    .on_hover_text("从命令列表中移除")
                    .clicked()
                {
                    remove_idx = Some(i);
                }
            });
        }
        if let Some(i) = remove_idx {
            if i < self.settings_commands.len() {
                let removed = self.settings_commands.remove(i);
                if self.settings_command == removed {
                    self.settings_command = self
                        .settings_commands
                        .first()
                        .cloned()
                        .unwrap_or_default();
                }
                dirty = true;
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.settings_new_command)
                    .desired_width(280.0)
                    .hint_text("新命令，如 lazygit / htop"),
            );
            if ui.button("浏览…").on_hover_text("选择可执行文件").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("选择 TUI 可执行文件")
                    .add_filter("可执行文件", &["exe", "bat", "cmd", "com"])
                    .pick_file()
                {
                    self.settings_new_command = path.to_string_lossy().to_string();
                }
            }
            let clicked = ui.button("添加").clicked();
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if clicked || enter {
                let cmd = self.settings_new_command.trim().to_string();
                if !cmd.is_empty() && !self.settings_commands.contains(&cmd) {
                    self.settings_commands.push(cmd.clone());
                    self.settings_new_command.clear();
                    dirty = true;
                }
            }
        });
        ui.label(
            RichText::new("示例: nvim / lazygit / htop / cmd / bash")
                .weak()
                .small(),
        );
        if dirty {
            self.config.settings.tui_command = self.settings_command.trim().to_string();
            self.config.settings.tui_commands = self.settings_commands.clone();
            self.save_config("设置已自动保存".to_string());
        }
        ui.add_space(12.0);
        ui.label(RichText::new(format!("配置文件: {}", self.config_path.display())).weak());
        ui.add_space(12.0);
        if ui.button("← 返回项目列表").clicked() {
            self.screen = Screen::Main;
        }
    }

    fn input_dialog(&mut self, ui: &mut egui::Ui) {
        let mut dialog = match self.input.take() {
            Some(d) => d,
            None => return,
        };
        let title = dialog.title();
        let is_rename = matches!(dialog, InputDialog::Rename { .. });
        let is_edit_path = matches!(dialog, InputDialog::EditPath { .. });
        let mut commit = false;
        let mut cancel = false;

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                match &mut dialog {
                    InputDialog::AddProject { name, path } => {
                        ui.label(
                            RichText::new("项目名称（留空则使用路径最后一段作为名称）")
                                .weak()
                                .small(),
                        );
                        let name_resp =
                            ui.add(egui::TextEdit::singleline(name).desired_width(340.0));
                        ui.add_space(4.0);
                        ui.label(RichText::new("项目路径").weak().small());
                        ui.horizontal(|ui| {
                            let path_resp = ui.add(
                                egui::TextEdit::singleline(path)
                                    .desired_width(340.0)
                                    .hint_text("选择或输入文件夹路径"),
                            );
                            let browse = ui.button("浏览…").clicked();
                            if browse {
                                if let Some(dir) = rfd::FileDialog::new()
                                    .set_title("选择项目文件夹")
                                    .pick_folder()
                                {
                                    *path = dir.to_string_lossy().to_string();
                                }
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && (name_resp.has_focus() || path_resp.has_focus())
                            {
                                commit = true;
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Escape))
                                && (name_resp.has_focus() || path_resp.has_focus())
                            {
                                cancel = true;
                            }
                        });
                    }
                    InputDialog::Rename { value } | InputDialog::EditPath { value } => {
                        let hint = if is_rename { "新名称" } else { "新路径" };
                        ui.label(RichText::new(hint).weak().small());
                        ui.horizontal(|ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(value)
                                    .desired_width(340.0)
                                    .hint_text(hint),
                            );
                            if !resp.has_focus() {
                                resp.request_focus();
                            }
                            if is_edit_path {
                                if ui.button("浏览…").clicked() {
                                    if let Some(dir) = rfd::FileDialog::new()
                                        .set_title("选择项目文件夹")
                                        .pick_folder()
                                    {
                                        *value = dir.to_string_lossy().to_string();
                                    }
                                }
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) && resp.has_focus() {
                                commit = true;
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Escape)) && resp.has_focus() {
                                cancel = true;
                            }
                        });
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("确定").clicked() {
                        commit = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            });

        if commit {
            self.commit_input(dialog);
        } else if cancel {
            self.input = None;
        } else {
            self.input = Some(dialog);
        }
    }

    fn confirm_dialog(&mut self, ui: &mut egui::Ui) {
        let dialog = match self.confirm.take() {
            Some(d) => d,
            None => return,
        };
        let (message, _index) = match &dialog {
            ConfirmDialog::DeleteProject { index, name } => {
                (format!("确定删除项目「{name}」吗？"), *index)
            }
        };
        let mut yes = false;
        let mut no = false;
        egui::Window::new("确认")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label(message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("确定").clicked() {
                        yes = true;
                    }
                    if ui.button("取消").clicked() {
                        no = true;
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    yes = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    no = true;
                }
            });
        if yes {
            match dialog {
                ConfirmDialog::DeleteProject { index, .. } => self.confirm_delete(index),
            }
        } else if !no {
            self.confirm = Some(dialog);
        }
    }
}

impl Drop for ClientApp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl eframe::App for ClientApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 记录打开中的终端页签（退出后下次启动自动重新拉起）。
        self.config.tabs = config::TabsState {
            dirs: self
                .tabs
                .iter()
                .skip(1)
                .filter_map(|t| match t {
                    Tab::Session(s) if !s.exited => Some(s.dir.clone()),
                    _ => None,
                })
                .collect(),
            active: self.current,
        };
        // 窗口状态已在每帧 logic 中记录，退出时落盘。
        let _ = config::save(&self.config);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 消息驱动，不设基线定时器：重绘全部由事件触发——终端输出经 redraw
        // 通道推送、用户输入/窗口事件由 egui 自带、状态栏/更新检查/粘贴等
        // 都显式 request_repaint，空闲时零重绘。唯一例外：当前页签是终端且
        // 光标可见闪烁时，按闪烁半周期定时重绘（否则光标不闪）。
        // redraw 通道容量为 1（见 new），try_iter 把当前积压信号一次抽干：
        // 无论多少会话同时输出，每帧至多触发一次重绘，burst 结束后不会
        // 留下逐帧消化的“信号尾巴”，也不会在输出不停时无限堆积。
        let cursor_blinks = match self.tabs.get(self.current) {
            Some(Tab::Session(s)) => s
                .term
                .lock()
                .ok()
                .map(|t| {
                    t.mode()
                        .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR)
                        && t.cursor_style().blinking
                })
                .unwrap_or(false),
            _ => false,
        };
        if cursor_blinks {
            ctx.request_repaint_after(std::time::Duration::from_millis(400));
        }

        // 更新检查结果回到状态栏。
        if let Ok((msg, latest)) = self.update_rx.try_recv() {
            self.status = Some(msg);
            self.update_latest = latest;
            ctx.request_repaint();
        }

        let redraw = self.redraw_rx.try_iter().next().is_some();
        let exited = self.update_exited();
        if exited {
            self.status = Some("有会话已退出".to_string());
        }
        if redraw || exited {
            ctx.request_repaint();
        }

        // 记录窗口状态，退出时保存。
        let vp = ctx.input(|i| i.viewport().clone());
        self.config.window.maximized = vp.maximized.unwrap_or(false);
        if !self.config.window.maximized {
            if let Some(r) = vp.outer_rect {
                self.config.window.pos = Some([r.min.x, r.min.y]);
            }
            if let Some(r) = vp.inner_rect {
                self.config.window.size = Some([r.width(), r.height()]);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("tab_bar").show(ui, |ui| self.tab_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(Tab::Session(s)) = self.tabs.get_mut(self.current) {
                terminal::show_terminal(
                    ui,
                    s,
                    self.config.settings.dark_mode,
                    &mut self.status,
                    &mut self.term_focused,
                );
            } else {
                self.home_ui(ui);
            }
        });

        self.input_dialog(ui);
        self.confirm_dialog(ui);
    }
}