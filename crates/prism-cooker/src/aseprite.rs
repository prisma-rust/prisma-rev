//! Aseprite (.ase / .aseprite) native binary and JSON format parser and cooker.
//!
//! Provides:
//! - Native Aseprite (.ase / .aseprite) binary parser extracting frames, durations, tags, and slices
//! - Supports uncompressed (cel type 0) and zlib-compressed (cel type 2) pixel cels
//! - Aseprite JSON export format parser for spritesheet metadata
//! - Automated atlas packing (`pack_atlas`) and `.ranim2d` 2D animation metadata generation

use crate::atlas::{pack_atlas, AtlasPackOptions, PackedAtlasResult, RawSpriteInput};
use crate::cooker::CookError;
use miniz_oxide::inflate::decompress_to_vec_zlib;
use prism_asset::formats::{Anim2DDirection, Anim2DFrame, Anim2DSlice, Anim2DTag, Ranim2DInfo};
use serde::{Deserialize, Serialize};

/// Parsed frame data extracted from an Aseprite file.
#[derive(Debug, Clone, PartialEq)]
pub struct AsepriteFrame {
    pub index: u32,
    pub duration_ms: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Parsed animation document representation.
#[derive(Debug, Clone, PartialEq)]
pub struct AsepriteDocument {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<AsepriteFrame>,
    pub tags: Vec<Anim2DTag>,
    pub slices: Vec<Anim2DSlice>,
}

/// Cooked Aseprite assets result.
#[derive(Debug, Clone)]
pub struct AsepriteCookResult {
    /// Packed texture atlas containing all animation frames.
    pub atlas: PackedAtlasResult,
    /// 2D animation metadata ready for `.ranim2d` serialization.
    pub anim: Ranim2DInfo,
}

impl AsepriteDocument {
    /// Parse from native Aseprite binary (`.ase` or `.aseprite`) bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, CookError> {
        if data.len() < 128 {
            return Err(CookError::CookFailed(
                "Aseprite file too short: missing 128-byte header".into(),
            ));
        }

        let magic = u16::from_le_bytes(
            data[4..6]
                .try_into()
                .map_err(|_| CookError::CookFailed("Invalid header slice".into()))?,
        );
        if magic != 0xA5E0 {
            return Err(CookError::CookFailed(format!(
                "Invalid Aseprite magic: expected 0xA5E0, found 0x{magic:04X}"
            )));
        }

        let frame_count = u16::from_le_bytes(data[6..8].try_into().unwrap()) as usize;
        let doc_w = u16::from_le_bytes(data[8..10].try_into().unwrap()) as u32;
        let doc_h = u16::from_le_bytes(data[10..12].try_into().unwrap()) as u32;
        let color_depth = u16::from_le_bytes(data[12..14].try_into().unwrap());

        if color_depth != 32 && color_depth != 16 && color_depth != 8 {
            return Err(CookError::CookFailed(format!(
                "Unsupported color depth {color_depth} (expected 32-bit RGBA)"
            )));
        }

        let mut pos = 128;
        let mut frames = Vec::with_capacity(frame_count);
        let mut tags = Vec::new();
        let mut slices = Vec::new();

        for frame_idx in 0..frame_count {
            if pos + 16 > data.len() {
                return Err(CookError::CookFailed("Truncated frame header".into()));
            }

            let frame_bytes = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            let frame_magic = u16::from_le_bytes(data[pos + 4..pos + 6].try_into().unwrap());
            if frame_magic != 0xF1FA {
                return Err(CookError::CookFailed(format!(
                    "Invalid frame magic: expected 0xF1FA, found 0x{frame_magic:04X}"
                )));
            }

            let chunks_old = u16::from_le_bytes(data[pos + 6..pos + 8].try_into().unwrap());
            let duration_ms =
                u16::from_le_bytes(data[pos + 8..pos + 10].try_into().unwrap()) as u32;
            let chunks_new = u32::from_le_bytes(data[pos + 12..pos + 16].try_into().unwrap());

            let chunk_count = if chunks_new > 0 {
                chunks_new as usize
            } else {
                chunks_old as usize
            };

            let frame_end = pos
                .checked_add(frame_bytes)
                .ok_or_else(|| CookError::CookFailed("Frame bytes size overflow".into()))?;
            if frame_end > data.len() {
                return Err(CookError::CookFailed(
                    "Frame extends past file length".into(),
                ));
            }

            let mut chunk_pos = pos + 16;
            let mut frame_canvas = vec![0u8; (doc_w * doc_h * 4) as usize];

            for _ in 0..chunk_count {
                if chunk_pos + 6 > frame_end {
                    break;
                }
                let chunk_size =
                    u32::from_le_bytes(data[chunk_pos..chunk_pos + 4].try_into().unwrap()) as usize;
                let chunk_type =
                    u16::from_le_bytes(data[chunk_pos + 4..chunk_pos + 6].try_into().unwrap());

                if chunk_size < 6 || chunk_pos + chunk_size > frame_end {
                    break;
                }

                let chunk_data = &data[chunk_pos + 6..chunk_pos + chunk_size];

                match chunk_type {
                    // CEL chunk
                    0x2004 => {
                        Self::parse_cel_chunk(
                            chunk_data,
                            color_depth,
                            doc_w,
                            doc_h,
                            &mut frame_canvas,
                        );
                    }
                    // TAGS chunk
                    0x2018 => {
                        Self::parse_tags_chunk(chunk_data, &mut tags);
                    }
                    // SLICE chunk
                    0x2022 => {
                        Self::parse_slice_chunk(chunk_data, &mut slices);
                    }
                    _ => {}
                }

                chunk_pos += chunk_size;
            }

            frames.push(AsepriteFrame {
                index: frame_idx as u32,
                duration_ms: if duration_ms > 0 { duration_ms } else { 100 },
                width: doc_w,
                height: doc_h,
                rgba: frame_canvas,
            });

            pos = frame_end;
        }

        Ok(Self {
            width: doc_w,
            height: doc_h,
            frames,
            tags,
            slices,
        })
    }

    fn parse_cel_chunk(
        chunk_data: &[u8],
        color_depth: u16,
        doc_w: u32,
        doc_h: u32,
        canvas: &mut [u8],
    ) {
        if chunk_data.len() < 16 {
            return;
        }
        let x = i16::from_le_bytes(chunk_data[2..4].try_into().unwrap());
        let y = i16::from_le_bytes(chunk_data[4..6].try_into().unwrap());
        let cel_type = u16::from_le_bytes(chunk_data[7..9].try_into().unwrap());

        let (cel_w, cel_h, cel_pixels) = match cel_type {
            // Raw cel
            0 => {
                if chunk_data.len() < 20 {
                    return;
                }
                let w = u16::from_le_bytes(chunk_data[16..18].try_into().unwrap()) as u32;
                let h = u16::from_le_bytes(chunk_data[18..20].try_into().unwrap()) as u32;
                let raw = chunk_data[20..].to_vec();
                (w, h, raw)
            }
            // Compressed cel (zlib)
            2 => {
                if chunk_data.len() < 20 {
                    return;
                }
                let w = u16::from_le_bytes(chunk_data[16..18].try_into().unwrap()) as u32;
                let h = u16::from_le_bytes(chunk_data[18..20].try_into().unwrap()) as u32;
                let compressed = &chunk_data[20..];
                if let Ok(decompressed) = decompress_to_vec_zlib(compressed) {
                    (w, h, decompressed)
                } else {
                    return;
                }
            }
            _ => return,
        };

        if color_depth == 32 {
            // Blit RGBA8 cel onto canvas
            for cy in 0..cel_h {
                let dst_y = y as i32 + cy as i32;
                if dst_y < 0 || dst_y >= doc_h as i32 {
                    continue;
                }
                for cx in 0..cel_w {
                    let dst_x = x as i32 + cx as i32;
                    if dst_x < 0 || dst_x >= doc_w as i32 {
                        continue;
                    }
                    let src_idx = ((cy * cel_w + cx) * 4) as usize;
                    let dst_idx = ((dst_y as u32 * doc_w + dst_x as u32) * 4) as usize;
                    if src_idx + 4 <= cel_pixels.len() && dst_idx + 4 <= canvas.len() {
                        let sa = cel_pixels[src_idx + 3] as u32;
                        if sa > 0 {
                            canvas[dst_idx..dst_idx + 4]
                                .copy_from_slice(&cel_pixels[src_idx..src_idx + 4]);
                        }
                    }
                }
            }
        }
    }

    fn parse_tags_chunk(chunk_data: &[u8], tags: &mut Vec<Anim2DTag>) {
        if chunk_data.len() < 10 {
            return;
        }
        let count = u16::from_le_bytes(chunk_data[0..2].try_into().unwrap()) as usize;
        let mut pos = 10;
        for _ in 0..count {
            if pos + 19 > chunk_data.len() {
                break;
            }
            let from_frame =
                u16::from_le_bytes(chunk_data[pos..pos + 2].try_into().unwrap()) as u32;
            let to_frame =
                u16::from_le_bytes(chunk_data[pos + 2..pos + 4].try_into().unwrap()) as u32;
            let anim_dir = chunk_data[pos + 4];
            let repeat =
                u16::from_le_bytes(chunk_data[pos + 5..pos + 7].try_into().unwrap()) as u32;
            let name_len =
                u16::from_le_bytes(chunk_data[pos + 17..pos + 19].try_into().unwrap()) as usize;
            pos += 19;
            if pos + name_len > chunk_data.len() {
                break;
            }
            let name = std::str::from_utf8(&chunk_data[pos..pos + name_len])
                .unwrap_or("tag")
                .to_string();
            pos += name_len;

            tags.push(Anim2DTag {
                name,
                from_frame,
                to_frame,
                direction: Anim2DDirection::from_u8(anim_dir),
                repeat,
            });
        }
    }

    fn parse_slice_chunk(chunk_data: &[u8], slices: &mut Vec<Anim2DSlice>) {
        if chunk_data.len() < 14 {
            return;
        }
        let keys_count = u32::from_le_bytes(chunk_data[0..4].try_into().unwrap()) as usize;
        let flags = u32::from_le_bytes(chunk_data[4..8].try_into().unwrap());
        let name_len = u16::from_le_bytes(chunk_data[12..14].try_into().unwrap()) as usize;
        let mut pos = 14;
        if pos + name_len > chunk_data.len() {
            return;
        }
        let name = std::str::from_utf8(&chunk_data[pos..pos + name_len])
            .unwrap_or("slice")
            .to_string();
        pos += name_len;

        let has_pivot = (flags & 2) != 0;

        // Only the first key is read: the runtime slice format stores a single
        // rect per slice, so the remaining keys of a multi-key slice are ignored.
        if keys_count > 0 && pos + 20 <= chunk_data.len() {
            let x = i32::from_le_bytes(chunk_data[pos + 4..pos + 8].try_into().unwrap());
            let y = i32::from_le_bytes(chunk_data[pos + 8..pos + 12].try_into().unwrap());
            let w = u32::from_le_bytes(chunk_data[pos + 12..pos + 16].try_into().unwrap()) as i32;
            let h = u32::from_le_bytes(chunk_data[pos + 16..pos + 20].try_into().unwrap()) as i32;
            pos += 20;

            if (flags & 1) != 0 {
                // Skip 9-slice center (16 bytes)
                pos += 16;
            }

            let pivot = if has_pivot && pos + 8 <= chunk_data.len() {
                let px = i32::from_le_bytes(chunk_data[pos..pos + 4].try_into().unwrap()) as f32;
                let py =
                    i32::from_le_bytes(chunk_data[pos + 4..pos + 8].try_into().unwrap()) as f32;
                Some([px, py])
            } else {
                None
            };

            slices.push(Anim2DSlice {
                name,
                bounds: [x, y, w, h],
                pivot,
            });
        }
    }

    /// Parse from Aseprite JSON export format string.
    pub fn from_json_str(json: &str) -> Result<AsepriteJsonExport, CookError> {
        serde_json::from_str(json).map_err(|e| CookError::CookFailed(e.to_string()))
    }
}

/// JSON representation of exported Aseprite metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsepriteJsonExport {
    pub frames: serde_json::Value,
    pub meta: AsepriteJsonMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsepriteJsonMeta {
    #[serde(default)]
    pub frame_tags: Vec<AsepriteJsonTag>,
    #[serde(default)]
    pub slices: Vec<AsepriteJsonSlice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsepriteJsonTag {
    pub name: String,
    pub from: u32,
    pub to: u32,
    #[serde(default)]
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsepriteJsonSlice {
    pub name: String,
    pub keys: Vec<AsepriteJsonSliceKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsepriteJsonSliceKey {
    pub frame: u32,
    pub bounds: AsepriteJsonRect,
    pub pivot: Option<AsepriteJsonPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsepriteJsonRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsepriteJsonPoint {
    pub x: f32,
    pub y: f32,
}

/// Cook an Aseprite document into an atlas and a `.ranim2d` animation metadata file.
pub fn cook_aseprite(
    doc: &AsepriteDocument,
    base_name: &str,
    atlas_opts: &AtlasPackOptions,
) -> Result<AsepriteCookResult, CookError> {
    if doc.frames.is_empty() {
        return Err(CookError::CookFailed(
            "Aseprite document has no frames".into(),
        ));
    }

    let mut raw_sprites = Vec::with_capacity(doc.frames.len());
    let mut anim_frames = Vec::with_capacity(doc.frames.len());

    for (i, frame) in doc.frames.iter().enumerate() {
        let sprite_name = format!("{base_name}_{i}");
        raw_sprites.push(RawSpriteInput::new(
            sprite_name.clone(),
            frame.width,
            frame.height,
            frame.rgba.clone(),
        ));
        anim_frames.push(Anim2DFrame {
            sprite_name,
            duration_ms: frame.duration_ms,
        });
    }

    let atlas = pack_atlas(&raw_sprites, atlas_opts)?;

    let anim = Ranim2DInfo {
        atlas_path: atlas_opts.texture_path.clone(),
        frames: anim_frames,
        tags: doc.tags.clone(),
        slices: doc.slices.clone(),
    };

    Ok(AsepriteCookResult { atlas, anim })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cook_aseprite_document() {
        let frame0 = AsepriteFrame {
            index: 0,
            duration_ms: 100,
            width: 16,
            height: 16,
            rgba: [255, 0, 0, 255].repeat(16 * 16),
        };
        let frame1 = AsepriteFrame {
            index: 1,
            duration_ms: 120,
            width: 16,
            height: 16,
            rgba: [0, 255, 0, 255].repeat(16 * 16),
        };

        let tag = Anim2DTag {
            name: "walk".into(),
            from_frame: 0,
            to_frame: 1,
            direction: Anim2DDirection::Forward,
            repeat: 0,
        };

        let slice = Anim2DSlice {
            name: "sword".into(),
            bounds: [8, 4, 8, 12],
            pivot: Some([0.5, 0.5]),
        };

        let doc = AsepriteDocument {
            width: 16,
            height: 16,
            frames: vec![frame0, frame1],
            tags: vec![tag],
            slices: vec![slice],
        };

        let opts = AtlasPackOptions {
            max_size: 256,
            min_size: 64,
            texture_path: "atlases/player.ratlas".into(),
            ..Default::default()
        };

        let res = cook_aseprite(&doc, "player", &opts).expect("cook_aseprite should succeed");
        assert_eq!(res.atlas.info.sprites.len(), 2);
        assert_eq!(res.anim.frames.len(), 2);
        assert_eq!(res.anim.tags.len(), 1);
        assert_eq!(res.anim.slices.len(), 1);
        assert_eq!(res.anim.total_duration_ms(), 220);
    }
}
