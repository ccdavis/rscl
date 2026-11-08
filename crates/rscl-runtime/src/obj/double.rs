//! Boxed f64.
use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct DoubleObj {
    pub hdr: ObjHeader,
    pub d: f64,
}

pub fn new(d: f64) -> Value {
    alloc(DoubleObj {
        hdr: ObjHeader::new(Kind::Double),
        d,
    })
}

pub fn get(v: &Value) -> f64 {
    debug_assert!(v.is_kind(Kind::Double));
    unsafe { cast::<DoubleObj>(v) }.d
}
