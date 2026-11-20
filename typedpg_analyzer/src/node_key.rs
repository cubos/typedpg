//! Identity keys for AST nodes.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

/// The identity of one node of kind `T` in a parsed tree: its address.
///
/// A key identifies a node of *one* tree for as long as that tree is
/// neither moved nor dropped (the AST boxes its nodes, so moving the
/// handle that owns a subtree does not move the nodes in it). A clone of
/// a subtree is a different tree: each of its nodes has a different key.
/// So never clone a subtree on a path whose nodes' keys are looked up —
/// the lookup would silently miss.
///
/// Keys of different node kinds are different types, so they cannot be
/// compared or mixed in one map.
pub(crate) struct NodeKey<T: ?Sized>(usize, PhantomData<fn() -> *const T>);

impl<T: ?Sized> NodeKey<T> {
    /// The key of `node`.
    pub(crate) fn of(node: &T) -> Self {
        Self(std::ptr::from_ref(node).cast::<()>().addr(), PhantomData)
    }
}

impl<T: ?Sized> Clone for NodeKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for NodeKey<T> {}

impl<T: ?Sized> PartialEq for NodeKey<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: ?Sized> Eq for NodeKey<T> {}

impl<T: ?Sized> PartialOrd for NodeKey<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: ?Sized> Ord for NodeKey<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<T: ?Sized> Hash for NodeKey<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<T: ?Sized> fmt::Debug for NodeKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeKey({:#x})", self.0)
    }
}
