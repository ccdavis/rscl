//! Atoms: a mutex-guarded mutable reference. `compare-and-set!` compares identity.
use std::sync::Mutex;

use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct AtomObj {
    pub hdr: ObjHeader,
    value: Mutex<Value>,
    meta: Mutex<Value>,
    /// Cycle-collector bookkeeping (see `gc.rs`): buffered as a candidate, and a
    /// counter of writes and lost references.
    pub buffered: std::sync::atomic::AtomicBool,
    pub version: std::sync::atomic::AtomicU32,
}

pub fn new(v: Value) -> Value {
    alloc(AtomObj {
        hdr: ObjHeader::new(Kind::Atom),
        value: Mutex::new(v),
        meta: Mutex::new(Value::NIL),
        buffered: std::sync::atomic::AtomicBool::new(false),
        version: std::sync::atomic::AtomicU32::new(0),
    })
}

/// A write may close a reference cycle through the atom: make it a candidate.
fn noted(v: &Value) {
    crate::gc::note_write(v);
}

/// Slots the collector walks (the world is stopped).
///
/// # Safety
/// `p` is a live atom and no other thread runs.
pub(crate) unsafe fn gc_slots(p: *mut AtomObj) -> Vec<*const Value> {
    let o = unsafe { &mut *p };
    vec![
        o.value.get_mut().unwrap_or_else(|e| e.into_inner()) as *const Value,
        o.meta.get_mut().unwrap_or_else(|e| e.into_inner()) as *const Value,
    ]
}

/// Drop every reference a garbage atom holds.
///
/// # Safety
/// `p` is an unreachable atom and no other thread runs.
pub(crate) unsafe fn gc_unlink(p: *mut AtomObj) {
    let o = unsafe { &mut *p };
    *o.value.get_mut().unwrap_or_else(|e| e.into_inner()) = Value::NIL;
    *o.meta.get_mut().unwrap_or_else(|e| e.into_inner()) = Value::NIL;
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
        let old = std::mem::replace(&mut *self.meta.lock().unwrap(), m);
        drop(old);
    }
    /// Store `v`, returning it (+1) like `reset!`.
    pub fn reset(&self, v: Value) -> Value {
        let r = v.clone();
        let old = std::mem::replace(&mut *self.value.lock().unwrap(), v);
        drop(old);
        noted(&self.as_value());
        r
    }
    pub fn compare_and_set(&self, expected: &Value, new: Value) -> bool {
        let mut g = self.value.lock().unwrap();
        if g.identical(expected) {
            let old = std::mem::replace(&mut *g, new);
            drop(g);
            drop(old);
            noted(&self.as_value());
            true
        } else {
            false
        }
    }
    /// A borrowed view of the atom as a value (for the collector's bookkeeping).
    fn as_value(&self) -> std::mem::ManuallyDrop<Value> {
        std::mem::ManuallyDrop::new(unsafe { Value::from_raw(self as *const AtomObj as u64) })
    }
}
