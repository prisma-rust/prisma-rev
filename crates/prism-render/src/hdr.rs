//! Radiance RGBE (`.hdr`) loader for the IBL environment map.
//!
//! Decodes the standard 32-bit RLE RGBE format into a linear `Vec<f32>` RGBA
//! buffer (values can exceed 1.0 — that's the point of HDR). The engine uploads
//! this straight into a floating-point equirectangular texture; the PBR shader
//! samples it (with mips) for image-based lighting.

use anyhow::{bail, Context as _};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Axis {
    X,
    Y,
}

/// One half of the resolution line: `±X <len>` or `±Y <len>`.
#[derive(Clone, Copy, Debug)]
struct AxisSpec {
    axis: Axis,
    /// `+` in the file. For X that means left-to-right, for Y bottom-to-top.
    positive: bool,
    len: u32,
}

/// Resolution line split into the scanline ("major") direction and the
/// within-scanline ("minor") direction.
#[derive(Clone, Copy, Debug)]
struct Resolution {
    major: AxisSpec,
    minor: AxisSpec,
}

/// Decode a Radiance `.hdr` (RGBE) byte buffer into `(rgba_f32, width, height)`.
/// `rgba_f32` is row-major, 4 floats per pixel (R,G,B,1).
pub fn load_rgbe(bytes: &[u8]) -> anyhow::Result<(Vec<f32>, u32, u32)> {
    let mut pos = 0usize;

    // --- Header: lines until the resolution line ("±Y <H> ±X <W>"). ---
    let mut resolution = None;
    while pos < bytes.len() {
        let line_end = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .context("unterminated HDR header")?;
        let line = &bytes[pos..pos + line_end];
        pos += line_end + 1;
        if let Some(parsed) = parse_resolution_line(line)? {
            resolution = Some(parsed);
            break;
        }
    }

    let resolution = resolution.context("HDR resolution line missing or invalid")?;
    let Resolution { major, minor } = resolution;

    // The scanline count and the pixels-per-scanline can each be either the
    // image height or width, depending on which axes the file uses.
    let (width, height) = match (major.axis, minor.axis) {
        (Axis::Y, Axis::X) => (minor.len, major.len),
        (Axis::X, Axis::Y) => (major.len, minor.len),
        _ => unreachable!("parse_resolution_line rejects equal axes"),
    };

    // Checked so hostile/corrupt dimensions cannot overflow the allocation
    // arithmetic before we read a single scanline.
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .context("HDR dimensions overflow")?;
    let float_count = pixel_count
        .checked_mul(4)
        .context("HDR buffer size overflow")?;
    let mut rgba = vec![0.0f32; float_count];

    let scanline_pixels = minor.len as usize;
    let mut scanline = vec![0u8; scanline_pixels * 4];

    for s in 0..major.len as usize {
        read_scanline(bytes, &mut pos, scanline_pixels, &mut scanline)?;

        for p in 0..scanline_pixels {
            let x = match major.axis {
                Axis::X => x_at(major, s),
                Axis::Y => x_at(minor, p),
            };
            let y = match major.axis {
                Axis::Y => y_at(major, s),
                Axis::X => y_at(minor, p),
            };
            let src = p * 4;
            let (r, g, b) = rgbe_to_float(
                scanline[src] as f32,
                scanline[src + 1] as f32,
                scanline[src + 2] as f32,
                scanline[src + 3] as f32,
            );
            let dst = (y * width as usize + x) * 4;
            rgba[dst..dst + 4].copy_from_slice(&[r, g, b, 1.0]);
        }
    }

    Ok((rgba, width, height))
}

/// Parse a resolution line such as `-Y 512 +X 1024`.
///
/// Radiance allows both axis orders and both signs on each axis, giving eight
/// orientations. Returns `Ok(None)` when the line is not a resolution line so
/// the header scan can keep going.
fn parse_resolution_line(line: &[u8]) -> anyhow::Result<Option<Resolution>> {
    let Ok(text) = std::str::from_utf8(line) else {
        return Ok(None);
    };
    let mut tokens = text.split([' ', '\t']).filter(|t| !t.is_empty());
    let Some(major) = tokens.next().and_then(parse_axis_token) else {
        return Ok(None);
    };
    let major = AxisSpec {
        len: parse_length(tokens.next())?,
        ..major
    };
    let minor = tokens
        .next()
        .and_then(parse_axis_token)
        .context("HDR resolution line is missing its second axis")?;
    let minor = AxisSpec {
        len: parse_length(tokens.next())?,
        ..minor
    };

    if major.axis == minor.axis {
        bail!(
            "HDR resolution line reuses the {} axis",
            match major.axis {
                Axis::X => "X",
                Axis::Y => "Y",
            }
        );
    }
    if major.len == 0 || minor.len == 0 {
        bail!("HDR resolution line has a zero dimension");
    }

    Ok(Some(Resolution { major, minor }))
}

fn parse_length(token: Option<&str>) -> anyhow::Result<u32> {
    let token = token.context("HDR resolution line is missing a length")?;
    token
        .parse::<u32>()
        .with_context(|| format!("bad HDR resolution length: {token}"))
}

fn parse_axis_token(token: &str) -> Option<AxisSpec> {
    let (axis, positive) = match token {
        "+X" => (Axis::X, true),
        "-X" => (Axis::X, false),
        "+Y" => (Axis::Y, true),
        "-Y" => (Axis::Y, false),
        _ => return None,
    };
    Some(AxisSpec {
        axis,
        positive,
        len: 0,
    })
}

/// Output column for a pixel on the X axis. `+X` runs left-to-right, `-X`
/// right-to-left.
fn x_at(spec: AxisSpec, index: usize) -> usize {
    debug_assert_eq!(spec.axis, Axis::X);
    if spec.positive {
        index
    } else {
        spec.len as usize - 1 - index
    }
}

/// Output row for a pixel on the Y axis. Radiance `-Y` runs from the top of the
/// image down, which is also this buffer's row order, so `+Y` is the flipped
/// case.
fn y_at(spec: AxisSpec, index: usize) -> usize {
    debug_assert_eq!(spec.axis, Axis::Y);
    if spec.positive {
        spec.len as usize - 1 - index
    } else {
        index
    }
}

/// Read one scanline (`pixels` RGBE quads) into `out`, advancing `pos`.
fn read_scanline(
    bytes: &[u8],
    pos: &mut usize,
    pixels: usize,
    out: &mut [u8],
) -> anyhow::Result<()> {
    // Detect RLE: a scanline starts with the 2-byte marker 0x02 0x02,
    // followed by the scanline width (big-endian u16).
    let rle_header = bytes.get(*pos..).and_then(|rest| rest.get(..4));
    let rle = rle_header.is_some_and(|h| h[0] == 0x02 && h[1] == 0x02);
    if rle {
        let header = rle_header.expect("checked above");
        let marker_w = u16::from_be_bytes([header[2], header[3]]) as usize;
        *pos += 4;
        if marker_w != pixels {
            bail!("RLE scanline width mismatch: {} != {}", marker_w, pixels);
        }
        // Each of the 4 channels is RLE-encoded across the whole scanline.
        for ch in 0..4usize {
            let mut x = 0usize;
            while x < pixels {
                let count = *bytes.get(*pos).context("truncated HDR RLE count byte")? as usize;
                *pos += 1;
                if count == 0 {
                    bail!("invalid HDR RLE count 0");
                }
                if count > 128 {
                    let run = count - 128;
                    if x + run > pixels {
                        bail!("HDR RLE run overruns scanline");
                    }
                    let val = *bytes.get(*pos).context("truncated HDR RLE run value")?;
                    *pos += 1;
                    for i in 0..run {
                        out[(x + i) * 4 + ch] = val;
                    }
                    x += run;
                } else {
                    if x + count > pixels {
                        bail!("HDR RLE literals overrun scanline");
                    }
                    let values = bytes
                        .get(*pos..)
                        .and_then(|rest| rest.get(..count))
                        .context("truncated HDR RLE literal run")?;
                    for (i, &val) in values.iter().enumerate() {
                        out[(x + i) * 4 + ch] = val;
                    }
                    *pos += count;
                    x += count;
                }
            }
        }
    } else {
        // Uncompressed: 4 bytes per pixel, row-major within the scanline.
        let row_len = pixels.checked_mul(4).context("HDR row size overflow")?;
        let row = bytes
            .get(*pos..)
            .and_then(|rest| rest.get(..row_len))
            .context("truncated HDR scanline")?;
        out.copy_from_slice(row);
        *pos += row_len;
    }
    Ok(())
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
