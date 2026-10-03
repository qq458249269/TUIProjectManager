// 回归测试：会话页签区横向滚动（app.rs::tab_bar 的「只滚这一段」）。
// 注：app.rs 在 bin crate 里，集成测试链不上，故照抄 strip_geom / offset_to_show /
// 滚轮处理那段最小实现。背景：旧实现每帧用 offset_to_show 把当前页签抅回视野，
// 于是用户拨轮把当前页签（通常是左边那个）拨出视野后偏移被立刻复位 → 滚轮完全
// 失灵，看着就是「页签怎么都滚不动」。修复：滚轮接管时关掉跟随，切页/改宽度才
// 重新武装。滚动**不画滚动条**（自定义布局起点左移 + 裁剪）。
// cargo test --test tab_scroll
use eframe::egui;
use egui::{Color32, Context, Event, Pos2, Rect};

const TAB_GAP: f32 = 4.0;

#[derive(Clone, Copy)]
struct StripMetrics {
    slot_w: f32,
    close_w: f32,
    min_width: f32,
    gap: f32,
    pad: f32,
}

struct StripGeom {
    spans: Vec<(usize, f32, f32)>,
    content_w: f32,
}

fn strip_geom(items: &[(usize, f32, bool)], m: StripMetrics) -> StripGeom {
    let mut spans = Vec::with_capacity(items.len());
    let mut x = 0.0f32;
    for &(i, title_w, has_close) in items {
        if !spans.is_empty() {
            x += m.gap;
        }
        let inner = m.slot_w + TAB_GAP + title_w + if has_close { TAB_GAP + m.close_w } else { 0.0 };
        let min_w = m.min_width - if has_close { 0.0 } else { TAB_GAP };
        let w = inner.max(min_w) + m.pad;
        spans.push((i, x, w));
        x += w;
    }
    StripGeom { spans, content_w: x }
}

fn offset_to_show(off: f32, view_w: f32, content_w: f32, x: f32, w: f32) -> f32 {
    let max_off = (content_w - view_w).max(0.0);
    let off = off.clamp(0.0, max_off);
    if w >= view_w {
        return 0.0;
    }
    let shown = if x < off {
        x
    } else if x + w > off + view_w {
        x + w - view_w
    } else {
        off
    };
    shown.clamp(0.0, max_off)
}

// ── 视口两端的滚动箭头（照抄 app.rs::strip_arrows / strip_arrow_scroll / strip_arrow）──

const TAB_ARROW_W: f32 = 16.0;

fn strip_arrows(off: f32, view_w: f32, content_w: f32) -> (bool, bool) {
    let max_off = (content_w - view_w).max(0.0);
    if max_off <= 0.5 {
        return (false, false);
    }
    (off > 0.5, off < max_off - 0.5)
}

fn strip_arrow_scroll(off: f32, view_w: f32, content_w: f32, left: bool) -> f32 {
    let max_off = (content_w - view_w).max(0.0);
    let step = (view_w * 0.6).clamp(48.0, 220.0);
    (if left { off - step } else { off + step }).clamp(0.0, max_off)
}

fn strip_arrow(ui: &egui::Ui, view: egui::Rect, left: bool) -> bool {
    if view.width() < TAB_ARROW_W * 2.0 {
        return false;
    }
    let zone = if left {
        egui::Rect::from_min_max(
            view.left_top(),
            view.left_top() + egui::vec2(TAB_ARROW_W, view.height()),
        )
    } else {
        egui::Rect::from_min_max(
            view.right_top() - egui::vec2(TAB_ARROW_W, 0.0),
            view.right_top(),
        )
    };
    let bg = ui.visuals().panel_fill;
    ui.painter().add(egui::Shape::gradient_rect(
        zone,
        if left {
            egui::epaint::Direction::LeftToRight
        } else {
            egui::epaint::Direction::RightToLeft
        },
        [bg, Color32::TRANSPARENT],
    ));
    let c = egui::pos2(zone.center().x, view.center().y);
    let (hw, hh) = (3.5, 6.0);
    let tip = egui::pos2(c.x + if left { hw } else { -hw }, c.y);
    let tail = egui::pos2(c.x + if left { -hw } else { hw }, 0.0);
    let stroke = egui::Stroke::new(1.4, ui.visuals().weak_text_color());
    ui.painter()
        .add(egui::Shape::line_segment([tip, tail + egui::vec2(0.0, -hh)], stroke));
    ui.painter()
        .add(egui::Shape::line_segment([tip, tail + egui::vec2(0.0, hh)], stroke));
    ui.interact(zone, egui::Id::new(("tab_strip_arrow", left)), egui::Sense::click())
        .clicked_by(egui::PointerButton::Primary)
}

struct App {
    titles: Vec<String>,
    current: usize,
    tab_scroll_x: f32,
    tab_scroll_follow: bool,
    tab_scroll_follow_at: usize,
    tab_scroll_view_w: f32,
    cache: std::collections::HashMap<String, f32>,
    /// 本帧视口宽 / 内容宽 / 读到的位移，供断言与调试。
    view_log: (f32, f32, f32),
/// 首页、设置两个固定页签的矩形（断言设置与首页同尺寸）。
    fixed_rects: Vec<Rect>,
    /// 本帧两端箭头的显隐（左, 右）。
    arrows: (bool, bool),
    /// 本帧两端箭头的热区（左, 右；None = 未显示）。
    arrow_zones: [Option<Rect>; 2],
    /// 虚拟时钟：egui 的滚轮位移是「摊到几十帧」的平滑量，不推进时间就永远衰不
    /// 干净，测不出「拨完之后跟随重新武装」。
    now: f64,
    /// 窗口宽（改它就等于缩放窗口，测「视口变了要重新跟随」）。
    win_w: f32,
}

impl App {
    fn new(n: usize, current: usize) -> Self {
        App {
            titles: (0..n).map(|i| format!("项目-{i:02}")).collect(),
            current,
            tab_scroll_x: 0.0,
            tab_scroll_follow: true,
            tab_scroll_follow_at: current,
            tab_scroll_view_w: -1.0,
cache: Default::default(),
            view_log: (0.0, 0.0, 0.0),
            fixed_rects: Vec::new(),
            arrows: (false, false),
            arrow_zones: [None, None],
            now: 0.0,
            win_w: 800.0,
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let tab_font = egui::TextStyle::Body.resolve(ui.style());
        let tab_margin = egui::Margin { left: 2, right: 2, top: 2, bottom: 2 };
        let slot_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap("x".to_string(), tab_font.clone(), Color32::TRANSPARENT)
                .size()
                .x
        });
        let close_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap("×".to_string(), tab_font.clone(), Color32::TRANSPARENT)
                .size()
                .x
        });
        ui.horizontal(|ui| {
            // 首页 + 常驻设置页签：同款 Frame、只有文字、同样宽。
            self.fixed_rects.clear();
            for title in ["🏠 首页", "⚙ 设置"] {
                let rect = egui::Frame::new()
                    .corner_radius(4.0)
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(tab_margin)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(title).selectable(false));
                    })
                    .response
                    .rect;
                let _ = ui.interact(rect, egui::Id::new(title), egui::Sense::click());
                self.fixed_rects.push(rect);
            }
            ui.add_space(4.0);

            let avail = ui.available_rect_before_wrap();
            let view_w = avail.width().max(0.0);
            let m = StripMetrics {
                slot_w,
                close_w,
                min_width: ui.text_style_height(&egui::TextStyle::Body) * 4.0,
                gap: ui.spacing().item_spacing.x + TAB_GAP,
                pad: tab_margin.left as f32 + tab_margin.right as f32,
            };
            let mut items: Vec<(usize, f32, bool)> = Vec::new();
            for (i, t) in self.titles.clone().into_iter().enumerate() {
                items.push((i + 2, self.title_w(ui, &t, &tab_font), true));
            }
            let geom = strip_geom(&items, m);
            let view = egui::Rect::from_min_size(avail.min, egui::vec2(view_w, avail.height()));
            let max_off = (geom.content_w - view_w).max(0.0);
            if self.current != self.tab_scroll_follow_at || view_w != self.tab_scroll_view_w {
                self.tab_scroll_follow = true;
                self.tab_scroll_follow_at = self.current;
                self.tab_scroll_view_w = view_w;
            }
            let mut off = self.tab_scroll_x.clamp(0.0, max_off);
            let scroll = ui.input(|i| i.smooth_scroll_delta.y + i.smooth_scroll_delta.x);
            if scroll != 0.0 && ui.ctx().pointer_interact_pos().is_some_and(|p| view.contains(p)) {
                off = (off - scroll).clamp(0.0, max_off);
                self.tab_scroll_follow = false;
                ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
            }
            if self.tab_scroll_follow {
                off = match geom.spans.iter().find(|(i, _, _)| *i == self.current) {
                    Some((_, x, w)) => offset_to_show(off, view_w, geom.content_w, *x, *w),
                    None => off,
                };
            }
            self.tab_scroll_x = off;
            self.view_log = (view_w, geom.content_w, scroll);
            let content_rect = egui::Rect::from_min_max(
                egui::pos2(view.left() - off, view.top()),
                egui::pos2(view.left() - off + geom.content_w.max(view_w), view.bottom()),
            );
            let mut ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("tab_strip")
                    .max_rect(content_rect)
                    .layout(
                        egui::Layout::left_to_right(egui::Align::Center)
                            .with_cross_align(egui::Align::Center),
                    ),
            );
            ui.set_clip_rect(view);
            for t in self.titles.clone() {
                ui.add_space(TAB_GAP);
                let title_w = self.title_w(&ui, &t, &tab_font);
                let _ = egui::Frame::new()
                    .corner_radius(4.0)
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(tab_margin)
                    .show(&mut ui, |ui| {
                        ui.spacing_mut().item_spacing.x = TAB_GAP;
                        let min_width = ui.text_style_height(&egui::TextStyle::Body) * 4.0;
                        let s = TAB_GAP;
                        let icon_title_w = slot_w + s + title_w;
                        let slack = (min_width - icon_title_w - s - close_w).max(0.0);
                        let (pad_l, pad_m) = if slack > 0.0 && slack >= close_w + s {
                            ((slack + close_w + s) / 2.0, (slack - close_w - s) / 2.0)
                        } else if slack > 0.0 {
                            (slack, 0.0)
                        } else {
                            (0.0, 0.0)
                        };
                        if pad_l > 0.0 {
                            ui.add_space(pad_l);
                        }
                        ui.add_sized(egui::vec2(slot_w, 12.0), egui::Label::new(" ").selectable(false));
                        ui.add(egui::Label::new(t.as_str()).selectable(false));
                        if pad_m > 0.0 {
                            ui.add_space(pad_m);
                        }
                        ui.add(egui::Label::new("×").selectable(false));
                    });
            }
            // 视口两端的滚动箭头（照抄 app.rs：页签之后画，才盖得住半截页签）。
            self.arrows = strip_arrows(off, view_w, geom.content_w);
            let mut arrow = None;
            if self.arrows.0 && strip_arrow(&ui, view, true) {
                arrow = Some(true);
            }
            if self.arrows.1 && strip_arrow(&ui, view, false) {
                arrow = Some(false);
            }
            if let Some(left) = arrow {
                self.tab_scroll_x = strip_arrow_scroll(off, view_w, geom.content_w, left);
                self.tab_scroll_follow = false;
            }
            self.arrow_zones = [
                self.arrows.0.then(|| egui::Rect::from_min_max(
                    view.left_top(),
                    view.left_top() + egui::vec2(TAB_ARROW_W, view.height()),
                )),
                self.arrows.1.then(|| egui::Rect::from_min_max(
                    view.right_top() - egui::vec2(TAB_ARROW_W, 0.0),
                    view.right_top(),
                )),
            ];
        });
    }

    fn title_w(&mut self, ui: &egui::Ui, t: &str, font: &egui::FontId) -> f32 {
        *self.cache.entry(t.to_string()).or_insert_with(|| {
            ui.ctx().fonts_mut(|f| {
                f.layout_no_wrap(t.to_string(), font.clone(), Color32::TRANSPARENT)
                    .size()
                    .x
            })
        })
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
    app.now += 0.05;
    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(app.win_w, 60.0))),
        predicted_dt: 0.05,
        time: Some(app.now),
        events,
        ..Default::default()
    };
    let win_w = app.win_w;
    let mut out = ctx.run_ui(raw, |ui| {
        ui.set_min_size(egui::vec2(win_w, 60.0));
        app.tab_bar(ui);
    });
    out.textures_delta.clear();
}

fn wheel(dx: f32, dy: f32) -> Event {
    Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: egui::vec2(dx, dy),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::default(),
    }
}

/// 在会话页签区里拨 n 次滚轮（指针落在页签行右侧的空白/页签上）。
fn scroll_wheel(ctx: &Context, app: &mut App, dy: f32, n: usize) {
    for _ in 0..n {
        frame(ctx, vec![wheel(0.0, dy)], app);
        settle(ctx, app);
    }
}

/// 推进若干空帧直到平滑滚轮位移衰干净（egui 把它摊到 ~50ms）。
fn settle(ctx: &Context, app: &mut App) {
    for _ in 0..200 {
        frame(ctx, vec![], app);
        if app.view_log.2 == 0.0 {
            return;
        }
    }
}

#[test]
fn wheel_scrolls_strip_even_when_active_tab_is_leftmost() {
    let ctx = font_ctx();
    // 当前页签 = 第一个会话页签（旧实现下拨轮恒被抅回 0 = 完全滚不动）。
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    assert!(app.view_log.1 > app.view_log.0, "会话区必须比视口宽才有得滚");

    scroll_wheel(&ctx, &mut app, -3.0, 4);
    let right = app.tab_scroll_x;
    assert!(right > 50.0, "滚轮向下应把页签条往左滚，实际 {right}");

    scroll_wheel(&ctx, &mut app, 3.0, 8);
    assert!(
        app.tab_scroll_x < right - 50.0,
        "反向拨轮应滚回来，实际 {} → {}",
        right,
        app.tab_scroll_x
    );
    // 拨轮期间不再抢镜头（这正是旧 bug）。
    assert!(!app.tab_scroll_follow);
}

#[test]
fn tab_change_rearms_follow() {
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    scroll_wheel(&ctx, &mut app, -3.0, 4);
    let scrolled = app.tab_scroll_x;
    assert!(scrolled > 50.0, "先拨轮滚到后面");
    settle(&ctx, &mut app);
    assert!(!app.tab_scroll_follow, "拨轮后不该再抢镜头");

    // 切到最后一个页签 → 重新武装跟随 → 它被滚进视野（贴视口右缘）。
    app.current = 15;
    frame(&ctx, vec![], &mut app);
    let (view_w, content_w, _) = app.view_log;
    assert!(app.tab_scroll_follow, "切页后应重新跟随");
    assert!(
        app.tab_scroll_x >= content_w - view_w - 1.0,
        "当前页签应在视野内（off={} 最大={}）",
        app.tab_scroll_x,
        content_w - view_w
    );
}

#[test]
fn resize_rearms_follow() {
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    // 拨轮滚到很靠后的位置（此时窗口宽，当前页签已被拨出视野）。
    scroll_wheel(&ctx, &mut app, -3.0, 6);
    settle(&ctx, &mut app);
    assert!(!app.tab_scroll_follow);
    assert!(app.tab_scroll_x > 100.0);

    // 窗口缩窄 → 视口变了 → 重新跟随，把当前页签（最左那个）拽回视野。
    app.win_w = 400.0;
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(350.0, 12.0))], &mut app);
    assert!(app.tab_scroll_follow, "视口变化后应重新跟随");
    assert!(
        app.tab_scroll_x <= 1.0,
        "当前页签应回到最左，实际 off={}",
        app.tab_scroll_x
    );
}

#[test]
fn settings_tab_matches_home_tab_size() {
    let ctx = font_ctx();
    let mut app = App::new(4, 0);
    frame(&ctx, vec![], &mut app);
    frame(&ctx, vec![], &mut app);
    let (home, settings) = (app.fixed_rects[0], app.fixed_rects[1]);
    assert_eq!(home.height(), settings.height(), "设置页签高度应与首页一致");
    assert!(
        (home.width() - settings.width()).abs() < 1.0,
        "设置页签宽度应与首页一致：{} vs {}",
        home.width(),
        settings.width()
    );
    // 设置页签固定在首页右边，不参与会话区滚动。
    assert!(settings.left() > home.right());
}

// ── 视口两端的滚动箭头 ──

fn press(pos: Pos2) -> Event {
    Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
    }
}

fn release(pos: Pos2) -> Event {
    Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
    }
}

/// 在第 idx 端箭头（0 左 / 1 右）的热区中心点一下。
fn click_arrow(ctx: &Context, app: &mut App, idx: usize) {
    let z = app.arrow_zones[idx].expect("该端箭头这一帧应该是显示的");
    let c = z.center();
    frame(ctx, vec![press(c)], app);
    frame(ctx, vec![release(c)], app);
}

#[test]
fn no_arrow_when_everything_fits() {
    let ctx = font_ctx();
    // 3 个页签远小于视口：压根没得滚，两端都不该挂箭头（否则是假提示）。
    let mut app = App::new(3, 2);
    frame(&ctx, vec![], &mut app);
    frame(&ctx, vec![], &mut app);
    assert_eq!(app.arrows, (false, false), "放得下就不该有滚动箭头");
}

#[test]
fn arrow_follows_scroll_position() {
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    let (view_w, content_w, _) = app.view_log;
    let max_off = content_w - view_w;
    assert!(max_off > 100.0, "得有得滚才有箭头可言");
    // 在最左：只有右箭头。
    assert_eq!(app.arrows, (false, true));

    // 滚到中间：两端都有。
    app.tab_scroll_x = max_off * 0.5;
    app.tab_scroll_follow = false;
    frame(&ctx, vec![], &mut app);
    assert_eq!(app.arrows, (true, true));

    // 滚到最右：左箭头留着，右箭头收掉（抵到头了还提示「还能往右」是骗人）。
    app.tab_scroll_x = max_off;
    frame(&ctx, vec![], &mut app);
    assert_eq!(app.arrows, (true, false));
}

#[test]
fn arrow_hidden_by_float_residue_at_end() {
    // 纯函数：抵到两端时 off 与 max_off 常差最后一点浮点残差，
    // 没有 0.5px 容差箭头就赖着不走（app.rs 里 strip_arrows 的存在理由）。
    let (view_w, content_w) = (400.0, 700.0);
    let max_off = content_w - view_w;
    assert_eq!(strip_arrows(max_off - 1e-4, view_w, content_w), (true, false));
    assert_eq!(strip_arrows(1e-4, view_w, content_w), (false, true));
    assert_eq!(strip_arrows(0.0, view_w, content_w), (false, true));
    // 内容不宽于视口 → 一律不出箭头。
    assert_eq!(strip_arrows(0.0, 400.0, 400.0), (false, false));
    assert_eq!(strip_arrows(12.0, 400.0, 380.0), (false, false));
}

#[test]
fn arrow_click_scrolls_by_a_page() {
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
let (view_w, _content_w, _) = app.view_log;
    assert_eq!(app.tab_scroll_x, 0.0);

    // 点右箭头 → 往右翻大半屏；同时别把镜头抢回当前页签（否则点了就弹回去）。
    click_arrow(&ctx, &mut app, 1);
    let after_right = app.tab_scroll_x;
    assert!(
        after_right >= 48.0 && after_right <= view_w * 0.6 + 1.0,
        "点右箭头应翻大半屏，实际 {after_right}"
    );
    assert!(!app.tab_scroll_follow, "用户自己在翻页，别再抢镜头");
    frame(&ctx, vec![], &mut app);
    assert!(app.tab_scroll_x >= after_right - 1.0, "翻过去的偏移不该被复位");
    assert_eq!(app.arrows, (true, true));

    // 点左箭头 → 原路翻回来。
    click_arrow(&ctx, &mut app, 0);
    assert!(
        app.tab_scroll_x < after_right - 1.0,
        "点左箭头应往回翻，实际 {} → {}",
        after_right,
        app.tab_scroll_x
    );
}

#[test]
fn arrow_click_lands_on_end() {
    // 一步迈不到底：连点几下要能贴到两端，而不是在半路磨。
    let (view_w, content_w) = (300.0, 700.0);
    let mut off = 0.0;
    for _ in 0..20 {
        off = strip_arrow_scroll(off, view_w, content_w, false);
    }
    assert!((off - (content_w - view_w)).abs() < 0.01, "应贴到最右，实际 {off}");
    for _ in 0..20 {
        off = strip_arrow_scroll(off, view_w, content_w, true);
    }
    assert!(off.abs() < 0.01, "应贴到最左，实际 {off}");
}

#[test]
fn arrow_zone_beats_the_half_tab_underneath() {
    // 箭头画在页签之后并自带热区：点在箭头上不能激活/拖动下面那个半截页签，
    // 否则「点箭头」会被解读成「点那个页签」。这里盯住滚动确实发生了。
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    assert_eq!(app.current, 2, "起始当前页签不变");
    click_arrow(&ctx, &mut app, 1);
    assert!(app.tab_scroll_x > 0.0, "箭头热区应吃掉这次点击（滚动了）");
    assert_eq!(app.current, 2, "点箭头不该切页");
}

