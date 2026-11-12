//! Interior nodes of the persistent collections: a header, one auxiliary word and
//! an array of values.
//!
//! Nodes are ordinary heap objects, so a collection's structure is traced, aged
//! and swept like everything else: a new version of a large vector or map shares
//! its nodes with the old one, and a collection only ever looks at the nodes that
//! were allocated since the last one. A node is never changed once it has been
//! built (an update copies the path to the root), so an old node never refers to
//! a younger object and no write barrier is involved. Nodes own no Rust data:
//! dead ones are released without being looked at.
//!
//! The number of slots is kept in the header's `pos` field (nodes have no source
//! position).
use super::Kind;
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct NodeObj {
    pub hdr: ObjHeader,
    /// The hash trie's bitmaps (see `hamt.rs`); unused by vectors.
    pub aux: u64,
    // followed by `hdr.pos` values
}

const SLOTS_OFFSET: usize = std::mem::size_of::<NodeObj>();

/// Allocate a node with `n` slots. The slots are not initialised: the caller
/// writes every one of them before anything else can happen (allocation is not a
/// safepoint, so the collector cannot see the node in between).
#[inline]
pub fn alloc(aux: u64, n: usize) -> (Value, &'static mut [Value]) {
    let p = super::raw_alloc(
        std::alloc::Layout::from_size_align(SLOTS_OFFSET + 8 * n, 8).expect("node layout"),
    ) as *mut NodeObj;
    unsafe {
        let mut hdr = ObjHeader::new(Kind::Node);
        hdr.pos = n as u32;
        std::ptr::write(p, NodeObj { hdr, aux });
        let slots = (p as *mut u8).add(SLOTS_OFFSET) as *mut Value;
        (Value::from_obj(p), std::slice::from_raw_parts_mut(slots, n))
    }
}

/// A node holding a copy of `items`.
#[inline]
pub fn from_slice(aux: u64, items: &[Value]) -> Value {
    let (v, slots) = alloc(aux, items.len());
    slots.copy_from_slice(items);
    v
}

/// The slots of a node. The borrow is tied to the reference to the node, which in
/// turn is reachable from a collection the caller holds.
#[inline]
pub fn slots(v: &Value) -> &[Value] {
    debug_assert!(v.is_kind(Kind::Node));
    unsafe { raw_slots(v.ptr() as *const NodeObj) }
}

#[inline]
pub fn aux(v: &Value) -> u64 {
    debug_assert!(v.is_kind(Kind::Node));
    unsafe { (*(v.ptr() as *const NodeObj)).aux }
}

/// The slots of the node at `p` (for the collector).
///
/// # Safety
/// `p` must point at a live node.
#[inline]
pub unsafe fn raw_slots<'a>(p: *const NodeObj) -> &'a [Value] {
    unsafe {
        let n = (*p).hdr.pos as usize;
        std::slice::from_raw_parts((p as *const u8).add(SLOTS_OFFSET) as *const Value, n)
    }
}
