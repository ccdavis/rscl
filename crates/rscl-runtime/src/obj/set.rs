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
}

pub fn new(s: Set_) -> Value {
    alloc(SetObj {
        hdr: ObjHeader::new(Kind::Set),
        s,
        meta: Value::NIL,
    })
}

pub fn empty_set() -> Set_ {
    Set_::with_hasher(Hasher)
}

pub fn from_slice(items: &[Value]) -> Value {
    let mut s = empty_set();
    for x in items {
        s.insert(x.clone());
    }
    new(s)
}

pub fn get(v: &Value) -> &SetObj {
    debug_assert!(v.is_kind(Kind::Set));
    unsafe { cast::<SetObj>(v) }
}

pub fn as_set(v: &Value) -> Option<&SetObj> {
    if v.is_kind(Kind::Set) {
        Some(get(v))
    } else {
        None
    }
}
