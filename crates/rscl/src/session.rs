//! A compilation session: the JIT, the source path, and (during `build`) the AOT
//! compiler. It lives in a thread-local so the runtime's `require`/`load-file` can
//! reach it from inside running code.
//!
//! Discipline: the session is only ever borrowed for *compilation*; no borrow is held
//! while generated code runs, because that code may call back into the session to
//! load another lib. `eval_form` compiles under a short borrow, then runs outside it.
use std::cell::RefCell;
use std::path::{Path, PathBuf};

use cranelift_object::ObjectModule;
use rscl_runtime::obj::symbol;
use rscl_runtime::reader::{DefaultEnv, Reader};
use rscl_runtime::{Value, exc, ns, pos};

use crate::analyzer::{Analyzer, split_do};
use crate::codegen::{Compiler, UnitIds};
use crate::errors::CompileError;
use crate::jit::{CompiledUnit, EvalError, Jit};

pub struct SessionConfig {
    pub paths: Vec<PathBuf>,
    pub aot: Option<Compiler<ObjectModule>>,
    pub no_eval: bool,
}

pub struct AotState {
    pub compiler: Compiler<ObjectModule>,
    core_units: Vec<UnitIds>,
    main_units: Vec<UnitIds>,
    /// `(key, units)` for every lib loaded during the build; each becomes a loader fn.
    loaders: Vec<(String, Vec<UnitIds>)>,
    /// Index of the loader currently being filled, or `None` for the main program.
    current: Option<usize>,
}

pub struct Session {
    pub jit: Jit,
    pub paths: Vec<PathBuf>,
    pub no_eval: bool,
    pub aot: Option<AotState>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Borrow the session for compilation work. Panics if called while already borrowed
/// (which would mean running code is being invoked under a borrow).
pub fn with<R>(f: impl FnOnce(&mut Session) -> R) -> R {
    SESSION.with(|s| {
        let mut guard = s
            .try_borrow_mut()
            .expect("session re-entered while borrowed");
        let session = guard
            .as_mut()
            .expect("no active session (call session::start)");
        f(session)
    })
}

pub fn is_active() -> bool {
    SESSION.with(|s| s.borrow().is_some())
}

/// Create the session for this thread: start the JIT (which loads core.clj), compile
/// core.clj into the AOT module too when building, and install the `require` hook.
pub fn start(cfg: SessionConfig) -> Result<(), String> {
    rscl_runtime::entry::init();
    let jit = Jit::new()?;
    let mut aot = None;
    if let Some(mut compiler) = cfg.aot {
        let mut core_units = Vec::new();
        let forms = crate::boot::in_core(crate::boot::core_forms).map_err(|e| e.to_string())?;
        crate::boot::in_core(|| -> Result<(), String> {
            for form in &forms {
                for unit_form in split_do(form) {
                    let mut an = Analyzer::new(ns::current());
                    let node = an.analyze_toplevel(&unit_form).map_err(|e| e.to_string())?;
                    core_units.push(
                        compiler
                            .compile_unit(&node, &an)
                            .map_err(|e| e.to_string())?,
                    );
                }
            }
            Ok(())
        })?;
        aot = Some(AotState {
            compiler,
            core_units,
            main_units: Vec::new(),
            loaders: Vec::new(),
            current: None,
        });
    }
    SESSION.with(|s| {
        *s.borrow_mut() = Some(Session {
            jit,
            paths: cfg.paths,
            no_eval: cfg.no_eval,
            aot,
        })
    });
    ns::set_dynamic_loader(Some(dynamic_loader));
    Ok(())
}

/// Source directories: the program's own directory, the working directory, explicit
/// `--path` entries, then `RSCL_PATH`.
pub fn source_paths(program: Option<&Path>, extra: &[String]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = program.and_then(|p| p.parent()) {
        paths.push(if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir.to_path_buf()
        });
    }
    paths.push(PathBuf::from("."));
    paths.extend(extra.iter().map(PathBuf::from));
    if let Ok(env) = std::env::var("RSCL_PATH") {
        paths.extend(env.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    paths.dedup();
    paths
}

/// Evaluate one top-level form (macroexpanded and `do`-split like Clojure's compiler).
pub fn eval_form(form: &Value) -> Result<Value, EvalError> {
    crate::toplevel::eval(form, &mut eval_unit)
}

/// Namespace bookkeeping forms that must run even under `--no-eval`, or later forms
/// cannot be analysed and required libs would not be compiled into the binary.
fn is_ns_directive(form: &Value) -> bool {
    let Some(items) = crate::analyzer::seq_items(form) else {
        return false;
    };
    items.first().and_then(symbol::as_sym).is_some_and(|s| {
        (s.ns().is_none() || s.ns() == Some("clojure.core"))
            && matches!(
                s.name(),
                "in-ns" | "require" | "use" | "refer" | "alias" | "load-file" | "refer-clojure"
            )
    })
}

fn eval_unit(form: &Value) -> Result<Value, EvalError> {
    let no_eval = with(|s| s.no_eval);
    let run = !no_eval || is_ns_directive(form);
    // Analysis runs macros, and a macro may `require` a lib, which re-enters the
    // session: analyse without holding the borrow. Compilation itself runs no code.
    let mut an = Analyzer::new(ns::current());
    let node = an.analyze_toplevel(form)?;
    let unit = with(|s| -> Result<Option<CompiledUnit>, EvalError> {
        let compiled = if run {
            Some(s.jit.compile(&node, &an)?)
        } else {
            None
        };
        if let Some(aot) = &mut s.aot {
            let ids = aot.compiler.compile_unit(&node, &an)?;
            match aot.current {
                Some(i) => aot.loaders[i].1.push(ids),
                None => aot.main_units.push(ids),
            }
        }
        Ok(compiled)
    })?;
    // Run outside the borrow: the code may `require` other libs through the hook.
    match unit {
        Some(u) => Jit::run(u),
        None => Ok(Value::NIL),
    }
}

/// Read and evaluate every form of `src` (positions attributed to `file`).
pub fn eval_source(src: &str, file: &str) -> Result<Value, EvalError> {
    let file_id = pos::intern_file(file);
    let mut reader = Reader::new(src, &DefaultEnv).with_file(file_id);
    let mut last = Value::NIL;
    while let Some(form) = reader.read()? {
        last = eval_form(&form)?;
    }
    Ok(last)
}

/// The runtime's hook for `require` / `load-file`: locate the source, compile and run
/// it (and, when building, collect its units into a loader fn for the binary).
fn dynamic_loader(key: &str) -> rscl_runtime::RtResult<()> {
    let paths = with(|s| s.paths.clone());
    let (path, display) = match key.strip_prefix("file:") {
        Some(p) => (PathBuf::from(p), p.to_string()),
        None => {
            let rel = ns::ns_to_relative_path(key);
            match paths.iter().map(|d| d.join(&rel)).find(|p| p.is_file()) {
                Some(p) => (p.clone(), p.to_string_lossy().into_owned()),
                None => {
                    let dirs = paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return exc::err(
                        rscl_runtime::obj::exception::RUNTIME,
                        format!("Could not locate {rel} on the source path [{dirs}]"),
                    );
                }
            }
        }
    };
    let src = std::fs::read_to_string(&path).map_err(|e| {
        exc::throw_new(
            rscl_runtime::obj::exception::RUNTIME,
            format!("cannot read {display}: {e}"),
        )
    })?;
    let prev_target = with(|s| {
        s.aot.as_mut().map(|aot| {
            aot.loaders.push((key.to_string(), Vec::new()));
            aot.current.replace(aot.loaders.len() - 1)
        })
    });
    // (`ns::load_lib` restores the caller's current namespace afterwards.)
    let result = eval_source(&src, &display);
    if let Some(prev) = prev_target {
        with(|s| {
            if let Some(aot) = &mut s.aot {
                aot.current = prev;
            }
        });
    }
    match result {
        Ok(_) => Ok(()),
        Err(EvalError::Exception(e)) => Err(exc::throw(e)),
        Err(e) => exc::err(
            rscl_runtime::obj::exception::COMPILER,
            format!("while loading {display}: {e}"),
        ),
    }
}

/// Finish an AOT build: emit the loader fns, the entry and `main`, and return the
/// object file bytes.
pub fn finish_aot() -> Result<Vec<u8>, EvalError> {
    let mut aot =
        with(|s| s.aot.take()).ok_or_else(|| CompileError::new("finish_aot: not building", 0))?;
    let mut loader_fns = Vec::new();
    for (key, units) in &aot.loaders {
        loader_fns.push((key.clone(), aot.compiler.compile_loader(key, units)?));
    }
    let main_ns = ns::current().name.to_string();
    let entry =
        aot.compiler
            .compile_entry(&aot.core_units, &aot.main_units, &loader_fns, &main_ns)?;
    aot.compiler.compile_c_main(entry)?;
    let product = aot.compiler.into_module().finish();
    product
        .emit()
        .map_err(|e| CompileError::new(format!("object emission failed: {e}"), 0).into())
}

/// The `-main` var of the current namespace (interned if absent).
pub fn main_var() -> &'static rscl_runtime::obj::var::VarObj {
    ns::current().intern("-main")
}
