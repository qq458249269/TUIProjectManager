// 回归测试：忠实复制 app.rs::tab_bar（单一 Sense::click_and_drag 控件方案）的交互行为。
// 背景：egui dnd_drag_source 容器吞掉整块点击且 dragged 标志不可靠，改单控件后在此固化
// 点击激活 / 关闭 / 拖动重排 / 光标 四类行为。
// 注：集成测试无法链接 bin crate，故此处按 app.rs 逻辑复制一份最小实现。
// cargo test --test tab_bar
use eframe::egui;
use egui::{Color32, Context, Event, Modifiers, PointerButton, Pos2, Rect, Response, RichText, Sense};

#[derive(Clone)]
struct Sess {
    title: &'static str,
    dir: &'static str,
    exited: bool,
}

#[derive(Clone)]
enum Tab {
    Home,
    Session(Sess),
}

struct App {
    tabs: Vec<Tab>,
    current: usize,
    drag_tab: Option<usize>,
    rects: Vec<(usize, Rect)>,
    home_rect: Option<Rect>,
}

enum Action {
    Activate(usize),
    Close(usize),
}

fn tab_bg(sel: Color32, selected: bool, hovering: bool) -> Color32 {
    if selected {
        sel
    } else if hovering {
        Color32::from_white_alpha(25)
    } else {
        Color32::TRANSPARENT
    }
}

impl App {
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut actions: Vec<Action> = Vec::new();
        let sel_fill = ui.visuals().selection.bg_fill;
        let tab_margin = egui::Margin { left: 12, right: 8, top: 5, bottom: 5 };
        let mut tab_rects: Vec<(usize, egui::Rect)> = Vec::new();
        let mut drag_index: Option<usize> = None;
        self.rects.clear();

        ui.horizontal(|ui| {
            if let Some(Tab::Home) = self.tabs.first() {
                let selected = self.current == 0;
                let bg_idx = ui.painter().add(egui::Shape::Noop);
                let resp = egui::Frame::new()
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(tab_margin)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new("🏠 首页").strong()).selectable(false),
                        )
                    });
                let rect = resp.response.rect;
                // 交互层注册在内容之后：单击切回首页。不用 Label 自带 sense——
                // Frame 的 response 是另一个控件，读不到内层 label 的点击。
                let hit = ui.interact(rect, egui::Id::new("home_tab"), Sense::click());
                if hit.clicked() && !selected {
                    actions.push(Action::Activate(0));
                }
                // 首页是按钮：悬停显示小手。
                if hit.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                self.home_rect = Some(rect);
                let hovering = !selected
                    && ui.ctx().pointer_interact_pos().is_some_and(|p| rect.contains(p));
                let bg = tab_bg(sel_fill, selected, hovering);
                ui.painter().set(bg_idx, egui::Shape::rect_filled(rect, 0.0, bg));
            }

            for (i, tab) in self.tabs.iter().enumerate().skip(1) {
                if let Tab::Session(s) = tab {
                    ui.add_space(4.0);
                    let title = if s.exited { format!("{} (已退出)", s.title) } else { s.title.to_string() };
                    let selected = self.current == i;
                    let dir_key = s.dir;
                    let bg_idx = ui.painter().add(egui::Shape::Noop);
                    let (close_rect, frame_resp) = egui::Frame::new()
                        .fill(Color32::TRANSPARENT)
                        .inner_margin(tab_margin)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.add(
                                if selected {
                                    egui::Label::new(RichText::new(title).strong())
                                } else {
                                    egui::Label::new(RichText::new(title))
                                }
                                .selectable(false),
                            );
                            (ui.add(egui::Label::new("×").selectable(false)).rect, ui.response())
                        })
                        .inner;
                    let rect = frame_resp.rect;
                    let resp: Response = ui.interact(
                        rect,
                        egui::Id::new(("session_tab", i, dir_key)),
                        Sense::click_and_drag(),
                    );
                    if resp.dragged() {
                        drag_index = Some(i);
                    }
                    if resp.clicked() {
                        let pos = ui.ctx().pointer_interact_pos();
                        if pos.is_some_and(|p| close_rect.contains(p)) {
                            actions.push(Action::Close(i));
                        } else if !selected {
                            actions.push(Action::Activate(i));
                        }
                    }
                    // × 上悬停 → 小手（其余区域保持普通箭头）。
                    if resp.hovered()
                        && ui
                            .ctx()
                            .pointer_interact_pos()
                            .is_some_and(|p| close_rect.contains(p))
                    {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let hovering = !selected
                        && drag_index.is_none()
                        && ui.ctx().pointer_interact_pos().is_some_and(|p| rect.contains(p));
                    let bg = tab_bg(sel_fill, selected, hovering);
                    ui.painter().set(bg_idx, egui::Shape::rect_filled(rect, 0.0, bg));
                    tab_rects.push((i, rect));
                }
            }
        });
        self.rects.extend(tab_rects.iter().copied());

        if let Some(i) = drag_index {
            self.drag_tab = Some(i);
        }
        if let Some(from) = self.drag_tab {
            let pointer = ui.ctx().pointer_interact_pos();
            let mut target: Option<(usize, f32)> = None;
            if let Some(pos) = pointer {
                for (i, rect) in &tab_rects {
                    if rect.contains(pos) {
                        let bx = if pos.x < rect.center().x { rect.left() } else { rect.right() };
                        target = Some((*i, bx));
                        break;
                    }
                }
            }
            if let Some((_, bx)) = target {
                let area = ui.max_rect();
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(egui::pos2(bx - 1.0, area.top()), egui::pos2(bx + 1.0, area.bottom())),
                    0.0,
                    sel_fill,
                );
            }
            if ui.input(|i| i.pointer.any_released()) {
                self.drag_tab = None;
                if let Some((hov, _)) = target {
                    let p = match pointer.and_then(|pos| {
                        tab_rects.iter().find(|(ii, _)| *ii == hov).map(|(_, r)| (pos, *r))
                    }) {
                        Some((pos, r)) => {
                            if pos.x < r.center().x { hov } else { hov + 1 }
                        }
                        None => hov,
                    };
                    let new_p = if p > from { p - 1 } else { p };
                    let tab = self.tabs.remove(from);
                    self.tabs.insert(new_p, tab);
                    self.current = if from < self.current {
                        self.current - 1
                    } else if new_p <= self.current {
                        self.current + 1
                    } else {
                        self.current
                    };
                }
            }
        }

        for action in actions {
            match action {
                Action::Activate(i) => self.current = i,
                Action::Close(i) => {
                    self.tabs.remove(i);
                    if self.current >= i && self.current > 0 {
                        self.current -= 1;
                    }
                }
            }
        }
    }
}

fn new_app() -> App {
    App {
        tabs: vec![
            Tab::Home,
            Tab::Session(Sess { title: "aaa", dir: "d1", exited: false }),
            Tab::Session(Sess { title: "bbb", dir: "d2", exited: false }),
            Tab::Session(Sess { title: "ccc", dir: "d3", exited: false }),
        ],
        current: 0,
        drag_tab: None,
        rects: Vec::new(),
        home_rect: None,

    }
}

#[allow(clippy::type_complexity)]
fn frame(ctx: &Context, events: Vec<Event>, app: &mut App) -> egui::CursorIcon {
    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 60.0))),
        events,
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        ui.set_min_size(egui::vec2(800.0, 60.0));
        app.tab_bar(ui);
    });
    out.textures_delta.clear();
    out.platform_output.cursor_icon
}

fn btn(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::default(),
    }
}

fn titles(a: &App) -> Vec<String> {
    a.tabs
        .iter()
        .map(|t| match t {
            Tab::Home => "H".to_string(),
            Tab::Session(s) => s.title.to_string(),
        })
        .collect()
}

#[test]
fn click_close_drag_cursor() {
    let ctx = Context::default();
    let mut app = new_app();
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(10.0, 10.0))], &mut app);
    // 第二帧才有真实的 rect（ui.response() 基于上一帧）
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(10.0, 10.0))], &mut app);
    let (home, t1, t2, t3) = (
        app.home_rect.unwrap(),
        app.rects.iter().find(|(i, _)| *i == 1).unwrap().1,
        app.rects.iter().find(|(i, _)| *i == 2).unwrap().1,
        app.rects.iter().find(|(i, _)| *i == 3).unwrap().1,
    );

    // 0) 从会话切回首页：先激活 tab1，再点首页
    let mut h = new_app();
    frame(&ctx, vec![Event::PointerMoved(t1.center())], &mut h);
    frame(&ctx, vec![btn(t1.center(), true)], &mut h);
    frame(&ctx, vec![btn(t1.center(), false)], &mut h);
    assert_eq!(h.current, 1);
    frame(&ctx, vec![Event::PointerMoved(home.center())], &mut h);
    frame(&ctx, vec![btn(home.center(), true)], &mut h);
    let home_cur = frame(&ctx, vec![btn(home.center(), false)], &mut h);
    assert_eq!(h.current, 0, "点击首页应切回");
    assert_eq!(
        home_cur,
        egui::CursorIcon::PointingHand,
        "悬停首页应显示小手"
    );

    // 1) 点击 t2 → 激活
    let mut a = new_app();
    frame(&ctx, vec![Event::PointerMoved(t2.center())], &mut a);
    frame(&ctx, vec![btn(t2.center(), true)], &mut a);
    let cur = frame(&ctx, vec![btn(t2.center(), false)], &mut a);
    assert_eq!(a.current, 2, "点击应激活 tab2");
    assert_eq!(cur, egui::CursorIcon::Default, "点击时不展现 Grab");

    // 2) 悬停页签正文 → 普通光标（无 Grab）；悬停 × → 小手
    // 注意：hovered 标志在 headless 里滞后一帧，同位置的 move 各发两次。
    let cur = frame(&ctx, vec![Event::PointerMoved(t1.center())], &mut a);
    let cur = frame(&ctx, vec![Event::PointerMoved(t1.center())], &mut a);
    assert_eq!(cur, egui::CursorIcon::Default, "页签正文悬停为普通箭头");
    let xp_hover = egui::pos2(t1.right() - 3.5, t1.center().y);
    let cur = frame(&ctx, vec![Event::PointerMoved(xp_hover)], &mut a);
    let cur = frame(&ctx, vec![Event::PointerMoved(xp_hover)], &mut a);
    assert_eq!(
        cur,
        egui::CursorIcon::PointingHand,
        "悬停 × 上应显示小手"
    );
    let cur = frame(&ctx, vec![Event::PointerMoved(t3.center())], &mut a);
    let cur = frame(&ctx, vec![Event::PointerMoved(t3.center())], &mut a);
    assert_eq!(cur, egui::CursorIcon::Default, "其他页签正文仍为普通箭头");

    // 3) 点击 t1 的 × → 关闭 t1
    let mut b = new_app();
    frame(&ctx, vec![Event::PointerMoved(t1.center())], &mut b);
    // × 在右侧：右内边距 8，取 × 字形内部（≈ [right-7.3, right]）
    let xp = egui::pos2(t1.right() - 3.5, t1.center().y);
    frame(&ctx, vec![btn(xp, true)], &mut b);
    frame(&ctx, vec![btn(xp, false)], &mut b);
    assert_eq!(titles(&b), vec!["H", "bbb", "ccc"], "点击 × 应关闭该页签");

    // 4) 拖动 t2 → t1 左侧 → 重排
    let mut d = new_app();
    frame(&ctx, vec![Event::PointerMoved(t2.center())], &mut d);
    frame(&ctx, vec![btn(t2.center(), true)], &mut d);
    let m1 = t2.center() + egui::vec2(30.0, 25.0);
    let m2 = egui::pos2(t1.left() + 6.0, t1.center().y); // t1 左半，仍在行内
    frame(&ctx, vec![Event::PointerMoved(m1)], &mut d);
    frame(&ctx, vec![Event::PointerMoved(m2)], &mut d);
    frame(&ctx, vec![btn(m2, false)], &mut d);
    assert_eq!(titles(&d), vec!["H", "bbb", "aaa", "ccc"], "拖动应重排");
    assert_eq!(d.current, 0, "拖动不改选中");
}