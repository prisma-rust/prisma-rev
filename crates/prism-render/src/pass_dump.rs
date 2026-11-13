//! Offline per-pass image dumps for headless debugging.
//!
//! A headless [`crate::GraphRenderer`] has no swapchain, so
//! [`crate::screenshot`] cannot capture anything — it needs a
//! `PRESENT_SRC_KHR` image. This module fills that gap: at the end of a chosen
//! frame it copies the render graph's published attachments into host-visible
//! buffers and writes them out as PNGs once the frame fence has been waited on.
//!
//! Enable with:
//!
//! * `PRISMREV_PASS_DUMP=<dir>` — output directory (required).
//! * `PRISMREV_PASS_DUMP_FRAME=<n>` — 1-based render frame to capture on,
//!   default `200` (late enough for path-tracing geometry uploads to land).
//!
//! Output lands in `<dir>/frame_<n>/<handle>.png`. Values are gamma encoded for
//! readability; the dump is a diagnostic image, not a colour reference.

use std::path::PathBuf;

use ash::vk;

/// How a dumped attachment's channels should be mapped to 8-bit greyscale/RGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DumpKind {
    /// HDR radiance: clamp to `[0, 1]` then gamma encode.
    Radiance,
    /// View-space normals in `[-1, 1]`: remap to `[0, 1]`.
    Normal,
    /// `D32_SFLOAT` depth: compress non-linearly so distant geometry is visible.
    Depth,
}

/// One recorded readback waiting for the frame fence.
pub(crate) struct PendingDump {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    pub kind: DumpKind,
    pub path: PathBuf,
}

/// Dump state owned by [`crate::render_graph::RenderGraph`].
///
/// When `PRISMREV_PASS_DUMP` is unset every method is a cheap no-op, which is
/// the normal case.
pub(crate) struct PassDump {
    dir: Option<PathBuf>,
    target_frame: u64,
    frame_counter: u64,
    captured: bool,
    pending: Vec<PendingDump>,
}

impl PassDump {
    /// Read the dump configuration from the environment.
    pub(crate) fn from_env() -> Self {
        let dir = std::env::var("PRISMREV_PASS_DUMP")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);

        let target_frame = std::env::var("PRISMREV_PASS_DUMP_FRAME")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(200);

        if let Some(dir) = dir.as_ref() {
            log::info!(
                "pass dump: enabled, dir={} frame={target_frame}",
                dir.display()
            );
        }

        Self {
            dir,
            target_frame,
            frame_counter: 0,
            captured: false,
            pending: Vec::new(),
        }
    }

    /// Advance the frame counter and report whether this frame should capture.
    pub(crate) fn should_capture(&mut self) -> bool {
        if self.dir.is_none() || self.captured {
            return false;
        }
        self.frame_counter += 1;
        self.frame_counter >= self.target_frame
    }

    /// Path a handle's dump is written to.
    pub(crate) fn path_for(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| {
            dir.join(format!("frame_{:04}", self.target_frame))
                .join(format!("{name}.png"))
        })
    }

    /// Queue a buffer copy recorded into `cmd`.
    pub(crate) fn push(&mut self, pending: PendingDump) {
        self.pending.push(pending);
    }

    /// Mark the current frame's capture as complete; later frames are skipped.
    pub(crate) fn mark_captured(&mut self) {
        self.captured = true;
    }

    /// Decode the recorded buffers into PNGs and release them.
    ///
    /// Must run after the frame's fence has signalled (headless `present` waits
    /// on it), otherwise the mapped memory may still be written by the GPU.
    pub(crate) fn flush(&mut self, device: &ash::Device) -> anyhow::Result<()> {
        for pending in self.pending.drain(..) {
            let result = write_png(device, &pending);
            unsafe {
                device.destroy_buffer(pending.buffer, None);
                device.free_memory(pending.memory, None);
            }
            result?;
        }
        Ok(())
    }

    /// Release queued buffers without writing them (renderer teardown).
    pub(crate) fn discard(&mut self, device: &ash::Device) {
        for pending in self.pending.drain(..) {
            unsafe {
                device.destroy_buffer(pending.buffer, None);
                device.free_memory(pending.memory, None);
            }
        }
    }
}

/// Bytes occupied by one texel.
pub(crate) fn texel_size(format: vk::Format) -> u64 {
    match format {
        vk::Format::R16G16B16A16_SFLOAT => 8,
        vk::Format::R32G32B32A32_SFLOAT => 16,
        vk::Format::D32_SFLOAT => 4,
        _ => 4,
    }
}

/// Bytes needed for a full readback of `extent` at `format`.
pub(crate) fn image_bytes(extent: vk::Extent2D, format: vk::Format) -> u64 {
    extent.width as u64 * extent.height as u64 * texel_size(format)
}

fn write_png(device: &ash::Device, pending: &PendingDump) -> anyhow::Result<()> {
    let bytes = image_bytes(pending.extent, pending.format);
    let raw = unsafe {
        let ptr = device.map_memory(pending.memory, 0, bytes, vk::MemoryMapFlags::empty())?;
        let data = std::slice::from_raw_parts(ptr as *const u8, bytes as usize).to_vec();
        device.unmap_memory(pending.memory);
        data
    };

    log_stats(
        &raw,
        pending.extent,
        pending.format,
        pending.kind,
        &pending.path,
    );

    let rgba = decode_to_rgba(&raw, pending.extent, pending.format, pending.kind);
    crate::screenshot::save_rgba_png(
        &pending.path,
        pending.extent.width,
        pending.extent.height,
        &rgba,
        None,
    )?;
    Ok(())
}

/// Log min / mean / max of the dumped channels.
///
/// A black PNG is ambiguous: the pass may have written real zeros, or NaNs that
/// `to_byte` clamps to zero. These numbers tell the two apart.
fn log_stats(
    pixels: &[u8],
    extent: vk::Extent2D,
    format: vk::Format,
    kind: DumpKind,
    path: &std::path::Path,
) {
    let count = extent.width as usize * extent.height as usize;
    if count == 0 {
        return;
    }
    let texel = texel_size(format) as usize;

    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f64;
    let mut samples = 0usize;
    let mut nan = 0usize;
    let channels = if kind == DumpKind::Depth { 1 } else { 3 };

    for i in 0..count {
        let base = i * texel;
        for c in 0..channels {
            let value = if format == vk::Format::R16G16B16A16_SFLOAT {
                read_f16(pixels, base + c * 2)
            } else {
                read_f32(pixels, base + c * 4)
            };
            if !value.is_finite() {
                nan += 1;
                continue;
            }
            min = min.min(value);
            max = max.max(value);
            sum += value as f64;
            samples += 1;
        }
    }

    if samples == 0 {
        log::warn!(
            "pass dump: {} has no finite samples ({nan} non-finite of {})",
            path.display(),
            count * channels
        );
        return;
    }

    log::info!(
        "pass dump: {} min={min:.6} mean={:.6} max={max:.6} non_finite={nan}",
        path.display(),
        sum / samples as f64
    );
}

/// Expand an IEEE 754 binary16 bit pattern to `f32`.
fn f16_to_f32(bits: u16) -> f32 {
    let sign = (bits >> 15) & 1;
    let exponent = (bits >> 10) & 0x1f;
    let mantissa = bits & 0x3ff;

    let magnitude = if exponent == 0 {
        // Subnormal (or zero).
        mantissa as f32 * 2.0f32.powi(-24)
    } else if exponent == 0x1f {
        if mantissa == 0 {
            f32::INFINITY
        } else {
            f32::NAN
        }
    } else {
        (1.0 + mantissa as f32 / 1024.0) * 2.0f32.powi(exponent as i32 - 15)
    };

    if sign == 1 {
        -magnitude
    } else {
        magnitude
    }
}

fn read_f16(pixels: &[u8], offset: usize) -> f32 {
    f16_to_f32(u16::from_le_bytes([pixels[offset], pixels[offset + 1]]))
}

fn read_f32(pixels: &[u8], offset: usize) -> f32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&pixels[offset..offset + 4]);
    f32::from_le_bytes(word)
}

fn to_byte(value: f32) -> u8 {
    let value = if value.is_finite() { value } else { 0.0 };
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn gamma_encode(value: f32) -> u8 {
    let value = if value.is_finite() { value } else { 0.0 };
    to_byte(value.clamp(0.0, 1.0).powf(1.0 / 2.2))
}

/// Convert a raw attachment readback into the RGBA8 layout a PNG needs.
fn decode_to_rgba(
    pixels: &[u8],
    extent: vk::Extent2D,
    format: vk::Format,
    kind: DumpKind,
) -> Vec<u8> {
    let count = extent.width as usize * extent.height as usize;
    let mut out = vec![0u8; count * 4];
    let texel = texel_size(format) as usize;

    for i in 0..count {
        let base = i * texel;
        let dst = i * 4;
        match kind {
            DumpKind::Depth => {
                // Cleared depth (>= 1) is background; everything nearer gets
                // brighter, compressed so a 10..1000 unit scene is readable.
                let depth = read_f32(pixels, base);
                let value = if depth >= 1.0 {
                    0.0
                } else {
                    1.0 / (1.0 + depth * 64.0)
                };
                let grey = to_byte(value);
                out[dst] = grey;
                out[dst + 1] = grey;
                out[dst + 2] = grey;
                out[dst + 3] = 255;
            }
            DumpKind::Normal => {
                // View normals arrive in [-1, 1]; show them in [0, 1].
                for c in 0..3 {
                    let value = if format == vk::Format::R16G16B16A16_SFLOAT {
                        read_f16(pixels, base + c * 2)
                    } else {
                        read_f32(pixels, base + c * 4)
                    };
                    out[dst + c] = to_byte(value * 0.5 + 0.5);
                }
                out[dst + 3] = 255;
            }
            DumpKind::Radiance => {
                for c in 0..3 {
                    out[dst + c] = if format == vk::Format::R16G16B16A16_SFLOAT {
                        gamma_encode(read_f16(pixels, base + c * 2))
                    } else {
                        gamma_encode(read_f32(pixels, base + c * 4))
                    };
                }
                out[dst + 3] = 255;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats_round_trip_through_f32() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0x4000), 2.0);
        assert_eq!(f16_to_f32(0xbc00), -1.0);
        assert_eq!(f16_to_f32(0x3555), 0.333_251_95);
        assert!(f16_to_f32(0x7c00).is_infinite());
        assert!(f16_to_f32(0x7e00).is_nan());
        // Smallest positive subnormal.
        assert_eq!(f16_to_f32(0x0001), 2.0f32.powi(-24));
    }

    #[test]
    fn radiance_decoding_gamma_encodes_and_clamps() {
        let extent = vk::Extent2D {
            width: 2,
            height: 1,
        };
        // Two RGBA16F texels: (0, 0, 0, 1) and (1, 1, 1, 1).
        let mut raw = Vec::new();
        for value in [0u16, 0, 0, 0x3c00, 0x3c00, 0x3c00, 0x3c00, 0x3c00] {
            raw.extend_from_slice(&value.to_le_bytes());
        }
        let rgba = decode_to_rgba(
            &raw,
            extent,
            vk::Format::R16G16B16A16_SFLOAT,
            DumpKind::Radiance,
        );
        assert_eq!(&rgba[0..3], &[0, 0, 0]);
        assert_eq!(rgba[3], 255);
        assert_eq!(&rgba[4..7], &[255, 255, 255]);
    }

    #[test]
    fn depth_decoding_treats_cleared_depth_as_background() {
        let extent = vk::Extent2D {
            width: 2,
            height: 1,
        };
        let mut raw = Vec::new();
        raw.extend_from_slice(&1.0f32.to_le_bytes()); // cleared
        raw.extend_from_slice(&0.0f32.to_le_bytes()); // at the near plane
        let rgba = decode_to_rgba(&raw, extent, vk::Format::D32_SFLOAT, DumpKind::Depth);
        assert_eq!(rgba[0], 0);
        assert_eq!(rgba[4], 255);
    }
}
