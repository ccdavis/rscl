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

/// Entry points of a finalised unit.
#[derive(Clone, Copy)]
pub struct CompiledUnit {
    init: unsafe extern "C" fn() -> u64,
    body: unsafe extern "C" fn() -> u64,
}

impl Jit {
    pub fn new() -> Result<Jit, String> {
        rscl_runtime::entry::init();
        let flags = [
            ("opt_level", "none"),
            // The default register allocator, not the single-pass one, which starts
            // up 15% faster but leaves dead references where the conservative part
            // of the stack scan finds them (in registers that natives then save in
            // their frames): `(reduce + (range 3000000))` then keeps the whole
            // sequence, 316 MB against 84 MB.
            ("use_colocated_libcalls", "false"),
            ("is_pic", "false"),
            ("preserve_frame_pointers", "true"),
        ];
        let mut builder =
            JITBuilder::with_flags(&flags, default_libcall_names()).map_err(|e| e.to_string())?;
        for &f in RtFn::ALL {
            let d = f.desc();
            builder.symbol(d.name, d.addr);
        }
        // Direct calls to core builtins import them by their Rust symbol.
        for d in rscl_runtime::core::all() {
            builder.symbol(d.symbol, d.entry as *const u8);
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

    /// Compile and finalise a unit, returning its entry points. Running them is a
    /// separate step so no borrow of the JIT is held while generated code executes.
    pub fn compile(&mut self, node: &Node, an: &Analyzer) -> Result<CompiledUnit, EvalError> {
        let ids = self.compiler.compile_unit(node, an)?;
        self.finalize(ids)
    }

    fn finalize(&mut self, ids: crate::codegen::UnitIds) -> Result<CompiledUnit, EvalError> {
        self.compiler
            .module
            .finalize_definitions()
            .map_err(|e| CompileError::new(format!("finalize: {e}"), 0))?;
        for f in &ids.fns {
            let start = self.compiler.module.get_finalized_function(f.id) as usize;
            let lines = f.lines.clone();
            rscl_runtime::trace::register_code(
                start,
                f.size as usize,
                &f.name,
                lines,
                Some(f.frames.clone()),
                f.hidden,
            );
        }
        let init = self.compiler.module.get_finalized_function(ids.init);
        let body = self.compiler.module.get_finalized_function(ids.body);
        // SAFETY: both functions were compiled with the `() -> i64` signature.
        Ok(CompiledUnit {
            init: unsafe { std::mem::transmute::<*const u8, unsafe extern "C" fn() -> u64>(init) },
            body: unsafe { std::mem::transmute::<*const u8, unsafe extern "C" fn() -> u64>(body) },
        })
    }

    /// Run a compiled unit: its constant initialiser, then its body.
    pub fn run(unit: CompiledUnit) -> Result<Value, EvalError> {
        // Whatever earlier work left on the dead stack must not look like
        // references to the frames this unit is about to push.
        rscl_runtime::gc::clear_dead_stack();
        let r = rscl_runtime::gc::enter_compiled(|| unsafe { (unit.init)() });
        if r == value::EXC {
            return Err(EvalError::Exception(exc::take().unwrap_or(Value::NIL)));
        }
        let r = rscl_runtime::gc::enter_compiled(|| unsafe { (unit.body)() });
        if r == value::EXC {
            return Err(EvalError::Exception(exc::take().unwrap_or(Value::NIL)));
        }
        Ok(unsafe { Value::from_raw(r) })
    }

    /// Evaluate a top-level form. A top-level `(do ...)` is evaluated one sub-form at a
    /// time so earlier definitions are visible to later ones.
    pub fn eval(&mut self, form: &Value) -> Result<Value, EvalError> {
        crate::toplevel::eval(form, &mut |unit| self.eval_unit(unit))
    }

    fn eval_unit(&mut self, form: &Value) -> Result<Value, EvalError> {
        let unit = {
            // The analyser keeps forms in its own data while it runs macros: no
            // collection until the unit is compiled.
            let _no_gc = rscl_runtime::gc::inhibit();
            let mut an = Analyzer::new(ns::current());
            let node = an.analyze_toplevel(form)?;
            self.compile(&node, &an)?
        };
        Jit::run(unit)
    }

    /// Read and evaluate every form in `src`, returning the last value.
    pub fn eval_str(&mut self, src: &str) -> Result<Value, EvalError> {
        // With a file label, so forms carry positions like those of the REPL and of
        // source files (stack-trace lines).
        let mut last = Value::NIL;
        crate::boot::each_form(src, "<eval>", |form| {
            last = self.eval(&form)?;
            Ok(())
        })?;
        Ok(last)
    }
}

impl Drop for Jit {
    fn drop(&mut self) {
        // JIT code is never unmapped: vars are process-global and keep pointing at
        // compiled fns (core.clj's macros, for one) after the driver that compiled
        // them is gone, and a later `Jit` in the same process would call into freed
        // memory. Dropping the module without `free_memory` releases its bookkeeping
        // but leaves the code pages mapped until the process exits.
        let module = unsafe { ManuallyDrop::take(&mut self.compiler) }.into_module();
        drop(module);
    }
}
