//! `TypeRegistry`: type id to description plus a dynamic constructor.

use std::any::Any;
use std::collections::HashMap;

use crate::{Object, Reflect, ReflectError, TypeDesc, TypeId};

/// A registry entry: a description plus type-erased function pointers in both
/// directions.
pub struct TypeEntry {
    desc: &'static TypeDesc,
    /// The concrete Rust type it was registered for, used to detect "same
    /// name, different type" conflicts (#166).
    rust_type: std::any::TypeId,
    /// Same as above, used only to build the error message.
    rust_type_name: &'static str,
    from_object: fn(&Object) -> Result<Box<dyn Any>, ReflectError>,
    to_object: fn(&dyn Any) -> Option<Object>,
}

impl TypeEntry {
    pub fn descriptor(&self) -> &'static TypeDesc {
        self.desc
    }

    /// Dynamic construction: `Object` to concrete type (type-erased).
    pub fn from_object(&self, object: &Object) -> Result<Box<dyn Any>, ReflectError> {
        (self.from_object)(object)
    }

    /// Dynamic export: concrete type to `Object`; returns `None` on a type
    /// mismatch.
    pub fn to_object(&self, value: &dyn Any) -> Option<Object> {
        (self.to_object)(value)
    }
}

/// Runtime type table.
///
/// The editor, cooker, and scene loader each own (or share) one and register
/// the component types they support. Restoring `Object -> Box<dyn Any>`
/// **does not** require knowing the corresponding Rust type at compile
/// time.
#[derive(Default)]
pub struct TypeRegistry {
    by_id: HashMap<TypeId, TypeEntry>,
    by_name: HashMap<&'static str, TypeId>,
    /// In-process `std::any::TypeId` to stable `TypeId`, for the fast reverse
    /// lookup in `to_object`.
    by_any: HashMap<std::any::TypeId, TypeId>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a reflectable type. Returns the id that previously occupied
    /// the same `TypeId`, if any.
    ///
    /// The stable `TypeId` is derived only from the reflect name, so a name
    /// maps to exactly one Rust type. Registering **the same type** twice is
    /// idempotent (returns `Some(id)`); claiming the same name with a
    /// different Rust type panics immediately, because otherwise data written
    /// by the first type would be decoded by the second into an all-default
    /// instance with no error at all (see #166).
    pub fn register<T: Reflect>(&mut self) -> Option<TypeId> {
        let desc = T::type_desc();
        let id = desc.id;
        let rust_type = std::any::TypeId::of::<T>();
        if let Some(existing) = self.by_id.get(&id) {
            assert_eq!(
                existing.rust_type,
                rust_type,
                "reflect name `{}` (TypeId {}) is already registered by `{}`, \
                 so `{}` cannot reuse it",
                desc.name,
                id,
                existing.rust_type_name,
                std::any::type_name::<T>(),
            );
        }
        self.by_name.insert(desc.name, id);
        self.by_any.insert(rust_type, id);
        let replaced = self.by_id.insert(
            id,
            TypeEntry {
                desc,
                rust_type,
                rust_type_name: std::any::type_name::<T>(),
                from_object: |object| {
                    let value = <T as Reflect>::from_object(object)?;
                    Ok(Box::new(value) as Box<dyn Any>)
                },
                to_object: |value| {
                    value
                        .downcast_ref::<T>()
                        .map(|value| <T as Reflect>::to_object(value))
                },
            },
        );
        replaced.map(|_| id)
    }

    pub fn descriptor(&self, id: TypeId) -> Option<&'static TypeDesc> {
        self.by_id.get(&id).map(TypeEntry::descriptor)
    }

    /// Register a `TypeDesc` directly (useful for tests and dynamic schema registration).
    pub fn register_desc(&mut self, desc: &'static TypeDesc) {
        self.by_id.insert(
            desc.id,
            TypeEntry {
                desc,
                rust_type: std::any::TypeId::of::<()>(),
                rust_type_name: desc.name,
                from_object: |_| Err(ReflectError::NotRegistered(TypeId(0))),
                to_object: |_| None,
            },
        );
        self.by_name.insert(desc.name, desc.id);
    }

    pub fn descriptor_by_name(&self, name: &str) -> Option<&'static TypeDesc> {
        self.by_name.get(name).and_then(|id| self.descriptor(*id))
    }

    pub fn entry(&self, id: TypeId) -> Option<&TypeEntry> {
        self.by_id.get(&id)
    }

    /// Restores an object with the registered constructor.
    pub fn from_object(&self, object: &Object) -> Result<Box<dyn Any>, ReflectError> {
        match self.by_id.get(&object.type_id) {
            Some(entry) => entry.from_object(object),
            None => Err(ReflectError::NotRegistered(object.type_id)),
        }
    }

    /// Exports an object from a concrete type; returns `None` if the type is
    /// not registered.
    pub fn to_object(&self, value: &dyn Any) -> Option<Object> {
        let stable_id = *self.by_any.get(&value.type_id())?;
        self.by_id.get(&stable_id)?.to_object(value)
    }

    /// Number of known types.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&TypeId, &TypeEntry)> + '_ {
        self.by_id.iter()
    }
}
