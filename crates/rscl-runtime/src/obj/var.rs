//! Vars: immortal, named, mutable roots. One `VarObj` exists per (namespace, name).
//! Generated code never touches the root directly; it calls `rt_var_root`, which
//! returns a +1 reference taken under the lock, so a concurrent `def` cannot free a
//! function while it is running.
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::{Kind, cast};
use crate::value::{ObjHeader, Value};

pub const FLAG_MACRO: u32 = 1;
pub const FLAG_DYNAMIC: u32 = 2;
pub const FLAG_PRIVATE: u32 = 4;

#[repr(C)]
pub struct VarObj {
    pub hdr: ObjHeader,
    /// Lock-free copy of the root for generated code, valid only when the root can
    /// never be freed (an immediate or an immortal object) and the var is not
    /// dynamic; `UNBOUND` otherwise, which sends readers to `rt_var_root`. A stale
    /// read after a `def` still sees a live (immortal) value, like Clojure's volatile
    /// root read.
    fast: AtomicU64,
    pub ns: Box<str>,
    pub name: Box<str>,
    root: Mutex<Value>,
    pub flags: AtomicU32,
    meta: Mutex<Value>,
}

/// Byte offset of `fast` (ABI for generated code).
pub const OFF_FAST: i32 = 16;
const _: () = assert!(std::mem::offset_of!(VarObj, fast) == OFF_FAST as usize);

/// Create an immortal var with no root. Only `ns.rs` should call this.
pub(crate) fn new(ns: &str, name: &str) -> *const VarObj {
    Box::into_raw(Box::new(VarObj {
        hdr: ObjHeader::immortal(Kind::Var),
        fast: AtomicU64::new(crate::value::UNBOUND),
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

thread_local! {
    /// `binding` frames: var address → value, innermost last.
    static BINDINGS: RefCell<Vec<HashMap<usize, Value>>> = const { RefCell::new(Vec::new()) };
}

/// `push-thread-bindings`: establish a frame of dynamic bindings on this thread.
pub fn push_bindings(frame: Vec<(&VarObj, Value)>) {
    let map: HashMap<usize, Value> = frame
        .into_iter()
        .map(|(v, x)| (v as *const VarObj as usize, x))
        .collect();
    BINDINGS.with(|b| b.borrow_mut().push(map));
}

/// `pop-thread-bindings`; false if there was no frame.
pub fn pop_bindings() -> bool {
    BINDINGS.with(|b| b.borrow_mut().pop().is_some())
}

fn thread_binding(var: &VarObj) -> Option<Value> {
    let key = var as *const VarObj as usize;
    BINDINGS.with(|b| {
        b.borrow()
            .iter()
            .rev()
            .find_map(|frame| frame.get(&key).cloned())
    })
}

/// `set!` on a dynamic var: replace the innermost thread binding; false if none.
pub fn set_binding(var: &VarObj, v: Value) -> bool {
    let key = var as *const VarObj as usize;
    BINDINGS.with(|b| {
        for frame in b.borrow_mut().iter_mut().rev() {
            if let Some(slot) = frame.get_mut(&key) {
                *slot = v;
                return true;
            }
        }
        false
    })
}

pub fn thread_bound(var: &VarObj) -> bool {
    thread_binding(var).is_some()
}

impl VarObj {
    /// The current value, +1: the innermost thread binding for a dynamic var, else
    /// the root (`UNBOUND` if none).
    pub fn root(&self) -> Value {
        if self.has_flag(FLAG_DYNAMIC)
            && let Some(v) = thread_binding(self)
        {
            return v;
        }
        self.root.lock().unwrap().clone()
    }
    /// Bound by a root or (for a dynamic var) by a thread binding.
    pub fn is_bound(&self) -> bool {
        !self.root().is_unbound()
    }
    /// Replace the root. The old value is released after the lock is dropped.
    pub fn bind_root(&self, v: Value) {
        let old = std::mem::replace(&mut *self.root.lock().unwrap(), v);
        self.refresh_fast();
        drop(old);
    }

    /// Recompute `fast` from the root and flags.
    fn refresh_fast(&self) {
        let root = self.root.lock().unwrap();
        let stable = !root.is_unbound()
            && !self.has_flag(FLAG_DYNAMIC)
            && root.header().is_none_or(|h| h.is_immortal());
        let bits = if stable {
            root.bits()
        } else {
            crate::value::UNBOUND
        };
        self.fast.store(bits, Ordering::Release);
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
        if f & FLAG_DYNAMIC != 0 {
            self.refresh_fast();
        }
    }
    pub fn is_macro(&self) -> bool {
        self.has_flag(FLAG_MACRO)
    }
    pub fn meta(&self) -> Value {
        self.meta.lock().unwrap().clone()
    }
    /// Replace the metadata. The `:dynamic`, `:private` and `:macro` flags follow it: a
    /// key that is absent or falsy clears the flag, so `(defn f ...)` after
    /// `(defn- f ...)` makes `f` public again.
    pub fn set_meta(&self, m: Value) {
        if let Some(map) = super::map::as_map(&m) {
            for (kw, flag) in [
                ("dynamic", FLAG_DYNAMIC),
                ("private", FLAG_PRIVATE),
                ("macro", FLAG_MACRO),
            ] {
                let on = map
                    .m
                    .get(&super::keyword::intern(None, kw))
                    .is_some_and(Value::truthy);
                self.set_flag(flag, on);
            }
        }
        *self.meta.lock().unwrap() = m;
    }
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.ns, self.name)
    }
    pub fn as_value(&self) -> Value {
        as_value(self as *const VarObj)
    }
}
