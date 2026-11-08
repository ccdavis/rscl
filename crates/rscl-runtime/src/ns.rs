//! Namespaces and the global var registry. Namespaces and vars are immortal.
//!
//! Resolution of an unqualified symbol looks in the namespace's own mappings and
//! then falls back to `clojure.core` (an implicit `refer`), so user code can shadow a
//! core name by defining it.
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, RwLock};

use crate::obj::var::{self, VarObj};
use crate::obj::{Kind, cast};
use crate::value::{ObjHeader, Value};

pub const CORE: &str = "clojure.core";
pub const USER: &str = "user";

#[repr(C)]
pub struct NsObj {
    pub hdr: ObjHeader,
    pub name: Box<str>,
    mappings: RwLock<HashMap<String, usize>>,
    aliases: RwLock<HashMap<String, usize>>,
}

struct Registry(HashMap<String, usize>);
// SAFETY: stores addresses of leaked immortal objects.
unsafe impl Send for Registry {}
unsafe impl Sync for Registry {}

fn registry() -> &'static RwLock<Registry> {
    static R: OnceLock<RwLock<Registry>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(Registry(HashMap::new())))
}

static CURRENT: AtomicUsize = AtomicUsize::new(0);

pub fn find(name: &str) -> Option<&'static NsObj> {
    registry()
        .read()
        .unwrap()
        .0
        .get(name)
        .map(|&p| unsafe { &*(p as *const NsObj) })
}

pub fn find_or_create(name: &str) -> &'static NsObj {
    if let Some(ns) = find(name) {
        return ns;
    }
    let mut w = registry().write().unwrap();
    if let Some(&p) = w.0.get(name) {
        return unsafe { &*(p as *const NsObj) };
    }
    let ns = Box::leak(Box::new(NsObj {
        hdr: ObjHeader::immortal(Kind::Namespace),
        name: name.into(),
        mappings: RwLock::new(HashMap::new()),
        aliases: RwLock::new(HashMap::new()),
    }));
    w.0.insert(name.to_string(), ns as *const NsObj as usize);
    ns
}

pub fn all() -> Vec<&'static NsObj> {
    registry()
        .read()
        .unwrap()
        .0
        .values()
        .map(|&p| unsafe { &*(p as *const NsObj) })
        .collect()
}

pub fn core() -> &'static NsObj {
    find_or_create(CORE)
}

/// The current namespace (`*ns*`), defaulting to `user`.
pub fn current() -> &'static NsObj {
    let p = CURRENT.load(Ordering::Acquire);
    if p == 0 {
        find_or_create(USER)
    } else {
        unsafe { &*(p as *const NsObj) }
    }
}

pub fn set_current(ns: &'static NsObj) {
    CURRENT.store(ns as *const NsObj as usize, Ordering::Release);
}

pub fn get(v: &Value) -> &NsObj {
    debug_assert!(v.is_kind(Kind::Namespace));
    unsafe { cast::<NsObj>(v) }
}

impl NsObj {
    pub fn as_value(&self) -> Value {
        unsafe { Value::from_raw(self as *const NsObj as u64) }
    }

    /// The var mapped to `name` in this namespace (own or referred).
    pub fn lookup(&self, name: &str) -> Option<&'static VarObj> {
        self.mappings
            .read()
            .unwrap()
            .get(name)
            .map(|&p| unsafe { &*(p as *const VarObj) })
    }

    /// Find or create this namespace's own var `name`. A referred var of the same
    /// name is replaced by a fresh local var (Clojure warns; we silently shadow).
    pub fn intern(&self, name: &str) -> &'static VarObj {
        if let Some(v) = self.lookup(name)
            && *v.ns == *self.name
        {
            return v;
        }
        let mut w = self.mappings.write().unwrap();
        if let Some(&p) = w.get(name) {
            let v = unsafe { &*(p as *const VarObj) };
            if *v.ns == *self.name {
                return v;
            }
        }
        let v = unsafe { &*var::new(&self.name, name) };
        w.insert(name.to_string(), v as *const VarObj as usize);
        v
    }

    /// Map `name` to a var owned by another namespace (`refer`).
    pub fn refer(&self, name: &str, v: &'static VarObj) {
        self.mappings
            .write()
            .unwrap()
            .insert(name.to_string(), v as *const VarObj as usize);
    }

    pub fn alias(&self, alias: &str, target: &'static NsObj) {
        self.aliases
            .write()
            .unwrap()
            .insert(alias.to_string(), target as *const NsObj as usize);
    }

    pub fn resolve_alias(&self, alias: &str) -> Option<&'static NsObj> {
        self.aliases
            .read()
            .unwrap()
            .get(alias)
            .map(|&p| unsafe { &*(p as *const NsObj) })
    }

    /// Vars interned in this namespace (not referred ones), sorted by name.
    pub fn own_vars(&self) -> Vec<&'static VarObj> {
        let mut v: Vec<&'static VarObj> = self
            .mappings
            .read()
            .unwrap()
            .values()
            .map(|&p| unsafe { &*(p as *const VarObj) })
            .filter(|v| *v.ns == *self.name)
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

/// Resolve a symbol relative to `ns`: `alias/name` or `full.ns/name` for qualified
/// symbols; own mappings then `clojure.core` for unqualified ones.
pub fn resolve(ns: &NsObj, sym_ns: Option<&str>, name: &str) -> Option<&'static VarObj> {
    match sym_ns {
        Some(q) => {
            let target = ns.resolve_alias(q).or_else(|| find(q))?;
            target.lookup(name).filter(|v| *v.ns == *target.name)
        }
        None => ns.lookup(name).or_else(|| {
            if &*ns.name != CORE {
                core().lookup(name)
            } else {
                None
            }
        }),
    }
}

// ---- FFI ----------------------------------------------------------------------------

/// Find or create the var `ns/name`. Returns an immortal pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_intern_var(
    ns: *const u8,
    ns_len: i64,
    name: *const u8,
    name_len: i64,
) -> *const VarObj {
    let (ns, name) = unsafe {
        (
            crate::ffi::str_arg(ns, ns_len),
            crate::ffi::str_arg(name, name_len),
        )
    };
    find_or_create(ns).intern(name) as *const VarObj
}

/// The var's root, +1. Throws `IllegalStateException` (returning `EXC`) when unbound.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_var_root(v: *const VarObj) -> Value {
    let var = unsafe { &*v };
    let root = var.root();
    if root.is_unbound() {
        std::mem::forget(root);
        crate::exc::throw_new(
            crate::obj::exception::ILLEGAL_STATE,
            format!("Attempting to call unbound fn: #'{}", var.qualified()),
        );
        return Value::EXC;
    }
    root
}

/// `def`: bind the root (ownership of `val` transferred) and return the var as a value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_var_bind_root(v: *const VarObj, val: Value) -> Value {
    let var = unsafe { &*v };
    var.bind_root(val);
    var.as_value()
}

/// Set or clear a var flag (`FLAG_MACRO`, ...).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_var_set_flag(v: *const VarObj, flag: i64, on: i64) {
    unsafe { (*v).set_flag(flag as u32, on != 0) }
}
