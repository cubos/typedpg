//! Copy-on-write catalog tables.
//!
//! A transaction or a savepoint snapshots the whole catalog to roll back to,
//! and the `sql!` macro hands each query its own copy. Most of the catalog
//! is the seed (thousands of functions, operators, types), which a migration
//! rarely touches, so copying it every time made applying migrations
//! quadratic in the schema's size. [`Shared`] keeps a table behind an `Arc`:
//! cloning the catalog shares every table, and a table is copied only on its
//! first write after a clone ([`Arc::make_mut`]).

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// A catalog table shared between catalog copies until one writes to it.
#[derive(Debug, Default)]
pub struct Shared<T>(Arc<T>);

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Shared(Arc::new(value))
    }

    /// The table, owned: moved out if this copy is its only owner, cloned
    /// otherwise.
    pub fn into_inner(self) -> T
    where
        T: Clone,
    {
        Arc::unwrap_or_clone(self.0)
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(Arc::clone(&self.0))
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        Arc::make_mut(&mut self.0)
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
        (&*self.0).into_iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Shared<T>
where
    &'a mut T: IntoIterator,
{
    type Item = <&'a mut T as IntoIterator>::Item;
    type IntoIter = <&'a mut T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        Arc::make_mut(&mut self.0).into_iter()
    }
}
