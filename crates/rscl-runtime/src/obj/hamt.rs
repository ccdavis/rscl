//! Persistent hash trie, the structure behind hash maps and sets.
//!
//! Each level of the trie consumes five bits of the key's hash. A node (see
//! `node.rs`) has two 32-bit maps: which of its 32 positions hold an entry, and
//! which hold a child node. Its slots follow in position order, an entry taking
//! `W` slots (the key, and for a map the value) and a child one. Keys whose
//! hashes agree in all 64 bits end up in a collision node at the bottom, a plain
//! list of entries (both maps zero).
//!
//! An update copies the nodes on the path from the root and shares the rest, and
//! the nodes are heap objects, so the collector looks only at the part of a big
//! map that is new.
//!
//! By default the hash is `hasheq`, so iteration (and printing) order is the same
//! in every run. A service that keys maps by attacker-controlled data can set
//! `RSCL_HASH_SEED=random` (or a number) to mix keys with a per-process secret,
//! which defeats precomputed hash collisions at the cost of a stable order.
use std::hash::{BuildHasher, RandomState};
use std::sync::OnceLock;

use super::node;
use crate::equiv::{equiv, hasheq, mix64};
use crate::value::Value;

const BITS: u32 = 5;
/// The deepest level at which hash bits remain (the last one has four).
const LAST_SHIFT: u32 = 60;

/// The process-wide seed from `RSCL_HASH_SEED`: `None` for the deterministic hash.
fn seed() -> Option<u64> {
    static SEED: OnceLock<Option<u64>> = OnceLock::new();
    *SEED.get_or_init(|| {
        let Ok(v) = std::env::var("RSCL_HASH_SEED") else {
            return None;
        };
        let v = v.trim();
        if v == "random" || v == "0" {
            return Some(RandomState::new().hash_one(0u64) | 1);
        }
        match v.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                eprintln!(
                    "RSCL_HASH_SEED={v:?} is not \"random\" or a number: using the deterministic hasher"
                );
                None
            }
        }
    })
}

#[cfg(test)]
thread_local! {
    /// Tests of collision nodes give every key the same hash.
    static FORCED_HASH: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[inline]
fn key_hash(k: &Value) -> u64 {
    #[cfg(test)]
    if let Some(h) = FORCED_HASH.get() {
        return h;
    }
    let h = hasheq(k);
    match seed() {
        None => h,
        Some(key) => mix64(h ^ key),
    }
}

/// The trie with entries of `W` values: a small handle that is copied freely. An
/// "update" replaces the handle with one for the new version.
#[derive(Clone, Copy)]
pub struct Hamt<const W: usize> {
    /// A node, or `nil` when empty.
    root: Value,
    len: usize,
}

#[inline]
fn maps(n: &Value) -> (u32, u32) {
    let aux = node::aux(n);
    (aux as u32, (aux >> 32) as u32)
}

#[inline]
fn aux(data: u32, nodes: u32) -> u64 {
    u64::from(data) | u64::from(nodes) << 32
}

#[inline]
fn bit_at(hash: u64, shift: u32) -> u32 {
    1 << ((hash >> shift) & 31)
}

/// Slot index of position `bit` in a node with these maps.
#[inline]
fn offset<const W: usize>(data: u32, nodes: u32, bit: u32) -> usize {
    let below = bit - 1;
    W * (data & below).count_ones() as usize + (nodes & below).count_ones() as usize
}

/// A copy of `slots` with `remove` slots at `at` replaced by `put`.
fn splice(aux: u64, slots: &[Value], at: usize, remove: usize, put: &[Value]) -> Value {
    let (v, new) = node::alloc(aux, slots.len() - remove + put.len());
    new[..at].copy_from_slice(&slots[..at]);
    new[at..at + put.len()].copy_from_slice(put);
    new[at + put.len()..].copy_from_slice(&slots[at + remove..]);
    v
}

/// A node at `shift` holding two entries with different keys.
fn pair<const W: usize>(shift: u32, h1: u64, e1: &[Value], h2: u64, e2: &[Value]) -> Value {
    if shift > LAST_SHIFT {
        let (v, slots) = node::alloc(0, 2 * W);
        slots[..W].copy_from_slice(e1);
        slots[W..].copy_from_slice(e2);
        return v;
    }
    let (b1, b2) = (bit_at(h1, shift), bit_at(h2, shift));
    if b1 == b2 {
        let child = pair::<W>(shift + BITS, h1, e1, h2, e2);
        return node::from_slice(aux(0, b1), &[child]);
    }
    let (v, slots) = node::alloc(aux(b1 | b2, 0), 2 * W);
    let (lo, hi) = if b1 < b2 { (e1, e2) } else { (e2, e1) };
    slots[..W].copy_from_slice(lo);
    slots[W..].copy_from_slice(hi);
    v
}

/// What an insertion did.
enum Put {
    /// The key was new.
    Added,
    /// The key was there; its value was replaced (the stored key is kept).
    Replaced,
    /// The key was there with this very value: nothing changed.
    Same,
}

fn insert<const W: usize>(n: &Value, shift: u32, hash: u64, e: &[Value; W]) -> (Value, Put) {
    if n.is_nil() {
        let a = if shift > LAST_SHIFT {
            0
        } else {
            aux(bit_at(hash, shift), 0)
        };
        return (node::from_slice(a, e), Put::Added);
    }
    let slots = node::slots(n);
    if shift > LAST_SHIFT {
        // A collision node: every key in it has this hash.
        for at in (0..slots.len()).step_by(W) {
            if equiv(&slots[at], &e[0]) {
                return replace::<W>(n, slots, at, e);
            }
        }
        return (splice(0, slots, slots.len(), 0, e), Put::Added);
    }
    let (data, nodes) = maps(n);
    let bit = bit_at(hash, shift);
    let at = offset::<W>(data, nodes, bit);
    if data & bit != 0 {
        if equiv(&slots[at], &e[0]) {
            return replace::<W>(n, slots, at, e);
        }
        // Two keys at one position: they move down a level together.
        let there = &slots[at..at + W];
        let child = pair::<W>(shift + BITS, key_hash(&there[0]), there, hash, e);
        let a = aux(data & !bit, nodes | bit);
        (splice(a, slots, at, W, &[child]), Put::Added)
    } else if nodes & bit != 0 {
        let (child, put) = insert::<W>(&slots[at], shift + BITS, hash, e);
        if matches!(put, Put::Same) {
            return (*n, put);
        }
        (splice(node::aux(n), slots, at, 1, &[child]), put)
    } else {
        (splice(aux(data | bit, nodes), slots, at, 0, e), Put::Added)
    }
}

/// The entry at `at` gets the value of `e`; the key that is stored stays (`1`
/// stays `1` when `1N` is put), as in Clojure.
fn replace<const W: usize>(n: &Value, slots: &[Value], at: usize, e: &[Value; W]) -> (Value, Put) {
    if (1..W).all(|i| slots[at + i].identical(&e[i])) {
        return (*n, Put::Same);
    }
    (
        splice(node::aux(n), slots, at + 1, W - 1, &e[1..]),
        Put::Replaced,
    )
}

/// `n` without `key`: `None` if it is not there, else the new node (`nil` when
/// nothing is left).
fn remove<const W: usize>(n: &Value, shift: u32, hash: u64, key: &Value) -> Option<Value> {
    if n.is_nil() {
        return None;
    }
    let slots = node::slots(n);
    if shift > LAST_SHIFT {
        let at = (0..slots.len())
            .step_by(W)
            .find(|&at| equiv(&slots[at], key))?;
        return Some(if slots.len() == W {
            Value::NIL
        } else {
            splice(0, slots, at, W, &[])
        });
    }
    let (data, nodes) = maps(n);
    let bit = bit_at(hash, shift);
    let at = offset::<W>(data, nodes, bit);
    if data & bit != 0 {
        if !equiv(&slots[at], key) {
            return None;
        }
        return Some(if slots.len() == W {
            Value::NIL
        } else {
            splice(aux(data & !bit, nodes), slots, at, W, &[])
        });
    }
    if nodes & bit == 0 {
        return None;
    }
    let child = remove::<W>(&slots[at], shift + BITS, hash, key)?;
    Some(if child.is_nil() {
        if slots.len() == 1 {
            Value::NIL
        } else {
            splice(aux(data, nodes & !bit), slots, at, 1, &[])
        }
    } else if node::slots(&child).len() == W && maps(&child).1 == 0 {
        // A child with a single entry: the entry moves up in its place.
        splice(
            aux(data | bit, nodes & !bit),
            slots,
            at,
            1,
            node::slots(&child),
        )
    } else {
        splice(node::aux(n), slots, at, 1, &[child])
    })
}

impl<const W: usize> Default for Hamt<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const W: usize> Hamt<W> {
    pub const fn new() -> Self {
        Hamt {
            root: Value::NIL,
            len: 0,
        }
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The entry whose key equals `key`.
    fn find(&self, key: &Value) -> Option<&[Value]> {
        if self.root.is_nil() {
            return None;
        }
        let hash = key_hash(key);
        let mut n = &self.root;
        let mut shift = 0;
        loop {
            let slots = node::slots(n);
            if shift > LAST_SHIFT {
                return slots
                    .as_chunks::<W>()
                    .0
                    .iter()
                    .find(|e| equiv(&e[0], key))
                    .map(|e| &e[..]);
            }
            let (data, nodes) = maps(n);
            let bit = bit_at(hash, shift);
            let at = offset::<W>(data, nodes, bit);
            if data & bit != 0 {
                return equiv(&slots[at], key).then(|| &slots[at..at + W]);
            }
            if nodes & bit == 0 {
                return None;
            }
            n = &slots[at];
            shift += BITS;
        }
    }

    /// Put an entry; true if its key was not there before.
    fn put(&mut self, e: [Value; W]) -> bool {
        let (root, put) = insert::<W>(&self.root, 0, key_hash(&e[0]), &e);
        self.root = root;
        let added = matches!(put, Put::Added);
        self.len += usize::from(added);
        added
    }

    /// Take the entry for `key` out; true if there was one.
    fn take(&mut self, key: &Value) -> bool {
        match remove::<W>(&self.root, 0, key_hash(key), key) {
            Some(root) => {
                self.root = root;
                self.len -= 1;
                true
            }
            None => false,
        }
    }

    fn entries(&self) -> Entries<'_, W> {
        let mut it = Entries {
            stack: [Frame::EMPTY; DEPTH],
            depth: 0,
            left: self.len,
        };
        if !self.root.is_nil() {
            it.push(&self.root);
        }
        it
    }

    /// The root the collector follows.
    pub(crate) fn trace(&self, f: &mut dyn FnMut(&Value)) {
        f(&self.root);
    }
}

/// Levels of the trie: thirteen that consume hash bits, and the collision nodes.
const DEPTH: usize = 14;

#[derive(Clone, Copy)]
struct Frame<'a> {
    slots: &'a [Value],
    /// Positions not visited yet.
    data: u32,
    nodes: u32,
    /// The next slot.
    at: usize,
}

impl Frame<'_> {
    const EMPTY: Frame<'static> = Frame {
        slots: &[],
        data: 0,
        nodes: 0,
        at: 0,
    };
}

/// The entries of a trie, depth first in position order.
struct Entries<'a, const W: usize> {
    stack: [Frame<'a>; DEPTH],
    depth: usize,
    left: usize,
}

impl<'a, const W: usize> Entries<'a, W> {
    fn push(&mut self, n: &'a Value) {
        let (data, nodes) = maps(n);
        self.stack[self.depth] = Frame {
            slots: node::slots(n),
            data,
            nodes,
            at: 0,
        };
        self.depth += 1;
    }
}

impl<'a, const W: usize> Iterator for Entries<'a, W> {
    type Item = &'a [Value];
    fn next(&mut self) -> Option<&'a [Value]> {
        while self.depth > 0 {
            let f = &mut self.stack[self.depth - 1];
            let slots = f.slots;
            let at = f.at;
            let occupied = f.data | f.nodes;
            if occupied == 0 {
                // Done, unless this is a collision node (no maps, only entries).
                if at < slots.len() {
                    f.at += W;
                    self.left -= 1;
                    return Some(&slots[at..at + W]);
                }
                self.depth -= 1;
                continue;
            }
            let bit = occupied.isolate_lowest_one();
            if f.data & bit != 0 {
                f.data &= !bit;
                f.at += W;
                self.left -= 1;
                return Some(&slots[at..at + W]);
            }
            f.nodes &= !bit;
            f.at += 1;
            self.push(&slots[at]);
        }
        None
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

/// A hash map: entries are a key and a value.
pub type Map_ = Hamt<2>;
/// A hash set: entries are the elements.
pub type Set_ = Hamt<1>;

impl Hamt<2> {
    pub fn get(&self, k: &Value) -> Option<&Value> {
        self.find(k).map(|e| &e[1])
    }
    /// The key and value stored for `k`.
    pub fn get_key_value(&self, k: &Value) -> Option<(&Value, &Value)> {
        self.find(k).map(|e| (&e[0], &e[1]))
    }
    pub fn contains_key(&self, k: &Value) -> bool {
        self.find(k).is_some()
    }
    /// Associate `k` with `v`. A key equal to one already present keeps the stored
    /// key; only the value is replaced.
    pub fn insert(&mut self, k: Value, v: Value) {
        self.put([k, v]);
    }
    /// Remove `k`; true if it was there.
    pub fn remove(&mut self, k: &Value) -> bool {
        self.take(k)
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Value, &Value)> {
        MapIter(self.entries())
    }
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &Value> {
        self.iter().map(|(k, _)| k)
    }
    pub fn values(&self) -> impl ExactSizeIterator<Item = &Value> {
        self.iter().map(|(_, v)| v)
    }
}

/// Maps are equal when they have equal keys with equal values.
impl PartialEq for Hamt<2> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && (self.root.identical(&other.root)
                || self
                    .iter()
                    .all(|(k, v)| other.get(k).is_some_and(|w| equiv(v, w))))
    }
}

struct MapIter<'a>(Entries<'a, 2>);

impl<'a> Iterator for MapIter<'a> {
    type Item = (&'a Value, &'a Value);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|e| (&e[0], &e[1]))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}
impl ExactSizeIterator for MapIter<'_> {}

impl Hamt<1> {
    /// The element equal to `k`, as stored.
    pub fn get(&self, k: &Value) -> Option<&Value> {
        self.find(k).map(|e| &e[0])
    }
    pub fn contains(&self, k: &Value) -> bool {
        self.find(k).is_some()
    }
    /// Add `k`; true if no equal element was there (one that was stays).
    pub fn insert(&mut self, k: Value) -> bool {
        self.put([k])
    }
    /// Remove `k`; true if it was there.
    pub fn remove(&mut self, k: &Value) -> bool {
        self.take(k)
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Value> {
        SetIter(self.entries())
    }
}

/// Sets are equal when they have equal elements.
impl PartialEq for Hamt<1> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && (self.root.identical(&other.root) || self.iter().all(|k| other.contains(k)))
    }
}

struct SetIter<'a>(Entries<'a, 1>);

impl<'a> Iterator for SetIter<'a> {
    type Item = &'a Value;
    fn next(&mut self) -> Option<&'a Value> {
        self.0.next().map(|e| &e[0])
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}
impl ExactSizeIterator for SetIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// A small deterministic generator (xorshift).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn check(m: &Map_, model: &BTreeMap<i64, i64>) {
        assert_eq!(m.len(), model.len());
        let mut seen: Vec<(i64, i64)> = m
            .iter()
            .map(|(k, v)| (k.as_fixnum().unwrap(), v.as_fixnum().unwrap()))
            .collect();
        assert_eq!(m.iter().len(), model.len());
        seen.sort_unstable();
        assert_eq!(
            seen,
            model.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>()
        );
        for (k, v) in model {
            assert_eq!(m.get(&Value::int(*k)).unwrap().as_fixnum(), Some(*v));
        }
    }

    #[test]
    fn map_matches_a_plain_map() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut m = Map_::new();
        let mut model = BTreeMap::new();
        let mut snapshots = Vec::new();
        for step in 0..60_000u32 {
            let k = (rng.next() % 20_000) as i64;
            if rng.next().is_multiple_of(3) {
                assert_eq!(m.remove(&Value::int(k)), model.remove(&k).is_some());
            } else {
                let v = (rng.next() % 1000) as i64;
                m.insert(Value::int(k), Value::int(v));
                model.insert(k, v);
            }
            assert_eq!(m.len(), model.len());
            if step % 7919 == 0 {
                check(&m, &model);
                snapshots.push((m, model.clone()));
            }
        }
        check(&m, &model);
        assert!(!m.contains_key(&Value::int(-5)));
        // Earlier versions are untouched by everything that came after.
        for (m, model) in &snapshots {
            check(m, model);
        }
        // Removing everything leaves nothing behind.
        for k in model.keys() {
            assert!(m.remove(&Value::int(*k)));
        }
        assert!(m.is_empty());
        assert!(m.root.is_nil());
        assert_eq!(m.iter().count(), 0);
    }

    #[test]
    fn set_matches_a_plain_set() {
        let mut rng = Rng(42);
        let mut s = Set_::new();
        let mut model = BTreeSet::new();
        for _ in 0..30_000 {
            let k = (rng.next() % 5_000) as i64;
            if rng.next().is_multiple_of(4) {
                assert_eq!(s.remove(&Value::int(k)), model.remove(&k));
            } else {
                assert_eq!(s.insert(Value::int(k)), model.insert(k));
            }
        }
        assert_eq!(s.len(), model.len());
        let mut seen: Vec<i64> = s.iter().map(|x| x.as_fixnum().unwrap()).collect();
        seen.sort_unstable();
        assert_eq!(seen, model.iter().copied().collect::<Vec<_>>());
        assert!(model.iter().all(|k| s.contains(&Value::int(*k))));
    }

    #[test]
    fn keys_with_one_hash_share_a_collision_node() {
        FORCED_HASH.set(Some(0xdead_beef_0123_4567));
        let mut m = Map_::new();
        let mut model = BTreeMap::new();
        for i in 0..40 {
            m.insert(Value::int(i), Value::int(i * 10));
            model.insert(i, i * 10);
        }
        m.insert(Value::int(3), Value::int(99));
        model.insert(3, 99);
        check(&m, &model);
        assert!(m.get(&Value::int(77)).is_none());
        assert!(!m.remove(&Value::int(77)));
        for i in (0..40).step_by(3) {
            assert!(m.remove(&Value::int(i)));
            model.remove(&i);
        }
        check(&m, &model);
        for i in model.keys() {
            assert!(m.remove(&Value::int(*i)));
        }
        assert!(m.is_empty() && m.root.is_nil());
        FORCED_HASH.set(None);
    }
}
