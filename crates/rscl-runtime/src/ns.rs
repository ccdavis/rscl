//! Namespaces and the global var registry. Namespaces and vars are permanent.
//!
//! Resolution of an unqualified symbol looks in the namespace's own mappings and
//! then falls back to `clojure.core` (an implicit `refer`), so user code can shadow a
//! core name by defining it.
use std::collections::{HashMap, HashSet};
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
// SAFETY: stores addresses of leaked permanent objects.
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
    let ns: &'static mut NsObj = unsafe {
        &mut *crate::obj::alloc_raw(NsObj {
            hdr: ObjHeader::permanent(Kind::Namespace),
            name: name.into(),
            mappings: RwLock::new(HashMap::new()),
            aliases: RwLock::new(HashMap::new()),
        })
    };
    crate::gc::register_permanent(ns as *mut NsObj as *mut ObjHeader);
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

    /// All aliases of this namespace, sorted.
    pub fn aliases(&self) -> Vec<(String, &'static NsObj)> {
        let mut v: Vec<(String, &'static NsObj)> = self
            .aliases
            .read()
            .unwrap()
            .iter()
            .map(|(a, &p)| (a.clone(), unsafe { &*(p as *const NsObj) }))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
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

/// Find or create the var `ns/name`. Returns a permanent pointer.
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
        crate::exc::throw_new(
            crate::obj::exception::ILLEGAL_STATE,
            format!("Attempting to call unbound fn: #'{}", var.qualified()),
        );
        return Value::EXC;
    }
    root
}

/// `def`: bind the root and return the var as a value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_var_bind_root(v: *const VarObj, val: Value) -> Value {
    let var = unsafe { &*v };
    var.bind_root(val);
    var.as_value()
}

/// `set!` on a var: replaces the thread binding of a dynamic var. Returns the value
/// or `EXC`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_var_set(v: *const VarObj, val: Value) -> Value {
    let var = unsafe { &*v };
    if var.has_flag(var::FLAG_DYNAMIC) && var::set_binding(var, val) {
        return val;
    }
    let _ = val;
    crate::exc::throw_new(
        crate::obj::exception::ILLEGAL_STATE,
        format!(
            "Can't change/establish root binding of: {} with set",
            var.qualified()
        ),
    );
    Value::EXC
}

/// `def` metadata (also applies `:dynamic`/`:private` flags).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_var_set_meta(v: *const VarObj, meta: Value) {
    unsafe { (*v).set_meta(meta) }
}

// ---- loading namespaces --------------------------------------------------------------

/// A compiled-in loader (AOT binaries): runs a namespace's top-level forms. Returns
/// `nil` or `EXC`.
pub type StaticLoader = unsafe extern "C" fn() -> Value;

/// The compiler's loader (JIT sessions): compile and run a lib from source.
pub type DynamicLoader = fn(&str) -> crate::exc::RtResult<()>;

struct Loading {
    loaded: HashSet<String>,
    /// Libs being loaded (by `owner`), outermost first.
    in_progress: Vec<String>,
    /// The thread that is loading libs. Loading is serialised across threads, as
    /// Clojure's `require` is: a load switches the process's current namespace
    /// while it runs, and waiting for each other's libs could deadlock.
    owner: Option<u64>,
    static_loaders: HashMap<String, usize>,
    dynamic: Option<DynamicLoader>,
}

fn loading() -> &'static RwLock<Loading> {
    static L: OnceLock<RwLock<Loading>> = OnceLock::new();
    L.get_or_init(|| {
        RwLock::new(Loading {
            loaded: HashSet::from([CORE.to_string()]),
            in_progress: Vec::new(),
            owner: None,
            static_loaders: HashMap::new(),
            dynamic: None,
        })
    })
}

pub fn set_dynamic_loader(f: Option<DynamicLoader>) {
    loading().write().unwrap().dynamic = f;
}

pub fn register_static_loader(key: &str, f: StaticLoader) {
    loading()
        .write()
        .unwrap()
        .static_loaders
        .insert(key.to_string(), f as usize);
}

pub fn loaded_libs() -> Vec<String> {
    let mut v: Vec<String> = loading().read().unwrap().loaded.iter().cloned().collect();
    v.sort();
    v
}

/// Load a lib once. `key` is a namespace name (`foo.bar`) or `file:<path>`. Static
/// loaders (compiled in) win; otherwise the session's dynamic loader compiles it from
/// source. Cycles are reported, not followed.
pub fn load_lib(key: &str) -> crate::exc::RtResult<()> {
    load_lib_opt(key, false)
}

/// `load_lib` with `force`: run the lib again even if it was loaded before
/// (`load-file`, `:reload`).
pub fn load_lib_opt(key: &str, force: bool) -> crate::exc::RtResult<()> {
    use crate::exc::err;
    use crate::obj::exception::{ILLEGAL_STATE, RUNTIME};
    let me = crate::native::thread_id();
    let (static_loader, dynamic) = loop {
        {
            let mut l = loading().write().unwrap();
            if !force && l.loaded.contains(key) {
                return Ok(());
            }
            if l.owner.is_none_or(|t| t == me) {
                if l.in_progress.iter().any(|k| k == key) {
                    let chain = l.in_progress.join(" -> ");
                    return err(
                        ILLEGAL_STATE,
                        format!("Cyclic load dependency: {chain} -> {key}"),
                    );
                }
                l.owner = Some(me);
                l.in_progress.push(key.to_string());
                break (l.static_loaders.get(key).copied(), l.dynamic);
            }
        }
        // Another thread is loading: wait until it is done with all of its libs.
        crate::native::blocking(|| std::thread::sleep(std::time::Duration::from_millis(2)));
        crate::native::check_interrupt()?;
    };
    // Loading a lib must not change the caller's current namespace (its `ns` form
    // switches namespaces while it runs).
    let prev_ns = current();
    let result = match (static_loader, dynamic) {
        (Some(f), _) => {
            let f: StaticLoader = unsafe { std::mem::transmute::<usize, StaticLoader>(f) };
            crate::exc::check(crate::gc::enter_compiled(|| unsafe { f() })).map(drop)
        }
        (None, Some(dynamic)) => dynamic(key),
        (None, None) => err(
            RUNTIME,
            format!("Could not load {key}: no loader available in this program"),
        ),
    };
    set_current(prev_ns);
    let mut l = loading().write().unwrap();
    l.in_progress.retain(|k| k != key);
    if l.in_progress.is_empty() {
        l.owner = None;
    }
    if result.is_ok() {
        l.loaded.insert(key.to_string());
    }
    result
}

/// Mark a lib loaded without running anything (the lib that is being compiled now).
pub fn mark_loaded(key: &str) {
    loading().write().unwrap().loaded.insert(key.to_string());
}

/// `foo.bar-baz` → `foo/bar_baz.clj`
pub fn ns_to_relative_path(name: &str) -> String {
    format!("{}.clj", name.replace('.', "/").replace('-', "_"))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_register_ns_loader(name: *const u8, len: i64, f: *const u8) {
    let name = unsafe { crate::ffi::str_arg(name, len) };
    let f: StaticLoader = unsafe { std::mem::transmute::<*const u8, StaticLoader>(f) };
    register_static_loader(name, f);
}

/// `*ns*`: the current namespace as a value (permanent).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_current_ns() -> Value {
    current().as_value()
}
