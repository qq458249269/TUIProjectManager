// 回归测试：忠实复制 terminal.rs::show_terminal 的「本地文本选择 + 右键菜单」交互行为。
// 背景：用户反馈拖选后右键菜单的「复制」仍为灰色（has_selection=false）。
// 注：集成测试无法链接 bin crate，故按 terminal.rs 逻辑复制一份最小实现。
// cargo test --test term_select
use eframe::egui;
use egui::{Context, Event, Modifiers, PointerButton, Pos2, Rect, Sense};

#[derive(Default)]
struct TermSim {
    /// term.selection 替身：Some = 有选区。
    selection: bool,
    /// 菜单闭包本帧读到的 has_selection（None = 菜单未打开）。
    menu_has_selection: Option<bool>,
    /// 快速拖选兜底：主键按下点。
    drag_press_pos: Option<Pos2>,
}

fn frame(ctx: &Context, events: Vec<Event>, sim: &mut TermSim) {
    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
        events,
        ..Default::default()
    };
    sim.menu_has_selection = None;
    let mut out = ctx.run_ui(raw, |ui| {
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(760.0, 560.0), Sense::click_and_drag());

        // ── 复制自 terminal.rs：本地文本选择 ──
        let primary_released = ui.input(|i| i.pointer.button_released(egui::PointerButton::Primary));
        let latest_pos = ui.input(|i| i.pointer.latest_pos());
        // 同帧手势里指针状态已是帧末，按下点只能从原始事件取。
        if primary_released && sim.drag_press_pos.is_none() {
            sim.drag_press_pos = ui.input(|i| {
                i.raw.events.iter().find_map(|e| match e {
                    Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, .. } => Some(*pos),
                    _ => None,
                })
            });
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            // interact_pointer_pos 在矩形内即建选区（point_at 简化为恒成功）。
            if resp.interact_pointer_pos().is_some_and(|p| rect.contains(p)) {
                sim.selection = true;
            }
        } else if resp.dragged_by(egui::PointerButton::Primary) {
            // update 选区终点，无状态变化。
        }
        if resp.clicked() {
            sim.selection = false;
        }
        // ── 复制自 terminal.rs：快速拖选兜底 ──
        if primary_released {
            if let (Some(p0), Some(p1)) = (sim.drag_press_pos.take(), latest_pos) {
                let moved = p0.distance(p1) > 4.0;
                if moved && rect.contains(p1) && !sim.selection {
                    sim.selection = true;
                }
            }
        }

        // ── 复制自 terminal.rs：右键菜单 ──
        resp.context_menu(|ui| {
            let has_selection = sim.selection;
            sim.menu_has_selection = Some(has_selection);
            ui.add_enabled(has_selection, egui::Button::new("📋 复制"));
        });
    });
    out.textures_delta.clear();
}

fn btn(pos: Pos2, button: PointerButton, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: Modifiers::default(),
    }
}

/// 拖选 → 右键：菜单里的「复制」应为可用（has_selection=true）。
#[test]
fn drag_select_then_right_click_menu_enabled() {
    let ctx = Context::default();
    let mut sim = TermSim::default();

    frame(&ctx, vec![Event::PointerMoved(Pos2::new(50.0, 50.0))], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(50.0, 50.0), PointerButton::Primary, true)], &mut sim);
    // 拖动跨过 click 判定阈值（多帧移动）。
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(90.0, 60.0))], &mut sim);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(150.0, 80.0))], &mut sim);
    assert!(sim.selection, "拖选过程中应有选区");
    frame(&ctx, vec![btn(Pos2::new(150.0, 80.0), PointerButton::Primary, false)], &mut sim);
    assert!(sim.selection, "拖选释放后选区应保留（不应被 clicked 清掉）");

    // 右键按下+释放 → 菜单打开；再跑一帧让菜单内容渲染并读取选区状态。
    frame(&ctx, vec![btn(Pos2::new(100.0, 100.0), PointerButton::Secondary, true)], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(100.0, 100.0), PointerButton::Secondary, false)], &mut sim);
    frame(&ctx, vec![], &mut sim);
    assert!(
        matches!(sim.menu_has_selection, Some(true)),
        "右键菜单里应检测到选区，实际 {:?}",
        sim.menu_has_selection
    );
}

/// 单击清除选区是预期行为；双击（两次快速单击）同样以无选区收场。
#[test]
fn single_click_clears_selection() {
    let ctx = Context::default();
    let mut sim = TermSim::default();
    sim.selection = true;

    frame(&ctx, vec![Event::PointerMoved(Pos2::new(60.0, 60.0))], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, true)], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, false)], &mut sim);
    assert!(!sim.selection, "单击应清除选区");
}

/// 低帧率下快速拖选：按下/移动/释放全部落在同一帧，egui 不判 click 也不判
/// drag —— 兕底逻辑应把选区建出来（修复右键菜单「复制」恒灰的根因）。
#[test]
fn same_frame_flick_select_creates_selection() {
    let ctx = Context::default();
    let mut sim = TermSim::default();

    frame(
        &ctx,
        vec![Event::PointerMoved(Pos2::new(50.0, 50.0))],
        &mut sim,
    );
    // 同一帧内：按下 → 移动 → 释放。
    frame(
        &ctx,
        vec![
            btn(Pos2::new(50.0, 50.0), PointerButton::Primary, true),
            Event::PointerMoved(Pos2::new(200.0, 90.0)),
            btn(Pos2::new(200.0, 90.0), PointerButton::Primary, false),
        ],
        &mut sim,
    );
    assert!(sim.selection, "同帧快拖应兕底建出选区");

    // 随后右键菜单应读到选区存在。
    frame(&ctx, vec![btn(Pos2::new(100.0, 100.0), PointerButton::Secondary, true)], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(100.0, 100.0), PointerButton::Secondary, false)], &mut sim);
    frame(&ctx, vec![], &mut sim);
    assert!(
        matches!(sim.menu_has_selection, Some(true)),
        "右键菜单里应检测到选区，实际 {:?}",
        sim.menu_has_selection
    );
}
