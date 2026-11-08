//! # prism-asset::package
//!
//! PrismaRev 资源管道的 `.pak` 二进制归档格式
//!
//! ## 格式概览
//!
//! ```text
//! ┌─────────────────────────────┐
//! │ PackageHeader（32 字节）       │
//! ├─────────────────────────────┤
//! │ 资源注册表 [n]             │ ← 连续的 `RuntimeAssetRecord` 数组
//! ├─────────────────────────────┤
//! │ 依赖数组 [m]               │ ← AssetId (u64) 的平面数组
//! ├─────────────────────────────┤
//! │ 数据块                      │ ← 资源有效载荷，可选 zstd 压缩
//! └─────────────────────────────┘
//! ```
//!
//! - Magic: `b"RPAK"` (4 bytes)
//! - All multi-byte values are little-endian.
//! - The header checksum covers everything from the next byte after the checksum
//!   field through the 结束 of the file (i.e. header[12..] + registry + deps + data).

use crate::core::{AssetId, AssetType};
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;
use xxhash_rust::xxh3::Xxh3;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic identifier: `b"RPAK"`.
pub const MAGIC: [u8; 4] = *b"RPAK";
/// Current package format version.
pub const VERSION: u32 = 1;

/// Flag: asset data is zstd-compressed in the data chunk.
pub const FLAG_COMPRESSED: u32 = 1 << 0;
/// Flag: asset data is intended for streaming (large, can be loaded in chunks).
pub const FLAG_STREAMED: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum PackageError {
    #[error("invalid magic: expected RPAK, got {0:?}")]
    InvalidMagic([u8; 4]),

    #[error("unsupported version {0} (expected {VERSION})")]
    UnsupportedVersion(u32),

    #[error("truncated file: {0}")]
    Truncated(String),

    #[error("checksum mismatch: computed {computed:#x}, stored {stored:#x}")]
    ChecksumMismatch { computed: u64, stored: u64 },

    #[error("decompression error: {0}")]
    Decompress(String),

    /// The requested operation needs the whole `.pak` in memory, but the
    /// package was opened from a file and streams its payload on demand.
    ///
    /// Borrowed slices ([`PackageReader::asset_record_raw_slice`]) cannot be
    /// produced from a file-backed reader; use
    /// [`PackageReader::asset_record_raw_bytes`] instead.
    #[error("asset data is not held in memory (package was opened from a file)")]
    NotInMemory,

    #[error("compression error: {0}")]
    Compress(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// On-disk package header (32 bytes + registry_offset/data_offset metadata).
///
/// Total fixed header size: 4 + 4 + 4 + 8 + 8 + 8 + 8 + 8 = 52 bytes
/// followed by the asset registry array.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct PackageHeader {
    pub magic: [u8; 4],
    pub version: u32,
    /// Number of assets in the registry.
    pub asset_count: u32,
    /// Byte offset from file start to the asset registry.
    pub registry_offset: u64,
    /// Byte size of the asset registry.
    pub registry_size: u64,
    /// Byte offset from file start to the data chunk area.
    pub data_offset: u64,
    /// Byte size of the data chunk area (uncompressed).
    pub data_size: u64,
    /// xxh3-64 校验和 of `header[4..44]` (version, asset_count, offsets/sizes)
    /// + registry + deps + data. `magic` 与 `checksum` 字段自身不参与。
    pub checksum: u64,
}

impl PackageHeader {
    /// Serialized header size in bytes (magic 4 + version 4 + ... + checksum 8 + padding).
    pub const SERIALIZED_SIZE: u64 = 4 + 4 + 4 + 8 + 8 + 8 + 8 + 8; // 52

    /// 校验和覆盖的 header 起点：跳过 `magic`（固定值，改了就不是 pak 了）。
    const CHECKSUM_HEADER_START: usize = 4;
    /// 校验和覆盖的 header 终点 = `checksum` 字段起点。`version` / `asset_count`
    /// 必须在这里面，否则篡改它们不会触发 [`PackageError::ChecksumMismatch`]
    /// （#89）。
    const CHECKSUM_FIELD_OFFSET: usize = 44;

    /// Compute the xxh3 checksum over the payload (everything after the checksum field).
    fn compute_checksum(header_body: &[u8], registry: &[u8], deps: &[u8], data: &[u8]) -> u64 {
        let mut hasher = Xxh3::default();
        hasher.update(header_body);
        hasher.update(registry);
        hasher.update(deps);
        hasher.update(data);
        hasher.digest()
    }

    /// Serialize the header to bytes (without checksum — caller computes it).
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::SERIALIZED_SIZE as usize);
        buf.extend_from_slice(&self.magic);
        buf.extend_from_slice(&self.version.to_le_bytes());
        buf.extend_from_slice(&self.asset_count.to_le_bytes());
        buf.extend_from_slice(&self.registry_offset.to_le_bytes());
        buf.extend_from_slice(&self.registry_size.to_le_bytes());
        buf.extend_from_slice(&self.data_offset.to_le_bytes());
        buf.extend_from_slice(&self.data_size.to_le_bytes());
        buf.extend_from_slice(&self.checksum.to_le_bytes());
        buf
    }

    /// Deserialize from bytes (must be at least `SERIALIZED_SIZE`).
    fn from_bytes(bytes: &[u8]) -> Result<Self, PackageError> {
        if bytes.len() < Self::SERIALIZED_SIZE as usize {
            return Err(PackageError::Truncated(format!(
                "header too small: {} < {}",
                bytes.len(),
                Self::SERIALIZED_SIZE
            )));
        }
        let mut off = 0;
        let magic: [u8; 4] = bytes[off..off + 4].try_into().unwrap();
        off += 4;
        if magic != MAGIC {
            return Err(PackageError::InvalidMagic(magic));
        }
        let version = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        off += 4;
        if version != VERSION {
            return Err(PackageError::UnsupportedVersion(version));
        }
        let asset_count = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        off += 4;
        let registry_offset = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;
        let registry_size = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;
        let data_offset = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;
        let data_size = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;
        let checksum = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());

        Ok(Self {
            magic,
            version,
            asset_count,
            registry_offset,
            registry_size,
            data_offset,
            data_size,
            checksum,
        })
    }
}

// ---------------------------------------------------------------------------
// Runtime Asset Record
// ---------------------------------------------------------------------------

/// On-disk record for one cooked asset in the package.
///
/// Total size: 8 + 4 + 4 + 8 + 8 + 8 + 4 + 4 = 48 bytes.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct RuntimeAssetRecord {
    /// Asset ID (u64).
    pub id: u64,
    /// AssetType discriminant (u32).
    pub type_id: u32,
    /// Flags: `FLAG_COMPRESSED`, `FLAG_STREAMED`, etc.
    pub flags: u32,
    /// Byte offset of uncompressed data in the data chunk area.
    pub offset: u64,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// Compressed size in bytes (0 when `!flags & FLAG_COMPRESSED`).
    pub compressed_size: u64,
    /// Start index into the flat dependency array.
    pub dependency_start: u32,
    /// Number of dependency entries.
    pub dependency_count: u32,
}

impl RuntimeAssetRecord {
    pub const SERIALIZED_SIZE: u64 = 48;

    /// Returns true if this asset is compressed in the package.
    #[inline]
    pub fn is_compressed(&self) -> bool {
        (self.flags & FLAG_COMPRESSED) != 0
    }

    /// Returns true if this asset is intended for streaming.
    #[inline]
    pub fn is_streamed(&self) -> bool {
        (self.flags & FLAG_STREAMED) != 0
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::SERIALIZED_SIZE as usize);
        buf.extend_from_slice(&self.id.to_le_bytes());
        buf.extend_from_slice(&self.type_id.to_le_bytes());
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.extend_from_slice(&self.offset.to_le_bytes());
        buf.extend_from_slice(&self.size.to_le_bytes());
        buf.extend_from_slice(&self.compressed_size.to_le_bytes());
        buf.extend_from_slice(&self.dependency_start.to_le_bytes());
        buf.extend_from_slice(&self.dependency_count.to_le_bytes());
        buf
    }

    fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), PackageError> {
        if bytes.len() < Self::SERIALIZED_SIZE as usize {
            return Err(PackageError::Truncated(format!(
                "record too small: {} < {}",
                bytes.len(),
                Self::SERIALIZED_SIZE
            )));
        }
        let mut off = 0;
        let id = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;
        let type_id = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        off += 4;
        let flags = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        off += 4;
        let offset = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;
        let size = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;
        let compressed_size = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;
        let dependency_start = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        off += 4;
        let dependency_count = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        off += 4;
        Ok((
            Self {
                id,
                type_id,
                flags,
                offset,
                size,
                compressed_size,
                dependency_start,
                dependency_count,
            },
            off,
        ))
    }
}

// ---------------------------------------------------------------------------
// Package Builder
// ---------------------------------------------------------------------------

/// A pending asset that will be written into the .pak.
struct PendingAsset {
    id: AssetId,
    asset_type: AssetType,
    data: Vec<u8>,
    dependencies: Vec<AssetId>,
    flags: u32,
}

/// Builder for creating `.pak` package files.
///
/// Usage:
/// ```ignore
/// let mut builder = PackageBuilder::new();
/// builder.add_asset(id, AssetType::Binary, data, &[]);
/// builder.build_to_file("game.pak")?;
/// ```
#[derive(Default)]
pub struct PackageBuilder {
    assets: Vec<PendingAsset>,
    compression_level: i32,
}

impl PackageBuilder {
    pub fn new() -> Self {
        Self {
            assets: Vec::new(),
            compression_level: 0, // 0 = no compression
        }
    }

    /// Set the zstd compression level (1-22). 0 = no compression (default).
    pub fn set_compression(&mut self, level: i32) {
        self.compression_level = level;
    }

    /// Add an asset to the package.
    pub fn add_asset(
        &mut self,
        id: AssetId,
        asset_type: AssetType,
        data: Vec<u8>,
        dependencies: &[AssetId],
    ) {
        self.add_asset_with_flags(id, asset_type, data, dependencies, 0);
    }

    /// Add an asset intended for streaming.
    ///
    /// Streamed assets have `FLAG_STREAMED` set in their record and are stored
    /// uncompressed in the package so they can be streamed directly in chunks
    /// without decompressing or copying the entire payload into memory.
    pub fn add_streamed_asset(
        &mut self,
        id: AssetId,
        asset_type: AssetType,
        data: Vec<u8>,
        dependencies: &[AssetId],
    ) {
        self.add_asset_with_flags(id, asset_type, data, dependencies, FLAG_STREAMED);
    }

    /// Add an asset with custom flags (e.g. `FLAG_STREAMED`).
    pub fn add_asset_with_flags(
        &mut self,
        id: AssetId,
        asset_type: AssetType,
        data: Vec<u8>,
        dependencies: &[AssetId],
        flags: u32,
    ) {
        self.assets.push(PendingAsset {
            id,
            asset_type,
            data,
            dependencies: dependencies.to_vec(),
            flags,
        });
    }

    /// Number of assets added.
    pub fn asset_count(&self) -> usize {
        self.assets.len()
    }

    /// Build the .pak file in memory and return the raw bytes.
    pub fn build(&mut self) -> Result<Vec<u8>, PackageError> {
        // Compute compressed data if needed.
        let compressed_data: Vec<(Vec<u8>, u64, bool)> = self
            .assets
            .iter()
            .map(|a| {
                let is_streamed = (a.flags & FLAG_STREAMED) != 0;
                let should_compress = self.compression_level > 0 && !is_streamed;
                if should_compress {
                    let compressed =
                        zstd::encode_all(std::io::Cursor::new(&a.data), self.compression_level)
                            .map_err(|e| PackageError::Compress(e.to_string()))?;
                    Ok((compressed, a.data.len() as u64, true))
                } else {
                    Ok((a.data.clone(), a.data.len() as u64, false))
                }
            })
            .collect::<Result<Vec<_>, PackageError>>()?;

        // Build flat dependency array and record each asset's dependency range.
        let mut dep_array = Vec::<u64>::new();
        let mut records = Vec::<RuntimeAssetRecord>::new();
        let mut dep_start: u32 = 0;

        for (i, asset) in self.assets.iter().enumerate() {
            let (data, uncompressed_size, is_compressed) = &compressed_data[i];
            let mut flags = asset.flags;
            if *is_compressed {
                flags |= FLAG_COMPRESSED;
            }
            let comp_size = if *is_compressed { data.len() as u64 } else { 0 };

            let deps: Vec<u64> = asset.dependencies.iter().map(|d| d.into_raw()).collect();
            let dep_count = deps.len() as u32;
            dep_array.extend_from_slice(&deps);

            records.push(RuntimeAssetRecord {
                id: asset.id.into_raw(),
                type_id: asset.asset_type.to_u32(),
                flags,
                offset: 0, // filled below
                size: *uncompressed_size,
                compressed_size: comp_size,
                dependency_start: dep_start,
                dependency_count: dep_count,
            });
            dep_start += dep_count;
        }

        let asset_count = records.len() as u32;

        // Calculate offsets.
        let registry_offset = PackageHeader::SERIALIZED_SIZE;
        let registry_size = records.len() as u64 * RuntimeAssetRecord::SERIALIZED_SIZE;
        let deps_offset = registry_offset + registry_size;
        let deps_size = dep_array.len() as u64 * 8;
        let data_offset = deps_offset + deps_size;
        let data_size: u64 = compressed_data.iter().map(|(d, _, _)| d.len() as u64).sum();

        // Fill in data offsets for each record.
        let mut data_off = data_offset;
        for (i, record) in records.iter_mut().enumerate() {
            record.offset = data_off;
            data_off += if compressed_data[i].2 {
                compressed_data[i].0.len() as u64
            } else {
                compressed_data[i].1 // uncompressed size
            };
        }

        // Serialize registry + deps + data.
        let registry_bytes: Vec<u8> = records.iter().flat_map(|r| r.to_bytes()).collect();
        let deps_bytes: Vec<u8> = dep_array.iter().flat_map(|d| d.to_le_bytes()).collect();
        let data_bytes: Vec<u8> = compressed_data
            .iter()
            .flat_map(|(d, _, _)| d.clone())
            .collect();

        // 校验和覆盖 header[4..44]（version、asset_count、各偏移/长度）+ registry +
        // deps + data；`checksum` 字段自身与固定的 `magic` 不参与。
        let hdr = PackageHeader {
            magic: MAGIC,
            version: VERSION,
            asset_count,
            registry_offset,
            registry_size,
            data_offset,
            data_size,
            checksum: 0, // placeholder
        };
        let header_bytes = hdr.to_bytes();
        let header_prefix = &header_bytes
            [PackageHeader::CHECKSUM_HEADER_START..PackageHeader::CHECKSUM_FIELD_OFFSET];

        let checksum = PackageHeader::compute_checksum(
            header_prefix,
            &registry_bytes,
            &deps_bytes,
            &data_bytes,
        );

        // Write final header with checksum.
        let mut final_hdr = hdr;
        final_hdr.checksum = checksum;
        let final_header_bytes = final_hdr.to_bytes();

        // Assemble final file.
        let mut output = Vec::with_capacity((data_offset + data_size) as usize);
        output.extend_from_slice(&final_header_bytes);
        output.extend_from_slice(&registry_bytes);
        output.extend_from_slice(&deps_bytes);
        output.extend_from_slice(&data_bytes);

        tracing::info!(
            "Built .pak: {} assets, {} deps, {} bytes data, checksum={:#x}",
            asset_count,
            dep_array.len(),
            data_size,
            checksum
        );

        Ok(output)
    }

    /// Build and write to a file.
    pub fn build_to_file(&mut self, path: impl AsRef<Path>) -> Result<(), PackageError> {
        let bytes = self.build()?;
        std::fs::write(path.as_ref(), &bytes)?;
        tracing::info!("Written .pak to {}", path.as_ref().display());
        Ok(())
    }

    /// Async: build and write to a file via tokio.
    #[cfg(feature = "async-io")]
    pub async fn build_to_file_async(
        &mut self,
        path: impl AsRef<Path> + Send,
    ) -> Result<(), PackageError> {
        let bytes = self.build()?;
        tokio::fs::write(path.as_ref(), &bytes).await?;
        tracing::info!("Written .pak to {} (async)", path.as_ref().display());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Package Reader
// ---------------------------------------------------------------------------

/// A loaded `.pak` package, ready for asset lookup and extraction.
///
/// The reader parses the header, the 资源 registry and the dependency array at
/// 打开 时间, then serves 资源 payloads on demand:
///
/// - [`PackageReader::open`] keeps an open [`File`] and `pread`s the ranges it
///   needs, so a 1 GB `.pak` costs a few kilobytes of RAM (#509);
/// - [`PackageReader::from_bytes`] keeps the buffer it was handed (Android
///   `assets/`, editor previews, tests) and hands out borrowed slices.
///
/// Both backings share the same layout checks, so neither can slice past the
/// end of the file.
#[derive(Debug, Clone)]
pub struct PackageReader {
    header: PackageHeader,
    records: Vec<RuntimeAssetRecord>,
    dependency_array: Vec<u64>,
    backing: Backing,
}

/// Where the `.pak` bytes live.
///
/// The format has always been random-access (header offsets + per-record
/// offset/length); this enum is what finally lets the reader use that instead
/// of demanding one contiguous `Vec<u8>` of the whole archive.
#[derive(Debug, Clone)]
enum Backing {
    /// The whole archive, already in RAM.
    Bytes(Arc<Vec<u8>>),
    /// An open file handle; payload bytes are read on demand.
    File { file: Arc<File>, len: u64 },
}

impl Backing {
    fn len(&self) -> u64 {
        match self {
            Backing::Bytes(bytes) => bytes.len() as u64,
            Backing::File { len, .. } => *len,
        }
    }

    /// The in-memory buffer, when the package has one.
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Backing::Bytes(bytes) => Some(bytes.as_slice()),
            Backing::File { .. } => None,
        }
    }

    /// 读取 `len` 字节 at `offset`.
    fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>, PackageError> {
        match self {
            Backing::Bytes(bytes) => {
                let start = usize::try_from(offset)
                    .map_err(|_| PackageError::Truncated("offset does not fit usize".into()))?;
                let end = usize::try_from(
                    offset
                        .checked_add(len)
                        .ok_or_else(|| PackageError::Truncated("range overflows u64".into()))?,
                )
                .map_err(|_| PackageError::Truncated("range end does not fit usize".into()))?;
                let slice = bytes.get(start..end).ok_or_else(|| {
                    PackageError::Truncated(format!(
                        "range {start}..{end} extends past file: {}",
                        bytes.len()
                    ))
                })?;
                Ok(slice.to_vec())
            }
            Backing::File {
                file,
                len: file_len,
            } => {
                let end = offset
                    .checked_add(len)
                    .ok_or_else(|| PackageError::Truncated("range overflows u64".into()))?;
                if end > *file_len {
                    return Err(PackageError::Truncated(format!(
                        "range {offset}..{end} extends past file: {file_len}"
                    )));
                }
                let mut buf = vec![0u8; len as usize];
                read_exact_at(file, &mut buf, offset)?;
                Ok(buf)
            }
        }
    }
}

/// `pread`: 读取 `buf.len()` 字节 at `offset` without touching the file cursor.
///
/// A `&File` is shared by every thread that holds the reader, so this must not
/// `seek` (that would race); Windows and Unix both expose positioned reads.
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            let read = file.seek_read(&mut buf[done..], offset + done as u64)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file ended before the requested range",
                ));
            }
            done += read;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut handle = file.try_clone()?;
        handle.seek(SeekFrom::Start(offset))?;
        handle.read_exact(buf)
    }
}

/// 打开 a `.pak` file from disk without reading the whole archive into RAM.
///
/// Only the header, the 资源 registry and the dependency array are read up
/// front; payload bytes are `pread` per asset. The 校验和 is verified in
/// bounded chunks (1 MiB at a time), so opening still costs one sequential
/// pass over the file but no per-file allocation.
impl PackageReader {
    /// Open a `.pak` file from disk and verify its integrity.
    ///
    /// The file stays on disk: only the metadata regions are read here, payload
    /// bytes are fetched per asset with positioned reads (#509).
    ///
    /// The reader keeps the handle open, so deleting or replacing the `.pak`
    /// while it lives is safe on both Unix and Windows (Rust opens with
    /// `FILE_SHARE_DELETE`): this reader keeps serving the version it opened and
    /// the next [`Self::open`] picks up the new file — which is exactly what the
    /// hot-reload path in `ResourceManager::on_pak_changed` relies on.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PackageError> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let len = file.metadata()?.len();

        let mut head = [0u8; PackageHeader::SERIALIZED_SIZE as usize];
        read_exact_at(&file, &mut head, 0)?;
        let header = PackageHeader::from_bytes(&head)?;

        // 完整性先于一切结构性判断，理由见 `from_owned_bytes`（#89）。
        verify_checksum_file(&file, len, &header)?;

        Self::parse(
            header,
            Backing::File {
                file: Arc::new(file),
                len,
            },
        )
    }

    /// Async open via tokio.
    #[cfg(feature = "async-io")]
    pub async fn open_async(path: impl AsRef<Path> + Send) -> Result<Self, PackageError> {
        Self::from_owned_bytes(tokio::fs::read(path.as_ref()).await?)
    }

    /// Parse from an in-memory byte 切片 (copies the bytes into the reader).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PackageError> {
        Self::from_owned_bytes(bytes.to_vec())
    }

    /// Parse from an owned buffer, taking ownership instead of copying it.
    ///
    /// [`Self::from_bytes`] / [`Self::open_async`] go through this so opening
    /// an in-memory `.pak` allocates the buffer exactly once.
    fn from_owned_bytes(bytes: Vec<u8>) -> Result<Self, PackageError> {
        let header = PackageHeader::from_bytes(&bytes)?;

        // 完整性先于一切结构性判断：校验和只看「header 里除 magic/checksum 外的
        // 字段」+ 文件剩余字节，不依赖 header 里那些尚未验证的偏移，所以被篡改的
        // `version` / `asset_count` 会在这里就变成 ChecksumMismatch，而不是让后面的
        // registry 解析在错误数据上继续跑（#89）。
        verify_checksum(&bytes, &header)?;

        Self::parse(header, Backing::Bytes(Arc::new(bytes)))
    }

    /// Validate the layout described by an already-checksummed header and load
    /// the metadata regions from `backing`.
    fn parse(header: PackageHeader, backing: Backing) -> Result<Self, PackageError> {
        let file_len = backing.len();

        if header.registry_offset != PackageHeader::SERIALIZED_SIZE {
            return Err(PackageError::Truncated(format!(
                "registry_offset {} != header size {}",
                header.registry_offset,
                PackageHeader::SERIALIZED_SIZE
            )));
        }

        let expected_registry_size =
            header.asset_count as u64 * RuntimeAssetRecord::SERIALIZED_SIZE;
        if header.registry_size != expected_registry_size {
            return Err(PackageError::Truncated(format!(
                "registry_size {} != asset_count {} * {}",
                header.registry_size,
                header.asset_count,
                RuntimeAssetRecord::SERIALIZED_SIZE
            )));
        }

        let reg_end = header
            .registry_offset
            .checked_add(header.registry_size)
            .ok_or_else(|| PackageError::Truncated("registry end overflows u64".into()))?;
        if reg_end > file_len {
            return Err(PackageError::Truncated(format!(
                "registry extends past file: {reg_end} > {file_len}"
            )));
        }

        // 依赖数组位于 registry 与 data 之间，长度必须是 u64 的整数倍。
        let deps_start = reg_end;
        let deps_size = header.data_offset.checked_sub(deps_start).ok_or_else(|| {
            PackageError::Truncated(format!(
                "data_offset {} precedes the registry end {deps_start}",
                header.data_offset
            ))
        })?;
        if deps_size % 8 != 0 {
            return Err(PackageError::Truncated(format!(
                "dependency array size {deps_size} is not a multiple of 8"
            )));
        }

        let data_end = header
            .data_offset
            .checked_add(header.data_size)
            .ok_or_else(|| PackageError::Truncated("data chunk end overflows u64".into()))?;
        if data_end > file_len {
            return Err(PackageError::Truncated(format!(
                "data chunk extends past file: {data_end} > {file_len}"
            )));
        }

        // Read registry.
        let reg_len = reg_end - header.registry_offset;
        let reg_bytes = backing.read_range(header.registry_offset, reg_len)?;
        let mut records = Vec::with_capacity(header.asset_count as usize);
        let mut off = 0usize;
        for _ in 0..header.asset_count {
            let (rec, consumed) = RuntimeAssetRecord::from_bytes(&reg_bytes[off..])?;
            records.push(rec);
            off += consumed;
        }

        // Read dependency array.
        let deps_bytes = backing.read_range(deps_start, deps_size)?;
        let dep_count = deps_bytes.len() / 8;
        let mut dependency_array = Vec::with_capacity(dep_count);
        for &chunk in deps_bytes.as_chunks::<8>().0 {
            dependency_array.push(u64::from_le_bytes(chunk));
        }

        // record 里的依赖区间与数据区间同样来自文件内容，不校验就会在
        // dependencies() / read_asset_record_data 里越界切片 panic。
        for rec in &records {
            let dep_end = rec
                .dependency_start
                .checked_add(rec.dependency_count)
                .ok_or_else(|| PackageError::Truncated("dependency range overflows u32".into()))?;
            if dep_end as usize > dep_count {
                return Err(PackageError::Truncated(format!(
                    "record dependency range {}..{} exceeds {dep_count} dependencies",
                    rec.dependency_start, dep_end
                )));
            }

            let stored_len = if (rec.flags & FLAG_COMPRESSED) != 0 {
                rec.compressed_size
            } else {
                rec.size
            };
            let asset_end = rec
                .offset
                .checked_add(stored_len)
                .ok_or_else(|| PackageError::Truncated("asset range overflows u64".into()))?;
            if rec.offset < header.data_offset || asset_end > data_end {
                return Err(PackageError::Truncated(format!(
                    "record data range {}..{} escapes the data chunk {}..{}",
                    rec.offset, asset_end, header.data_offset, data_end
                )));
            }
        }

        Ok(Self {
            header,
            records,
            dependency_array,
            backing,
        })
    }

    /// Header reference.
    pub fn header(&self) -> &PackageHeader {
        &self.header
    }

    /// Number of assets in the package.
    pub fn asset_count(&self) -> usize {
        self.records.len()
    }

    /// All records.
    pub fn records(&self) -> &[RuntimeAssetRecord] {
        &self.records
    }

    /// Find a record by asset ID.
    pub fn find_record(&self, id: AssetId) -> Option<&RuntimeAssetRecord> {
        let raw = id.into_raw();
        self.records.iter().find(|r| r.id == raw)
    }

    /// Find a record by raw u64 ID.
    pub fn find_record_by_raw(&self, id: u64) -> Option<&RuntimeAssetRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// Get the dependencies for a record.
    pub fn dependencies(&self, record: &RuntimeAssetRecord) -> &[u64] {
        let start = record.dependency_start as usize;
        let end = start + record.dependency_count as usize;
        &self.dependency_array[start..end]
    }

    /// Byte range of the stored payload for `record`, validated against the file.
    fn stored_range(&self, record: &RuntimeAssetRecord) -> Result<(u64, u64), PackageError> {
        let stored_len = if record.is_compressed() {
            record.compressed_size
        } else {
            record.size
        };
        let end = record
            .offset
            .checked_add(stored_len)
            .ok_or_else(|| PackageError::Truncated("asset range overflows u64".into()))?;
        let file_len = self.backing.len();
        if end > file_len {
            return Err(PackageError::Truncated(format!(
                "asset data extends past file: {end} > {file_len}"
            )));
        }
        Ok((record.offset, end))
    }

    /// Return the raw byte slice of the package payload for a specific record.
    ///
    /// For uncompressed or streamed assets, this slice is the actual asset bytes.
    /// For compressed assets, this slice contains the compressed zstd frame.
    ///
    /// Only packages opened from memory ([`Self::from_bytes`]) can hand out a
    /// borrowed slice; a file-backed reader returns
    /// [`PackageError::NotInMemory`] — use [`Self::asset_record_raw_bytes`].
    pub fn asset_record_raw_slice(
        &self,
        record: &RuntimeAssetRecord,
    ) -> Result<&[u8], PackageError> {
        let (start, end) = self.stored_range(record)?;
        let bytes = self.backing.bytes().ok_or(PackageError::NotInMemory)?;
        let range = start as usize..end as usize;
        Ok(&bytes[range])
    }

    /// Return the raw payload bytes for a specific record, reading them from
    /// disk when the package is file-backed.
    ///
    /// This allocates only the one asset (or its compressed frame), never the
    /// whole archive.
    pub fn asset_record_raw_bytes(
        &self,
        record: &RuntimeAssetRecord,
    ) -> Result<Vec<u8>, PackageError> {
        let (start, end) = self.stored_range(record)?;
        self.backing.read_range(start, end - start)
    }

    /// Return the raw byte slice of the package payload for a given asset ID.
    pub fn asset_raw_slice(&self, id: AssetId) -> Result<Option<&[u8]>, PackageError> {
        let record = match self.find_record(id) {
            Some(r) => r,
            None => return Ok(None),
        };
        self.asset_record_raw_slice(record).map(Some)
    }

    /// Returns whether the specified asset is marked for streaming.
    pub fn is_asset_streamed(&self, id: AssetId) -> bool {
        self.find_record(id).is_some_and(|r| r.is_streamed())
    }

    /// Read the uncompressed data for an asset.
    ///
    /// Returns `None` if the record is not found. Returns decompressed bytes
    /// for compressed assets, or a slice of the mmap'd data for uncompressed ones.
    pub fn read_asset_data(&self, id: AssetId) -> Result<Option<Vec<u8>>, PackageError> {
        let record = match self.find_record(id) {
            Some(r) => r,
            None => return Ok(None),
        };
        self.read_asset_record_data(record).map(Some)
    }

    /// Read data for a specific record.
    pub fn read_asset_record_data(
        &self,
        record: &RuntimeAssetRecord,
    ) -> Result<Vec<u8>, PackageError> {
        let raw = self.asset_record_raw_bytes(record)?;
        if record.is_compressed() {
            let decompressed = zstd::decode_all(std::io::Cursor::new(&raw))
                .map_err(|e| PackageError::Decompress(e.to_string()))?;
            Ok(decompressed)
        } else {
            Ok(raw)
        }
    }

    /// Check integrity of the package (re-checksums the entire file body).
    pub fn verify_integrity(&self) -> Result<(), PackageError> {
        match &self.backing {
            Backing::Bytes(bytes) => verify_checksum(bytes, &self.header),
            Backing::File { file, len } => verify_checksum_file(file, *len, &self.header),
        }
    }
}

/// 校验 `.pak` 的 xxh3-64 校验和。
///
/// 覆盖 `header[4..44]`（version / asset_count / 各偏移与长度）加上 header 之后的
/// 全部字节（registry + deps + data）。只按文件布局取切片，不读 header 里那些
/// 尚未验证的偏移，所以可以放在解析 registry **之前**（#89）。
fn verify_checksum(bytes: &[u8], header: &PackageHeader) -> Result<(), PackageError> {
    let header_prefix = bytes
        .get(PackageHeader::CHECKSUM_HEADER_START..PackageHeader::CHECKSUM_FIELD_OFFSET)
        .ok_or_else(|| {
            PackageError::Truncated(format!(
                "file is {} bytes, shorter than the checksummed header",
                bytes.len()
            ))
        })?;
    let payload = bytes
        .get(PackageHeader::SERIALIZED_SIZE as usize..)
        .unwrap_or(&[]);

    let computed = PackageHeader::compute_checksum(header_prefix, &[], &[], payload);
    if computed != header.checksum {
        return Err(PackageError::ChecksumMismatch {
            computed,
            stored: header.checksum,
        });
    }
    Ok(())
}

/// 校验 `.pak` 的 xxh3-64 校验和，文件后端流式读取。
///
/// 语义与 [`verify_checksum`] 完全一致（`header[4..44]` + 其后全部字节），只是按
/// 1 MiB 块读磁盘，所以打开大包不再需要先在内存里放一份完整映像（#509）。
fn verify_checksum_file(
    file: &File,
    file_len: u64,
    header: &PackageHeader,
) -> Result<(), PackageError> {
    let prefix_len = PackageHeader::CHECKSUM_FIELD_OFFSET - PackageHeader::CHECKSUM_HEADER_START;
    let mut prefix =
        [0u8; PackageHeader::CHECKSUM_FIELD_OFFSET - PackageHeader::CHECKSUM_HEADER_START];
    if file_len < PackageHeader::CHECKSUM_FIELD_OFFSET as u64 {
        return Err(PackageError::Truncated(format!(
            "file is {file_len} bytes, shorter than the checksummed header"
        )));
    }
    read_exact_at(
        file,
        &mut prefix[..prefix_len],
        PackageHeader::CHECKSUM_HEADER_START as u64,
    )?;

    let mut hasher = Xxh3::default();
    hasher.update(&prefix[..prefix_len]);

    // 从 header 末尾开始，按块喂给 hasher；不读 header 里那些尚未验证的偏移。
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut offset = PackageHeader::SERIALIZED_SIZE;
    while offset < file_len {
        let len = (file_len - offset).min(CHUNK as u64) as usize;
        read_exact_at(file, &mut buf[..len], offset)?;
        hasher.update(&buf[..len]);
        offset += len as u64;
    }

    let computed = hasher.digest();
    if computed != header.checksum {
        return Err(PackageError::ChecksumMismatch {
            computed,
            stored: header.checksum,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
