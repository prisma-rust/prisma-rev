//! RSCN v4 — compact cooked scene format.
//!
//! Unlike RTTIBin, RSCN does **not** embed the full type schema. The file only
//! carries `(TypeId, schema_hash)` entries; the actual field layout comes from
//! the `TypeDesc` compiled into the player. This keeps shipped scenes small
//! while still detecting incompatible field changes.
//!
//! ```text
//! magic   : "RSCN" (4)
//! version : u8 = 4
//! entities: u32
//! types   : u32
//!   type_id     u64
//!   schema_hash u64
//! entities (parent-first):
//!   name str | parent i32 | position [f32;3]
//!   rotation [f32;4] | scale [f32;3] | components u16
//!     type_id u64 | payload_len u32 | payload
//! ```
//!
//! `str` = `u16` byte length + UTF-8 (entity names), while field strings inside
//! a payload use a `u32` length. Payload fields are positional and ordered by
//! [`prism_reflect::TypeDesc::fields`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use prism_reflect::{FieldType, Object, TypeDesc, TypeId, TypeRegistry, Value};

use crate::json::SceneJson;
use crate::registry::ComponentRegistry;

pub const MAGIC: [u8; 4] = *b"RSCN";
pub const VERSION: u8 = 4;

const MAX_ENTITIES: usize = 1 << 24;
const MAX_TYPES: usize = 1 << 16;
const MAX_FIELDS: usize = 1 << 16;
const MAX_DEPTH: u32 = 64;
const MAX_STRING: usize = 1 << 24;
const MAX_COLLECTION: usize = 1 << 24;

/// Smallest possible encoded entity: name length `u16` (2) + parent `i32` (4)
/// + position (12) + rotation (16) + scale (12) + component count `u16` (2).
///
/// Used to clamp the capacity hint derived from `entity_count` before any
/// entity is actually parsed, so a tiny forged header cannot reserve hundreds
/// of megabytes.
const MIN_ENCODED_ENTITY_BYTES: usize = 2 + 4 + 12 + 16 + 12 + 2;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RscnError {
    BadMagic,
    UnsupportedVersion(u8),
    UnexpectedEof,
    InvalidUtf8,
    LimitExceeded(&'static str),
    TrailingBytes,
    UnknownComponent(String),
    NotRegistered(TypeId),
    SchemaMismatch {
        type_name: String,
        stored: u64,
        expected: u64,
    },
    MissingField {
        type_name: String,
        field: String,
    },
    BadValue(String),
}

impl fmt::Display for RscnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "bad RSCN magic"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported RSCN version {version}")
            }
            Self::UnexpectedEof => write!(f, "unexpected end of RSCN data"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 in RSCN data"),
            Self::LimitExceeded(what) => write!(f, "RSCN {what} count exceeds limit"),
            Self::TrailingBytes => write!(f, "trailing bytes after RSCN entity"),
            Self::UnknownComponent(name) => write!(f, "unknown component '{name}'"),
            Self::NotRegistered(id) => write!(f, "type {id} is not registered"),
            Self::SchemaMismatch {
                type_name,
                stored,
                expected,
            } => write!(
                f,
                "schema mismatch for '{type_name}': stored {stored:016x}, runtime {expected:016x}"
            ),
            Self::MissingField { type_name, field } => {
                write!(f, "missing field '{field}' on '{type_name}'")
            }
            Self::BadValue(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for RscnError {}

impl From<prism_reflect::ReflectError> for RscnError {
    fn from(error: prism_reflect::ReflectError) -> Self {
        Self::BadValue(error.to_string())
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RscnHeader {
    pub version: u8,
    pub entity_count: u32,
}

/// Read only the magic/version/entity count (used for cheap manifest checks).
pub fn read_header(data: &[u8]) -> Option<RscnHeader> {
    if data.len() < 9 || data[..4] != MAGIC {
        return None;
    }
    Some(RscnHeader {
        version: data[4],
        entity_count: u32::from_le_bytes(data[5..9].try_into().ok()?),
    })
}

/// `true` if the blob starts with an RSCN v4 header.
pub fn is_v4(data: &[u8]) -> bool {
    data.len() >= 5 && data[..4] == MAGIC && data[4] == VERSION
}

// ---------------------------------------------------------------------------
// Decoded scene
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DecodedScene {
    pub entities: Vec<DecodedEntity>,
}

#[derive(Debug, Clone)]
pub struct DecodedEntity {
    pub name: String,
    pub parent: Option<u32>,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    pub components: Vec<Object>,
}

impl DecodedScene {
    /// Extract the HDR path declared by a `Skybox` component, if any.
    pub fn env_path(&self) -> Option<String> {
        let skybox_id = TypeId::from_name("prism_engine::scene::Skybox");
        self.entities.iter().find_map(|entity| {
            entity.components.iter().find_map(|object| {
                if object.type_id != skybox_id {
                    return None;
                }
                match object.fields.get("hdr_path") {
                    Some(Value::Str(path)) if !path.is_empty() => Some(path.clone()),
                    _ => None,
                }
            })
        })
    }
}

/// Read the skybox HDR path from cooked RSCN bytes (v4 component or v2 header).
///
/// Returns `None` when the data is not valid RSCN v2+ or declares no skybox.
/// The app layer and the build pipeline use this to preload the environment
/// map without spawning an entity graph.
pub fn read_env_path(data: &[u8]) -> Option<String> {
    if is_v4(data) {
        let mut registry = ComponentRegistry::new();
        crate::registry::register_builtin_components(&mut registry);
        return decode_scene(data, &registry)
            .ok()
            .and_then(|scene| scene.env_path());
    }
    if data.len() < 11 || &data[..4] != b"RSCN" {
        return None;
    }
    if data[4] < 2 {
        return None; // v1 has no env path
    }
    let env_len = u16::from_le_bytes(data[9..11].try_into().ok()?) as usize;
    if env_len == 0 || data.len() < 11 + env_len {
        return None;
    }
    let path = String::from_utf8_lossy(&data[11..11 + env_len]).into_owned();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

struct PendingEntity {
    name: String,
    parent: i32,
    position: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
    components: Vec<(TypeId, Vec<u8>)>,
}

/// Encode an authoring scene into the compact RSCN v4 runtime format.
pub fn encode_scene(scene: &SceneJson, registry: &ComponentRegistry) -> Result<Vec<u8>, RscnError> {
    scene.validate().map_err(RscnError::BadValue)?;

    let order = scene.topological_order();
    // 原始下标 → 拓扑序位置。预计算一次，避免每个实体都线性扫描 `order`。
    let mut positions = vec![0usize; order.len()];
    for (position, &index) in order.iter().enumerate() {
        positions[index] = position;
    }
    // `parent` 存的是实体 id，这里解析成数组下标。
    let index_of_id = scene.index_of_id();
    let mut entities = Vec::with_capacity(order.len());
    let mut used_types = BTreeSet::new();

    for &index in &order {
        let source = &scene.entities[index];
        let parent = match source.parent {
            None => -1,
            Some(parent_id) => {
                let parent_index = index_of_id.get(&parent_id).copied().ok_or_else(|| {
                    RscnError::BadValue(format!(
                        "entity {index}: parent id {parent_id} does not exist"
                    ))
                })?;
                positions[parent_index] as i32
            }
        };

        // Sort by name so repeated cooking produces byte-identical output.
        let mut keys: Vec<_> = source.components.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(b.0));

        let mut components = Vec::with_capacity(keys.len());
        for (name, json) in keys {
            let object = registry
                .object_from_json(name, json)
                .ok_or_else(|| RscnError::UnknownComponent(name.clone()))?;
            let descriptor = registry
                .descriptor(object.type_id)
                .ok_or(RscnError::NotRegistered(object.type_id))?;
            collect_schema_types(descriptor.id, registry.type_registry(), &mut used_types)?;
            let mut payload = Vec::new();
            encode_object(
                &object,
                descriptor,
                registry.type_registry(),
                &mut payload,
                0,
            )?;
            components.push((descriptor.id, payload));
        }

        entities.push(PendingEntity {
            name: source.name.clone().unwrap_or_default(),
            parent,
            position: source.transform.position,
            rotation: source.transform.rotation,
            scale: source.transform.scale,
            components,
        });
    }

    if entities.len() > MAX_ENTITIES {
        return Err(RscnError::LimitExceeded("entity"));
    }
    if used_types.len() > MAX_TYPES {
        return Err(RscnError::LimitExceeded("type"));
    }

    let mut output = Vec::with_capacity(16 + entities.len() * 64);
    output.extend_from_slice(&MAGIC);
    output.push(VERSION);
    write_u32(&mut output, entities.len() as u32);
    write_u32(&mut output, used_types.len() as u32);
    let type_registry = registry.type_registry();
    for id in &used_types {
        let descriptor = type_registry
            .descriptor(*id)
            .ok_or(RscnError::NotRegistered(*id))?;
        write_u64(&mut output, id.raw());
        write_u64(&mut output, descriptor.schema_hash(type_registry));
    }

    for entity in &entities {
        if entity.name.len() > u16::MAX as usize {
            return Err(RscnError::LimitExceeded("entity name length"));
        }
        write_u16(&mut output, entity.name.len() as u16);
        output.extend_from_slice(entity.name.as_bytes());
        output.extend_from_slice(&entity.parent.to_le_bytes());
        write_vec3(&mut output, entity.position);
        write_vec4(&mut output, entity.rotation);
        write_vec3(&mut output, entity.scale);
        if entity.components.len() > u16::MAX as usize {
            return Err(RscnError::LimitExceeded("component"));
        }
        write_u16(&mut output, entity.components.len() as u16);
        for (type_id, payload) in &entity.components {
            if payload.len() > u32::MAX as usize {
                return Err(RscnError::LimitExceeded("component payload length"));
            }
            write_u64(&mut output, type_id.raw());
            write_u32(&mut output, payload.len() as u32);
            output.extend_from_slice(payload);
        }
    }

    Ok(output)
}

fn collect_schema_types(
    id: TypeId,
    registry: &TypeRegistry,
    out: &mut BTreeSet<TypeId>,
) -> Result<(), RscnError> {
    if !out.insert(id) {
        return Ok(());
    }
    let descriptor = registry
        .descriptor(id)
        .ok_or(RscnError::NotRegistered(id))?;
    for field in &descriptor.fields {
        collect_field_type(&field.ty, registry, out)?;
    }
    Ok(())
}

fn collect_field_type(
    ty: &FieldType,
    registry: &TypeRegistry,
    out: &mut BTreeSet<TypeId>,
) -> Result<(), RscnError> {
    match ty {
        FieldType::Struct(id) => collect_schema_types(*id, registry, out),
        FieldType::List(inner) | FieldType::Optional(inner) => {
            collect_field_type(inner, registry, out)
        }
        FieldType::Array(inner, _) => collect_field_type(inner, registry, out),
        _ => Ok(()),
    }
}

fn encode_object(
    object: &Object,
    descriptor: &TypeDesc,
    registry: &TypeRegistry,
    out: &mut Vec<u8>,
    depth: u32,
) -> Result<(), RscnError> {
    if depth > MAX_DEPTH {
        return Err(RscnError::LimitExceeded("nesting depth"));
    }
    if object.type_id != descriptor.id {
        return Err(RscnError::BadValue(format!(
            "object type {} does not match descriptor {}",
            object.type_id, descriptor.id
        )));
    }
    for field in &descriptor.fields {
        let value = object
            .fields
            .get(field.name)
            .ok_or_else(|| RscnError::MissingField {
                type_name: descriptor.name.to_owned(),
                field: field.name.to_owned(),
            })?;
        encode_value(value, &field.ty, registry, out, depth + 1)?;
    }
    Ok(())
}

pub fn encode_value(
    value: &Value,
    ty: &FieldType,
    registry: &TypeRegistry,
    out: &mut Vec<u8>,
    depth: u32,
) -> Result<(), RscnError> {
    if depth > MAX_DEPTH {
        return Err(RscnError::LimitExceeded("nesting depth"));
    }
    match ty {
        FieldType::Bool => out.push(if value.as_bool()? { 1 } else { 0 }),
        FieldType::I8 => out.push(i8::try_from(value.as_i64()?).map_err(bad_range)? as u8),
        FieldType::I16 => out.extend_from_slice(
            &i16::try_from(value.as_i64()?)
                .map_err(bad_range)?
                .to_le_bytes(),
        ),
        FieldType::I32 => out.extend_from_slice(
            &i32::try_from(value.as_i64()?)
                .map_err(bad_range)?
                .to_le_bytes(),
        ),
        FieldType::I64 => out.extend_from_slice(&value.as_i64()?.to_le_bytes()),
        FieldType::U8 => out.push(u8::try_from(value.as_u64()?).map_err(bad_range)?),
        FieldType::U16 => out.extend_from_slice(
            &u16::try_from(value.as_u64()?)
                .map_err(bad_range)?
                .to_le_bytes(),
        ),
        FieldType::U32 => out.extend_from_slice(
            &u32::try_from(value.as_u64()?)
                .map_err(bad_range)?
                .to_le_bytes(),
        ),
        FieldType::U64 => out.extend_from_slice(&value.as_u64()?.to_le_bytes()),
        FieldType::F32 => out.extend_from_slice(&(value.as_f32()?).to_le_bytes()),
        FieldType::F64 => out.extend_from_slice(&value.as_f64()?.to_le_bytes()),
        FieldType::String => {
            let text = value.as_str()?;
            if text.len() > MAX_STRING {
                return Err(RscnError::LimitExceeded("string length"));
            }
            write_u32(out, text.len() as u32);
            out.extend_from_slice(text.as_bytes());
        }
        FieldType::Struct(id) => {
            let descriptor = registry
                .descriptor(*id)
                .ok_or(RscnError::NotRegistered(*id))?;
            let object = value.as_object()?;
            encode_object(object, descriptor, registry, out, depth + 1)?;
        }
        FieldType::List(inner) => {
            let items = value.as_list()?;
            if items.len() > MAX_COLLECTION {
                return Err(RscnError::LimitExceeded("list"));
            }
            write_u32(out, items.len() as u32);
            for item in items {
                encode_value(item, inner, registry, out, depth + 1)?;
            }
        }
        FieldType::Optional(inner) => {
            if value.is_none() {
                out.push(0);
            } else {
                out.push(1);
                encode_value(value, inner, registry, out, depth + 1)?;
            }
        }
        FieldType::Array(inner, len) => {
            let items = value.as_list()?;
            if items.len() != *len {
                return Err(RscnError::BadValue(format!(
                    "array length mismatch: expected {len}, got {}",
                    items.len()
                )));
            }
            for item in items {
                encode_value(item, inner, registry, out, depth + 1)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

pub fn decode_scene(data: &[u8], registry: &ComponentRegistry) -> Result<DecodedScene, RscnError> {
    let mut reader = Reader::new(data);
    if reader.take(4)? != MAGIC {
        return Err(RscnError::BadMagic);
    }
    let version = reader.u8()?;
    if version != VERSION {
        return Err(RscnError::UnsupportedVersion(version));
    }

    let entity_count = reader.u32()? as usize;
    if entity_count > MAX_ENTITIES {
        return Err(RscnError::LimitExceeded("entity"));
    }
    let type_count = reader.u32()? as usize;
    if type_count > MAX_TYPES {
        return Err(RscnError::LimitExceeded("type"));
    }

    let type_registry = registry.type_registry();
    let mut types = BTreeMap::new();
    for _ in 0..type_count {
        let id = TypeId(reader.u64()?);
        let stored_hash = reader.u64()?;
        let descriptor = type_registry
            .descriptor(id)
            .ok_or(RscnError::NotRegistered(id))?;
        let expected = descriptor.schema_hash(type_registry);
        if stored_hash != expected {
            return Err(RscnError::SchemaMismatch {
                type_name: descriptor.name.to_owned(),
                stored: stored_hash,
                expected,
            });
        }
        if types.insert(id, descriptor).is_some() {
            return Err(RscnError::BadValue(format!("duplicate type {id} in table")));
        }
    }

    // `entity_count` is attacker-controlled until the first entity is read.
    // Clamp the reservation to what the remaining input could actually hold.
    let capacity = entity_count.min(data.len() / MIN_ENCODED_ENTITY_BYTES);
    let mut entities = Vec::with_capacity(capacity);
    for _ in 0..entity_count {
        let name = reader.short_string()?;
        let parent = reader.i32()?;
        let position = reader.vec3()?;
        let rotation = reader.vec4()?;
        let scale = reader.vec3()?;
        let component_count = reader.u16()? as usize;
        if component_count > MAX_FIELDS {
            return Err(RscnError::LimitExceeded("component"));
        }

        let mut components = Vec::with_capacity(component_count);
        for _ in 0..component_count {
            let id = TypeId(reader.u64()?);
            let payload_len = reader.u32()? as usize;
            let descriptor = *types.get(&id).ok_or(RscnError::NotRegistered(id))?;
            let payload = reader.take(payload_len)?;
            let mut payload_reader = Reader::new(payload);
            let object = decode_object(&mut payload_reader, descriptor, type_registry, 0)?;
            if !payload_reader.is_empty() {
                return Err(RscnError::TrailingBytes);
            }
            components.push(object);
        }

        entities.push(DecodedEntity {
            name,
            parent: if parent < 0 {
                None
            } else {
                Some(parent as u32)
            },
            position,
            rotation,
            scale,
            components,
        });
    }

    if !reader.is_empty() {
        return Err(RscnError::TrailingBytes);
    }
    Ok(DecodedScene { entities })
}

fn decode_object(
    reader: &mut Reader<'_>,
    descriptor: &TypeDesc,
    registry: &TypeRegistry,
    depth: u32,
) -> Result<Object, RscnError> {
    if depth > MAX_DEPTH {
        return Err(RscnError::LimitExceeded("nesting depth"));
    }
    let mut object = Object::new(descriptor.id, descriptor.name);
    for field in &descriptor.fields {
        let value = decode_value(reader, &field.ty, registry, depth + 1)?;
        object.fields.insert(field.name.to_owned(), value);
    }
    Ok(object)
}

pub fn decode_value(
    reader: &mut Reader<'_>,
    ty: &FieldType,
    registry: &TypeRegistry,
    depth: u32,
) -> Result<Value, RscnError> {
    if depth > MAX_DEPTH {
        return Err(RscnError::LimitExceeded("nesting depth"));
    }
    Ok(match ty {
        FieldType::Bool => match reader.u8()? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            other => return Err(RscnError::BadValue(format!("invalid bool byte {other}"))),
        },
        FieldType::I8 => Value::I64(reader.u8()? as i8 as i64),
        FieldType::I16 => Value::I64(reader.i16()? as i64),
        FieldType::I32 => Value::I64(reader.i32()? as i64),
        FieldType::I64 => Value::I64(reader.i64()?),
        FieldType::U8 => Value::U64(reader.u8()? as u64),
        FieldType::U16 => Value::U64(reader.u16()? as u64),
        FieldType::U32 => Value::U64(reader.u32()? as u64),
        FieldType::U64 => Value::U64(reader.u64()?),
        FieldType::F32 => Value::F64(reader.f32()? as f64),
        FieldType::F64 => Value::F64(reader.f64()?),
        FieldType::String => Value::Str(reader.long_string()?),
        FieldType::Struct(id) => {
            let descriptor = registry
                .descriptor(*id)
                .ok_or(RscnError::NotRegistered(*id))?;
            Value::Object(decode_object(reader, descriptor, registry, depth + 1)?)
        }
        FieldType::List(inner) => {
            let count = reader.u32()? as usize;
            if count > MAX_COLLECTION {
                return Err(RscnError::LimitExceeded("list"));
            }
            let mut items = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                items.push(decode_value(reader, inner, registry, depth + 1)?);
            }
            Value::List(items)
        }
        FieldType::Optional(inner) => match reader.u8()? {
            0 => Value::None,
            1 => decode_value(reader, inner, registry, depth + 1)?,
            other => return Err(RscnError::BadValue(format!("invalid optional tag {other}"))),
        },
        FieldType::Array(inner, len) => {
            let mut items = Vec::with_capacity(*len);
            for _ in 0..*len {
                items.push(decode_value(reader, inner, registry, depth + 1)?);
            }
            Value::List(items)
        }
    })
}

// ---------------------------------------------------------------------------
// Byte helpers
// ---------------------------------------------------------------------------

fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn write_vec3(out: &mut Vec<u8>, value: [f32; 3]) {
    for item in value {
        out.extend_from_slice(&item.to_le_bytes());
    }
}
fn write_vec4(out: &mut Vec<u8>, value: [f32; 4]) {
    for item in value {
        out.extend_from_slice(&item.to_le_bytes());
    }
}

fn bad_range<E: fmt::Display>(error: E) -> RscnError {
    RscnError::BadValue(format!("integer out of range: {error}"))
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos == self.data.len()
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8], RscnError> {
        let end = self.pos.checked_add(len).ok_or(RscnError::UnexpectedEof)?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or(RscnError::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, RscnError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, RscnError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, RscnError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64, RscnError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16, RscnError> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, RscnError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64, RscnError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32, RscnError> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> Result<f64, RscnError> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn vec3(&mut self) -> Result<[f32; 3], RscnError> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }

    pub fn vec4(&mut self) -> Result<[f32; 4], RscnError> {
        Ok([self.f32()?, self.f32()?, self.f32()?, self.f32()?])
    }

    pub fn short_string(&mut self) -> Result<String, RscnError> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| RscnError::InvalidUtf8)
    }

    pub fn long_string(&mut self) -> Result<String, RscnError> {
        let len = self.u32()? as usize;
        if len > MAX_STRING {
            return Err(RscnError::LimitExceeded("string length"));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| RscnError::InvalidUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ComponentRegistry;

    #[test]
    fn forged_entity_count_does_not_preallocate_unbounded() {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC);
        data.push(VERSION);
        data.extend_from_slice(&(MAX_ENTITIES as u32).to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(data.len(), 13);

        let error = decode_scene(&data, &ComponentRegistry::new()).unwrap_err();
        assert!(matches!(error, RscnError::UnexpectedEof));
    }
}
