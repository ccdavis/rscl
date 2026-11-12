//! A seq over a vector: the vector, a position in it, and what follows it.
//!
//! `seq`, `rest` and `next` of a vector cost one small object, whatever the
//! vector's size, and `count` and `nth` of the result are as cheap as on the
//! vector. Maps, sets and strings get their seq the same way, over a vector of
//! their elements made once.
//!
//! The same object is a *chunked* seq, as in Clojure: a vector of up to 32
//! elements (a chunk) followed by the rest of a sequence, usually lazy. `map`,
//! `filter` and `range` produce their results a chunk at a time when their source
//! comes in chunks, so the cost of a lazy step is paid once per 32 elements.
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
    /// What follows the vector's elements: `nil`, or any seqable value (a lazy
    /// seq is not realised until the walk gets there).
    pub more: Value,
}

/// The seq of `vec` from index `i`, which must be in range, followed by `more`.
pub fn new(vec: Value, i: usize, more: Value) -> Value {
    debug_assert!(i < vector::get(&vec).len());
    alloc(VecSeqObj {
        hdr: ObjHeader::new(Kind::VecSeq),
        vec,
        i,
        more,
    })
}

pub fn get(v: &Value) -> &VecSeqObj {
    debug_assert!(v.is_kind(Kind::VecSeq));
    unsafe { cast::<VecSeqObj>(v) }
}

impl VecSeqObj {
    pub fn items(&self) -> &vector::VecObj {
        vector::get(&self.vec)
    }
    /// Elements left in the vector.
    pub fn here(&self) -> usize {
        self.items().len() - self.i
    }
    pub fn first(&self) -> Value {
        self.items()[self.i]
    }
    /// What follows the first element: the next position in the vector, or
    /// `more` (which may be `nil`).
    pub fn rest(&self) -> Value {
        if self.here() > 1 {
            new(self.vec, self.i + 1, self.more)
        } else {
            self.more
        }
    }
    /// The elements from here to the end of the vector's current chunk.
    pub fn chunk(&self) -> &[Value] {
        self.items().chunk_from(self.i)
    }
    /// What follows the current chunk: a later position in the vector, or `more`.
    pub fn after_chunk(&self) -> Value {
        let n = self.chunk().len();
        if self.here() > n {
            new(self.vec, self.i + n, self.more)
        } else {
            self.more
        }
    }
}
