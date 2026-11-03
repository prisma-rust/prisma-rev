//! Slang reflection -> Rust binding codegen for PrismaRev.
//!
//! Reads the `reflection/*.json` emitted by `slangc -reflection-json`
//! (see `shaders/compile.sh`) and generates a Rust module describing each
//! shader's resource bindings: descriptor set/binding indices, resource kinds,
//! and push-constant sizes. The generated file is committed to the repo so the
//! engine builds on hosts without slangc (Termux/Android).
//!
//! Run on a desktop/CI host after recompiling shaders:
//!   cargo run -p xtask --bin shader-bindgen -- \
//!     shaders/reflection crates/prism-render/src/shader_bindings.rs
//!
//! This is intentionally a standalone tool (NOT a build.rs) so the normal
//! `cargo build` never needs slangc.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Top-level reflection document.
#[derive(Debug, Deserialize)]
struct Reflection {
    #[serde(default)]
    parameters: Vec<Parameter>,
    #[serde(default, rename = "entryPoints")]
    entry_points: Vec<EntryPoint>,
}

#[derive(Debug, Deserialize)]
struct Parameter {
    name: String,
    #[serde(default)]
    binding: Option<Binding>,
    #[serde(rename = "type")]
    ty: Option<TypeInfo>,
}

#[derive(Debug, Deserialize)]
struct Binding {
    /// e.g. "descriptorTableSlot", "pushConstantBuffer", uniform
    kind: String,
    #[serde(default)]
    index: u32,
    #[serde(default)]
    space: u32,
    #[serde(default)]
    size: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TypeInfo {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, rename = "baseShape")]
    base_shape: Option<String>,
    /// For struct types (constantBuffer → elementType → struct with fields).
    #[serde(default, rename = "elementType")]
    element_type: Option<StructTypeInfo>,
}

/// Info about a struct type nested inside a parameter's type.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StructTypeInfo {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    fields: Option<Vec<StructField>>,
}

/// A single field inside a struct definition in the reflection JSON.
#[derive(Debug, Deserialize)]
struct StructField {
    name: String,
    #[serde(rename = "type")]
    ty: FieldType,
    #[serde(default)]
    binding: FieldBinding,
}

/// Type info for a struct field (more detailed than TypeInfo).
#[derive(Debug, Deserialize)]
struct FieldType {
    /// "scalar", "vector", "matrix"
    #[serde(default)]
    kind: String,
    /// "float32", "uint32", "int32"
    #[serde(default, rename = "scalarType")]
    scalar_type: Option<String>,
    /// For vectors: 2, 3, or 4
    #[serde(default, rename = "elementCount")]
    element_count: Option<u32>,
    /// For matrices: 4
    #[serde(default, rename = "rowCount")]
    row_count: Option<u32>,
    /// For matrices: 4
    #[serde(default, rename = "columnCount")]
    column_count: Option<u32>,
    /// For vectors/matrices: the element type
    #[serde(default, rename = "elementType")]
    element_type: Option<Box<FieldType>>,
}

/// Binding info for a struct field.
#[derive(Debug, Default, Deserialize)]
struct FieldBinding {
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct EntryPoint {
    name: String,
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    parameters: Vec<Parameter>,
}

/// Map a Slang reflection field type to its Rust type name.
fn field_type_to_rust(ft: &FieldType) -> String {
    match ft.kind.as_str() {
        "scalar" => match ft.scalar_type.as_deref() {
            Some("float32") => "f32".into(),
            Some("uint32") => "u32".into(),
            Some("int32") => "i32".into(),
            Some("float16") => "f16".into(),
            Some("bool") => "u32".into(),
            _ => "u32".into(),
        },
        "vector" => {
            let count = ft.element_count.unwrap_or(4);
            let elem = ft
                .element_type
                .as_deref()
                .map(field_type_to_rust)
                .unwrap_or_else(|| "f32".into());
            format!("[{elem}; {count}]")
        }
        "matrix" => {
            let rows = ft.row_count.unwrap_or(4);
            let cols = ft.column_count.unwrap_or(4);
            let elem = ft
                .element_type
                .as_deref()
                .map(field_type_to_rust)
                .unwrap_or_else(|| "f32".into());
            format!("[[{elem}; {cols}]; {rows}]")
        }
        _ => "u32".into(),
    }
}

/// Rust `#[repr(C)]` layout of the type [`field_type_to_rust`] emits:
/// `(size, alignment)` in bytes.
///
/// Slang reflects push-constant fields on 4-byte boundaries, so scalars and
/// vectors keep their natural width; matrices are column-major arrays of
/// columns. Anything unrecognised falls back to the `u32` used by
/// [`field_type_to_rust`].
fn rust_layout(ft: &FieldType) -> (u64, u64) {
    match ft.kind.as_str() {
        "scalar" => match ft.scalar_type.as_deref() {
            Some("float16") => (2, 2),
            _ => (4, 4),
        },
        "vector" => {
            let count = u64::from(ft.element_count.unwrap_or(4));
            let (elem_size, elem_align) = ft
                .element_type
                .as_deref()
                .map(rust_layout)
                .unwrap_or((4, 4));
            (elem_size * count, elem_align)
        }
        "matrix" => {
            let rows = u64::from(ft.row_count.unwrap_or(4));
            let cols = u64::from(ft.column_count.unwrap_or(4));
            let (elem_size, elem_align) = ft
                .element_type
                .as_deref()
                .map(rust_layout)
                .unwrap_or((4, 4));
            (elem_size * rows * cols, elem_align)
        }
        _ => (4, 4),
    }
}

/// One member of the emitted push-constant struct.
#[derive(Debug)]
enum PushMember {
    /// A reflected field, placed at its reflected offset.
    Field {
        name: String,
        rust_type: String,
        offset: u64,
        size: u64,
    },
    /// Filler for a std140 alignment hole.
    ///
    /// Leaning on `#[repr(C)]` alone would collapse the hole (the emitted types
    /// only need 4-byte alignment) and silently shift every later field.
    Pad { offset: u64, len: u64 },
}

/// Lay reflected fields out as `#[repr(C)]` members that keep their offsets.
///
/// Returns the members and the resulting struct size. A field that would
/// overlap the previous member is an error: no `#[repr(C)]` layout can express
/// it, and guessing would desync the Rust struct from the shader.
fn layout_push_fields(fields: &[StructField]) -> Result<(Vec<PushMember>, u64)> {
    let mut members: Vec<PushMember> = Vec::new();
    let mut cursor = 0u64;

    for f in fields {
        let (natural_size, _) = rust_layout(&f.ty);
        // The emitted Rust type must never be larger than what the shader
        // reserved for the field; when it is smaller (e.g. a std140 matrix with
        // 16-byte column stride) the reserved size wins.
        let reserved = u64::from(f.binding.size.unwrap_or(natural_size as u32));
        let occupied = natural_size.max(reserved);
        let offset = f.binding.offset.map(u64::from).unwrap_or(cursor);

        if offset < cursor {
            anyhow::bail!(
                "field `{}` is reflected at offset {offset} but the previous member ends at {cursor}",
                f.name
            );
        }
        if offset > cursor {
            members.push(PushMember::Pad {
                offset: cursor,
                len: offset - cursor,
            });
        }
        members.push(PushMember::Field {
            name: f.name.clone(),
            rust_type: field_type_to_rust(&f.ty),
            offset,
            size: reserved,
        });
        cursor = offset + occupied;
    }

    Ok((members, cursor))
}

/// Generate a Rust struct definition from reflected push constant fields.
fn emit_push_struct(struct_name: &str, fields: &[StructField]) -> Result<String> {
    let (members, total) = layout_push_fields(fields)?;
    let mut out = String::new();
    out.push_str(&format!(
        "\n/// Push-constant struct (auto-generated from Slang `{struct_name}`).\n\
         ///\n\
         /// Field offsets are the reflected std140 ones — alignment holes are emitted as\n\
         /// explicit `_pad_*` fillers, so `size_of` is {total} bytes and every field sits\n\
         /// where the shader expects it.\n"
    ));
    out.push_str("#[repr(C)]\n#[derive(Clone, Copy, Default)]\n");
    out.push_str(&format!("pub struct {struct_name} {{\n"));
    for member in &members {
        match member {
            PushMember::Field {
                name,
                rust_type,
                offset,
                size,
            } => {
                out.push_str(&format!(
                    "    /// offset {offset}, size {size}\n    pub {name}: {rust_type},\n"
                ));
            }
            PushMember::Pad { offset, len } => {
                out.push_str(&format!(
                    "    /// std140 alignment hole at offset {offset} ({len} bytes)\n    \
                     pub _pad_{offset}: [u8; {len}],\n"
                ));
            }
        }
    }
    out.push_str("}\n");
    Ok(out)
}

/// Push-constant structs declared by a reflection document.
///
/// Slang lets either the global scope or an entry point declare the block, and
/// the same block can show up in both; the first declaration wins so the
/// generated file never defines a struct twice.
fn push_constant_structs(refl: &Reflection) -> Vec<(&str, &[StructField])> {
    fn pick(p: &Parameter) -> Option<(&str, &[StructField])> {
        let is_push = p
            .binding
            .as_ref()
            .is_some_and(|b| b.kind == "pushConstantBuffer" || b.kind == "pushConstant");
        if !is_push {
            return None;
        }
        let element = p.ty.as_ref()?.element_type.as_ref()?;
        let name = element.name.as_deref().unwrap_or("PushConstants");
        let fields = element.fields.as_deref()?;
        Some((name, fields))
    }

    let mut found: Vec<(&str, &[StructField])> = Vec::new();
    let params = refl
        .parameters
        .iter()
        .chain(refl.entry_points.iter().flat_map(|ep| ep.parameters.iter()));
    for p in params {
        if let Some((name, fields)) = pick(p) {
            if !found.iter().any(|(known, _)| *known == name) {
                found.push((name, fields));
            }
        }
    }
    found
}

/// A resolved binding fact we care about for Rust codegen.
struct ResolvedBinding {
    name: String,
    set: u32,
    binding: u32,
    kind: BindKind,
}

enum BindKind {
    UniformBuffer,
    CombinedImageSampler,
    PushConstant { size: u32 },
}

/// Fallback push-constant sizes for shaders whose slangc reflection omits the
/// `size` field on the `pushConstantBuffer` parameter. Newer Slang releases
/// (e.g. 2026.13.1) stopped emitting `size` on the parameter binding, so we
/// keep fallback values here. Struct-level layout is now auto-generated from
/// the reflection JSON (see `emit_push_struct`).
const PUSH_SIZE_FALLBACK: &[(&str, u32)] = &[
    ("overlay", 0),
    // LightingPushConstants: 4×u32 + 4×f32 = 32 bytes.
    ("lighting", 32),
    // SharcQueryPushConstants: 7 fields padded to 48.
    ("sharc_query", 48),
];

fn fallback_push_size(shader: &str) -> u32 {
    PUSH_SIZE_FALLBACK
        .iter()
        .find(|(s, _)| *s == shader)
        .map(|(_, sz)| *sz)
        .unwrap_or(0)
}

impl BindKind {
    fn descriptor_type(&self) -> Option<&'static str> {
        match self {
            BindKind::UniformBuffer => Some("UNIFORM_BUFFER"),
            BindKind::CombinedImageSampler => Some("COMBINED_IMAGE_SAMPLER"),
            _ => None,
        }
    }
}

fn classify(p: &Parameter) -> Option<ResolvedBinding> {
    let b = p.binding.as_ref()?;
    // Only emit descriptor-set slots and push-constant buffers. Slang also
    // reflects vertex-shader `in` parameters (kind "vertexInput" / no
    // descriptor binding) — those are not Vulkan descriptor bindings and must
    // be skipped.
    let kind = match b.kind.as_str() {
        "pushConstantBuffer" | "pushConstant" => BindKind::PushConstant {
            size: b.size.unwrap_or(0),
        },
        "descriptorTableSlot" | "uniform" => {
            // Distinguish UBO vs texture via the type shape.
            let shape =
                p.ty.as_ref()
                    .and_then(|t| t.base_shape.clone().or_else(|| t.kind.clone()))
                    .unwrap_or_default();
            if shape.contains("texture")
                || shape.contains("Texture")
                || shape.contains("sampler")
                || shape.contains("Sampler")
                || shape.contains("resource")
            {
                BindKind::CombinedImageSampler
            } else {
                BindKind::UniformBuffer
            }
        }
        // vertex inputs, stage inputs, etc. — not descriptor bindings
        _ => return None,
    };
    Some(ResolvedBinding {
        name: p.name.clone(),
        set: b.space,
        binding: b.index,
        kind,
    })
}

fn to_screaming_snake(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            out.push('_');
        }
        if c == '-' || c == ' ' {
            out.push('_');
            prev_lower = false;
            continue;
        }
        out.push(c.to_ascii_uppercase());
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    out
}

fn process_file(path: &Path) -> Result<(String, String)> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let refl: Reflection = serde_json::from_str(&raw)
        .with_context(|| format!("parse reflection JSON {}", path.display()))?;

    let shader = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shader")
        .to_string();
    let mod_name = shader.replace('-', "_");

    // Gather global params + entry-point params.
    let mut resolved: Vec<ResolvedBinding> = Vec::new();
    for p in &refl.parameters {
        if let Some(r) = classify(p) {
            resolved.push(r);
        }
    }
    for ep in &refl.entry_points {
        for p in &ep.parameters {
            if let Some(r) = classify(p) {
                resolved.push(r);
            }
        }
    }
    // Dedup by (set, binding, name).
    resolved.sort_by(|a, b| (a.set, a.binding, &a.name).cmp(&(b.set, b.binding, &b.name)));
    resolved.dedup_by(|a, b| a.set == b.set && a.binding == b.binding && a.name == b.name);

    let mut out = String::new();
    // File header
    out.push_str("// @generated by xtask/shader-bindgen from Slang reflection.\n");
    out.push_str("// DO NOT EDIT. Regenerate: `cd xtask && cargo run --bin shader-bindgen -- \\\n");
    out.push_str("//   ../shaders/reflection ../crates/prism-render/src/shader_bindings`\n");
    out.push_str(&format!(
        "\n//! Bindings reflected from `shaders/slang/{shader}.slang`.\n\n"
    ));
    out.push_str("#![allow(dead_code, non_snake_case)]\n\n");

    // Entry points.
    if !refl.entry_points.is_empty() {
        out.push_str("/// Entry point names (for VkPipelineShaderStageCreateInfo).\n");
        for ep in &refl.entry_points {
            let stage = ep.stage.clone().unwrap_or_default().to_uppercase();
            let cname = to_screaming_snake(&ep.name);
            out.push_str(&format!(
                "pub const ENTRY_{cname}: &str = \"{}\"; // stage: {stage}\n",
                ep.name
            ));
        }
    }

    // Descriptor bindings grouped by set.
    let mut by_set: BTreeMap<u32, Vec<&ResolvedBinding>> = BTreeMap::new();

    // std140 size of the emitted structs. This is the authoritative fallback for
    // shaders whose slangc reflection omits the block size on the
    // `pushConstantBuffer` parameter.
    let mut struct_size: u32 = 0;
    for (struct_name, fields) in push_constant_structs(&refl) {
        let (_, total) = layout_push_fields(fields)
            .with_context(|| format!("layout push-constant struct `{struct_name}`"))?;
        struct_size = struct_size.max(total as u32);
    }

    let mut push_size: Option<u32> = None;
    for r in &resolved {
        match &r.kind {
            BindKind::PushConstant { size } => {
                let sz = (*size).max(fallback_push_size(&shader)).max(struct_size);
                push_size = Some(push_size.unwrap_or(0).max(sz));
            }
            _ => by_set.entry(r.set).or_default().push(r),
        }
    }

    if let Some(sz) = push_size {
        out.push_str(&format!(
            "\n/// Push-constant block size in bytes (reflected).\npub const PUSH_CONSTANT_SIZE: u32 = {sz};\n"
        ));
    }

    for (set, binds) in &by_set {
        out.push_str(&format!("\n// --- descriptor set {set} ---\n"));
        for r in binds {
            let cname = to_screaming_snake(&r.name);
            let dtype = r.kind.descriptor_type().unwrap_or("/* unknown */");
            out.push_str(&format!(
                "pub const {cname}_SET: u32 = {};\npub const {cname}_BINDING: u32 = {}; // {dtype}\n",
                r.set, r.binding
            ));
        }
    }

    // --- Auto-generated push-constant struct definitions ---
    // Scan global and entry-point parameters for push-constant buffers whose
    // types carry struct field info; emit a #[repr(C)] Rust struct for each.
    // This replaces the hand-written push-constant structs in the engine.
    for (struct_name, fields) in push_constant_structs(&refl) {
        out.push_str(&emit_push_struct(struct_name, fields)?);
    }

    Ok((mod_name, out))
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let in_dir = PathBuf::from(
        args.next()
            .context("usage: shader-bindgen <reflection_dir> <out_dir>")?,
    );
    let out_dir = PathBuf::from(
        args.next()
            .context("usage: shader-bindgen <reflection_dir> <out_dir>")?,
    );

    let mut files: Vec<PathBuf> = std::fs::read_dir(&in_dir)
        .with_context(|| format!("read dir {}", in_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    files.sort();

    if files.is_empty() {
        anyhow::bail!(
            "no reflection JSON found in {} — run assets/shaders/compile.sh first",
            in_dir.display()
        );
    }

    std::fs::create_dir_all(&out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    let mut mod_rs = String::new();
    mod_rs.push_str("// @generated by xtask/shader-bindgen from Slang reflection.\n");
    mod_rs.push_str(
        "// DO NOT EDIT. Regenerate: `cd crates/xtask && cargo run --bin shader-bindgen -- \\\\\n",
    );
    mod_rs.push_str(
        "//   ../../assets/shaders/reflection ../../crates/prism-render/src/shader_bindings`\n",
    );
    mod_rs.push_str("#![allow(dead_code, non_snake_case)]\n\n");

    for f in &files {
        let (mod_name, content) = process_file(f)?;
        // Write per-shader file: {out_dir}/{mod_name}.rs
        let mod_path = out_dir.join(format!("{mod_name}.rs"));
        std::fs::write(&mod_path, &content)
            .with_context(|| format!("write {}", mod_path.display()))?;
        // Add to mod.rs
        mod_rs.push_str(&format!("pub mod {mod_name};\n"));
    }

    // Write mod.rs
    let mod_rs_path = out_dir.join("mod.rs");
    std::fs::write(&mod_rs_path, &mod_rs)
        .with_context(|| format!("write {}", mod_rs_path.display()))?;

    println!("wrote {} module(s) to {}", files.len(), out_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reflection_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shaders/reflection")
    }

    fn load(name: &str) -> Reflection {
        let path = reflection_dir().join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
    }

    /// Every committed reflection document, so the layout can be checked against
    /// what `slangc` actually emitted instead of a hand-written fixture.
    fn committed_reflections() -> Vec<(String, Reflection)> {
        let dir = reflection_dir();
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".json"))
            .collect();
        names.sort();
        assert!(!names.is_empty(), "no reflection JSON in {}", dir.display());
        names
            .into_iter()
            .map(|name| {
                let refl = load(&name);
                (name, refl)
            })
            .collect()
    }

    fn scalar32() -> FieldType {
        FieldType {
            kind: "scalar".into(),
            scalar_type: Some("float32".into()),
            element_count: None,
            row_count: None,
            column_count: None,
            element_type: None,
        }
    }

    fn vector(count: u32) -> FieldType {
        FieldType {
            kind: "vector".into(),
            scalar_type: None,
            element_count: Some(count),
            row_count: None,
            column_count: None,
            element_type: Some(Box::new(scalar32())),
        }
    }

    fn field(name: &str, ty: FieldType, offset: u32, size: u32) -> StructField {
        StructField {
            name: name.into(),
            ty,
            binding: FieldBinding {
                offset: Some(offset),
                size: Some(size),
            },
        }
    }

    #[test]
    fn reflected_offsets_are_reproduced_for_every_committed_shader() {
        let mut blocks = 0;
        for (file, refl) in committed_reflections() {
            for (name, fields) in push_constant_structs(&refl) {
                let (members, total) = layout_push_fields(fields)
                    .unwrap_or_else(|error| panic!("{file} :: {name}: {error}"));

                // Every reflected field must land on its own reflected offset.
                for reflected in fields {
                    let expected = u64::from(reflected.binding.offset.expect("reflected offset"));
                    let emitted = members.iter().find_map(|member| match member {
                        PushMember::Field {
                            name: member_name,
                            offset,
                            ..
                        } if member_name == &reflected.name => Some(*offset),
                        _ => None,
                    });
                    assert_eq!(
                        emitted,
                        Some(expected),
                        "{file} :: {name}.{} must stay at its reflected offset",
                        reflected.name
                    );
                }

                // And the struct must extend to the last reflected byte.
                let extent = fields
                    .iter()
                    .map(|f| u64::from(f.binding.offset.unwrap_or(0) + f.binding.size.unwrap_or(0)))
                    .max()
                    .unwrap_or(0);
                assert_eq!(total, extent, "{file} :: {name} struct size");
                blocks += 1;
            }
        }
        assert!(
            blocks >= 8,
            "expected the committed reflection set to carry push blocks, saw {blocks}"
        );
    }

    #[test]
    fn alignment_holes_become_explicit_padding() {
        // std140 rounds the vec4 after a u32 up to 16: bytes 4..16 are a hole that
        // a dense `#[repr(C)]` layout would silently collapse.
        let fields = vec![
            field("flags", scalar32(), 0, 4),
            field("color", vector(4), 16, 16),
        ];

        let (members, total) = layout_push_fields(&fields).unwrap();
        assert_eq!(total, 32);
        match members.as_slice() {
            [PushMember::Field { offset: 0, .. }, PushMember::Pad { offset: 4, len: 12 }, PushMember::Field { offset: 16, .. }] =>
                {}
            other => panic!("unexpected layout: {other:?}"),
        }

        let text = emit_push_struct("Padding", &fields).unwrap();
        assert!(text.contains("pub _pad_4: [u8; 12],"), "{text}");
    }

    #[test]
    fn scene_frag_padding_keeps_its_reflected_offset() {
        let refl = load("scene_frag.json");
        let (name, fields) = push_constant_structs(&refl)
            .pop()
            .expect("scene_frag declares a push block");
        let (members, total) = layout_push_fields(fields).unwrap();

        let offset_of = |wanted: &str| {
            members.iter().find_map(|member| match member {
                PushMember::Field { name, offset, .. } if name == wanted => Some(*offset),
                _ => None,
            })
        };

        // The shader reserves `_padding` at 96; the dense layout used to put it at 84.
        assert_eq!(offset_of("_padding"), Some(96), "{name}");
        assert_eq!(total, 108, "{name}");

        let holes: u64 = members
            .iter()
            .map(|member| match member {
                PushMember::Pad { len, .. } => *len,
                _ => 0,
            })
            .sum();
        assert_eq!(holes, 12, "{name} must fill the 84..96 hole");
    }

    #[test]
    fn emitted_types_never_exceed_the_reflected_field_size() {
        for (file, refl) in committed_reflections() {
            for (name, fields) in push_constant_structs(&refl) {
                for reflected in fields {
                    let (natural, _) = rust_layout(&reflected.ty);
                    let reserved = u64::from(reflected.binding.size.unwrap_or(natural as u32));
                    assert!(
                        natural <= reserved,
                        "{file} :: {name}.{}: emitted type is {natural} bytes but the shader \
                         reserved {reserved}",
                        reflected.name
                    );
                }
            }
        }
    }

    #[test]
    fn generated_const_matches_the_emitted_struct_size() {
        let path = reflection_dir().join("scene_frag.json");
        let (module, text) = process_file(&path).expect("generate scene_frag bindings");
        assert_eq!(module, "scene_frag");
        // 108 = model(64) + 5×4 scalars + the 12-byte hole + `_padding`(12).
        assert!(
            text.contains("pub const PUSH_CONSTANT_SIZE: u32 = 108;"),
            "{text}"
        );
        assert!(text.contains("pub _pad_84: [u8; 12],"), "{text}");
    }

    #[test]
    fn overlapping_fields_are_rejected() {
        let fields = vec![
            field("whole", vector(4), 0, 16),
            field("overlap", scalar32(), 8, 4),
        ];
        let error = layout_push_fields(&fields).unwrap_err().to_string();
        assert!(error.contains("overlap"), "{error}");
    }
}
