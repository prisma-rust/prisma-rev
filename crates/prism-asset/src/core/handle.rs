//! 运行时安全的句柄类型。
//!
//! `Handle<T>` 是一个代际计数的运行时槽数组索引。它是运行时代码引用已加载资源的主要方式。
//! 代际守卫防止了句柄比其资源存活更久的释放后使用错误。
//!
//! 句柄空间分为两个区域：
//! - **静态**（索引 < `MAX_STATIC`）：众所周知/回退资源。
//! - **动态**：在运行时加载。

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

/// 为静态/众所周知句柄保留的最大索引值。
/// Everything above this is a dynamically-loaded asset.
pub const MAX_STATIC_INDEX: u32 = 1024;

/// A generation-counted handle to a runtime resource of type `T`.
///
/// `Handle<T>` is `Copy`, `Send`, `Sync` and has the same size as `u64`.
pub struct Handle<T: ?Sized> {
    /// Packed: index in the low 32 bits, generation in the high 32 bits.
    packed: u64,
    /// `fn() -> T` keeps `Handle<T>: Send + Sync` for every `T` without
    /// implying ownership (a raw `*const T` would make it `!Send + !Sync`).
    _marker: PhantomData<fn() -> T>,
}

// Manual Clone/Copy to avoid deriving T: Clone/T: Copy bounds.
impl<T: ?Sized> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: ?Sized> Copy for Handle<T> {}

// Manual PartialEq/Eq/Hash to avoid deriving T-bound versions.
impl<T: ?Sized> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.packed == other.packed
    }
}
impl<T: ?Sized> Eq for Handle<T> {}
impl<T: ?Sized> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.packed.hash(state);
    }
}

impl<T: ?Sized> Handle<T> {
    const INDEX_MASK: u64 = 0x0000_0000_FFFF_FFFF;
    const GENERATION_SHIFT: u64 = 32;

    /// Create a new handle from an index and generation.
    #[inline]
    pub const fn new(index: u32, generation: u32) -> Self {
        Self {
            packed: ((generation as u64) << Self::GENERATION_SHIFT) | (index as u64),
            _marker: PhantomData,
        }
    }

    /// The slot index this handle points to.
    #[inline]
    pub const fn index(self) -> u32 {
        (self.packed & Self::INDEX_MASK) as u32
    }

    /// The generation of the slot this handle was created for.
    #[inline]
    pub const fn generation(self) -> u32 {
        (self.packed >> Self::GENERATION_SHIFT) as u32
    }

    /// Returns `true` if this handle points to a static / well-known asset.
    #[inline]
    pub fn is_static(self) -> bool {
        self.index() < MAX_STATIC_INDEX
    }

    /// A null/invalid handle (index 0, generation 0).
    #[inline]
    pub const fn null() -> Self {
        Self {
            packed: 0,
            _marker: PhantomData,
        }
    }

    /// Returns `true` if this is the null handle.
    #[inline]
    pub fn is_null(self) -> bool {
        self.packed == 0
    }

    /// Unpack into the raw components (index, generation).
    #[inline]
    pub const fn into_raw_parts(self) -> (u32, u32) {
        (self.index(), self.generation())
    }

    /// Pack from raw components. Inverse of [`into_raw_parts`](Self::into_raw_parts).
    #[inline]
    pub const fn from_raw_parts(index: u32, generation: u32) -> Self {
        Self::new(index, generation)
    }
}

impl<T: ?Sized> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let type_name = std::any::type_name::<T>();
        write!(
            f,
            "Handle<{}>(index={}, gen={})",
            type_name,
            self.index(),
            self.generation()
        )
    }
}

impl<T: ?Sized> Default for Handle<T> {
    fn default() -> Self {
        Self::null()
    }
}

// ---------------------------------------------------------------------------
// Type-erased handle (for heterogeneous storage)
// ---------------------------------------------------------------------------

/// A type-erased handle. Use this in collections that store handles of mixed
/// types (e.g. the runtime's asset slot array).
///
/// Convert to/from a typed `Handle<T>` via `From` / `Into`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnyHandle {
    packed: u64,
}

impl AnyHandle {
    /// Create from raw packed value.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self { packed: raw }
    }

    /// Get the raw packed value.
    #[inline]
    pub const fn into_raw(self) -> u64 {
        self.packed
    }

    /// The slot index.
    #[inline]
    pub fn index(self) -> u32 {
        (self.packed & 0x0000_0000_FFFF_FFFF) as u32
    }

    /// The generation.
    #[inline]
    pub fn generation(self) -> u32 {
        (self.packed >> 32) as u32
    }

    /// Null handle.
    #[inline]
    pub const fn null() -> Self {
        Self { packed: 0 }
    }

    /// Returns `true` if this is the null handle.
    #[inline]
    pub fn is_null(self) -> bool {
        self.packed == 0
    }
}

impl<T: ?Sized> From<Handle<T>> for AnyHandle {
    #[inline]
    fn from(h: Handle<T>) -> Self {
        Self { packed: h.packed }
    }
}

impl<T: ?Sized> From<AnyHandle> for Handle<T> {
    #[inline]
    fn from(h: AnyHandle) -> Self {
        Self {
            packed: h.packed,
            _marker: PhantomData,
        }
    }
}

impl fmt::Debug for AnyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AnyHandle(index={}, gen={})",
            self.index(),
            self.generation()
        )
    }
}

// ---------------------------------------------------------------------------
// Type alias for asset handles
// ---------------------------------------------------------------------------

/// A handle to any asset type (type-erased, stored alongside a type tag).
pub type AnyAsset = AnyHandle;

#[cfg(test)]
#[path = "handle_tests.rs"]
mod tests;
