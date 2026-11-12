//! Immutable UTF-8 string.
//!
//! The bytes of a string of ordinary size are part of the string object, after
//! its header: making one is a single allocation in the object heap, and a dead
//! one is released like a cons cell, without being looked at. A long string keeps
//! its bytes in a separate allocation, which is freed when the object dies. Either
//! way the bytes are followed by a NUL, for the C FFI.
use std::cell::Cell;
use std::ffi::c_char;

use super::{Kind, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct StrObj {
    pub hdr: ObjHeader,
    len: usize,
    /// The bytes: right after this object, or (a long string) on their own.
    bytes: *const u8,
}

const INLINE_OFFSET: usize = std::mem::size_of::<StrObj>();
/// Longest string kept in the object (the object then fills a 4 KB slot).
const MAX_INLINE: usize = 4096 - INLINE_OFFSET - 1;

impl StrObj {
    fn is_inline(&self) -> bool {
        self.bytes as usize == self as *const StrObj as usize + INLINE_OFFSET
    }
}

pub fn new(s: &str) -> Value {
    let len = s.len();
    unsafe {
        let (p, bytes) = if len <= MAX_INLINE {
            let p = crate::arena::alloc(INLINE_OFFSET + len + 1, false) as *mut StrObj;
            (p, (p as *mut u8).add(INLINE_OFFSET))
        } else {
            // (A drop block: `free` is called on the object when it dies.)
            let p = crate::arena::alloc(INLINE_OFFSET, true) as *mut StrObj;
            let buf = vec![0u8; len + 1].into_boxed_slice();
            (p, Box::into_raw(buf) as *mut u8)
        };
        std::ptr::copy_nonoverlapping(s.as_ptr(), bytes, len);
        *bytes.add(len) = 0;
        std::ptr::write(
            p,
            StrObj {
                hdr: ObjHeader::new(Kind::String),
                len,
                bytes,
            },
        );
        Value::from_obj(p)
    }
}

pub fn from_string(s: String) -> Value {
    new(&s)
}

thread_local! {
    /// The buffer `build` writes into, kept from one call to the next.
    static SCRATCH: Cell<String> = const { Cell::new(String::new()) };
}

/// Make a string by writing it into a buffer that is reused from call to call
/// (the common case of `str` then touches no allocator but the object heap's).
pub fn build(write: impl FnOnce(&mut String)) -> Value {
    // Taken out for the duration: `write` may print values, which can run rscl
    // code that builds strings of its own.
    let mut buf = SCRATCH.with(Cell::take);
    buf.clear();
    write(&mut buf);
    let v = new(&buf);
    if buf.capacity() <= 16 << 10 {
        SCRATCH.with(|s| s.set(buf));
    }
    v
}

/// Release what a dead string owns outside the object heap.
///
/// # Safety
/// `p` must point at a string object nothing refers to any more.
pub(crate) unsafe fn free(p: *mut StrObj) {
    unsafe {
        if !(*p).is_inline() {
            let bytes = std::ptr::slice_from_raw_parts_mut((*p).bytes as *mut u8, (*p).len + 1);
            drop(Box::from_raw(bytes));
        }
    }
}

/// Borrow the string contents. Panics in debug if `v` is not a string.
pub fn get(v: &Value) -> &str {
    debug_assert!(v.is_kind(Kind::String));
    let o = unsafe { cast::<StrObj>(v) };
    // SAFETY: the bytes were copied from a `str` and never change.
    unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(o.bytes, o.len)) }
}

pub fn as_str(v: &Value) -> Option<&str> {
    if v.is_kind(Kind::String) {
        Some(get(v))
    } else {
        None
    }
}

/// A NUL-terminated pointer valid for as long as the string object lives. (C sees
/// the string up to its first NUL, if it contains one.)
pub fn cstr(v: &Value) -> *const c_char {
    unsafe { cast::<StrObj>(v) }.bytes as *const c_char
}
