//! Persistent hash map backed by `imbl::HashMap` with a deterministic hasher, so
//! printing order is identical across processes (the JIT/AOT corpus tests rely on it).
use std::hash::{BuildHasherDefault, DefaultHasher};

use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

pub type Hasher = BuildHasherDefault<DefaultHasher>;
pub type Map_ = imbl::GenericHashMap<Value, Value, Hasher, imbl::shared_ptr::DefaultSharedPtr>;

#[repr(C)]
pub struct MapObj {
    pub hdr: ObjHeader,
    pub m: Map_,
    pub meta: Value,
}

pub fn new(m: Map_) -> Value {
    alloc(MapObj {
        hdr: ObjHeader::new(Kind::Map),
        m,
        meta: Value::NIL,
    })
}

pub fn empty_map() -> Map_ {
    Map_::with_hasher(Hasher::default())
}

/// Build from alternating key/value pairs (borrowed). Later duplicates win.
pub fn from_pairs(kvs: &[Value]) -> Value {
    debug_assert!(kvs.len().is_multiple_of(2));
    let mut m = empty_map();
    for kv in kvs.chunks(2) {
        m.insert(kv[0].clone(), kv[1].clone());
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
