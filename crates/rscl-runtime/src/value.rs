//! The tagged 64-bit value word and the heap object header.
//!
//! Encoding (locked, see docs/SEMANTICS.md):
//!
//! | word                | meaning                                              |
//! |---------------------|------------------------------------------------------|
//! | `...xxx1`           | fixnum, 63-bit, `(n << 1) | 1`                       |
//! | `...x00`, non-zero  | pointer to a heap object whose first field is `ObjHeader` |
//! | `0`                 | `nil`                                                |
//! | `...10`             | immediate: bits 2–7 sub-tag, bits 8–63 payload       |
//! | `0x002` / `0x102`   | `false` / `true`                                     |
//! | `0x202` / `0x302`   | `EXC` (exception pending) / `UNBOUND` (var has no root) |
//! | `0x06 | cp << 8`    | char                                                 |
//!
//! Ownership: a `Value` is an owned (+1) reference; dropping it releases. `VRef` is a
//! borrowed view with the same bit pattern and no `Drop`, used for FFI parameters.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::atomic::{AtomicI64, Ordering, fence};

use crate::obj;
use crate::obj::Kind;

pub const NIL: u64 = 0;
pub const FALSE: u64 = 0x002;
pub const TRUE: u64 = 0x102;
pub const EXC: u64 = 0x202;
pub const UNBOUND: u64 = 0x302;
const CHAR_TAG: u64 = 0x06;
const IMM_PAYLOAD_SHIFT: u32 = 8;

pub const FIXNUM_MIN: i64 = -(1 << 62);
pub const FIXNUM_MAX: i64 = (1 << 62) - 1;

/// Refcount value of objects that are never freed. Retain/release still run their
/// atomic ops on it; it can never reach the "last reference" state.
pub const IMMORTAL: i64 = i64::MIN / 2;

/// Header shared by every heap object. `pos` is a source-position id (see `pos.rs`),
/// zero when unknown.
#[repr(C)]
pub struct ObjHeader {
    pub rc: AtomicI64,
    pub kind: Kind,
    pub pos: u32,
}

impl ObjHeader {
    pub fn new(kind: Kind) -> Self {
        #[cfg(feature = "live-count")]
        if counted(kind) {
            LIVE_OBJECTS.fetch_add(1, Ordering::Relaxed);
        }
        crate::gc::note_alloc();
        ObjHeader {
            rc: AtomicI64::new(1),
            kind,
            pos: 0,
        }
    }
    pub fn immortal(kind: Kind) -> Self {
        ObjHeader {
            rc: AtomicI64::new(IMMORTAL),
            kind,
            pos: 0,
        }
    }
    pub fn is_immortal(&self) -> bool {
        self.rc.load(Ordering::Relaxed) < 0
    }
}

#[cfg(feature = "live-count")]
pub static LIVE_OBJECTS: AtomicI64 = AtomicI64::new(0);

/// Keywords are interned (weakly) and may be promoted to immortal by compiled code
/// that mentions them; they are not part of the live-object count that the leak
/// tests compare, which tracks ordinary allocations.
#[inline]
#[cfg_attr(not(feature = "live-count"), allow(dead_code))]
pub(crate) fn counted(kind: Kind) -> bool {
    kind != Kind::Keyword
}

/// Bookkeeping hook called whenever a counted heap object is freed or made immortal.
#[inline]
pub(crate) fn note_freed(kind: Kind) {
    #[cfg(feature = "live-count")]
    if counted(kind) {
        LIVE_OBJECTS.fetch_sub(1, Ordering::Relaxed);
    }
    #[cfg(not(feature = "live-count"))]
    let _ = kind;
}

/// Every allocation that is deliberately kept alive forever (immortal objects, fn
/// prototypes). Leak checkers then see them as reachable even after JIT memory, which
/// holds the constant tables, has been unmapped.
static IMMORTALS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

pub fn register_immortal(addr: usize) {
    IMMORTALS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(addr);
}

/// Number of live, non-immortal heap objects (only with the `live-count` feature;
/// always 0 otherwise). Tests assert this returns to a baseline.
pub fn live_objects() -> i64 {
    #[cfg(feature = "live-count")]
    {
        LIVE_OBJECTS.load(Ordering::SeqCst)
    }
    #[cfg(not(feature = "live-count"))]
    {
        0
    }
}

/// An owned reference to an rscl value.
#[repr(transparent)]
pub struct Value(u64);

/// A borrowed reference to an rscl value: same bits, no `Drop`.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct VRef(u64);

// Heap objects are either immutable or internally synchronised; refcounts are atomic.
unsafe impl Send for Value {}
unsafe impl Sync for Value {}
unsafe impl Send for VRef {}
unsafe impl Sync for VRef {}

impl Deref for VRef {
    type Target = Value;
    #[inline]
    fn deref(&self) -> &Value {
        // SAFETY: identical repr(transparent) layout; the borrow lifetime bounds use.
        unsafe { &*(self as *const VRef as *const Value) }
    }
}

impl VRef {
    #[inline]
    pub const fn from_raw(bits: u64) -> VRef {
        VRef(bits)
    }
    #[inline]
    pub fn bits(self) -> u64 {
        self.0
    }
    /// Take an owned copy (+1).
    #[inline]
    pub fn owned(self) -> Value {
        (*self).clone()
    }
}

impl Value {
    pub const NIL: Value = Value(NIL);
    pub const TRUE: Value = Value(TRUE);
    pub const FALSE: Value = Value(FALSE);
    pub const EXC: Value = Value(EXC);
    pub const UNBOUND: Value = Value(UNBOUND);

    // ---- construction ------------------------------------------------------------

    /// Wrap raw bits as an owned value. The caller transfers one reference.
    #[inline]
    pub const unsafe fn from_raw(bits: u64) -> Value {
        Value(bits)
    }
    /// Give up ownership and return the raw bits (no release).
    #[inline]
    pub fn into_raw(self) -> u64 {
        let bits = self.0;
        std::mem::forget(self);
        bits
    }
    #[inline]
    pub fn bits(&self) -> u64 {
        self.0
    }
    #[inline]
    pub fn as_ref(&self) -> VRef {
        VRef(self.0)
    }
    #[inline]
    pub fn bool(b: bool) -> Value {
        Value(if b { TRUE } else { FALSE })
    }
    #[inline]
    pub fn char(c: char) -> Value {
        Value(CHAR_TAG | ((c as u64) << IMM_PAYLOAD_SHIFT))
    }
    /// A 64-bit integer: a fixnum when it fits, otherwise a boxed `Long`.
    #[inline]
    pub fn int(n: i64) -> Value {
        if (FIXNUM_MIN..=FIXNUM_MAX).contains(&n) {
            Value(((n as u64) << 1) | 1)
        } else {
            obj::long::new(n)
        }
    }
    #[inline]
    pub fn double(d: f64) -> Value {
        obj::double::new(d)
    }
    /// Wrap a freshly allocated heap object (refcount already 1).
    #[inline]
    pub(crate) fn from_obj<T>(p: *mut T) -> Value {
        debug_assert!(!p.is_null() && (p as u64) & 0b11 == 0);
        Value(p as u64)
    }

    // ---- classification ----------------------------------------------------------

    #[inline]
    pub fn is_nil(&self) -> bool {
        self.0 == NIL
    }
    #[inline]
    pub fn is_fixnum(&self) -> bool {
        self.0 & 1 == 1
    }
    #[inline]
    pub fn is_ptr(&self) -> bool {
        self.0 & 0b11 == 0 && self.0 != 0
    }
    #[inline]
    pub fn is_immediate(&self) -> bool {
        self.0 & 0b11 == 0b10
    }
    #[inline]
    pub fn is_exc(&self) -> bool {
        self.0 == EXC
    }
    #[inline]
    pub fn is_unbound(&self) -> bool {
        self.0 == UNBOUND
    }
    #[inline]
    pub fn is_true(&self) -> bool {
        self.0 == TRUE
    }
    #[inline]
    pub fn is_false(&self) -> bool {
        self.0 == FALSE
    }
    /// Clojure truthiness: everything except `nil` and `false`.
    #[inline]
    pub fn truthy(&self) -> bool {
        (self.0 | 2) != 2
    }
    #[inline]
    pub fn as_fixnum(&self) -> Option<i64> {
        if self.is_fixnum() {
            Some((self.0 as i64) >> 1)
        } else {
            None
        }
    }
    #[inline]
    pub fn as_char(&self) -> Option<char> {
        if self.0 & 0xff == CHAR_TAG {
            char::from_u32((self.0 >> IMM_PAYLOAD_SHIFT) as u32)
        } else {
            None
        }
    }
    #[inline]
    pub fn as_bool(&self) -> Option<bool> {
        match self.0 {
            TRUE => Some(true),
            FALSE => Some(false),
            _ => None,
        }
    }
    /// Header of a heap object, if this is a pointer.
    #[inline]
    pub fn header(&self) -> Option<&ObjHeader> {
        if self.is_ptr() {
            // SAFETY: every heap value points at an ObjHeader-prefixed object that is
            // alive for as long as this reference exists.
            Some(unsafe { &*(self.0 as *const ObjHeader) })
        } else {
            None
        }
    }
    #[inline]
    pub fn kind(&self) -> Option<Kind> {
        self.header().map(|h| h.kind)
    }
    #[inline]
    pub fn is_kind(&self, k: Kind) -> bool {
        self.kind() == Some(k)
    }
    /// Raw pointer to the object (for `unsafe` casts by the `obj` modules).
    #[inline]
    pub fn ptr(&self) -> *mut u8 {
        self.0 as *mut u8
    }
    /// Pointer identity (also used for `identical?`).
    #[inline]
    pub fn identical(&self, other: &Value) -> bool {
        self.0 == other.0
    }

    /// Integer value of a fixnum or boxed long (not of a `BigInt`: see
    /// `equiv::int_value` for equality and hashing across the two).
    #[inline]
    pub fn as_int(&self) -> Option<i64> {
        if let Some(n) = self.as_fixnum() {
            Some(n)
        } else if self.is_kind(Kind::Long) {
            Some(obj::long::get(self))
        } else {
            None
        }
    }
    /// The 64-bit integer this value stands for: a long, or a `BigInt` that fits one
    /// (indexes, counts and `=`/`hash` accept either).
    pub fn int_value(&self) -> Option<i64> {
        self.as_int()
            .or_else(|| obj::bigint::as_big(self).and_then(num_traits::ToPrimitive::to_i64))
    }
    #[inline]
    pub fn as_double(&self) -> Option<f64> {
        if self.is_kind(Kind::Double) {
            Some(obj::double::get(self))
        } else {
            None
        }
    }

    // ---- refcounting -------------------------------------------------------------

    /// +1 on a heap object; no-op for immediates. The result is the same bits, owned.
    #[inline]
    pub fn retain(&self) -> Value {
        if let Some(h) = self.header() {
            h.rc.fetch_add(1, Ordering::Relaxed);
        }
        Value(self.0)
    }

    /// Drop one reference without consuming a `Value` wrapper (used by FFI code).
    ///
    /// # Safety
    /// `bits` must hold a reference the caller owns.
    #[inline]
    pub unsafe fn release_raw(bits: u64) {
        drop(Value(bits));
    }
}

impl Clone for Value {
    #[inline]
    fn clone(&self) -> Value {
        self.retain()
    }
}

impl Drop for Value {
    #[inline]
    fn drop(&mut self) {
        if let Some(h) = self.header() {
            // An atom or pending value losing a reference may be the root of a
            // cycle: the collector's buffer takes this reference over (once) and
            // decides later.
            if matches!(h.kind, Kind::Atom | Kind::Pending)
                && unsafe { crate::gc::take_candidate(self.0 as *const ObjHeader) }
            {
                return;
            }
            // SAFETY: `self` holds one reference.
            unsafe { release_raw(self.0) }
        }
    }
}

/// Drop one reference without the cycle-candidate hook (the collector's own path).
///
/// # Safety
/// `bits` must hold a reference the caller owns, to a heap object.
pub(crate) unsafe fn release_raw(bits: u64) {
    let h = unsafe { &*(bits as *const ObjHeader) };
    if h.rc.fetch_sub(1, Ordering::Release) == 1 {
        fence(Ordering::Acquire);
        note_freed(h.kind);
        // SAFETY: this was the last reference.
        unsafe { obj::free(bits as *mut ObjHeader) }
    }
}

impl Default for Value {
    fn default() -> Value {
        Value::NIL
    }
}

// ---- equality and hashing (Clojure `=` / `hash`) -------------------------------

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        crate::equiv::equiv(self, other)
    }
}
impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(crate::equiv::hasheq(self));
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::printer::pr_str(self))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::printer::print_str(self))
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::int(n)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::bool(b)
    }
}
impl From<f64> for Value {
    fn from(d: f64) -> Value {
        Value::double(d)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Value {
        obj::string::new(s)
    }
}
impl From<String> for Value {
    fn from(s: String) -> Value {
        obj::string::new(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediates_round_trip() {
        assert!(Value::NIL.is_nil());
        assert!(!Value::NIL.truthy());
        assert!(!Value::FALSE.truthy());
        assert!(Value::TRUE.truthy());
        assert!(Value::int(0).truthy());
        assert_eq!(Value::int(42).as_fixnum(), Some(42));
        assert_eq!(Value::int(-7).as_fixnum(), Some(-7));
        assert_eq!(Value::int(FIXNUM_MAX).as_fixnum(), Some(FIXNUM_MAX));
        assert_eq!(Value::int(FIXNUM_MIN).as_fixnum(), Some(FIXNUM_MIN));
        assert_eq!(Value::char('λ').as_char(), Some('λ'));
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert!(Value::EXC.is_exc() && !Value::EXC.is_ptr());
        assert!(Value::UNBOUND.is_unbound());
    }

    #[test]
    fn big_ints_box() {
        let v = Value::int(i64::MAX);
        assert!(v.is_ptr());
        assert_eq!(v.kind(), Some(Kind::Long));
        assert_eq!(v.as_int(), Some(i64::MAX));
        let w = Value::int(FIXNUM_MAX + 1);
        assert_eq!(w.as_int(), Some(FIXNUM_MAX + 1));
    }
}
