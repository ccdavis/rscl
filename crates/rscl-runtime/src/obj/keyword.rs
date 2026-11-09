//! Keywords are interned, so equality is pointer equality, but the table only holds
//! weak references: a keyword nobody references any more is freed and removed, so
//! `(keyword untrusted-string)` cannot grow memory without bound (Clojure uses weak
//! references for the same reason). Keywords that appear in compiled code are made
//! immortal by their unit's constant table and stay interned for the process.
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{OnceLock, RwLock};

use super::{Kind, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct KwObj {
    pub hdr: ObjHeader,
    pub ns: Option<Box<str>>,
    pub name: Box<str>,
    pub hash: u64,
}

/// Live keyword objects by name (weak: the objects own themselves) and by address,
/// so a thread that comes to free an object another thread already freed can tell
/// without touching the memory.
struct Table {
    names: HashMap<(Option<String>, String), usize>,
    live: std::collections::HashSet<usize>,
}
// SAFETY: the table stores addresses; every access retains or frees under the lock.
unsafe impl Send for Table {}
unsafe impl Sync for Table {}

fn table() -> &'static RwLock<Table> {
    static T: OnceLock<RwLock<Table>> = OnceLock::new();
    T.get_or_init(|| {
        RwLock::new(Table {
            names: HashMap::new(),
            live: std::collections::HashSet::new(),
        })
    })
}

/// Retain the keyword at `p` (+1). Done under a table lock, so `free` (which runs
/// under the write lock) sees either the old count or the incremented one.
unsafe fn retain(p: usize) -> Value {
    let h = unsafe { &*(p as *const ObjHeader) };
    h.rc.fetch_add(1, Ordering::Relaxed);
    unsafe { Value::from_raw(p as u64) }
}

/// Intern a keyword: the one live object for this name (+1).
pub fn intern(ns: Option<&str>, name: &str) -> Value {
    let key = (ns.map(str::to_owned), name.to_owned());
    if let Some(&p) = table().read().unwrap().names.get(&key) {
        return unsafe { retain(p) };
    }
    let mut w = table().write().unwrap();
    if let Some(&p) = w.names.get(&key) {
        return unsafe { retain(p) };
    }
    let hash = crate::equiv::hash_named(ns, name) ^ 0x9e37_79b9_7f4a_7c15;
    let obj = Box::new(KwObj {
        hdr: ObjHeader::new(Kind::Keyword),
        ns: ns.map(Into::into),
        name: name.into(),
        hash,
    });
    let p = Box::into_raw(obj) as usize;
    w.names.insert(key, p);
    w.live.insert(p);
    unsafe { Value::from_raw(p as u64) }
}

/// A reference count reached zero. Several threads can get here for one object (a
/// resurrection by `intern` followed by another drop), so the first one to claim it
/// under the write lock frees it; the others find it gone and never touch it. An
/// object resurrected meanwhile (count back above zero) is left alone.
pub(super) unsafe fn free(p: *mut KwObj) {
    let mut w = table().write().unwrap();
    if !w.live.contains(&(p as usize)) {
        return;
    }
    let kw = unsafe { &*p };
    if kw.hdr.rc.load(Ordering::Acquire) != 0 {
        return;
    }
    let key = (kw.ns.as_deref().map(str::to_owned), kw.name.to_string());
    if w.names.get(&key) == Some(&(p as usize)) {
        w.names.remove(&key);
    }
    w.live.remove(&(p as usize));
    drop(w);
    drop(unsafe { Box::from_raw(p) });
}

/// Number of interned keywords (for diagnostics).
pub fn interned_count() -> usize {
    table().read().unwrap().names.len()
}

/// Whether a keyword of this name is currently interned (for tests).
pub fn is_interned(ns: Option<&str>, name: &str) -> bool {
    let key = (ns.map(str::to_owned), name.to_owned());
    table().read().unwrap().names.contains_key(&key)
}

/// Intern from `ns/name` text (without the leading colon).
pub fn parse(s: &str) -> Value {
    let (ns, name) = super::symbol::split_ns(s);
    intern(ns, name)
}

pub fn get(v: &Value) -> &KwObj {
    debug_assert!(v.is_kind(Kind::Keyword));
    unsafe { cast::<KwObj>(v) }
}

pub fn as_kw(v: &Value) -> Option<&KwObj> {
    if v.is_kind(Kind::Keyword) {
        Some(get(v))
    } else {
        None
    }
}

impl KwObj {
    pub fn ns(&self) -> Option<&str> {
        self.ns.as_deref()
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn qualified(&self) -> String {
        match &self.ns {
            Some(ns) => format!("{ns}/{}", self.name),
            None => self.name.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_interning() {
        // Other tests intern keywords concurrently, so only this name is inspected.
        let (ns, name) = (Some("weak-test"), "one");
        assert!(!is_interned(ns, name));
        let a = intern(ns, name);
        let b = intern(ns, name);
        assert!(a.identical(&b));
        assert!(is_interned(ns, name));
        drop(a);
        assert!(is_interned(ns, name), "still referenced by b");
        drop(b);
        assert!(!is_interned(ns, name), "freed and forgotten");
        let c = intern(ns, name);
        let d = parse("weak-test/one");
        assert!(c.identical(&d));
        assert!(is_interned(ns, name));
    }
}
