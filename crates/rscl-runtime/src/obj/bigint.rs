//! Arbitrary-precision integers (`123N`), backed by `num-bigint`.
use num_bigint::BigInt;

use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct BigIntObj {
    pub hdr: ObjHeader,
    pub n: BigInt,
}

pub fn new(n: BigInt) -> Value {
    alloc(BigIntObj {
        hdr: ObjHeader::new(Kind::BigInt),
        n,
    })
}

pub fn get(v: &Value) -> &BigInt {
    debug_assert!(v.is_kind(Kind::BigInt));
    &unsafe { cast::<BigIntObj>(v) }.n
}

pub fn as_big(v: &Value) -> Option<&BigInt> {
    if v.is_kind(Kind::BigInt) {
        Some(get(v))
    } else {
        None
    }
}
