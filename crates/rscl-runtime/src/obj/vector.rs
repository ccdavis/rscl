//! Persistent vector: a 32-way trie of nodes with the last (partial) chunk, the
//! tail, kept in the vector object itself, as in Clojure's `PersistentVector`.
//!
//! A vector of up to 32 elements is a single object. `conj` makes a new object
//! with the tail one longer or, when the tail is full, turns the tail into a leaf
//! and copies the path from the root to where it goes; `nth` and `assoc` follow
//! one path. The nodes are heap objects (see `node.rs`), so versions of a vector
//! share structure and the collector looks only at what is new.
use std::ops::Index;

use super::{Kind, cast, node};
use crate::value::{ObjHeader, Value};

const BITS: u32 = 5;
const WIDTH: usize = 1 << BITS;
const MASK: usize = WIDTH - 1;

#[repr(C)]
pub struct VecObj {
    pub hdr: ObjHeader,
    len: usize,
    /// Bits of the index consumed above the leaves: `BITS` times the trie's depth.
    shift: u32,
    /// A node of nodes, or `nil` while everything fits in the tail.
    root: Value,
    pub meta: Value,
    // followed by the tail: the last 1..=32 elements (none in an empty vector)
}

const TAIL_OFFSET: usize = std::mem::size_of::<VecObj>();

/// Index of the first element kept in the tail of a vector of `len` elements.
#[inline]
fn tail_off(len: usize) -> usize {
    if len < WIDTH {
        0
    } else {
        ((len - 1) >> BITS) << BITS
    }
}

/// Allocate a vector object. Its tail is not initialised: the caller writes every
/// element of it before anything else can happen (see `node::alloc`).
#[inline]
fn alloc(len: usize, shift: u32, root: Value, meta: Value) -> (Value, &'static mut [Value]) {
    let n = len - tail_off(len);
    let p = super::raw_alloc(
        std::alloc::Layout::from_size_align(TAIL_OFFSET + 8 * n, 8).expect("vector layout"),
    ) as *mut VecObj;
    unsafe {
        std::ptr::write(
            p,
            VecObj {
                hdr: ObjHeader::new(Kind::Vector),
                len,
                shift,
                root,
                meta,
            },
        );
        let tail = (p as *mut u8).add(TAIL_OFFSET) as *mut Value;
        (Value::from_obj(p), std::slice::from_raw_parts_mut(tail, n))
    }
}

fn slots(n: &Value) -> &[Value] {
    if n.is_nil() { &[] } else { node::slots(n) }
}

/// A copy of node `n` with slot `i` set to `x` (one longer when `i` is its length).
fn with_slot(n: &Value, i: usize, x: Value) -> Value {
    let old = slots(n);
    let (v, new) = node::alloc(0, old.len().max(i + 1));
    new[..old.len()].copy_from_slice(old);
    new[i] = x;
    v
}

/// A chain of single-child nodes `level / BITS` deep ending in `leaf`.
fn new_path(level: u32, leaf: Value) -> Value {
    if level == 0 {
        leaf
    } else {
        node::from_slice(0, &[new_path(level - BITS, leaf)])
    }
}

/// `parent` (at `level`, in a vector of `len` elements) with the leaf `leaf`
/// added after its last one.
fn push_tail(len: usize, level: u32, parent: &Value, leaf: Value) -> Value {
    let sub = ((len - 1) >> level) & MASK;
    let child = if level == BITS {
        leaf
    } else {
        match slots(parent).get(sub) {
            Some(child) => push_tail(len, level - BITS, child, leaf),
            None => new_path(level - BITS, leaf),
        }
    };
    with_slot(parent, sub, child)
}

fn assoc_in(level: u32, n: &Value, i: usize, x: Value) -> Value {
    if level == 0 {
        with_slot(n, i & MASK, x)
    } else {
        let sub = (i >> level) & MASK;
        let child = assoc_in(level - BITS, &node::slots(n)[sub], i, x);
        with_slot(n, sub, child)
    }
}

/// `n` (at `level`, in a vector of `len` elements) without its last leaf; `nil`
/// when nothing is left of it.
fn pop_tail(len: usize, level: u32, n: &Value) -> Value {
    let sub = ((len - 2) >> level) & MASK;
    if level > BITS {
        let child = pop_tail(len, level - BITS, &node::slots(n)[sub]);
        if !child.is_nil() {
            return with_slot(n, sub, child);
        }
    }
    if sub == 0 {
        Value::NIL
    } else {
        node::from_slice(0, &node::slots(n)[..sub])
    }
}

pub fn empty() -> Value {
    alloc(0, BITS, Value::NIL, Value::NIL).0
}

pub fn from_slice(items: &[Value]) -> Value {
    let (body, tail) = items.split_at(tail_off(items.len()));
    let (root, shift) = if body.is_empty() {
        (Value::NIL, BITS)
    } else {
        let mut level: Vec<Value> = body.chunks(WIDTH).map(|c| node::from_slice(0, c)).collect();
        let mut shift = BITS;
        while level.len() > WIDTH {
            level = level
                .chunks(WIDTH)
                .map(|c| node::from_slice(0, c))
                .collect();
            shift += BITS;
        }
        (node::from_slice(0, &level), shift)
    };
    let (v, new_tail) = alloc(items.len(), shift, root, Value::NIL);
    new_tail.copy_from_slice(tail);
    v
}

pub fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Value {
    let items: Vec<Value> = iter.into_iter().collect();
    from_slice(&items)
}

pub fn get(v: &Value) -> &VecObj {
    debug_assert!(v.is_kind(Kind::Vector));
    unsafe { cast::<VecObj>(v) }
}

pub fn as_vec(v: &Value) -> Option<&VecObj> {
    if v.is_kind(Kind::Vector) {
        Some(get(v))
    } else {
        None
    }
}

impl VecObj {
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn tail(&self) -> &[Value] {
        let n = self.len - tail_off(self.len);
        unsafe {
            let p = (self as *const VecObj as *const u8).add(TAIL_OFFSET) as *const Value;
            std::slice::from_raw_parts(p, n)
        }
    }

    /// The leaf node holding element `i`, which must be below the tail.
    #[inline]
    fn leaf_node(&self, i: usize) -> &Value {
        let mut n = &self.root;
        let mut level = self.shift;
        while level > 0 {
            n = &node::slots(n)[(i >> level) & MASK];
            level -= BITS;
        }
        n
    }

    /// The chunk holding element `i` (which must be in range).
    #[inline]
    fn chunk(&self, i: usize) -> &[Value] {
        if i >= tail_off(self.len) {
            self.tail()
        } else {
            node::slots(self.leaf_node(i))
        }
    }

    /// The elements from `i`, which must be in range, to the end of its chunk.
    #[inline]
    pub fn chunk_from(&self, i: usize) -> &[Value] {
        &self.chunk(i)[i & MASK..]
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<&Value> {
        if i < self.len {
            Some(&self.chunk(i)[i & MASK])
        } else {
            None
        }
    }
    pub fn front(&self) -> Option<&Value> {
        self.get(0)
    }
    pub fn back(&self) -> Option<&Value> {
        self.tail().last()
    }

    /// This vector with `x` added at the end (and the same metadata).
    pub fn conj(&self, x: Value) -> Value {
        let tail = self.tail();
        let (v, new_tail) = if tail.len() < WIDTH {
            alloc(self.len + 1, self.shift, self.root, self.meta)
        } else {
            // The tail is full: it becomes a leaf of the trie.
            let leaf = node::from_slice(0, tail);
            let (root, shift) = if (self.len >> BITS) > (1usize << self.shift) {
                // The trie is full too: one more level.
                (
                    node::from_slice(0, &[self.root, new_path(self.shift, leaf)]),
                    self.shift + BITS,
                )
            } else {
                (
                    push_tail(self.len, self.shift, &self.root, leaf),
                    self.shift,
                )
            };
            alloc(self.len + 1, shift, root, self.meta)
        };
        let n = new_tail.len() - 1;
        new_tail[..n].copy_from_slice(&tail[tail.len() - n..]);
        new_tail[n] = x;
        v
    }

    /// This vector with element `i`, which must be in range, replaced.
    pub fn assoc(&self, i: usize, x: Value) -> Value {
        assert!(i < self.len, "index out of range");
        let in_tail = i >= tail_off(self.len);
        let root = if in_tail {
            self.root
        } else {
            assoc_in(self.shift, &self.root, i, x)
        };
        let (v, new_tail) = alloc(self.len, self.shift, root, self.meta);
        new_tail.copy_from_slice(self.tail());
        if in_tail {
            new_tail[i & MASK] = x;
        }
        v
    }

    /// This vector, which must not be empty, without its last element.
    pub fn pop(&self) -> Value {
        assert!(self.len > 0, "pop of an empty vector");
        let tail = self.tail();
        if tail.len() > 1 || self.len == 1 {
            let (v, new_tail) = alloc(self.len - 1, self.shift, self.root, self.meta);
            new_tail.copy_from_slice(&tail[..tail.len() - 1]);
            return v;
        }
        // The tail empties: the last leaf of the trie becomes the tail.
        let leaf = *self.leaf_node(self.len - 2);
        let mut root = pop_tail(self.len, self.shift, &self.root);
        let mut shift = self.shift;
        if shift > BITS && slots(&root).len() == 1 {
            root = node::slots(&root)[0];
            shift -= BITS;
        }
        let (v, new_tail) = alloc(self.len - 1, shift, root, self.meta);
        new_tail.copy_from_slice(node::slots(&leaf));
        v
    }

    /// This vector as a value.
    fn value(&self) -> Value {
        Value::from_obj(self as *const VecObj as *mut VecObj)
    }

    /// This vector with `items` appended (and the same metadata).
    pub fn extend(&self, items: &[Value]) -> Value {
        if items.is_empty() {
            return self.value();
        }
        if items.len() * 8 < self.len {
            // A few elements onto a big vector: share its structure.
            let mut v = self.conj(items[0]);
            for x in &items[1..] {
                v = get(&v).conj(*x);
            }
            return v;
        }
        let mut all = self.to_vec();
        all.extend_from_slice(items);
        get(&from_slice(&all)).with_meta(self.meta)
    }

    /// A copy of this vector with other metadata.
    pub fn with_meta(&self, meta: Value) -> Value {
        let (v, new_tail) = alloc(self.len, self.shift, self.root, meta);
        new_tail.copy_from_slice(self.tail());
        v
    }

    pub fn to_vec(&self) -> Vec<Value> {
        self.iter().copied().collect()
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter {
            v: self,
            i: 0,
            end: self.len,
            chunk: &[],
        }
    }

    /// What the collector follows.
    pub(crate) fn trace(&self, f: &mut dyn FnMut(&Value)) {
        f(&self.root);
        f(&self.meta);
        for x in self.tail() {
            f(x);
        }
    }
}

impl Index<usize> for VecObj {
    type Output = Value;
    fn index(&self, i: usize) -> &Value {
        self.get(i).expect("index out of range")
    }
}

impl<'a> IntoIterator for &'a VecObj {
    type Item = &'a Value;
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

/// Elements `i..end` in order, a chunk at a time.
pub struct Iter<'a> {
    v: &'a VecObj,
    i: usize,
    end: usize,
    /// The chunk that holds element `i`, once `i` has been looked at.
    chunk: &'a [Value],
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a Value;
    #[inline]
    fn next(&mut self) -> Option<&'a Value> {
        if self.i >= self.end {
            return None;
        }
        if self.i & MASK == 0 || self.chunk.is_empty() {
            self.chunk = self.v.chunk(self.i);
        }
        let x = &self.chunk[self.i & MASK];
        self.i += 1;
        Some(x)
    }
    fn nth(&mut self, n: usize) -> Option<&'a Value> {
        self.i = self.i.saturating_add(n).min(self.end);
        self.chunk = &[];
        self.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.end - self.i;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl<'a> DoubleEndedIterator for Iter<'a> {
    fn next_back(&mut self) -> Option<&'a Value> {
        if self.i >= self.end {
            return None;
        }
        self.end -= 1;
        self.v.get(self.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(v: &Value) -> Vec<i64> {
        get(v).iter().map(|x| x.as_fixnum().unwrap()).collect()
    }

    #[test]
    fn matches_a_plain_vector() {
        // Sizes around every boundary of the trie: tail, one level, two, three.
        let mut v = empty();
        let mut model: Vec<i64> = Vec::new();
        for i in 0..(32 * 32 * 32 + 32 * 3 + 5) as i64 {
            v = get(&v).conj(Value::int(i));
            model.push(i);
            if i < 1100 || i % 997 == 0 {
                let o = get(&v);
                assert_eq!(o.len(), model.len());
                assert_eq!(o.back().unwrap().as_fixnum(), Some(i));
                assert_eq!(o[model.len() / 2].as_fixnum(), Some(model[model.len() / 2]));
            }
        }
        assert_eq!(ints(&v), model);
        assert_eq!(get(&v).iter().rev().count(), model.len());
        assert_eq!(
            get(&v).iter().nth(30_000).unwrap().as_fixnum(),
            Some(30_000)
        );
        assert!(get(&v).iter().nth(40_000).is_none());

        // Updates leave the old version alone.
        let old = v;
        for i in [0usize, 31, 32, 1023, 1024, 32_767, 32_768, model.len() - 1] {
            v = get(&v).assoc(i, Value::int(-1));
            model[i] = -1;
        }
        assert_eq!(ints(&v), model);
        assert_eq!(get(&old)[1024].as_fixnum(), Some(1024));
        assert_eq!(
            get(&old).back().unwrap().as_fixnum(),
            Some(model.len() as i64 - 1)
        );

        // Built in one go, it is the same vector.
        let items: Vec<Value> = model.iter().map(|&n| Value::int(n)).collect();
        for n in [
            0usize,
            1,
            31,
            32,
            33,
            64,
            65,
            1024,
            1056,
            1057,
            32_800,
            items.len(),
        ] {
            let b = from_slice(&items[..n]);
            assert_eq!(ints(&b), model[..n]);
            let c = get(&b).conj(Value::int(7));
            assert_eq!(get(&c).len(), n + 1);
            assert_eq!(get(&c)[n].as_fixnum(), Some(7));
            assert_eq!(ints(&c)[..n], model[..n]);
        }

        // Popping walks back through every shape.
        while !get(&v).is_empty() {
            assert_eq!(get(&v).back().unwrap().as_fixnum(), model.pop());
            v = get(&v).pop();
            if model.len() < 1100 || model.len().is_multiple_of(991) {
                assert_eq!(get(&v).len(), model.len());
                assert_eq!(
                    get(&v).back().map(|x| x.as_fixnum().unwrap()),
                    model.last().copied()
                );
            }
            if model.len() == 33_000 || model.len() == 1000 || model.len() == 40 {
                assert_eq!(ints(&v), model);
                // And it can grow again from there, a few at a time or in bulk.
                let more: Vec<Value> = (0..70).map(Value::int).collect();
                let w = get(&get(&v).conj(Value::int(9))).extend(&more);
                assert_eq!(get(&w).len(), model.len() + 71);
                assert_eq!(get(&w)[model.len()].as_fixnum(), Some(9));
                assert_eq!(get(&w).back().unwrap().as_fixnum(), Some(69));
                assert_eq!(ints(&w)[..model.len()], model[..]);
            }
        }
        assert!(model.is_empty());
    }
}
