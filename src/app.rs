use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;
use egui::{Color32, RichText};

use crate::config;
use crate::session::{self, Session};
use crate::terminal::{self, TermCommand};

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
    pub prefix_active: bool,
    pub term_focused: bool,
    pub input: Option<InputDialog>,
    pub confirm: Option<ConfirmDialog>,
    redraw_tx: Sender<()>,
    redraw_rx: Receiver<()>,
}

impl ClientApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let config = config::load();
        let config_path = config::config_path();
        let settings_command = config.settings.tui_command.clone();
        let settings_commands = config.settings.tui_commands.clone();
        let (redraw_tx, redraw_rx) = std::sync::mpsc::channel();
        Self {
            config,
            tabs: vec![Tab::Home],
            current: 0,
            screen: Screen::Main,
            selected_project: 0,
            settings_command,
            settings_commands,
            settings_new_command: String::new(),
            status: Some("在左侧选择项目并点击「启动」，或在终端页签中按 Ctrl+B 管理页签。".to_string()),
            config_path,
            prefix_active: false,
            term_focused: false,
            input: None,
            confirm: None,
            redraw_tx,
            redraw_rx,
        }
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

    fn go_home(&mut self) {
        self.current = 0;
        self.screen = Screen::Main;
        self.term_focused = false;
    }

    fn refresh_focus(&mut self) {
        self.term_focused = !matches!(self.tabs.get(self.current), Some(Tab::Home));
        if !self.term_focused {
            self.screen = Screen::Main;
        }
    }

    fn next_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.current = (self.current + 1) % self.tabs.len();
        }
        self.refresh_focus();
    }

    fn prev_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.current = (self.current + self.tabs.len() - 1) % self.tabs.len();
        }
        self.refresh_focus();
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
                self.status = Some(format!("已启动: {}  (Ctrl+B 管理页签)", project.name));
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

    fn send_raw(&mut self, bytes: Vec<u8>) {
        let Some(Tab::Session(s)) = self.tabs.get_mut(self.current) else {
            return;
        };
        if let Ok(mut w) = s.writer.lock() {
            let _ = w.write_all(&bytes);
        }
    }

    fn apply_term_commands(&mut self, cmds: Vec<TermCommand>) {
        for c in cmds {
            match c {
                TermCommand::GoHome => self.go_home(),
                TermCommand::NextTab => self.next_tab(),
                TermCommand::PrevTab => self.prev_tab(),
                TermCommand::CloseTab => self.close_session(self.current),
                TermCommand::SendCtrlB => self.send_raw(vec![0x02]),
            }
        }
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
                    let title = if s.exited {
                        format!("{} (已退出)", s.title)
                    } else {
                        s.title.clone()
                    };
                    let selected = self.current == i;
                    if ui.selectable_label(selected, title).clicked() && !selected {
                        actions.push(TabAction::Activate(i));
                    }
                    if ui.small_button("×").on_hover_text("关闭会话").clicked() {
                        actions.push(TabAction::Close(i));
                    }
                }
            }
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
                Some(s) => (s.clone(), Color32::from_rgb(200, 180, 60)),
                None => match self.tabs.get(self.current) {
                    Some(Tab::Session(s)) => (
                        format!(
                            "会话 {} / {}  项目: {}   |  Ctrl+B 页签菜单 (h 首页 n 下一个 p 上一个 x 关闭)",
                            self.current,
                            self.tabs.len() - 1,
                            s.title
                        ),
                        Color32::GRAY,
                    ),
                    _ => (
                        "选择项目 → 启动（内嵌终端页签）   |   添加 / 重命名 / 改路径 / 删除 / 设置"
                            .to_string(),
                        Color32::GRAY,
                    ),
                },
            };
            ui.label(RichText::new(text).color(color));
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
                                Color32::from_rgb(220, 170, 60)
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
            ui.add_space(6.0);
            ui.label(
                RichText::new("终端页签内按 Ctrl+B 进入页签管理：h 首页  n 下一个  p 上一个  x 关闭  b 发送 Ctrl+B")
                    .weak(),
            );
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
            .color(if exists {
                Color32::from_rgb(90, 200, 90)
            } else {
                Color32::from_rgb(200, 160, 60)
            }),
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
                            resp.request_focus();
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
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let redraw = self.redraw_rx.try_recv().is_ok();
        let exited = self.update_exited();
        if exited {
            self.status = Some("有会话已退出".to_string());
        }
        if redraw || exited {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("tab_bar").show(ui, |ui| self.tab_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(Tab::Session(s)) = self.tabs.get_mut(self.current) {
                let cmds = terminal::show_terminal(
                    ui,
                    s,
                    &mut self.prefix_active,
                    &mut self.status,
                    &mut self.term_focused,
                );
                self.apply_term_commands(cmds);
            } else {
                self.home_ui(ui);
            }
        });

        self.input_dialog(ui);
        self.confirm_dialog(ui);
    }
}