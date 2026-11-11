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
/// Arguments are borrowed views.
///
/// A native defers collections while it runs (it may hold values in Rust heap data
/// the collector cannot see, and call back into rscl code meanwhile). Writing `@gc
/// fn` instead of `fn` declares that a collection may run under it: then every
/// value it holds across a call into rscl code (directly, or by realising a lazy
/// seq, comparing, hashing or printing) must be on its stack, in its arguments, or
/// rooted with `gc::root_slice`.
#[macro_export]
macro_rules! native_fns {
    ($( $name:expr => $(@ $safe:ident)? fn $ident:ident ( $args:ident $(, $this:ident)? $(; owned = $taker:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $(
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $ident(
                this: *const $crate::obj::func::FnObj,
                argc: i64,
                argv: *const $crate::value::Value,
                owned: u64,
            ) -> $crate::value::Value {
                #[inline(never)]
                unsafe extern "C" fn body(
                    _this: *const $crate::obj::func::FnObj,
                    argc: i64,
                    argv: *const $crate::value::Value,
                    owned: u64,
                ) -> $crate::value::Value {
                    const MASK: u64 = 0 $(| (1u64 << $fixed))*;
                    const VMIN: i64 = -1 $(+ 1 + $vmin)?;
                    let __taker = $crate::core::Taker::new(argv, argc, owned);
                    $crate::__native_hold!($($safe)?; argv, argc, _this; $($this)?);
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
                    $crate::core::finish($name, r)
                }
                // A native that walks a sequence it was handed gets a clean stack to
                // work on: its frame is long-lived, and leftovers of the calls that
                // produced the sequence must not show through it as references.
                $( let _ = stringify!($taker); unsafe { $crate::core::clear_stack_for_stream(argv, argc, owned) }; )?
                unsafe { body(this, argc, argv, owned) }
            }
        )*
    };
}

/// Before a native starts to walk a lazy sequence its caller gave up: zero the dead
/// stack below (see `gc::clear_dead_stack`).
///
/// # Safety
/// `argv` must point at `argc` values.
#[inline]
pub unsafe fn clear_stack_for_stream(argv: *const Value, argc: i64, owned: u64) {
    if owned == 0 {
        return;
    }
    for i in 0..argc.clamp(0, 64) as usize {
        if owned >> i & 1 == 1 && crate::coll::is_lazy_kind(unsafe { &*argv.add(i) }) {
            crate::gc::clear_dead_stack();
            return;
        }
    }
}

/// Expands to a collection hold for an ordinary native, and to rooting the argument
/// list for a `gc` one.
#[doc(hidden)]
#[macro_export]
macro_rules! __native_hold {
    (; $argv:ident, $argc:ident, $fnobj:ident; $($this:ident)?) => {
        let _hold = $crate::gc::hold();
    };
    // A native under which a collection may run keeps its arguments reachable (its
    // caller's argument slots are not roots by themselves).
    (gc; $argv:ident, $argc:ident, $fnobj:ident;) => {
        let __args = $crate::gc::ArgRoot::new($argv, $argc);
        let _rooted = __args.enter();
    };
    // One that is a closure keeps itself reachable too, and through itself what it
    // captured: the caller may have dropped the fn object the moment it called it.
    (gc; $argv:ident, $argc:ident, $fnobj:ident; $this:ident) => {
        let __args = $crate::gc::ArgRoot::new($argv, $argc);
        let _rooted = __args.enter();
        let __self_word = [$fnobj as u64];
        let __self =
            $crate::gc::ArgRoot::new(__self_word.as_ptr() as *const $crate::value::Value, 1);
        let _self_rooted = __self.enter();
    };
}

/// Lets a builtin take over an argument it will consume. A caller sets a slot's
/// `owned` bit when it has no further use for that argument; taking such a slot
/// clears it, so the caller's argument area stops referring to the value. That is
/// what lets a long lazy sequence be collected while a builtin walks it: otherwise
/// the slot would keep the head, and with it every realised cell, alive for the
/// whole call. A taken slot must not be read again through the `args` view.
pub struct Taker {
    argv: *const Value,
    argc: i64,
    owned: u64,
}

impl Taker {
    pub fn new(argv: *const Value, argc: i64, owned: u64) -> Taker {
        Taker { argv, argc, owned }
    }

    /// A cursor over argument `i` for a sequence walk: the slot itself when the
    /// caller gave it up, a copy otherwise (the caller keeps the head anyway).
    pub fn cursor(&self, i: usize) -> crate::coll::Cursor {
        assert!(
            (i as i64) < self.argc,
            "cursor: argument index out of range"
        );
        unsafe {
            let slot = self.argv.add(i) as *mut Value;
            if i < 64 && self.owned >> i & 1 == 1 {
                crate::coll::Cursor::Slot(slot)
            } else {
                crate::coll::Cursor::Own(std::ptr::read(slot))
            }
        }
    }

    pub fn take(&self, i: usize) -> Value {
        assert!((i as i64) < self.argc, "take: argument index out of range");
        unsafe {
            let slot = self.argv.add(i) as *mut Value;
            let v = std::ptr::read_volatile(slot);
            if i < 64 && self.owned >> i & 1 == 1 {
                // The caller gave the slot up: it is writable memory of its frame.
                std::ptr::write_volatile(slot, Value::NIL);
            }
            v
        }
    }
}

/// Define builtins registered as vars in a namespace (`clojure.core` by default):
/// `builtins! { ns = "clojure.string"; "upper-case" => fn ... }`.
/// `[1, 2]` accepts 1 or 2 args; `[; 0]` is variadic from 0; `[1; 3]` accepts 1 or 3+.
#[macro_export]
macro_rules! builtins {
    (ns = $ns:literal; $( $name:literal => $(@ $safe:ident)? fn $ident:ident ( $args:ident $(, $this:ident)? $(; owned = $taker:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $crate::native_fns! {
            $( concat!($ns, "/", $name) => $(@ $safe)? fn $ident ( $args $(, $this)? $(; owned = $taker)? ) [ $($fixed),* $(; $vmin)? ] $body )*
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
    ($( $name:literal => $(@ $safe:ident)? fn $ident:ident ( $args:ident $(, $this:ident)? $(; owned = $taker:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $crate::builtins! { ns = "clojure.core"; $( $name => $(@ $safe)? fn $ident ( $args $(, $this)? $(; owned = $taker)? ) [ $($fixed),* $(; $vmin)? ] $body )* }
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
#[inline(always)]
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
        Some(Kind::BigInt) => "BigInt",
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
        v.int_value().map_or_else(
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
