//! Function objects and their immortal prototypes.
//!
//! Layout is ABI: generated code reads `proto` at `OFF_PROTO`, `entry` at
//! `PROTO_OFF_ENTRY` inside the proto, and capture `i` at `OFF_CAPTURES + 8*i`.
//! A `FnObj` is a single allocation with the captures stored inline after the header.
use std::alloc::Layout;

use super::{Kind, cast};
use crate::value::{ObjHeader, Value};

/// The uniform calling convention every rscl function and every builtin uses.
/// `owned` is non-zero when the argument slots belong to the callee for the
/// duration of the call: the caller will not read them again, so the callee may
/// overwrite them (a native that walks a sequence uses its slot as the cursor, see
/// `core::Taker`). Compiled code always passes non-zero; a native that calls with a
/// slice it still uses passes 0. The collector does not treat a compiled caller's
/// argument slots as roots: the callee keeps its arguments reachable (a compiled fn
/// loads them into its own variables before its first safepoint; a native under
/// which a collection may run roots its argument list, see `gc::ArgRoot`).
pub type Entry =
    unsafe extern "C" fn(f: *const FnObj, argc: i64, argv: *const Value, owned: u64) -> Value;

#[repr(C)]
pub struct FnProto {
    pub entry: Entry,
    /// Bit `i` set: the function accepts exactly `i` positional arguments (i < 64).
    pub arity_mask: u64,
    /// Minimum argument count of the variadic arity, or -1 when there is none.
    pub variadic_min: i64,
    pub name: Box<str>,
    pub meta: Value,
}

#[repr(C)]
pub struct FnObj {
    pub hdr: ObjHeader,
    pub proto: *const FnProto,
    pub ncaptures: i64,
    // followed by `ncaptures` inline `Value`s
}

pub const OFF_PROTO: i32 = 16;
pub const OFF_NCAPTURES: i32 = 24;
pub const OFF_CAPTURES: i32 = 32;
pub const PROTO_OFF_ENTRY: i32 = 0;

const _: () = assert!(std::mem::size_of::<FnObj>() == OFF_CAPTURES as usize);

fn layout(ncaptures: usize) -> Layout {
    Layout::from_size_align(OFF_CAPTURES as usize + 8 * ncaptures, 8).unwrap()
}

/// Create an immortal prototype. `name` is the qualified name used for printing and
/// error messages.
pub fn new_proto(entry: Entry, name: &str, arity_mask: u64, variadic_min: i64) -> *const FnProto {
    let p = Box::into_raw(Box::new(FnProto {
        entry,
        arity_mask,
        variadic_min,
        name: name.into(),
        meta: Value::NIL,
    }));
    crate::gc::register_proto(p);
    p
}

/// Allocate a function object with `ncaptures` capture slots, all `nil`.
pub fn new(proto: *const FnProto, ncaptures: usize) -> Value {
    unsafe {
        let p = super::raw_alloc(layout(ncaptures));
        std::ptr::write_bytes(p, 0, layout(ncaptures).size());
        let p = p as *mut FnObj;
        std::ptr::write(&mut (*p).hdr, ObjHeader::new(Kind::Fn));
        (*p).proto = proto;
        (*p).ncaptures = ncaptures as i64;
        Value::from_obj(p)
    }
}

/// Store a capture (takes ownership of `v`). Only valid before the fn is shared.
pub unsafe fn set_capture(f: &Value, i: usize, v: Value) {
    unsafe {
        // No write barrier: the fn object was allocated a moment ago and is young.
        *capture_ptr(f, i) = v;
    }
}

pub fn capture(f: &Value, i: usize) -> &Value {
    unsafe { &*capture_ptr(f, i) }
}

unsafe fn capture_ptr(f: &Value, i: usize) -> *mut Value {
    debug_assert!(f.is_kind(Kind::Fn));
    debug_assert!((i as i64) < get(f).ncaptures);
    unsafe { capture_ptr_raw(f.ptr() as *const FnObj, i) }
}

/// Address of capture `i` of a fn object (for the collector).
///
/// # Safety
/// `f` must be a live fn object with more than `i` captures.
pub(crate) unsafe fn capture_ptr_raw(f: *const FnObj, i: usize) -> *mut Value {
    unsafe {
        (f as *mut u8)
            .add(OFF_CAPTURES as usize)
            .cast::<Value>()
            .add(i)
    }
}

pub fn get(v: &Value) -> &FnObj {
    debug_assert!(v.is_kind(Kind::Fn));
    unsafe { cast::<FnObj>(v) }
}

pub fn as_fn(v: &Value) -> Option<&FnObj> {
    if v.is_kind(Kind::Fn) {
        Some(get(v))
    } else {
        None
    }
}

impl FnObj {
    pub fn proto(&self) -> &FnProto {
        unsafe { &*self.proto }
    }
}

impl FnProto {
    pub fn accepts(&self, argc: i64) -> bool {
        (argc < 64 && self.arity_mask & (1u64 << argc) != 0)
            || (self.variadic_min >= 0 && argc >= self.variadic_min)
    }
    pub fn is_variadic(&self) -> bool {
        self.variadic_min >= 0
    }
}
