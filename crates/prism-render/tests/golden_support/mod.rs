//! Golden-frame harness built on the headless `GraphRenderer`.
//!
//! A deterministic scene (one unit box, fixed camera / light / extent) is
//! rendered through the real composed graph. The final RGBA8 image is compared
//! against a committed PNG with a per-channel tolerance, so ordering or
//! resource-lifetime regressions show up as a frame diff instead of only as a
//! validation-layer message.
//!
//! Set `PRISM_UPDATE_GOLDENS=1` to rewrite the committed goldens after an
//! intentional rendering change. On a mismatch the harness writes
//! `target/golden-frames/<name>.actual.png`, `<name>.diff.png` and
//! `<name>.device.txt` for triage. The device file records the adapter /
//! driver identity, so a golden generated on another machine can be told apart
//! from a real rendering regression (#832).
//!
//! `FrameHarness::new` returns `None` when no Vulkan driver is available, so the
//! tests skip gracefully on hosts without one (same convention as
//! `headless_clear.rs`).

#![allow(dead_code)] // Reusable helper surface; individual scenarios use a subset.

use std::path::{Path, PathBuf};

use anyhow::Result;
use prism_render::managers::MeshHandle;
use prism_render::render_graph::PostGrade;
use prism_render::{DrawItem, DrawState, FrameUBOData, GraphRenderer, RenderMode};

/// Per-channel difference that still counts as "the same pixel".
const CHANNEL_TOLERANCE: u8 = 12;
/// Fraction of pixels allowed to exceed [`CHANNEL_TOLERANCE`].
const CHANGED_FRACTION: f32 = 0.05;
/// Mean absolute channel difference allowed across the whole frame.
const MEAN_TOLERANCE: f32 = 3.0;
/// Largest single-channel difference allowed anywhere.
const MAX_TOLERANCE: u8 = 96;

/// Knobs that intentionally change the frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameOptions {
    pub wireframe: bool,
    pub shaded_wireframe: bool,
    pub grade: PostGrade,
}

/// One deterministic offscreen scene plus the real render graph.
pub struct FrameHarness {
    renderer: GraphRenderer,
    mesh: MeshHandle,
    size: usize,
}

impl FrameHarness {
    /// Create an offscreen renderer of `size × size` pixels.
    pub fn new(size: usize) -> Option<Self> {
        assert!(size > 0, "frame size must be positive");
        let mut renderer = match GraphRenderer::offscreen_new(size as u32, size as u32, None) {
            Ok(renderer) => renderer,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                    || message.contains("VK_ERROR_EXTENSION_NOT_PRESENT")
                    || message.contains("no devices")
                    || message.contains("No valid")
                    || message.contains("PhysicalDevice")
                {
                    eprintln!("[SKIP] no Vulkan driver: {error:#}");
                    return None;
                }
                panic!("offscreen_new failed: {error:#}");
            }
        };
        let mesh = renderer
            .register_mesh(&prism_render::primitives::builtin("box").expect("box mesh"))
            .expect("register box");
        Some(Self {
            renderer,
            mesh,
            size,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn supports_wireframe(&self) -> bool {
        self.renderer.context().wireframe_supported
    }

    /// Adapter and driver identity of the device rendering this frame.
    ///
    /// Committed goldens are generated on one driver; another conformant
    /// driver may produce a different (still correct) image. Recording this in
    /// the mismatch report keeps "this machine renders differently" separate
    /// from "the code changed the image" (#832).
    pub fn adapter_info(&self) -> String {
        let properties = &self.renderer.context().physical_device_properties;
        let name = properties
            .device_name_as_c_str()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<invalid device name>".to_string());
        format!(
            "{name} | Vulkan API {} | driver {}",
            format_version(properties.api_version),
            format_version(properties.driver_version)
        )
    }

    /// Render the fixed scene once and return tightly packed RGBA8 pixels.
    pub fn render(&mut self, options: FrameOptions) -> Vec<u8> {
        let (view_proj, view) = camera();
        let frame_data = frame_data(view_proj, view, self.size);
        let item = draw_item(self.mesh);

        self.renderer
            .render(
                std::slice::from_ref(&item),
                &frame_data,
                frame_data.light_view_proj,
                [[0.0; 4]; 4],
                0, // debug flags
                1, // ACES tonemap
                0, // AA off: keep the rasterizer deterministic
                0, // debug target
                view_proj[2][2],
                view_proj[3][2],
                &[],
                RenderMode::Raster,
                0,
                0.0,
                0,
                1.0,
                &[],
                options.grade,
                options.wireframe,
                options.shaded_wireframe,
                &[],
            )
            .expect("headless render failed");
        self.renderer
            .copy_offscreen_to_readback()
            .expect("copy_offscreen_to_readback failed");
        self.renderer
            .readback_pixels()
            .expect("readback_pixels failed")
    }
}

/// Fixed camera: at `(0, 0, 3)` looking down `-Z` at a unit box.
fn camera() -> ([[f32; 4]; 4], [[f32; 4]; 4]) {
    let view = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, -3.0, 1.0],
    ];
    let near = 0.1_f32;
    let far = 100.0_f32;
    let f = 1.0 / (45.0_f32.to_radians() * 0.5).tan();
    let aspect = 1.0_f32;
    let mut proj = [[0.0f32; 4]; 4];
    proj[0][0] = f / aspect;
    proj[1][1] = -f; // Vulkan NDC +Y points down.
    proj[2][2] = far / (near - far);
    proj[2][3] = -1.0;
    proj[3][2] = near * far / (near - far);
    (mul(&proj, &view), view)
}

fn frame_data(view_proj: [[f32; 4]; 4], view: [[f32; 4]; 4], size: usize) -> FrameUBOData {
    FrameUBOData {
        view_proj,
        camera_position: [0.0, 0.0, 3.0, 0.0],
        light_direction: [0.3, 0.8, 0.5, 0.8],
        light_color: [1.0, 0.98, 0.95, 0.25],
        view,
        light_view_proj: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        tonemap_mode: 1,
        viewport_size: [size as f32, size as f32],
        exposure: 1.0,
        // The shadow pass still runs, but the box must not be darkened by the
        // identity light matrix; both drivers then agree on the lit surface.
        shadow_strength: 0.0,
        _pad2: [0.0; 3],
        light_view_proj1: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        shadow_split: 0.0,
        shadow_cascade_count: 1,
        _pad3: [0.0; 2],
    }
}

fn draw_item(mesh: MeshHandle) -> DrawItem {
    DrawItem {
        mesh,
        entity_id: 7,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        material: None,
        cast_shadow: false,
        visible_in_main_pass: true,
        cast_shadow_two_sided: false,
        bone_offset: None,
        state: DrawState::default(),
        world_bounds: None,
        index_start: 0,
        index_count: None,
    }
}

fn mul(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            out[column][row] = (0..4).map(|k| a[k][row] * b[column][k]).sum();
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Golden comparison
// ---------------------------------------------------------------------------

/// The committed golden directory (inside the crate).
pub fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens")
}

/// Where mismatch artifacts are written.
pub fn artifact_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/golden-frames")
        .to_path_buf()
}

fn update_mode() -> bool {
    std::env::var("PRISM_UPDATE_GOLDENS")
        .map(|value| !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// Decode a Vulkan `VK_MAKE_VERSION`-encoded version number.
pub fn format_version(version: u32) -> String {
    format!(
        "{}.{}.{} ({version:#010x})",
        (version >> 22) & 0x3ff,
        (version >> 12) & 0x3ff,
        version & 0xfff
    )
}

/// Compare `actual` with the committed golden and panic with a readable report
/// on failure. In update mode the golden is rewritten instead.
///
/// `adapter` should come from [`FrameHarness::adapter_info`]; it is written to
/// the artifact directory and included in the panic message.
pub fn assert_golden(name: &str, actual: &[u8], size: usize, adapter: &str) {
    assert_eq!(actual.len(), size * size * 4, "unexpected frame size");
    let path = golden_dir().join(format!("{name}.png"));

    if update_mode() {
        std::fs::create_dir_all(golden_dir()).expect("create golden dir");
        save_png(&path, actual, size).expect("write golden");
        eprintln!("[golden] updated {} ({}×{})", path.display(), size, size);
        return;
    }

    let expected = image::open(&path)
        .unwrap_or_else(|error| {
            panic!(
                "missing golden {}: {error}. Run with PRISM_UPDATE_GOLDENS=1",
                path.display()
            )
        })
        .to_rgba8();
    assert_eq!(
        (expected.width() as usize, expected.height() as usize),
        (size, size),
        "golden {} has the wrong dimensions",
        path.display()
    );
    let expected = expected.into_raw();

    let report = compare(&expected, actual);
    if report.is_within_tolerance() {
        return;
    }

    let dir =
        write_mismatch_artifacts(name, actual, &expected, size).expect("write mismatch artifacts");
    let device_path = dir.join(format!("{name}.device.txt"));
    if let Err(error) = std::fs::write(&device_path, adapter) {
        eprintln!(
            "[golden] could not write {}: {error}",
            device_path.display()
        );
    }
    panic!(
        "frame {name:?} differs from {}: {} of {} pixels exceed ±{} (max diff {}, mean {:.2})\n  \
         adapter: {adapter}\n  \
         actual/diff/device info written to {}.\n  \
         Re-run with PRISM_UPDATE_GOLDENS=1 for an intentional change; if only the adapter \
         differs, compare against a frame rendered on the golden's machine (#832).",
        path.display(),
        report.changed_pixels,
        size * size,
        CHANNEL_TOLERANCE,
        report.max_diff,
        report.mean_diff,
        dir.display()
    );
}

/// Summary of an image comparison.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameDiff {
    pub changed_pixels: usize,
    pub total_pixels: usize,
    pub max_diff: u8,
    pub mean_diff: f32,
}

impl FrameDiff {
    pub fn is_within_tolerance(&self) -> bool {
        let changed_limit = (self.total_pixels as f32 * CHANGED_FRACTION) as usize;
        self.changed_pixels <= changed_limit
            && self.mean_diff <= MEAN_TOLERANCE
            && self.max_diff <= MAX_TOLERANCE
    }
}

/// Compare two tightly packed RGBA8 images, ignoring alpha.
pub fn compare(expected: &[u8], actual: &[u8]) -> FrameDiff {
    assert_eq!(expected.len(), actual.len(), "image sizes differ");
    assert_eq!(expected.len() % 4, 0, "RGBA8 images are 4-byte aligned");

    let mut changed_pixels = 0usize;
    let mut max_diff = 0u8;
    let mut sum = 0u64;
    for (expected, actual) in expected
        .as_chunks::<4>()
        .0
        .iter()
        .zip(actual.as_chunks::<4>().0)
    {
        let mut worst = 0u8;
        for channel in 0..3 {
            let diff = expected[channel].abs_diff(actual[channel]);
            worst = worst.max(diff);
            sum += u64::from(diff);
        }
        max_diff = max_diff.max(worst);
        if worst > CHANNEL_TOLERANCE {
            changed_pixels += 1;
        }
    }

    let total_pixels = expected.len() / 4;
    FrameDiff {
        changed_pixels,
        total_pixels,
        max_diff,
        mean_diff: sum as f32 / (total_pixels * 3) as f32,
    }
}

/// Write `actual` and an amplified grayscale `diff` PNG into [`artifact_dir`].
pub fn write_mismatch_artifacts(
    name: &str,
    actual: &[u8],
    expected: &[u8],
    size: usize,
) -> Result<PathBuf> {
    let dir = artifact_dir();
    std::fs::create_dir_all(&dir)?;
    let actual_path = dir.join(format!("{name}.actual.png"));
    save_png(&actual_path, actual, size)?;

    let mut diff = Vec::with_capacity(size * size * 4);
    for (expected, actual) in expected
        .as_chunks::<4>()
        .0
        .iter()
        .zip(actual.as_chunks::<4>().0)
    {
        let value = expected[0]
            .abs_diff(actual[0])
            .max(expected[1].abs_diff(actual[1]))
            .max(expected[2].abs_diff(actual[2]));
        let amplified = value.saturating_mul(4);
        diff.extend_from_slice(&[amplified, amplified, amplified, 255]);
    }
    save_png(&dir.join(format!("{name}.diff.png")), &diff, size)?;
    Ok(dir)
}

fn save_png(path: &Path, pixels: &[u8], size: usize) -> Result<()> {
    image::save_buffer_with_format(
        path,
        pixels,
        size as u32,
        size as u32,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )?;
    Ok(())
}
