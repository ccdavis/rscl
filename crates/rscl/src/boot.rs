//! Loading the bootstrap library (`core.clj`) into a session.
use rscl_runtime::reader::{DefaultEnv, Reader};
use rscl_runtime::{Value, ns, pos};

use crate::jit::{EvalError, Jit};

/// Read every form of `core.clj`. Must run with `clojure.core` as the current
/// namespace so syntax-quote qualifies symbols into it.
pub fn core_forms() -> Result<Vec<Value>, EvalError> {
    let file = pos::intern_file("clojure/core.clj");
    let mut reader = Reader::new(rscl_runtime::CORE_CLJ, &DefaultEnv).with_file(file);
    let mut forms = Vec::new();
    while let Some(f) = reader.read()? {
        forms.push(f);
    }
    Ok(forms)
}

/// Run `f` with `clojure.core` as the current namespace, restoring the previous one.
pub fn in_core<T>(f: impl FnOnce() -> T) -> T {
    let prev = ns::current();
    ns::set_current(ns::core());
    let r = f();
    ns::set_current(prev);
    r
}

/// Compile and run `core.clj` in the JIT so its macros and fns are available.
pub fn load_core(jit: &mut Jit) -> Result<(), EvalError> {
    in_core(|| {
        for form in core_forms()? {
            jit.eval(&form)?;
        }
        Ok(())
    })
}
