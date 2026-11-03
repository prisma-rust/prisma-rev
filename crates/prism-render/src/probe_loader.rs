//! 烘焙全局光照探测器体积的二进制加载器/保存器。
//!
//! 从旧版开发模式 `prism-asset` crate 迁移至此，使全局光照运行时
//! 完全不依赖该 crate。磁盘格式不变（PRPV 魔数，版本 2）；
//! 现有的 `.bin` 文件继续可加载。
//!
//! 文件格式（小端序）：
//!
//! | 偏移 | 大小 | 字段 |
//! |-------:|-----:|-------|
//! | 0 | 4 | 魔数 `b"PRPV"` |
//! | 4 | 4 | 版本 `u32`（当前为 2） |
//! | 8 | 12 | 原点 `[f32; 3]` |
//! | 20 | 12 | 间距 `[f32; 3]` |
//! | 32 | 12 | 维度 `[u32; 3]` |
//! | 44 | 4 | 系数格式 `u32`（0 = f32，1 = f16 - 预留） |
//! | 48 | 64 | 场景名称 `[u8; 64]`（空填充 UTF-8） |
//! | 112 | 4 | 全局命中率 `f32`（每个探测器的均值；-1 = 未知） |
//! | 116 | N | 系数字段：`dims.x*dims.y*dims.z*9*3` 个 f32 值（每个系数 RGB） |
//!
//! Header = 116 bytes. Body = `probe_count * 9 * 3 * 4` bytes (f32).

use std::io;
use std::path::Path;

/// Baked GI probe volume in CPU memory.
///
/// `coeffs` length = `dims[0] * dims[1] * dims[2] * 9`, indexed as
/// `coeffs[(probe_idx * 9 + coeff_idx)]` where probe_idx is row-major
/// `(x + y*dims[0] + z*dims[0]*dims[1])`.
#[derive(Clone, Debug)]
pub struct ProbeVolumeData {
    /// World position of probe `(0,0,0)`.
    pub origin: [f32; 3],
    /// World distance between adjacent probes (per axis).
    pub spacing: [f32; 3],
    /// Probe count per axis (each >= 1).
    pub dims: [u32; 3],
    /// SH coefficients: `dims.x * dims.y * dims.z * 9` RGB triplets.
    pub coeffs: Vec<[f32; 3]>,
    /// Name of the scene this 音量 was baked for (from the scene manifest).
    /// Used at runtime to reject a `.bin` baked for a different scene
    /// (prevents silent wrong-scene GI). Empty for v1 files / unknown scenes
    /// -> the runtime skips the binding check.
    pub scene_name: String,
    /// Mean per-probe hit ratio across all probes (fraction of rays that hit
    /// geometry). `-1.0` = unknown (v1 file). At runtime a value in
    /// `[0, 0.05)` signals an all-miss (broken) bake, so the renderer can
    /// reject it and keep the synthetic field instead of showing flat sky.
    pub global_hit_ratio: f32,
}

impl ProbeVolumeData {
    /// Total number of probes in the grid.
    pub fn probe_count(&self) -> usize {
        self.dims[0] as usize * self.dims[1] as usize * self.dims[2] as usize
    }

    /// Expected `coeffs.len()` for the given dims.
    pub fn expected_coeff_count(&self) -> usize {
        self.probe_count() * 9
    }

    /// Validate internal consistency (coeffs length matches dims).
    pub fn is_valid(&self) -> bool {
        self.coeffs.len() == self.expected_coeff_count() && self.dims.iter().all(|&d| d >= 1)
    }
}

/// Magic bytes identifying a PrismaRev probe-volume file.
pub const MAGIC: &[u8; 4] = b"PRPV";
/// Current (and only supported) file format version.
pub const VERSION: u32 = 2;
/// Header size in bytes (magic + version + origin + spacing + dims + format +
/// scene_name + global_hit_ratio).
pub const HEADER_SIZE: usize = 116;
/// Fixed width of the null-padded scene-name field.
pub const SCENE_NAME_LEN: usize = 64;

/// Coeff format: 32-bit float per component.
const FORMAT_F32: u32 = 0;

/// Sentinel for an unknown global hit ratio.
pub const HIT_RATIO_UNKNOWN: f32 = -1.0;

/// Load a probe volume from a binary `.bin` file.
pub fn load_probe_volume(path: &Path) -> io::Result<ProbeVolumeData> {
    let bytes = std::fs::read(path)?;
    load_probe_volume_from_bytes(&bytes)
}

/// Load a probe volume from an in-memory byte slice.
pub fn load_probe_volume_from_bytes(bytes: &[u8]) -> io::Result<ProbeVolumeData> {
    if bytes.len() < HEADER_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file too small ({} bytes, need >= {})",
                bytes.len(),
                HEADER_SIZE
            ),
        ));
    }

    // Magic check.
    if &bytes[0..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid magic (expected PRPV)",
        ));
    }

    let version = read_u32(&bytes[4..8]);
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported version {} (expected {})", version, VERSION),
        ));
    }

    let origin = [
        read_f32(&bytes[8..12]),
        read_f32(&bytes[12..16]),
        read_f32(&bytes[16..20]),
    ];
    let spacing = [
        read_f32(&bytes[20..24]),
        read_f32(&bytes[24..28]),
        read_f32(&bytes[28..32]),
    ];
    let dims = [
        read_u32(&bytes[32..36]),
        read_u32(&bytes[36..40]),
        read_u32(&bytes[40..44]),
    ];
    let coeff_format = read_u32(&bytes[44..48]);

    if coeff_format != FORMAT_F32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported coeff format {} (only f32=0)", coeff_format),
        ));
    }

    if dims.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dims must all be >= 1",
        ));
    }

    // Scene name (64 null-padded bytes) + global hit ratio.
    let scene_name = read_scene_name(&bytes[48..48 + SCENE_NAME_LEN]);
    let global_hit_ratio = read_f32(&bytes[112..116]);

    let probe_count = dims[0] as usize * dims[1] as usize * dims[2] as usize;
    let coeff_count = probe_count * 9;
    let expected_body = coeff_count * 3 * 4; // 3 floats per coeff, 4 bytes each

    if bytes.len() < HEADER_SIZE + expected_body {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file truncated: {} bytes, need {} (header {} + body {})",
                bytes.len(),
                HEADER_SIZE + expected_body,
                HEADER_SIZE,
                expected_body
            ),
        ));
    }

    // Parse coefficient body.
    let body = &bytes[HEADER_SIZE..HEADER_SIZE + expected_body];
    let mut coeffs = Vec::with_capacity(coeff_count);
    for i in 0..coeff_count {
        let base = i * 3 * 4;
        let r = read_f32(&body[base..base + 4]);
        let g = read_f32(&body[base + 4..base + 8]);
        let b = read_f32(&body[base + 8..base + 12]);
        coeffs.push([r, g, b]);
    }

    Ok(ProbeVolumeData {
        origin,
        spacing,
        dims,
        coeffs,
        scene_name,
        global_hit_ratio,
    })
}

/// Save a probe volume to a binary `.bin` file.
pub fn save_probe_volume(path: &Path, data: &ProbeVolumeData) -> io::Result<()> {
    let bytes = save_probe_volume_to_bytes(data)?;
    std::fs::write(path, bytes)
}

/// Serialize a probe volume to an in-memory byte vector.
pub fn save_probe_volume_to_bytes(data: &ProbeVolumeData) -> io::Result<Vec<u8>> {
    if !data.is_valid() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "invalid ProbeVolumeData: dims={:?}, coeffs.len()={}, expected={}",
                data.dims,
                data.coeffs.len(),
                data.expected_coeff_count()
            ),
        ));
    }

    let coeff_count = data.coeffs.len();
    let body_size = coeff_count * 3 * 4;
    let mut buf = Vec::with_capacity(HEADER_SIZE + body_size);

    // Header.
    buf.extend_from_slice(MAGIC);
    write_u32(&mut buf, VERSION);
    write_f32(&mut buf, data.origin[0]);
    write_f32(&mut buf, data.origin[1]);
    write_f32(&mut buf, data.origin[2]);
    write_f32(&mut buf, data.spacing[0]);
    write_f32(&mut buf, data.spacing[1]);
    write_f32(&mut buf, data.spacing[2]);
    write_u32(&mut buf, data.dims[0]);
    write_u32(&mut buf, data.dims[1]);
    write_u32(&mut buf, data.dims[2]);
    write_u32(&mut buf, FORMAT_F32);
    // Scene name (64 null-padded bytes) + global hit ratio.
    write_scene_name(&mut buf, &data.scene_name);
    write_f32(&mut buf, data.global_hit_ratio);

    // Body: RGB triplets per coefficient.
    for c in &data.coeffs {
        write_f32(&mut buf, c[0]);
        write_f32(&mut buf, c[1]);
        write_f32(&mut buf, c[2]);
    }

    Ok(buf)
}

// -------------------------------------------------------------------
// Little-endian read/write helpers
// -------------------------------------------------------------------

fn read_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn read_f32(b: &[u8]) -> f32 {
    f32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_f32(buf: &mut Vec<u8>, v: f32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Decode a 64-byte null-padded scene-name field into a UTF-8 `String`.
fn read_scene_name(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Encode a scene name into a 64-byte null-padded field (truncated to 63
/// bytes so at least one trailing NUL remains).
fn write_scene_name(buf: &mut Vec<u8>, name: &str) {
    let mut name_bytes = name.as_bytes();
    // Keep at most SCENE_NAME_LEN - 1 bytes so the field is always NUL-terminated.
    if name_bytes.len() >= SCENE_NAME_LEN {
        name_bytes = &name_bytes[..SCENE_NAME_LEN - 1];
    }
    let mut field = [0u8; SCENE_NAME_LEN];
    field[..name_bytes.len()].copy_from_slice(name_bytes);
    buf.extend_from_slice(&field);
}

#[cfg(test)]
#[path = "probe_loader_tests.rs"]
mod tests;
