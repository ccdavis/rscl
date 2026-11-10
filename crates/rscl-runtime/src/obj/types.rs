//! User types (`deftype`, `defrecord`), their instances, and protocols.
//!
//! A `TypeObj` is immortal (redefining a type at the REPL creates a new one; old
//! instances keep the old type). Built-in kinds are represented by cached `TypeObj`s
//! too, so `(type x)` and protocol dispatch use one notion of type.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use super::{Kind, alloc, cast, keyword};
use crate::exc::{RtResult, err};
use crate::obj::exception::{ARITY, ILLEGAL_ARGUMENT};
use crate::value::{ObjHeader, Value};

pub const KEY_OBJECT: u64 = 0;
pub const KEY_NIL: u64 = 1;
pub const KEY_BOOLEAN: u64 = 2;
pub const KEY_CHARACTER: u64 = 3;
pub const KEY_LONG: u64 = 4;
pub const KEY_DOUBLE: u64 = 5;
const KEY_KIND_BASE: u64 = 100;
static NEXT_ID: AtomicU64 = AtomicU64::new(1000);

#[repr(C)]
pub struct TypeObj {
    pub hdr: ObjHeader,
    /// Dispatch key: built-in keys above, or a unique id for user types.
    pub id: u64,
    /// Printed name, e.g. `user.Point` or `String`.
    pub name: Box<str>,
    pub fields: Vec<Box<str>>,
    pub field_kws: Vec<Value>,
    pub is_record: bool,
}

fn type_value(id: u64, name: &str, fields: Vec<String>, is_record: bool) -> Value {
    let field_kws = fields.iter().map(|f| keyword::intern(None, f)).collect();
    let p = super::alloc_raw(TypeObj {
        hdr: ObjHeader::immortal(Kind::Type),
        id,
        name: name.into(),
        fields: fields.into_iter().map(Into::into).collect(),
        field_kws,
        is_record,
    });
    crate::gc::register_permanent(p as *mut ObjHeader);
    unsafe { Value::from_raw(p as u64) }
}

/// A new user type. `name` is qualified with dots (`user.Point`).
pub fn new_type(name: &str, fields: Vec<String>, is_record: bool) -> Value {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let v = type_value(id, name, fields, is_record);
    user_table()
        .write()
        .unwrap()
        .0
        .insert(id, v.bits() as usize);
    v
}

fn user_table() -> &'static RwLock<BuiltinTypes> {
    static T: OnceLock<RwLock<BuiltinTypes>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(BuiltinTypes(HashMap::new())))
}

/// The type object for any dispatch key (built-in or user).
pub fn type_by_key(key: u64) -> Value {
    if key < 1000 {
        return builtin_type(key);
    }
    match user_table().read().unwrap().0.get(&key) {
        Some(&p) => unsafe { Value::from_raw(p as u64) },
        None => builtin_type(KEY_OBJECT),
    }
}

pub fn get(v: &Value) -> &TypeObj {
    debug_assert!(v.is_kind(Kind::Type));
    unsafe { cast::<TypeObj>(v) }
}

pub fn as_type(v: &Value) -> Option<&TypeObj> {
    if v.is_kind(Kind::Type) {
        Some(get(v))
    } else {
        None
    }
}

/// Name of a built-in dispatch key.
pub fn builtin_name(key: u64) -> &'static str {
    match key {
        KEY_OBJECT => "Object",
        KEY_NIL => "nil",
        KEY_BOOLEAN => "Boolean",
        KEY_CHARACTER => "Character",
        KEY_LONG => "Long",
        KEY_DOUBLE => "Double",
        _ => match kind_of_key(key) {
            Some(Kind::String) => "String",
            Some(Kind::Symbol) => "Symbol",
            Some(Kind::Keyword) => "Keyword",
            Some(Kind::List) => "PersistentList",
            Some(Kind::Vector) => "PersistentVector",
            Some(Kind::Map) => "PersistentHashMap",
            Some(Kind::Set) => "PersistentHashSet",
            Some(Kind::SortedMap) => "PersistentTreeMap",
            Some(Kind::SortedSet) => "PersistentTreeSet",
            Some(Kind::Fn) => "Fn",
            Some(Kind::Var) => "Var",
            Some(Kind::Exception) => "Exception",
            Some(Kind::Atom) => "Atom",
            Some(Kind::LazySeq) => "LazySeq",
            Some(Kind::Cons) => "Cons",
            Some(Kind::Namespace) => "Namespace",
            Some(Kind::Regex) => "Pattern",
            Some(Kind::Native) => "Native",
            Some(Kind::Type) => "Type",
            Some(Kind::Protocol) => "Protocol",
            Some(Kind::Pending) => "Pending",
            Some(Kind::BigInt) => "BigInt",
            _ => "Object",
        },
    }
}

fn kind_of_key(key: u64) -> Option<Kind> {
    let k = key.checked_sub(KEY_KIND_BASE)? as u32;
    ALL_KINDS.iter().copied().find(|x| *x as u32 == k)
}

const ALL_KINDS: &[Kind] = &[
    Kind::Long,
    Kind::Double,
    Kind::String,
    Kind::Symbol,
    Kind::Keyword,
    Kind::List,
    Kind::Vector,
    Kind::Map,
    Kind::Set,
    Kind::Fn,
    Kind::Var,
    Kind::Exception,
    Kind::Atom,
    Kind::LazySeq,
    Kind::Namespace,
    Kind::Cons,
    Kind::Regex,
    Kind::Native,
    Kind::Type,
    Kind::Instance,
    Kind::Protocol,
    Kind::SortedMap,
    Kind::SortedSet,
    Kind::Pending,
    Kind::BigInt,
];

/// The dispatch key of any value.
pub fn type_key(v: &Value) -> u64 {
    if v.is_nil() {
        return KEY_NIL;
    }
    if v.as_bool().is_some() {
        return KEY_BOOLEAN;
    }
    if v.as_char().is_some() {
        return KEY_CHARACTER;
    }
    if v.is_fixnum() {
        return KEY_LONG;
    }
    match v.kind() {
        Some(Kind::Long) => KEY_LONG,
        Some(Kind::Double) => KEY_DOUBLE,
        Some(Kind::Instance) => inst(v).ty().id,
        Some(k) => KEY_KIND_BASE + k as u64,
        None => KEY_OBJECT,
    }
}

/// Keys of every built-in type that has a core var (`String`, `Long`, ...).
pub fn builtin_keys() -> Vec<u64> {
    let mut keys = vec![
        KEY_OBJECT,
        KEY_NIL,
        KEY_BOOLEAN,
        KEY_CHARACTER,
        KEY_LONG,
        KEY_DOUBLE,
    ];
    keys.extend(
        ALL_KINDS
            .iter()
            .filter(|k| !matches!(k, Kind::Long | Kind::Double | Kind::Instance))
            .map(|k| KEY_KIND_BASE + *k as u64),
    );
    keys
}

struct BuiltinTypes(HashMap<u64, usize>);
// SAFETY: stores addresses of immortal type objects.
unsafe impl Send for BuiltinTypes {}
unsafe impl Sync for BuiltinTypes {}

fn builtin_table() -> &'static RwLock<BuiltinTypes> {
    static T: OnceLock<RwLock<BuiltinTypes>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(BuiltinTypes(HashMap::new())))
}

/// The cached `TypeObj` for a built-in dispatch key.
pub fn builtin_type(key: u64) -> Value {
    if let Some(&p) = builtin_table().read().unwrap().0.get(&key) {
        return unsafe { Value::from_raw(p as u64) };
    }
    let mut w = builtin_table().write().unwrap();
    if let Some(&p) = w.0.get(&key) {
        return unsafe { Value::from_raw(p as u64) };
    }
    let v = type_value(key, builtin_name(key), Vec::new(), false);
    w.0.insert(key, v.bits() as usize);
    v
}

/// `(type x)`: `nil` for `nil` (as in Clojure), else the type object.
pub fn type_of(v: &Value) -> Value {
    match v.kind() {
        None if v.is_nil() => Value::NIL,
        Some(Kind::Instance) => unsafe { Value::from_raw(inst(v).ty as u64) },
        _ => builtin_type(type_key(v)),
    }
}

// ---- instances -------------------------------------------------------------------

#[repr(C)]
pub struct InstanceObj {
    pub hdr: ObjHeader,
    pub ty: *const TypeObj,
    pub fields: Vec<Value>,
    /// Records: extra keys beyond the declared fields (a map or `nil`).
    pub ext: Value,
    pub meta: Value,
}

// SAFETY: `ty` is immortal.
unsafe impl Send for InstanceObj {}
unsafe impl Sync for InstanceObj {}

impl InstanceObj {
    pub fn ty(&self) -> &TypeObj {
        unsafe { &*self.ty }
    }
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.ty().fields.iter().position(|f| &**f == name)
    }
    /// Keyword lookup over fields, then the extension map (records).
    pub fn get_kw(&self, k: &Value) -> Option<Value> {
        let ty = self.ty();
        if let Some(i) = ty.field_kws.iter().position(|fk| fk.identical(k)) {
            return Some(self.fields[i].clone());
        }
        super::map::as_map(&self.ext).and_then(|m| m.m.get(k).cloned())
    }
    /// `[k v]` entries: fields in declaration order, then the extension map.
    pub fn entries(&self) -> Vec<(Value, Value)> {
        let ty = self.ty();
        let mut out: Vec<(Value, Value)> = ty
            .field_kws
            .iter()
            .cloned()
            .zip(self.fields.iter().cloned())
            .collect();
        if let Some(m) = super::map::as_map(&self.ext) {
            out.extend(m.m.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        out
    }
    pub fn count(&self) -> usize {
        self.fields.len() + super::map::as_map(&self.ext).map_or(0, |m| m.m.len())
    }
}

pub fn instance(ty: &Value, fields: Vec<Value>, ext: Value) -> RtResult<Value> {
    let t = as_type(ty).ok_or_else(|| crate::exc::throw_new(ILLEGAL_ARGUMENT, "not a type"))?;
    if t.id < 1000 {
        return err(
            ILLEGAL_ARGUMENT,
            format!("cannot instantiate built-in type {}", t.name),
        );
    }
    if fields.len() != t.fields.len() {
        return err(
            ARITY,
            format!(
                "Wrong number of args ({}) passed to: {}",
                fields.len(),
                t.name
            ),
        );
    }
    Ok(alloc(InstanceObj {
        hdr: ObjHeader::new(Kind::Instance),
        ty: t as *const TypeObj,
        fields,
        ext,
        meta: Value::NIL,
    }))
}

pub fn inst(v: &Value) -> &InstanceObj {
    debug_assert!(v.is_kind(Kind::Instance));
    unsafe { cast::<InstanceObj>(v) }
}

pub fn as_instance(v: &Value) -> Option<&InstanceObj> {
    if v.is_kind(Kind::Instance) {
        Some(inst(v))
    } else {
        None
    }
}

pub fn is_record(v: &Value) -> bool {
    as_instance(v).is_some_and(|i| i.ty().is_record)
}

/// A copy with `fields`/`ext` replaced (keeps type and metadata).
pub fn with(v: &Value, fields: Vec<Value>, ext: Value) -> Value {
    let i = inst(v);
    alloc(InstanceObj {
        hdr: ObjHeader::new(Kind::Instance),
        ty: i.ty,
        fields,
        ext,
        meta: i.meta.clone(),
    })
}

// ---- protocols ---------------------------------------------------------------------

#[repr(C)]
pub struct ProtocolObj {
    pub hdr: ObjHeader,
    pub name: Box<str>,
    pub methods: Vec<Box<str>>,
    impls: RwLock<HashMap<u64, HashMap<String, Value>>>,
}

pub fn protocol_new(name: &str, methods: Vec<String>) -> Value {
    let p = super::alloc_raw(ProtocolObj {
        hdr: ObjHeader::immortal(Kind::Protocol),
        name: name.into(),
        methods: methods.into_iter().map(Into::into).collect(),
        impls: RwLock::new(HashMap::new()),
    });
    crate::gc::register_permanent(p as *mut ObjHeader);
    unsafe { Value::from_raw(p as u64) }
}

/// Visit every method implementation (the world is stopped, so the lock is
/// bypassed).
///
/// # Safety
/// `p` is a protocol and no other thread runs rscl code.
pub(crate) unsafe fn gc_trace_protocol(p: *mut ProtocolObj, f: &mut dyn FnMut(&Value)) {
    let o = unsafe { &mut *p };
    for methods in o
        .impls
        .get_mut()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        for v in methods.values() {
            f(v);
        }
    }
}

pub fn proto(v: &Value) -> &ProtocolObj {
    debug_assert!(v.is_kind(Kind::Protocol));
    unsafe { cast::<ProtocolObj>(v) }
}

pub fn as_protocol(v: &Value) -> Option<&ProtocolObj> {
    if v.is_kind(Kind::Protocol) {
        Some(proto(v))
    } else {
        None
    }
}

impl ProtocolObj {
    pub fn extend(&self, key: u64, method: &str, f: Value) {
        self.impls
            .write()
            .unwrap()
            .entry(key)
            .or_default()
            .insert(method.to_string(), f);
    }
    /// The implementation for `key`, falling back to `Object` (`nil` is not an
    /// `Object`, as in Clojure).
    pub fn find(&self, key: u64, method: &str) -> Option<Value> {
        let impls = self.impls.read().unwrap();
        impls
            .get(&key)
            .and_then(|m| m.get(method))
            .or_else(|| {
                (key != KEY_NIL)
                    .then(|| impls.get(&KEY_OBJECT).and_then(|m| m.get(method)))
                    .flatten()
            })
            .cloned()
    }
    pub fn extends(&self, key: u64) -> bool {
        self.impls.read().unwrap().contains_key(&key)
    }
    pub fn satisfied_by(&self, key: u64) -> bool {
        let impls = self.impls.read().unwrap();
        impls.contains_key(&key) || (key != KEY_NIL && impls.contains_key(&KEY_OBJECT))
    }
    pub fn extenders(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.impls.read().unwrap().keys().copied().collect();
        v.sort();
        v
    }
}
