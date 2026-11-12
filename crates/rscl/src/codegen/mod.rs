//! `Node` → Cranelift IR, generic over `cranelift_module::Module` so the same code
//! serves the JIT and the AOT object writer.
//!
//! Invariants: no raw address is embedded in IR, and every call that can throw is
//! followed by an `EXC` check.
//!
//! Memory: the runtime has a tracing collector, so generated code neither counts
//! references nor cleans up on exception paths. What it owes the collector is to
//! say where its references are: every tagged value that may be a heap pointer is
//! declared to Cranelift (`FnCtx::gc`, and every value variable), which keeps the
//! ones that are live across a call in stack slots and lists those slots in the
//! call's stack map. The maps are handed to the runtime with the code (see
//! `FrameInfo`), and the collector reads compiled frames through them, precisely:
//! a value that is dead at a call is not a root. Liveness is Cranelift's, so there
//! is no locals-clearing pass here.
use std::collections::HashMap;
use std::rc::Rc;

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, FuncRef, InstBuilder, MemFlagsData, SigRef, Signature, StackSlot,
    StackSlotData, StackSlotKind, UserFuncName, Value, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use rscl_runtime::api::{self, RtFn, Ty as RtTy};
use rscl_runtime::core::BuiltinDesc;
use rscl_runtime::obj::Kind;
use rscl_runtime::{obj, printer};

use crate::analyzer::{
    Analyzer, Arity, CatchClause, CollKind, FnNode, LocalId, LocalInfo, LoopId, NativeTy, Node,
    PrimOp, RecurTarget, Ty, TyCache, ty_of_memo,
};
use crate::errors::{CResult, CompileError};

pub struct UnitIds {
    pub init: FuncId,
    pub body: FuncId,
    /// Every function compiled for this unit, `init` and `body` included (to be
    /// registered with the runtime: stack traces and the collector's frame maps).
    pub fns: Vec<CompiledFn>,
}

#[derive(Clone, Debug)]
pub struct CompiledFn {
    pub id: FuncId,
    pub name: String,
    pub size: u32,
    /// Source positions of the calls in the code (for stack traces).
    pub lines: Vec<LineEntry>,
    /// Where the function's frames keep their references at each call.
    pub frames: FrameInfo,
    /// Not an rscl fn (a unit's `init` or `body`, a loader): left out of stack
    /// traces.
    pub hidden: bool,
}

pub use rscl_runtime::trace::{FrameInfo, Line as LineEntry};

pub struct Compiler<M: Module> {
    pub module: M,
    /// The compilation context, reused from one function to the next so that its
    /// buffers are grown once (taken out while a function is being compiled).
    ctx: Option<cranelift_codegen::Context>,
    fb_ctx: FunctionBuilderContext,
    rt_ids: HashMap<RtFn, FuncId>,
    builtin_ids: HashMap<&'static str, FuncId>,
    debug_ir: bool,
    next_unit: u32,
    /// The runtime's `rscl_interrupt_flag` (an imported data symbol).
    flag_id: Option<DataId>,
    /// Every function compiled into this module (AOT registers them at start-up).
    registered_fns: Vec<CompiledFn>,
    /// Remember every compiled fn for `compile_entry` (AOT builds).
    pub keep_fns: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ConstEntry {
    Keyword(Option<String>, String),
    Str(String),
    Symbol(Option<String>, String),
    Double(u64),
    Long(i64),
    Edn(String),
    Var(String, String),
    /// Address of a C symbol, resolved with dlsym at unit initialisation.
    Native(String),
    Proto {
        func: FuncId,
        name: String,
        mask: u64,
        vmin: i64,
    },
    /// A permanent fn object for a capture-less fn: created once per unit, shared by
    /// every evaluation of the `fn*` form (Clojure's non-closure fns are singletons too).
    FnObj {
        func: FuncId,
        name: String,
        mask: u64,
        vmin: i64,
    },
}

struct UnitCtx {
    unit: u32,
    table_id: DataId,
    blob_id: DataId,
    init_id: FuncId,
    consts: Vec<ConstEntry>,
    const_index: HashMap<ConstEntry, u32>,
    blob: Vec<u8>,
    blob_index: HashMap<String, (i64, i64)>,
    pending_fns: Vec<(FuncId, Rc<FnNode>)>,
    fn_ids: HashMap<usize, FuncId>,
    fns: Vec<CompiledFn>,
    /// Source positions referenced by `SourceLoc`s in this unit (index + 1).
    positions: Vec<(std::sync::Arc<str>, u32, u32)>,
    files: HashMap<String, std::sync::Arc<str>>,
    pos_index: HashMap<u32, u32>,
}

impl UnitCtx {
    fn intern_const(&mut self, e: ConstEntry) -> u32 {
        if let Some(&i) = self.const_index.get(&e) {
            return i;
        }
        let i = self.consts.len() as u32;
        self.consts.push(e.clone());
        self.const_index.insert(e, i);
        i
    }

    /// `SourceLoc` index (+1) for a reader position, resolved to file/line/col now.
    fn srcloc_index(&mut self, pos: u32) -> Option<u32> {
        if pos == 0 {
            return None;
        }
        if let Some(&i) = self.pos_index.get(&pos) {
            return Some(i);
        }
        let p = rscl_runtime::pos::get(pos)?;
        let name = rscl_runtime::pos::file_name(p.file);
        let file = match self.files.get(&name) {
            Some(f) => f.clone(),
            None => {
                let f: std::sync::Arc<str> = name.as_str().into();
                self.files.insert(name, f.clone());
                f
            }
        };
        self.positions.push((file, p.line, p.col));
        let i = self.positions.len() as u32;
        self.pos_index.insert(pos, i);
        Some(i)
    }

    /// Append raw bytes to the blob; returns the offset.
    fn blob_bytes(&mut self, bytes: &[u8]) -> i64 {
        let off = self.blob.len() as i64;
        self.blob.extend_from_slice(bytes);
        off
    }

    fn blob_str(&mut self, s: &str) -> (i64, i64) {
        if let Some(&r) = self.blob_index.get(s) {
            return r;
        }
        let r = (self.blob.len() as i64, s.len() as i64);
        self.blob.extend_from_slice(s.as_bytes());
        self.blob_index.insert(s.to_string(), r);
        r
    }

    fn fn_id<M: Module>(&mut self, module: &mut M, f: &Rc<FnNode>) -> CResult<FuncId> {
        let key = Rc::as_ptr(f) as usize;
        if let Some(&id) = self.fn_ids.get(&key) {
            return Ok(id);
        }
        let sig = fn_signature(module);
        let n = self.fn_ids.len();
        let name = format!("u{}_f{}_{}", self.unit, n, sanitize(&f.name));
        let id = module
            .declare_function(&name, Linkage::Local, &sig)
            .map_err(internal)?;
        self.fn_ids.insert(key, id);
        self.pending_fns.push((id, f.clone()));
        Ok(id)
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn internal(e: impl std::fmt::Display) -> CompileError {
    CompileError::new(format!("internal compiler error: {e}"), 0)
}

/// The uniform calling convention, `entry(f, argc, argv) -> Value`: see
/// `rscl_runtime::obj::func::Entry`.
pub fn fn_signature<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    for _ in 0..3 {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

fn thunk_signature<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

fn cl_type(t: RtTy) -> cranelift_codegen::ir::Type {
    match t {
        RtTy::I64 => types::I64,
        RtTy::F64 => types::F64,
        RtTy::I32 => types::I32,
    }
}

impl<M: Module> Compiler<M> {
    pub fn new(module: M) -> Self {
        Compiler {
            module,
            ctx: None,
            fb_ctx: FunctionBuilderContext::new(),
            rt_ids: HashMap::new(),
            builtin_ids: HashMap::new(),
            debug_ir: std::env::var_os("RSCL_DEBUG_IR").is_some(),
            next_unit: 0,
            flag_id: None,
            registered_fns: Vec::new(),
            keep_fns: false,
        }
    }

    pub fn into_module(self) -> M {
        self.module
    }

    /// Compile one top-level form into `init` and `body` functions plus the functions
    /// for every `fn*` it contains.
    pub fn compile_unit(&mut self, node: &Node, an: &Analyzer) -> CResult<UnitIds> {
        let mut u = self.new_unit()?;
        let compiled = (|| {
            let body = self.compile_body_fn(&mut u, node, &an.locals)?;
            while let Some((id, f)) = u.pending_fns.pop() {
                self.compile_fn(&mut u, id, &f, &an.locals)?;
            }
            self.compile_init(&mut u)?;
            Ok(body)
        })();
        // Also for a failed unit: functions of it that were already defined refer to
        // its data, which the module must be able to resolve later.
        let data = self.define_unit_data(&mut u);
        let body = compiled?;
        data?;
        Ok(UnitIds {
            init: u.init_id,
            body,
            fns: std::mem::take(&mut u.fns),
        })
    }

    /// AOT: a `() -> i64` function that runs a lib's units in order (a `require`d
    /// namespace or a `load-file`d file); returns `EXC` if one of them throws.
    pub fn compile_loader(&mut self, key: &str, units: &[UnitIds]) -> CResult<FuncId> {
        let mut u = self.new_unit()?;
        let sig = thunk_signature(&self.module);
        let name = format!("u{}_load_{}", u.unit, sanitize(key));
        let id = self
            .module
            .declare_function(&name, Linkage::Local, &sig)
            .map_err(internal)?;
        let own_init = u.init_id;
        self.with_function(id, sig, &name, &mut u, false, |fc| {
            let entry = fc.b.create_block();
            fc.b.switch_to_block(entry);
            fc.b.seal_block(entry);
            fc.start(0, &[])?;
            let fail = fc.b.create_block();
            run_step(fc, own_init, fail);
            for unit in units {
                run_step(fc, unit.init, fail);
                run_step(fc, unit.body, fail);
            }
            let zero = fc.b.ins().iconst(types::I64, 0);
            fc.b.ins().return_(&[zero]);
            fc.b.switch_to_block(fail);
            fc.b.seal_block(fail);
            let exc = fc.b.ins().iconst(types::I64, api::consts::EXC as i64);
            fc.b.ins().return_(&[exc]);
            Ok(())
        })?;
        self.finish_unit(&mut u)?;
        Ok(id)
    }

    /// Declare a unit's constant table, name blob and `init` function.
    fn new_unit(&mut self) -> CResult<UnitCtx> {
        let unit = self.next_unit;
        self.next_unit += 1;
        let table_id = self
            .module
            .declare_anonymous_data(true, false)
            .map_err(internal)?;
        let blob_id = self
            .module
            .declare_anonymous_data(false, false)
            .map_err(internal)?;
        let init_id = self
            .module
            .declare_function(
                &format!("u{unit}_init"),
                Linkage::Local,
                &thunk_signature(&self.module),
            )
            .map_err(internal)?;
        Ok(UnitCtx {
            unit,
            table_id,
            blob_id,
            init_id,
            consts: Vec::new(),
            const_index: HashMap::new(),
            blob: Vec::new(),
            blob_index: HashMap::new(),
            pending_fns: Vec::new(),
            fn_ids: HashMap::new(),
            fns: Vec::new(),
            positions: Vec::new(),
            files: HashMap::new(),
            pos_index: HashMap::new(),
        })
    }

    /// Compile the unit's `init` and define its data objects (after every function
    /// that may add constants has been compiled).
    fn finish_unit(&mut self, u: &mut UnitCtx) -> CResult<()> {
        self.compile_init(u)?;
        self.define_unit_data(u)
    }

    fn define_unit_data(&mut self, u: &mut UnitCtx) -> CResult<()> {
        let mut table = DataDescription::new();
        table.define_zeroinit(8 * u.consts.len().max(1));
        table.set_align(8);
        self.module
            .define_data(u.table_id, &table)
            .map_err(internal)?;
        let mut blob = DataDescription::new();
        if u.blob.is_empty() {
            u.blob.push(0);
        }
        blob.define(u.blob.clone().into_boxed_slice());
        self.module
            .define_data(u.blob_id, &blob)
            .map_err(internal)?;
        Ok(())
    }

    /// AOT: `rscl_entry() -> i32` runs every unit's `init` and `body` in order, then
    /// `-main` (if bound) with the command-line arguments. Any uncaught exception is
    /// reported and becomes exit status 1.
    pub fn compile_entry(
        &mut self,
        core_units: &[UnitIds],
        main_units: &[UnitIds],
        loaders: &[(String, FuncId)],
        main_ns: &str,
    ) -> CResult<FuncId> {
        let mut u = self.new_unit()?;
        let mut sig = self.module.make_signature();
        sig.returns.push(AbiParam::new(types::I32));
        let id = self
            .module
            .declare_function("rscl_entry", Linkage::Local, &sig)
            .map_err(internal)?;
        let own_init = u.init_id;
        let main_ns = main_ns.to_string();
        let fns = self.registered_fns.clone();
        self.with_function(id, sig, "rscl_entry", &mut u, false, |fc| {
            let entry = fc.b.create_block();
            fc.b.switch_to_block(entry);
            fc.b.seal_block(entry);
            fc.start(0, &[])?;
            let fail = fc.b.create_block();
            run_step(fc, own_init, fail);
            // Every compiled function is registered with the runtime: its code
            // range, the source positions of its calls (so exceptions carry traces
            // with file and line), and its frame maps (for the collector).
            for f in &fns {
                let fref = fc.func_ref(f.id);
                let addr = fc.b.ins().func_addr(types::I64, fref);
                let size = fc.b.ins().iconst(types::I64, f.size as i64);
                if !f.hidden {
                    let (np, nl) = fc.blob_ptr(&f.name);
                    // Entries: start, end, line, col, file offset, file length (u32
                    // each).
                    let mut bytes = Vec::with_capacity(f.lines.len() * 24);
                    for l in &f.lines {
                        let (fo, fl) = fc.unit.blob_str(&l.file);
                        for x in [l.start, l.end, l.line, l.col, fo as u32, fl as u32] {
                            bytes.extend_from_slice(&x.to_le_bytes());
                        }
                    }
                    let off = fc.unit.blob_bytes(&bytes);
                    let entries = fc.b.ins().iadd_imm_s(fc.blob_base, off);
                    let n = fc.b.ins().iconst(types::I64, f.lines.len() as i64);
                    let base = fc.blob_base;
                    fc.call_rt(RtFn::RegisterCode, &[addr, size, np, nl, base, entries, n]);
                }
                let off = fc.unit.blob_bytes(&f.frames.encode());
                let table = fc.b.ins().iadd_imm_s(fc.blob_base, off);
                let hidden = fc.b.ins().iconst(types::I64, i64::from(f.hidden));
                let fp_to_sp = fc.b.ins().iconst(types::I64, f.frames.fp_to_sp as i64);
                fc.call_rt(RtFn::RegisterFrames, &[addr, size, hidden, fp_to_sp, table]);
            }
            for unit in core_units {
                run_step(fc, unit.init, fail);
                run_step(fc, unit.body, fail);
            }
            for (key, f) in loaders {
                let (np, nl) = fc.blob_ptr(key);
                let fref = fc.func_ref(*f);
                let addr = fc.b.ins().func_addr(types::I64, fref);
                fc.call_rt(RtFn::RegisterNsLoader, &[np, nl, addr]);
            }
            for unit in main_units {
                run_step(fc, unit.init, fail);
                run_step(fc, unit.body, fail);
            }
            let var = fc.const_slot(ConstEntry::Var(main_ns.clone(), "-main".to_string()));
            let code = fc.call_rt(RtFn::RunMain, &[var]).unwrap();
            fc.b.ins().return_(&[code]);
            fc.b.switch_to_block(fail);
            fc.b.seal_block(fail);
            let code = fc.call_rt(RtFn::ReportUncaught, &[]).unwrap();
            fc.b.ins().return_(&[code]);
            Ok(())
        })?;
        self.finish_unit(&mut u)?;
        Ok(id)
    }

    /// AOT: the exported C `main(argc, argv)` that hands control to `rt_main`.
    pub fn compile_c_main(&mut self, entry: FuncId) -> CResult<FuncId> {
        let mut u = self.new_unit()?;
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I32));
        let id = self
            .module
            .declare_function("main", Linkage::Export, &sig)
            .map_err(internal)?;
        self.with_function(id, sig, "main", &mut u, false, |fc| {
            let block = fc.b.create_block();
            fc.b.append_block_params_for_function_params(block);
            fc.b.switch_to_block(block);
            fc.b.seal_block(block);
            let params = fc.b.block_params(block).to_vec();
            let eref = fc.func_ref(entry);
            let addr = fc.b.ins().func_addr(types::I64, eref);
            let code = fc
                .call_rt(RtFn::Main, &[params[0], params[1], addr])
                .unwrap();
            fc.b.ins().return_(&[code]);
            Ok(())
        })?;
        self.finish_unit(&mut u)?;
        Ok(id)
    }

    fn with_function<F>(
        &mut self,
        func_id: FuncId,
        sig: Signature,
        name: &str,
        u: &mut UnitCtx,
        record: bool,
        body: F,
    ) -> CResult<()>
    where
        F: FnOnce(&mut FnCtx<'_, M>) -> CResult<()>,
    {
        let mut ctx = self
            .ctx
            .take()
            .unwrap_or_else(|| self.module.make_context());
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.index() as u32);
        {
            if self.flag_id.is_none() {
                self.flag_id = Some(
                    self.module
                        .declare_data("rscl_interrupt_flag", Linkage::Import, true, false)
                        .map_err(internal)?,
                );
            }
            let flag_id = self.flag_id.unwrap();
            let b = FunctionBuilder::new(&mut ctx.func, &mut self.fb_ctx);
            let mut fc = FnCtx::new(
                b,
                &mut self.module,
                u,
                &mut self.rt_ids,
                &mut self.builtin_ids,
                flag_id,
            )?;
            match body(&mut fc) {
                Ok(()) => {
                    let config = fc.module.target_config();
                    fc.b.finalize(config);
                    None
                }
                Err(e) => Some(e),
            }
        }
        .map_or(Ok(()), |e| {
            // The half-built function is abandoned: the shared builder context still
            // describes it and must not leak into the next compilation.
            self.fb_ctx = FunctionBuilderContext::new();
            Err(e)
        })?;
        if self.debug_ir {
            eprintln!("=== IR for {name} ===\n{}", ctx.func.display());
        }
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                CompileError::new(
                    format!(
                        "internal compiler error in {name}: {e}\n{}",
                        ctx.func.display()
                    ),
                    0,
                )
            })?;
        let (size, lines, frames) = match ctx.compiled_code() {
            Some(c) => {
                let lines = c
                    .buffer
                    .get_srclocs_sorted()
                    .iter()
                    .filter_map(|l| {
                        // `compile` has already applied the function's base
                        // location, but tolerate a relative one.
                        let loc = if l.loc.is_rel() {
                            l.loc.relocate(ctx.func.params.base_srcloc())
                        } else {
                            l.loc.as_abs()
                        };
                        if loc.is_default() {
                            return None;
                        }
                        let (file, line, col) = u.positions.get(loc.bits() as usize - 1)?;
                        Some(LineEntry {
                            start: l.start,
                            end: l.end,
                            file: file.clone(),
                            line: *line,
                            col: *col,
                        })
                    })
                    .collect();
                // Every call site, with the frame slots that hold live references
                // there (none for a call no reference is live across).
                let mut sites: Vec<(u32, Vec<u32>)> = c
                    .buffer
                    .call_sites()
                    .map(|s| (s.ret_addr, Vec::new()))
                    .collect();
                sites.sort_by_key(|s| s.0);
                sites.dedup_by_key(|s| s.0);
                for (ret, _, map) in c.buffer.user_stack_maps() {
                    let slots: Vec<u32> = map.entries().map(|(_, off)| off).collect();
                    match sites.binary_search_by_key(ret, |s| s.0) {
                        Ok(i) => sites[i].1 = slots,
                        Err(i) => sites.insert(i, (*ret, slots)),
                    }
                }
                let fp_to_sp = c
                    .buffer
                    .frame_layout()
                    .map_or(u32::MAX, |l| l.frame_to_fp_offset);
                (
                    c.code_info().total_size,
                    lines,
                    FrameInfo { fp_to_sp, sites },
                )
            }
            None => (0, Vec::new(), FrameInfo::default()),
        };
        let f = CompiledFn {
            id: func_id,
            name: name.to_string(),
            size,
            lines: if record { lines } else { Vec::new() },
            frames,
            hidden: !record,
        };
        // Only an AOT build registers every function again in the program's entry.
        if self.keep_fns {
            self.registered_fns.push(f.clone());
        }
        u.fns.push(f);
        self.module.clear_context(&mut ctx);
        self.ctx = Some(ctx);
        Ok(())
    }

    fn compile_body_fn(
        &mut self,
        u: &mut UnitCtx,
        node: &Node,
        locals: &[LocalInfo],
    ) -> CResult<FuncId> {
        let sig = thunk_signature(&self.module);
        let name = format!("u{}_body", u.unit);
        let id = self
            .module
            .declare_function(&name, Linkage::Local, &sig)
            .map_err(internal)?;
        let argv_cap = max_args(node);
        self.with_function(id, sig, &name, u, false, |fc| {
            let entry = fc.b.create_block();
            fc.b.switch_to_block(entry);
            fc.b.seal_block(entry);
            fc.start(argv_cap, locals)?;
            let v = fc.compile(node)?;
            fc.b.ins().return_(&[v]);
            Ok(())
        })?;
        Ok(id)
    }

    fn compile_fn(
        &mut self,
        u: &mut UnitCtx,
        id: FuncId,
        f: &Rc<FnNode>,
        locals: &[LocalInfo],
    ) -> CResult<()> {
        let sig = fn_signature(&self.module);
        let name = f.name.clone();
        let argv_cap = f
            .arities
            .iter()
            .map(|a| max_args(&a.body))
            .max()
            .unwrap_or(0);
        self.with_function(id, sig, &name, u, true, |fc| {
            let entry = fc.b.create_block();
            fc.b.append_block_params_for_function_params(entry);
            fc.b.switch_to_block(entry);
            fc.b.seal_block(entry);
            let params = fc.b.block_params(entry).to_vec();
            let (fv, argc, argv) = (params[0], params[1], params[2]);
            fc.gc(fv);
            fc.start(argv_cap, locals)?;

            // Captures live in the fn object.
            for (i, id) in f.captures.iter().enumerate() {
                let v = fc.b.ins().load(
                    types::I64,
                    MemFlagsData::trusted(),
                    fv,
                    api::consts::OFF_CAPTURES + 8 * i as i32,
                );
                fc.bind_local(*id, v);
            }
            if let Some(sl) = f.self_local {
                fc.bind_local(sl, fv);
            }

            // Arity dispatch.
            let blocks: Vec<Block> = f.arities.iter().map(|_| fc.b.create_block()).collect();
            // An exact fixed arity wins over the variadic one, whatever the order
            // they were written in.
            let mut order: Vec<usize> = (0..f.arities.len()).collect();
            order.sort_by_key(|&i| f.arities[i].rest.is_some());
            for i in order {
                let a = &f.arities[i];
                let n = a.params.len() as i64;
                let c = if a.rest.is_some() {
                    fc.b.ins()
                        .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, argc, n)
                } else {
                    fc.b.ins().icmp_imm_s(IntCC::Equal, argc, n)
                };
                let next = fc.b.create_block();
                fc.b.ins().brif(c, blocks[i], &[], next, &[]);
                fc.b.switch_to_block(next);
                fc.b.seal_block(next);
            }
            let r = fc.call_rt(RtFn::ThrowArity, &[fv, argc]).unwrap();
            fc.b.ins().return_(&[r]);

            for (i, a) in f.arities.iter().enumerate() {
                fc.b.switch_to_block(blocks[i]);
                fc.b.seal_block(blocks[i]);
                fc.compile_arity(a, argv, argc, f.pos)?;
            }
            Ok(())
        })
    }

    fn compile_init(&mut self, u: &mut UnitCtx) -> CResult<FuncId> {
        let sig = thunk_signature(&self.module);
        let name = format!("u{}_init", u.unit);
        let id = u.init_id;
        let consts = u.consts.clone();
        self.with_function(id, sig, &name, u, false, |fc| {
            let entry = fc.b.create_block();
            fc.b.switch_to_block(entry);
            fc.b.seal_block(entry);
            fc.start(0, &[])?;
            let table = fc.table_base;
            for (i, c) in consts.iter().enumerate() {
                let v = match c {
                    ConstEntry::Keyword(ns, name) => {
                        let (np, nl) = fc.blob_ptr(name);
                        let (sp, sl) = fc.opt_blob_ptr(ns.as_deref());
                        fc.call_rt(RtFn::Keyword, &[sp, sl, np, nl]).unwrap()
                    }
                    ConstEntry::Symbol(ns, name) => {
                        let (np, nl) = fc.blob_ptr(name);
                        let (sp, sl) = fc.opt_blob_ptr(ns.as_deref());
                        fc.call_rt(RtFn::Symbol, &[sp, sl, np, nl]).unwrap()
                    }
                    ConstEntry::Str(s) => {
                        let (p, l) = fc.blob_ptr(s);
                        fc.call_rt(RtFn::StringNew, &[p, l]).unwrap()
                    }
                    ConstEntry::Double(bits) => {
                        let d = fc.b.ins().f64const(f64::from_bits(*bits));
                        fc.call_rt(RtFn::BoxDouble, &[d]).unwrap()
                    }
                    ConstEntry::Long(n) => {
                        let n = fc.b.ins().iconst(types::I64, *n);
                        fc.call_rt(RtFn::BoxLong, &[n]).unwrap()
                    }
                    ConstEntry::Edn(s) => {
                        let (p, l) = fc.blob_ptr(s);
                        let r = fc.call_rt(RtFn::ReadConst, &[p, l]).unwrap();
                        fc.check_exc(r);
                        r
                    }
                    ConstEntry::Var(ns, name) => {
                        let (sp, sl) = fc.blob_ptr(ns);
                        let (np, nl) = fc.blob_ptr(name);
                        fc.call_rt(RtFn::InternVar, &[sp, sl, np, nl]).unwrap()
                    }
                    ConstEntry::Native(name) => {
                        let (np, nl) = fc.blob_ptr(name);
                        let p = fc.call_rt(RtFn::NativeSymbol, &[np, nl]).unwrap();
                        let is_null = fc.b.ins().icmp_imm_s(IntCC::Equal, p, 0);
                        let fail = fc.b.create_block();
                        let ok = fc.b.create_block();
                        fc.b.ins().brif(is_null, fail, &[], ok, &[]);
                        fc.b.switch_to_block(fail);
                        fc.b.seal_block(fail);
                        let r = fc.call_rt(RtFn::ThrowUnresolvedNative, &[np, nl]).unwrap();
                        fc.b.ins().return_(&[r]);
                        fc.b.switch_to_block(ok);
                        fc.b.seal_block(ok);
                        p
                    }
                    ConstEntry::Proto {
                        func,
                        name,
                        mask,
                        vmin,
                    }
                    | ConstEntry::FnObj {
                        func,
                        name,
                        mask,
                        vmin,
                    } => {
                        let fref = fc.func_ref(*func);
                        let addr = fc.b.ins().func_addr(types::I64, fref);
                        let (np, nl) = fc.blob_ptr(name);
                        let mask = fc.b.ins().iconst(types::I64, *mask as i64);
                        let vmin = fc.b.ins().iconst(types::I64, *vmin);
                        let proto = fc
                            .call_rt(RtFn::FnProtoNew, &[addr, np, nl, mask, vmin])
                            .unwrap();
                        if matches!(c, ConstEntry::FnObj { .. }) {
                            let zero = fc.b.ins().iconst(types::I64, 0);
                            fc.call_rt(RtFn::FnNew, &[proto, zero]).unwrap()
                        } else {
                            proto
                        }
                    }
                };
                if matches!(
                    c,
                    ConstEntry::Str(_)
                        | ConstEntry::Keyword(..)
                        | ConstEntry::Symbol(..)
                        | ConstEntry::Double(_)
                        | ConstEntry::Long(_)
                        | ConstEntry::Edn(_)
                        | ConstEntry::FnObj { .. }
                ) {
                    fc.call_rt(RtFn::MakeConstant, &[v]);
                }
                fc.b.ins()
                    .store(MemFlagsData::trusted(), v, table, 8 * i as i32);
            }
            let zero = fc.b.ins().iconst(types::I64, 0);
            fc.b.ins().return_(&[zero]);
            Ok(())
        })?;
        Ok(id)
    }
}

/// Call a `() -> i64` unit function and branch to `fail` on `EXC`.
fn run_step<M: Module>(fc: &mut FnCtx<'_, M>, f: FuncId, fail: Block) {
    let fref = fc.func_ref(f);
    let inst = fc.b.ins().call(fref, &[]);
    let v = fc.b.inst_results(inst)[0];
    let is_exc =
        fc.b.ins()
            .icmp_imm_s(IntCC::Equal, v, api::consts::EXC as i64);
    let cont = fc.b.create_block();
    fc.b.ins().brif(is_exc, fail, &[], cont, &[]);
    fc.b.switch_to_block(cont);
    fc.b.seal_block(cont);
}

/// Largest argument list (call or collection literal) in a function body, not
/// counting nested fns, which have their own slot.
fn max_args(node: &Node) -> usize {
    match node {
        Node::Const(_)
        | Node::Local(_)
        | Node::VarRef(_)
        | Node::TheVar(_)
        | Node::Fn(_)
        | Node::CurrentNs => 0,
        Node::Def { init, meta, .. } => init
            .as_ref()
            .map_or(0, |n| max_args(n))
            .max(meta.as_ref().map_or(0, |n| max_args(n))),
        Node::VarSet { value, .. } => max_args(value),
        Node::If { test, then, else_ } => max_args(test).max(max_args(then)).max(max_args(else_)),
        Node::Do(nodes) => nodes.iter().map(max_args).max().unwrap_or(0),
        Node::Let { bindings, body } | Node::Loop { bindings, body, .. } => bindings
            .iter()
            .map(|(_, n)| max_args(n))
            .max()
            .unwrap_or(0)
            .max(max_args(body)),
        Node::Recur { args, .. } => args.iter().map(max_args).max().unwrap_or(0),
        Node::Throw { expr, .. } => max_args(expr),
        Node::Try {
            body,
            catches,
            finally,
        } => max_args(body)
            .max(catches.iter().map(|c| max_args(&c.body)).max().unwrap_or(0))
            .max(finally.as_ref().map_or(0, |f| max_args(f))),
        Node::NativeCall { args, .. } => args.iter().map(max_args).max().unwrap_or(0),
        Node::Call { f, args, .. } => args
            .len()
            .max(max_args(f))
            .max(args.iter().map(max_args).max().unwrap_or(0)),
        Node::BuiltinCall { args, .. } => {
            args.len().max(args.iter().map(max_args).max().unwrap_or(0))
        }
        Node::Coll { items, .. } => items
            .len()
            .max(items.iter().map(max_args).max().unwrap_or(0)),
    }
}

/// Operators with a fast path on tagged fixnums (operand types unknown statically).
fn inline_op(desc: &BuiltinDesc, argc: usize) -> Option<PrimOp> {
    PrimOp::of(desc, argc).filter(|op| op.has_fixnum_path(argc))
}

// ---- per-function state --------------------------------------------------------------

#[derive(Clone)]
struct LoopCtx {
    header: Block,
    /// The loop variables in binding order. Primitive ones hold raw bits.
    vars: Vec<(Variable, Ty)>,
}

#[derive(Clone, Copy)]
struct Slot {
    var: Variable,
    /// `Val`: a tagged value. `Long`/`Double`: raw `i64`/`f64` in the variable.
    ty: Ty,
}

fn raw_type(ty: Ty) -> cranelift_codegen::ir::Type {
    match ty {
        Ty::Double => types::F64,
        _ => types::I64,
    }
}

struct FnCtx<'a, M: Module> {
    b: FunctionBuilder<'a>,
    module: &'a mut M,
    unit: &'a mut UnitCtx,
    rt_ids: &'a mut HashMap<RtFn, FuncId>,
    rt_refs: HashMap<RtFn, FuncRef>,
    builtin_ids: &'a mut HashMap<&'static str, FuncId>,
    builtin_refs: HashMap<&'static str, FuncRef>,
    func_refs: HashMap<FuncId, FuncRef>,
    locals: HashMap<LocalId, Slot>,
    loops: HashMap<LoopId, LoopCtx>,
    fn_recur: Option<LoopCtx>,
    argv_slot: Option<StackSlot>,
    table_base: Value,
    blob_base: Value,
    call_sig: SigRef,
    /// The analyzer's locals table (names for diagnostics, types for inference).
    local_infos: Vec<LocalInfo>,
    ty_cache: TyCache,
    /// Position of the primitive operation being compiled (for its error path).
    prim_pos: u32,
    /// Handler blocks of the active `try`s, innermost last.
    handlers: Vec<Block>,
    flag_id: DataId,
    flag_addr: Option<Value>,
    scratch: Option<StackSlot>,
}

impl<'a, M: Module> FnCtx<'a, M> {
    fn new(
        b: FunctionBuilder<'a>,
        module: &'a mut M,
        unit: &'a mut UnitCtx,
        rt_ids: &'a mut HashMap<RtFn, FuncId>,
        builtin_ids: &'a mut HashMap<&'static str, FuncId>,
        flag_id: DataId,
    ) -> CResult<Self> {
        let call_sig = b.func.import_signature(fn_signature(module));
        // Placeholders; `start` sets the real values once the entry block exists.
        let dummy = Value::from_u32(0);
        Ok(FnCtx {
            b,
            module,
            unit,
            rt_ids,
            rt_refs: HashMap::new(),
            builtin_ids,
            builtin_refs: HashMap::new(),
            func_refs: HashMap::new(),
            locals: HashMap::new(),
            loops: HashMap::new(),
            fn_recur: None,
            argv_slot: None,
            table_base: dummy,
            blob_base: dummy,
            call_sig,
            local_infos: Vec::new(),
            ty_cache: TyCache::new(),
            prim_pos: 0,
            handlers: Vec::new(),
            flag_id,
            flag_addr: None,
            scratch: None,
        })
    }

    /// Address of the runtime's interrupt flag (computed once per function).
    fn interrupt_flag_addr(&mut self) -> Value {
        if let Some(a) = self.flag_addr {
            return a;
        }
        let gv = self.module.declare_data_in_func(self.flag_id, self.b.func);
        let a = self.b.ins().symbol_value(types::I64, gv);
        self.flag_addr = Some(a);
        a
    }

    /// An 8-byte scratch slot for out-parameters of conversion helpers.
    fn scratch_slot(&mut self) -> StackSlot {
        if let Some(s) = self.scratch {
            return s;
        }
        let s =
            self.b
                .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        self.scratch = Some(s);
        s
    }

    /// Emit the per-function prologue pieces: constant table base, blob base, argv slot.
    fn start(&mut self, argv_cap: usize, locals: &[LocalInfo]) -> CResult<()> {
        self.local_infos = locals.to_vec();
        let tgv = self
            .module
            .declare_data_in_func(self.unit.table_id, self.b.func);
        self.table_base = self.b.ins().symbol_value(types::I64, tgv);
        let bgv = self
            .module
            .declare_data_in_func(self.unit.blob_id, self.b.func);
        self.blob_base = self.b.ins().symbol_value(types::I64, bgv);
        let size = (8 * argv_cap.max(1)) as u32;
        self.argv_slot = Some(self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            3,
        )));
        self.interrupt_flag_addr();
        Ok(())
    }

    fn argv_addr(&mut self) -> Value {
        let slot = self.argv_slot.expect("argv slot");
        self.b.ins().stack_addr(types::I64, slot, 0)
    }

    // ---- runtime calls and constants ----------------------------------------------

    fn rt(&mut self, f: RtFn) -> CResult<FuncRef> {
        if let Some(&r) = self.rt_refs.get(&f) {
            return Ok(r);
        }
        let id = match self.rt_ids.get(&f) {
            Some(&id) => id,
            None => {
                let d = f.desc();
                let mut sig = self.module.make_signature();
                for p in d.params {
                    sig.params.push(AbiParam::new(cl_type(*p)));
                }
                if let Some(r) = d.ret {
                    sig.returns.push(AbiParam::new(cl_type(r)));
                }
                let id = self
                    .module
                    .declare_function(d.name, Linkage::Import, &sig)
                    .map_err(internal)?;
                self.rt_ids.insert(f, id);
                id
            }
        };
        let r = self.module.declare_func_in_func(id, self.b.func);
        self.rt_refs.insert(f, r);
        Ok(r)
    }

    fn call_rt(&mut self, f: RtFn, args: &[Value]) -> Option<Value> {
        let r = self.rt(f).expect("runtime function declaration");
        let inst = self.b.ins().call(r, args);
        self.b.inst_results(inst).first().copied()
    }

    fn func_ref(&mut self, id: FuncId) -> FuncRef {
        if let Some(&r) = self.func_refs.get(&id) {
            return r;
        }
        let r = self.module.declare_func_in_func(id, self.b.func);
        self.func_refs.insert(id, r);
        r
    }

    /// Import of a Rust builtin by its symbol (uniform fn signature).
    fn builtin_ref(&mut self, desc: &'static BuiltinDesc) -> CResult<FuncRef> {
        if let Some(&r) = self.builtin_refs.get(desc.symbol) {
            return Ok(r);
        }
        let id = match self.builtin_ids.get(desc.symbol) {
            Some(&id) => id,
            None => {
                let sig = fn_signature(self.module);
                let id = self
                    .module
                    .declare_function(desc.symbol, Linkage::Import, &sig)
                    .map_err(internal)?;
                self.builtin_ids.insert(desc.symbol, id);
                id
            }
        };
        let r = self.module.declare_func_in_func(id, self.b.func);
        self.builtin_refs.insert(desc.symbol, r);
        Ok(r)
    }

    fn const_slot(&mut self, e: ConstEntry) -> Value {
        let i = self.unit.intern_const(e);
        self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.table_base,
            8 * i as i32,
        )
    }

    /// Attribute the instructions emitted next to the reader position `pos` (0:
    /// none), so a return address maps back to a file and line.
    fn srcloc(&mut self, pos: u32) {
        let loc = match self.unit.srcloc_index(pos) {
            Some(i) => cranelift_codegen::ir::SourceLoc::new(i),
            None => cranelift_codegen::ir::SourceLoc::default(),
        };
        self.b.set_srcloc(loc);
    }

    fn blob_ptr(&mut self, s: &str) -> (Value, Value) {
        let (off, len) = self.unit.blob_str(s);
        let p = self.b.ins().iadd_imm_s(self.blob_base, off);
        let l = self.b.ins().iconst(types::I64, len);
        (p, l)
    }

    fn opt_blob_ptr(&mut self, s: Option<&str>) -> (Value, Value) {
        match s {
            Some(s) => self.blob_ptr(s),
            None => {
                let z = self.b.ins().iconst(types::I64, 0);
                (z, z)
            }
        }
    }

    fn imm(&mut self, bits: u64) -> Value {
        self.b.ins().iconst(types::I64, bits as i64)
    }

    // ---- values and the collector ---------------------------------------------------

    /// Declare `v` a reference the collector must find: wherever it is live across a
    /// call, Cranelift keeps it in a stack slot that the call's stack map lists.
    /// Every tagged value that may be a heap pointer goes through here (immediates
    /// and compile-time constants, which are permanent, need not).
    fn gc(&mut self, v: Value) -> Value {
        self.b.declare_value_needs_stack_map(v);
        v
    }

    fn bind_local(&mut self, id: LocalId, val: Value) -> Variable {
        let var = self.b.declare_var(types::I64);
        self.b.declare_var_needs_stack_map(var);
        self.b.def_var(var, val);
        self.locals.insert(id, Slot { var, ty: Ty::Val });
        var
    }

    /// Bind a primitive local holding raw bits (not a reference).
    fn bind_typed(&mut self, id: LocalId, raw: Value, ty: Ty) -> Variable {
        let var = self.b.declare_var(raw_type(ty));
        self.b.def_var(var, raw);
        self.locals.insert(id, Slot { var, ty });
        var
    }

    /// End of a binding scope.
    fn unbind(&mut self, vars: &[(LocalId, Variable)]) {
        for (id, _) in vars {
            self.locals.remove(id);
        }
    }

    fn slot(&self, id: LocalId) -> CResult<Slot> {
        self.locals.get(&id).copied().ok_or_else(|| {
            internal(format!(
                "unbound local #{id} ({})",
                self.local_infos
                    .get(id as usize)
                    .map_or("?", |l| l.name.as_str())
            ))
        })
    }

    /// Static type of a local as the analyzer inferred it (the slot's type may differ
    /// inside a closure, where captures arrive boxed).
    fn local_ty(&self, id: LocalId) -> Ty {
        self.local_infos.get(id as usize).map_or(Ty::Val, |l| l.ty)
    }

    fn ty_of(&mut self, node: &Node) -> Ty {
        ty_of_memo(node, &self.local_infos, &mut self.ty_cache)
    }

    /// A local as a tagged value, boxing a primitive one.
    fn read_local(&mut self, id: LocalId) -> CResult<Value> {
        let slot = self.slot(id)?;
        let raw = self.b.use_var(slot.var);
        Ok(self.box_raw(raw, slot.ty))
    }

    // ---- primitives -----------------------------------------------------------------

    /// Box raw bits: a fixnum when it fits (no allocation), else a heap number.
    fn box_raw(&mut self, raw: Value, ty: Ty) -> Value {
        match ty {
            Ty::Double => {
                let v = self.call_rt(RtFn::BoxDouble, &[raw]).unwrap();
                self.gc(v)
            }
            Ty::Long => {
                let shifted = self.b.ins().ishl_imm_s(raw, 1);
                let back = self.b.ins().sshr_imm_s(shifted, 1);
                let fits = self.b.ins().icmp(IntCC::Equal, back, raw);
                let fast = self.b.create_block();
                let slow = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, types::I64);
                self.b.ins().brif(fits, fast, &[], slow, &[]);
                self.b.switch_to_block(fast);
                self.b.seal_block(fast);
                let tagged = self.b.ins().bor_imm_s(shifted, 1);
                self.b.ins().jump(merge, &[BlockArg::Value(tagged)]);
                self.b.switch_to_block(slow);
                self.b.seal_block(slow);
                let boxed = self.call_rt(RtFn::BoxLong, &[raw]).unwrap();
                self.b.ins().jump(merge, &[BlockArg::Value(boxed)]);
                self.b.switch_to_block(merge);
                self.b.seal_block(merge);
                let v = self.b.block_params(merge)[0];
                self.gc(v)
            }
            Ty::Val => raw,
        }
    }

    /// Unbox a tagged value into raw bits of `want`, throwing `ClassCastException`
    /// for non-numbers. Fixnums are converted inline; everything else goes through
    /// the runtime.
    fn unbox(&mut self, v: Value, want: Ty, strict: bool) -> CResult<Value> {
        let is_fix = self.b.ins().band_imm_s(v, 1);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, raw_type(want));
        self.b.ins().brif(is_fix, fast, &[], slow, &[]);

        self.b.switch_to_block(fast);
        self.b.seal_block(fast);
        let n = self.b.ins().sshr_imm_s(v, 1);
        let r = if want == Ty::Double {
            self.b.ins().fcvt_from_sint(types::F64, n)
        } else {
            n
        };
        self.b.ins().jump(merge, &[BlockArg::Value(r)]);

        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let slot = self.scratch_slot();
        let out = self.b.ins().stack_addr(types::I64, slot, 0);
        let r = if want == Ty::Double {
            self.call_rt(RtFn::UnboxDouble, &[v, out]).unwrap()
        } else {
            let s = self.b.ins().iconst(types::I64, i64::from(strict));
            self.call_rt(RtFn::UnboxLong, &[v, out, s]).unwrap()
        };
        self.check_exc(r);
        let loaded = self.b.ins().stack_load(types::I64, raw_type(want), slot, 0);
        self.b.ins().jump(merge, &[BlockArg::Value(loaded)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        Ok(self.b.block_params(merge)[0])
    }

    /// Convert raw bits between primitive types (`Long` widens to `Double`; a double
    /// is truncated to a long as `(long d)` does).
    fn convert(&mut self, raw: Value, from: Ty, to: Ty) -> Value {
        match (from, to) {
            (Ty::Long, Ty::Double) => self.b.ins().fcvt_from_sint(types::F64, raw),
            (Ty::Double, Ty::Long) => self.b.ins().fcvt_to_sint_sat(types::I64, raw),
            _ => raw,
        }
    }

    /// Raw bits of `from` where `to` is required: a long widens to a double; a double
    /// where a long is required (a `^long` binding or parameter) is a
    /// `ClassCastException`, never a silent truncation (`(long d)` truncates).
    fn coerce(&mut self, raw: Value, from: Ty, to: Ty) -> CResult<Value> {
        if from == Ty::Double && to == Ty::Long {
            let c = self.b.ins().iconst(types::I64, 2);
            let r = self.call_rt(RtFn::ThrowPrim, &[c]).unwrap();
            self.emit_exc_path(r);
            self.dead_block();
            return Ok(self.raw_zero(to));
        }
        Ok(self.convert(raw, from, to))
    }

    fn raw_zero(&mut self, ty: Ty) -> Value {
        if ty == Ty::Double {
            self.b.ins().f64const(0.0)
        } else {
            self.b.ins().iconst(types::I64, 0)
        }
    }

    /// Throw an `ArithmeticException` when `cond` holds (ends in the continuation).
    fn arith_check(&mut self, cond: Value, code: i64) {
        let fail = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(cond, fail, &[], cont, &[]);
        self.b.switch_to_block(fail);
        self.b.seal_block(fail);
        let c = self.b.ins().iconst(types::I64, code);
        self.srcloc(self.prim_pos);
        let r = self.call_rt(RtFn::ThrowPrim, &[c]).unwrap();
        self.srcloc(0);
        self.emit_exc_path(r);
        self.b.switch_to_block(cont);
        self.b.seal_block(cont);
    }

    /// Evaluate `node` to raw bits of type `want` (`Long` or `Double`).
    fn compile_typed(&mut self, node: &Node, want: Ty) -> CResult<Value> {
        debug_assert!(want != Ty::Val);
        match node {
            Node::Const(v) => {
                if let Some(n) = v.as_int() {
                    return Ok(if want == Ty::Double {
                        self.b.ins().f64const(n as f64)
                    } else {
                        self.b.ins().iconst(types::I64, n)
                    });
                }
                // A double literal in a long context only arises from a strict `^long`
                // binding, which must throw: fall through to the generic unboxing.
                if let Some(d) = v.as_double()
                    && want == Ty::Double
                {
                    return Ok(self.b.ins().f64const(d));
                }
            }
            Node::Local(id) => {
                let slot = self.slot(*id)?;
                if slot.ty != Ty::Val {
                    let raw = self.b.use_var(slot.var);
                    return self.coerce(raw, slot.ty, want);
                }
            }
            Node::If { test, then, else_ } if self.ty_of(node) != Ty::Val => {
                let c = self.compile_cond(test)?;
                let then_b = self.b.create_block();
                let else_b = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, raw_type(want));
                self.b.ins().brif(c, then_b, &[], else_b, &[]);
                self.b.switch_to_block(then_b);
                self.b.seal_block(then_b);
                let a = self.compile_typed(then, want)?;
                self.b.ins().jump(merge, &[BlockArg::Value(a)]);
                self.b.switch_to_block(else_b);
                self.b.seal_block(else_b);
                let e = self.compile_typed(else_, want)?;
                self.b.ins().jump(merge, &[BlockArg::Value(e)]);
                self.b.switch_to_block(merge);
                self.b.seal_block(merge);
                return Ok(self.b.block_params(merge)[0]);
            }
            Node::Let { bindings, body } => {
                let vars = self.bind_all(bindings)?;
                let r = self.compile_typed(body, want)?;
                self.unbind(&vars);
                return Ok(r);
            }
            Node::Do(nodes) if !nodes.is_empty() => {
                for n in &nodes[..nodes.len() - 1] {
                    self.compile(n)?;
                }
                return self.compile_typed(&nodes[nodes.len() - 1], want);
            }
            Node::Recur { target, args } => {
                self.compile_recur(*target, args)?;
                return Ok(self.raw_zero(want));
            }
            Node::Throw { expr, pos } => {
                self.compile_throw(expr, *pos)?;
                return Ok(self.raw_zero(want));
            }
            Node::BuiltinCall { desc, args, pos } => {
                let ty = self.ty_of(node);
                if ty != Ty::Val {
                    let raw = self.compile_prim_builtin(desc, args, ty, *pos)?;
                    return self.coerce(raw, ty, want);
                }
            }
            _ => {}
        }
        // Anything else: evaluate as a value and unbox (a `^long` binding is strict).
        let op = self.compile(node)?;
        self.unbox(op, want, want == Ty::Long)
    }

    /// Evaluate operands left to right to raw bits of `want`: statically primitive ones
    /// directly, the rest as values that are unboxed only after every operand has been
    /// evaluated, so a `ClassCastException` surfaces at the same point as in the
    /// builtin.
    fn prim_args(&mut self, args: &[Node], want: Ty) -> CResult<Vec<Value>> {
        enum Pre {
            Raw(Value),
            Boxed(Value),
        }
        let mut pre = Vec::with_capacity(args.len());
        for a in args {
            pre.push(if self.ty_of(a) != Ty::Val {
                Pre::Raw(self.compile_typed(a, want)?)
            } else {
                Pre::Boxed(self.compile(a)?)
            });
        }
        let mut out: Vec<Option<Value>> = vec![None; pre.len()];
        for (i, p) in pre.into_iter().enumerate().rev() {
            out[i] = Some(match p {
                Pre::Raw(v) => v,
                Pre::Boxed(v) => self.unbox(v, want, want == Ty::Long)?,
            });
        }
        Ok(out.into_iter().map(|v| v.expect("operand")).collect())
    }

    /// Primitive arithmetic for a builtin whose static result type is `ty`.
    fn compile_prim_builtin(
        &mut self,
        desc: &'static BuiltinDesc,
        args: &[Node],
        ty: Ty,
        pos: u32,
    ) -> CResult<Value> {
        // The overflow and divide-by-zero checks of this operation report `pos`; a
        // nested primitive operand sets and restores its own.
        let outer = std::mem::replace(&mut self.prim_pos, pos);
        let r = self.compile_prim_op(desc, args, ty);
        self.prim_pos = outer;
        r
    }

    fn compile_prim_op(
        &mut self,
        desc: &'static BuiltinDesc,
        args: &[Node],
        ty: Ty,
    ) -> CResult<Value> {
        let op = PrimOp::of(desc, args.len()).ok_or_else(|| internal("not a primitive op"))?;
        match op {
            PrimOp::ToDouble => return self.compile_typed(&args[0], Ty::Double),
            PrimOp::ToLong => {
                // `(long d)` truncates, unlike a `^long` binding.
                return match self.ty_of(&args[0]) {
                    Ty::Val => {
                        let o = self.compile(&args[0])?;
                        self.unbox(o, Ty::Long, false)
                    }
                    t => {
                        let raw = self.compile_typed(&args[0], t)?;
                        if t == Ty::Double {
                            // NaN and values outside the long range are errors,
                            // not saturated.
                            let lo = self.b.ins().f64const(-9_223_372_036_854_775_808.0);
                            let hi = self.b.ins().f64const(9_223_372_036_854_775_808.0);
                            let below = self.b.ins().fcmp(FloatCC::UnorderedOrLessThan, raw, lo);
                            let above =
                                self.b
                                    .ins()
                                    .fcmp(FloatCC::UnorderedOrGreaterThanOrEqual, raw, hi);
                            let bad = self.b.ins().bor(below, above);
                            self.arith_check(bad, 3);
                        }
                        Ok(self.convert(raw, t, Ty::Long))
                    }
                };
            }
            PrimOp::Inc | PrimOp::Dec => {
                let a = self.compile_typed(&args[0], ty)?;
                return Ok(if ty == Ty::Double {
                    let one = self.b.ins().f64const(1.0);
                    if op == PrimOp::Inc {
                        self.b.ins().fadd(a, one)
                    } else {
                        self.b.ins().fsub(a, one)
                    }
                } else {
                    let one = self.b.ins().iconst(types::I64, 1);
                    let (r, of) = if op == PrimOp::Inc {
                        self.b.ins().sadd_overflow(a, one)
                    } else {
                        self.b.ins().ssub_overflow(a, one)
                    };
                    self.arith_check(of, 0);
                    r
                });
            }
            PrimOp::Rem | PrimOp::Quot | PrimOp::Mod => {
                let xs = self.prim_args(args, Ty::Long)?;
                let (a, b) = (xs[0], xs[1]);
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, b, 0);
                self.arith_check(zero, 1);
                // `MIN / -1` overflows (and traps in hardware): handle -1 separately.
                let minus_one = self.b.ins().icmp_imm_s(IntCC::Equal, b, -1);
                let one = self.b.ins().iconst(types::I64, 1);
                let safe_b = self.b.ins().select(minus_one, one, b);
                if op == PrimOp::Quot {
                    let min = self.b.ins().icmp_imm_s(IntCC::Equal, a, i64::MIN);
                    let overflow = self.b.ins().band(minus_one, min);
                    self.arith_check(overflow, 0);
                    let neg = self.b.ins().ineg(a);
                    let q = self.b.ins().sdiv(a, safe_b);
                    return Ok(self.b.ins().select(minus_one, neg, q));
                }
                let r = self.b.ins().srem(a, safe_b);
                let zero_v = self.b.ins().iconst(types::I64, 0);
                let r = self.b.ins().select(minus_one, zero_v, r);
                if op == PrimOp::Rem {
                    return Ok(r);
                }
                // mod: the result takes the divisor's sign.
                let x = self.b.ins().bxor(r, b);
                let differ = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, x, 0);
                let nonzero = self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0);
                let adjust = self.b.ins().band(differ, nonzero);
                let adjusted = self.b.ins().iadd(r, b);
                return Ok(self.b.ins().select(adjust, adjusted, r));
            }
            _ => {}
        }
        // + - * / folds over every operand, evaluated before any arithmetic.
        if args.is_empty() {
            return Ok(if op == PrimOp::Mul {
                self.b.ins().iconst(types::I64, 1)
            } else {
                self.b.ins().iconst(types::I64, 0)
            });
        }
        let xs = self.prim_args(args, ty)?;
        if xs.len() == 1 {
            return Ok(match op {
                PrimOp::Sub => {
                    if ty == Ty::Double {
                        self.b.ins().fneg(xs[0])
                    } else {
                        let zero = self.b.ins().iconst(types::I64, 0);
                        let (r, of) = self.b.ins().ssub_overflow(zero, xs[0]);
                        self.arith_check(of, 0);
                        r
                    }
                }
                PrimOp::Div => {
                    let one = self.b.ins().f64const(1.0);
                    self.b.ins().fdiv(one, xs[0])
                }
                _ => xs[0],
            });
        }
        let mut acc = xs[0];
        for &x in &xs[1..] {
            acc = if ty == Ty::Double {
                match op {
                    PrimOp::Add => self.b.ins().fadd(acc, x),
                    PrimOp::Sub => self.b.ins().fsub(acc, x),
                    PrimOp::Mul => self.b.ins().fmul(acc, x),
                    _ => self.b.ins().fdiv(acc, x),
                }
            } else {
                let (r, of) = match op {
                    PrimOp::Add => self.b.ins().sadd_overflow(acc, x),
                    PrimOp::Sub => self.b.ins().ssub_overflow(acc, x),
                    _ => self.b.ins().smul_overflow(acc, x),
                };
                self.arith_check(of, 0);
                r
            };
        }
        Ok(acc)
    }

    /// A comparison or sign test on primitive operands as an i8 condition, when the
    /// static types allow it: any double operand makes it a double comparison (longs
    /// widen, values are unboxed with a check); all-long operands compare as longs.
    /// `=`/`not=` need both types known (a long never equals a double).
    fn prim_cond(&mut self, desc: &'static BuiltinDesc, args: &[Node]) -> CResult<Option<Value>> {
        let Some(op) = PrimOp::of(desc, args.len()).filter(|o| o.is_predicate()) else {
            return Ok(None);
        };
        let tys: Vec<Ty> = args.iter().map(|a| self.ty_of(a)).collect();
        if op.is_sign_test() {
            if tys[0] == Ty::Val {
                return Ok(None);
            }
            let a = self.compile_typed(&args[0], tys[0])?;
            let c = if tys[0] == Ty::Long {
                let cc = match op {
                    PrimOp::Zero => IntCC::Equal,
                    PrimOp::Pos => IntCC::SignedGreaterThan,
                    _ => IntCC::SignedLessThan,
                };
                self.b.ins().icmp_imm_s(cc, a, 0)
            } else {
                let z = self.b.ins().f64const(0.0);
                let cc = match op {
                    PrimOp::Zero => FloatCC::Equal,
                    PrimOp::Pos => FloatCC::GreaterThan,
                    _ => FloatCC::LessThan,
                };
                self.b.ins().fcmp(cc, a, z)
            };
            return Ok(Some(c));
        }
        let any_double = tys.contains(&Ty::Double);
        let all_long = tys.iter().all(|t| *t == Ty::Long);
        if matches!(op, PrimOp::Eq | PrimOp::Ne) {
            if tys[0] == Ty::Val || tys[1] == Ty::Val {
                return Ok(None);
            }
            if tys[0] != tys[1] {
                // Evaluate for effects; a long and a double are never `=`.
                for a in args {
                    self.compile(a)?;
                }
                let r = i64::from(op == PrimOp::Ne);
                return Ok(Some(self.b.ins().iconst(types::I8, r)));
            }
        }
        let (icc, fcc) = match op {
            PrimOp::Lt => (IntCC::SignedLessThan, FloatCC::LessThan),
            PrimOp::Le => (IntCC::SignedLessThanOrEqual, FloatCC::LessThanOrEqual),
            PrimOp::Gt => (IntCC::SignedGreaterThan, FloatCC::GreaterThan),
            PrimOp::Ge => (IntCC::SignedGreaterThanOrEqual, FloatCC::GreaterThanOrEqual),
            PrimOp::Ne => (IntCC::NotEqual, FloatCC::NotEqual),
            _ => (IntCC::Equal, FloatCC::Equal),
        };
        // A boxed operand may be a BigInt beyond the double range, which must not
        // compare as infinity: leave those to the runtime.
        if any_double && !tys.contains(&Ty::Val) {
            let xs = self.prim_args(args, Ty::Double)?;
            Ok(Some(self.b.ins().fcmp(fcc, xs[0], xs[1])))
        } else if all_long {
            let xs = self.prim_args(args, Ty::Long)?;
            Ok(Some(self.b.ins().icmp(icc, xs[0], xs[1])))
        } else {
            Ok(None)
        }
    }

    /// After a call: on `EXC`, continue at the innermost `try` handler, or return
    /// `EXC` from the function. Nothing needs cleaning up on the way.
    fn check_exc(&mut self, r: Value) {
        let is_exc = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, r, api::consts::EXC as i64);
        let exc_block = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(is_exc, exc_block, &[], cont, &[]);
        self.b.switch_to_block(exc_block);
        self.b.seal_block(exc_block);
        self.emit_exc_path(r);
        self.b.switch_to_block(cont);
        self.b.seal_block(cont);
    }

    /// An exception is pending (`r == EXC`): go to the innermost `try` handler, or
    /// return `EXC`. Ends the current block.
    fn emit_exc_path(&mut self, r: Value) {
        match self.handlers.last().copied() {
            Some(block) => {
                self.b.ins().jump(block, &[]);
            }
            None => {
                self.b.ins().return_(&[r]);
            }
        }
    }

    /// Continue emitting in a fresh unreachable block after a terminator.
    fn dead_block(&mut self) {
        let dead = self.b.create_block();
        self.b.switch_to_block(dead);
        self.b.seal_block(dead);
    }

    /// Loop-header check of the interrupt flag (one byte load and branch). The slow
    /// path is a safepoint: a collection may run there.
    fn emit_interrupt_check(&mut self) {
        let addr = self.interrupt_flag_addr();
        let flag = self
            .b
            .ins()
            .load(types::I8, MemFlagsData::trusted(), addr, 0);
        let hit = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(flag, hit, &[], cont, &[]);
        self.b.switch_to_block(hit);
        self.b.seal_block(hit);
        let r = self.call_rt(RtFn::Interrupted, &[]).unwrap();
        // Not necessarily for this thread: `nil` means carry on.
        self.check_exc(r);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
        self.b.seal_block(cont);
    }

    fn truthy(&mut self, v: Value) -> Value {
        let o = self.b.ins().bor_imm_s(v, 2);
        self.b.ins().icmp_imm_s(IntCC::NotEqual, o, 2)
    }

    // ---- nodes --------------------------------------------------------------------

    /// Evaluate `node` to a tagged value.
    fn compile(&mut self, node: &Node) -> CResult<Value> {
        let v = self.compile_node(node)?;
        // Constants are permanent objects and vars too; everything else may be a
        // reference to an object only this frame keeps alive.
        if !matches!(node, Node::Const(_) | Node::TheVar(_)) {
            self.gc(v);
        }
        Ok(v)
    }

    fn compile_node(&mut self, node: &Node) -> CResult<Value> {
        match node {
            Node::Const(v) => self.compile_const(v),
            Node::Local(id) => self.read_local(*id),
            Node::VarRef(v) => {
                let var = v.get();
                let p = self.const_slot(ConstEntry::Var(var.ns.to_string(), var.name.to_string()));
                // Fast path: the var's lock-free word holds the root of a bound,
                // non-dynamic var; otherwise ask the runtime (thread bindings, or the
                // unbound error).
                let fast = self.b.ins().load(
                    types::I64,
                    MemFlagsData::trusted(),
                    p,
                    api::consts::OFF_VAR_FAST,
                );
                let slow_path =
                    self.b
                        .ins()
                        .icmp_imm_s(IntCC::Equal, fast, api::consts::UNBOUND as i64);
                let slow = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, types::I64);
                self.b
                    .ins()
                    .brif(slow_path, slow, &[], merge, &[BlockArg::Value(fast)]);
                self.b.switch_to_block(slow);
                self.b.seal_block(slow);
                let r = self.call_rt(RtFn::VarRoot, &[p]).unwrap();
                self.check_exc(r);
                self.b.ins().jump(merge, &[BlockArg::Value(r)]);
                self.b.switch_to_block(merge);
                self.b.seal_block(merge);
                Ok(self.b.block_params(merge)[0])
            }
            Node::TheVar(v) => {
                let var = v.get();
                Ok(self.const_slot(ConstEntry::Var(var.ns.to_string(), var.name.to_string())))
            }
            Node::CurrentNs => Ok(self.call_rt(RtFn::CurrentNs, &[]).unwrap()),
            Node::Def { var, init, meta } => {
                let vo = var.get();
                let p = self.const_slot(ConstEntry::Var(vo.ns.to_string(), vo.name.to_string()));
                let result = match init {
                    None => p,
                    Some(init) => {
                        let v = self.compile(init)?;
                        self.call_rt(RtFn::VarBindRoot, &[p, v]).unwrap()
                    }
                };
                // Metadata after the init, so a throwing init leaves the var untouched.
                // (The analyzer already applied `^:dynamic` at compile time; the root
                // is republished when the flag changes.)
                if let Some(m) = meta {
                    let mv = self.compile(m)?;
                    self.call_rt(RtFn::VarSetMeta, &[p, mv]);
                }
                Ok(result)
            }
            Node::VarSet { var, value } => {
                let vo = var.get();
                let p = self.const_slot(ConstEntry::Var(vo.ns.to_string(), vo.name.to_string()));
                let v = self.compile(value)?;
                let r = self.call_rt(RtFn::VarSet, &[p, v]).unwrap();
                self.check_exc(r);
                Ok(r)
            }
            Node::If { test, then, else_ } => self.compile_if(test, then, else_),
            Node::Do(nodes) => {
                let mut last = self.imm(api::consts::NIL);
                for n in nodes {
                    last = self.compile(n)?;
                }
                Ok(last)
            }
            Node::Let { bindings, body } => self.compile_let(bindings, body),
            Node::Loop {
                id,
                bindings,
                body,
                pos,
            } => self.compile_loop(*id, bindings, body, *pos),
            Node::Recur { target, args } => self.compile_recur(*target, args),
            Node::Fn(f) => self.compile_fn_value(f),
            Node::Call { f, args, pos } => self.compile_call(f, args, *pos),
            Node::BuiltinCall { desc, args, pos } => {
                let ty = self.ty_of(node);
                if ty != Ty::Val {
                    let raw = self.compile_prim_builtin(desc, args, ty, *pos)?;
                    return Ok(self.box_raw(raw, ty));
                }
                self.compile_builtin_call(desc, args, false, *pos)
            }
            Node::Coll { kind, items } => self.compile_coll(*kind, items),
            Node::Throw { expr, pos } => self.compile_throw(expr, *pos),
            Node::Try {
                body,
                catches,
                finally,
            } => self.compile_try(body, catches, finally.as_deref()),
            Node::NativeCall {
                symbol,
                ret,
                params,
                args,
            } => self.compile_native_call(symbol, *ret, params, args),
        }
    }

    fn compile_throw(&mut self, e: &Node, pos: u32) -> CResult<Value> {
        let v = self.compile(e)?;
        self.srcloc(pos);
        let r = self.call_rt(RtFn::Throw, &[v]).unwrap();
        self.srcloc(0);
        self.emit_exc_path(r);
        self.dead_block();
        Ok(self.imm(api::consts::NIL))
    }

    /// Run `region` with a handler: on an exception control reaches `handler` with
    /// the taken exception. `handler` must end its block (jump to `merge` with a
    /// value, or propagate).
    fn protect(
        &mut self,
        region: &mut dyn FnMut(&mut Self) -> CResult<Value>,
        handler: &mut dyn FnMut(&mut Self, Value, Block) -> CResult<()>,
    ) -> CResult<Value> {
        let handler_block = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        self.handlers.push(handler_block);
        let v = region(self);
        self.handlers.pop();
        let v = v?;
        self.b.ins().jump(merge, &[BlockArg::Value(v)]);

        self.b.switch_to_block(handler_block);
        self.b.seal_block(handler_block);
        let e = self.call_rt(RtFn::TakeException, &[]).unwrap();
        let e = self.gc(e);
        handler(self, e, merge)?;

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        let r = self.b.block_params(merge)[0];
        Ok(self.gc(r))
    }

    fn compile_try(
        &mut self,
        body: &Node,
        catches: &[CatchClause],
        finally: Option<&Node>,
    ) -> CResult<Value> {
        if catches.is_empty() && finally.is_none() {
            return self.compile(body);
        }
        let mut inner = |fc: &mut Self| -> CResult<Value> {
            if catches.is_empty() {
                return fc.compile(body);
            }
            fc.protect(&mut |fc| fc.compile(body), &mut |fc, e, merge| {
                fc.catch_dispatch(e, merge, catches)
            })
        };
        match finally {
            None => inner(self),
            Some(fin) => {
                let v = self.protect(&mut inner, &mut |fc, e, _merge| {
                    // Exceptional path: run finally, then rethrow. If finally
                    // throws, its exception wins.
                    fc.compile(fin)?;
                    let r = fc.call_rt(RtFn::Throw, &[e]).unwrap();
                    fc.emit_exc_path(r);
                    Ok(())
                })?;
                // Normal path: finally runs for effect; the result is the body's.
                self.compile(fin)?;
                Ok(v)
            }
        }
    }

    /// Handler body for `catch` clauses: match classes in order, bind, run, merge;
    /// rethrow when nothing matches.
    fn catch_dispatch(&mut self, e: Value, merge: Block, catches: &[CatchClause]) -> CResult<()> {
        for clause in catches {
            let (cp, cl) = self.blob_ptr(&clause.class);
            let m = self.call_rt(RtFn::ExceptionInstance, &[e, cp, cl]).unwrap();
            let clause_b = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(m, clause_b, &[], next, &[]);

            self.b.switch_to_block(clause_b);
            self.b.seal_block(clause_b);
            self.bind_local(clause.local, e);
            let r = self.compile(&clause.body)?;
            self.locals.remove(&clause.local);
            self.b.ins().jump(merge, &[BlockArg::Value(r)]);

            self.b.switch_to_block(next);
            self.b.seal_block(next);
        }
        let r = self.call_rt(RtFn::Throw, &[e]).unwrap();
        self.emit_exc_path(r);
        Ok(())
    }

    fn native_cl_type(t: NativeTy) -> cranelift_codegen::ir::Type {
        match t {
            NativeTy::Void => types::INVALID,
            NativeTy::Long | NativeTy::Str | NativeTy::Ptr => types::I64,
            NativeTy::Int | NativeTy::UInt => types::I32,
            NativeTy::Double => types::F64,
            NativeTy::Float => types::F32,
            NativeTy::Bool => types::I8,
        }
    }

    fn compile_native_call(
        &mut self,
        symbol: &str,
        ret: NativeTy,
        params: &[NativeTy],
        args: &[Node],
    ) -> CResult<Value> {
        let ops = self.eval_args(args)?;
        let fptr = self.const_slot(ConstEntry::Native(symbol.to_string()));
        let scratch = self.scratch_slot();
        let scratch_addr = self.b.ins().stack_addr(types::I64, scratch, 0);
        let mut sig = self.module.make_signature();
        let mut cargs = Vec::with_capacity(ops.len());
        for (op, ty) in ops.iter().zip(params) {
            let v = match ty {
                NativeTy::Long | NativeTy::Ptr | NativeTy::Int | NativeTy::UInt => {
                    let r = self.call_rt(RtFn::ToLong, &[*op, scratch_addr]).unwrap();
                    self.check_exc(r);
                    let n = self.b.ins().stack_load(types::I64, types::I64, scratch, 0);
                    if matches!(ty, NativeTy::Int | NativeTy::UInt) {
                        self.b.ins().ireduce(types::I32, n)
                    } else {
                        n
                    }
                }
                NativeTy::Double | NativeTy::Float => {
                    let r = self.call_rt(RtFn::ToDouble, &[*op, scratch_addr]).unwrap();
                    self.check_exc(r);
                    let d = self.b.ins().stack_load(types::I64, types::F64, scratch, 0);
                    if *ty == NativeTy::Float {
                        self.b.ins().fdemote(types::F32, d)
                    } else {
                        d
                    }
                }
                NativeTy::Bool => self.truthy(*op),
                NativeTy::Str => {
                    let r = self.call_rt(RtFn::ToCstr, &[*op, scratch_addr]).unwrap();
                    self.check_exc(r);
                    self.b.ins().stack_load(types::I64, types::I64, scratch, 0)
                }
                NativeTy::Void => return Err(internal("void argument")),
            };
            let mut p = AbiParam::new(Self::native_cl_type(*ty));
            if *ty == NativeTy::Bool {
                p = p.uext();
            }
            sig.params.push(p);
            cargs.push(v);
        }
        if ret != NativeTy::Void {
            sig.returns.push(AbiParam::new(Self::native_cl_type(ret)));
        }
        let sref = self.b.import_signature(sig);
        let inst = self.b.ins().call_indirect(sref, fptr, &cargs);
        let result = match ret {
            NativeTy::Void => self.imm(api::consts::NIL),
            NativeTy::Long | NativeTy::Ptr => {
                let r = self.b.inst_results(inst)[0];
                self.call_rt(RtFn::BoxLong, &[r]).unwrap()
            }
            NativeTy::Int => {
                let r = self.b.inst_results(inst)[0];
                let r = self.b.ins().sextend(types::I64, r);
                self.call_rt(RtFn::BoxLong, &[r]).unwrap()
            }
            NativeTy::UInt => {
                let r = self.b.inst_results(inst)[0];
                let r = self.b.ins().uextend(types::I64, r);
                self.call_rt(RtFn::BoxLong, &[r]).unwrap()
            }
            NativeTy::Double => {
                let r = self.b.inst_results(inst)[0];
                self.call_rt(RtFn::BoxDouble, &[r]).unwrap()
            }
            NativeTy::Float => {
                let r = self.b.inst_results(inst)[0];
                let r = self.b.ins().fpromote(types::F64, r);
                self.call_rt(RtFn::BoxDouble, &[r]).unwrap()
            }
            NativeTy::Bool => {
                let r = self.b.inst_results(inst)[0];
                let c = self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0);
                let t = self.b.ins().iconst(types::I64, api::consts::TRUE as i64);
                let f = self.b.ins().iconst(types::I64, api::consts::FALSE as i64);
                self.b.ins().select(c, t, f)
            }
            NativeTy::Str => {
                let r = self.b.inst_results(inst)[0];
                self.call_rt(RtFn::StringFromCstr, &[r]).unwrap()
            }
        };
        Ok(result)
    }

    fn compile_const(&mut self, v: &rscl_runtime::Value) -> CResult<Value> {
        if !v.is_ptr() {
            return Ok(self.imm(v.bits()));
        }
        let entry = match v.kind().unwrap() {
            Kind::Long => ConstEntry::Long(obj::long::get(v)),
            Kind::Double => ConstEntry::Double(obj::double::get(v).to_bits()),
            Kind::String => ConstEntry::Str(obj::string::get(v).to_string()),
            Kind::Keyword => {
                let k = obj::keyword::get(v);
                ConstEntry::Keyword(k.ns().map(str::to_string), k.name().to_string())
            }
            // A symbol with metadata goes through the reader, which restores it.
            Kind::Symbol
                if obj::map::as_map(&obj::symbol::get(v).meta).is_some_and(|m| !m.m.is_empty()) =>
            {
                ConstEntry::Edn(printer::pr_str_meta(v))
            }
            Kind::Symbol => {
                let s = obj::symbol::get(v);
                ConstEntry::Symbol(s.ns().map(str::to_string), s.name().to_string())
            }
            Kind::List
            | Kind::Vector
            | Kind::Map
            | Kind::Set
            | Kind::Regex
            | Kind::Cons
            | Kind::LazySeq
            | Kind::VecSeq
            | Kind::BigInt => ConstEntry::Edn(printer::pr_str_meta(v)),
            k => {
                return Err(internal(format!(
                    "cannot embed a {} as a constant",
                    k.name()
                )));
            }
        };
        Ok(self.const_slot(entry))
    }

    /// An i8 condition for `test`: inline comparisons skip the boolean value.
    fn compile_cond(&mut self, test: &Node) -> CResult<Value> {
        if let Node::BuiltinCall { desc, args, pos } = test
            && (inline_op(desc, args.len()).is_some() || self.ty_of(test) == Ty::Val)
        {
            return self.compile_builtin_call(desc, args, true, *pos);
        }
        let t = self.compile(test)?;
        Ok(self.truthy(t))
    }

    fn compile_if(&mut self, test: &Node, then: &Node, else_: &Node) -> CResult<Value> {
        let c = self.compile_cond(test)?;
        let then_b = self.b.create_block();
        let else_b = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        self.b.ins().brif(c, then_b, &[], else_b, &[]);

        self.b.switch_to_block(then_b);
        self.b.seal_block(then_b);
        let a = self.compile(then)?;
        self.b.ins().jump(merge, &[BlockArg::Value(a)]);

        self.b.switch_to_block(else_b);
        self.b.seal_block(else_b);
        let e = self.compile(else_)?;
        self.b.ins().jump(merge, &[BlockArg::Value(e)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        Ok(self.b.block_params(merge)[0])
    }

    fn bind_all(&mut self, bindings: &[(LocalId, Node)]) -> CResult<Vec<(LocalId, Variable)>> {
        let mut out = Vec::with_capacity(bindings.len());
        for (id, init) in bindings {
            let ty = self.local_ty(*id);
            let var = if ty != Ty::Val {
                let raw = self.compile_typed(init, ty)?;
                self.bind_typed(*id, raw, ty)
            } else {
                let v = self.compile(init)?;
                self.bind_local(*id, v)
            };
            out.push((*id, var));
        }
        Ok(out)
    }

    fn compile_let(&mut self, bindings: &[(LocalId, Node)], body: &Node) -> CResult<Value> {
        let vars = self.bind_all(bindings)?;
        let res = self.compile(body)?;
        self.unbind(&vars);
        Ok(res)
    }

    fn compile_loop(
        &mut self,
        id: LoopId,
        bindings: &[(LocalId, Node)],
        body: &Node,
        pos: u32,
    ) -> CResult<Value> {
        let vars = self.bind_all(bindings)?;
        let header = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.b.switch_to_block(header);
        // An interrupt (Ctrl-C, cancellation, the heap limit) taken here is reported
        // at the loop.
        self.srcloc(pos);
        self.emit_interrupt_check();
        self.srcloc(0);
        self.loops.insert(
            id,
            LoopCtx {
                header,
                vars: vars
                    .iter()
                    .map(|(id, v)| (*v, self.local_ty(*id)))
                    .collect(),
            },
        );
        let res = self.compile(body);
        self.loops.remove(&id);
        self.b.seal_block(header);
        let res = res?;
        self.unbind(&vars);
        Ok(res)
    }

    fn compile_recur(&mut self, target: RecurTarget, args: &[Node]) -> CResult<Value> {
        let ctx = match target {
            RecurTarget::Loop(id) => self.loops.get(&id).cloned(),
            RecurTarget::Fn => self.fn_recur.clone(),
        }
        .ok_or_else(|| internal("recur target missing"))?;
        // Every argument is evaluated before any variable is rebound.
        let mut vals = Vec::with_capacity(args.len());
        for (a, (_, ty)) in args.iter().zip(&ctx.vars) {
            vals.push(if *ty == Ty::Val {
                self.compile(a)?
            } else {
                self.compile_typed(a, *ty)?
            });
        }
        for ((var, _), val) in ctx.vars.iter().zip(vals) {
            self.b.def_var(*var, val);
        }
        self.b.ins().jump(ctx.header, &[]);
        self.dead_block();
        Ok(self.imm(api::consts::NIL))
    }

    fn compile_fn_value(&mut self, f: &Rc<FnNode>) -> CResult<Value> {
        let func = self.unit.fn_id(self.module, f)?;
        if f.captures.is_empty() {
            return Ok(self.const_slot(ConstEntry::FnObj {
                func,
                name: f.name.clone(),
                mask: f.arity_mask(),
                vmin: f.variadic_min(),
            }));
        }
        let proto = self.const_slot(ConstEntry::Proto {
            func,
            name: f.name.clone(),
            mask: f.arity_mask(),
            vmin: f.variadic_min(),
        });
        let n = self.b.ins().iconst(types::I64, f.captures.len() as i64);
        let fv = self.call_rt(RtFn::FnNew, &[proto, n]).unwrap();
        let fv = self.gc(fv);
        for (i, id) in f.captures.iter().enumerate() {
            let v = self.read_local(*id)?;
            let i = self.b.ins().iconst(types::I64, i as i64);
            self.call_rt(RtFn::FnSetCapture, &[fv, i, v]);
        }
        Ok(fv)
    }

    /// Evaluate arguments left to right. Each is a declared reference, so the ones
    /// already evaluated are in the stack maps of the calls that evaluate the rest.
    fn eval_args(&mut self, items: &[Node]) -> CResult<Vec<Value>> {
        items.iter().map(|a| self.compile(a)).collect()
    }

    /// Write evaluated arguments into the argv slot; returns (addr, count). From
    /// here to the call nothing else may be emitted that calls: the slot is not in
    /// any stack map, and the callee takes the arguments over (a compiled fn loads
    /// them into its own variables before its first safepoint; a native that lets
    /// collections run roots its argument list).
    fn store_ops(&mut self, ops: &[Value]) -> (Value, Value) {
        let addr = self.argv_addr();
        for (i, op) in ops.iter().enumerate() {
            self.b
                .ins()
                .store(MemFlagsData::trusted(), *op, addr, 8 * i as i32);
        }
        let n = self.b.ins().iconst(types::I64, ops.len() as i64);
        (addr, n)
    }

    /// Direct call of a Rust builtin, with an inline fixnum fast path for the
    /// arithmetic and comparison operators. With `as_cond`, the result is an i8
    /// condition (for `if`) instead of a value.
    fn compile_builtin_call(
        &mut self,
        desc: &'static BuiltinDesc,
        args: &[Node],
        as_cond: bool,
        pos: u32,
    ) -> CResult<Value> {
        if let Some(c) = self.prim_cond(desc, args)? {
            return Ok(if as_cond {
                c
            } else {
                let t = self.b.ins().iconst(types::I64, api::consts::TRUE as i64);
                let f = self.b.ins().iconst(types::I64, api::consts::FALSE as i64);
                self.b.ins().select(c, t, f)
            });
        }
        let ops = self.eval_args(args)?;
        match inline_op(desc, args.len()) {
            Some(op) => self.compile_inline(op, desc, &ops, as_cond, pos),
            None => {
                let r = self.direct_call(desc, &ops, pos)?;
                Ok(if as_cond { self.truthy(r) } else { r })
            }
        }
    }

    /// `builtin(null, argc, argv)` with the exception check.
    fn direct_call(
        &mut self,
        desc: &'static BuiltinDesc,
        ops: &[Value],
        pos: u32,
    ) -> CResult<Value> {
        let (argv, argc) = self.store_ops(ops);
        let fref = self.builtin_ref(desc)?;
        let this = self.b.ins().iconst(types::I64, 0);
        self.srcloc(pos);
        let inst = self.b.ins().call(fref, &[this, argc, argv]);
        self.srcloc(0);
        let r = self.b.inst_results(inst)[0];
        self.check_exc(r);
        Ok(self.gc(r))
    }

    /// Fixnum fast path for `op`, falling back to the builtin for anything else
    /// (boxed longs, doubles, non-numbers, overflow).
    fn compile_inline(
        &mut self,
        op: PrimOp,
        desc: &'static BuiltinDesc,
        ops: &[Value],
        as_cond: bool,
        pos: u32,
    ) -> CResult<Value> {
        let a = ops[0];
        let b = ops.get(1).copied();
        let tagged = match b {
            Some(b) => self.b.ins().band(a, b),
            None => a,
        };
        let is_fix = self.b.ins().band_imm_s(tagged, 1);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let merge = self.b.create_block();
        let merge_ty = if as_cond { types::I8 } else { types::I64 };
        self.b.append_block_param(merge, merge_ty);
        self.b.ins().brif(is_fix, fast, &[], slow, &[]);

        self.b.switch_to_block(fast);
        self.b.seal_block(fast);
        match op {
            PrimOp::Add | PrimOp::Sub | PrimOp::Mul | PrimOp::Inc | PrimOp::Dec => {
                // a = 2x+1, b = 2y+1: the tagged forms combine with one adjustment;
                // a 64-bit overflow means the result leaves the 63-bit fixnum range.
                let (res, of) = match op {
                    PrimOp::Add => {
                        let b1 = self.b.ins().iadd_imm_s(b.unwrap(), -1);
                        self.b.ins().sadd_overflow(a, b1)
                    }
                    PrimOp::Sub => {
                        let b1 = self.b.ins().iadd_imm_s(b.unwrap(), -1);
                        self.b.ins().ssub_overflow(a, b1)
                    }
                    PrimOp::Mul => {
                        let x = self.b.ins().sshr_imm_s(a, 1);
                        let y2 = self.b.ins().iadd_imm_s(b.unwrap(), -1);
                        let (p, of) = self.b.ins().smul_overflow(x, y2);
                        (self.b.ins().bor_imm_s(p, 1), of)
                    }
                    PrimOp::Inc => {
                        let two = self.b.ins().iconst(types::I64, 2);
                        self.b.ins().sadd_overflow(a, two)
                    }
                    _ => {
                        let two = self.b.ins().iconst(types::I64, 2);
                        self.b.ins().ssub_overflow(a, two)
                    }
                };
                let res = if as_cond { self.truthy(res) } else { res };
                self.b
                    .ins()
                    .brif(of, slow, &[], merge, &[BlockArg::Value(res)]);
            }
            _ => {
                let c = match op {
                    PrimOp::Lt => self.b.ins().icmp(IntCC::SignedLessThan, a, b.unwrap()),
                    PrimOp::Le => self
                        .b
                        .ins()
                        .icmp(IntCC::SignedLessThanOrEqual, a, b.unwrap()),
                    PrimOp::Gt => self.b.ins().icmp(IntCC::SignedGreaterThan, a, b.unwrap()),
                    PrimOp::Ge => self
                        .b
                        .ins()
                        .icmp(IntCC::SignedGreaterThanOrEqual, a, b.unwrap()),
                    PrimOp::Eq | PrimOp::NumEq => self.b.ins().icmp(IntCC::Equal, a, b.unwrap()),
                    PrimOp::Ne => self.b.ins().icmp(IntCC::NotEqual, a, b.unwrap()),
                    PrimOp::Zero => self.b.ins().icmp_imm_s(IntCC::Equal, a, 1),
                    PrimOp::Pos => self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, a, 1),
                    PrimOp::Neg => self.b.ins().icmp_imm_s(IntCC::SignedLessThan, a, 0),
                    _ => unreachable!(),
                };
                let res = if as_cond {
                    c
                } else {
                    let t = self.b.ins().iconst(types::I64, api::consts::TRUE as i64);
                    let f = self.b.ins().iconst(types::I64, api::consts::FALSE as i64);
                    self.b.ins().select(c, t, f)
                };
                self.b.ins().jump(merge, &[BlockArg::Value(res)]);
            }
        }

        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let r = self.direct_call(desc, ops, pos)?;
        let r = if as_cond { self.truthy(r) } else { r };
        self.b.ins().jump(merge, &[BlockArg::Value(r)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        let r = self.b.block_params(merge)[0];
        // Arithmetic may yield a boxed long from the builtin.
        Ok(if as_cond { r } else { self.gc(r) })
    }

    fn compile_call(&mut self, f: &Node, args: &[Node], pos: u32) -> CResult<Value> {
        let fv = self.compile(f)?;
        let ops = self.eval_args(args)?;
        let (argv, argc) = self.store_ops(&ops);

        // Fast path: a heap pointer whose kind is Fn → call its entry directly.
        let low = self.b.ins().band_imm_s(fv, 3);
        let tag_ok = self.b.ins().icmp_imm_s(IntCC::Equal, low, 0);
        let nonzero = self.b.ins().icmp_imm_s(IntCC::NotEqual, fv, 0);
        let is_ptr = self.b.ins().band(tag_ok, nonzero);
        let kind_b = self.b.create_block();
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        self.b.ins().brif(is_ptr, kind_b, &[], slow, &[]);

        self.b.switch_to_block(kind_b);
        self.b.seal_block(kind_b);
        let kind = self.b.ins().load(
            types::I32,
            MemFlagsData::trusted(),
            fv,
            api::consts::OFF_KIND,
        );
        let is_fn = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, api::consts::KIND_FN as i64);
        self.b.ins().brif(is_fn, fast, &[], slow, &[]);

        self.b.switch_to_block(fast);
        self.b.seal_block(fast);
        let proto = self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            fv,
            api::consts::OFF_PROTO,
        );
        let entry = self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            proto,
            api::consts::PROTO_OFF_ENTRY,
        );
        self.srcloc(pos);
        let inst = self
            .b
            .ins()
            .call_indirect(self.call_sig, entry, &[fv, argc, argv]);
        self.srcloc(0);
        let r1 = self.b.inst_results(inst)[0];
        self.b.ins().jump(merge, &[BlockArg::Value(r1)]);

        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        self.srcloc(pos);
        let r2 = self.call_rt(RtFn::Invoke, &[fv, argc, argv]).unwrap();
        self.srcloc(0);
        self.b.ins().jump(merge, &[BlockArg::Value(r2)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        let r = self.b.block_params(merge)[0];
        self.check_exc(r);
        Ok(self.gc(r))
    }

    fn compile_coll(&mut self, kind: CollKind, items: &[Node]) -> CResult<Value> {
        let ops = self.eval_args(items)?;
        let (addr, n) = self.store_ops(&ops);
        let f = match kind {
            CollKind::Vector => RtFn::VectorFromSlice,
            CollKind::Map => RtFn::MapFromSlice,
            CollKind::Set => RtFn::SetFromSlice,
        };
        let r = self.call_rt(f, &[addr, n]).unwrap();
        Ok(self.gc(r))
    }

    /// Body of one fn arity: bind params (and rest), check the stack, compile the
    /// body as the recur target, return.
    fn compile_arity(&mut self, a: &Arity, argv: Value, argc: Value, pos: u32) -> CResult<()> {
        // The arguments move from the caller's argument slots into this function's
        // own variables before anything here can reach a safepoint: the collector
        // does not look at the caller's slots.
        let vals: Vec<Value> = (0..a.params.len())
            .map(|j| {
                let v = self
                    .b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), argv, 8 * j as i32);
                self.gc(v)
            })
            .collect();
        let rest = a.rest.map(|_| {
            let nfixed = a.params.len() as i64;
            let extra = self.b.ins().iadd_imm_s(argc, -nfixed);
            let addr = self.b.ins().iadd_imm_s(argv, 8 * nfixed);
            let r = self.call_rt(RtFn::ListFromSlice, &[addr, extra]).unwrap();
            self.gc(r)
        });
        let mut vars: Vec<(LocalId, Variable)> = Vec::new();
        for (id, v) in a.params.iter().zip(&vals) {
            let ty = self.local_ty(*id);
            let var = if ty != Ty::Val {
                // ClassCastException for anything else, like a primitive-hinted
                // Clojure fn.
                let raw = self.unbox(*v, ty, true)?;
                self.bind_typed(*id, raw, ty)
            } else {
                self.bind_local(*id, *v)
            };
            vars.push((*id, var));
        }
        if let (Some(id), Some(r)) = (a.rest, rest) {
            let var = self.bind_local(id, r);
            vars.push((id, var));
        }
        // Stack-depth and interrupt guard: a safepoint.
        let sp = self.b.ins().get_stack_pointer(types::I64);
        self.srcloc(pos);
        let guard = self.call_rt(RtFn::StackCheck, &[sp]).unwrap();
        self.srcloc(0);
        self.check_exc(guard);

        let header = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.b.switch_to_block(header);
        // A fn that loops with `recur` must stay interruptible, like a `loop`.
        if a.has_recur {
            self.srcloc(pos);
            self.emit_interrupt_check();
            self.srcloc(0);
        }
        self.fn_recur = Some(LoopCtx {
            header,
            vars: vars
                .iter()
                .map(|(id, v)| (*v, self.local_ty(*id)))
                .collect(),
        });
        let res = self.compile(&a.body);
        self.fn_recur = None;
        self.b.seal_block(header);
        let res = res?;
        self.unbind(&vars);
        self.b.ins().return_(&[res]);
        Ok(())
    }
}
