use super::*;
use serde_json::json;

const MESH_GUID: &str = "11111111111111111111111111111111";
const TEX_GUID: &str = "22222222222222222222222222222222";

fn index() -> impl Fn(&str) -> Option<String> {
    |guid: &str| match guid {
        MESH_GUID => Some("models/box.gltf".into()),
        TEX_GUID => Some("textures/wood.png".into()),
        _ => None,
    }
}

fn reverse() -> impl Fn(&str) -> Option<String> {
    |path: &str| match path {
        "models/box.gltf" => Some(MESH_GUID.into()),
        "textures/wood.png" => Some(TEX_GUID.into()),
        _ => None,
    }
}

#[test]
fn guid_wins_over_a_stale_path() {
    let mut doc = json!({
        "entities": [{
            "id": 0,
            "components": {
                "prism_engine::scene::MeshRenderer": {
                    "mesh_path": "../old/renamed.gltf",
                    "mesh_guid": MESH_GUID
                }
            }
        }]
    });

    let changed = sync_document(
        &mut doc,
        "scenes/intro.scene.json",
        SyncMode::Load,
        index(),
        reverse(),
    );

    assert_eq!(changed, 1);
    assert_eq!(
        doc["entities"][0]["components"]["prism_engine::scene::MeshRenderer"]["mesh_path"],
        "../models/box.gltf"
    );
}

#[test]
fn load_falls_back_to_path_when_guid_is_missing_or_unknown() {
    let mut doc = json!({
        "entities": [{
            "id": 0,
            "components": {
                "prism_engine::scene::MeshRenderer": {
                    "mesh_path": "../models/box.gltf",
                    "mesh_guid": "ffffffffffffffffffffffffffffffff"
                },
                "prism_engine::scene::Skybox": {
                    "hdr_path": "../env/sky.hdr"
                }
            }
        }]
    });
    let original = doc.clone();

    assert_eq!(
        sync_document(
            &mut doc,
            "scenes/intro.scene.json",
            SyncMode::Load,
            index(),
            reverse()
        ),
        0
    );
    assert_eq!(doc, original);
}

#[test]
fn save_fills_and_refreshes_guids_from_sidecars() {
    let mut doc = json!({
        "entities": [{
            "id": 0,
            "components": {
                "prism_engine::scene::MeshRenderer": {
                    "mesh_path": "../models/box.gltf"
                },
                "prism_engine::scene::Skybox": {
                    "hdr_path": "../env/sky.hdr",
                    "hdr_guid": "ffffffffffffffffffffffffffffffff"
                },
                "prism_engine::scene::AudioSource": {
                    "sound_path": "../audio/theme.ogg"
                }
            }
        }]
    });

    let changed = sync_document(
        &mut doc,
        "scenes/intro.scene.json",
        SyncMode::Save,
        index(),
        reverse(),
    );

    assert_eq!(changed, 1);
    let renderer = &doc["entities"][0]["components"]["prism_engine::scene::MeshRenderer"];
    assert_eq!(renderer["mesh_guid"], MESH_GUID);
    // Unknown path: no GUID to add.
    let audio = &doc["entities"][0]["components"]["prism_engine::scene::AudioSource"];
    assert!(audio.get("sound_guid").is_none());
    // Invalid GUID with an unresolvable path is left alone rather than guessed.
    let skybox = &doc["entities"][0]["components"]["prism_engine::scene::Skybox"];
    assert_eq!(skybox["hdr_guid"], "ffffffffffffffffffffffffffffffff");
}

#[test]
fn save_refreshes_an_invalid_guid_when_the_path_has_a_sidecar() {
    let mut doc = json!({
        "entities": [{
            "id": 0,
            "components": {
                "prism_engine::scene::MeshRenderer": {
                    "mesh_path": "assets/models/box.gltf",
                    "mesh_guid": "not-a-guid"
                }
            }
        }]
    });

    sync_document(
        &mut doc,
        "scenes/intro.scene.json",
        SyncMode::Save,
        index(),
        reverse(),
    );

    let renderer = &doc["entities"][0]["components"]["prism_engine::scene::MeshRenderer"];
    assert_eq!(renderer["mesh_guid"], MESH_GUID);
    assert_eq!(renderer["mesh_path"], "assets/models/box.gltf");
}

#[test]
fn arrays_stay_index_aligned_and_keep_project_style() {
    let mut doc = json!({
        "entities": [{
            "id": 0,
            "components": {
                "prism_engine::scene::MeshRenderer": {
                    "material_paths": ["assets/textures/wood.png", "../textures/gone.mat"],
                    "material_guids": [TEX_GUID, ""]
                }
            }
        }]
    });

    let changed = sync_document(
        &mut doc,
        "scenes/intro.scene.json",
        SyncMode::Save,
        index(),
        reverse(),
    );

    assert_eq!(changed, 0);
    let renderer = &doc["entities"][0]["components"]["prism_engine::scene::MeshRenderer"];
    assert_eq!(
        renderer["material_paths"],
        json!(["assets/textures/wood.png", "../textures/gone.mat"])
    );
    assert_eq!(renderer["material_guids"], json!([TEX_GUID, ""]));
}

#[test]
fn array_guid_recovers_a_missing_path() {
    let mut doc = json!({
        "entities": [{
            "id": 0,
            "components": {
                "prism_engine::scene::MeshRenderer": {
                    "material_guids": [TEX_GUID]
                }
            }
        }]
    });

    sync_document(
        &mut doc,
        "scenes/intro.scene.json",
        SyncMode::Load,
        index(),
        reverse(),
    );

    let renderer = &doc["entities"][0]["components"]["prism_engine::scene::MeshRenderer"];
    assert_eq!(renderer["material_paths"], json!(["../textures/wood.png"]));
}

#[test]
fn unresolved_extra_guids_do_not_grow_the_path_array() {
    let mut doc = json!({
        "entities": [{
            "id": 0,
            "components": {
                "prism_engine::scene::MeshRenderer": {
                    "material_paths": ["../textures/wood.png"],
                    "material_guids": [TEX_GUID, "ffffffffffffffffffffffffffffffff"]
                }
            }
        }]
    });

    let changed = sync_document(
        &mut doc,
        "scenes/intro.scene.json",
        SyncMode::Load,
        index(),
        reverse(),
    );

    assert_eq!(changed, 0);
    let renderer = &doc["entities"][0]["components"]["prism_engine::scene::MeshRenderer"];
    assert_eq!(renderer["material_paths"], json!(["../textures/wood.png"]));
}

#[test]
fn legacy_documents_without_guids_are_untouched() {
    let mut doc = json!({
        "entities": [{
            "id": 0,
            "components": {
                "prism_engine::scene::MeshRenderer": {
                    "mesh_path": "../models/box.gltf"
                }
            }
        }]
    });
    let original = doc.clone();

    sync_document(
        &mut doc,
        "scenes/intro.scene.json",
        SyncMode::Load,
        index(),
        reverse(),
    );

    assert_eq!(doc, original);
}

#[test]
fn path_normalisation_handles_both_styles() {
    assert_eq!(
        path_to_asset_relative("scenes/intro.scene.json", "../models/box.gltf"),
        Some("models/box.gltf".into())
    );
    assert_eq!(
        path_to_asset_relative("scenes/intro.scene.json", "assets/models/box.gltf"),
        Some("models/box.gltf".into())
    );
    assert_eq!(
        path_to_asset_relative("intro.scene.json", "models/box.gltf"),
        Some("models/box.gltf".into())
    );
    assert_eq!(
        path_to_asset_relative("scenes/intro.scene.json", "../../escape.gltf"),
        None
    );
    assert_eq!(
        format_reference("scenes/intro.scene.json", "../old.gltf", "models/box.gltf"),
        "../models/box.gltf"
    );
    assert_eq!(
        format_reference(
            "scenes/intro.scene.json",
            "assets/old.gltf",
            "models/box.gltf"
        ),
        "assets/models/box.gltf"
    );
}
