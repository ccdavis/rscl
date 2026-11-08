//! Immutable UTF-8 string, with a lazily created NUL-terminated copy for the C FFI.
use std::ffi::{CString, c_char};
use std::sync::OnceLock;

use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct StrObj {
    pub hdr: ObjHeader,
    pub s: Box<str>,
    cstr: OnceLock<CString>,
}

pub fn new(s: &str) -> Value {
    alloc(StrObj {
        hdr: ObjHeader::new(Kind::String),
        s: s.into(),
        cstr: OnceLock::new(),
    })
}

pub fn from_string(s: String) -> Value {
    alloc(StrObj {
        hdr: ObjHeader::new(Kind::String),
        s: s.into_boxed_str(),
        cstr: OnceLock::new(),
    })
}

/// Borrow the string contents. Panics in debug if `v` is not a string.
pub fn get(v: &Value) -> &str {
    debug_assert!(v.is_kind(Kind::String));
    &unsafe { cast::<StrObj>(v) }.s
}

pub fn as_str(v: &Value) -> Option<&str> {
    if v.is_kind(Kind::String) {
        Some(get(v))
    } else {
        None
    }
}

/// A NUL-terminated pointer valid for as long as the string object lives. Interior
/// NULs are replaced by `?` (C cannot see past them anyway).
pub fn cstr(v: &Value) -> *const c_char {
    let obj = unsafe { cast::<StrObj>(v) };
    obj.cstr
        .get_or_init(|| CString::new(obj.s.replace('\0', "?")).expect("no interior NUL"))
        .as_ptr()
}
