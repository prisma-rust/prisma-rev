// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;

#[test]
fn asset_type_labels() {
    assert_eq!(AssetType::Binary.label(), "binary");
    assert_eq!(AssetType::Texture.label(), "texture");
    assert_eq!(AssetType::Mesh.label(), "mesh");
    assert_eq!(AssetType::Material.label(), "material");
    assert_eq!(AssetType::Unknown.label(), "unknown");
}

#[test]
fn asset_type_from_u32_roundtrip() {
    for raw in [0u32, 1, 2, 3, 4, 5, 6, 7, 0xFF] {
        let ty = AssetType::from_u32(raw);
        assert_eq!(ty.to_u32(), raw);
    }
}

#[test]
fn unknown_extension_yields_unknown() {
    assert_eq!(AssetType::from_extension("xyz"), AssetType::Unknown);
    assert_eq!(AssetType::from_extension(""), AssetType::Unknown);
}

#[test]
fn known_extensions_map_correctly() {
    assert_eq!(AssetType::from_extension("png"), AssetType::Texture);
    assert_eq!(AssetType::from_extension("PNG"), AssetType::Texture);
    assert_eq!(AssetType::from_extension("jpg"), AssetType::Texture);
    assert_eq!(AssetType::from_extension("gltf"), AssetType::Mesh);
    assert_eq!(AssetType::from_extension("glb"), AssetType::Mesh);
    assert_eq!(AssetType::from_extension("fbx"), AssetType::Mesh);
    assert_eq!(AssetType::from_extension("wav"), AssetType::Audio);
    assert_eq!(AssetType::from_extension("ogg"), AssetType::Audio);
    assert_eq!(AssetType::from_extension("slang"), AssetType::Shader);
    assert_eq!(AssetType::from_extension("spv"), AssetType::Shader);
    assert_eq!(AssetType::from_extension("prefab"), AssetType::Prefab);
    assert_eq!(AssetType::from_extension("scene"), AssetType::Scene);
    assert_eq!(AssetType::from_extension("rscn"), AssetType::Scene);
    assert_eq!(AssetType::from_extension("rtex"), AssetType::Texture);
    assert_eq!(AssetType::from_extension("rmes"), AssetType::Mesh);
    assert_eq!(AssetType::from_extension("rmat"), AssetType::Material);
}

#[test]
fn font_extensions_are_fonts_not_binary() {
    // #583：字体必须能被 cooker / pack 识别，否则不会进 `.pak`。
    for extension in ["ttf", "TTF", "otf", "ttc"] {
        assert_eq!(
            AssetType::from_extension(extension),
            AssetType::Font,
            "{extension}"
        );
    }
    assert_eq!(AssetType::Font.label(), "font");
    assert_eq!(
        AssetType::from_u32(AssetType::Font.to_u32()),
        AssetType::Font
    );
}

#[test]
fn asset_ref_serde() {
    let r = AssetRef::new(crate::AssetId::generate(), AssetType::Texture);
    let json = serde_json::to_string(&r).unwrap();
    let back: AssetRef = serde_json::from_str(&json).unwrap();
    assert_eq!(r.id, back.id);
    assert_eq!(r.asset_type, back.asset_type);
}

#[test]
fn known_type_is_known() {
    assert!(AssetType::Texture.is_known());
    assert!(AssetType::Mesh.is_known());
}

#[test]
fn unknown_type_is_not_known() {
    assert!(!AssetType::Unknown.is_known());
}
