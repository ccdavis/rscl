//! Persistent hash set: a hash trie (see `hamt.rs`) with metadata.
use super::{Kind, alloc, cast, vector};
use crate::value::{ObjHeader, Value};

pub use super::hamt::Set_;

#[repr(C)]
pub struct SetObj {
    pub hdr: ObjHeader,
    pub s: Set_,
    pub meta: Value,
    /// For a set the reader built: a vector of its elements in source order.
    /// Otherwise `nil`.
    pub order: Value,
}

pub fn new(s: Set_) -> Value {
    alloc(SetObj {
        hdr: ObjHeader::new(Kind::Set),
        s,
        meta: Value::NIL,
        order: Value::NIL,
    })
}

/// A set literal: `items` is the order its elements were written in.
pub fn new_ordered(s: Set_, items: Vec<Value>) -> Value {
    alloc(SetObj {
        hdr: ObjHeader::new(Kind::Set),
        s,
        meta: Value::NIL,
        order: vector::from_slice(&items),
    })
}

/// A new set object with the contents of `o` (and no metadata).
pub fn copy_of(o: &SetObj) -> Value {
    alloc(SetObj {
        hdr: ObjHeader::new(Kind::Set),
        s: o.s,
        meta: Value::NIL,
        order: o.order,
    })
}

/// The elements in source order when the set came from the reader.
pub fn ordered_items(v: &Value) -> Vec<Value> {
    let o = get(v);
    match vector::as_vec(&o.order) {
        Some(items) if items.len() == o.s.len() => items.to_vec(),
        _ => o.s.iter().copied().collect(),
    }
}

/// The element of `s` equal to `k`, as stored (`1` for `1N`, a list for an equal
/// vector).
pub fn stored(s: &Set_, k: &Value) -> Option<Value> {
    s.get(k).copied()
}

pub fn empty_set() -> Set_ {
    Set_::new()
}

pub fn from_slice(items: &[Value]) -> Value {
    let mut s = empty_set();
    for x in items {
        s.insert(*x);
    }
    new(s)
}

pub fn get(v: &Value) -> &SetObj {
    debug_assert!(v.is_kind(Kind::Set));
    unsafe { cast::<SetObj>(v) }
}
