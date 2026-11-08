//! Pure heightfield data helpers shared by the runtime terrain system and the
//! build-time tools.
//!
//! The engine's `Heightfield` owns the same numbers and delegates its raw
//! parsing, sampling and sculpting here, so `prism-build-pipeline` can edit a
//! raw heightmap without linking the renderer.

/// Brush mode for the terrain sculpt tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SculptMode {
    /// Raise the brush area: `h += strength * falloff` (strength is in height units).
    #[default]
    Raise,
    /// Lower the brush area: `h -= strength * falloff`.
    Lower,
    /// Blend towards the average of the four neighbours; `strength` is a `0..1` factor.
    Smooth,
    /// Blend towards the height at the brush centre; `strength` is a `0..1` factor.
    Flatten,
}

/// Parse a row-major `.raw` heightmap of `cells × cells` little-endian `f32` values.
///
/// Non-finite samples become `0.0`. Returns `None` when the grid is smaller than
/// `2 × 2` or the byte length does not match `cells * cells * 4`.
pub fn decode_heights(bytes: &[u8], cells: usize) -> Option<Vec<f32>> {
    let expected = cells.checked_mul(cells)?.checked_mul(4)?;
    if cells < 2 || bytes.len() != expected {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .map(|value| if value.is_finite() { value } else { 0.0 })
            .collect(),
    )
}

/// Parse a square `.raw` heightmap and infer its resolution.
pub fn decode_heights_auto(bytes: &[u8]) -> Option<(usize, Vec<f32>)> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let samples = bytes.len() / 4;
    let cells = (samples as f64).sqrt() as usize;
    if cells * cells != samples || cells < 2 {
        return None;
    }
    decode_heights(bytes, cells).map(|heights| (cells, heights))
}

/// Serialize row-major heights back to little-endian `f32` bytes.
pub fn encode_heights(heights: &[f32]) -> Vec<u8> {
    heights
        .iter()
        .flat_map(|height| height.to_le_bytes())
        .collect()
}

/// Bilinear sample in normalized `[0, 1]` grid coordinates (out-of-range clamps).
pub fn sample_height(cells: usize, heights: &[f32], u: f32, v: f32) -> f32 {
    let last = (cells - 1) as f32;
    let u = (u.clamp(0.0, 1.0) * last).max(0.0);
    let v = (v.clamp(0.0, 1.0) * last).max(0.0);
    let (x0, y0) = (u.floor() as usize, v.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(cells - 1), (y0 + 1).min(cells - 1));
    let (tx, ty) = (u - x0 as f32, v - y0 as f32);
    let h00 = heights[y0 * cells + x0];
    let h10 = heights[y0 * cells + x1];
    let h01 = heights[y1 * cells + x0];
    let h11 = heights[y1 * cells + x1];
    let top = h00 + (h10 - h00) * tx;
    let bottom = h01 + (h11 - h01) * tx;
    top + (bottom - top) * ty
}

/// Apply one sculpt brush at normalized `(center_u, center_v)`.
///
/// `radius` is also normalized to the terrain width; `strength` is in height
/// units for `Raise` / `Lower` and a `0..1` blend factor for `Smooth` /
/// `Flatten`. Cells outside the radius are untouched and the edge falls off
/// with `(1 - t²)²`. Returns the number of affected cells.
///
/// Non-finite or non-positive arguments are a no-op; the result never contains
/// NaN as long as the input heights are finite.
pub fn sculpt_heights(
    cells: usize,
    heights: &mut [f32],
    center_u: f32,
    center_v: f32,
    radius: f32,
    strength: f32,
    mode: SculptMode,
) -> usize {
    if cells < 2
        || heights.len() != cells * cells
        || !center_u.is_finite()
        || !center_v.is_finite()
        || !radius.is_finite()
        || !strength.is_finite()
        || radius <= 0.0
        || strength <= 0.0
    {
        return 0;
    }
    let last = (cells - 1) as f32;
    let center_u = center_u.clamp(0.0, 1.0);
    let center_v = center_v.clamp(0.0, 1.0);
    let radius = radius.min(1.0) * last;
    if radius <= 0.0 {
        return 0;
    }
    let center_x = center_u * last;
    let center_z = center_v * last;
    let min_x = (center_x - radius).floor().max(0.0) as usize;
    let max_x = (center_x + radius).ceil().min(last) as usize;
    let min_z = (center_z - radius).floor().max(0.0) as usize;
    let max_z = (center_z + radius).ceil().min(last) as usize;
    let original = (mode == SculptMode::Smooth).then(|| heights.to_vec());
    let target = if mode == SculptMode::Flatten {
        sample_height(cells, heights, center_u, center_v)
    } else {
        0.0
    };
    let blend = strength.clamp(0.0, 1.0);

    let mut affected = 0;
    for z in min_z..=max_z {
        for x in min_x..=max_x {
            let dx = x as f32 - center_x;
            let dz = z as f32 - center_z;
            let distance = (dx * dx + dz * dz).sqrt() / radius;
            if distance > 1.0 {
                continue;
            }
            let falloff = (1.0 - distance * distance).max(0.0).powi(2);
            if falloff <= 0.0 {
                continue;
            }
            let index = z * cells + x;
            match mode {
                SculptMode::Raise => heights[index] += strength * falloff,
                SculptMode::Lower => heights[index] -= strength * falloff,
                SculptMode::Smooth => {
                    let original = original.as_ref().expect("smooth keeps a snapshot");
                    let left = original[z * cells + x.saturating_sub(1)];
                    let right = original[z * cells + (x + 1).min(cells - 1)];
                    let up = original[z.saturating_sub(1) * cells + x];
                    let down = original[(z + 1).min(cells - 1) * cells + x];
                    let average = (left + right + up + down) * 0.25;
                    let value = original[index];
                    heights[index] = value + (average - value) * falloff * blend;
                }
                SculptMode::Flatten => {
                    let value = heights[index];
                    heights[index] = value + (target - value) * falloff * blend;
                }
            }
            affected += 1;
        }
    }
    affected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(cells: usize) -> Vec<f32> {
        (0..cells * cells).map(|i| i as f32).collect()
    }

    #[test]
    fn raw_round_trip_keeps_every_sample() {
        let heights = ramp(3);
        let bytes = encode_heights(&heights);
        assert_eq!(decode_heights(&bytes, 3).unwrap(), heights);
    }

    #[test]
    fn raw_decode_rejects_wrong_sizes_and_sanitizes_nan() {
        assert!(decode_heights(&[], 2).is_none());
        assert!(decode_heights(&[0; 4], 2).is_none());
        let mut bytes = encode_heights(&[1.0, f32::NAN, f32::INFINITY, 2.0]);
        assert_eq!(bytes.len(), 16);
        bytes[4..8].copy_from_slice(&f32::NAN.to_le_bytes());
        let decoded = decode_heights(&bytes, 2).unwrap();
        assert_eq!(decoded, vec![1.0, 0.0, 0.0, 2.0]);
    }

    #[test]
    fn auto_decode_infers_a_square_resolution() {
        let (cells, heights) = decode_heights_auto(&encode_heights(&ramp(4))).unwrap();
        assert_eq!((cells, heights.len()), (4, 16));
        assert!(decode_heights_auto(&[0; 12]).is_none());
    }

    #[test]
    fn bilinear_sample_hits_corners_and_the_middle() {
        let heights = vec![0.0, 0.0, 0.0, 10.0];
        assert_eq!(sample_height(2, &heights, 0.0, 0.0), 0.0);
        assert_eq!(sample_height(2, &heights, 1.0, 1.0), 10.0);
        assert_eq!(sample_height(2, &heights, 0.5, 0.5), 2.5);
    }

    #[test]
    fn raise_sculpts_the_brush_center_only() {
        let mut heights = vec![0.0; 9];
        let affected = sculpt_heights(3, &mut heights, 0.5, 0.5, 0.1, 2.0, SculptMode::Raise);
        assert_eq!(affected, 1);
        assert_eq!(heights[4], 2.0);
    }

    #[test]
    fn flatten_blends_towards_the_center_height() {
        let mut heights = vec![0.0, 0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 0.0, 0.0];
        sculpt_heights(3, &mut heights, 0.5, 0.5, 1.0, 1.0, SculptMode::Flatten);
        // Center keeps its height; the edge gets (1 - 0.25)² = 0.5625 of it and
        // the corner (1 - 0.5)² = 0.25 — the brush never overshoots the target.
        assert_eq!(heights[4], 10.0);
        assert!((heights[1] - 5.625).abs() < 1e-4, "{:?}", heights);
        assert!((heights[0] - 2.5).abs() < 1e-4, "{:?}", heights);
        assert!(heights.iter().all(|value| (0.0..=10.0).contains(value)));
    }

    #[test]
    fn invalid_brushes_are_a_no_op() {
        let mut heights = ramp(3);
        let original = heights.clone();
        assert_eq!(
            sculpt_heights(3, &mut heights, 0.5, 0.5, 0.0, 1.0, SculptMode::Raise),
            0
        );
        assert_eq!(
            sculpt_heights(4, &mut heights, 0.5, 0.5, 1.0, 1.0, SculptMode::Raise),
            0
        );
        assert_eq!(heights, original);
    }
}
