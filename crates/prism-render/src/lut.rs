//! Adobe `.cube` 3D LUT parsing for post-process color grading (#502).
//!
//! The renderer only needs the two pieces of information a `.cube` file
//! carries: the edge length (`LUT_3D_SIZE`) and the RGB output triples in
//! red-fastest order. The data is uploaded as a `VK_IMAGE_TYPE_3D`
//! `R32G32B32A32_SFLOAT` image and sampled with hardware trilinear filtering,
//! so no CPU interpolation is needed at render time.
//!
//! Supported subset:
//! - `LUT_3D_SIZE n` with `2 <= n <= 128`;
//! - `TITLE "..."` (ignored);
//! - comments starting with `#`;
//! - `DOMAIN_MIN 0 0 0` / `DOMAIN_MAX 1 1 1` (the defaults). A non-default
//!   input domain would need a shader-side remap, so it is rejected instead of
//!   silently producing a wrong grade;
//! - exactly `n³` data rows of three floats.
//!
//! Output values are clamped to `[0, 1]`: the LUT is applied in display-referred
//! space after tonemapping, where values outside that range cannot be shown.

use std::fmt;

/// A parsed 3D LUT.
#[derive(Clone, Debug, PartialEq)]
pub struct Lut3d {
    /// Edge length (`LUT_3D_SIZE`).
    pub size: u32,
    /// RGB triples in red-fastest order: `index = r + g*size + b*size²`.
    pub data: Vec<[f32; 3]>,
}

impl Lut3d {
    /// The identity LUT: sampling it reproduces the input color exactly.
    pub fn identity(size: u32) -> Self {
        let n = size.max(2);
        let scale = 1.0 / (n - 1) as f32;
        let mut data = Vec::with_capacity((n * n * n) as usize);
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    data.push([r as f32 * scale, g as f32 * scale, b as f32 * scale]);
                }
            }
        }
        Self { size: n, data }
    }

    /// Flatten to tightly packed RGBA `f32` texels for a 3D image upload.
    pub fn to_rgba_f32(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.data.len() * 4);
        for [r, g, b] in &self.data {
            out.extend_from_slice(&[*r, *g, *b, 1.0]);
        }
        out
    }
}

/// Why a `.cube` file could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LutParseError {
    /// No `LUT_3D_SIZE` line.
    MissingSize,
    /// `LUT_3D_SIZE` outside the supported range.
    UnsupportedSize(u32),
    /// A `LUT_1D_SIZE` line (1D LUTs are not supported).
    Unsupported1d,
    /// A `DOMAIN_MIN`/`DOMAIN_MAX` other than 0/1.
    UnsupportedDomain,
    /// A data row that is not three finite numbers.
    MalformedRow { line: usize },
    /// The file has fewer triples than `size³`.
    TooFewEntries { expected: usize, actual: usize },
    /// The file has more triples than `size³`.
    TooManyEntries { expected: usize, actual: usize },
}

impl fmt::Display for LutParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSize => write!(f, "missing LUT_3D_SIZE"),
            Self::UnsupportedSize(size) => {
                write!(f, "unsupported LUT_3D_SIZE {size} (expected 2..=128)")
            }
            Self::Unsupported1d => write!(f, "1D LUTs are not supported; use LUT_3D_SIZE"),
            Self::UnsupportedDomain => {
                write!(
                    f,
                    "only the default DOMAIN_MIN 0 0 0 / DOMAIN_MAX 1 1 1 is supported"
                )
            }
            Self::MalformedRow { line } => {
                write!(f, "line {line}: expected three numbers")
            }
            Self::TooFewEntries { expected, actual } => write!(
                f,
                "expected {expected} RGB triples (LUT_3D_SIZE³), found {actual}"
            ),
            Self::TooManyEntries { expected, actual } => write!(
                f,
                "expected {expected} RGB triples (LUT_3D_SIZE³), found {actual}"
            ),
        }
    }
}

impl std::error::Error for LutParseError {}

/// Parse the text of an Adobe `.cube` file.
pub fn parse_cube(text: &str) -> Result<Lut3d, LutParseError> {
    let mut size: Option<u32> = None;
    let mut data: Vec<[f32; 3]> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        // Strip comments, including the `#` that some exporters put inline.
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        match keyword {
            "TITLE" => {}
            "LUT_3D_SIZE" => {
                let n = parts
                    .next()
                    .and_then(|value| value.parse::<u32>().ok())
                    .ok_or(LutParseError::MissingSize)?;
                if !(2..=128).contains(&n) {
                    return Err(LutParseError::UnsupportedSize(n));
                }
                size = Some(n);
            }
            "LUT_1D_SIZE" => return Err(LutParseError::Unsupported1d),
            "DOMAIN_MIN" => {
                if !parts.all(|value| value.parse::<f32>() == Ok(0.0)) {
                    return Err(LutParseError::UnsupportedDomain);
                }
            }
            "DOMAIN_MAX" => {
                if !parts.all(|value| value.parse::<f32>() == Ok(1.0)) {
                    return Err(LutParseError::UnsupportedDomain);
                }
            }
            // Everything else must be a data row.
            _ => {
                let mut values = [0.0f32; 3];
                let mut count = 0;
                for (slot, value) in values.iter_mut().zip(line.split_whitespace()) {
                    *slot = value
                        .parse::<f32>()
                        .map_err(|_| LutParseError::MalformedRow { line: line_no })?;
                    if !slot.is_finite() {
                        return Err(LutParseError::MalformedRow { line: line_no });
                    }
                    count += 1;
                }
                if count != 3 || line.split_whitespace().count() != 3 {
                    return Err(LutParseError::MalformedRow { line: line_no });
                }
                data.push([
                    values[0].clamp(0.0, 1.0),
                    values[1].clamp(0.0, 1.0),
                    values[2].clamp(0.0, 1.0),
                ]);
            }
        }
    }

    let size = size.ok_or(LutParseError::MissingSize)?;
    let expected = (size * size * size) as usize;
    if data.len() < expected {
        return Err(LutParseError::TooFewEntries {
            expected,
            actual: data.len(),
        });
    }
    if data.len() > expected {
        return Err(LutParseError::TooManyEntries {
            expected,
            actual: data.len(),
        });
    }
    Ok(Lut3d { size, data })
}

#[cfg(test)]
mod tests {
    use super::{parse_cube, Lut3d, LutParseError};

    const IDENTITY_2: &str = "\
TITLE \"identity\"
LUT_3D_SIZE 2
DOMAIN_MIN 0 0 0
DOMAIN_MAX 1 1 1
0 0 0
1 0 0
0 1 0
1 1 0
0 0 1
1 0 1
0 1 1
1 1 1
";

    #[test]
    fn identity_cube_round_trips_with_red_fastest_order() {
        let lut = parse_cube(IDENTITY_2).expect("valid cube");
        assert_eq!(lut.size, 2);
        assert_eq!(lut.data.len(), 8);
        assert_eq!(lut.data, Lut3d::identity(2).data);
        // index 1 is r=1, g=0, b=0 (red changes fastest)
        assert_eq!(lut.data[1], [1.0, 0.0, 0.0]);
        // index 4 is r=0, g=0, b=1 (blue changes slowest)
        assert_eq!(lut.data[4], [0.0, 0.0, 1.0]);
    }

    #[test]
    fn comments_blank_lines_and_inline_comments_are_ignored() {
        let text = "# header\n\nLUT_3D_SIZE 2 # inline\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        assert_eq!(parse_cube(text).unwrap(), Lut3d::identity(2));
    }

    #[test]
    fn values_are_clamped_to_the_display_range() {
        let text = "LUT_3D_SIZE 2\n0 0 0\n2 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        let lut = parse_cube(text).unwrap();
        assert_eq!(lut.data[1], [1.0, 0.0, 0.0]);
    }

    #[test]
    fn errors_are_specific() {
        assert_eq!(parse_cube(""), Err(LutParseError::MissingSize));
        assert_eq!(
            parse_cube("LUT_3D_SIZE 1\n0 0 0\n"),
            Err(LutParseError::UnsupportedSize(1))
        );
        assert_eq!(
            parse_cube("LUT_1D_SIZE 16\n"),
            Err(LutParseError::Unsupported1d)
        );
        assert_eq!(
            parse_cube("LUT_3D_SIZE 2\nDOMAIN_MAX 2 1 1\n"),
            Err(LutParseError::UnsupportedDomain)
        );
        assert_eq!(
            parse_cube("LUT_3D_SIZE 2\n0 0\n"),
            Err(LutParseError::MalformedRow { line: 2 })
        );
        let too_few = parse_cube("LUT_3D_SIZE 2\n0 0 0\n").unwrap_err();
        assert!(matches!(
            too_few,
            LutParseError::TooFewEntries {
                expected: 8,
                actual: 1
            }
        ));
        let too_many = parse_cube(&format!("LUT_3D_SIZE 2\n{}", "0 0 0\n".repeat(9))).unwrap_err();
        assert!(matches!(
            too_many,
            LutParseError::TooManyEntries {
                expected: 8,
                actual: 9
            }
        ));
    }
}
