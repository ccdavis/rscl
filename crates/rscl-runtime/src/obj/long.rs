//! Boxed 64-bit integer for values outside the fixnum range.
use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct LongObj {
    pub hdr: ObjHeader,
    pub n: i64,
}

pub fn new(n: i64) -> Value {
    alloc(LongObj {
        hdr: ObjHeader::new(Kind::Long),
        n,
    })
}

pub fn get(v: &Value) -> i64 {
    debug_assert!(v.is_kind(Kind::Long));
    unsafe { cast::<LongObj>(v) }.n
}
