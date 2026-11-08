//! Vars: immortal, named, mutable roots. One `VarObj` exists per (namespace, name).
//! Generated code never touches the root directly; it calls `rt_var_root`, which
//! returns a +1 reference taken under the lock, so a concurrent `def` cannot free a
//! function while it is running.
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use super::{Kind, cast};
use crate::value::{ObjHeader, Value};

pub const FLAG_MACRO: u32 = 1;
pub const FLAG_DYNAMIC: u32 = 2;
pub const FLAG_PRIVATE: u32 = 4;

#[repr(C)]
pub struct VarObj {
    pub hdr: ObjHeader,
    pub ns: Box<str>,
    pub name: Box<str>,
    root: Mutex<Value>,
    pub flags: AtomicU32,
    meta: Mutex<Value>,
}

/// Create an immortal var with no root. Only `ns.rs` should call this.
pub(crate) fn new(ns: &str, name: &str) -> *const VarObj {
    Box::into_raw(Box::new(VarObj {
        hdr: ObjHeader::immortal(Kind::Var),
        ns: ns.into(),
        name: name.into(),
        root: Mutex::new(Value::UNBOUND),
        flags: AtomicU32::new(0),
        meta: Mutex::new(Value::NIL),
    }))
}

pub fn as_value(p: *const VarObj) -> Value {
    unsafe { Value::from_raw(p as u64) }
}

pub fn get(v: &Value) -> &VarObj {
    debug_assert!(v.is_kind(Kind::Var));
    unsafe { cast::<VarObj>(v) }
}

pub fn as_var(v: &Value) -> Option<&VarObj> {
    if v.is_kind(Kind::Var) {
        Some(get(v))
    } else {
        None
    }
}

impl VarObj {
    /// The root value, +1, or `UNBOUND`.
    pub fn root(&self) -> Value {
        self.root.lock().unwrap().clone()
    }
    pub fn is_bound(&self) -> bool {
        !self.root.lock().unwrap().is_unbound()
    }
    /// Replace the root. The old value is released after the lock is dropped.
    pub fn bind_root(&self, v: Value) {
        let old = std::mem::replace(&mut *self.root.lock().unwrap(), v);
        drop(old);
    }
    pub fn unbind(&self) {
        self.bind_root(Value::UNBOUND);
    }
    pub fn has_flag(&self, f: u32) -> bool {
        self.flags.load(Ordering::Acquire) & f != 0
    }
    pub fn set_flag(&self, f: u32, on: bool) {
        if on {
            self.flags.fetch_or(f, Ordering::AcqRel);
        } else {
            self.flags.fetch_and(!f, Ordering::AcqRel);
        }
    }
    pub fn is_macro(&self) -> bool {
        self.has_flag(FLAG_MACRO)
    }
    pub fn meta(&self) -> Value {
        self.meta.lock().unwrap().clone()
    }
    pub fn set_meta(&self, m: Value) {
        *self.meta.lock().unwrap() = m;
    }
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.ns, self.name)
    }
    pub fn as_value(&self) -> Value {
        as_value(self as *const VarObj)
    }
}
