//! Core builtins: Rust functions with the uniform calling convention, registered into
//! their namespaces at start-up and callable as first-class fns with zero glue.
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::obj::func::Entry;
use crate::obj::{self, Kind};
use crate::value::Value;

pub mod arith;
pub mod basic;
pub mod coll;
pub mod conc;
pub mod fns;
pub mod io;
pub mod lazy;
pub mod nsfns;
pub mod re;
pub mod string;
pub mod types;

#[derive(Debug)]
pub struct BuiltinDesc {
    pub ns: &'static str,
    pub name: &'static str,
    pub entry: Entry,
    /// Linker symbol of `entry` (the Rust function name), for direct calls from
    /// compiled code. Direct calls pass a null `this`: builtins never read it.
    pub symbol: &'static str,
    pub arity_mask: u64,
    pub variadic_min: i64,
}

/// Every builtin, across all core modules.
pub fn all() -> impl Iterator<Item = &'static BuiltinDesc> {
    arith::TABLE
        .iter()
        .chain(basic::TABLE.iter())
        .chain(coll::TABLE.iter())
        .chain(fns::TABLE.iter())
        .chain(io::TABLE.iter())
        .chain(string::TABLE.iter())
        .chain(string::core_fns::TABLE.iter())
        .chain(lazy::TABLE.iter())
        .chain(nsfns::TABLE.iter())
        .chain(re::TABLE.iter())
        .chain(types::TABLE.iter())
        .chain(conc::TABLE.iter())
}

/// Look a builtin up by namespace and name (indexed once; the analyzer asks for
/// every var call it compiles).
pub fn find(ns: &str, name: &str) -> Option<&'static BuiltinDesc> {
    type Index = HashMap<&'static str, HashMap<&'static str, &'static BuiltinDesc>>;
    static INDEX: OnceLock<Index> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let mut m: Index = HashMap::new();
        for d in all() {
            m.entry(d.ns).or_default().insert(d.name, d);
        }
        m
    });
    index.get(ns)?.get(name).copied()
}

/// Define native functions with the uniform ABI (no namespace registration). Each
/// entry is `"display-name" => fn rust_ident(args) [fixed arities ; variadic-min] { body }`
/// where the body evaluates to `RtResult<Value>`; `this` names the fn object itself,
/// and `; owned = taker` binds a [`Taker`] for bodies that consume an argument.
/// Arguments are borrowed views; those the caller handed over (`owned` bits) are
/// released after the body unless the body took them.
#[macro_export]
macro_rules! native_fns {
    ($( $name:expr => fn $ident:ident ( $args:ident $(, $this:ident)? $(; owned = $taker:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $(
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $ident(
                _this: *const $crate::obj::func::FnObj,
                argc: i64,
                argv: *const $crate::value::Value,
                owned: u64,
            ) -> $crate::value::Value {
                const MASK: u64 = 0 $(| (1u64 << $fixed))*;
                const VMIN: i64 = -1 $(+ 1 + $vmin)?;
                let __taker = $crate::core::Taker::new(argv, argc, owned);
                let r: std::thread::Result<$crate::exc::RtResult<$crate::value::Value>> =
                    if !$crate::core::accepts(MASK, VMIN, argc) {
                        Ok(Err($crate::exc::Exc_from($crate::core::arity_error($name, argc))))
                    } else {
                        let $args: &[$crate::value::VRef] = if argc == 0 {
                            &[]
                        } else {
                            unsafe { std::slice::from_raw_parts(argv as *const $crate::value::VRef, argc as usize) }
                        };
                        let _ = &$args;
                        $( let $this = $crate::value::VRef::from_raw(_this as u64); )?
                        $( let $taker = &__taker; )?
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            || -> $crate::exc::RtResult<$crate::value::Value> { $body },
                        ))
                    };
                unsafe { $crate::core::release_owned(argv, argc, owned & !__taker.taken()) };
                $crate::core::finish($name, r)
            }
        )*
    };
}

/// Release the arguments a caller handed over (bits of `owned`; the caller clears
/// the bits of slots a body took).
///
/// # Safety
/// `argv` must point at `argc` values; owned slots must not be used afterwards.
pub unsafe fn release_owned(argv: *const Value, argc: i64, owned: u64) {
    if owned == 0 {
        return;
    }
    for i in 0..argc.min(64) {
        if owned >> i & 1 == 1 {
            drop(unsafe { std::ptr::read(argv.add(i as usize)) });
        }
    }
}

/// Lets a builtin take ownership of an argument it will consume (an owned slot's
/// reference is moved out and the slot is excluded from the release after the body;
/// a borrowed one is retained), so a sequence passed by a caller that no longer
/// needs it is freed as the builtin walks it. The slot itself is never written, so
/// the body's `&[VRef]` view stays valid; a taken slot must not be read again.
pub struct Taker {
    argv: *const Value,
    argc: i64,
    owned: u64,
    taken: std::cell::Cell<u64>,
}

impl Taker {
    pub fn new(argv: *const Value, argc: i64, owned: u64) -> Taker {
        Taker {
            argv,
            argc,
            owned,
            taken: std::cell::Cell::new(0),
        }
    }

    pub fn take(&self, i: usize) -> Value {
        assert!((i as i64) < self.argc, "take: argument index out of range");
        unsafe {
            let slot = self.argv.add(i);
            if i < 64 && self.owned >> i & 1 == 1 {
                self.taken.set(self.taken.get() | 1 << i);
                std::ptr::read(slot)
            } else {
                (*slot).clone()
            }
        }
    }

    /// Bits of the owned slots the body took over.
    pub fn taken(&self) -> u64 {
        self.taken.get()
    }
}

/// Define builtins registered as vars in a namespace (`clojure.core` by default):
/// `builtins! { ns = "clojure.string"; "upper-case" => fn ... }`.
/// `[1, 2]` accepts 1 or 2 args; `[; 0]` is variadic from 0; `[1; 3]` accepts 1 or 3+.
#[macro_export]
macro_rules! builtins {
    (ns = $ns:literal; $( $name:literal => fn $ident:ident ( $args:ident $(, $this:ident)? $(; owned = $taker:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $crate::native_fns! {
            $( concat!($ns, "/", $name) => fn $ident ( $args $(, $this)? $(; owned = $taker)? ) [ $($fixed),* $(; $vmin)? ] $body )*
        }
        pub static TABLE: &[$crate::core::BuiltinDesc] = &[
            $( $crate::core::BuiltinDesc {
                ns: $ns,
                name: $name,
                entry: $ident,
                symbol: stringify!($ident),
                arity_mask: 0 $(| (1u64 << $fixed))*,
                variadic_min: -1 $(+ 1 + $vmin)?,
            }, )*
        ];
    };
    ($( $name:literal => fn $ident:ident ( $args:ident $(, $this:ident)? $(; owned = $taker:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $crate::builtins! { ns = "clojure.core"; $( $name => fn $ident ( $args $(, $this)? $(; owned = $taker)? ) [ $($fixed),* $(; $vmin)? ] $body )* }
    };
}

#[inline]
pub fn accepts(mask: u64, vmin: i64, argc: i64) -> bool {
    (argc < 64 && mask & (1u64 << argc) != 0) || (vmin >= 0 && argc >= vmin)
}

pub fn arity_error(name: &str, argc: i64) -> Value {
    crate::exc::throw_new(
        obj::exception::ARITY,
        format!("Wrong number of args ({argc}) passed to: {name}"),
    );
    Value::EXC
}

/// Turn a builtin body's outcome into the FFI return value.
pub fn finish(name: &str, r: std::thread::Result<crate::exc::RtResult<Value>>) -> Value {
    match r {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => Value::EXC,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            crate::exc::throw_new(
                obj::exception::RUNTIME,
                format!("internal error in {name}: {msg}"),
            );
            Value::EXC
        }
    }
}

/// Clojure-style type name used in error messages (`nil cannot be cast to Number`).
pub fn type_name(v: &Value) -> &'static str {
    if v.is_nil() {
        return "nil";
    }
    if v.as_bool().is_some() {
        return "Boolean";
    }
    if v.is_fixnum() {
        return "Long";
    }
    if v.as_char().is_some() {
        return "Character";
    }
    match v.kind() {
        Some(Kind::Long) => "Long",
        Some(Kind::Double) => "Double",
        Some(Kind::String) => "String",
        Some(Kind::Symbol) => "Symbol",
        Some(Kind::Keyword) => "Keyword",
        Some(Kind::List) => "PersistentList",
        Some(Kind::Vector) => "PersistentVector",
        Some(Kind::Map) => "PersistentHashMap",
        Some(Kind::Set) => "PersistentHashSet",
        Some(Kind::Fn) => "Fn",
        Some(Kind::Var) => "Var",
        Some(Kind::Exception) => "Exception",
        Some(Kind::Atom) => "Atom",
        Some(Kind::LazySeq) => "LazySeq",
        Some(Kind::Namespace) => "Namespace",
        Some(Kind::Cons) => "Cons",
        Some(Kind::Regex) => "Pattern",
        Some(Kind::Native) => "Native",
        Some(Kind::Type) => "Type",
        Some(Kind::Protocol) => "Protocol",
        Some(Kind::SortedMap) => "PersistentTreeMap",
        Some(Kind::SortedSet) => "PersistentTreeSet",
        Some(Kind::Pending) => obj::pending::get(v).flavor.name(),
        Some(Kind::Instance) => {
            // SAFETY: type objects are immortal, so their names live for the process.
            unsafe { &*(&*obj::types::inst(v).ty().name as *const str) }
        }
        None => "Object",
    }
}

/// Helpers for builtin bodies.
pub mod args {
    use crate::exc::{RtResult, err};
    use crate::obj::exception::CLASS_CAST;
    use crate::obj::string;
    use crate::value::Value;

    pub fn int(v: &Value, what: &str) -> RtResult<i64> {
        v.as_int().map_or_else(
            || {
                err(
                    CLASS_CAST,
                    format!("{} cannot be cast to Long ({what})", super::type_name(v)),
                )
            },
            Ok,
        )
    }

    pub fn string<'a>(v: &'a Value, what: &str) -> RtResult<&'a str> {
        string::as_str(v).map_or_else(
            || {
                err(
                    CLASS_CAST,
                    format!("{} cannot be cast to String ({what})", super::type_name(v)),
                )
            },
            Ok,
        )
    }
}
