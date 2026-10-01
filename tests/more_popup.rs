// 回归测试：忠实复制 app.rs::status_bar 右下角「⋯ 更多」弹层的写法，固化三条行为：
//   ① 点弹层里的项 → 动作触发，但弹层**不关**（egui Popup 默认 CloseOnClick 会在
//      点任何地方（含弹层内部）时顺手关掉菜单；app.rs 已改成 CloseOnClickOutside
//      且菜单项里不再调 ui.close()）。
//   ② 点弹层**外面**才关。
//   ③ 再点「⋯ 更多」本身仍是开关（点一下开、再点一下关）。
// 注：集成测试无法链接 bin crate，故此处按 app.rs 逻辑复制一份最小实现。
// cargo test --test more_popup
use eframe::egui;
use egui::{Context, Event, Id, Modifiers, PointerButton, Pos2, Rect};

/// 字体：默认 Context 不带任何字形，文字宽度算出来是 0（按钮被压成 8px 宽的空壳，
/// 点击坐标全错位）。这里装 app.rs 同款内嵌 HACK 字体，保证排版宽度是真的。
fn setup_fonts(ctx: &Context) {
    ctx.add_font(egui::epaint::text::FontInsert::new(
        "hack",
        egui::epaint::text::FontData::from_static(epaint_default_fonts::HACK_REGULAR),
        vec![egui::epaint::text::InsertFontFamily {
            family: egui::FontFamily::Proportional,
            priority: egui::epaint::text::FontPriority::Lowest,
        }],
    ));
}

/// app.rs 里被本测试盯住的那几样：菜单项是否被点到、各控件的矩形。
#[derive(Default)]
struct Sim {
    /// 每帧开头清零；弹层里任一菜单项被点到时置 true。
    fired: bool,
    /// 第一项「🔄 检查更新」的矩形（弹层画出来那帧才有值）。
    item_rect: Option<Rect>,
    /// 「⋯ 更多」按钮矩形。
    more_rect: Option<Rect>,
}

/// 与 app.rs::status_bar 右侧固定簇同构的最小实现。
fn frame(ctx: &Context, events: Vec<Event>, sim: &mut Sim) {
    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
        events,
        ..Default::default()
    };
    sim.fired = false;
    let mut out = ctx.run_ui(raw, |ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let more_id = Id::new("status_more_menu");
            let more_resp = ui.add(egui::Button::new("⋯ 更多").sense(egui::Sense::CLICK));
            sim.more_rect = Some(more_resp.rect);
            if more_resp.clicked() && ui.input(|i| i.pointer.any_click()) {
                egui::Popup::toggle_id(ui.ctx(), more_id);
            }
            egui::Popup::from_response(&more_resp)
                .id(more_id)
                .open_memory(None)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        // ⚠ 菜单项里**不能**调 ui.close()：那会直接置 CLOSE 标记，
                        // 绕过 close_behavior 把弹层关掉（就是本测试要防的回归）。
                        // 首项是「🔄 检查更新」：「⚙ 设置」已从本弹层移除（设置页签
                        // 常驻在页签栏首页右边，见 app.rs::tab_bar）。
                        let r = ui.selectable_label(false, "🔄 检查更新");
                        sim.item_rect = Some(r.rect);
                        if r.clicked() {
                            sim.fired = true;
                        }
                        if ui.selectable_label(false, "📂 打开用户目录").clicked() {
                            sim.fired = true;
                        }
                    });
                });
        });
    });
    out.textures_delta.clear();
}

fn btn(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::default(),
    }
}

/// 点一次：按下 + 释放分两帧（egui 跨帧才认 click，与真实鼠标一致）。
fn click(ctx: &Context, sim: &mut Sim, at: Pos2) {
    frame(ctx, vec![btn(at, true)], sim);
    frame(ctx, vec![btn(at, false)], sim);
}

fn more_id() -> Id {
    Id::new("status_more_menu")
}

fn center(r: Rect) -> Pos2 {
    r.center()
}

/// 打开弹层并等它摆稳，返回菜单项位置。
///
/// 必须多跑一帧：弹层刚打开那帧用的是 `default_area_size` 撑出来的临时位置，
/// 下一帧才落到最终位置 —— 拿首帧的坐标去点会点空（这与真实运行一致，
/// egui 的 Area 尺寸就是这样一帧一变）。
fn open_popup(ctx: &Context, sim: &mut Sim) -> Rect {
    frame(ctx, vec![], sim);
    let more = sim.more_rect.expect("应画出 ⋯ 更多");
    assert!(
        more.width() > 30.0,
        "按钮宽度要正常（字体没装上会退化成 8px）"
    );
    click(ctx, sim, center(more));
    assert!(
        egui::Popup::is_id_open(ctx, more_id()),
        "点 ⋯ 更多应打开弹层"
    );
    frame(ctx, vec![], sim);
    let item = sim.item_rect.expect("弹层里应画出菜单项");
    assert!(item.width() > 20.0, "菜单项宽度要正常");
    item
}

/// 点**当前**这一帧的菜单项（矩形每帧重取，绝不用上一帧的旧坐标）。
fn click_item(ctx: &Context, sim: &mut Sim) -> Rect {
    let item = sim.item_rect.expect("弹层应仍开着");
    click(ctx, sim, center(item));
    item
}

/// 核心行为：点弹层里的项 → 动作触发 + 弹层保持打开（连着点多项不用重开）。
#[test]
fn clicking_menu_item_keeps_popup_open() {
    let ctx = Context::default();
    setup_fonts(&ctx);
    let mut sim = Sim::default();

    open_popup(&ctx, &mut sim);
    assert!(!sim.fired, "只是点开菜单，不该触发菜单项");

    // 点第一项 → 触发，但弹层不能关。
    click_item(&ctx, &mut sim);
    assert!(sim.fired, "点「🔄 检查更新」应触发动作");
    assert!(
        egui::Popup::is_id_open(&ctx, more_id()),
        "点弹层里的项后弹层必须还在（egui 默认 CloseOnClick 会关掉）"
    );

    // 弹层还开着时再点一次菜单项 → 同样触发、同样不关（不必重新点开 ⋯ 更多）。
    click_item(&ctx, &mut sim);
    assert!(sim.fired, "弹层没关的话，项应同样能点到");
    assert!(egui::Popup::is_id_open(&ctx, more_id()), "仍应保持打开");
}

/// 只有点弹层**外面**才隐藏。
#[test]
fn clicking_outside_closes_popup() {
    let ctx = Context::default();
    setup_fonts(&ctx);
    let mut sim = Sim::default();

    let item = open_popup(&ctx, &mut sim);

    // 弹层左侧之外、同高的空白处（状态栏消息那一段）→ 关闭。
    let outside = Pos2::new(item.left() - 40.0, item.center().y);
    assert!(
        !item.expand2(egui::vec2(8.0, 8.0)).contains(outside),
        "选的坐标得在弹层外面，实测 item={:?} outside={outside:?}",
        item
    );
    click(&ctx, &mut sim, outside);
    assert!(
        !egui::Popup::is_id_open(&ctx, more_id()),
        "点弹层外面才该隐藏"
    );
}

/// Esc 也照旧能关（CloseOnClickOutside 不影响键盘路径）。
#[test]
fn escape_closes_popup() {
    let ctx = Context::default();
    setup_fonts(&ctx);
    let mut sim = Sim::default();

    open_popup(&ctx, &mut sim);
    frame(
        &ctx,
        vec![Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        }],
        &mut sim,
    );
    assert!(!egui::Popup::is_id_open(&ctx, more_id()), "Esc 应关闭弹层");
}

/// 「⋯ 更多」本身仍是开关：再点一下收起来（不是只能点外面）。
#[test]
fn anchor_toggles_popup() {
    let ctx = Context::default();
    setup_fonts(&ctx);
    let mut sim = Sim::default();

    open_popup(&ctx, &mut sim);
    // 菜单开着时再点 ⋯ 更多：先被 CloseOnClickOutside 判为外部点击，再被 toggle 关掉
    // —— 净效果是关闭（不能被 toggle 又弹回来）。
    let more = sim.more_rect.expect("⋯ 更多 始终在");
    click(&ctx, &mut sim, center(more));
    assert!(
        !egui::Popup::is_id_open(&ctx, more_id()),
        "再点 ⋯ 更多应收起弹层"
    );
}
