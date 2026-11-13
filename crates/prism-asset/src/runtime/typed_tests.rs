//! `Asset::into_bytes` round-trip tests for the cooked asset wrappers.
//!
//! These guard the contract that a value produced by `from_bytes` can hand the
//! same bytes back. `into_bytes` used to return an empty vector for textures,
//! meshes and materials, which looked like a successful export of a 0-byte
//! asset to any caller.

use super::*;
use crate::core::AssetId;
use crate::formats::MATERIAL_SCALAR_COUNT;

/// Minimal RTEX blob: 2x2, one mip level, 4 payload bytes.
fn rtex_blob() -> Vec<u8> {
    let mut blob = Vec::new();
    blob.extend_from_slice(b"RTEX");
    blob.push(1);
    blob.extend_from_slice(&2u32.to_le_bytes());
    blob.extend_from_slice(&2u32.to_le_bytes());
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.push(1);
    let table_end = 18 + 4;
    blob.extend_from_slice(&(table_end as u32).to_le_bytes());
    blob.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
    blob
}

/// Minimal RMES blob: one vertex of 12 bytes, three u32 indices.
///
/// The header carries `positions_offset` / `normals_offset` / `uv0_offset`
/// between byte 21 and 33; `decode_rmes` does not model them, so the bytes are
/// deliberately non-zero here to prove `into_bytes` returns them untouched.
fn rmes_blob() -> Vec<u8> {
    let mut blob = Vec::new();
    blob.extend_from_slice(b"RMES");
    blob.push(1);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&3u32.to_le_bytes());
    blob.extend_from_slice(&0u32.to_le_bytes());
    blob.extend_from_slice(&12u32.to_le_bytes());
    blob.extend_from_slice(&33u32.to_le_bytes());
    blob.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    blob.extend_from_slice(&0xFEED_FACEu32.to_le_bytes());
    blob.extend_from_slice(&[1u8; 12]);
    blob.extend_from_slice(&[2u8; 12]);
    blob
}

/// Minimal RMAT blob: 18 scalars, alpha block, one present texture slot, plus a
/// trailing payload that the decoder ignores.
fn rmat_blob() -> Vec<u8> {
    let mut blob = Vec::new();
    blob.extend_from_slice(b"RMAT");
    blob.push(crate::formats::RMAT_VERSION);
    for i in 0..MATERIAL_SCALAR_COUNT {
        blob.extend_from_slice(&(i as f32).to_le_bytes());
    }
    blob.push(1); // Mask
    blob.extend_from_slice(&crate::formats::DEFAULT_ALPHA_CUTOFF.to_le_bytes());
    blob.push(1);
    blob.extend_from_slice(&7u64.to_le_bytes());
    blob.extend_from_slice(&[0; 4]);
    blob.extend_from_slice(b"tail");
    blob
}

#[test]
fn texture_into_bytes_returns_the_cooked_blob() {
    let blob = rtex_blob();
    let asset = TextureAsset::from_bytes(&blob).expect("valid RTEX");

    assert_eq!(asset.info.width, 2);
    let bytes = asset.into_bytes();
    assert!(
        !bytes.is_empty(),
        "into_bytes must not silently return nothing"
    );
    assert_eq!(bytes, blob);
}

#[test]
fn mesh_into_bytes_keeps_the_header_offsets_it_did_not_decode() {
    let blob = rmes_blob();
    let asset = MeshAsset::from_bytes(&blob).expect("valid RMES");

    assert_eq!(asset.info.vert_count, 1);
    assert_eq!(asset.info.idx_count, 3);
    let bytes = asset.into_bytes();
    assert!(
        !bytes.is_empty(),
        "into_bytes must not silently return nothing"
    );
    assert_eq!(bytes, blob);
    // The three sub-buffer offsets are not part of `RmesInfo`; they survive
    // only because the original bytes are kept.
    assert_eq!(&bytes[25..29], &0xDEAD_BEEFu32.to_le_bytes());
    assert_eq!(&bytes[29..33], &0xFEED_FACEu32.to_le_bytes());
}

#[test]
fn material_into_bytes_returns_the_cooked_blob() {
    let blob = rmat_blob();
    let asset = MaterialAsset::from_bytes(&blob).expect("valid RMAT");

    assert_eq!(asset.texture_ids()[0], Some(AssetId::from_raw(7)));
    assert_eq!(asset.scalars()[1], 1.0);
    let bytes = asset.into_bytes();
    assert!(
        !bytes.is_empty(),
        "into_bytes must not silently return nothing"
    );
    assert_eq!(bytes, blob);
}
