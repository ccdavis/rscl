//! The JIT driver: one `JITModule` per session; each top-level form becomes a unit
//! that is compiled, finalised and run immediately. Redefinition happens through vars,
//! never through Cranelift, so old code is simply left in place.
use std::mem::ManuallyDrop;

use cranelift_jit::{ArenaMemoryProvider, JITBuilder, JITModule};
use cranelift_module::default_libcall_names;
use rscl_runtime::api::RtFn;
use rscl_runtime::reader::ReadError;
use rscl_runtime::{Value, exc, ns, value};

use crate::analyzer::{Analyzer, Node};
use crate::codegen::Compiler;
use crate::errors::CompileError;

/// Virtual address space reserved for JIT code and data, so RIP-relative references
/// between them always fit in 32 bits.
const JIT_ARENA_SIZE: usize = 1 << 30;

pub enum EvalError {
    Read(ReadError),
    Compile(CompileError),
    /// The thrown exception object (already taken from the pending slot).
    Exception(Value),
}

impl From<CompileError> for EvalError {
    fn from(e: CompileError) -> Self {
        EvalError::Compile(e)
    }
}

impl From<ReadError> for EvalError {
    fn from(e: ReadError) -> Self {
        EvalError::Read(e)
    }
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::Read(e) => write!(f, "Syntax error reading source: {e}"),
            EvalError::Compile(e) => write!(f, "Compile error: {e}"),
            EvalError::Exception(e) => write!(f, "Execution error: {}", exc::format_exception(e)),
        }
    }
}

pub struct Jit {
    compiler: ManuallyDrop<Compiler<JITModule>>,
}

impl Jit {
    pub fn new() -> Result<Jit, String> {
        rscl_runtime::entry::init();
        let flags = [
            ("opt_level", "none"),
            ("use_colocated_libcalls", "false"),
            ("is_pic", "false"),
        ];
        let mut builder =
            JITBuilder::with_flags(&flags, default_libcall_names()).map_err(|e| e.to_string())?;
        for &f in RtFn::ALL {
            let d = f.desc();
            builder.symbol(d.name, d.addr);
        }
        builder.symbol(
            "rscl_interrupt_flag",
            rscl_runtime::native::interrupt_flag_addr(),
        );
        let arena = ArenaMemoryProvider::new_with_size(JIT_ARENA_SIZE)
            .map_err(|e| format!("cannot reserve JIT memory: {e}"))?;
        builder.memory_provider(Box::new(arena));
        let module = JITModule::new(builder);
        let mut jit = Jit {
            compiler: ManuallyDrop::new(Compiler::new(module)),
        };
        crate::boot::load_core(&mut jit).map_err(|e| format!("error loading core.clj: {e}"))?;
        Ok(jit)
    }

    /// Compile an analysed form into the JIT and run it (used by `build` to evaluate
    /// forms at build time while a second compiler emits them into the object file).
    pub fn compile_and_run(&mut self, node: &Node, an: &Analyzer) -> Result<Value, EvalError> {
        let ids = self.compiler.compile_unit(node, an)?;
        self.run_unit(ids)
    }

    /// Evaluate a top-level form. A top-level `(do ...)` is evaluated one sub-form at a
    /// time so earlier definitions are visible to later ones.
    pub fn eval(&mut self, form: &Value) -> Result<Value, EvalError> {
        let mut last = Value::NIL;
        for unit_form in crate::analyzer::split_do(form) {
            last = self.eval_unit(&unit_form)?;
        }
        Ok(last)
    }

    fn eval_unit(&mut self, form: &Value) -> Result<Value, EvalError> {
        let mut an = Analyzer::new(ns::current());
        let node = an.analyze_toplevel(form)?;
        let ids = self.compiler.compile_unit(&node, &an)?;
        self.run_unit(ids)
    }

    fn run_unit(&mut self, ids: crate::codegen::UnitIds) -> Result<Value, EvalError> {
        self.compiler
            .module
            .finalize_definitions()
            .map_err(|e| CompileError::new(format!("finalize: {e}"), 0))?;
        let init = self.compiler.module.get_finalized_function(ids.init);
        let body = self.compiler.module.get_finalized_function(ids.body);
        // SAFETY: both functions were compiled with the `() -> i64` signature.
        let init: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(init) };
        let body: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(body) };
        let r = unsafe { init() };
        if r == value::EXC {
            return Err(EvalError::Exception(exc::take().unwrap_or(Value::NIL)));
        }
        let r = unsafe { body() };
        if r == value::EXC {
            return Err(EvalError::Exception(exc::take().unwrap_or(Value::NIL)));
        }
        Ok(unsafe { Value::from_raw(r) })
    }

    /// Read and evaluate every form in `src`, returning the last value.
    pub fn eval_str(&mut self, src: &str) -> Result<Value, EvalError> {
        let mut last = Value::NIL;
        for form in rscl_runtime::reader::read_all(src)? {
            last = self.eval(&form)?;
        }
        Ok(last)
    }
}

impl Drop for Jit {
    fn drop(&mut self) {
        // JIT code is never unmapped: vars are process-global and keep pointing at
        // compiled fns (core.clj's macros, for one) after the driver that compiled
        // them is gone, and a later `Jit` in the same process would call into freed
        // memory. The arena is reserved address space; the pages actually used stay
        // mapped until the process exits.
        let module = unsafe { ManuallyDrop::take(&mut self.compiler) }.into_module();
        std::mem::forget(module);
    }
}
