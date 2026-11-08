//! Persistent vector backed by `imbl::Vector`.
use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

pub type Vec_ = imbl::Vector<Value>;

#[repr(C)]
pub struct VecObj {
    pub hdr: ObjHeader,
    pub v: Vec_,
    pub meta: Value,
}

pub fn new(v: Vec_) -> Value {
    alloc(VecObj {
        hdr: ObjHeader::new(Kind::Vector),
        v,
        meta: Value::NIL,
    })
}

pub fn from_slice(items: &[Value]) -> Value {
    new(items.iter().cloned().collect())
}

pub fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Value {
    new(iter.into_iter().collect())
}

pub fn get(v: &Value) -> &VecObj {
    debug_assert!(v.is_kind(Kind::Vector));
    unsafe { cast::<VecObj>(v) }
}

pub fn as_vec(v: &Value) -> Option<&VecObj> {
    if v.is_kind(Kind::Vector) {
        Some(get(v))
    } else {
        None
    }
}
