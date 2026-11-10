//! Atoms: a mutex-guarded mutable reference. `compare-and-set!` compares identity.
use std::sync::Mutex;

use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct AtomObj {
    pub hdr: ObjHeader,
    value: Mutex<Value>,
    meta: Mutex<Value>,
}

pub fn new(v: Value) -> Value {
    alloc(AtomObj {
        hdr: ObjHeader::new(Kind::Atom),
        value: Mutex::new(v),
        meta: Mutex::new(Value::NIL),
    })
}

/// Visit the values the atom refers to (the world is stopped, so the locks are
/// bypassed: a parked thread may hold one, but never in the middle of a write).
///
/// # Safety
/// `p` is a live atom and no other thread runs rscl code.
pub(crate) unsafe fn gc_trace(p: *mut AtomObj, f: &mut dyn FnMut(&Value)) {
    let o = unsafe { &mut *p };
    f(o.value.get_mut().unwrap_or_else(|e| e.into_inner()));
    f(o.meta.get_mut().unwrap_or_else(|e| e.into_inner()));
}

pub fn get(v: &Value) -> &AtomObj {
    debug_assert!(v.is_kind(Kind::Atom));
    unsafe { cast::<AtomObj>(v) }
}

pub fn as_atom(v: &Value) -> Option<&AtomObj> {
    if v.is_kind(Kind::Atom) {
        Some(get(v))
    } else {
        None
    }
}

impl AtomObj {
    pub fn deref(&self) -> Value {
        self.value.lock().unwrap().clone()
    }
    pub fn meta(&self) -> Value {
        self.meta.lock().unwrap().clone()
    }
    pub fn set_meta(&self, m: Value) {
        *self.meta.lock().unwrap() = m;
    }
    /// Store `v`, returning it (+1) like `reset!`.
    pub fn reset(&self, v: Value) -> Value {
        let r = v.clone();
        *self.value.lock().unwrap() = v;
        r
    }
    pub fn compare_and_set(&self, expected: &Value, new: Value) -> bool {
        let mut g = self.value.lock().unwrap();
        if g.identical(expected) {
            *g = new;
            true
        } else {
            false
        }
    }
}
