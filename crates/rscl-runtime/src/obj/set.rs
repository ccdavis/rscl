//! Persistent hash set backed by `imbl::HashSet` (hasher shared with map.rs).
use super::map::Hasher;
use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

pub type Set_ = imbl::GenericHashSet<Value, Hasher, imbl::shared_ptr::DefaultSharedPtr>;

#[repr(C)]
pub struct SetObj {
    pub hdr: ObjHeader,
    pub s: Set_,
    pub meta: Value,
    /// Elements in source order, for a set the reader built.
    pub order: Option<Box<Vec<Value>>>,
}

pub fn new(s: Set_) -> Value {
    alloc(SetObj {
        hdr: ObjHeader::new(Kind::Set),
        s,
        meta: Value::NIL,
        order: None,
    })
}

/// A set literal: `items` is the order its elements were written in.
pub fn new_ordered(s: Set_, items: Vec<Value>) -> Value {
    alloc(SetObj {
        hdr: ObjHeader::new(Kind::Set),
        s,
        meta: Value::NIL,
        order: Some(Box::new(items)),
    })
}

/// The elements in source order when the set came from the reader.
pub fn ordered_items(v: &Value) -> Vec<Value> {
    let o = get(v);
    match &o.order {
        Some(items) if items.len() == o.s.len() => items.to_vec(),
        _ => o.s.iter().cloned().collect(),
    }
}

/// The element of `s` equal to `k`, as stored. Only a number or a sequential
/// collection can be equal to a key without being the same kind of value (`1` and
/// `1N`, a list and a vector); for those the set is searched.
pub fn stored(s: &Set_, k: &Value) -> Option<Value> {
    if !s.contains(k) {
        return None;
    }
    if matches!(
        k.kind(),
        Some(Kind::BigInt | Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq | Kind::VecSeq)
    ) {
        return s.iter().find(|x| crate::equiv::equiv(x, k)).cloned();
    }
    Some(*k)
}

pub fn empty_set() -> Set_ {
    Set_::with_hasher(Hasher)
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
