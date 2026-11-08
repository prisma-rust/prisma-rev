//! Export heightmap to image formats (PNG, EXR).

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::heightmap::Heightmap;

/// Output format for the heightmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// 16-bit grayscale PNG (lossless, widely compatible).
    Png,
    /// 32-bit float OpenEXR (full precision, HDR).
    Exr,
}

impl std::str::FromStr for ExportFormat {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "png" => Ok(Self::Png),
            "exr" => Ok(Self::Exr),
            _ => Err(format!("unknown format '{s}', expected 'png' or 'exr'")),
        }
    }
}

/// Export a heightmap to a file.
///
/// The heightmap values should be in [0, 1] range.
/// They will be mapped to the output format's bit depth.
pub fn export_heightmap(hm: &Heightmap, path: &Path, format: ExportFormat) -> Result<()> {
    match format {
        ExportFormat::Png => export_png_16bit(hm, path),
        ExportFormat::Exr => export_exr_32bit(hm, path),
    }
}

/// Export as 16-bit grayscale PNG using image::ImageBuffer.
fn export_png_16bit(hm: &Heightmap, path: &Path) -> Result<()> {
    use image::{ImageBuffer, Luma};

    let w = hm.width;
    let h = hm.height;
    let mut img = ImageBuffer::<Luma<u16>, Vec<u16>>::new(w, h);

    for (y, row) in img.chunks_mut(w as usize).enumerate().take(h as usize) {
        for (x, pixel) in row.iter_mut().enumerate() {
            let v = hm.get(x as u32, y as u32);
            *pixel = (v.clamp(0.0, 1.0) * 65535.0) as u16;
        }
    }

    // Save via DynamicImage to handle PNG encoding.
    let dyn_img = image::DynamicImage::ImageLuma16(img);
    dyn_img
        .save_with_format(path, image::ImageFormat::Png)
        .with_context(|| format!("failed to save PNG to {path:?}"))?;

    log::info!("Exported 16-bit PNG: {path:?} ({}x{})", hm.width, hm.height);
    Ok(())
}

/// Export as 32-bit float EXR (height stored in R channel).
fn export_exr_32bit(hm: &Heightmap, path: &Path) -> Result<()> {
    use image::{ImageFormat, Rgba, Rgba32FImage};

    let w = hm.width as usize;
    let mut img = Rgba32FImage::new(hm.width, hm.height);
    for (y, row) in hm.data.chunks(w).enumerate() {
        for (x, &val) in row.iter().enumerate() {
            img.put_pixel(x as u32, y as u32, Rgba([val, 0.0, 0.0, 1.0]));
        }
    }

    let dyn_img = image::DynamicImage::ImageRgba32F(img);
    dyn_img
        .save_with_format(path, ImageFormat::OpenExr)
        .with_context(|| format!("failed to save EXR to {path:?}"))?;

    log::info!(
        "Exported 32-bit float EXR (R channel): {path:?} ({}x{})",
        hm.width,
        hm.height
    );
    Ok(())
}

/// Auto-detect format from file extension.
pub fn format_from_extension(path: &Path) -> Result<ExportFormat> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("png") => Ok(ExportFormat::Png),
        Some("exr") => Ok(ExportFormat::Exr),
        Some(ext) => bail!("unknown extension '.{ext}', use .png or .exr"),
        None => bail!("file path has no extension, use .png or .exr"),
    }
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
