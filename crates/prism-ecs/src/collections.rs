//! `.Collections`-shaped container facades.
//!
//! The engine's `Native*` containers are manually managed unmanaged memory,
//! tagged with an `Allocator` lifetime and released with an explicit
//! `Dispose()`. Rust has no such manual memory management: these containers use
//! `Box<[T]>` / `Vec<T>` for **safe owned storage**, keep `Allocator` only as
//! metadata (to stay readable next to engine code and to leave room for a
//! custom allocator later), and release through `Drop`.
//!
//! This is therefore **shape alignment**, not a line-by-line port: the
//! manual-management APIs (`Dispose`, `NativeArrayOptions`,
//! `AtomicSafetyHandle`, ...) do not exist here.
//!
//! Naming (Rust uses snake_case; the original name is in parentheses):
//! [`NativeArray::len`] (`Length`), [`NativeArray::copy_from`] (`CopyFrom`),
//! [`NativeList::add`] (`Add`), [`NativeReference::value`] (`Value`),
//! [`NativeSlice`] (`NativeSlice`).

/// `Allocator`: lifetime category for container memory.
///
/// In PrismaRev it does not drive real allocation; it records which allocator
/// the container would use in the engine, for comparison and for a future
/// custom allocator hookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Allocator {
    /// Unspecified (the engine's `Allocator.Invalid`).
    #[default]
    Invalid,
    /// Allocation that needs no release (`Allocator.None`).
    None,
    /// Temporary allocation valid for one frame (`Allocator.Temp`).
    Temp,
    /// Temporary allocation valid for one job (`Allocator.TempJob`).
    TempJob,
    /// Long-lived allocation (`Allocator.Persistent`).
    Persistent,
}

impl Allocator {
    /// Whether this is a recognized allocator (anything but `Allocator.Invalid`).
    pub fn is_valid(self) -> bool {
        self != Allocator::Invalid
    }
}

/// `NativeArray<T>`-shaped fixed-length array.
///
/// Created filled with [`Default`] (mirrors the engine's default
/// `NativeArrayOptions.ClearMemory`); indexing and slicing go through safe
/// Rust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeArray<T> {
    data: Box<[T]>,
    allocator: Allocator,
}

impl<T: Default> NativeArray<T> {
    /// Creates an array of `len` elements (`new NativeArray<T>(len, allocator)`).
    pub fn new(len: usize, allocator: Allocator) -> Self {
        let data = std::iter::repeat_with(T::default)
            .take(len)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { data, allocator }
    }
}

impl<T> NativeArray<T> {
    /// Takes over the storage of an existing `Vec` (no direct engine
    /// equivalent; equivalent to `CopyFrom` followed by releasing the source).
    pub fn from_vec(values: Vec<T>, allocator: Allocator) -> Self {
        Self {
            data: values.into_boxed_slice(),
            allocator,
        }
    }

    /// Number of elements (`Length`).
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the array is empty (`Length == 0`).
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The allocator recorded at creation time.
    pub fn allocator(&self) -> Allocator {
        self.allocator
    }

    /// Read-only slice view.
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    /// Mutable slice view.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }

    /// Reads by index; returns `None` when out of bounds.
    pub fn get(&self, index: usize) -> Option<&T> {
        self.data.get(index)
    }

    /// Mutably reads by index; returns `None` when out of bounds.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.data.get_mut(index)
    }

    /// Read-only iteration.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.data.iter()
    }

    /// Mutable iteration.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.data.iter_mut()
    }

    /// Copies `source` to the front, keeping the element count unchanged
    /// (`CopyFrom`).
    ///
    /// # Panics
    ///
    /// Panics when `source` is longer than this array.
    pub fn copy_from(&mut self, source: &[T])
    where
        T: Copy,
    {
        assert!(
            source.len() <= self.data.len(),
            "copy_from source length {} exceeds array length {}",
            source.len(),
            self.data.len()
        );
        self.data[..source.len()].copy_from_slice(source);
    }

    /// Copies into a `Vec` (`ToArray`).
    pub fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.data.to_vec()
    }
}

impl<T: Clone> NativeArray<T> {
    /// Fills the whole array with one value (no direct equivalent; usually a
    /// `for` loop in the engine).
    pub fn fill(&mut self, value: T) {
        self.data.fill(value);
    }
}

impl<T> std::ops::Index<usize> for NativeArray<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        &self.data[index]
    }
}

impl<T> std::ops::IndexMut<usize> for NativeArray<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.data[index]
    }
}

impl<T> From<Vec<T>> for NativeArray<T> {
    fn from(values: Vec<T>) -> Self {
        Self::from_vec(values, Allocator::Persistent)
    }
}

impl<'a, T> IntoIterator for &'a NativeArray<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut NativeArray<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter_mut()
    }
}

/// `NativeSlice<T>`-shaped read-only view.
///
/// The engine's `NativeSlice` is writable; this Rust mirror only provides a
/// read-only view. Use `&mut [T]` / [`NativeArray::as_mut_slice`] for mutable
/// slices.
#[derive(Debug)]
pub struct NativeSlice<'a, T> {
    data: &'a [T],
}

impl<'a, T> NativeSlice<'a, T> {
    /// Creates a view from a slice.
    pub fn new(data: &'a [T]) -> Self {
        Self { data }
    }

    /// Number of elements (`Length`).
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the slice is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The underlying slice.
    pub fn as_slice(&self) -> &'a [T] {
        self.data
    }

    /// Reads by index; returns `None` when out of bounds.
    pub fn get(&self, index: usize) -> Option<&'a T> {
        self.data.get(index)
    }

    /// Takes a sub-range (`Slice(start, length)`); returns `None` when out of
    /// bounds.
    pub fn slice(&self, start: usize, length: usize) -> Option<NativeSlice<'a, T>> {
        let end = start.checked_add(length)?;
        self.data.get(start..end).map(NativeSlice::new)
    }

    /// Read-only iteration.
    pub fn iter(&self) -> std::slice::Iter<'a, T> {
        self.data.iter()
    }
}

impl<T> Clone for NativeSlice<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for NativeSlice<'_, T> {}

impl<'a, T> From<&'a [T]> for NativeSlice<'a, T> {
    fn from(data: &'a [T]) -> Self {
        Self::new(data)
    }
}

impl<'a, T> IntoIterator for NativeSlice<'a, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

/// `NativeList<T>`-shaped growable list.
///
/// Backed by `Vec<T>`; `Capacity` semantics match common containers (each
/// `add` is not guaranteed to reallocate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeList<T> {
    values: Vec<T>,
    allocator: Allocator,
}

impl<T> NativeList<T> {
    /// Creates an empty list (`new NativeList<T>(allocator)`).
    pub fn new(allocator: Allocator) -> Self {
        Self {
            values: Vec::new(),
            allocator,
        }
    }

    /// Creates an empty list with an initial capacity.
    pub fn with_capacity(capacity: usize, allocator: Allocator) -> Self {
        Self {
            values: Vec::with_capacity(capacity),
            allocator,
        }
    }

    /// Takes over the storage of an existing `Vec`.
    pub fn from_vec(values: Vec<T>, allocator: Allocator) -> Self {
        Self { values, allocator }
    }

    /// Appends an element (`Add`).
    pub fn add(&mut self, value: T) {
        self.values.push(value);
    }

    /// Number of elements (`Length`).
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Current capacity (`Capacity`).
    pub fn capacity(&self) -> usize {
        self.values.capacity()
    }

    /// The allocator recorded at creation time.
    pub fn allocator(&self) -> Allocator {
        self.allocator
    }

    /// Read-only slice.
    pub fn as_slice(&self) -> &[T] {
        &self.values
    }

    /// Mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.values
    }

    /// Reads by index; returns `None` when out of bounds.
    pub fn get(&self, index: usize) -> Option<&T> {
        self.values.get(index)
    }

    /// Mutably reads by index; returns `None` when out of bounds.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.values.get_mut(index)
    }

    /// Clears the elements while keeping the capacity (`Clear`).
    pub fn clear(&mut self) {
        self.values.clear();
    }

    /// Removes and returns the last element (`RemoveAt(len - 1)`).
    pub fn pop(&mut self) -> Option<T> {
        self.values.pop()
    }

    /// Inserts at `index`; returns `false` when `index > len` (`Insert`, which
    /// throws when out of bounds).
    pub fn insert(&mut self, index: usize, value: T) -> bool {
        if index > self.values.len() {
            return false;
        }
        self.values.insert(index, value);
        true
    }

    /// Removes the element at `index`, preserving order; returns `None` when
    /// out of bounds (`RemoveAt`).
    pub fn remove_at(&mut self, index: usize) -> Option<T> {
        if index >= self.values.len() {
            return None;
        }
        Some(self.values.remove(index))
    }

    /// Read-only iteration.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.values.iter()
    }

    /// Mutable iteration.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.values.iter_mut()
    }
}

impl<T: Ord> NativeList<T> {
    /// Sorts in ascending order (`Sort`).
    pub fn sort(&mut self) {
        self.values.sort();
    }
}

impl<T> std::ops::Index<usize> for NativeList<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        &self.values[index]
    }
}

impl<T> std::ops::IndexMut<usize> for NativeList<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.values[index]
    }
}

impl<'a, T> IntoIterator for &'a NativeList<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut NativeList<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.iter_mut()
    }
}

/// `NativeReference<T>`-shaped single-value container.
///
/// Commonly used to pass one value between jobs (the engine pairs it with
/// attributes such as `NativeDisableParallelForRestriction`); on the Rust side
/// it is just a value with allocator metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeReference<T> {
    value: T,
    allocator: Allocator,
}

impl<T> NativeReference<T> {
    /// Creates the reference (`new NativeReference<T>(value, allocator)`).
    pub fn new(value: T, allocator: Allocator) -> Self {
        Self { value, allocator }
    }

    /// Reads the value (`Value`).
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Overwrites the value (`Value = ...`).
    pub fn set_value(&mut self, value: T) {
        self.value = value;
    }

    /// Takes out the inner value.
    pub fn into_inner(self) -> T {
        self.value
    }

    /// The allocator recorded at creation time.
    pub fn allocator(&self) -> Allocator {
        self.allocator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_validity_semantics() {
        assert!(!Allocator::Invalid.is_valid());
        assert!(Allocator::None.is_valid());
        assert!(Allocator::Temp.is_valid());
        assert!(Allocator::TempJob.is_valid());
        assert!(Allocator::Persistent.is_valid());
        assert_eq!(Allocator::default(), Allocator::Invalid);
    }

    #[test]
    fn native_array_clears_memory_and_tracks_allocator() {
        let array = NativeArray::<u32>::new(4, Allocator::TempJob);
        assert_eq!(array.len(), 4);
        assert!(!array.is_empty());
        assert_eq!(array.allocator(), Allocator::TempJob);
        assert_eq!(array.as_slice(), &[0, 0, 0, 0]);
    }

    #[test]
    fn native_array_index_and_copy_from() {
        let mut array = NativeArray::from_vec(vec![1u32, 2, 3], Allocator::Persistent);
        assert_eq!(array[1], 2);
        array[1] = 20;
        array.copy_from(&[7, 8]);
        assert_eq!(array.to_vec(), vec![7, 8, 3]);
        array.fill(5);
        assert_eq!(array.iter().copied().collect::<Vec<_>>(), vec![5, 5, 5]);
        assert_eq!(array.get(9), None);
    }

    #[test]
    fn native_slice_bounds_are_checked() {
        let values = [10, 20, 30, 40];
        let slice = NativeSlice::new(&values);
        assert_eq!(slice.len(), 4);
        assert_eq!(slice.get(2), Some(&30));
        assert_eq!(slice.slice(1, 2).map(|s| s.as_slice()), Some(&[20, 30][..]));
        assert!(slice.slice(3, 2).is_none());
        assert_eq!(slice.iter().copied().sum::<i32>(), 100);
    }

    #[test]
    fn native_list_add_insert_remove_and_sort() {
        let mut list = NativeList::new(Allocator::Temp);
        list.add(3);
        list.add(1);
        assert!(list.insert(1, 2));
        assert!(!list.insert(9, 0));
        assert_eq!(list.as_slice(), &[3, 2, 1]);
        assert_eq!(list.remove_at(0), Some(3));
        assert_eq!(list.remove_at(3), None);
        list.sort();
        assert_eq!(list.as_slice(), &[1, 2]);
        assert_eq!(list.pop(), Some(2));
        assert_eq!(list.allocator(), Allocator::Temp);
    }

    #[test]
    fn native_reference_reads_and_writes() {
        let mut reference = NativeReference::new(5u32, Allocator::Persistent);
        assert_eq!(*reference.value(), 5);
        reference.set_value(9);
        assert_eq!(*reference.value(), 9);
        assert_eq!(reference.allocator(), Allocator::Persistent);
        assert_eq!(reference.into_inner(), 9);
    }
}
