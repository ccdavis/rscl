//! Runtime entry points called from generated code (see `api.rs` for the table that
//! the compiler uses to declare them). Conventions: pointer-ish arguments are
//! borrowed (`VRef`) unless documented as consumed (`Value`); results are owned.
use std::ffi::c_char;

use crate::exc::{self, RtResult};
use crate::obj::func::{self, Entry, FnObj, FnProto};
use crate::obj::{self, Kind, exception, keyword, list, map, set, string, symbol, vector};
use crate::value::{VRef, Value};

/// View a (pointer, length) pair passed by generated code as a `&str`; null or empty
/// yields `""`. The compiler only ever passes UTF-8 it produced itself.
pub(crate) unsafe fn str_arg<'a>(ptr: *const u8, len: i64) -> &'a str {
    if ptr.is_null() || len == 0 {
        return "";
    }
    unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len as usize)) }
}

unsafe fn args_slice<'a>(argv: *const Value, argc: i64) -> &'a [VRef] {
    if argc == 0 || argv.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(argv as *const VRef, argc as usize) }
    }
}

// ---- refcounting -------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_retain(v: VRef) {
    std::mem::forget(v.retain());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_release(v: Value) {
    drop(v);
}

// ---- constants ---------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_box_double(d: f64) -> Value {
    Value::double(d)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_box_long(n: i64) -> Value {
    Value::int(n)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_string_new(ptr: *const u8, len: i64) -> Value {
    string::new(unsafe { str_arg(ptr, len) })
}

/// `ns` may be null for an unqualified keyword.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_keyword(
    ns: *const u8,
    ns_len: i64,
    name: *const u8,
    name_len: i64,
) -> Value {
    let ns = if ns.is_null() {
        None
    } else {
        Some(unsafe { str_arg(ns, ns_len) })
    };
    keyword::intern(ns, unsafe { str_arg(name, name_len) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_symbol(
    ns: *const u8,
    ns_len: i64,
    name: *const u8,
    name_len: i64,
) -> Value {
    let ns = if ns.is_null() {
        None
    } else {
        Some(unsafe { str_arg(ns, ns_len) })
    };
    symbol::new(ns, unsafe { str_arg(name, name_len) })
}

/// Read one form of EDN-ish text produced by the compiler for quoted constants.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_read_const(ptr: *const u8, len: i64) -> Value {
    let text = unsafe { str_arg(ptr, len) };
    match crate::reader::read_str(text) {
        Ok(v) => v,
        Err(e) => {
            exc::throw_new(exception::COMPILER, format!("bad constant {text:?}: {e}"));
            Value::EXC
        }
    }
}

// ---- functions ---------------------------------------------------------------------

/// Create an immortal prototype for a compiled function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_fnproto_new(
    entry: *const u8,
    name: *const u8,
    name_len: i64,
    arity_mask: u64,
    variadic_min: i64,
) -> *const FnProto {
    let entry: Entry = unsafe { std::mem::transmute::<*const u8, Entry>(entry) };
    func::new_proto(
        entry,
        unsafe { str_arg(name, name_len) },
        arity_mask,
        variadic_min,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_fn_new(proto: *const FnProto, ncaptures: i64) -> Value {
    func::new(proto, ncaptures as usize)
}

/// Store capture `i` (ownership of `v` transferred).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_fn_set_capture(f: VRef, i: i64, v: Value) {
    unsafe { func::set_capture(&f, i as usize, v) }
}

/// Arity failure for a compiled function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_throw_arity(f: VRef, argc: i64) -> Value {
    let name = func::as_fn(&f)
        .map(|fo| fo.proto().name.to_string())
        .unwrap_or_else(|| "fn".to_string());
    crate::core::arity_error(&name, argc)
}

/// The general call path: anything callable that is not a plain fn object (keywords,
/// maps, vectors, sets, vars), plus the "not a function" error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_invoke(f: VRef, argc: i64, argv: *const Value) -> Value {
    let args = unsafe { args_slice(argv, argc) };
    match invoke(&f, args) {
        Ok(v) => v,
        Err(_) => Value::EXC,
    }
}

/// Invoke any callable from Rust.
pub fn invoke(f: &Value, args: &[VRef]) -> RtResult<Value> {
    match f.kind() {
        Some(Kind::Fn) => {
            let fo = func::get(f);
            let proto = fo.proto();
            if !proto.accepts(args.len() as i64) {
                return Err(exc::Exc_from(crate::core::arity_error(
                    &proto.name,
                    args.len() as i64,
                )));
            }
            let argv = if args.is_empty() {
                std::ptr::null()
            } else {
                args.as_ptr() as *const Value
            };
            let r = unsafe { (proto.entry)(f.ptr() as *const FnObj, args.len() as i64, argv) };
            exc::check(r)
        }
        Some(Kind::Keyword) | Some(Kind::Map) | Some(Kind::Set) => lookup_call(f, args),
        Some(Kind::Vector) => {
            if args.len() != 1 {
                return Err(exc::Exc_from(crate::core::arity_error(
                    "vector",
                    args.len() as i64,
                )));
            }
            match args[0].as_int() {
                Some(i) => crate::coll::nth(f, i, None),
                None => exc::err(exception::ILLEGAL_ARGUMENT, "Key must be integer"),
            }
        }
        Some(Kind::Var) => {
            let root = obj::var::get(f).root();
            if root.is_unbound() {
                std::mem::forget(root);
                return exc::err(
                    exception::ILLEGAL_STATE,
                    format!(
                        "Attempting to call unbound fn: #'{}",
                        obj::var::get(f).qualified()
                    ),
                );
            }
            invoke(&root, args)
        }
        _ => exc::err(
            exception::CLASS_CAST,
            format!("{} cannot be cast to IFn", crate::core::type_name(f)),
        ),
    }
}

fn lookup_call(f: &Value, args: &[VRef]) -> RtResult<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(exc::Exc_from(crate::core::arity_error(
            crate::core::type_name(f),
            args.len() as i64,
        )));
    }
    let not_found = if args.len() == 2 {
        args[1].owned()
    } else {
        Value::NIL
    };
    // A keyword looks itself up in its argument; a map or set looks the argument up.
    match f.kind() {
        Some(Kind::Keyword) => crate::coll::get(&args[0], f, Some(&not_found)),
        _ => crate::coll::get(f, &args[0], Some(&not_found)),
    }
}

impl Value {
    /// Call this value as a function with borrowed arguments.
    pub fn call(&self, args: &[Value]) -> RtResult<Value> {
        // SAFETY: Value and VRef share a layout.
        let refs = unsafe { std::slice::from_raw_parts(args.as_ptr() as *const VRef, args.len()) };
        invoke(self, refs)
    }
}

// ---- collections -------------------------------------------------------------------

/// Build the rest-argument list of a variadic call: `nil` when there are no extra
/// arguments (Clojure semantics), otherwise a list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_list_from_slice(items: *const Value, n: i64) -> Value {
    if n <= 0 {
        return Value::NIL;
    }
    let items = unsafe { args_slice(items, n) };
    list::from_iter(items.iter().map(|x| x.owned()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_vector_from_slice(items: *const Value, n: i64) -> Value {
    let items = unsafe { args_slice(items, n) };
    vector::from_iter(items.iter().map(|x| x.owned()))
}

/// Alternating keys and values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_map_from_slice(items: *const Value, n: i64) -> Value {
    let items = unsafe { args_slice(items, n) };
    let mut m = map::empty_map();
    for kv in items.chunks(2) {
        m.insert(kv[0].owned(), kv[1].owned());
    }
    map::new(m)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_set_from_slice(items: *const Value, n: i64) -> Value {
    let items = unsafe { args_slice(items, n) };
    let mut s = set::empty_set();
    for x in items {
        s.insert(x.owned());
    }
    set::new(s)
}

/// Mark a compile-time constant (and everything it contains) immortal: the unit's
/// constant table references it forever, so refcounting it is pure overhead and it
/// must not count as a live object in leak checks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_make_immortal(v: VRef) {
    make_immortal(&v);
}

pub fn make_immortal(v: &Value) {
    let Some(h) = v.header() else { return };
    if h.is_immortal() {
        return;
    }
    h.rc.store(crate::value::IMMORTAL, std::sync::atomic::Ordering::Relaxed);
    crate::value::note_freed();
    crate::value::register_immortal(v.bits() as usize);
    match v.kind() {
        Some(Kind::List) => {
            // Each cons cell is its own object: mark the chain iteratively.
            let mut cell = list::get(v);
            loop {
                make_immortal(&cell.first);
                make_immortal(&cell.meta);
                if !cell.rest.is_kind(Kind::List) {
                    break;
                }
                let next = list::get(&cell.rest);
                let h = cell.rest.header().unwrap();
                if h.is_immortal() {
                    break;
                }
                h.rc.store(crate::value::IMMORTAL, std::sync::atomic::Ordering::Relaxed);
                crate::value::note_freed();
                crate::value::register_immortal(cell.rest.bits() as usize);
                cell = next;
            }
        }
        Some(Kind::Vector) => {
            for x in vector::get(v).v.iter() {
                make_immortal(x);
            }
            make_immortal(&vector::get(v).meta);
        }
        Some(Kind::Map) => {
            for (k, x) in map::get(v).m.iter() {
                make_immortal(k);
                make_immortal(x);
            }
            make_immortal(&map::get(v).meta);
        }
        Some(Kind::Set) => {
            for x in set::get(v).s.iter() {
                make_immortal(x);
            }
            make_immortal(&set::get(v).meta);
        }
        Some(Kind::Symbol) => make_immortal(&symbol::get(v).meta),
        _ => {}
    }
}

// ---- printing ----------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_pr_str(v: VRef) -> Value {
    string::from_string(crate::printer::pr_str(&v))
}

/// Print a value followed by a newline (the top-level form printer of AOT binaries).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_prn(v: VRef) {
    let mut s = crate::printer::pr_str(&v);
    s.push('\n');
    crate::core::basic::emit(&s);
}

/// Report the pending exception on stderr; returns the process exit code (1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_report_uncaught() -> i32 {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    match exc::take() {
        Some(e) => eprintln!("Uncaught exception: {}", exc::format_exception(&e)),
        None => eprintln!("Uncaught exception: <none pending>"),
    }
    1
}

/// AOT entry: call `-main` (the var, if bound) with `*command-line-args*` spread as
/// positional arguments. Returns the process exit status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_run_main(v: *const obj::var::VarObj) -> i32 {
    let var = unsafe { &*v };
    let root = var.root();
    if root.is_unbound() {
        std::mem::forget(root);
        return 0;
    }
    let cli = crate::ns::core().intern("*command-line-args*").root();
    let args: Vec<Value> = match list::as_list(&cli) {
        Some(l) => l.to_vec(),
        None => Vec::new(),
    };
    match root.call(&args) {
        Ok(v) => {
            drop(v);
            0
        }
        Err(_) => unsafe { rt_report_uncaught() },
    }
}

/// Borrow a string's bytes as a NUL-terminated C string (kept alive by `v`); null for
/// non-strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_string_cstr(v: VRef) -> *const c_char {
    if v.is_kind(Kind::String) {
        string::cstr(&v)
    } else {
        std::ptr::null()
    }
}
