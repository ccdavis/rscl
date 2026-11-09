//! Heap object kinds. Every object is `#[repr(C)]` with an `ObjHeader` first so
//! generated code can read the kind at a fixed offset.

use crate::value::{ObjHeader, Value};

pub mod atom;
pub mod bigint;
pub mod cons;
pub mod double;
pub mod exception;
pub mod func;
pub mod keyword;
pub mod lazyseq;
pub mod list;
pub mod long;
pub mod map;
pub mod native;
pub mod pending;
pub mod regex;
pub mod set;
pub mod sorted;
pub mod string;
pub mod symbol;
pub mod types;
pub mod var;
pub mod vector;

/// Object kind tag stored in `ObjHeader::kind`. Values are part of the ABI that
/// generated code relies on (e.g. `Kind::Fn` in the call fast path).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Long = 1,
    Double = 2,
    String = 3,
    Symbol = 4,
    Keyword = 5,
    List = 6,
    Vector = 7,
    Map = 8,
    Set = 9,
    Fn = 10,
    Var = 11,
    Exception = 12,
    Atom = 13,
    LazySeq = 14,
    Namespace = 15,
    Cons = 16,
    Regex = 17,
    Native = 18,
    Type = 19,
    Instance = 20,
    Protocol = 21,
    SortedMap = 22,
    SortedSet = 23,
    /// A future, promise or delay (see `pending.rs`).
    Pending = 24,
    /// Arbitrary-precision integer (`123N`).
    BigInt = 25,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Long => "Long",
            Kind::Double => "Double",
            Kind::String => "String",
            Kind::Symbol => "Symbol",
            Kind::Keyword => "Keyword",
            Kind::List => "List",
            Kind::Vector => "Vector",
            Kind::Map => "Map",
            Kind::Set => "Set",
            Kind::Fn => "Fn",
            Kind::Var => "Var",
            Kind::Exception => "Exception",
            Kind::Atom => "Atom",
            Kind::LazySeq => "LazySeq",
            Kind::Namespace => "Namespace",
            Kind::Cons => "Cons",
            Kind::Regex => "Regex",
            Kind::Native => "Native",
            Kind::Type => "Type",
            Kind::Instance => "Instance",
            Kind::Protocol => "Protocol",
            Kind::SortedMap => "SortedMap",
            Kind::SortedSet => "SortedSet",
            Kind::Pending => "Pending",
            Kind::BigInt => "BigInt",
        }
    }
}

/// Free an object whose refcount just reached zero.
///
/// # Safety
/// `p` must point at a live heap object with no remaining references.
pub(crate) unsafe fn free(p: *mut ObjHeader) {
    unsafe {
        match (*p).kind {
            Kind::Long => drop(Box::from_raw(p as *mut long::LongObj)),
            Kind::Double => drop(Box::from_raw(p as *mut double::DoubleObj)),
            Kind::String => drop(Box::from_raw(p as *mut string::StrObj)),
            Kind::Symbol => drop(Box::from_raw(p as *mut symbol::SymObj)),
            Kind::Vector => drop(Box::from_raw(p as *mut vector::VecObj)),
            Kind::Map => drop(Box::from_raw(p as *mut map::MapObj)),
            Kind::Set => drop(Box::from_raw(p as *mut set::SetObj)),
            Kind::Fn => func::free(p as *mut func::FnObj),
            Kind::Exception => drop(Box::from_raw(p as *mut exception::ExcObj)),
            Kind::Atom => drop(Box::from_raw(p as *mut atom::AtomObj)),
            Kind::Regex => drop(Box::from_raw(p as *mut regex::RegexObj)),
            Kind::Native => drop(Box::from_raw(p as *mut native::NativeObj)),
            Kind::Instance => drop(Box::from_raw(p as *mut types::InstanceObj)),
            Kind::SortedMap => drop(Box::from_raw(p as *mut sorted::SortedMapObj)),
            Kind::SortedSet => drop(Box::from_raw(p as *mut sorted::SortedSetObj)),
            Kind::Pending => drop(Box::from_raw(p as *mut pending::PendingObj)),
            Kind::BigInt => drop(Box::from_raw(p as *mut bigint::BigIntObj)),
            // Sequence cells are dismantled iteratively: a long realised chain would
            // otherwise recurse once per element while dropping.
            Kind::List | Kind::Cons | Kind::LazySeq => {
                let tail = dismantle(p);
                free_tail(tail);
            }
            Kind::Keyword => keyword::free(p as *mut keyword::KwObj),
            // Immortal kinds never reach a zero refcount.
            Kind::Var | Kind::Namespace | Kind::Type | Kind::Protocol => {
                unreachable!("attempt to free an immortal object kind")
            }
        }
    }
}

/// Free one sequence cell whose last reference is gone and hand back its tail
/// (still owned by the caller).
unsafe fn dismantle(p: *mut ObjHeader) -> Value {
    unsafe {
        match (*p).kind {
            Kind::List => {
                let mut b = Box::from_raw(p as *mut list::ListObj);
                let t = std::mem::take(&mut b.rest);
                drop(b);
                t
            }
            Kind::Cons => {
                let mut b = Box::from_raw(p as *mut cons::ConsObj);
                let t = std::mem::take(&mut b.more);
                drop(b);
                t
            }
            Kind::LazySeq => {
                let mut b = Box::from_raw(p as *mut lazyseq::LazySeqObj);
                let t = lazyseq::take_tail(&mut b);
                drop(b);
                t
            }
            _ => unreachable!(),
        }
    }
}

/// Release `tail`, walking sequence cells iteratively while this is their last
/// reference.
fn free_tail(mut tail: Value) {
    use std::sync::atomic::{Ordering, fence};
    loop {
        let Some(h) = tail.header() else { return };
        if !matches!(h.kind, Kind::List | Kind::Cons | Kind::LazySeq) {
            return; // an ordinary value: dropped normally
        }
        if h.rc.fetch_sub(1, Ordering::Release) != 1 {
            std::mem::forget(tail);
            return;
        }
        fence(Ordering::Acquire);
        crate::value::note_freed(h.kind);
        let p = tail.ptr() as *mut ObjHeader;
        std::mem::forget(tail);
        tail = unsafe { dismantle(p) };
    }
}

/// Allocate a boxed object and hand back an owned `Value`.
#[inline]
pub(crate) fn alloc<T>(obj: T) -> Value {
    Value::from_obj(Box::into_raw(Box::new(obj)))
}

/// View a heap value as `&T`. Callers check the kind first.
#[inline]
pub(crate) unsafe fn cast<T>(v: &Value) -> &T {
    unsafe { &*(v.ptr() as *const T) }
}

/// Metadata of a value that supports it (`nil` when none); `None` for other kinds.
pub fn meta(v: &Value) -> Option<Value> {
    Some(match v.kind()? {
        Kind::Symbol => symbol::get(v).meta.clone(),
        Kind::List => list::get(v).meta.clone(),
        Kind::Vector => vector::get(v).meta.clone(),
        Kind::Map => map::get(v).meta.clone(),
        Kind::Set => set::get(v).meta.clone(),
        Kind::Atom => atom::get(v).meta(),
        Kind::Var => var::get(v).meta(),
        Kind::Fn => func::get(v).proto().meta.clone(),
        Kind::Instance => types::inst(v).meta.clone(),
        Kind::SortedMap => sorted::map_get(v).meta.clone(),
        Kind::SortedSet => sorted::set_get(v).meta.clone(),
        _ => return None,
    })
}

/// Overwrite the metadata of a value that no one else references yet (the reader
/// calls this on forms it just created). Takes ownership of `meta`.
pub fn set_meta_unshared(v: &Value, meta_: Value) {
    unsafe {
        let p = v.ptr();
        match v.kind() {
            Some(Kind::Symbol) => (*(p as *mut symbol::SymObj)).meta = meta_,
            Some(Kind::List) => (*(p as *mut list::ListObj)).meta = meta_,
            Some(Kind::Vector) => (*(p as *mut vector::VecObj)).meta = meta_,
            Some(Kind::Map) => (*(p as *mut map::MapObj)).meta = meta_,
            Some(Kind::Set) => (*(p as *mut set::SetObj)).meta = meta_,
            Some(Kind::Instance) => (*(p as *mut types::InstanceObj)).meta = meta_,
            Some(Kind::SortedMap) => (*(p as *mut sorted::SortedMapObj)).meta = meta_,
            Some(Kind::SortedSet) => (*(p as *mut sorted::SortedSetObj)).meta = meta_,
            _ => {}
        }
    }
}
