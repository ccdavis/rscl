//! Persistent hash map: a hash trie (see `hamt.rs`) with metadata.
use super::{Kind, alloc, cast, vector};
use crate::value::{ObjHeader, Value};

pub use super::hamt::Map_;

#[repr(C)]
pub struct MapObj {
    pub hdr: ObjHeader,
    pub m: Map_,
    pub meta: Value,
    /// For a map the reader built: a vector of its keys in source order (the
    /// compiler evaluates a literal's entries in that order). Otherwise `nil`.
    pub order: Value,
}

pub fn new(m: Map_) -> Value {
    alloc(MapObj {
        hdr: ObjHeader::new(Kind::Map),
        m,
        meta: Value::NIL,
        order: Value::NIL,
    })
}

/// A map literal: `keys` is the order its entries were written in.
pub fn new_ordered(m: Map_, keys: Vec<Value>) -> Value {
    alloc(MapObj {
        hdr: ObjHeader::new(Kind::Map),
        m,
        meta: Value::NIL,
        order: vector::from_slice(&keys),
    })
}

/// A new map object with the contents of `o` (and no metadata).
pub fn copy_of(o: &MapObj) -> Value {
    alloc(MapObj {
        hdr: ObjHeader::new(Kind::Map),
        m: o.m,
        meta: Value::NIL,
        order: o.order,
    })
}

/// The entries in source order when the map came from the reader, else in the map's
/// own order.
pub fn ordered_entries(v: &Value) -> Vec<(Value, Value)> {
    let o = get(v);
    if let Some(keys) = vector::as_vec(&o.order)
        && keys.len() == o.m.len()
        && let Some(entries) = keys
            .iter()
            .map(|k| o.m.get(k).map(|x| (*k, *x)))
            .collect::<Option<Vec<_>>>()
    {
        return entries;
    }
    o.m.iter().map(|(k, x)| (*k, *x)).collect()
}

/// Associate `k` with `v`. A key equal to one already present keeps the stored key
/// (`1` stays `1` when `1N` is added), as in Clojure; only the value is replaced.
pub fn put(m: &mut Map_, k: Value, v: Value) {
    m.insert(k, v);
}

pub fn empty_map() -> Map_ {
    Map_::new()
}

/// Build from alternating key/value pairs. Later duplicates win.
pub fn from_pairs(kvs: &[Value]) -> Value {
    debug_assert!(kvs.len().is_multiple_of(2));
    let mut m = empty_map();
    for kv in kvs.chunks(2) {
        m.insert(kv[0], kv[1]);
    }
    new(m)
}

pub fn get(v: &Value) -> &MapObj {
    debug_assert!(v.is_kind(Kind::Map));
    unsafe { cast::<MapObj>(v) }
}

pub fn as_map(v: &Value) -> Option<&MapObj> {
    if v.is_kind(Kind::Map) {
        Some(get(v))
    } else {
        None
    }
}
