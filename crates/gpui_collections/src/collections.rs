use std::any::TypeId;
use std::hash::{BuildHasherDefault, Hasher};

pub type HashMap<K, V> = FxHashMap<K, V>;
pub type HashSet<T> = FxHashSet<T>;
pub type IndexMap<K, V> = indexmap::IndexMap<K, V, rustc_hash::FxBuildHasher>;
pub type IndexSet<T> = indexmap::IndexSet<T, rustc_hash::FxBuildHasher>;

/// A `HashMap` keyed by [`TypeId`], using a hasher specialized for the fact
/// that a `TypeId` is already a high-quality hash.
pub type TypeIdHashMap<V> = std::collections::HashMap<TypeId, V, BuildHasherDefault<TypeIdHasher>>;
/// A `HashSet` of [`TypeId`], using a hasher specialized for the fact that a
/// `TypeId` is already a high-quality hash.
pub type TypeIdHashSet = std::collections::HashSet<TypeId, BuildHasherDefault<TypeIdHasher>>;

/// A `Hasher` for [`TypeId`]s. `TypeId`s are already thoroughly hashed by the
/// compiler, so there is no need to hash them again — we simply forward the
/// underlying bits.
#[derive(Default)]
pub struct TypeIdHasher(u64);

impl Hasher for TypeIdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // `TypeId`'s `Hash` impl uses `write_u64`/`write_u128` on current Rust,
        // but fold any raw bytes deterministically so this can never panic if
        // the standard library changes how it hashes a `TypeId`.
        for &byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn write_u128(&mut self, value: u128) {
        self.0 = value as u64;
    }
}

pub use indexmap::Equivalent;
pub use rustc_hash::FxHasher;
pub use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
pub use std::collections::*;

pub mod vecmap;
#[cfg(test)]
mod vecmap_tests;
