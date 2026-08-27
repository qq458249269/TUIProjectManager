//! 终端字形批渲染：字形预光栅化进一张纹理图集，格子直接拼 quad 提交，
//! 跳过逐格 Galley 排版与 epaint tessellation（终端区每帧上万三角形的
//! 主开销）。走 egui 自身纹理 + Mesh 管线，无自定义 shader/GL 调用：
//! 核显、软件渲染（WARP/llvmpipe）与普通 UI 元素同一条路，能跑本程序
//! 就能用。图集缺字形（emoji 等）的格子自动回落原 galley 路径，逐格混合。
//!
//! 静止帧优化：每格算 FNV 哈希入表，与上一帧全等则直接重放缓存的 Mesh，
//! 跳过 quad 重建；内容一变即整帧重建（不做局部更新——重建本身已是微秒级）。

use std::collections::HashMap;

use eframe::egui;
use egui::{Color32, ColorImage, Mesh, Pos2, Rect, TextureHandle, TextureOptions};

/// 图集边长（物理像素）。1024×1024 RGBA ≈ 4MB 显存；CJK 字形约 28px 高，
/// 可容纳上千个不同字符，超出后整体清空按需重灌（下一帧自然恢复）。
const ATLAS_SIZE: u32 = 1024;
/// 字形位图之间的间隔像素：LINEAR 采样时防止相邻字形边缘渗色。
const GLYPH_PAD: u32 = 1;

/// 单个字形的图集记录。UV 指向图集内的位图矩形；
/// dx 相对格子左缘、dy 相对基线的位图左上角偏移（逻辑点，dy 恒 ≤ 0）。
#[derive(Clone, Copy)]
pub struct GlyphSlot {
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub dx: f32,
    pub dy: f32,
    /// 位图尺寸（逻辑点）。w == 0 表示空槽：无可见笔画或所有字体都缺该字形，
    /// 命中缓存但不出 quad（调用方据此回落 galley 路径）。
    pub w: f32,
    pub h: f32,
}

impl GlyphSlot {
    const EMPTY: Self = Self {
        u0: 0.0,
        v0: 0.0,
        u1: 0.0,
        v1: 0.0,
        dx: 0.0,
        dy: 0.0,
        w: 0.0,
        h: 0.0,
    };
}

/// 字形纹理图集：动态 shelf 打包，按需光栅化。
pub struct GlyphAtlas {
    /// Monospace 家族字体链（主字体在前，CJK fallback 在后），按序试到命中。
    fonts: Vec<fontdue::Font>,
    /// 光栅化字号（物理像素）= 字号 pt × ppp。
    px: f32,
    ppp: f32,
    ascent_px: f32,
    descent_px: f32,
    w: u32,
    h: u32,
    /// RGBA8 预乘白色覆盖度（rgb == a）。
    rgba: Vec<u8>,
    cx: u32,
    cy: u32,
    rh: u32,
    map: HashMap<char, GlyphSlot>,
    /// 图集内容版本号：新增/清空字形时 +1，驱动调用方重传纹理并重建网格。
    pub version: u64,
}

impl GlyphAtlas {
    /// `font_data`：(字体文件字节, ttc 子索引)。解析失败的字体跳过。
    pub fn new(font_data: &[(Vec<u8>, u32)], font_size_pt: f32, ppp: f32) -> Self {
        let px = font_size_pt * ppp;
        let mut fonts = Vec::new();
        let mut ascent_px = px * 0.8;
        let mut descent_px = -px * 0.2;
        for (data, index) in font_data {
            let settings = fontdue::FontSettings {
                collection_index: *index,
                scale: px,
                load_substitutions: false,
            };
            match fontdue::Font::from_bytes(data.as_slice(), settings) {
                Ok(f) => {
                    if fonts.is_empty() {
                        if let Some(lm) = f.horizontal_line_metrics(px) {
                            ascent_px = lm.ascent;
                            descent_px = lm.descent;
                        }
                    }
                    fonts.push(f);
                }
                Err(_) => continue,
            }
        }
        let mut atlas = Self {
            fonts,
            px,
            ppp,
            ascent_px,
            descent_px,
            w: ATLAS_SIZE,
            h: ATLAS_SIZE,
            rgba: vec![0; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize],
            cx: 1, // (0,0) 保留为纯白素：实心 quad（下划线）取色用
            cy: 0,
            rh: 0,
            map: HashMap::new(),
            version: 0,
        };
        atlas.paint_white_texel();
        atlas
    }

    fn paint_white_texel(&mut self) {
        self.rgba[0..4].copy_from_slice(&[255, 255, 255, 255]);
    }

    /// 图集是否为空（尚未光栅化任何字形）。首帧懒预热据此判断。
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 基线相对格子顶部的偏移（逻辑点）：内容行高在 cell_h 内垂直居中，
    /// 与 egui 强制 line_height 的排版结果一致。
    #[inline]
    pub fn baseline_rel(&self, cell_h: f32) -> f32 {
        let content_h = (self.ascent_px - self.descent_px) / self.ppp;
        (cell_h - content_h) / 2.0 + self.ascent_px / self.ppp
    }

    /// 下划线相对格子顶部的偏移（逻辑点）：基线下方 1px。
    #[inline]
    pub fn underline_rel(&self, cell_h: f32) -> f32 {
        self.baseline_rel(cell_h) + 1.0
    }

    /// 实心 quad 的 UV（纯白素中心），配合顶点色画下划线等待意矩形。
    #[inline]
    pub fn solid_uv() -> Pos2 {
        Pos2::new(0.5 / ATLAS_SIZE as f32, 0.5 / ATLAS_SIZE as f32)
    }

    /// shelf 打包：返回位图区域左上角。满则整体清空重试一次，仍放不下返回 None。
    fn alloc(&mut self, gw: u32, gh: u32) -> Option<(u32, u32)> {
        if gw > self.w || gh > self.h {
            return None;
        }
        if self.cx + gw > self.w {
            self.cy += self.rh;
            self.cx = 1;
            self.rh = 0;
        }
        if self.cy + gh > self.h {
            self.reset();
            if self.cx + gw > self.w || self.cy + gh > self.h {
                return None;
            }
        }
        let pos = (self.cx, self.cy);
        self.cx += gw;
        self.rh = self.rh.max(gh);
        Some(pos)
    }

    fn reset(&mut self) {
        self.rgba.fill(0);
        self.paint_white_texel();
        self.map.clear();
        self.cx = 1;
        self.cy = 0;
        self.rh = 0;
        self.version += 1;
    }

    /// 查询字形，未缓存则光栅化入库。永不失败：缺字形/图集满记为空槽
    /// （负缓存，避免每帧重复尝试），调用方按 slot.w == 0 回落 galley。
    pub fn glyph(&mut self, ch: char) -> GlyphSlot {
        if let Some(&s) = self.map.get(&ch) {
            return s;
        }
        // 先在不可变借用内完成字体链查找与光栅化，再拿可变借用入库。
        let mut hit = None;
        for font in &self.fonts {
            if !font.has_glyph(ch) {
                continue;
            }
            hit = Some(font.rasterize(ch, self.px));
            break;
        }
        let slot = match hit {
            // 有字形无笔画（零宽/空白）：空槽即可
            Some((m, _)) if m.width == 0 || m.height == 0 => GlyphSlot::EMPTY,
            Some((m, bmp)) => {
                let gw = m.width as u32 + GLYPH_PAD * 2;
                let gh = m.height as u32 + GLYPH_PAD * 2;
                let Some((ax, ay)) = self.alloc(gw, gh) else {
                    // 图集放不下（超大字形或真满了）：回落 galley，不占缓存位
                    // 以便 reset 后重试。ponytail: 上限=单字形 ATLAS_SIZE 像素，
                    // 更大字形永远走 galley；需要时改分块图集。
                    return GlyphSlot::EMPTY;
                };
                let stride = self.w as usize * 4;
                for row in 0..m.height {
                    let src = &bmp[row * m.width..(row + 1) * m.width];
                    let dst = (ay as usize + GLYPH_PAD as usize + row) * stride
                        + (ax as usize + GLYPH_PAD as usize) * 4;
                    for (i, &c) in src.iter().enumerate() {
                        let o = dst + i * 4;
                        self.rgba[o] = c;
                        self.rgba[o + 1] = c;
                        self.rgba[o + 2] = c;
                        self.rgba[o + 3] = c;
                    }
                }
                let wf = self.w as f32;
                let hf = self.h as f32;
                let x0 = ax as f32 + GLYPH_PAD as f32;
                let y0 = ay as f32 + GLYPH_PAD as f32;
                GlyphSlot {
                    u0: x0 / wf,
                    v0: y0 / hf,
                    u1: (x0 + m.width as f32) / wf,
                    v1: (y0 + m.height as f32) / hf,
                    dx: m.xmin as f32 / self.ppp,
                    dy: -(m.ymin + m.height as i32) as f32 / self.ppp,
                    w: m.width as f32 / self.ppp,
                    h: m.height as f32 / self.ppp,
                }
            }
            None => return GlyphSlot::EMPTY, // 所有字体都没有该字形（不入负缓存，
                                             // 避免运行时加载字体后永远缺字）
        };
        self.map.insert(ch, slot);
        self.version += 1;
        slot
    }

    /// 当前图集内容的 egui 纹理镜像。
    pub fn image(&self) -> ColorImage {
        ColorImage::from_rgba_premultiplied([self.w as usize, self.h as usize], &self.rgba)
    }
}

/// 一个待提交的字形/实心 quad。
pub struct CellQuad {
    pub rect: Rect,
    pub uv0: Pos2,
    pub uv1: Pos2,
    pub color: Color32,
}

/// 把 quad 列表组装成 egui Mesh（两个三角形一格）。
pub fn build_mesh(quads: &[CellQuad], texture_id: egui::TextureId) -> Mesh {
    let mut mesh = Mesh::with_texture(texture_id);
    mesh.vertices.reserve(quads.len() * 4);
    mesh.indices.reserve(quads.len() * 6);
    for q in quads {
        let vi = mesh.vertices.len() as u32;
        let r = q.rect;
        let v = |pos: Pos2, uv: Pos2| egui::epaint::Vertex { pos, uv, color: q.color };
        mesh.vertices.extend([
            v(r.left_top(), q.uv0),
            v(r.right_top(), Pos2::new(q.uv1.x, q.uv0.y)),
            v(r.right_bottom(), q.uv1),
            v(r.left_bottom(), Pos2::new(q.uv0.x, q.uv1.y)),
        ]);
        mesh.indices.extend_from_slice(&[vi, vi + 1, vi + 2, vi, vi + 2, vi + 3]);
    }
    mesh
}

/// FNV-1a 搅拌一步。
#[inline]
pub fn hash_mix(h: &mut u64, v: u64) {
    *h ^= v;
    *h = h.wrapping_mul(0x100_0000_01b3);
}

/// 每会话 GPU 批渲染状态。`None` = 尚未初始化或初始化失败，整格走 galley 回落。
pub struct TermGpu {
    pub atlas: GlyphAtlas,
    /// 图集重建所需的原始字体字节（fontdue 不外借字节，DPI 变化时重建用）。
    font_bytes: Vec<(Vec<u8>, u32)>,
    font_size_pt: f32,
    params_ppp: f32,
    pub tex: Option<TextureHandle>,
    tex_version: u64,
    /// 上一帧每格哈希（rows×cols，索引 vline*cols+col，未访问格保持 0）。
    prev_hash: Vec<u64>,
    /// 本帧哈希写入缓冲（跨帧复用分配）。
    pub hash_scratch: Vec<u64>,
    /// 本帧 quad 收集缓冲（跨帧复用分配）。
    pub quads: Vec<CellQuad>,
    /// 静止帧复用的已提交网格。
    pub mesh: Option<std::sync::Arc<Mesh>>,
}

impl TermGpu {
    /// 从 egui 已注册的 Monospace 家族提取字体数据初始化。字体链为空返回 None。
    pub fn new(ctx: &egui::Context, font_size_pt: f32, ppp: f32) -> Option<Self> {
        let defs = ctx.fonts_mut(|f| f.definitions().clone());
        let mono = defs.families.get(&egui::FontFamily::Monospace)?.clone();
        let mut font_bytes: Vec<(Vec<u8>, u32)> = Vec::new();
        for name in mono {
            if let Some(d) = defs.font_data.get(&name) {
                font_bytes.push((d.font.as_ref().to_vec(), d.index));
            }
        }
        if font_bytes.is_empty() {
            return None;
        }
        let atlas = GlyphAtlas::new(&font_bytes, font_size_pt, ppp);
        Some(Self {
            atlas,
            font_bytes,
            font_size_pt,
            params_ppp: ppp,
            tex: None,
            tex_version: 0,
            prev_hash: Vec::new(),
            hash_scratch: Vec::new(),
            quads: Vec::new(),
            mesh: None,
        })
    }

    /// DPI 变化时重建图集（光栅化字号随物理像素变化）；其余情况原地复用。
    pub fn ensure_params(&mut self, font_size_pt: f32, ppp: f32) {
        if self.params_ppp == ppp && self.font_size_pt == font_size_pt {
            return;
        }
        self.atlas = GlyphAtlas::new(&self.font_bytes, font_size_pt, ppp);
        self.params_ppp = ppp;
        self.font_size_pt = font_size_pt;
        self.tex_version = 0; // 强制重传纹理
        self.mesh = None; // UV 全部失效，强制重建网格
        self.prev_hash.clear();
    }

    /// 帧首准备：哈希缓冲对齐 rows×cols。返回 true 表示几何参数变了需全量重绘。
    pub fn begin_frame(&mut self, rows: usize, cols: usize) {
        self.hash_scratch.clear();
        self.hash_scratch.resize(rows * cols, 0);
        self.quads.clear();
    }

    /// 帧尾判定 + 网格组装。返回 Some(mesh) 需要提交绘制（静止帧返回缓存的
    /// 同一 Arc，调用方 clone 后照常 add——egui 每帧都要画，省的是 CPU 侧重建）。
    pub fn end_frame(&mut self, ctx: &egui::Context) -> Option<std::sync::Arc<Mesh>> {
        let changed =
            self.prev_hash != self.hash_scratch || self.tex_version != self.atlas.version;
        if changed {
            self.prev_hash.clear();
            self.prev_hash.extend_from_slice(&self.hash_scratch);
            if self.tex.is_none() || self.tex_version != self.atlas.version {
                self.tex = Some(ctx.load_texture(
                    "term_glyph_atlas",
                    self.atlas.image(),
                    TextureOptions::LINEAR,
                ));
                self.tex_version = self.atlas.version;
            }
            let tid = self.tex.as_ref().unwrap().id();
            self.mesh = Some(std::sync::Arc::new(build_mesh(&self.quads, tid)));
        }
        self.mesh.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_atlas(ppp: f32) -> GlyphAtlas {
        GlyphAtlas::new(
            &[(epaint_default_fonts::HACK_REGULAR.to_vec(), 0)],
            14.0,
            ppp,
        )
    }

    /// ASCII 字形应成功光栅化入库，且二次查询走缓存返回同一槽位。
    #[test]
    fn ascii_glyph_rasterizes_and_caches() {
        let mut a = test_atlas(1.0);
        let s1 = a.glyph('A');
        assert!(s1.w > 0.0 && s1.h > 0.0, "'A' 应有可见笔画");
        assert!(s1.dy <= 0.0 && s1.dy >= -40.0, "字形顶应在基线上方合理范围内 dy={}", s1.dy);
        assert!((0.0..=1.0).contains(&s1.u0) && (0.0..=1.0).contains(&s1.v1));
        let s2 = a.glyph('A');
        assert_eq!((s1.u0, s1.v0), (s2.u0, s2.v0), "二次查询应命中缓存");
        let v_before = a.version;
        a.glyph('B');
        assert_eq!(a.version, v_before + 1, "新增字形应推进版本号");
    }

    /// 缺字形（emoji）应得空槽（负缓存），调用方回落 galley 路径。
    #[test]
    fn missing_glyph_returns_empty_slot() {
        let mut a = test_atlas(1.0);
        let s = a.glyph('\u{1F600}'); // 😀 Hack 无此字形
        assert_eq!(s.w, 0.0, "缺字形应为空槽");
    }

    /// 基线应在格子内部（内容行高垂直居中）。
    #[test]
    fn baseline_inside_cell() {
        let a = test_atlas(2.0); // 高 DPI
        let b = a.baseline_rel(20.0);
        assert!(b > 0.0 && b < 20.0, "baseline={b}");
        assert!(a.underline_rel(20.0) > b);
    }

    /// Mesh 组装：quad 数 × 4 顶点 × 6 索引，索引引用合法。
    #[test]
    fn build_mesh_layout() {
        let quads = [CellQuad {
            rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(10.0, 20.0)),
            uv0: Pos2::ZERO,
            uv1: Pos2::new(1.0, 1.0),
            color: Color32::WHITE,
        }];
        let m = build_mesh(&quads, egui::TextureId::Managed(0));
        assert_eq!(m.vertices.len(), 4);
        assert_eq!(m.indices.len(), 6);
        assert!(m.is_valid());
    }

    /// 哈希搅拌确定性 + 对不同输入产生不同值（防碰撞冒烟）。
    #[test]
    fn hash_mix_deterministic() {
        let mut h1 = 0xcbf2_9ce4_8422_2325u64;
        hash_mix(&mut h1, 'a' as u64);
        let mut h2 = 0xcbf2_9ce4_8422_2325u64;
        hash_mix(&mut h2, 'a' as u64);
        assert_eq!(h1, h2);
        let mut h3 = h1;
        hash_mix(&mut h3, 'b' as u64);
        assert_ne!(h1, h3);
    }
}
