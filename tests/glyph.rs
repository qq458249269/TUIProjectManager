// 回归测试：采样 egui 字体图集，确认 msyh 汉字字形在应用内渲染的覆盖度。
// 背景：截图像素取证显示选中区汉字段只有 ~12% 白色覆盖（字形极淡，与底色几乎同色），
// 而拉丁字母满覆盖 255。此测试验证是否字体图集光栅化问题（与选区颜色无关）。
// cargo test --test glyph -- --nocapture
use eframe::egui;
use egui::epaint::text::{FontData, FontInsert, FontPriority, InsertFontFamily};
use egui::{Color32, FontFamily, FontId};

fn setup(ctx: &egui::Context) {
    for path in [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\msyhbd.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
    ] {
        if let Ok(data) = std::fs::read(path) {
            ctx.add_font(FontInsert::new(
                "cjk",
                FontData::from_owned(data),
                vec![
                    InsertFontFamily {
                        family: FontFamily::Proportional,
                        priority: FontPriority::Lowest,
                    },
                    InsertFontFamily {
                        family: FontFamily::Monospace,
                        priority: FontPriority::Lowest,
                    },
                ],
            ));
            println!("loaded font: {path}");
            break;
        }
    }
}

#[test]
fn glyph_coverage() {
    let ctx = egui::Context::default();
    setup(&ctx);
    // 字体在首次 run 时才建立，先跑一帧空 UI，并清掉未消费的纹理增量，避免 drop 时 panic
    let mut fo = ctx.run_ui(egui::RawInput::default(), |_| {});
    fo.textures_delta.clear();

    let mut job = egui::text::LayoutJob::default();
    job.append(
        "字A汉",
        0.0,
        egui::TextFormat {
            font_id: FontId::monospace(14.0),
            color: Color32::WHITE,
            ..Default::default()
        },
    );
    let galley = ctx.fonts_mut(|f| f.layout_job(job));
    let has_cjk = ctx.fonts_mut(|f| f.has_glyph(&FontId::monospace(14.0), '字'));
    let fill = ctx.fonts(|f| f.font_atlas_fill_ratio());
    let atlas = ctx.fonts(|f| f.image().clone());
    // 图集里非空 texel 的包围盒（找字形实际落在哪）
    let (mut minx, mut miny, mut maxx, mut maxy) = (usize::MAX, usize::MAX, 0usize, 0usize);
    let mut nonzero = 0u64;
    let mut max_alpha = 0u8;
    let mut max_alpha_xy = (0usize, 0usize);
    for (i, p) in atlas.pixels.iter().enumerate() {
        if p.a() > 0 {
            let (x, y) = (i % atlas.width(), i / atlas.width());
            minx = minx.min(x); miny = miny.min(y); maxx = maxx.max(x); maxy = maxy.max(y);
            nonzero += 1;
        }
        if p.a() > max_alpha {
            max_alpha = p.a();
            max_alpha_xy = (i % atlas.width(), i / atlas.width());
        }
    }
    println!("has_cjk={has_cjk} fill={fill:.3} 非空texel={nonzero} 包围盒 x[{minx}..{maxx}] y[{miny}..{maxy}] 全图最大alpha={max_alpha} 位置={max_alpha_xy:?}");
    println!("atlas {}x{} galley size {:?}", atlas.width(), atlas.height(), galley.size());

    let w = atlas.width();
    let mut per_glyph: Vec<(char, u8)> = Vec::new();
    for row in &galley.rows {
        for g in &row.glyphs {
            let r = g.uv_rect;
            let (x0, y0) = (r.min[0] as usize, r.min[1] as usize);
            let (x1, y1) = (r.max[0] as usize, r.max[1] as usize);
            let mut mx = 0u8;
            let (mut sx, mut sy, mut n) = (0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    if x < atlas.width() && y < atlas.height() {
                        let a = atlas.pixels[x + y * w].a();
                        mx = mx.max(a);
                        sx += a as u32;
                        n += 1;
                    }
                }
            }
            if n > 0 {
                per_glyph.push((g.chr, mx));
                let mean = sx / n;
                println!("  字形 {:?} uv_rect {r:?} 面积{n} 最大alpha={mx} 平均={mean}", g.chr);
            }
        }
    }
    if per_glyph.is_empty() {
        panic!("no glyphs found");
    }
    let cjk_max = per_glyph.iter().filter(|(c, _)| c.is_alphabetic() && !c.is_ascii()).map(|(_, a)| *a).max().unwrap_or(0);
    let latin_max = per_glyph.iter().filter(|(c, _)| c.is_ascii_alphabetic()).map(|(_, a)| *a).max().unwrap_or(0);
    println!("CJK最大alpha={cjk_max} Latin最大alpha={latin_max} (255=满覆盖)");
    assert!(cjk_max > 200, "汉字覆盖度过低：cjk_max={cjk_max}，说明 CJK 光栅化/缩放有问题");
}