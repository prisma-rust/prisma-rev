//! Texture thumbnails for the asset panel (#982).
//!
//! The asset panel should show what a texture looks like instead of only its file
//! name. Decoding and scaling live here so the UI framework stays out of the data
//! layer: [`ThumbnailCache`] hands the panel plain RGBA8 pixels.
//!
//! Only the formats enabled on the `image` dependency are decoded (PNG today);
//! everything else is cached as "no thumbnail" so a 600-file sprite folder does not
//! retry the decoder every frame.

use std::collections::HashMap;
use std::path::Path;

use crate::project::{file_stamp, FileStamp};

/// Longest side of a cached thumbnail, in pixels.
pub const DEFAULT_MAX_SIDE: u32 = 64;

/// Files above this size are not decoded at all: a thumbnail is not worth
/// allocating a hundred megabytes for a stray texture sheet.
pub const MAX_DECODE_BYTES: u64 = 32 * 1024 * 1024;

/// Decoded, downscaled RGBA8 image (row-major, 4 bytes per pixel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Thumbnail {
    /// Pixel at `(x, y)`, or `None` when the coordinates are outside the image.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = ((y * self.width + x) * 4) as usize;
        self.rgba
            .get(offset..offset + 4)
            .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Bytes of RGBA data; always `width * height * 4`.
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }

    /// Whether the image is wider than tall (used to lay out preview boxes).
    pub fn is_landscape(&self) -> bool {
        self.width > self.height
    }
}

/// Decodes image bytes and scales them so the longest side is at most `max_side`.
///
/// Returns `None` for unknown / disabled formats and for malformed files.
pub fn decode_thumbnail(bytes: &[u8], max_side: u32) -> Option<Thumbnail> {
    let format = image::guess_format(bytes).ok()?;
    let decoded = image::load_from_memory_with_format(bytes, format).ok()?;
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let max_side = max_side.max(1);
    if width <= max_side && height <= max_side {
        return Some(Thumbnail {
            width,
            height,
            rgba: rgba.into_raw(),
        });
    }
    let scale = max_side as f32 / width.max(height) as f32;
    let target_width = ((width as f32 * scale).round() as u32).max(1);
    let target_height = ((height as f32 * scale).round() as u32).max(1);
    let resized = image::imageops::resize(
        &rgba,
        target_width,
        target_height,
        image::imageops::FilterType::Triangle,
    );
    Some(Thumbnail {
        width: target_width,
        height: target_height,
        rgba: resized.into_raw(),
    })
}

/// Lazily decoded thumbnails keyed by asset path.
///
/// Entries are invalidated by [`FileStamp`] (mtime + length), the same rule the
/// document watcher and the loose-asset scanner use, so re-importing a texture
/// refreshes its thumbnail without restarting the editor.
#[derive(Debug, Default)]
pub struct ThumbnailCache {
    entries: HashMap<String, CacheEntry>,
    decodes: usize,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    stamp: Option<FileStamp>,
    thumbnail: Option<Thumbnail>,
}

impl ThumbnailCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Thumbnail for `relative` under `root`, decoding it once per file version.
    ///
    /// A missing file or an undecodable format is remembered as "no thumbnail";
    /// the caller simply draws the kind icon instead.
    pub fn get(&mut self, root: &Path, relative: &str, max_side: u32) -> Option<&Thumbnail> {
        let stamp = file_stamp(root, relative);
        let fresh = self
            .entries
            .get(relative)
            .map(|entry| entry.stamp == stamp)
            .unwrap_or(false);
        if !fresh {
            let thumbnail = if stamp.is_some() {
                self.decodes += 1;
                load(root, relative, max_side)
            } else {
                None
            };
            self.entries
                .insert(relative.to_string(), CacheEntry { stamp, thumbnail });
        }
        self.entries
            .get(relative)
            .and_then(|entry| entry.thumbnail.as_ref())
    }

    /// Drops every cached thumbnail (used when the project changes).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drops one entry, e.g. after the asset is renamed or deleted.
    pub fn forget(&mut self, relative: &str) {
        self.entries.remove(relative);
    }

    /// Number of decoded files since the cache was created; tests use it to
    /// prove a second lookup does not hit the disk / decoder again.
    pub fn decodes(&self) -> usize {
        self.decodes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn load(root: &Path, relative: &str, max_side: u32) -> Option<Thumbnail> {
    let path = root.join(relative);
    let metadata = std::fs::metadata(&path).ok()?;
    if metadata.len() > MAX_DECODE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    decode_thumbnail(&bytes, max_side)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbaImage};
    use std::io::Cursor;
    use std::path::PathBuf;

    fn png_bytes(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, image::Rgba(color));
        let mut buffer = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut buffer, ImageFormat::Png)
            .expect("encode png");
        buffer.into_inner()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("prism-thumbnails-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn decodes_a_png_at_its_native_size() {
        let bytes = png_bytes(8, 6, [10, 20, 30, 255]);
        let thumbnail = decode_thumbnail(&bytes, DEFAULT_MAX_SIDE).expect("decoded");
        assert_eq!(thumbnail.size(), (8, 6));
        assert_eq!(thumbnail.pixel(0, 0), Some([10, 20, 30, 255]));
        assert_eq!(thumbnail.byte_len(), 8 * 6 * 4);
        assert!(thumbnail.is_landscape());
    }

    #[test]
    fn downscales_until_the_longest_side_fits() {
        let bytes = png_bytes(256, 128, [200, 100, 50, 255]);
        let thumbnail = decode_thumbnail(&bytes, 32).expect("decoded");
        assert_eq!(thumbnail.size(), (32, 16));
        assert_eq!(thumbnail.byte_len(), 32 * 16 * 4);
        assert_eq!(thumbnail.pixel(31, 15).map(|pixel| pixel[3]), Some(255));
    }

    #[test]
    fn a_tall_image_keeps_its_aspect_ratio() {
        let bytes = png_bytes(50, 200, [0, 0, 0, 255]);
        let thumbnail = decode_thumbnail(&bytes, 20).expect("decoded");
        assert_eq!(thumbnail.size(), (5, 20));
        assert!(!thumbnail.is_landscape());
    }

    #[test]
    fn garbage_is_not_a_thumbnail() {
        assert!(decode_thumbnail(b"not an image", DEFAULT_MAX_SIDE).is_none());
    }

    #[test]
    fn the_cache_decodes_each_file_version_once() {
        let root = scratch("cache");
        std::fs::write(root.join("sprite.png"), png_bytes(16, 16, [1, 2, 3, 255])).unwrap();
        let mut cache = ThumbnailCache::new();
        assert_eq!(
            cache.get(&root, "sprite.png", 16).map(|t| t.size()),
            Some((16, 16))
        );
        assert_eq!(
            cache.get(&root, "sprite.png", 16).map(|t| t.size()),
            Some((16, 16))
        );
        assert_eq!(cache.decodes(), 1);

        // Rewriting the file invalidates the entry (different length).
        std::fs::write(root.join("sprite.png"), png_bytes(32, 16, [4, 5, 6, 255])).unwrap();
        assert_eq!(
            cache.get(&root, "sprite.png", 16).map(|t| t.size()),
            Some((16, 8))
        );
        assert_eq!(cache.decodes(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_files_are_cached_as_missing() {
        let root = scratch("missing");
        let mut cache = ThumbnailCache::new();
        assert!(cache.get(&root, "gone.png", 16).is_none());
        assert!(cache.get(&root, "gone.png", 16).is_none());
        assert_eq!(cache.decodes(), 0);
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());

        cache.forget("gone.png");
        assert!(cache.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
