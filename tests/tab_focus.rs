// 回归测试：终端聚焦时按 Tab/方向键，egui 焦点遍历不得把键盘焦点交给页签栏；
// 随后的 Enter/Space 也不得触发页签的「键盘点击」（切页/关页）。
// 背景：egui 0.36 起 Tab/方向键/Esc 默认被焦点遍历接管（EventFilter 默认全 false），
// 焦点一旦落到页签栏控件，Enter/Space 会被 get_response 记成 FAKE_PRIMARY_CLICKED
// → Response::clicked() 为真 → 页签被键盘「点击」→ 切到首页。
// 注：集成测试无法链接 bin crate，故此处按 app.rs/terminal.rs 逻辑复制最小实现。
// cargo test --test tab_focus
use eframe::egui;
use egui::{
    Color32, Context, Event, Key, Modifiers, PointerButton, Pos2, Rect, RichText, Sense, Vec2,
};

#[derive(Clone)]
enum Tab {
    Home,
    Session(&'static str),
}

struct App {
    tabs: Vec<Tab>,
    current: usize,
    term_focused: bool,
    term_rect: Option<Rect>,
    term_id: Option<egui::Id>,
    /// 复现开关：false = 修复前行为（无焦点锁、clicked() 认键盘点击）。
    lock_focus: bool,
    pointer_only_click: bool,
    /// 本帧终端收到的按键（对应 terminal.rs 写回 PTY 的那一次事件遍历）。
    keys_seen: Vec<Key>,
}

impl App {
    fn ui(&mut self, ui: &mut egui::Ui) {
        // ── 顶部页签栏（与 app.rs::tab_bar 同构）──
        ui.horizontal(|ui| {
            if let Some(Tab::Home) = self.tabs.first() {
                let selected = self.current == 0;
                let resp = egui::Frame::new()
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(egui::Margin::same(2))
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new("Home")).selectable(false));
                    });
                let rect = resp.response.rect;
                let hit = ui.interact(rect, egui::Id::new("home_tab"), Sense::click());
                if self.clicked(&hit) && !selected {
                    self.current = 0;
                }
            }
            for (i, tab) in self.tabs.iter().enumerate().skip(1) {
                if let Tab::Session(title) = tab {
                    let selected = self.current == i;
                    let resp = egui::Frame::new()
                        .fill(Color32::TRANSPARENT)
                        .inner_margin(egui::Margin::same(2))
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(*title).strong()).selectable(false),
                            );
                        });
                    let rect = resp.response.rect;
                    let r = ui.interact(
                        rect,
                        egui::Id::new(("session_tab", i)),
                        Sense::click_and_drag(),
                    );
                    if self.clicked(&r) && !selected {
                        self.current = i;
                    }
                }
            }
        });

        // ── 中央面板：终端（与 terminal.rs::show_terminal 同构）──
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        self.term_rect = Some(rect);
        self.term_id = Some(resp.id);
        let term_id = resp.id;
        if resp.clicked_by(PointerButton::Primary) {
            self.term_focused = true;
            resp.request_focus();
        }
        if self.term_focused && self.lock_focus {
            // 焦点锁在终端：独占 Tab/方向键/Esc；焦点被抢走时当帧拽回。
            if ui.memory(|m| m.focused()) != Some(term_id) {
                resp.request_focus();
            }
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    term_id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }
        // 终端把按键原样写回 PTY：焦点锁不能吞掉任何事件。
        if self.term_focused {
            self.keys_seen = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        Event::Key { key, pressed: true, .. } => Some(*key),
                        _ => None,
                    })
                    .collect()
            });
        }
    }

    fn clicked(&self, r: &egui::Response) -> bool {
        if self.pointer_only_click {
            r.clicked_by(PointerButton::Primary)
        } else {
            r.clicked()
        }
    }
}

fn new_app(lock_focus: bool, pointer_only_click: bool) -> App {
    App {
        tabs: vec![Tab::Home, Tab::Session("aaa"), Tab::Session("bbb")],
        current: 1,
        term_focused: true,
        term_rect: None,
        term_id: None,
        lock_focus,
        pointer_only_click,
        keys_seen: Vec::new(),
    }
}

fn font_ctx() -> Context {
    let ctx = Context::default();
    ctx.add_font(egui::epaint::text::FontInsert::new(
        "hack",
        egui::epaint::text::FontData::from_static(epaint_default_fonts::HACK_REGULAR),
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
    ctx
}

fn frame(ctx: &Context, events: Vec<Event>, app: &mut App) {
    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
        events,
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| app.ui(ui));
    out.textures_delta.clear();
}

fn key(key: Key, modifiers: Modifiers) -> Event {
    Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}

fn btn(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::default(),
    }
}

fn focus_in_terminal(ctx: &Context, app: &mut App) {
    frame(ctx, vec![], app);
    let c = app.term_rect.unwrap().center();
    frame(ctx, vec![Event::PointerMoved(c), btn(c, true)], app);
    frame(ctx, vec![btn(c, false)], app);
    frame(ctx, vec![], app);
    assert_eq!(
        ctx.memory(|m| m.focused()),
        app.term_id,
        "点击终端应拿到 egui 焦点"
    );
}

/// 复现（修复前）：Tab 把焦点交给页签栏，随后的 Enter 被当成首页的点击。
/// 这条用例钉住的是「egui 默认 EventFilter 不独占 Tab」这一前提：哪天 egui 自己
/// 改了这套行为，本用例失败即说明焦点锁可以简化，而不是回归。
#[test]
fn no_focus_lock_lets_tab_escape_focus() {
    let ctx = font_ctx();
    let mut app = new_app(false, false);
    focus_in_terminal(&ctx, &mut app);
    frame(&ctx, vec![key(Key::Tab, Modifiers::default())], &mut app);
    frame(&ctx, vec![], &mut app);
    assert_ne!(
        ctx.memory(|m| m.focused()),
        app.term_id,
        "（前置确认）无焦点锁时 Tab 会把焦点交出终端"
    );
    frame(&ctx, vec![key(Key::Enter, Modifiers::default())], &mut app);
    assert_eq!(app.current, 0, "（前置确认）回车应把页签切到首页");
}

/// 终端聚焦时 Tab + 回车：焦点留在终端，页签不被键盘切走。
#[test]
fn tab_then_enter_keeps_terminal() {
    let ctx = font_ctx();
    let mut app = new_app(true, true);
    focus_in_terminal(&ctx, &mut app);
    frame(&ctx, vec![key(Key::Tab, Modifiers::default())], &mut app);
    assert_eq!(
        ctx.memory(|m| m.focused()),
        app.term_id,
        "Tab 后 egui 焦点必须仍在终端上"
    );
    assert_eq!(app.keys_seen, vec![Key::Tab], "Tab 事件必须仍写回 PTY");
    frame(&ctx, vec![key(Key::Enter, Modifiers::default())], &mut app);
    assert_eq!(app.keys_seen, vec![Key::Enter], "回车事件必须仍写回 PTY");
    assert_eq!(app.current, 1, "回车不得把页签切到首页");
}

/// 方向键同理：终端里按 ↑（命令历史）不得把焦点交给页签栏。
#[test]
fn arrow_keys_keep_terminal_focus() {
    let ctx = font_ctx();
    let mut app = new_app(true, true);
    focus_in_terminal(&ctx, &mut app);
    for k in [Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight] {
        frame(&ctx, vec![key(k, Modifiers::default())], &mut app);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            app.term_id,
            "{k:?} 后 egui 焦点必须仍在终端上"
        );
        assert_eq!(app.keys_seen, vec![k], "{k:?} 事件必须仍写回 PTY");
    }
    frame(&ctx, vec![key(Key::Enter, Modifiers::default())], &mut app);
    assert_eq!(app.current, 1, "方向键后再回车也不得切页");
}

/// Esc 不应让终端丢焦点（子进程要用 Esc 取消斜杠命令）。
#[test]
fn escape_keeps_terminal_focus() {
    let ctx = font_ctx();
    let mut app = new_app(true, true);
    focus_in_terminal(&ctx, &mut app);
    frame(&ctx, vec![key(Key::Escape, Modifiers::default())], &mut app);
    assert_eq!(
        ctx.memory(|m| m.focused()),
        app.term_id,
        "Esc 后 egui 焦点必须仍在终端上"
    );
    assert_eq!(app.keys_seen, vec![Key::Escape], "Esc 事件必须仍写回 PTY");
}

/// 兜底：即使焦点被别的路径抢到页签栏，Enter/Space 也不得切页/关页。
#[test]
fn keyboard_click_never_switches_tab() {
    let ctx = font_ctx();
    let mut app = new_app(true, true);
    focus_in_terminal(&ctx, &mut app);
    // 人为把焦点塞到首页页签上（模拟旧的焦点遍历结果）。
    let home_id = egui::Id::new("home_tab");
    frame(&ctx, vec![key(Key::Tab, Modifiers::default())], &mut app);
    ctx.memory_mut(|m| m.request_focus(home_id));
    assert_eq!(ctx.memory(|m| m.focused()), Some(home_id));
    for k in [Key::Enter, Key::Space] {
        frame(&ctx, vec![key(k, Modifiers::default())], &mut app);
        assert_eq!(app.current, 1, "{k:?} 不得切页");
    }
    assert_eq!(app.tabs.len(), 3, "键盘不得关页签");
}
