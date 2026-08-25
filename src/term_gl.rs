//! GPU 加速终端渲染器：OpenGL texture atlas + 脏区跟踪。
//!
//! 架构：
//! - GlyphAtlas：把 ASCII 32..127 + CJK 常用字预渲染到一张 GL 纹理
//! - CellBatch：每帧收集脏格的 quad 顶点，上传到 VBO，一次 draw call 画完
//! - DirtyTracker：帧间逐格对比，只标记变化的格子
//!
//! 光标闪烁 = 1 格 dirty，打字 = 1-2 格 dirty，TUI 动画 = 仅变化格。
//! 全屏 TUI 重绘 = rows×cols（和现在一样，但 GPU 侧 quad 提交比 egui 逐格快 10x+）。

/// 单个字符在 atlas 纹理中的 UV 坐标。
#[derive(Clone, Copy, Debug)]
pub struct GlyphUv {
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
}

/// 一帧的脏格记录：(row, col) → 上一帧的渲染状态哈希。
#[derive(Clone, Debug)]
pub struct DirtyTracker {
    /// 上一帧每格状态哈希，index = row * cols + col
    prev: Vec<u64>,
    rows: usize,
    cols: usize,
}

impl DirtyTracker {
    pub fn new() -> Self {
        Self {
            prev: Vec::new(),
            rows: 0,
            cols: 0,
        }
    }

    /// 窗口 resize 后重建缓冲区，全量重绘。
    pub fn resize(&mut self, rows: usize, cols: usize) {
        if self.rows != rows || self.cols != cols {
            self.rows = rows;
            self.cols = cols;
            self.prev.resize(rows * cols, 0);
            self.prev.fill(0);
        }
    }

    /// 标记全量重绘（选区变化等场景）。
    pub fn force_full(&mut self) {
        self.prev.fill(0);
    }

    /// 检查某个格子是否脏（哈希不同或首次出现）。
    /// 返回 true 表示需要重绘，同时更新内部状态。
    #[inline]
    pub fn check_and_update(&mut self, row: usize, col: usize, hash: u64) -> bool {
        let idx = row * self.cols + col;
        if idx >= self.prev.len() {
            return true;
        }
        let old = self.prev[idx];
        if old != hash {
            self.prev[idx] = hash;
            true
        } else {
            false
        }
    }
}

/// 渲染一个格子的 quad 顶点数据。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CellVertex {
    /// 屏幕坐标 (x, y)
    pub pos: [f32; 2],
    /// 纹理 UV
    pub uv: [f32; 2],
    /// 前景色 (RGBA, linear)
    pub color: [f32; 4],
}

/// 一帧的 quad 批次：所有脏格的顶点数据。
pub struct CellBatch {
    /// 顶点数据（每格 6 个顶点 = 2 个三角形）
    pub vertices: Vec<CellVertex>,
    /// 背景色 quad（非默认背景的格子需要额外画背景矩形）
    pub bg_vertices: Vec<CellVertex>,
}

impl CellBatch {
    pub fn new() -> Self {
        Self {
            vertices: Vec::with_capacity(2048),
            bg_vertices: Vec::with_capacity(512),
        }
    }

    pub fn clear(&mut self) {
        self.vertices.clear();
        self.bg_vertices.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty() && self.bg_vertices.is_empty()
    }
}

/// Glyph Atlas：预渲染字符到 GL 纹理。
///
/// 布局：每行 16 个字符，每列 ceil(256/16)=16 行（ASCII），
/// CJK 单独区域。纹理尺寸自动对齐到 2 的幂。
pub struct GlyphAtlas {
    /// ASCII 字符 (32..127) 的 UV 映射
    pub ascii_uvs: Vec<Option<GlyphUv>>, // index = char - 32
    /// 纹理宽度/高度（像素）
    pub tex_width: u32,
    pub tex_height: u32,
    /// 每个字符格子的像素尺寸
    pub glyph_w: u32,
    pub glyph_h: u32,
    /// GL 纹理 ID（由调用方创建后填入）
    pub texture_id: Option<u32>,
}

impl GlyphAtlas {
    /// 创建 atlas 元数据（纹理由 OpenGL 侧创建）。
    /// `cell_px` = 每个字符格子的像素大小（如 14pt ≈ 19px）。
    pub fn new(cell_w_px: u32, cell_h_px: u32) -> Self {
        let glyphs_per_row = 16u32;
        let ascii_rows = 128u32 / glyphs_per_row; // 8 rows for 32..159
        let cjk_rows = 4u32; // CJK 预留区
        let total_rows = ascii_rows + cjk_rows;

        let tex_w = glyphs_per_row * cell_w_px;
        let tex_h = total_rows * cell_h_px;

        // UV 映射：ASCII 32..127
        let mut ascii_uvs = Vec::with_capacity(96);
        for i in 0u32..96 {
            let col = (i % glyphs_per_row) as f32;
            let row = (i / glyphs_per_row) as f32;
            let u0 = col * cell_w_px as f32 / tex_w as f32;
            let v0 = row * cell_h_px as f32 / tex_h as f32;
            let u1 = (col + 1.0) * cell_w_px as f32 / tex_w as f32;
            let v1 = (row + 1.0) * cell_h_px as f32 / tex_h as f32;
            ascii_uvs.push(Some(GlyphUv { u0, v0, u1, v1 }));
        }

        Self {
            ascii_uvs,
            tex_width: tex_w,
            tex_height: tex_h,
            glyph_w: cell_w_px,
            glyph_h: cell_h_px,
            texture_id: None,
        }
    }

    /// 获取字符的 UV 坐标。
    #[inline]
    pub fn get_uv(&self, ch: char) -> Option<GlyphUv> {
        let code = ch as u32;
        if code >= 32 && code < 128 {
            self.ascii_uvs[(code - 32) as usize]
        } else {
            // CJK 字符需要动态查找或使用默认 UV
            None
        }
    }
}

/// 构建格子渲染哈希：用于脏区对比。
/// 与 terminal.rs 中的 cell_hash 保持一致。
#[inline]
pub fn cell_hash(ch: char, fg: [u8; 4], bg: [u8; 4], underlined: bool, selected: bool) -> u64 {
    let mut h = ch as u64;
    let fg32 = (fg[0] as u64) << 24 | (fg[1] as u64) << 16 | (fg[2] as u64) << 8 | fg[3] as u64;
    let bg32 = (bg[0] as u64) << 24 | (bg[1] as u64) << 16 | (bg[2] as u64) << 8 | bg[3] as u64;
    h = h.wrapping_mul(0x1_0000_001B).wrapping_add(fg32);
    h = h.wrapping_mul(0x1_0000_001B).wrapping_add(bg32);
    if underlined {
        h ^= 1 << 62;
    }
    if selected {
        h ^= 1 << 63;
    }
    h
}
