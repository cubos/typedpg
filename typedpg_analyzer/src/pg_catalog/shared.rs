//! Copy-on-write catalog tables.
//!
//! A transaction or a savepoint snapshots the whole catalog to roll back to,
//! and the `sql!` macro hands each query its own copy. Most of the catalog
//! is the seed (thousands of functions, operators, types), which a migration
//! rarely touches, so copying it every time made applying migrations
//! quadratic in the schema's size. [`Shared`] keeps a table behind an `Arc`:
//! cloning the catalog shares every table, and a table is copied only on its
//! first write after a clone ([`Arc::make_mut`]).
//!
//! Each table also counts its writes ([`Shared::version`]), so a reader that
//! caches what it derived from a table — the stored expressions already
//! processed, say — can tell whether the table changed since. The counter is
//! shared by every copy of the catalog (a snapshot bumps it too, which only
//! makes the reader look again).

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A catalog table shared between catalog copies until one writes to it.
#[derive(Debug)]
pub struct Shared<T> {
    data: Arc<T>,
    /// How many times the table was opened for writing (see the module
    /// docs).
    version: Arc<AtomicU64>,
}

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Shared {
            data: Arc::new(value),
            version: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The table, owned: moved out if this copy is its only owner, cloned
    /// otherwise.
    pub fn into_inner(self) -> T
    where
        T: Clone,
    {
        Arc::unwrap_or_clone(self.data)
    }

    /// How many times the table was opened for writing. Equal versions mean
    /// the table was not written in between; a version only ever grows.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Shared::new(T::default())
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared {
            data: Arc::clone(&self.data),
            version: Arc::clone(&self.version),
        }
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.data
    }
}

impl<T: Clone> DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.version.fetch_add(1, Ordering::Relaxed);
        Arc::make_mut(&mut self.data)
    }
}

impl<T> From<T> for Shared<T> {
    fn from(value: T) -> Self {
        Shared::new(value)
    }
}

impl<'a, T> IntoIterator for &'a Shared<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        (&*self.data).into_iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Shared<T>
where
    &'a mut T: IntoIterator,
{
    type Item = <&'a mut T as IntoIterator>::Item;
    type IntoIter = <&'a mut T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.version.fetch_add(1, Ordering::Relaxed);
        Arc::make_mut(&mut self.data).into_iter()
    }
}
