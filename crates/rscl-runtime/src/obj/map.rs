//! Persistent hash map backed by `imbl::HashMap`. By default the hasher is
//! deterministic, so printing order is identical across processes (the JIT/AOT
//! corpus tests rely on it) and matches Clojure's fixed hash functions. A service that
//! keys maps by attacker-controlled data can set `RSCL_HASH_SEED=random` (or a
//! number) to mix keys with a per-process secret, which defeats precomputed hash
//! collisions at the cost of a stable iteration order.
use std::hash::{BuildHasher, DefaultHasher, RandomState};
use std::sync::OnceLock;

use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

/// The process-wide seed from `RSCL_HASH_SEED`: `None` for the deterministic hasher.
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

#[derive(Clone, Default)]
pub struct Hasher;

pub enum Mixer {
    Fixed(DefaultHasher),
    Keyed { h: u64, key: u64 },
}

impl BuildHasher for Hasher {
    type Hasher = Mixer;
    fn build_hasher(&self) -> Mixer {
        match seed() {
            None => Mixer::Fixed(DefaultHasher::new()),
            Some(key) => Mixer::Keyed { h: key, key },
        }
    }
}

impl std::hash::Hasher for Mixer {
    fn finish(&self) -> u64 {
        match self {
            Mixer::Fixed(h) => h.finish(),
            Mixer::Keyed { h, .. } => *h,
        }
    }
    fn write(&mut self, bytes: &[u8]) {
        match self {
            Mixer::Fixed(h) => h.write(bytes),
            Mixer::Keyed { h, key } => {
                for b in bytes {
                    *h = crate::equiv::mix64(
                        (*h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3) ^ *key,
                    );
                }
            }
        }
    }
    fn write_u64(&mut self, x: u64) {
        match self {
            Mixer::Fixed(h) => h.write_u64(x),
            Mixer::Keyed { h, key } => *h = crate::equiv::mix64(x ^ *key ^ h.rotate_left(17)),
        }
    }
}
pub type Map_ = imbl::GenericHashMap<Value, Value, Hasher, imbl::shared_ptr::DefaultSharedPtr>;

#[repr(C)]
pub struct MapObj {
    pub hdr: ObjHeader,
    pub m: Map_,
    pub meta: Value,
}

pub fn new(m: Map_) -> Value {
    alloc(MapObj {
        hdr: ObjHeader::new(Kind::Map),
        m,
        meta: Value::NIL,
    })
}

pub fn empty_map() -> Map_ {
    Map_::with_hasher(Hasher)
}

/// Build from alternating key/value pairs (borrowed). Later duplicates win.
pub fn from_pairs(kvs: &[Value]) -> Value {
    debug_assert!(kvs.len().is_multiple_of(2));
    let mut m = empty_map();
    for kv in kvs.chunks(2) {
        m.insert(kv[0].clone(), kv[1].clone());
    }
    new(m)
}

pub fn get(v: &Value) -> &MapObj {
    debug_assert!(v.is_kind(Kind::Map));
    unsafe { cast::<MapObj>(v) }
}

pub fn as_map(v: &Value) -> Option<&MapObj> {
    if v.is_kind(Kind::Map) {
        Some(get(v))
    } else {
        None
    }
}
