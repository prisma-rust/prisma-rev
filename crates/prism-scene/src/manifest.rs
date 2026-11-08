//! `scenes.json` scene manifest, shared by the runtime, the cooker and the
//! build pipeline.
//!
//! The manifest is authoring data: it lists the scenes a project builds and
//! starts in order. Keeping it in the schema crate means the pipeline can read
//! a manifest without linking the engine.

use serde::{Deserialize, Serialize};

/// One entry of the scene manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneManifestEntry {
    pub name: String,
    pub path: String,
    /// Build Settings checkbox; missing means enabled (old manifests).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Stable GUID of the source document (#466). Packaged manifests drop it;
    /// the runtime only uses `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SceneManifest {
    pub scenes: Vec<SceneManifestEntry>,
}

impl SceneManifest {
    /// Parse manifest text: `scenes.json`
    /// (`{"scenes":[{name,path,enabled,guid}]}`), falling back to the legacy
    /// `scenes.toml` (`[[scenes]]` array table) with a migration warning.
    ///
    /// The fallback only runs when JSON parsing fails, so a broken new-format
    /// manifest is never "guessed" into an empty list — both formats must fail.
    pub fn parse(text: &str) -> Result<Self, String> {
        match serde_json::from_str::<Self>(text) {
            Ok(manifest) => Ok(manifest),
            Err(json_error) => match parse_legacy_toml(text) {
                Ok(manifest) => {
                    log::warn!(
                        "scene manifest is in the legacy TOML format; run \
                         `prism-build-pipeline build-scenes` to migrate it to scenes.json"
                    );
                    Ok(manifest)
                }
                Err(toml_error) => Err(format!(
                    "not scenes.json ({json_error}) and not legacy scenes.toml ({toml_error})"
                )),
            },
        }
    }
}

fn default_enabled() -> bool {
    true
}

/// Minimal reader for the legacy `scenes.toml` format.
///
/// It only understands the subset the manifest uses: a `[[scenes]]` array
/// table plus the `name` / `path` / `enabled` keys and full-line comments. It
/// is not a general TOML parser — it exists solely so old projects keep
/// loading until `build-scenes` migrates them to `scenes.json`.
fn parse_legacy_toml(text: &str) -> Result<SceneManifest, String> {
    let mut scenes: Vec<SceneManifestEntry> = Vec::new();
    let mut current: Option<SceneManifestEntry> = None;
    let mut saw_table = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[scenes]]" {
            if let Some(entry) = current.take() {
                scenes.push(entry);
            }
            current = Some(SceneManifestEntry {
                name: String::new(),
                path: String::new(),
                enabled: true,
                guid: None,
            });
            saw_table = true;
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            // Another table (should not appear): close the current entry and
            // ignore the keys that follow.
            if let Some(entry) = current.take() {
                scenes.push(entry);
            }
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = prism_ini::parse_value(value);
        match key.trim() {
            "name" => entry.name = value,
            "path" => entry.path = value,
            "enabled" => {
                entry.enabled = !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "no" | "off"
                )
            }
            _ => {}
        }
    }
    if let Some(entry) = current.take() {
        scenes.push(entry);
    }

    if !saw_table {
        return Err("no [[scenes]] table".into());
    }
    Ok(SceneManifest { scenes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_manifest_round_trips() {
        let manifest = SceneManifest::parse(
            r#"{"scenes":[{"name":"intro","path":"scenes/intro.rscn","guid":"abc"}]}"#,
        )
        .unwrap();
        assert_eq!(manifest.scenes[0].name, "intro");
        assert!(manifest.scenes[0].enabled, "missing enabled means enabled");
        assert_eq!(manifest.scenes[0].guid.as_deref(), Some("abc"));
    }

    #[test]
    fn legacy_toml_still_parses() {
        let manifest = SceneManifest::parse(
            "[[scenes]]\nname = \"intro\"\npath = \"scenes/intro.rscn\"\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(manifest.scenes.len(), 1);
        assert!(!manifest.scenes[0].enabled);
    }

    #[test]
    fn broken_json_does_not_become_an_empty_manifest() {
        let error = SceneManifest::parse(r#"{"scenes": [}"#).unwrap_err();
        assert!(error.contains("not scenes.json"), "{error}");
    }
}
