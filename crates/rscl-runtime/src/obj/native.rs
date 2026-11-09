//! Opaque objects owned by a native bridge library. The runtime knows only the type
//! name (for matching and printing) and a destructor to call when the value dies.
use std::ffi::c_void;

use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct NativeObj {
    pub hdr: ObjHeader,
    pub type_name: Box<str>,
    /// Identifies the library instance whose type this is.
    pub token: u64,
    pub data: *mut c_void,
    pub drop_fn: Option<rscl_abi::DropFn>,
}

// SAFETY: the payload is managed by the library; the runtime only stores the pointer.
unsafe impl Send for NativeObj {}
unsafe impl Sync for NativeObj {}

impl Drop for NativeObj {
    fn drop(&mut self) {
        if let Some(f) = self.drop_fn
            && !self.data.is_null()
        {
            unsafe { f(self.data) }
        }
    }
}

pub fn new(
    type_name: &str,
    token: u64,
    data: *mut c_void,
    drop_fn: Option<rscl_abi::DropFn>,
) -> Value {
    alloc(NativeObj {
        hdr: ObjHeader::new(Kind::Native),
        type_name: type_name.into(),
        token,
        data,
        drop_fn,
    })
}

pub fn get(v: &Value) -> &NativeObj {
    debug_assert!(v.is_kind(Kind::Native));
    unsafe { cast::<NativeObj>(v) }
}

pub fn as_native(v: &Value) -> Option<&NativeObj> {
    if v.is_kind(Kind::Native) {
        Some(get(v))
    } else {
        None
    }
}
