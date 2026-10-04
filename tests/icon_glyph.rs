// 回归测试：页签状态图标 ✅🔄❌ 在**实际字体链**（HACK + CJK + Segoe UI Emoji/Symbol）
// 里必须真有可见字形。has_glyph 只查 cmap，字体真缺轮廓（fontdue 不支持 COLR 彩色
// emoji）时会「有字形、零像素」→ 页签上图标凭空消失。本测试按 alpha 覆盖度钉死。
// cargo test --test icon_glyph -- --nocapture
use eframe::egui;
use egui::epaint::text::{FontData, FontInsert, FontPriority, InsertFontFamily};
use egui::{Color32, FontFamily, FontId};

const FAMILIES: [egui::FontFamily; 2] = [FontFamily::Proportional, FontFamily::Monospace];

fn setup(ctx: &egui::Context) {
let add = |name: &'static str, data: FontData| {
        ctx.add_font(FontInsert::new(
            name,
            data,
            FAMILIES
                .iter()
                .map(|family| InsertFontFamily {
family: family.clone(),
                    priority: FontPriority::Lowest,
                })
                .collect(),
        ));
    };
    add("hack", FontData::from_static(epaint_default_fonts::HACK_REGULAR));
    for path in [
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
    ] {
        if let Ok(data) = std::fs::read(path) {
            add("cjk", FontData::from_owned(data));
            break;
        }
    }
    for (name, path) in [
        ("segoe-emoji", r"C:\Windows\Fonts\seguiemj.ttf"),
        ("segoe-symbol", r"C:\Windows\Fonts\seguisym.ttf"),
    ] {
        if let Ok(data) = std::fs::read(path) {
            add(name, FontData::from_owned(data));
        }
    }
}

/// 字形的最大 alpha（0 = 图标画不出来）。
fn max_alpha(ctx: &egui::Context, ch: char) -> u8 {
    let mut job = egui::text::LayoutJob::default();
    job.append(
&ch.to_string(),
        0.0,
        egui::TextFormat {
            font_id: FontId::monospace(14.0),
            color: Color32::WHITE,
            ..Default::default()
        },
    );
    let galley = ctx.fonts_mut(|f| f.layout_job(job));
    let atlas = ctx.fonts(|f| f.image().clone());
    let mut mx = 0u8;
    for row in &galley.rows {
        for g in &row.glyphs {
            if g.chr != ch {
                continue;
            }
            let (x0, y0) = (g.uv_rect.min[0] as usize, g.uv_rect.min[1] as usize);
            let (x1, y1) = (g.uv_rect.max[0] as usize, g.uv_rect.max[1] as usize);
            for y in y0..y1.min(atlas.height()) {
                for x in x0..x1.min(atlas.width()) {
                    mx = mx.max(atlas.pixels[x + y * atlas.width()].a());
                }
            }
        }
    }
    mx
}

#[test]
fn status_icons_have_glyphs() {
    let ctx = egui::Context::default();
    setup(&ctx);
    let mut fo = ctx.run_ui(egui::RawInput::default(), |_| {});
    fo.textures_delta.clear();

    let mut bad = vec![];
    for ch in ['✅', '\u{2705}', '🔄', '\u{1f504}', '❌', '\u{274c}'] {
        let a = max_alpha(&ctx, ch);
        println!("{ch} U+{:04X} 最大alpha={a}", ch as u32);
        if a < 40 {
            bad.push(format!("{ch}(U+{:04X}) alpha={a}", ch as u32));
        }
    }
    assert!(bad.is_empty(), "这些状态图标没有可见字形: {bad:?}");
}