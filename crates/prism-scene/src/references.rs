//! Stable asset references in authoring documents (#466).
//!
//! `.scene.json` / `.prefab.json` components may carry a stable GUID next to
//! the legacy path field:
//!
//! ```json
//! { "mesh_path": "../models/box.gltf",
//!   "mesh_guid": "0123456789abcdef0123456789abcdef" }
//! ```
//!
//! Readers resolve the GUID to a path (GUID wins) and fall back to the path
//! when the GUID is missing or stale. Writers keep both fields in sync, so old
//! documents that only know paths still work and new documents survive file
//! moves via their `.rmeta` sidecars.
//!
//! The resolver is deliberately independent of `prism-asset`: callers pass
//! two lookups. This keeps the scene schema usable without pulling the runtime
//! asset database into every consumer.

use serde_json::{Map, Value};

/// Component fields whose string value is an asset path.
pub const ASSET_PATH_FIELDS: &[&str] = &[
    "mesh_path",           // MeshRenderer
    "material_path",       // MeshRenderer
    "material_paths",      // MeshRenderer（Vec<String>，多 submesh 材质）
    "sprite_path",         // SpriteRenderer
    "tileset_path",        // Tilemap
    "hdr_path",            // Skybox
    "sound_path",          // AudioSource
    "heightmap",           // Terrain（不以 _path 结尾，必须显式登记）
    "skeleton_path",       // SkeletonRoot
    "default_clip_path",   // SkeletonRoot
    "skinned_mesh_path",   // SkinnedMeshRenderer
    "clip_path",           // Animator
    "state_machine_path",  // Animator（.rasm）
    "blend_clip_paths",    // Animator（Vec<String>，一维混合空间）
    "blend_2d_clip_paths", // Animator（Vec<String>，二维混合空间）
    "additive_clip_path",  // Animator（叠加层剪辑）
    "anim_graph_path",     // Animator（.rag 动画图）
    "env_map",             // EnvironmentLighting
    "volume",              // GiConfig
    "prefab",              // PrefabInstance
    "font",                // UI Text（工程自带字体，#583）
];

/// Path field → GUID field. Returns `None` for non-reference strings.
pub fn guid_field(path_field: &str) -> Option<&'static str> {
    Some(match path_field {
        "mesh_path" => "mesh_guid",
        "material_path" => "material_guid",
        "material_paths" => "material_guids",
        "sprite_path" => "sprite_guid",
        "tileset_path" => "tileset_guid",
        "hdr_path" => "hdr_guid",
        "sound_path" => "sound_guid",
        "heightmap" => "heightmap_guid",
        "skeleton_path" => "skeleton_guid",
        "default_clip_path" => "default_clip_guid",
        "skinned_mesh_path" => "skinned_mesh_guid",
        "clip_path" => "clip_guid",
        "state_machine_path" => "state_machine_guid",
        "blend_clip_paths" => "blend_clip_guids",
        "blend_2d_clip_paths" => "blend_2d_clip_guids",
        "additive_clip_path" => "additive_clip_guid",
        "anim_graph_path" => "anim_graph_guid",
        "env_map" => "env_map_guid",
        "volume" => "volume_guid",
        "prefab" => "prefab_guid",
        "font" => "font_guid",
        _ => return None,
    })
}

/// Whether `name` is a known asset-path field.
pub fn is_asset_path_field(name: &str) -> bool {
    ASSET_PATH_FIELDS.contains(&name)
}

/// Read vs. write behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Resolve GUIDs to paths; do not add GUID fields.
    Load,
    /// Resolve GUIDs and fill/refresh GUID fields from path sidecars.
    Save,
}

/// Resolve GUIDs in a whole scene / prefab document.
///
/// `doc_rel` is the document path relative to the project's `assets/` root
/// (`scenes/intro.scene.json`). The lookups use paths relative to that same
/// root. Returns the number of fields that changed.
pub fn sync_document<G, P>(
    doc: &mut Value,
    doc_rel: &str,
    mode: SyncMode,
    path_for_guid: G,
    guid_for_path: P,
) -> usize
where
    G: Fn(&str) -> Option<String>,
    P: Fn(&str) -> Option<String>,
{
    let Some(entities) = doc.get_mut("entities").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut changed = 0;
    for entity in entities {
        let Some(components) = entity.get_mut("components").and_then(Value::as_object_mut) else {
            continue;
        };
        for component in components.values_mut() {
            changed += sync_component(component, doc_rel, mode, &path_for_guid, &guid_for_path);
        }
    }
    changed
}

/// Resolve GUIDs in one component payload (`MeshRenderer`, `Skybox`, …).
pub fn sync_component<G, P>(
    component: &mut Value,
    doc_rel: &str,
    mode: SyncMode,
    path_for_guid: &G,
    guid_for_path: &P,
) -> usize
where
    G: Fn(&str) -> Option<String>,
    P: Fn(&str) -> Option<String>,
{
    let Some(fields) = component.as_object_mut() else {
        return 0;
    };
    let mut changed = 0;
    for &path_field in ASSET_PATH_FIELDS {
        let Some(guid_field) = guid_field(path_field) else {
            continue;
        };
        let path_value = fields.get(path_field).cloned();
        let guid_value = fields.get(guid_field).cloned();
        if path_value.is_none() && guid_value.is_none() {
            continue;
        }
        let is_array = path_value.as_ref().is_some_and(Value::is_array)
            || guid_value.as_ref().is_some_and(Value::is_array);
        if is_array {
            changed += sync_array(
                fields,
                path_field,
                guid_field,
                path_value,
                guid_value,
                doc_rel,
                mode,
                path_for_guid,
                guid_for_path,
            );
        } else {
            changed += sync_single(
                fields,
                path_field,
                guid_field,
                path_value,
                guid_value,
                doc_rel,
                mode,
                path_for_guid,
                guid_for_path,
            );
        }
    }
    changed
}

#[allow(clippy::too_many_arguments)]
fn sync_single<G, P>(
    fields: &mut Map<String, Value>,
    path_field: &str,
    guid_field: &str,
    path_value: Option<Value>,
    guid_value: Option<Value>,
    doc_rel: &str,
    mode: SyncMode,
    path_for_guid: &G,
    guid_for_path: &P,
) -> usize
where
    G: Fn(&str) -> Option<String>,
    P: Fn(&str) -> Option<String>,
{
    let mut path = path_value
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let guid = guid_value
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut changed = 0;

    if !guid.is_empty() {
        if let Some(asset_rel) = path_for_guid(&guid) {
            let next = format_reference(doc_rel, &path, &asset_rel);
            if next != path {
                path = next.clone();
                fields.insert(path_field.to_string(), Value::String(next));
                changed += 1;
            }
        }
    }

    if mode == SyncMode::Save {
        let resolved_guid = path_to_asset_relative(doc_rel, &path)
            .as_deref()
            .and_then(guid_for_path);
        if let Some(found) = resolved_guid {
            if guid != found {
                fields.insert(guid_field.to_string(), Value::String(found.to_string()));
                changed += 1;
            }
        }
    }

    changed
}

#[allow(clippy::too_many_arguments)]
fn sync_array<G, P>(
    fields: &mut Map<String, Value>,
    path_field: &str,
    guid_field: &str,
    path_value: Option<Value>,
    guid_value: Option<Value>,
    doc_rel: &str,
    mode: SyncMode,
    path_for_guid: &G,
    guid_for_path: &P,
) -> usize
where
    G: Fn(&str) -> Option<String>,
    P: Fn(&str) -> Option<String>,
{
    let paths_in: Vec<String> = path_value
        .as_ref()
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let guids_in: Vec<String> = guid_value
        .as_ref()
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let len = paths_in.len().max(guids_in.len());
    let mut paths = paths_in.clone();
    let mut guids = guids_in.clone();
    guids.resize(len, String::new());
    let mut paths_changed = false;

    for index in 0..len {
        let guid = guids[index].as_str();
        if index >= paths.len() {
            // A GUID array can outlive a shortened path array. Recover only
            // slots that actually resolve; unresolved entries must not grow
            // the path array with empty strings.
            if guid.is_empty() {
                continue;
            }
            let Some(asset_rel) = path_for_guid(guid) else {
                continue;
            };
            paths.push(format_reference(doc_rel, "", &asset_rel));
            paths_changed = true;
            continue;
        }

        if !guid.is_empty() {
            if let Some(asset_rel) = path_for_guid(guid) {
                let next = format_reference(doc_rel, &paths[index], &asset_rel);
                if next != paths[index] {
                    paths[index] = next;
                    paths_changed = true;
                }
            }
        }
        if mode == SyncMode::Save {
            let resolved_guid = path_to_asset_relative(doc_rel, &paths[index])
                .as_deref()
                .and_then(guid_for_path);
            if let Some(found) = resolved_guid {
                if guids[index] != found {
                    guids[index] = found.to_string();
                }
            }
        }
    }

    let mut changed = 0;
    if paths_changed {
        fields.insert(
            path_field.to_string(),
            Value::Array(paths.iter().cloned().map(Value::String).collect()),
        );
        changed += 1;
    }

    if mode == SyncMode::Save {
        // GUID arrays always mirror the path array's slots.
        guids.truncate(paths.len());
        while guids.len() < paths.len() {
            guids.push(String::new());
        }
        let any_guid = guids.iter().any(|guid| !guid.is_empty());
        if any_guid {
            let guid_json = Value::Array(guids.iter().cloned().map(Value::String).collect());
            if fields.get(guid_field) != Some(&guid_json) {
                fields.insert(guid_field.to_string(), guid_json);
                changed += 1;
            }
        } else if fields.remove(guid_field).is_some() {
            changed += 1;
        }
    }

    changed
}

/// Format an assets-root-relative path in the same style as `current`
/// (scene-relative by default, `assets/…` for project-root style).
pub fn format_reference(doc_rel: &str, current: &str, asset_rel: &str) -> String {
    if current.replace('\\', "/").starts_with("assets/") {
        return format!("assets/{asset_rel}");
    }
    relative_from(&doc_dir(doc_rel), asset_rel)
}

/// Normalise a reference from a document to an assets-root-relative path.
///
/// Returns `None` for absolute paths or paths that escape the assets root.
pub fn path_to_asset_relative(doc_rel: &str, value: &str) -> Option<String> {
    let value = value.replace('\\', "/");
    if value.is_empty() || value.starts_with('/') || value.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let doc_dir = doc_dir(doc_rel);
    let mut parts: Vec<&str> = if value.starts_with("assets/") {
        Vec::new()
    } else {
        doc_dir.split('/').filter(|part| !part.is_empty()).collect()
    };
    let relative = value.strip_prefix("assets/").unwrap_or(&value);
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn doc_dir(doc_rel: &str) -> String {
    doc_rel
        .rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default()
}

fn relative_from(from_dir: &str, target: &str) -> String {
    let from: Vec<&str> = from_dir
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let to: Vec<&str> = target.split('/').filter(|part| !part.is_empty()).collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = Vec::new();
    parts.extend(std::iter::repeat_n("..", from.len() - common));
    parts.extend(to[common..].iter().copied());
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
#[path = "references_tests.rs"]
mod tests;
