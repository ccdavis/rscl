//! Write Rust, call it from rscl.
//!
//! ```ignore
//! rscl_bridge::exports! {
//!     ns "demo";
//!     fn add(a: i64, b: i64) -> i64 { a + b }
//!     fn greet(name: &str) -> String { format!("Hello, {name}") }
//!     fn parse_int(s: &str) -> Result<i64, String> { s.parse().map_err(|e| format!("{e}")) }
//! }
//! ```
//!
//! Build the crate as a `cdylib`; `(load-native "target/debug/libdemo.so" :as d)` then
//! gives `(d/add 1 2)`, `(d/greet "x")`, `(d/parse-int "7")` (underscores become
//! hyphens). The library never links the rscl runtime: it receives an `RtApi` table of
//! function pointers at registration and does everything through it, so the process
//! has one runtime. Values are converted with [`FromVal`] / [`IntoVal`]; `Result<T,
//! String>` turns `Err` into an rscl exception, and a panic becomes one too (the crate
//! must keep the default `panic = "unwind"`).
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;

pub use rscl_abi::{API_VERSION, Entry, RtApi};

static API: OnceLock<&'static RtApi> = OnceLock::new();

/// Identifies this library instance: every loaded copy of a cdylib has its own static,
/// so native objects created by one copy are never reinterpreted by another.
static LIB_TOKEN: u8 = 0;

fn lib_token() -> u64 {
    &LIB_TOKEN as *const u8 as u64
}

/// The runtime's API table. Panics before registration.
pub fn api() -> &'static RtApi {
    API.get()
        .expect("rscl bridge used before rscl_module_register")
}

/// An exception is pending in the runtime (already thrown through the API). The only
/// way to get one is from `throw` or a failed API call, so holding an `Exc` means the
/// runtime has something pending; it is tied to the thread it was thrown on.
#[derive(Debug, Clone, Copy)]
pub struct Exc(std::marker::PhantomData<*const ()>);

impl Exc {
    fn pending() -> Exc {
        Exc(std::marker::PhantomData)
    }
}

/// Drop the pending exception: for code that handles an `Err(Exc)` itself (falls
/// back to a default, retries) instead of returning it to rscl.
pub fn clear_exception(_handled: Exc) {
    unsafe { (api().exc_clear)() }
}

pub type Result<T> = std::result::Result<T, Exc>;

/// Throw a new exception of `class` with `message`.
pub fn throw(class: &str, message: &str) -> Exc {
    unsafe {
        (api().throw)(
            class.as_ptr(),
            class.len() as i64,
            message.as_ptr(),
            message.len() as i64,
        )
    };
    Exc::pending()
}

/// An owned reference to an rscl value. It must stay on the thread rscl called the
/// library on (it is neither `Send` nor `Sync`): the runtime's collector and its
/// pending-exception slot only know about threads the runtime itself started.
#[repr(transparent)]
pub struct Val(u64, std::marker::PhantomData<*const ()>);

const fn val(bits: u64) -> Val {
    Val(bits, std::marker::PhantomData)
}

impl Val {
    /// Wrap raw bits, taking over one reference.
    ///
    /// # Safety
    /// `bits` must be a value word the caller owns one reference to (as returned by
    /// an `RtApi` function that hands out `+1` results).
    pub unsafe fn from_raw(bits: u64) -> Val {
        val(bits)
    }
    pub fn into_raw(self) -> u64 {
        let b = self.0;
        std::mem::forget(self);
        b
    }
    pub fn bits(&self) -> u64 {
        self.0
    }
    pub fn nil() -> Val {
        val(api().nil)
    }
    pub fn is_nil(&self) -> bool {
        unsafe { (api().is_nil)(self.0) != 0 }
    }
    pub fn truthy(&self) -> bool {
        unsafe { (api().truthy)(self.0) != 0 }
    }
    pub fn type_name(&self) -> &'static str {
        let (mut p, mut n): (*const u8, i64) = (std::ptr::null(), 0);
        unsafe {
            (api().type_name)(self.0, &mut p, &mut n);
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(p, n as usize))
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        let mut n = 0i64;
        unsafe { ((api().int_get)(self.0, &mut n) != 0).then_some(n) }
    }
    pub fn as_f64(&self) -> Option<f64> {
        let mut d = 0f64;
        unsafe { ((api().double_get)(self.0, &mut d) != 0).then_some(d) }
    }
    pub fn as_str(&self) -> Option<&str> {
        let (mut p, mut n): (*const u8, i64) = (std::ptr::null(), 0);
        unsafe {
            if (api().string_get)(self.0, &mut p, &mut n) == 0 {
                return None;
            }
            Some(std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                p, n as usize,
            )))
        }
    }
    /// The name of a keyword or symbol.
    pub fn named(&self) -> Option<&str> {
        let (mut p, mut n): (*const u8, i64) = (std::ptr::null(), 0);
        unsafe {
            if (api().named_name)(self.0, &mut p, &mut n) == 0 {
                return None;
            }
            Some(std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                p, n as usize,
            )))
        }
    }
    /// Call this value as a function.
    pub fn call(&self, args: &[Val]) -> Result<Val> {
        let r = unsafe { (api().invoke)(self.0, args.len() as i64, args.as_ptr() as *const u64) };
        check(r)
    }
    /// `pr-str`.
    pub fn pr_str(&self) -> String {
        let s = unsafe { val((api().pr_str)(self.0)) };
        s.as_str().unwrap_or("").to_string()
    }
    /// Materialise a seqable value as a vector of owned elements.
    pub fn to_vec(&self) -> Result<Vec<Val>> {
        let v = check(unsafe { (api().to_vector)(self.0) })?;
        let n = unsafe { (api().vector_len)(v.0) };
        Ok((0..n)
            .map(|i| unsafe { val((api().vector_get)(v.0, i)) })
            .collect())
    }
    pub fn vector(items: Vec<Val>) -> Val {
        // `vector_new` retains its elements; `items` releases its own references
        // when it drops. `Val` is a transparent `u64`.
        unsafe {
            val((api().vector_new)(
                items.as_ptr() as *const u64,
                items.len() as i64,
            ))
        }
    }
    pub fn keyword(ns: Option<&str>, name: &str) -> Val {
        unsafe {
            val((api().keyword_new)(
                ns.map_or(std::ptr::null(), str::as_ptr),
                ns.map_or(0, |s| s.len() as i64),
                name.as_ptr(),
                name.len() as i64,
            ))
        }
    }
    pub fn string(s: &str) -> Val {
        unsafe { val((api().string_new)(s.as_ptr(), s.len() as i64)) }
    }
    pub fn int(n: i64) -> Val {
        unsafe { val((api().int_new)(n)) }
    }
    pub fn double(d: f64) -> Val {
        unsafe { val((api().double_new)(d)) }
    }
    pub fn boolean(b: bool) -> Val {
        unsafe { val((api().bool_new)(b as i32)) }
    }
}

impl Clone for Val {
    fn clone(&self) -> Val {
        unsafe { (api().retain)(self.0) };
        val(self.0)
    }
}

impl Drop for Val {
    fn drop(&mut self) {
        unsafe { (api().release)(self.0) }
    }
}

fn check(r: u64) -> Result<Val> {
    if r == api().exc {
        Err(Exc::pending())
    } else {
        Ok(val(r))
    }
}

/// Convert an rscl argument into a Rust value. `'a` is the lifetime of the argument,
/// so borrowed types such as `&str` can be produced without copying.
pub trait FromVal<'a>: Sized {
    fn from_val(v: &'a Val, what: &str) -> Result<Self>;
}

/// Convert a Rust return value into an rscl value.
pub trait IntoVal {
    fn into_val(self) -> Result<Val>;
}

fn cast_err<T>(v: &Val, what: &str, expected: &str) -> Result<T> {
    Err(throw(
        "ClassCastException",
        &format!(
            "{} cannot be cast to {expected} (argument {what})",
            v.type_name()
        ),
    ))
}

impl<'a> FromVal<'a> for Val {
    fn from_val(v: &'a Val, _: &str) -> Result<Self> {
        Ok(v.clone())
    }
}
impl<'a> FromVal<'a> for &'a Val {
    fn from_val(v: &'a Val, _: &str) -> Result<Self> {
        Ok(v)
    }
}
impl<'a> FromVal<'a> for i64 {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        v.as_i64().map_or_else(|| cast_err(v, what, "i64"), Ok)
    }
}
impl<'a> FromVal<'a> for i32 {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        let n = i64::from_val(v, what)?;
        i32::try_from(n).map_err(|_| {
            throw(
                "IllegalArgumentException",
                &format!("{n} does not fit in i32 (argument {what})"),
            )
        })
    }
}
impl<'a> FromVal<'a> for u32 {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        let n = i64::from_val(v, what)?;
        u32::try_from(n).map_err(|_| {
            throw(
                "IllegalArgumentException",
                &format!("{n} does not fit in u32 (argument {what})"),
            )
        })
    }
}
impl<'a> FromVal<'a> for usize {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        let n = i64::from_val(v, what)?;
        usize::try_from(n).map_err(|_| {
            throw(
                "IllegalArgumentException",
                &format!("{n} is not a valid size (argument {what})"),
            )
        })
    }
}
impl<'a> FromVal<'a> for f64 {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        v.as_f64().map_or_else(|| cast_err(v, what, "f64"), Ok)
    }
}
impl<'a> FromVal<'a> for f32 {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        f64::from_val(v, what).map(|d| d as f32)
    }
}
impl<'a> FromVal<'a> for bool {
    fn from_val(v: &'a Val, _: &str) -> Result<Self> {
        Ok(v.truthy())
    }
}
impl<'a> FromVal<'a> for &'a str {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        v.as_str().map_or_else(|| cast_err(v, what, "String"), Ok)
    }
}
impl<'a> FromVal<'a> for String {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        <&str>::from_val(v, what).map(str::to_string)
    }
}
impl<'a, T: FromVal<'a>> FromVal<'a> for Option<T> {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        if v.is_nil() {
            Ok(None)
        } else {
            T::from_val(v, what).map(Some)
        }
    }
}
impl<'a, T: for<'b> FromVal<'b>> FromVal<'a> for Vec<T> {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        let items = v.to_vec()?;
        items.iter().map(|x| T::from_val(x, what)).collect()
    }
}
/// `nil` is the empty map (as it is the empty vector for `Vec<T>`); keys may be strings
/// or keywords.
impl<'a, V: for<'b> FromVal<'b>> FromVal<'a> for HashMap<String, V> {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        if v.is_nil() {
            return Ok(HashMap::new());
        }
        // `map_entries` gives a vector of `[key value]` vectors: read it in place.
        let entries = check(unsafe { (api().map_entries)(v.0) })?;
        let n = unsafe { (api().vector_len)(entries.0) };
        let mut out = HashMap::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let (k, x) = unsafe {
                let e = val((api().vector_get)(entries.0, i));
                (
                    val((api().vector_get)(e.0, 0)),
                    val((api().vector_get)(e.0, 1)),
                )
            };
            let key = match k.as_str() {
                Some(s) => s.to_string(),
                // A keyword or symbol with its namespace (`user/id`), so distinct
                // keys stay distinct.
                None if k.named().is_some() => k.pr_str().trim_start_matches(':').to_string(),
                None => return cast_err(&k, what, "String or Keyword key"),
            };
            if out.insert(key, V::from_val(&x, what)?).is_some() {
                return Err(throw(
                    "IllegalArgumentException",
                    &format!("{what}: two keys of the map have the same name"),
                ));
            }
        }
        Ok(out)
    }
}

impl IntoVal for Val {
    fn into_val(self) -> Result<Val> {
        Ok(self)
    }
}
impl IntoVal for () {
    fn into_val(self) -> Result<Val> {
        Ok(Val::nil())
    }
}
impl IntoVal for i64 {
    fn into_val(self) -> Result<Val> {
        Ok(Val::int(self))
    }
}
impl IntoVal for i32 {
    fn into_val(self) -> Result<Val> {
        Ok(Val::int(self as i64))
    }
}
impl IntoVal for u32 {
    fn into_val(self) -> Result<Val> {
        Ok(Val::int(self as i64))
    }
}
impl IntoVal for usize {
    fn into_val(self) -> Result<Val> {
        match i64::try_from(self) {
            Ok(n) => Ok(Val::int(n)),
            Err(_) => Err(throw(
                "IllegalArgumentException",
                &format!("Value out of range for long: {self}"),
            )),
        }
    }
}
impl IntoVal for f64 {
    fn into_val(self) -> Result<Val> {
        Ok(Val::double(self))
    }
}
impl IntoVal for f32 {
    fn into_val(self) -> Result<Val> {
        Ok(Val::double(self as f64))
    }
}
impl IntoVal for bool {
    fn into_val(self) -> Result<Val> {
        Ok(Val::boolean(self))
    }
}
impl IntoVal for String {
    fn into_val(self) -> Result<Val> {
        Ok(Val::string(&self))
    }
}
impl IntoVal for &str {
    fn into_val(self) -> Result<Val> {
        Ok(Val::string(self))
    }
}
impl<T: IntoVal> IntoVal for Option<T> {
    fn into_val(self) -> Result<Val> {
        match self {
            Some(x) => x.into_val(),
            None => Ok(Val::nil()),
        }
    }
}
impl<T: IntoVal> IntoVal for Vec<T> {
    fn into_val(self) -> Result<Val> {
        let items = self
            .into_iter()
            .map(IntoVal::into_val)
            .collect::<Result<Vec<Val>>>()?;
        Ok(Val::vector(items))
    }
}
/// Keys stay strings, so a map round-trips through Rust unchanged.
impl<V: IntoVal> IntoVal for HashMap<String, V> {
    fn into_val(self) -> Result<Val> {
        // Owned `Val`s: a conversion that fails part-way releases what was built.
        let mut kvs: Vec<Val> = Vec::with_capacity(self.len() * 2);
        for (k, v) in self {
            kvs.push(Val::string(&k));
            kvs.push(v.into_val()?);
        }
        Ok(unsafe {
            val((api().map_new)(
                kvs.as_ptr() as *const u64,
                kvs.len() as i64,
            ))
        })
    }
}
/// `Err(message)` becomes a `RuntimeException`.
impl<T: IntoVal> IntoVal for std::result::Result<T, String> {
    fn into_val(self) -> Result<Val> {
        match self {
            Ok(x) => x.into_val(),
            Err(msg) => Err(throw("RuntimeException", &msg)),
        }
    }
}
impl<T: IntoVal> IntoVal for Result<T> {
    fn into_val(self) -> Result<Val> {
        self?.into_val()
    }
}

/// A Rust object handed to rscl as an opaque value. Return `Native<T>` from an export
/// to create one; take `&Native<T>` (or `&T` via `Deref`) to use it again.
pub struct Native<T>(pub T);

impl<T> std::ops::Deref for Native<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// What a native value's payload points at: the object behind its `TypeId`, which
/// is what identifies the type (two types can share a `type_name`).
#[repr(C)]
struct NativeBox<T> {
    id: std::any::TypeId,
    value: Native<T>,
}

unsafe extern "C" fn drop_native<T>(data: *mut c_void) {
    unsafe { drop(Box::from_raw(data as *mut NativeBox<T>)) }
}

/// The payload is shared with every rscl thread that gets hold of the value, and is
/// dropped on whichever releases it last: it must be `Send + Sync`.
impl<T: Send + Sync + 'static> IntoVal for Native<T> {
    fn into_val(self) -> Result<Val> {
        let name = std::any::type_name::<T>();
        let data = Box::into_raw(Box::new(NativeBox {
            id: std::any::TypeId::of::<T>(),
            value: self,
        })) as *mut c_void;
        Ok(unsafe {
            val((api().native_new)(
                name.as_ptr(),
                name.len() as i64,
                lib_token(),
                data,
                Some(drop_native::<T>),
            ))
        })
    }
}
impl<'a, T: Send + Sync + 'static> FromVal<'a> for &'a Native<T> {
    fn from_val(v: &'a Val, what: &str) -> Result<Self> {
        let name = std::any::type_name::<T>();
        let p = unsafe { (api().native_get)(v.0, name.as_ptr(), name.len() as i64, lib_token()) };
        if p.is_null() {
            return cast_err(v, what, name);
        }
        // Same library and same name; the `TypeId` (first field of every payload
        // this library creates) settles that it is the same type.
        let id = unsafe { *(p as *const std::any::TypeId) };
        if id != std::any::TypeId::of::<T>() {
            return cast_err(v, what, name);
        }
        Ok(unsafe { &(*(p as *const NativeBox<T>)).value })
    }
}

/// Export information for one fn (used by `exports!`).
pub struct Export {
    pub name: &'static str,
    pub entry: Entry,
    pub arity: u64,
}

/// Called by the generated `rscl_module_register`.
///
/// # Safety
/// `api` must be null or point to an `RtApi` table that outlives the library (the
/// runtime's static table).
pub unsafe fn register(api: *const RtApi, ns: &str, exports: &[Export]) -> i32 {
    if api.is_null() {
        return 1;
    }
    let api_ref: &'static RtApi = unsafe { &*api };
    if api_ref.version != API_VERSION {
        return 2;
    }
    let _ = API.set(api_ref);
    unsafe {
        (api_ref.begin_module)(ns.as_ptr(), ns.len() as i64);
        for e in exports {
            let name = e.name.replace('_', "-");
            (api_ref.register_fn)(
                name.as_ptr(),
                name.len() as i64,
                e.entry,
                // `exports!` rejects 64 or more parameters at compile time.
                1u64.checked_shl(e.arity as u32).unwrap_or(0),
                -1,
            );
        }
    }
    0
}

/// Body of every generated wrapper: arity check, conversion, panic safety.
///
/// # Safety
/// `argv` must point at `argc` values that stay valid for the call.
pub unsafe fn wrap(
    name: &str,
    argc: i64,
    argv: *const u64,
    owned: u64,
    arity: i64,
    f: impl FnOnce(&[Val]) -> Result<Val>,
) -> u64 {
    let _ = owned;
    wrap_borrowed(name, argc, argv, arity, f)
}

fn wrap_borrowed(
    name: &str,
    argc: i64,
    argv: *const u64,
    arity: i64,
    f: impl FnOnce(&[Val]) -> Result<Val>,
) -> u64 {
    if argc != arity {
        return throw(
            "ArityException",
            &format!("Wrong number of args ({argc}) passed to: {name}"),
        )
        .into_bits();
    }
    let args: &[Val] = if argc == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(argv as *const Val, argc as usize) }
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(args))) {
        Ok(Ok(v)) => v.into_raw(),
        Ok(Err(_)) => api().exc,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".to_string());
            throw(
                "RuntimeException",
                &format!("panic in native fn {name}: {msg}"),
            )
            .into_bits()
        }
    }
}

impl Exc {
    fn into_bits(self) -> u64 {
        api().exc
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! __count {
    () => { 0i64 };
    ($head:ident $($tail:ident)*) => { 1i64 + $crate::__count!($($tail)*) };
}

/// Define the functions of a bridge library and its registration entry point.
///
/// ```ignore
/// rscl_bridge::exports! {
///     ns "demo";
///     fn add(a: i64, b: i64) -> i64 { a + b }
///     fn shout(s: &str) -> String { s.to_uppercase() }
/// }
/// ```
#[macro_export]
macro_rules! exports {
    (ns $ns:literal; $( $(#[$meta:meta])* fn $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) $(-> $ret:ty)? $body:block )*) => {
        // Panics become rscl exceptions by unwinding; `panic = "abort"` would take the
        // whole host process down instead.
        const _: () = assert!(
            cfg!(panic = "unwind"),
            "rscl bridge libraries must be built with panic = \"unwind\""
        );

        $( $(#[$meta])* pub fn $name($($arg: $ty),*) $(-> $ret)? $body )*

        #[doc(hidden)]
        #[allow(deprecated, dead_code, clippy::items_after_statements)]
        pub mod __rscl_exports {
            use super::*;
            $(
                // The export's attributes apply to its wrapper too (`#[cfg(..)]`
                // removes both).
                $(#[$meta])*
                pub unsafe extern "C" fn $name(_f: *const ::std::ffi::c_void, argc: i64, argv: *const u64, owned: u64) -> u64 {
                    const ARITY: i64 = $crate::__count!($($arg)*);
                    const _: () = assert!(ARITY < 64, "an export takes at most 63 parameters");
                    unsafe { $crate::wrap(stringify!($name), argc, argv, owned, ARITY, |args| {
                        #[allow(unused_variables, unused_mut)]
                        let mut i = 0usize;
                        $(
                            let $arg: $ty = <$ty as $crate::FromVal>::from_val(&args[i], stringify!($arg))?;
                            i += 1;
                        )*
                        let r = super::$name($($arg),*);
                        $crate::IntoVal::into_val(r)
                    }) }
                }
            )*
            pub fn exports() -> ::std::vec::Vec<$crate::Export> {
                let mut all = ::std::vec::Vec::new();
                $(
                    {
                        // Used when the export's attributes remove it: the inner
                        // `__rscl_push` below, when present, shadows this one.
                        fn __rscl_push(_all: &mut ::std::vec::Vec<$crate::Export>) {}
                        {
                            $(#[$meta])*
                            fn __rscl_push(all: &mut ::std::vec::Vec<$crate::Export>) {
                                all.push($crate::Export { name: stringify!($name), entry: $name, arity: $crate::__count!($($arg)*) as u64 });
                            }
                            __rscl_push(&mut all);
                        }
                    }
                )*
                all
            }
        }

        /// Entry point the rscl runtime calls after `load-native`.
        ///
        /// # Safety
        /// `api` must be null or the runtime's static `RtApi` table.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn rscl_module_register(api: *const $crate::RtApi) -> i32 {
            unsafe { $crate::register(api, $ns, &__rscl_exports::exports()) }
        }
    };
}
