// 回归测试：忠实复制 terminal.rs::show_terminal 的「本地文本选择 + 右键菜单」交互行为。
// 背景：用户反馈拖选后右键菜单的「复制」仍为灰色（has_selection=false）。
// 背景2：mouse reporting 的 TUI 下，拖选/取消选中的按下/释放被原样转发，
//        子进程当成点击 → 整页重绘 + 页签通知图标误闪。现在选区手势期间
//        这对按下/释放必须被吞掉，只有无选区的普通点击才转发。
// 注：集成测试无法链接 bin crate，故按 terminal.rs 逻辑复制一份最小实现。
// cargo test --test term_select
use eframe::egui;
use egui::{Context, Event, Modifiers, PointerButton, Pos2, Rect, Sense};

/// 一次被转发给子进程的鼠标事件（用于断言转发/吞掉）。
#[derive(Debug, PartialEq, Clone, Copy)]
enum Fwd {
    Press,
    Release,
}

#[derive(Default)]
struct TermSim {
    /// term.selection 替身：Some = 有选区。
    selection: bool,
    /// 菜单闭包本帧读到的 has_selection（None = 菜单未打开）。
    menu_has_selection: Option<bool>,
    /// 快速拖选兜底：主键按下点。
    drag_press_pos: Option<Pos2>,
    /// 按下位置（纯点击判定用，对应 sess.click_press_pos）。
    click_press_pos: Option<Pos2>,
    /// 已转发给子进程的鼠标按下（对应 sess.mouse_press_pending）。
    mouse_press_pending: bool,
    /// 本次主键手势被判为本地选区手势（对应 sess.mouse_gesture_sel）。
    mouse_gesture_sel: bool,
    /// 子进程是否开了鼠标上报（TermMode::MOUSE_MODE | SGR_MOUSE）。
    mouse_reporting: bool,
    /// 本帧转发给子进程的鼠标事件（frame 开始时清空）。
    forwarded: Vec<(Fwd, Pos2)>,
}

fn frame(ctx: &Context, events: Vec<Event>, sim: &mut TermSim) {
    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
        events,
        ..Default::default()
    };
    sim.menu_has_selection = None;
    sim.forwarded.clear();
    let mut out = ctx.run_ui(raw, |ui| {
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(760.0, 560.0), Sense::click_and_drag());
        let to_col_row = |pos: Pos2| -> Pos2 { pos };

        // ── 复制自 terminal.rs：mouse reporting 转发（选区手势分类） ──
        if sim.mouse_reporting {
            let ptr_pressed = ui.input(|i| {
                i.pointer.button_pressed(egui::PointerButton::Primary)
                    || i.pointer.button_pressed(egui::PointerButton::Middle)
            });
            let ptr_released = ui.input(|i| {
                i.pointer.button_released(egui::PointerButton::Primary)
                    || i.pointer.button_released(egui::PointerButton::Middle)
            });
            let primary_press_pos = ui.input(|i| {
                i.pointer
                    .button_pressed(egui::PointerButton::Primary)
                    .then(|| i.pointer.latest_pos())
                    .flatten()
            });

            if resp.hovered() && ptr_pressed {
                if primary_press_pos.is_some() {
                    // 主键：先记账，等释放帧分类（可能被本地选区手势吞掉）。
                    sim.mouse_press_pending = true;
                } else {
                    // 中键：照旧立即转发。
                    if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                        sim.forwarded.push((Fwd::Press, to_col_row(pos)));
                    }
                }
            }

            // 主键释放帧在下方「本地文本选择」块内分类转发；中键释放立即转发。
            if ptr_released
                && !ui.input(|i| i.pointer.button_released(egui::PointerButton::Primary))
                && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
            {
                sim.forwarded.push((Fwd::Release, to_col_row(pos)));
            }
        }

        // ── 复制自 terminal.rs：本地文本选择 ──
        let primary_released = ui.input(|i| i.pointer.button_released(egui::PointerButton::Primary));
        let latest_pos = ui.input(|i| i.pointer.latest_pos());
        let primary_pressed = ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary));
        if primary_pressed {
            sim.click_press_pos = ui.input(|i| i.pointer.latest_pos());
        }
        // 同帧手势里指针状态已是帧末，按下点只能从原始事件取。
        if primary_released && sim.drag_press_pos.is_none() {
            sim.drag_press_pos = ui.input(|i| {
                i.raw.events.iter().find_map(|e| match e {
                    Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, .. } => Some(*pos),
                    _ => None,
                })
            });
        }
        // 判断是否为纯点击（无拖动位移）：按下与释放位置距离 < 4px。
        let is_true_click = primary_released && sim.click_press_pos.is_some_and(|p0| {
            latest_pos.is_some_and(|p1| p1.distance(p0) < 4.0)
        });

        // sel_at_frame_start：释放帧分类要用「本帧开始时的选区状态」。
        let sel_at_frame_start = sim.selection;

        if resp.drag_started_by(egui::PointerButton::Primary) {
            if resp.interact_pointer_pos().is_some_and(|p| rect.contains(p)) {
                sim.selection = true;
                sim.mouse_gesture_sel = true;
                sim.mouse_press_pending = false;
            }
        } else if resp.dragged_by(egui::PointerButton::Primary) {
            sim.mouse_gesture_sel = true;
            sim.mouse_press_pending = false;
        }
        if resp.clicked() && is_true_click {
            sim.selection = false;
        }

        // ── 复制自 terminal.rs：快速拖选兜底 ──
        if primary_released
            && let (Some(p0), Some(p1)) = (sim.drag_press_pos.take(), latest_pos)
        {
            let moved = p0.distance(p1) > 4.0;
            if moved && rect.contains(p1) && !sim.selection {
                sim.selection = true;
                sim.mouse_gesture_sel = true;
                sim.mouse_press_pending = false;
            }
        }

        // ── 复制自 terminal.rs：释放帧分类（转发 vs 吞掉） ──
        if primary_released {
            let gesture_sel = sim.mouse_gesture_sel || sel_at_frame_start;
            if gesture_sel {
                // 本地选区手势：不给 TUI 发幽灵点击，静默丢弃。
                sim.mouse_press_pending = false;
            } else if sim.mouse_press_pending {
                // 普通点击：按原行为成对转发（按下位置 + 释放位置）。
                sim.forwarded.push((Fwd::Press, to_col_row(latest_pos.unwrap_or(rect.center()))));
                sim.forwarded.push((Fwd::Release, to_col_row(latest_pos.unwrap_or(rect.center()))));
                sim.mouse_press_pending = false;
            }
            sim.mouse_gesture_sel = false;
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
    let mut sim = TermSim { mouse_reporting: true, ..Default::default() };

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
    let mut sim = TermSim {
        selection: true,
        ..Default::default()
    };

    frame(&ctx, vec![Event::PointerMoved(Pos2::new(60.0, 60.0))], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, true)], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, false)], &mut sim);
    assert!(!sim.selection, "单击应清除选区");
}

/// 低帧率下快速拖选：按下/移动/释放全部落在同一帧，egui 不判 click 也不判
/// drag —— 兜底逻辑应把选区建出来（修复右键菜单「复制」恒灰的根因）。
#[test]
fn same_frame_flick_select_creates_selection() {
    let ctx = Context::default();
    let mut sim = TermSim { mouse_reporting: true, ..Default::default() };

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
    assert!(sim.selection, "同帧快拖应兜底建出选区");

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

/// mouse reporting 下拖选全程：按下/拖动/释放帧都不得向子进程转发任何
/// 鼠标事件（否则 TUI 收到幽灵点击 → 整页重绘 + 通知图标误闪）。
#[test]
fn drag_select_forwards_nothing_to_tui() {
    let ctx = Context::default();
    let mut sim = TermSim { mouse_reporting: true, ..Default::default() };

    frame(&ctx, vec![Event::PointerMoved(Pos2::new(50.0, 50.0))], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(50.0, 50.0), PointerButton::Primary, true)], &mut sim);
    frame(&ctx, vec![Event::PointerMoved(Pos2::new(150.0, 80.0))], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(150.0, 80.0), PointerButton::Primary, false)], &mut sim);
    assert!(sim.selection, "拖选应建出选区");
    assert!(
        sim.forwarded.is_empty(),
        "拖选手势不应向子进程转发任何鼠标事件，实际 {:?}",
        sim.forwarded
    );
}

/// mouse reporting 下取消选中（点一下已有选区）也不得转发幽灵点击。
#[test]
fn deselect_click_forwards_nothing_to_tui() {
    let ctx = Context::default();
    let mut sim = TermSim {
        mouse_reporting: true,
        selection: true,
        ..Default::default()
    };

    frame(&ctx, vec![Event::PointerMoved(Pos2::new(60.0, 60.0))], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, true)], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, false)], &mut sim);
    assert!(!sim.selection, "纯点击应清除选区");
    assert!(
        sim.forwarded.is_empty(),
        "取消选中的点击不应向子进程转发任何鼠标事件，实际 {:?}",
        sim.forwarded
    );
}

/// mouse reporting 下无选区的普通点击仍按原行为成对转发（TUI 按钮可点）。
#[test]
fn plain_click_still_forwards_pair() {
    let ctx = Context::default();
    let mut sim = TermSim { mouse_reporting: true, ..Default::default() };

    frame(&ctx, vec![Event::PointerMoved(Pos2::new(60.0, 60.0))], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, true)], &mut sim);
    assert!(sim.forwarded.is_empty(), "按下帧尚不转发（延迟到释放帧分类）");
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, false)], &mut sim);
    assert_eq!(
        sim.forwarded,
        vec![
            (Fwd::Press, Pos2::new(60.0, 60.0)),
            (Fwd::Release, Pos2::new(60.0, 60.0)),
        ],
        "普通点击应成对转发按下+释放"
    );

    // 第二次普通点击（无选区）同样成对转发。
    frame(&ctx, vec![btn(Pos2::new(120.0, 90.0), PointerButton::Primary, true)], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(122.0, 90.0), PointerButton::Primary, false)], &mut sim);
    assert_eq!(sim.forwarded.len(), 2, "第二次普通点击也应成对转发，实际 {:?}", sim.forwarded);
}

/// 快速拖选兜底建出选区的同帧手势同样不得转发。
#[test]
fn same_frame_flick_select_forwards_nothing() {
    let ctx = Context::default();
    let mut sim = TermSim { mouse_reporting: true, ..Default::default() };

    frame(&ctx, vec![Event::PointerMoved(Pos2::new(50.0, 50.0))], &mut sim);
    frame(
        &ctx,
        vec![
            btn(Pos2::new(50.0, 50.0), PointerButton::Primary, true),
            Event::PointerMoved(Pos2::new(200.0, 90.0)),
            btn(Pos2::new(200.0, 90.0), PointerButton::Primary, false),
        ],
        &mut sim,
    );
    assert!(sim.selection, "同帧快拖应兜底建出选区");
    assert!(
        sim.forwarded.is_empty(),
        "同帧快拖手势不应向子进程转发任何鼠标事件，实际 {:?}",
        sim.forwarded
    );
}

/// 鼠标上报关闭（普通 shell）时点击照旧转发，不受本次改动影响。
#[test]
fn no_mouse_reporting_click_forwards_immediately() {
    let ctx = Context::default();
    let mut sim = TermSim { mouse_reporting: false, ..Default::default() };

    frame(&ctx, vec![Event::PointerMoved(Pos2::new(60.0, 60.0))], &mut sim);
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, true)], &mut sim);
    // mouse_reporting=false 时整套转发逻辑不运行（本地选择照旧）。
    assert!(sim.forwarded.is_empty(), "未开上报时不转发");
    frame(&ctx, vec![btn(Pos2::new(60.0, 60.0), PointerButton::Primary, false)], &mut sim);
    assert!(sim.forwarded.is_empty(), "未开上报时不转发");
}
