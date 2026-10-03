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

// ── 视口两端的滚动按钮（照抄 app.rs::strip_btn_reserve / strip_arrows /
// strip_arrow_scroll / strip_scroll_btn）──

/// 按钮**占布局位**的槽宽；不叠在页签上（旧实现 TAB_ARROW_W=16
/// 的渐变底衫正是叠在视口上缘、吃掉半截页签）。
const TAB_SCROLL_BTN_W: f32 = 18.0;
/// 视口窄到这个数以内就不预留按钮（宁可只剩滚轮，也不能把
/// 页签挤没了）。
const TAB_SCROLL_MIN_STRIP_W: f32 = 48.0;

fn strip_btn_reserve(view_w: f32, content_w: f32) -> f32 {
    let two = TAB_SCROLL_BTN_W * 2.0;
    if view_w < TAB_SCROLL_MIN_STRIP_W + two {
        return 0.0;
    }
    if content_w > view_w - two {
        two
    } else {
        0.0
    }
}

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

fn strip_scroll_btn(ui: &egui::Ui, rect: egui::Rect, left: bool, enabled: bool) -> bool {
    let hovered = enabled && ui.rect_contains_pointer(rect);
    if hovered {
        ui.painter()
            .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.bg_fill);
    }
    let color = if !enabled {
        ui.visuals().weak_text_color().gamma_multiply(0.35)
    } else if hovered {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        if left { "◀" } else { "▶" },
        egui::TextStyle::Body.resolve(ui.style()),
        color,
    );
    let resp = ui.interact(
        rect,
        egui::Id::new(("tab_strip_scroll_btn", left)),
        egui::Sense::click(),
    );
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    enabled && resp.clicked_by(egui::PointerButton::Primary)
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
    /// 本帧两端按钮的可点性（左, 右）。
    btns: (bool, bool),
    /// 本帧两端按钮的槽位（左, 右；None = 未预留）。
    btn_slots: [Option<Rect>; 2],
    /// 本帧页签视口（两端按钮取位之间）。
    view_rect: Rect,
    /// 本帧页签的矩形（测「按钮不挡页签」用）。
    tab_rects: Vec<Rect>,
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
            btns: (false, false),
            btn_slots: [None, None],
            view_rect: Rect::NOTHING,
            tab_rects: Vec::new(),
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
            let full_w = avail.width().max(0.0);
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
            // 两端按钮占位（不叠页签）：视口从左按钮右侧开始。
            let btn_reserve = strip_btn_reserve(full_w, geom.content_w);
            let view_w = (full_w - btn_reserve).max(0.0);
            let view = egui::Rect::from_min_size(
                egui::pos2(avail.left() + btn_reserve * 0.5, avail.min.y),
                egui::vec2(view_w, avail.height()),
            );
            let btn_left = egui::Rect::from_min_size(
                avail.left_top(),
                egui::vec2(TAB_SCROLL_BTN_W, avail.height()),
            );
            let btn_right = egui::Rect::from_min_max(
                egui::pos2(avail.right() - TAB_SCROLL_BTN_W, avail.top()),
                avail.right_bottom(),
            );
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
            self.view_rect = view;
            // 按钮画在页签**之前**（用外层 ui，不是下面 clip 到 view 的 shadow
            // child）：与页签矩形互不相交，既不遮也不抢命中。
            if btn_reserve > 0.0 {
                self.btns = strip_arrows(off, view_w, geom.content_w);
                let mut btn = None;
                if strip_scroll_btn(ui, btn_left, true, self.btns.0) {
                    btn = Some(true);
                }
                if strip_scroll_btn(ui, btn_right, false, self.btns.1) {
                    btn = Some(false);
                }
                if let Some(left) = btn {
                    self.tab_scroll_x = strip_arrow_scroll(off, view_w, geom.content_w, left);
                    self.tab_scroll_follow = false;
                }
            } else {
                self.btns = (false, false);
            }
            self.btn_slots = [
                (btn_reserve > 0.0).then_some(btn_left),
                (btn_reserve > 0.0).then_some(btn_right),
            ];
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
            self.tab_rects.clear();
            for t in self.titles.clone() {
                ui.add_space(TAB_GAP);
                let title_w = self.title_w(&ui, &t, &tab_font);
                let rect = egui::Frame::new()
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
                    })
                    .response
                    .rect;
                self.tab_rects.push(rect);
            }
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

/// 在第 idx 端按钮（0 左 / 1 右）的槽位中心点一下。
fn click_scroll_btn(ctx: &Context, app: &mut App, idx: usize) {
    let z = app.btn_slots[idx].expect("该端按钮这一帧应该是预留着的");
    let c = z.center();
    frame(ctx, vec![press(c)], app);
    frame(ctx, vec![release(c)], app);
}

#[test]
fn no_scroll_btn_when_everything_fits() {
    let ctx = font_ctx();
    // 3 个页签远小于视口：根本没得滚，两端都不该预留按钮槽（否则既占了地方
    // 又是假提示）。
    let mut app = App::new(3, 2);
    frame(&ctx, vec![], &mut app);
    frame(&ctx, vec![], &mut app);
    assert_eq!(app.btn_slots, [None, None], "放得下就不该有滚动按钮");
    assert_eq!(app.btns, (false, false));
}

#[test]
fn btn_reserve_only_when_needed() {
    // 纯函数：预留判据是「扣掉按钮仍放不下」。
    let two = TAB_SCROLL_BTN_W * 2.0;
    // 宽度余量（扣掉按钮还宽很多）→ 不预留。
    assert_eq!(strip_btn_reserve(400.0, 300.0), 0.0);
    // 宽度刚好占满两个按钮位 → 不预留（否则留下两个死按钮）。
    assert_eq!(strip_btn_reserve(400.0, 400.0 - two), 0.0);
    // 超出一丁点 → 预留两端。
    assert_eq!(strip_btn_reserve(400.0, 400.0 - two + 1.0), two);
    assert_eq!(strip_btn_reserve(400.0, 900.0), two);
    // 视口窄到连按钮位都摆不下 → 不预留（保页签，保滚轮）。
    assert_eq!(strip_btn_reserve(TAB_SCROLL_MIN_STRIP_W + two - 0.5, 900.0), 0.0);
    assert_eq!(strip_btn_reserve(TAB_SCROLL_MIN_STRIP_W + two, 900.0), two);
}

#[test]
fn scroll_btns_never_cover_tabs() {
    // 核心要求：按钮只占布局位，绝不叠在页签上。旧实现拿 16px 渐变底衬压在视口
    // 边缘，正好把被切掉半截的页签吃掉（本次修改要解决的就是这个）。这里直接
    // 花几何时间比较按钮槽位与每个页签的矩形。
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    for x in [700.0, 500.0, 300.0, 120.0] {
        frame(&ctx, vec![Event::PointerMoved(Pos2::new(x, 12.0))], &mut app);
        let l = app.btn_slots[0].expect("应预留");
        let r = app.btn_slots[1].expect("应预留");
        assert!(!app.tab_rects.is_empty());
        // 按钮与视口不相交（视口就是「扣掉两端按钮」之后剩下的那条）。
        assert!(l.intersect(app.view_rect).width() <= 0.0);
        assert!(r.intersect(app.view_rect).width() <= 0.0);
        for t in &app.tab_rects {
            // 页签的 frame 矩形会比视口宽（被裁剪的部分画不出来），真正上屏的只有
            // 它与视口的交集 —— 就用这块「可见区域」去断言按钮没盖住页签。
            let painted = t.intersect(app.view_rect);
            if painted.width() <= 0.0 {
                continue;
            }
            assert!(
                painted.left() >= l.right() - 0.01,
                "左按钮盖住了页签可见部分：{:?} vs {:?}",
                l,
                painted
            );
            assert!(
                painted.right() <= r.left() + 0.01,
                "右按钮盖住了页签可见部分：{:?} vs {:?}",
                r,
                painted
            );
        }
        // 视口也应走在两个按钮之间（不被按钮咬掉一截）。
        assert!(app.view_rect.left() >= l.right() - 0.01);
        assert!(app.view_rect.right() <= r.left() + 0.01);
    }
}

#[test]
fn scroll_btn_state_follows_scroll_position() {
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    let (view_w, content_w, _) = app.view_log;
    let max_off = content_w - view_w;
    assert!(max_off > 100.0, "得有得滚才有按钮可言");
    // 在最左：只有右按钮可点。
    assert_eq!(app.btns, (false, true));

    // 滚到中间：两端都可点。
    app.tab_scroll_x = max_off * 0.5;
    app.tab_scroll_follow = false;
    frame(&ctx, vec![], &mut app);
    assert_eq!(app.btns, (true, true));

    // 滚到最右：左按钮可点，右端禁用（抵到头了还提示「还能往右」是骗人）。
    // **槽位不变**：若它一收回就会把页签在指针底下拉一下。
    app.tab_scroll_x = max_off;
    frame(&ctx, vec![], &mut app);
    assert_eq!(app.btns, (true, false));
    assert!(
        app.btn_slots.iter().all(|s| s.is_some()),
        "到了端槽位仍在"
    );
}

#[test]
fn btn_hidden_by_float_residue_at_end() {
    // 纯函数：抵到两端时 off 与 max_off 常差最后一点浮点残差，没有 0.5px 容差
    // 按钮就赖着不走（app.rs 里 strip_arrows 的存在理由）。
    let (view_w, content_w) = (400.0, 700.0);
    let max_off = content_w - view_w;
    assert_eq!(strip_arrows(max_off - 1e-4, view_w, content_w), (true, false));
    assert_eq!(strip_arrows(1e-4, view_w, content_w), (false, true));
    assert_eq!(strip_arrows(0.0, view_w, content_w), (false, true));
    // 内容不宽于视口 → 一律不可点。
    assert_eq!(strip_arrows(0.0, 400.0, 400.0), (false, false));
    assert_eq!(strip_arrows(12.0, 400.0, 380.0), (false, false));
}

#[test]
fn scroll_btn_click_scrolls_by_a_page() {
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    let (view_w, _content_w, _) = app.view_log;
    assert_eq!(app.tab_scroll_x, 0.0);

    // 点右按钮 → 往右翻大半屏；同时别把镜头抢回当前页签。
    click_scroll_btn(&ctx, &mut app, 1);
    let after_right = app.tab_scroll_x;
    assert!(
        after_right >= 48.0 && after_right <= view_w * 0.6 + 1.0,
        "点右按钮应翻大半屏，实际 {after_right}"
    );
    assert!(!app.tab_scroll_follow, "用户自己在翻页，别再抢镜头");
    frame(&ctx, vec![], &mut app);
    assert!(
        app.tab_scroll_x >= after_right - 1.0,
        "翻过去的偏移不该被复位"
    );
    assert_eq!(app.btns, (true, true));

    // 点左按钮 → 原路翻回来。
    click_scroll_btn(&ctx, &mut app, 0);
    assert!(
        app.tab_scroll_x < after_right - 1.0,
        "点左按钮应往回翻，实际 {} → {}",
        after_right,
        app.tab_scroll_x
    );
}

#[test]
fn scroll_btn_click_lands_on_end() {
    // 一步迈不到底：连点几下要能贴到两端，而不是在半路磨。
    let (view_w, content_w) = (300.0, 700.0);
    let mut off = 0.0;
    for _ in 0..20 {
        off = strip_arrow_scroll(off, view_w, content_w, false);
    }
    assert!(
        (off - (content_w - view_w)).abs() < 0.01,
        "应贴到最右，实际 {off}"
    );
    for _ in 0..20 {
        off = strip_arrow_scroll(off, view_w, content_w, true);
    }
    assert!(off.abs() < 0.01, "应贴到最左，实际 {off}");
}

#[test]
fn disabled_scroll_btn_does_nothing() {
    // 到了端的那一端保留槽位但不可点：点它必须既不滚动也不切页。
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    assert!(!app.btns.0, "起始在最左，左按钮应禁用");
    click_scroll_btn(&ctx, &mut app, 0);
    assert_eq!(app.tab_scroll_x, 0.0, "禁用的按钮点了不应动");
    assert_eq!(app.current, 2, "也不应误触到页签切换");
}

#[test]
fn scroll_btn_does_not_steal_tab_click() {
    // 按钮不再是「打在它下面的页签上」：槽位在视口之外，与页签矩形不交，命中也
    // 不会被抢走。这里盯「点按钮不切页」。
    let ctx = font_ctx();
    let mut app = App::new(14, 2);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(700.0, 12.0))], &mut app);
    assert_eq!(app.current, 2, "起始当前页签不变");
    click_scroll_btn(&ctx, &mut app, 1);
    assert!(app.tab_scroll_x > 0.0, "按钮应吃掉这次点击（滚动了）");
    assert_eq!(app.current, 2, "点按钮不该切页");
}
