//! A cons cell whose tail may be any seqable value, including an unrealised lazy
//! seq (so it carries no count, unlike `ListObj`).
use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct ConsObj {
    pub hdr: ObjHeader,
    pub first: Value,
    /// The tail: a seq, a seqable collection, or `nil`.
    pub more: Value,
}

pub fn new(first: Value, more: Value) -> Value {
    alloc(ConsObj {
        hdr: ObjHeader::new(Kind::Cons),
        first,
        more,
    })
}

pub fn get(v: &Value) -> &ConsObj {
    debug_assert!(v.is_kind(Kind::Cons));
    unsafe { cast::<ConsObj>(v) }
}
