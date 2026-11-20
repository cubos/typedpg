//! [`NodeBox`], the box every AST node lives in.
//!
//! A parse tree is as deep as the query nests: `SELECT 1 + 1 + …` with
//! 4,995 terms is 10,000 levels, the most the converter accepts. Dropping a
//! tree of plain `Box`es recurses once per level, and each level's drop glue
//! takes stack — enough, in a debug build, to overflow a 2 MB thread stack
//! on the deepest trees. So the AST boxes its nodes in a `NodeBox`, whose
//! `Drop` runs the drop of what it holds on a stack segment `stacker` grows
//! when less than 64 kB are left: dropping any tree works on any thread
//! stack, whatever the build profile.
//!
//! The `Drop` lives on the box rather than on [`Node`](crate::ast::Node),
//! so a node can still be moved out of a `Node` by value
//! (`match node { Node::SelectStmt(s) => … }`), which a `Drop` impl on the
//! enum would forbid.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};

/// A `Box<T>` whose drop never overflows the stack (see the module docs).
pub struct NodeBox<T>(ManuallyDrop<Box<T>>);

impl<T> NodeBox<T> {
    /// Box `value`.
    pub fn new(value: T) -> NodeBox<T> {
        NodeBox(ManuallyDrop::new(Box::new(value)))
    }

    /// The boxed value.
    pub fn into_inner(self) -> T {
        let mut this = ManuallyDrop::new(self);
        // SAFETY: `this` is never dropped, so the box is taken exactly once.
        *unsafe { ManuallyDrop::take(&mut this.0) }
    }
}

impl<T> Drop for NodeBox<T> {
    fn drop(&mut self) {
        // SAFETY: `drop` runs once, and nothing uses the box after it.
        let inner = unsafe { ManuallyDrop::take(&mut self.0) };
        stacker::maybe_grow(64 * 1024, 1024 * 1024, move || drop(inner));
    }
}

impl<T> Deref for NodeBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for NodeBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T> AsRef<T> for NodeBox<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> AsMut<T> for NodeBox<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T> From<T> for NodeBox<T> {
    fn from(value: T) -> NodeBox<T> {
        NodeBox::new(value)
    }
}

impl<T: Clone> Clone for NodeBox<T> {
    fn clone(&self) -> NodeBox<T> {
        // Cloning recurses as deep as dropping does.
        stacker::maybe_grow(64 * 1024, 1024 * 1024, || NodeBox::new((**self).clone()))
    }
}

impl<T: Default> Default for NodeBox<T> {
    fn default() -> NodeBox<T> {
        NodeBox::new(T::default())
    }
}

impl<T: PartialEq> PartialEq for NodeBox<T> {
    fn eq(&self, other: &NodeBox<T>) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for NodeBox<T> {}

impl<T: Hash> Hash for NodeBox<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl<T: fmt::Debug> fmt::Debug for NodeBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: serde::Serialize> serde::Serialize for NodeBox<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (**self).serialize(serializer)
    }
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for NodeBox<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<NodeBox<T>, D::Error> {
        T::deserialize(deserializer).map(NodeBox::new)
    }
}
