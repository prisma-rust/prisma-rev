//! 2D sprite atlas packer and `.ratlas` / `.rtex` generator.
//!
//! Features:
//! - Alpha trimming (crops transparent borders, preserving `original_size` and `trimmed_offset`)
//! - Edge extrusion (1px or more bleeding replication) and padding
//! - MaxRects (BSSF) 2D bin packing
//! - Compact power-of-two (POT) texture auto-sizing
//! - Emits binary `.ratlas` metadata and `.rtex` texture payload
//! - Content-addressed incremental build hash (xxh3-64)

use crate::cooker::CookError;
use prism_asset::formats::{
    encode_ratl, hash_sprite_name, AtlasSpriteEntry, RatlInfo, RTEX_FORMAT_RGBA8,
};
use xxhash_rust::xxh3::xxh3_64;

/// Input raw sprite image (untrimmed RGBA8 pixels).
#[derive(Debug, Clone, PartialEq)]
pub struct RawSpriteInput {
    /// Sprite name (e.g. `"character/idle_0"`).
    pub name: String,
    /// Original image width.
    pub width: u32,
    /// Original image height.
    pub height: u32,
    /// Contiguous RGBA8 pixel buffer (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
    /// Normalized pivot `[pivot_x, pivot_y]` (0..1, defaults to `[0.5, 0.5]`).
    pub pivot: [f32; 2],
    /// 9-slice borders `[top, right, bottom, left]` in pixels (0 = no 9-slice).
    pub nine_slice: [u32; 4],
}

impl RawSpriteInput {
    /// Create a raw sprite with default centred pivot and no 9-slice borders.
    pub fn new(name: impl Into<String>, width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            width,
            height,
            rgba,
            pivot: [0.5, 0.5],
            nine_slice: [0, 0, 0, 0],
        }
    }
}

/// Options configuring the atlas bin packing process.
#[derive(Debug, Clone)]
pub struct AtlasPackOptions {
    /// Maximum texture size (width and height upper bound, e.g. 2048 or 4096).
    pub max_size: u32,
    /// Minimum initial probe dimension (defaults to 64).
    pub min_size: u32,
    /// Inter-sprite spacing gap in pixels (defaults to 1).
    pub padding: u32,
    /// Edge extrusion / bleeding pixels outward (defaults to 1).
    pub extrude: u32,
    /// Whether to crop transparent outer margins (defaults to true).
    pub trim_alpha: bool,
    /// Alpha threshold below or equal to which a pixel is treated as transparent (defaults to 0).
    pub alpha_threshold: u8,
    /// Whether to force power-of-two texture dimensions (defaults to true).
    pub power_of_two: bool,
    /// Relative path to the `.rtex` texture stored in `.ratlas`.
    pub texture_path: String,
}

impl Default for AtlasPackOptions {
    fn default() -> Self {
        Self {
            max_size: 2048,
            min_size: 64,
            padding: 1,
            extrude: 1,
            trim_alpha: true,
            alpha_threshold: 0,
            power_of_two: true,
            texture_path: String::new(),
        }
    }
}

/// Output result of an atlas packing run.
#[derive(Debug, Clone)]
pub struct PackedAtlasResult {
    /// Atlas metadata (serializable to `.ratlas` via `encode_ratl(&self.info)`).
    pub info: RatlInfo,
    /// Assembled RGBA8 atlas pixel buffer (including extrusion pixels).
    pub rgba: Vec<u8>,
    /// Final atlas texture width.
    pub width: u32,
    /// Final atlas texture height.
    pub height: u32,
    /// 64-bit content hash of all input sprites for incremental caching.
    pub content_hash: u64,
}

impl PackedAtlasResult {
    /// Serialize atlas metadata into `.ratlas` binary bytes.
    pub fn encode_ratlas_bytes(&self) -> Vec<u8> {
        encode_ratl(&self.info)
    }

    /// Encode atlas pixels into an uncompressed single-mip RGBA8 RTEX blob.
    pub fn encode_rtex_bytes(&self) -> Vec<u8> {
        encode_rtex_rgba8(self.width, self.height, &self.rgba)
    }
}

/// Wrap RGBA8 pixel data into a single-mip RTEX binary format.
pub fn encode_rtex_rgba8(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    const RTEX_MAGIC: &[u8; 4] = b"RTEX";
    let levels = 1u32;
    let header_size = 4 + 1 + 4 + 4 + 4 + 1 + (levels as usize * 4);
    let mut buf = Vec::with_capacity(header_size + rgba.len());

    buf.extend_from_slice(RTEX_MAGIC);
    buf.push(1); // version
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(&levels.to_le_bytes());
    buf.push(RTEX_FORMAT_RGBA8);

    let mip0_offset = header_size as u32;
    buf.extend_from_slice(&mip0_offset.to_le_bytes());
    buf.extend_from_slice(rgba);

    buf
}

/// Crop transparent outer margins of an RGBA8 image.
///
/// Returns `(trimmed_width, trimmed_height, [offset_x, offset_y], trimmed_rgba)`.
pub fn trim_transparent_pixels(
    width: u32,
    height: u32,
    rgba: &[u8],
    alpha_threshold: u8,
) -> (u32, u32, [i32; 2], Vec<u8>) {
    if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
        return (1, 1, [0, 0], vec![0, 0, 0, 0]);
    }

    let mut min_x = width;
    let mut max_x = 0;
    let mut min_y = height;
    let mut max_y = 0;
    let mut has_solid = false;

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4 + 3) as usize;
            if idx < rgba.len() && rgba[idx] > alpha_threshold {
                has_solid = true;
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }

    if !has_solid {
        // Completely transparent sprite retains 1x1 placeholder to avoid zero-dimension divisions
        return (1, 1, [0, 0], vec![0, 0, 0, 0]);
    }

    let trimmed_w = max_x - min_x + 1;
    let trimmed_h = max_y - min_y + 1;
    let mut trimmed_rgba = Vec::with_capacity((trimmed_w * trimmed_h * 4) as usize);

    for y in min_y..=max_y {
        let src_start = ((y * width + min_x) * 4) as usize;
        let src_end = src_start + (trimmed_w * 4) as usize;
        trimmed_rgba.extend_from_slice(&rgba[src_start..src_end]);
    }

    (
        trimmed_w,
        trimmed_h,
        [min_x as i32, min_y as i32],
        trimmed_rgba,
    )
}

// ---------------------------------------------------------------------------
// MaxRects 2D Bin Packing Algorithm (Best Short Side Fit - BSSF)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Rect {
    fn right(&self) -> u32 {
        self.x + self.w
    }

    fn bottom(&self) -> u32 {
        self.y + self.h
    }

    fn contains(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }
}

struct MaxRectsBin {
    free_rects: Vec<Rect>,
}

impl MaxRectsBin {
    fn new(width: u32, height: u32) -> Self {
        Self {
            free_rects: vec![Rect {
                x: 0,
                y: 0,
                w: width,
                h: height,
            }],
        }
    }

    /// Best Short Side Fit (BSSF) positioning heuristic.
    fn find_position_bssf(&self, w: u32, h: u32) -> Option<(Rect, u32, u32)> {
        let mut best_rect = None;
        let mut best_short_side = u32::MAX;
        let mut best_long_side = u32::MAX;

        for r in &self.free_rects {
            if r.w >= w && r.h >= h {
                let leftover_x = r.w - w;
                let leftover_y = r.h - h;
                let short_side = leftover_x.min(leftover_y);
                let long_side = leftover_x.max(leftover_y);

                if short_side < best_short_side
                    || (short_side == best_short_side && long_side < best_long_side)
                {
                    best_rect = Some(Rect {
                        x: r.x,
                        y: r.y,
                        w,
                        h,
                    });
                    best_short_side = short_side;
                    best_long_side = long_side;
                }
            }
        }

        best_rect.map(|r| (r, best_short_side, best_long_side))
    }

    /// Place rectangle and subdivide all intersecting free rectangles.
    fn place_rect(&mut self, placed: Rect) {
        let mut i = 0;
        while i < self.free_rects.len() {
            if self.free_rects[i].intersects(&placed) {
                let free = self.free_rects.swap_remove(i);
                self.split_rect(free, placed);
            } else {
                i += 1;
            }
        }
        self.prune_free_rects();
    }

    fn split_rect(&mut self, free: Rect, placed: Rect) {
        if placed.y > free.y && placed.y < free.bottom() {
            self.free_rects.push(Rect {
                x: free.x,
                y: free.y,
                w: free.w,
                h: placed.y - free.y,
            });
        }
        if placed.bottom() < free.bottom() && placed.bottom() > free.y {
            self.free_rects.push(Rect {
                x: free.x,
                y: placed.bottom(),
                w: free.w,
                h: free.bottom() - placed.bottom(),
            });
        }
        if placed.x > free.x && placed.x < free.right() {
            self.free_rects.push(Rect {
                x: free.x,
                y: free.y,
                w: placed.x - free.x,
                h: free.h,
            });
        }
        if placed.right() < free.right() && placed.right() > free.x {
            self.free_rects.push(Rect {
                x: placed.right(),
                y: free.y,
                w: free.right() - placed.right(),
                h: free.h,
            });
        }
    }

    /// Remove free rectangles that are completely contained within another.
    fn prune_free_rects(&mut self) {
        let mut i = 0;
        while i < self.free_rects.len() {
            let mut j = i + 1;
            let mut remove_i = false;
            while j < self.free_rects.len() {
                if self.free_rects[j].contains(&self.free_rects[i]) {
                    remove_i = true;
                    break;
                }
                if self.free_rects[i].contains(&self.free_rects[j]) {
                    self.free_rects.swap_remove(j);
                } else {
                    j += 1;
                }
            }
            if remove_i {
                self.free_rects.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Atlas Packing Pipeline
// ---------------------------------------------------------------------------

struct PreparedSprite {
    name: String,
    original_size: [u32; 2],
    trimmed_offset: [i32; 2],
    trimmed_w: u32,
    trimmed_h: u32,
    trimmed_rgba: Vec<u8>,
    pivot: [f32; 2],
    nine_slice: [u32; 4],
    slot_w: u32,
    slot_h: u32,
}

/// Pack a list of raw sprites into a single atlas texture and `.ratlas` metadata.
pub fn pack_atlas(
    sprites: &[RawSpriteInput],
    options: &AtlasPackOptions,
) -> Result<PackedAtlasResult, CookError> {
    if sprites.is_empty() {
        return Ok(PackedAtlasResult {
            info: RatlInfo {
                texture_path: options.texture_path.clone(),
                width: 0,
                height: 0,
                sprites: Vec::new(),
            },
            rgba: Vec::new(),
            width: 0,
            height: 0,
            content_hash: 0,
        });
    }

    let mut hash_payload = Vec::new();
    for s in sprites {
        hash_payload.extend_from_slice(s.name.as_bytes());
        hash_payload.extend_from_slice(&s.width.to_le_bytes());
        hash_payload.extend_from_slice(&s.height.to_le_bytes());
        hash_payload.extend_from_slice(&s.pivot[0].to_le_bytes());
        hash_payload.extend_from_slice(&s.pivot[1].to_le_bytes());
        for &slice in &s.nine_slice {
            hash_payload.extend_from_slice(&slice.to_le_bytes());
        }
        hash_payload.extend_from_slice(&s.rgba);
    }
    let content_hash = xxh3_64(&hash_payload);

    let extrude = options.extrude;
    let padding = options.padding;

    let mut prepared: Vec<PreparedSprite> = sprites
        .iter()
        .map(|s| {
            let (tw, th, offset, trimmed_rgba) = if options.trim_alpha {
                trim_transparent_pixels(s.width, s.height, &s.rgba, options.alpha_threshold)
            } else {
                (s.width, s.height, [0, 0], s.rgba.clone())
            };

            let slot_w = tw + extrude * 2 + padding;
            let slot_h = th + extrude * 2 + padding;

            PreparedSprite {
                name: s.name.clone(),
                original_size: [s.width, s.height],
                trimmed_offset: offset,
                trimmed_w: tw,
                trimmed_h: th,
                trimmed_rgba,
                pivot: s.pivot,
                nine_slice: s.nine_slice,
                slot_w,
                slot_h,
            }
        })
        .collect();

    // Sort descending by height, then width (standard bin packing heuristic)
    prepared.sort_by(|a, b| {
        b.slot_h
            .cmp(&a.slot_h)
            .then_with(|| b.slot_w.cmp(&a.slot_w))
    });

    for p in &prepared {
        if p.slot_w > options.max_size || p.slot_h > options.max_size {
            return Err(CookError::CookFailed(format!(
                "Sprite '{}' (slot {}x{}) exceeds maximum atlas size ({}x{})",
                p.name, p.slot_w, p.slot_h, options.max_size, options.max_size
            )));
        }
    }

    let mut current_w = options.min_size.max(32);
    let mut current_h = options.min_size.max(32);

    for p in &prepared {
        while current_w < p.slot_w && current_w < options.max_size {
            current_w = (current_w * 2).min(options.max_size);
        }
        while current_h < p.slot_h && current_h < options.max_size {
            current_h = (current_h * 2).min(options.max_size);
        }
    }

    // Grow the atlas until every slot fits, then keep that placement.
    let placements = loop {
        let mut bin = MaxRectsBin::new(current_w, current_h);
        let mut candidate_placements = Vec::with_capacity(prepared.len());
        let mut all_fit = true;

        for p in &prepared {
            if let Some((rect, _, _)) = bin.find_position_bssf(p.slot_w, p.slot_h) {
                bin.place_rect(rect);
                candidate_placements.push((p, rect.x, rect.y));
            } else {
                all_fit = false;
                break;
            }
        }

        if all_fit {
            break candidate_placements;
        }

        if current_w <= current_h && current_w * 2 <= options.max_size {
            current_w *= 2;
        } else if current_h * 2 <= options.max_size {
            current_h *= 2;
        } else if current_w * 2 <= options.max_size {
            current_w *= 2;
        } else {
            return Err(CookError::CookFailed(format!(
                "Sprites could not fit within the maximum atlas size of {}x{}",
                options.max_size, options.max_size
            )));
        }
    };

    let atlas_w = current_w;
    let atlas_h = current_h;
    let mut atlas_rgba = vec![0u8; (atlas_w * atlas_h * 4) as usize];
    let mut sprite_entries = Vec::with_capacity(placements.len());

    for (p, slot_x, slot_y) in placements {
        let px = slot_x + extrude;
        let py = slot_y + extrude;
        let tw = p.trimmed_w;
        let th = p.trimmed_h;

        for row in 0..th {
            let dst_offset = (((py + row) * atlas_w + px) * 4) as usize;
            let src_offset = ((row * tw) * 4) as usize;
            let len = (tw * 4) as usize;
            atlas_rgba[dst_offset..dst_offset + len]
                .copy_from_slice(&p.trimmed_rgba[src_offset..src_offset + len]);
        }

        if extrude > 0 {
            for e in 1..=extrude {
                let dst_top = (((py - e) * atlas_w + px) * 4) as usize;
                let src_top = ((py * atlas_w + px) * 4) as usize;
                let len = (tw * 4) as usize;
                atlas_rgba.copy_within(src_top..src_top + len, dst_top);

                let dst_bot = (((py + th - 1 + e) * atlas_w + px) * 4) as usize;
                let src_bot = (((py + th - 1) * atlas_w + px) * 4) as usize;
                atlas_rgba.copy_within(src_bot..src_bot + len, dst_bot);
            }

            let total_h = th + extrude * 2;
            let start_y = py - extrude;
            for row in 0..total_h {
                let cur_y = start_y + row;
                let edge_left = (((cur_y * atlas_w + px) * 4) as usize)
                    ..(((cur_y * atlas_w + px) * 4 + 4) as usize);
                let edge_right = (((cur_y * atlas_w + px + tw - 1) * 4) as usize)
                    ..(((cur_y * atlas_w + px + tw - 1) * 4 + 4) as usize);

                for e in 1..=extrude {
                    let dst_left = (((cur_y * atlas_w + px - e) * 4) as usize)
                        ..(((cur_y * atlas_w + px - e) * 4 + 4) as usize);
                    let dst_right = (((cur_y * atlas_w + px + tw - 1 + e) * 4) as usize)
                        ..(((cur_y * atlas_w + px + tw - 1 + e) * 4 + 4) as usize);

                    atlas_rgba.copy_within(edge_left.clone(), dst_left.start);
                    atlas_rgba.copy_within(edge_right.clone(), dst_right.start);
                }
            }
        }

        let u0 = px as f32 / atlas_w as f32;
        let v0 = py as f32 / atlas_h as f32;
        let u1 = (px + tw) as f32 / atlas_w as f32;
        let v1 = (py + th) as f32 / atlas_h as f32;

        sprite_entries.push(AtlasSpriteEntry {
            name: p.name.clone(),
            name_hash: hash_sprite_name(&p.name),
            pixel_rect: [px, py, tw, th],
            uv_rect: [u0, v0, u1, v1],
            original_size: p.original_size,
            trimmed_offset: p.trimmed_offset,
            pivot: p.pivot,
            nine_slice: p.nine_slice,
        });
    }

    sprite_entries.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(PackedAtlasResult {
        info: RatlInfo {
            texture_path: options.texture_path.clone(),
            width: atlas_w,
            height: atlas_h,
            sprites: sprite_entries,
        },
        rgba: atlas_rgba,
        width: atlas_w,
        height: atlas_h,
        content_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_asset::formats::decode_ratl;

    #[test]
    fn test_trim_transparent_pixels() {
        let mut pixels = vec![0u8; 4 * 4 * 4];
        for y in 1..3 {
            for x in 1..3 {
                let idx = ((y * 4 + x) * 4) as usize;
                pixels[idx] = 255;
                pixels[idx + 1] = 0;
                pixels[idx + 2] = 0;
                pixels[idx + 3] = 255;
            }
        }

        let (tw, th, offset, trimmed) = trim_transparent_pixels(4, 4, &pixels, 0);
        assert_eq!(tw, 2);
        assert_eq!(th, 2);
        assert_eq!(offset, [1, 1]);
        assert_eq!(trimmed.len(), 2 * 2 * 4);
        assert_eq!(&trimmed[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn test_trim_completely_transparent() {
        let pixels = vec![0u8; 8 * 8 * 4];
        let (tw, th, offset, trimmed) = trim_transparent_pixels(8, 8, &pixels, 0);
        assert_eq!(tw, 1);
        assert_eq!(th, 1);
        assert_eq!(offset, [0, 0]);
        assert_eq!(trimmed.len(), 4);
    }

    #[test]
    fn test_pack_atlas_basic_and_extrusion() {
        let sprite_a_pixels = [255, 0, 0, 255].repeat(16 * 16);
        let sprite_b_pixels = [0, 255, 0, 255].repeat(32 * 16);

        let input = vec![
            RawSpriteInput::new("sprite_a", 16, 16, sprite_a_pixels),
            RawSpriteInput::new("sprite_b", 32, 16, sprite_b_pixels),
        ];

        let opts = AtlasPackOptions {
            max_size: 512,
            min_size: 64,
            padding: 1,
            extrude: 1,
            trim_alpha: false,
            alpha_threshold: 0,
            power_of_two: true,
            texture_path: "atlas.rtex".into(),
        };

        let result = pack_atlas(&input, &opts).expect("pack_atlas should succeed");
        assert_eq!(result.width, 64);
        assert_eq!(result.height, 64);
        assert_eq!(result.info.sprites.len(), 2);

        let entry_a = result.info.find_sprite("sprite_a").unwrap();
        let entry_b = result.info.find_sprite("sprite_b").unwrap();
        assert_eq!(entry_a.pixel_rect[2], 16);
        assert_eq!(entry_a.pixel_rect[3], 16);
        assert_eq!(entry_b.pixel_rect[2], 32);
        assert_eq!(entry_b.pixel_rect[3], 16);

        let ax = entry_a.pixel_rect[0] as usize;
        let ay = entry_a.pixel_rect[1] as usize;
        let left_pixel_idx = (ay * (result.width as usize) + (ax - 1)) * 4;
        assert_eq!(
            &result.rgba[left_pixel_idx..left_pixel_idx + 4],
            &[255, 0, 0, 255]
        );

        let ratlas_bytes = result.encode_ratlas_bytes();
        let decoded = decode_ratl(&ratlas_bytes).expect("should decode .ratlas");
        assert_eq!(decoded.texture_path, "atlas.rtex");
        assert_eq!(decoded.sprites.len(), 2);
    }

    #[test]
    fn test_pack_atlas_exceed_max_size() {
        let big_pixels = vec![255; 200 * 200 * 4];
        let input = vec![RawSpriteInput::new("huge", 200, 200, big_pixels)];
        let opts = AtlasPackOptions {
            max_size: 128,
            min_size: 64,
            ..Default::default()
        };
        let res = pack_atlas(&input, &opts);
        assert!(res.is_err(), "should fail when exceeding max_size");
    }
}
