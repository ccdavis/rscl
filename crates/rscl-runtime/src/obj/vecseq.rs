//! A seq over a vector: the vector and a position in it.
//!
//! `seq`, `rest` and `next` of a vector cost one small object, whatever the
//! vector's size, and `count` and `nth` of the result are as cheap as on the
//! vector. Maps, sets and strings get their seq the same way, over a vector of
//! their elements made once.
use super::{Kind, alloc, cast, vector};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct VecSeqObj {
    pub hdr: ObjHeader,
    /// A vector object.
    pub vec: Value,
    /// Index of the first element; always less than the vector's length (an empty
    /// seq is `nil` or the empty list, never one of these).
    pub i: usize,
}

/// The seq of `vec` from index `i`, which must be in range.
pub fn new(vec: Value, i: usize) -> Value {
    debug_assert!(i < vector::get(&vec).v.len());
    alloc(VecSeqObj {
        hdr: ObjHeader::new(Kind::VecSeq),
        vec,
        i,
    })
}

pub fn get(v: &Value) -> &VecSeqObj {
    debug_assert!(v.is_kind(Kind::VecSeq));
    unsafe { cast::<VecSeqObj>(v) }
}

impl VecSeqObj {
    pub fn items(&self) -> &vector::Vec_ {
        &vector::get(&self.vec).v
    }
    pub fn count(&self) -> usize {
        self.items().len() - self.i
    }
    pub fn first(&self) -> Value {
        self.items()[self.i]
    }
    /// Element `n` of the seq, if there is one.
    pub fn nth(&self, n: usize) -> Option<Value> {
        self.items().get(self.i + n).copied()
    }
    /// The seq after the first element, or `None` at the end.
    pub fn next(&self) -> Option<Value> {
        (self.count() > 1).then(|| new(self.vec, self.i + 1))
    }
    pub fn to_vec(&self) -> Vec<Value> {
        self.items().iter().skip(self.i).copied().collect()
    }
}
