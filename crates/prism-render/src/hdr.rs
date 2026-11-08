//! Radiance RGBE (`.hdr`) loader for the IBL environment map.
//!
//! Decodes the standard 32-bit RLE RGBE format into a linear `Vec<f32>` RGBA
//! buffer (values can exceed 1.0 — that's the point of HDR). The engine uploads
//! this straight into a floating-point equirectangular texture; the PBR shader
//! samples it (with mips) for image-based lighting.

use anyhow::{bail, Context as _};

/// Decode a Radiance `.hdr` (RGBE) byte buffer into `(rgba_f32, width, height)`.
/// `rgba_f32` is row-major, 4 floats per pixel (R,G,B,1).
pub fn load_rgbe(bytes: &[u8]) -> anyhow::Result<(Vec<f32>, u32, u32)> {
    let mut pos = 0usize;

    // --- Header: lines until the resolution line ("-Y H +X W"). ---
    let mut width = 0u32;
    let mut height = 0u32;
    let mut saw_resolution = false;
    while pos < bytes.len() {
        let line_end = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .context("unterminated HDR header")?;
        let line = &bytes[pos..pos + line_end];
        pos += line_end + 1;
        if line.starts_with(b"-Y") {
            // Format: "-Y <H> +X <W>"
            let mut it = line
                .split(|&b| b == b' ' || b == b'\t')
                .filter(|s| !s.is_empty());
            while let Some(tok) = it.next() {
                if tok == b"-Y" {
                    height = it
                        .next()
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| s.parse::<u32>().ok())
                        .context("bad -Y height")?;
                } else if tok == b"+X" {
                    width = it
                        .next()
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| s.parse::<u32>().ok())
                        .context("bad +X width")?;
                }
            }
            saw_resolution = true;
            break;
        }
    }

    if !saw_resolution || width == 0 || height == 0 {
        bail!("HDR resolution line missing or invalid");
    }

    // Checked so hostile/corrupt dimensions cannot overflow the allocation
    // arithmetic before we read a single scanline.
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .context("HDR dimensions overflow")?;
    let float_count = pixel_count
        .checked_mul(4)
        .context("HDR buffer size overflow")?;
    let mut rgba = vec![0.0f32; float_count];
    let mut out = 0usize; // float index

    for _y in 0..height as usize {
        // Detect RLE: a scanline starts with the 2-byte marker 0x02 0x02,
        // followed by the scanline 宽度 (big-endian u16).
        let rle_header = bytes.get(pos..).and_then(|rest| rest.get(..4));
        let rle = rle_header.is_some_and(|h| h[0] == 0x02 && h[1] == 0x02);
        if rle {
            let header = rle_header.expect("checked above");
            let marker_w = u16::from_be_bytes([header[2], header[3]]) as usize;
            pos += 4;
            if marker_w != width as usize {
                bail!("RLE scanline width mismatch: {} != {}", marker_w, width);
            }
            // Each of the 4 channels is RLE-encoded across the whole scanline.
            for ch in 0..4usize {
                let mut x = 0usize;
                while x < width as usize {
                    let count = *bytes.get(pos).context("truncated HDR RLE count byte")? as usize;
                    pos += 1;
                    if count == 0 {
                        bail!("invalid HDR RLE count 0");
                    }
                    if count > 128 {
                        let run = count - 128;
                        if x + run > width as usize {
                            bail!("HDR RLE run overruns scanline");
                        }
                        let val = *bytes.get(pos).context("truncated HDR RLE run value")?;
                        pos += 1;
                        for i in 0..run {
                            rgba[out + (x + i) * 4 + ch] = val as f32;
                        }
                        x += run;
                    } else {
                        if x + count > width as usize {
                            bail!("HDR RLE literals overrun scanline");
                        }
                        let values = bytes
                            .get(pos..)
                            .and_then(|rest| rest.get(..count))
                            .context("truncated HDR RLE literal run")?;
                        for (i, &val) in values.iter().enumerate() {
                            rgba[out + (x + i) * 4 + ch] = val as f32;
                        }
                        pos += count;
                        x += count;
                    }
                }
            }
        } else {
            // Uncompressed: 4 bytes per pixel, row-major.
            let row_len = (width as usize)
                .checked_mul(4)
                .context("HDR row size overflow")?;
            let row = bytes
                .get(pos..)
                .and_then(|rest| rest.get(..row_len))
                .context("truncated HDR scanline")?;
            for x in 0..width as usize {
                let p = &row[x * 4..x * 4 + 4];
                rgba[out + x * 4..out + x * 4 + 4].copy_from_slice(&[
                    p[0] as f32,
                    p[1] as f32,
                    p[2] as f32,
                    p[3] as f32,
                ]);
            }
            pos += row_len;
        }

        // Convert this scanline's RGBE → float RGB, set A = 1.
        for x in 0..width as usize {
            let base = out + x * 4;
            let (r, g, b, e) = (rgba[base], rgba[base + 1], rgba[base + 2], rgba[base + 3]);
            let (rf, gf, bf) = rgbe_to_float(r, g, b, e);
            rgba[base] = rf;
            rgba[base + 1] = gf;
            rgba[base + 2] = bf;
            rgba[base + 3] = 1.0;
        }
        out += (width as usize) * 4;
    }

    Ok((rgba, width, height))
}

/// Radiance RGBE → linear float RGB.
#[inline]
fn rgbe_to_float(r: f32, g: f32, b: f32, e: f32) -> (f32, f32, f32) {
    if e == 0.0 {
        return (0.0, 0.0, 0.0);
    }
    let f = 2.0f32.powf(e - 128.0 - 8.0);
    (r * f, g * f, b * f)
}

#[cfg(test)]
#[path = "hdr_tests.rs"]
mod tests;
