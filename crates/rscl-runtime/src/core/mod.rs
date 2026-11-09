//! Core builtins: Rust functions with the uniform calling convention, registered into
//! their namespaces at start-up and callable as first-class fns with zero glue.
use crate::obj::func::Entry;
use crate::obj::{self, Kind};
use crate::value::Value;

pub mod arith;
pub mod basic;
pub mod coll;
pub mod fns;
pub mod io;
pub mod lazy;
pub mod nsfns;
pub mod re;
pub mod string;

pub struct BuiltinDesc {
    pub ns: &'static str,
    pub name: &'static str,
    pub entry: Entry,
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
}

pub fn find(ns: &str, name: &str) -> Option<&'static BuiltinDesc> {
    all().find(|d| d.ns == ns && d.name == name)
}

/// Define native functions with the uniform ABI (no namespace registration). Each
/// entry is `"display-name" => fn rust_ident(args) [fixed arities ; variadic-min] { body }`
/// where the body evaluates to `RtResult<Value>`; `this` names the fn object itself.
#[macro_export]
macro_rules! native_fns {
    ($( $name:expr => fn $ident:ident ( $args:ident $(, $this:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $(
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $ident(
                _this: *const $crate::obj::func::FnObj,
                argc: i64,
                argv: *const $crate::value::Value,
            ) -> $crate::value::Value {
                const MASK: u64 = 0 $(| (1u64 << $fixed))*;
                const VMIN: i64 = -1 $(+ 1 + $vmin)?;
                if !$crate::core::accepts(MASK, VMIN, argc) {
                    return $crate::core::arity_error($name, argc);
                }
                let $args: &[$crate::value::VRef] = if argc == 0 {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(argv as *const $crate::value::VRef, argc as usize) }
                };
                $( let $this = $crate::value::VRef::from_raw(_this as u64); )?
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> $crate::exc::RtResult<$crate::value::Value> { $body },
                ));
                $crate::core::finish($name, r)
            }
        )*
    };
}

/// Define builtins registered as vars in a namespace (`clojure.core` by default):
/// `builtins! { ns = "clojure.string"; "upper-case" => fn ... }`.
/// `[1, 2]` accepts 1 or 2 args; `[; 0]` is variadic from 0; `[1; 3]` accepts 1 or 3+.
#[macro_export]
macro_rules! builtins {
    (ns = $ns:literal; $( $name:literal => fn $ident:ident ( $args:ident $(, $this:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $crate::native_fns! {
            $( concat!($ns, "/", $name) => fn $ident ( $args $(, $this)? ) [ $($fixed),* $(; $vmin)? ] $body )*
        }
        pub static TABLE: &[$crate::core::BuiltinDesc] = &[
            $( $crate::core::BuiltinDesc {
                ns: $ns,
                name: $name,
                entry: $ident,
                arity_mask: 0 $(| (1u64 << $fixed))*,
                variadic_min: -1 $(+ 1 + $vmin)?,
            }, )*
        ];
    };
    ($( $name:literal => fn $ident:ident ( $args:ident $(, $this:ident)? ) [ $($fixed:literal),* $(; $vmin:literal)? ] $body:block )*) => {
        $crate::builtins! { ns = "clojure.core"; $( $name => fn $ident ( $args $(, $this)? ) [ $($fixed),* $(; $vmin)? ] $body )* }
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
