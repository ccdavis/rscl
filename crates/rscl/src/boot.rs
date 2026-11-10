//! Loading the bootstrap library (`core.clj`) into a session.
use rscl_runtime::reader::{DefaultEnv, Reader};
use rscl_runtime::{Value, ns, pos};

use crate::jit::{EvalError, Jit};

/// Read every form of `core.clj`. Must run with `clojure.core` as the current
/// namespace so syntax-quote qualifies symbols into it.
pub fn core_forms() -> Result<Vec<Value>, EvalError> {
    let mut forms = Vec::new();
    each_form(rscl_runtime::CORE_CLJ, "clojure/core.clj", |f| {
        forms.push(f);
        Ok(())
    })?;
    Ok(forms)
}

/// Read `src` form by form, with positions attributed to `file`, handing each form
/// to `f` before the next one is read (a form may change how later ones read, e.g.
/// by switching the namespace).
pub fn each_form(
    src: &str,
    file: &str,
    mut f: impl FnMut(Value) -> Result<(), EvalError>,
) -> Result<(), EvalError> {
    let mut reader = Reader::new(src, &DefaultEnv).with_file(pos::intern_file(file));
    while let Some(form) = reader.read()? {
        f(form)?;
    }
    Ok(())
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
