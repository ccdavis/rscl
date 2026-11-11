//! Opaque objects owned by a native bridge library. The runtime knows only the type
//! name (for matching and printing) and a destructor to call when the value dies:
//! some time after the last reference is gone, when a collection finds the object
//! unreachable (a full collection, if the object has grown old). The destructor
//! runs after that collection, on the thread that collected, with no collector
//! lock held. A library whose objects hold scarce resources (files, sockets)
//! should offer an explicit close as well.
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

// A dead native object must be offered to the collector's finaliser (the library's
// destructor), which only happens for objects with something to drop (see
// `arena.rs`).
const _: () = assert!(std::mem::needs_drop::<NativeObj>());

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

/// Take the destructor out of a native object that is about to be freed, so the
/// collector can run it after the collection instead of inside it.
///
/// # Safety
/// `p` must point at a native object nothing refers to any more.
pub(crate) unsafe fn take_destructor(p: *mut NativeObj) -> Option<(rscl_abi::DropFn, *mut c_void)> {
    let o = unsafe { &mut *p };
    let f = o.drop_fn.take()?;
    if o.data.is_null() {
        return None;
    }
    Some((f, std::mem::replace(&mut o.data, std::ptr::null_mut())))
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
