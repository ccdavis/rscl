//! The runtime's side of the bridge ABI (`rscl_abi::RtApi`): one `extern "C"`
//! function per table entry, plus module registration and native library loading.
use std::cell::RefCell;
use std::ffi::{CString, c_char, c_void};

use rscl_abi::{API_VERSION, DropFn, Entry, REGISTER_SYMBOL, RtApi};

use crate::coll;
use crate::exc::{RtResult, err};
use crate::obj::exception::RUNTIME;
use crate::obj::{self, Kind, func, keyword, map, native, string, vector};
use crate::value::{EXC, NIL, VRef, Value};

unsafe fn s<'a>(p: *const u8, len: i64) -> &'a str {
    unsafe { crate::ffi::str_arg(p, len) }
}

fn borrowed(v: u64) -> VRef {
    VRef::from_raw(v)
}

unsafe fn write_str(src: &str, out: *mut *const u8, len: *mut i64) {
    unsafe {
        *out = src.as_ptr();
        *len = src.len() as i64;
    }
}

unsafe extern "C" fn api_retain(v: u64) {
    std::mem::forget(borrowed(v).retain());
}
unsafe extern "C" fn api_release(v: u64) {
    unsafe { Value::release_raw(v) }
}
unsafe extern "C" fn api_type_name(v: u64, out: *mut *const u8, len: *mut i64) {
    unsafe { write_str(crate::core::type_name(&borrowed(v)), out, len) }
}
unsafe extern "C" fn api_is_nil(v: u64) -> i32 {
    (v == NIL) as i32
}
unsafe extern "C" fn api_truthy(v: u64) -> i32 {
    borrowed(v).truthy() as i32
}
unsafe extern "C" fn api_bool_new(b: i32) -> u64 {
    Value::bool(b != 0).into_raw()
}
unsafe extern "C" fn api_int_get(v: u64, out: *mut i64) -> i32 {
    match borrowed(v).as_int() {
        Some(n) => {
            unsafe { *out = n };
            1
        }
        None => 0,
    }
}
unsafe extern "C" fn api_int_new(n: i64) -> u64 {
    Value::int(n).into_raw()
}
unsafe extern "C" fn api_double_get(v: u64, out: *mut f64) -> i32 {
    match crate::num::num(&borrowed(v)) {
        Some(crate::num::Num::I(n)) => {
            unsafe { *out = n as f64 };
            1
        }
        Some(crate::num::Num::D(d)) => {
            unsafe { *out = d };
            1
        }
        None => 0,
    }
}
unsafe extern "C" fn api_double_new(d: f64) -> u64 {
    Value::double(d).into_raw()
}
unsafe extern "C" fn api_string_get(v: u64, out: *mut *const u8, len: *mut i64) -> i32 {
    let v = borrowed(v);
    match string::as_str(&v) {
        Some(st) => {
            unsafe { write_str(st, out, len) };
            1
        }
        None => 0,
    }
}
unsafe extern "C" fn api_string_new(p: *const u8, len: i64) -> u64 {
    string::new(unsafe { s(p, len) }).into_raw()
}
unsafe extern "C" fn api_keyword_new(
    ns: *const u8,
    ns_len: i64,
    name: *const u8,
    name_len: i64,
) -> u64 {
    let ns = if ns.is_null() {
        None
    } else {
        Some(unsafe { s(ns, ns_len) })
    };
    keyword::intern(ns, unsafe { s(name, name_len) }).into_raw()
}
unsafe extern "C" fn api_named_name(v: u64, out: *mut *const u8, len: *mut i64) -> i32 {
    let v = borrowed(v);
    let name = match v.kind() {
        Some(Kind::Keyword) => keyword::get(&v).name(),
        Some(Kind::Symbol) => obj::symbol::get(&v).name(),
        _ => return 0,
    };
    unsafe { write_str(name, out, len) };
    1
}
unsafe extern "C" fn api_to_vector(v: u64) -> u64 {
    match coll::to_vec(&borrowed(v)) {
        Ok(items) => vector::from_iter(items).into_raw(),
        Err(_) => EXC,
    }
}
unsafe extern "C" fn api_vector_len(v: u64) -> i64 {
    vector::as_vec(&borrowed(v)).map_or(0, |x| x.v.len() as i64)
}
unsafe extern "C" fn api_vector_get(v: u64, i: i64) -> u64 {
    let v = borrowed(v);
    if i < 0 {
        return NIL;
    }
    vector::as_vec(&v)
        .and_then(|x| x.v.get(i as usize).cloned())
        .map_or(NIL, Value::into_raw)
}
unsafe extern "C" fn api_vector_new(items: *const u64, n: i64) -> u64 {
    let items: &[VRef] = if n <= 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(items as *const VRef, n as usize) }
    };
    vector::from_iter(items.iter().map(|x| x.owned())).into_raw()
}
unsafe extern "C" fn api_map_new(kvs: *const u64, n: i64) -> u64 {
    let items: &[VRef] = if n <= 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(kvs as *const VRef, n as usize) }
    };
    let mut m = map::empty_map();
    for kv in items.chunks(2) {
        if kv.len() == 2 {
            m.insert(kv[0].owned(), kv[1].owned());
        }
    }
    map::new(m).into_raw()
}
unsafe extern "C" fn api_map_get(m: u64, k: u64, not_found: u64) -> u64 {
    let nf = borrowed(not_found);
    match coll::get(&borrowed(m), &borrowed(k), Some(&nf)) {
        Ok(v) => v.into_raw(),
        Err(_) => EXC,
    }
}
unsafe extern "C" fn api_map_entries(m: u64) -> u64 {
    let m = borrowed(m);
    match map::as_map(&m) {
        Some(mo) => {
            let entries: Vec<Value> =
                mo.m.iter()
                    .map(|(k, v)| vector::from_slice(&[k.clone(), v.clone()]))
                    .collect();
            vector::from_iter(entries).into_raw()
        }
        None => {
            crate::exc::throw_new(
                obj::exception::CLASS_CAST,
                format!("{} cannot be cast to a map", crate::core::type_name(&m)),
            );
            EXC
        }
    }
}
unsafe extern "C" fn api_invoke(f: u64, argc: i64, argv: *const u64) -> u64 {
    unsafe { crate::ffi::rt_invoke(borrowed(f), argc, argv as *const Value) }.into_raw()
}
unsafe extern "C" fn api_throw(
    class: *const u8,
    class_len: i64,
    msg: *const u8,
    msg_len: i64,
) -> u64 {
    crate::exc::throw_new(unsafe { s(class, class_len) }, unsafe { s(msg, msg_len) });
    EXC
}
unsafe extern "C" fn api_native_new(
    type_name: *const u8,
    len: i64,
    token: u64,
    data: *mut c_void,
    drop: Option<DropFn>,
) -> u64 {
    native::new(unsafe { s(type_name, len) }, token, data, drop).into_raw()
}
unsafe extern "C" fn api_native_get(
    v: u64,
    type_name: *const u8,
    len: i64,
    token: u64,
) -> *mut c_void {
    let v = borrowed(v);
    match native::as_native(&v) {
        Some(n) if n.token == token && &*n.type_name == unsafe { s(type_name, len) } => n.data,
        _ => std::ptr::null_mut(),
    }
}

thread_local! {
    static MODULE_NS: RefCell<Option<String>> = const { RefCell::new(None) };
}

unsafe extern "C" fn api_begin_module(ns: *const u8, len: i64) {
    let name = unsafe { s(ns, len) }.to_string();
    crate::ns::find_or_create(&name);
    MODULE_NS.with(|m| *m.borrow_mut() = Some(name));
}
unsafe extern "C" fn api_register_fn(
    name: *const u8,
    len: i64,
    entry: Entry,
    arity_mask: u64,
    variadic_min: i64,
) {
    let name = unsafe { s(name, len) };
    let ns_name = MODULE_NS
        .with(|m| m.borrow().clone())
        .unwrap_or_else(|| crate::ns::current().name.to_string());
    let ns = crate::ns::find_or_create(&ns_name);
    let qualified = format!("{ns_name}/{name}");
    // The bridge's `Entry` takes `*const c_void`; ours takes `*const FnObj`: same ABI.
    let entry: func::Entry = unsafe { std::mem::transmute::<Entry, func::Entry>(entry) };
    let proto = func::new_proto(entry, &qualified, arity_mask, variadic_min);
    ns.intern(name).bind_root(func::new(proto, 0));
}
unsafe extern "C" fn api_pr_str(v: u64) -> u64 {
    string::from_string(crate::printer::pr_str(&borrowed(v))).into_raw()
}

/// The table handed to every bridge library.
pub static RT_API: RtApi = RtApi {
    version: API_VERSION,
    exc: EXC,
    retain: api_retain,
    release: api_release,
    type_name: api_type_name,
    is_nil: api_is_nil,
    truthy: api_truthy,
    nil: NIL,
    bool_new: api_bool_new,
    int_get: api_int_get,
    int_new: api_int_new,
    double_get: api_double_get,
    double_new: api_double_new,
    string_get: api_string_get,
    string_new: api_string_new,
    keyword_new: api_keyword_new,
    named_name: api_named_name,
    to_vector: api_to_vector,
    vector_len: api_vector_len,
    vector_get: api_vector_get,
    vector_new: api_vector_new,
    map_new: api_map_new,
    map_get: api_map_get,
    map_entries: api_map_entries,
    invoke: api_invoke,
    throw: api_throw,
    native_new: api_native_new,
    native_get: api_native_get,
    begin_module: api_begin_module,
    register_fn: api_register_fn,
    pr_str: api_pr_str,
};

// ---- loading libraries -------------------------------------------------------------

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

type RegisterFn = unsafe extern "C" fn(*const RtApi) -> i32;

/// Paths that have been opened at least once in this process.
static LOADED_PATHS: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Copy a library to a unique temporary file so `dlopen` maps the new contents.
fn reload_copy(found: &str, display: &str) -> RtResult<String> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file = std::path::Path::new(found)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!("rscl-reload-{}-{n}-{file}", std::process::id()));
    std::fs::copy(found, &tmp).map_err(|e| {
        crate::exc::throw_new(RUNTIME, format!("cannot copy {display} for reloading: {e}"))
    })?;
    Ok(tmp.to_string_lossy().into_owned())
}

/// Candidate locations for a library path that is not found as given.
fn candidates(path: &str) -> Vec<std::path::PathBuf> {
    let mut v = vec![std::path::PathBuf::from(path)];
    if let Ok(dirs) = std::env::var("RSCL_NATIVE_PATH") {
        for d in dirs.split(':').filter(|d| !d.is_empty()) {
            v.push(std::path::Path::new(d).join(path));
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        v.push(dir.join(path));
    }
    v
}

/// `load-native`: open a shared library (a rebuilt file at the same path loads the
/// new copy); if it exports `rscl_module_register`, register its fns and return the
/// module's namespace name.
pub fn load(path: &str) -> RtResult<Option<String>> {
    let found = candidates(path)
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from(path));
    let found = found.to_string_lossy().into_owned();
    // glibc returns the already-loaded object for a path it has seen, even if the file
    // was rebuilt. To reload, open a private copy under a fresh name instead.
    let to_open = if LOADED_PATHS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&found)
    {
        reload_copy(&found, path)?
    } else {
        found.clone()
    };
    // Bridge libraries stay local so a reloaded copy cannot clash with the previous
    // one; a plain C library is promoted to global so other libraries, `defnative`
    // and `dlsym` callers see its symbols.
    let h = crate::native::dlopen_checked(&to_open, path, crate::native::RTLD_LOCAL)?;
    if to_open != found {
        let _ = std::fs::remove_file(&to_open); // the mapping outlives the file
    }
    LOADED_PATHS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(found.clone());
    crate::native::remember_handle(h);
    let sym = CString::new(REGISTER_SYMBOL).unwrap();
    let reg = unsafe { dlsym(h, sym.as_ptr()) };
    if reg.is_null() {
        crate::native::dlopen_checked(&to_open, path, crate::native::RTLD_GLOBAL)?;
        return Ok(None);
    }
    let reg: RegisterFn = unsafe { std::mem::transmute::<*mut c_void, RegisterFn>(reg) };
    MODULE_NS.with(|m| *m.borrow_mut() = None);
    let status = unsafe { reg(&RT_API) };
    let ns = MODULE_NS.with(|m| m.borrow_mut().take());
    if status != 0 {
        return err(
            RUNTIME,
            format!(
                "{path}: rscl_module_register failed with status {status} (ABI version mismatch?)"
            ),
        );
    }
    Ok(ns)
}
