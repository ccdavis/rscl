//! Process-level entry points: runtime initialisation, the AOT `main` trampoline and
//! shutdown.
use std::ffi::{CStr, c_char};
use std::sync::Once;

use crate::ns;
use crate::obj::{func, list, string};
use crate::value::Value;

/// Stack size of the thread that runs rscl code (deep recursion without a guard yet).
pub const MAIN_STACK_SIZE: usize = 256 << 20;

static INIT: Once = Once::new();

/// Create `clojure.core` and `user`, register every builtin as a core var. Idempotent.
pub fn init() {
    INIT.call_once(|| {
        let core = ns::core();
        for d in crate::core::all() {
            let qualified = format!("{}/{}", d.ns, d.name);
            let proto = func::new_proto(d.entry, &qualified, d.arity_mask, d.variadic_min);
            let f = func::new(proto, 0);
            // Capture-less fns are shared, never freed: var reads need no refcounting.
            crate::ffi::make_immortal(&f);
            ns::find_or_create(d.ns).intern(d.name).bind_root(f);
        }
        core.intern("*command-line-args*").bind_root(Value::NIL);
        // Built-in types as vars (`String`, `Long`, `Object`, ...) for `instance?`,
        // `extend-type` and friends.
        for key in crate::obj::types::builtin_keys() {
            if key == crate::obj::types::KEY_NIL {
                continue;
            }
            let name = crate::obj::types::builtin_name(key);
            core.intern(name)
                .bind_root(crate::obj::types::builtin_type(key));
        }
        let user = ns::find_or_create(ns::USER);
        ns::set_current(user);
        // Namespaces that exist at start-up are provided by the runtime: `require`
        // must not look for a source file for them.
        for n in ns::all() {
            ns::mark_loaded(&n.name);
        }
    });
}

/// Bind `*command-line-args*` (program arguments, excluding the program name).
pub fn set_command_line_args(args: &[String]) {
    init();
    let v = if args.is_empty() {
        Value::NIL
    } else {
        list::from_iter(args.iter().map(|a| string::new(a)))
    };
    ns::core().intern("*command-line-args*").bind_root(v);
}

unsafe fn collect_args(argc: i32, argv: *const *const c_char) -> Vec<String> {
    if argv.is_null() || argc <= 1 {
        return Vec::new();
    }
    (1..argc as usize)
        .map(|i| {
            unsafe { CStr::from_ptr(*argv.add(i)) }
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_runtime_init(argc: i32, argv: *const *const c_char) {
    init();
    set_command_line_args(&unsafe { collect_args(argc, argv) });
}

/// `main` of an AOT-built program: initialise, run `entry` on a big-stack thread,
/// flush, and return its exit code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_main(
    argc: i32,
    argv: *const *const c_char,
    entry: Option<unsafe extern "C" fn() -> i32>,
) -> i32 {
    unsafe { rt_runtime_init(argc, argv) };
    let Some(entry) = entry else { return 0 };
    let entry_addr = entry as usize;
    let code = run_on_main_thread(move || {
        let f: unsafe extern "C" fn() -> i32 = unsafe { std::mem::transmute(entry_addr) };
        unsafe { f() }
    });
    unsafe { rt_shutdown() };
    code
}

/// Run `f` on a thread with the large rscl stack and the stack guard armed; a panic
/// or spawn failure is reported and becomes exit status 1.
pub fn run_on_main_thread<F: FnOnce() -> i32 + Send + 'static>(f: F) -> i32 {
    let handle = std::thread::Builder::new()
        .name("rscl-main".into())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(move || {
            crate::native::set_stack_limit_here(MAIN_STACK_SIZE);
            f()
        });
    match handle.map(|h| h.join()) {
        Ok(Ok(code)) => code,
        Ok(Err(_)) => {
            eprintln!("internal error: rscl main thread panicked");
            1
        }
        Err(e) => {
            eprintln!("internal error: cannot start main thread: {e}");
            1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_shutdown() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}
