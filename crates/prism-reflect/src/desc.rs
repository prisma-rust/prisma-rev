//! Type descriptions: `TypeId`, `FieldType`, `FieldDesc`, and `TypeDesc`.

use std::collections::HashSet;
use std::fmt;

use crate::registry::TypeRegistry;

/// FNV-1a 64-bit parameters, shared by type ids and schema fingerprints.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A stable type identifier.
///
/// This is **not** `std::any::TypeId`, which changes on every build and
/// cannot be persisted. Instead it hashes the type name (by default
/// `module_path!()::Type`) with FNV-1a 64, which is stable across processes
/// and builds, so it is safe to write to files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeId(pub u64);

impl TypeId {
    /// Computes the stable id from a canonical type name (deterministic
    /// FNV-1a).
    pub const fn from_name(name: &str) -> Self {
        let bytes = name.as_bytes();
        let mut hash: u64 = FNV_OFFSET_BASIS;
        let mut i = 0;
        while i < bytes.len() {
            hash ^= bytes[i] as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
            i += 1;
        }
        Self(hash)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// The kind of type. Only structs exist today; enums and unions are left to a
/// future format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Struct,
}

/// The static type of a field.
///
/// `Struct` points at another `TypeDesc` to support nested objects; `List`,
/// `Optional`, and `Array` are containers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    String,
    Struct(TypeId),
    List(Box<FieldType>),
    Optional(Box<FieldType>),
    Array(Box<FieldType>, usize),
}

impl FieldType {
    /// Whether the field is inline and fixed-size (can be memcpy'd into a POD
    /// layout).
    ///
    /// Strings, containers, and nested objects return `false`.
    pub fn is_plain(&self) -> bool {
        matches!(
            self,
            Self::Bool
                | Self::I8
                | Self::I16
                | Self::I32
                | Self::I64
                | Self::U8
                | Self::U16
                | Self::U32
                | Self::U64
                | Self::F32
                | Self::F64
        )
    }

    /// Human-readable name of the field type (for error messages and editor
    /// hints).
    pub fn display_name(&self) -> String {
        match self {
            Self::Bool => "bool".into(),
            Self::I8 => "i8".into(),
            Self::I16 => "i16".into(),
            Self::I32 => "i32".into(),
            Self::I64 => "i64".into(),
            Self::U8 => "u8".into(),
            Self::U16 => "u16".into(),
            Self::U32 => "u32".into(),
            Self::U64 => "u64".into(),
            Self::F32 => "f32".into(),
            Self::F64 => "f64".into(),
            Self::String => "string".into(),
            Self::Struct(id) => format!("struct {id}"),
            Self::List(inner) => format!("list<{}>", inner.display_name()),
            Self::Optional(inner) => format!("option<{}>", inner.display_name()),
            Self::Array(inner, len) => format!("[{}; {len}]", inner.display_name()),
        }
    }
}

/// Description of a single field.
#[derive(Debug, Clone)]
pub struct FieldDesc {
    /// Field name in the schema (overridable with
    /// `#[reflect(rename = "...")]`).
    pub name: &'static str,
    /// Byte offset from the start of the struct (`core::mem::offset_of!`).
    pub offset: usize,
    /// The field type.
    pub ty: FieldType,
    /// Editor read-only (`set_field` rejects writes).
    pub read_only: bool,
    /// Enum variant names (the strings used in authored JSON); `None` for
    /// non-enum fields.
    ///
    /// Comes from
    /// [`ReflectType::enum_variants`](crate::ReflectType::enum_variants).
    /// It **does not participate in
    /// [`TypeDesc::schema_hash`]**: it is only editor display metadata, so
    /// renaming a variant must not invalidate cooked `.rscn` data.
    pub enum_variants: Option<&'static [&'static str]>,
}

/// The full description of a type.
#[derive(Debug, Clone)]
pub struct TypeDesc {
    /// Stable id (same name, same id).
    pub id: TypeId,
    /// Canonical type name.
    pub name: &'static str,
    /// Schema version, bumped when fields change (used by migration).
    pub version: u32,
    /// `size_of::<T>()`.
    pub size: usize,
    /// `align_of::<T>()`.
    pub align: usize,
    /// The kind of type.
    pub kind: TypeKind,
    /// Field list in source order.
    pub fields: Vec<FieldDesc>,
}

impl TypeDesc {
    pub fn field(&self, name: &str) -> Option<&FieldDesc> {
        self.fields.iter().find(|field| field.name == name)
    }

    pub fn field_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.fields.iter().map(|field| field.name)
    }

    /// A schema fingerprint independent of memory layout.
    ///
    /// Covers: type name, `version`, field order, field names, and field
    /// types (including nested ones). Does not cover: field offsets,
    /// `size_of`, `align_of`, or `read_only`.
    ///
    /// Use: the compact runtime binary stores only `(TypeId, schema_hash)`
    /// and recomputes and compares it on load using the `TypeDesc` compiled
    /// into the binary. This way:
    /// - reordered fields or changed padding do not invalidate cooked data;
    /// - added, removed, renamed, retyped, or reordered fields are caught
    ///   immediately, even if the author forgot to bump `version`.
    ///
    /// Note: `TypeId` hashes only the type name, so it **must not** be used
    /// alone to validate a schema.
    pub fn schema_hash(&self, registry: &TypeRegistry) -> u64 {
        let mut hash = FNV_OFFSET_BASIS;
        let mut visiting = HashSet::new();
        visiting.insert(self.id);
        self.hash_into(&mut hash, registry, &mut visiting);
        hash
    }

    /// Registry-independent schema hash for leaf types or when no registry is available.
    /// Nested struct fields hash their TypeId without inspecting nested fields.
    pub fn schema_hash_standalone(&self) -> u64 {
        let registry = TypeRegistry::new();
        self.schema_hash(&registry)
    }

    pub(crate) fn hash_into(
        &self,
        hash: &mut u64,
        registry: &TypeRegistry,
        visiting: &mut HashSet<TypeId>,
    ) {
        hash_bytes(hash, self.name.as_bytes());
        hash_bytes(hash, &self.version.to_le_bytes());
        hash_bytes(hash, &(self.fields.len() as u32).to_le_bytes());
        for field in &self.fields {
            hash_bytes(hash, field.name.as_bytes());
            hash_bytes(hash, &[0xff]); // field separator
            hash_field_type(hash, &field.ty, registry, visiting);
        }
    }
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= *byte as u64;
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn hash_field_type(
    hash: &mut u64,
    ty: &FieldType,
    registry: &TypeRegistry,
    visiting: &mut HashSet<TypeId>,
) {
    match ty {
        FieldType::Bool => hash_bytes(hash, &[0]),
        FieldType::I8 => hash_bytes(hash, &[1]),
        FieldType::I16 => hash_bytes(hash, &[2]),
        FieldType::I32 => hash_bytes(hash, &[3]),
        FieldType::I64 => hash_bytes(hash, &[4]),
        FieldType::U8 => hash_bytes(hash, &[5]),
        FieldType::U16 => hash_bytes(hash, &[6]),
        FieldType::U32 => hash_bytes(hash, &[7]),
        FieldType::U64 => hash_bytes(hash, &[8]),
        FieldType::F32 => hash_bytes(hash, &[9]),
        FieldType::F64 => hash_bytes(hash, &[10]),
        FieldType::String => hash_bytes(hash, &[11]),
        FieldType::Struct(id) => {
            hash_bytes(hash, &[12]);
            hash_bytes(hash, &id.raw().to_le_bytes());
            if let Some(nested) = registry.descriptor(*id) {
                if visiting.insert(*id) {
                    nested.hash_into(hash, registry, visiting);
                    visiting.remove(id);
                } else {
                    hash_bytes(hash, &[0xfe]);
                }
            }
        }
        FieldType::List(inner) => {
            hash_bytes(hash, &[13]);
            hash_field_type(hash, inner, registry, visiting);
        }
        FieldType::Optional(inner) => {
            hash_bytes(hash, &[14]);
            hash_field_type(hash, inner, registry, visiting);
        }
        FieldType::Array(inner, len) => {
            hash_bytes(hash, &[15]);
            hash_field_type(hash, inner, registry, visiting);
            hash_bytes(hash, &(*len as u32).to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(version: u32, fields: Vec<FieldDesc>) -> TypeDesc {
        TypeDesc {
            id: TypeId::from_name("test::Thing"),
            name: "test::Thing",
            version,
            size: 0,
            align: 0,
            kind: TypeKind::Struct,
            fields,
        }
    }

    fn field(name: &'static str, offset: usize, ty: FieldType) -> FieldDesc {
        FieldDesc {
            name,
            offset,
            ty,
            read_only: false,
            enum_variants: None,
        }
    }

    #[test]
    fn schema_hash_ignores_layout_and_editor_flags() {
        let a = desc(
            1,
            vec![
                field("x", 0, FieldType::F32),
                FieldDesc {
                    name: "tag",
                    offset: 4,
                    ty: FieldType::U32,
                    read_only: false,
                    enum_variants: Some(&["A", "B"]),
                },
            ],
        );
        let b = desc(
            1,
            vec![
                field("x", 128, FieldType::F32),
                FieldDesc {
                    name: "tag",
                    offset: 256,
                    ty: FieldType::U32,
                    read_only: true, // editor flag flipped
                    enum_variants: None,
                },
            ],
        );
        let reg = TypeRegistry::new();
        assert_eq!(a.schema_hash(&reg), b.schema_hash(&reg));
        assert_eq!(a.schema_hash_standalone(), b.schema_hash_standalone());
    }

    #[test]
    fn schema_hash_tracks_everything_that_changes_encoding() {
        let reg = TypeRegistry::new();
        let base = desc(
            1,
            vec![
                field("a", 0, FieldType::I32),
                field("b", 4, FieldType::String),
            ],
        );
        let reordered = desc(
            1,
            vec![
                field("b", 0, FieldType::String),
                field("a", 8, FieldType::I32),
            ],
        );
        let renamed = desc(
            1,
            vec![
                field("c", 0, FieldType::I32),
                field("b", 4, FieldType::String),
            ],
        );
        let retyped = desc(
            1,
            vec![
                field("a", 0, FieldType::I64),
                field("b", 8, FieldType::String),
            ],
        );
        let versioned = desc(
            2,
            vec![
                field("a", 0, FieldType::I32),
                field("b", 4, FieldType::String),
            ],
        );
        let extra = desc(
            1,
            vec![
                field("a", 0, FieldType::I32),
                field("b", 4, FieldType::String),
                field("c", 8, FieldType::Bool),
            ],
        );
        let hash = base.schema_hash(&reg);
        assert_ne!(hash, reordered.schema_hash(&reg));
        assert_ne!(hash, renamed.schema_hash(&reg));
        assert_ne!(hash, retyped.schema_hash(&reg));
        assert_ne!(hash, versioned.schema_hash(&reg));
        assert_ne!(hash, extra.schema_hash(&reg));
        assert_eq!(hash, base.schema_hash(&reg));
    }

    #[test]
    fn schema_hash_covers_nested_types() {
        let mut reg_v1 = TypeRegistry::new();
        let mut reg_v2 = TypeRegistry::new();

        let inner_v1 = Box::leak(Box::new(TypeDesc {
            id: TypeId::from_name("test::Inner"),
            name: "test::Inner",
            version: 1,
            size: 4,
            align: 4,
            kind: TypeKind::Struct,
            fields: vec![field("x", 0, FieldType::F32)],
        }));
        let inner_v2 = Box::leak(Box::new(TypeDesc {
            id: TypeId::from_name("test::Inner"),
            name: "test::Inner",
            version: 2,
            size: 8,
            align: 4,
            kind: TypeKind::Struct,
            fields: vec![field("x", 0, FieldType::F32), field("y", 4, FieldType::F32)],
        }));

        reg_v1.register_desc(inner_v1);
        reg_v2.register_desc(inner_v2);

        let outer = desc(
            1,
            vec![field(
                "inner",
                0,
                FieldType::Struct(TypeId::from_name("test::Inner")),
            )],
        );

        // Nested struct schema change MUST change the outer struct's schema_hash!
        assert_ne!(outer.schema_hash(&reg_v1), outer.schema_hash(&reg_v2));

        // Also test nested inside List
        let outer_list = desc(
            1,
            vec![field(
                "items",
                0,
                FieldType::List(Box::new(FieldType::Struct(TypeId::from_name(
                    "test::Inner",
                )))),
            )],
        );
        assert_ne!(
            outer_list.schema_hash(&reg_v1),
            outer_list.schema_hash(&reg_v2)
        );

        // Also test self-referential / recursive struct termination
        let recursive = Box::leak(Box::new(TypeDesc {
            id: TypeId::from_name("test::Recursive"),
            name: "test::Recursive",
            version: 1,
            size: 8,
            align: 8,
            kind: TypeKind::Struct,
            fields: vec![field(
                "next",
                0,
                FieldType::Optional(Box::new(FieldType::Struct(TypeId::from_name(
                    "test::Recursive",
                )))),
            )],
        }));
        let mut reg_rec = TypeRegistry::new();
        reg_rec.register_desc(recursive);

        // Must terminate without stack overflow
        let rec_hash = recursive.schema_hash(&reg_rec);
        assert_ne!(rec_hash, 0);
    }
}
