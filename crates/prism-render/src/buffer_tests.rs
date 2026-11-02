use super::*;
use crate::managers::texture_manager::TextureFormat;

/// Every `TextureFormat` the runtime can upload must agree with the generic
/// `vk::Format` byte math, otherwise a baked KTX2 chain is sliced at the wrong
/// offsets (mip N lands mid-block and the upload desynchronises).
#[test]
fn baked_mip_layout_matches_texture_format_block_sizes() {
    let extents = [(1, 1), (3, 3), (4, 4), (5, 7), (64, 32), (600, 333)];
    for format in [
        TextureFormat::Rgba8,
        TextureFormat::Rgba8Srgb,
        TextureFormat::Bc7Unorm,
        TextureFormat::Bc7Srgb,
        TextureFormat::Bc5Unorm,
        TextureFormat::Bc4Unorm,
        TextureFormat::Bc6HUfloat,
        TextureFormat::Astc4x4Unorm,
        TextureFormat::Astc4x4Srgb,
        TextureFormat::Astc6x6Unorm,
        TextureFormat::Astc6x6Srgb,
        TextureFormat::Astc8x8Unorm,
        TextureFormat::Astc8x8Srgb,
        TextureFormat::Astc12x12Unorm,
        TextureFormat::Astc12x12Srgb,
        TextureFormat::Etc2R8G8B8A8Unorm,
        TextureFormat::Etc2R8G8B8A8Srgb,
    ] {
        for (width, height) in extents {
            assert_eq!(
                mip_level_byte_size(format.vk_format(), width, height, 0),
                Some(format.compressed_byte_len(width, height)),
                "{format:?} level 0 at {width}x{height}"
            );
            // Every subsequent level is the same function of the halved extent.
            for level in 1..4 {
                let level_width = (width >> level).max(1);
                let level_height = (height >> level).max(1);
                assert_eq!(
                    mip_level_byte_size(format.vk_format(), width, height, level),
                    Some(format.compressed_byte_len(level_width, level_height)),
                    "{format:?} level {level} at {width}x{height}"
                );
            }
        }
    }
}

#[test]
fn mip_chain_is_the_sum_of_its_levels() {
    // 8x8 RGBA8: 64 + 16 + 4 + 1 texels * 4 bytes.
    let rgba8 = vk::Format::R8G8B8A8_UNORM;
    assert_eq!(mip_level_byte_size(rgba8, 8, 8, 0), Some(256));
    assert_eq!(mip_level_byte_size(rgba8, 8, 8, 1), Some(64));
    assert_eq!(mip_level_byte_size(rgba8, 8, 8, 2), Some(16));
    assert_eq!(mip_level_byte_size(rgba8, 8, 8, 3), Some(4));
    assert_eq!(mip_chain_byte_size(rgba8, 8, 8, 4), Some(340));
    // A single-level "chain" is just the base level.
    assert_eq!(mip_chain_byte_size(rgba8, 8, 8, 1), Some(256));
}

#[test]
fn compressed_levels_are_block_aligned() {
    // ASTC 6x6 blocks 16 bytes: an 8x8 level needs a 2x2 block grid.
    let astc6 = vk::Format::ASTC_6X6_UNORM_BLOCK;
    assert_eq!(mip_level_byte_size(astc6, 8, 8, 0), Some(64));
    // 4x4 level still needs one 6x6 block (partial blocks are padded).
    assert_eq!(mip_level_byte_size(astc6, 8, 8, 1), Some(16));
    // BC4 is 8 bytes per 4x4 block.
    assert_eq!(
        mip_level_byte_size(vk::Format::BC4_UNORM_BLOCK, 4, 4, 0),
        Some(8)
    );
    // Degenerate extents clamp to 1, not 0.
    assert_eq!(
        mip_level_byte_size(vk::Format::BC7_UNORM_BLOCK, 1, 1, 3),
        Some(16)
    );
}

#[test]
fn unmeasured_formats_report_none_instead_of_a_wrong_size() {
    let unknown = vk::Format::R32G32B32A32_SFLOAT;
    assert_eq!(mip_level_byte_size(unknown, 8, 8, 0), None);
    assert_eq!(mip_chain_byte_size(unknown, 8, 8, 4), None);
}

#[test]
fn zero_levels_is_an_empty_chain() {
    assert_eq!(
        mip_chain_byte_size(vk::Format::R8G8B8A8_UNORM, 8, 8, 0),
        Some(0)
    );
}
