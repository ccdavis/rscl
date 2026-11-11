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
pub mod vecseq;
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
    /// A seq over a vector (see `vecseq.rs`).
    VecSeq = 26,
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
            Kind::VecSeq => "VecSeq",
        }
    }
}

/// Finalise an object the collector found unreachable: its fields are dropped as
/// plain Rust data (the values it referred to are not touched; they are the
/// collector's). The sweep then releases its slot.
///
/// # Safety
/// `p` must point at a heap object nothing refers to any more.
pub(crate) unsafe fn free(p: *mut ObjHeader) {
    unsafe {
        match (*p).kind {
            Kind::Long => free_obj(p as *mut long::LongObj),
            Kind::Double => free_obj(p as *mut double::DoubleObj),
            Kind::String => free_obj(p as *mut string::StrObj),
            Kind::Symbol => free_obj(p as *mut symbol::SymObj),
            Kind::Vector => free_obj(p as *mut vector::VecObj),
            Kind::Map => free_obj(p as *mut map::MapObj),
            Kind::Set => free_obj(p as *mut set::SetObj),
            Kind::Fn | Kind::VecSeq => {}
            Kind::Exception => free_obj(p as *mut exception::ExcObj),
            Kind::Atom => free_obj(p as *mut atom::AtomObj),
            Kind::Regex => free_obj(p as *mut regex::RegexObj),
            Kind::Native => free_obj(p as *mut native::NativeObj),
            Kind::Instance => free_obj(p as *mut types::InstanceObj),
            Kind::SortedMap => free_obj(p as *mut sorted::SortedMapObj),
            Kind::SortedSet => free_obj(p as *mut sorted::SortedSetObj),
            Kind::Pending => free_obj(p as *mut pending::PendingObj),
            Kind::BigInt => free_obj(p as *mut bigint::BigIntObj),
            Kind::List => free_obj(p as *mut list::ListObj),
            Kind::Cons => free_obj(p as *mut cons::ConsObj),
            Kind::LazySeq => free_obj(p as *mut lazyseq::LazySeqObj),
            Kind::Keyword => free_obj(p as *mut keyword::KwObj),
            // Permanent kinds are never swept.
            Kind::Var | Kind::Namespace | Kind::Type | Kind::Protocol => {
                unreachable!("attempt to free a permanent object kind")
            }
        }
    }
}

/// Memory for a heap object, from the object heap (see `arena.rs`).
pub(crate) fn raw_alloc(layout: std::alloc::Layout) -> *mut u8 {
    debug_assert!(layout.align() <= 16);
    crate::arena::alloc(layout.size())
}

/// Drop an object's fields; its slot is released by the sweep.
unsafe fn free_obj<T>(p: *mut T) {
    unsafe { std::ptr::drop_in_place(p) };
}

/// Allocate an object.
pub(crate) fn alloc_raw<T>(obj: T) -> *mut T {
    let p = raw_alloc(std::alloc::Layout::new::<T>()) as *mut T;
    unsafe { std::ptr::write(p, obj) };
    p
}

/// Allocate an object and return a reference to it.
#[inline]
pub(crate) fn alloc<T>(obj: T) -> Value {
    Value::from_obj(alloc_raw(obj))
}

/// View a heap value as `&T`. Callers check the kind first.
#[inline]
pub(crate) unsafe fn cast<T>(v: &Value) -> &T {
    unsafe { &*(v.ptr() as *const T) }
}

/// Metadata of a value that supports it (`nil` when none); `None` for other kinds.
pub fn meta(v: &Value) -> Option<Value> {
    Some(match v.kind()? {
        Kind::Symbol => symbol::get(v).meta,
        Kind::List => list::get(v).meta,
        Kind::Vector => vector::get(v).meta,
        Kind::Map => map::get(v).meta,
        Kind::Set => set::get(v).meta,
        Kind::Atom => atom::get(v).meta(),
        Kind::Var => var::get(v).meta(),
        Kind::Fn => func::get(v).proto().meta,
        Kind::Instance => types::inst(v).meta,
        Kind::SortedMap => sorted::map_get(v).meta,
        Kind::SortedSet => sorted::set_get(v).meta,
        _ => return None,
    })
}

/// Overwrite the metadata of a value that no one else references yet (the reader
/// calls this on forms it just created).
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
            _ => return,
        }
        crate::gc::write_barrier(p);
    }
}
