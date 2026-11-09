//! `Node` → Cranelift IR, generic over `cranelift_module::Module` so the same code
//! serves the JIT and the AOT object writer.
//!
//! Invariants (see the plan): no raw address is embedded in IR; every call is followed
//! by an `EXC` check whose cleanup path releases every live owned operand; operands
//! carry their ownership (`Own`) and consumers decide whether to retain or release.
use std::collections::HashMap;
use std::rc::Rc;

mod liveness;
use liveness::Liveness;

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
    /// The `fn*` functions compiled for this unit (for stack-trace registration).
    pub fns: Vec<CompiledFn>,
}

#[derive(Clone, Debug)]
pub struct CompiledFn {
    pub id: FuncId,
    pub name: String,
    pub size: u32,
}

pub struct Compiler<M: Module> {
    pub module: M,
    fb_ctx: FunctionBuilderContext,
    rt_ids: HashMap<RtFn, FuncId>,
    builtin_ids: HashMap<&'static str, FuncId>,
    debug_ir: bool,
    next_unit: u32,
    /// The runtime's `rscl_interrupt_flag` (an imported data symbol).
    flag_id: Option<DataId>,
    /// Every `fn*` compiled into this module (AOT registers them for stack traces).
    registered_fns: Vec<CompiledFn>,
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
    /// An immortal fn object for a capture-less fn: created once per unit, shared by
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

/// The uniform calling convention: `(f, argc, argv) -> value`.
/// `entry(f, argc, argv, owned) -> Value`: see `rscl_runtime::obj::func::Entry`.
pub fn fn_signature<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    for _ in 0..4 {
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
            fb_ctx: FunctionBuilderContext::new(),
            rt_ids: HashMap::new(),
            builtin_ids: HashMap::new(),
            debug_ir: std::env::var_os("RSCL_DEBUG_IR").is_some(),
            next_unit: 0,
            flag_id: None,
            registered_fns: Vec::new(),
        }
    }

    pub fn into_module(self) -> M {
        self.module
    }

    /// Compile one top-level form into `init` and `body` functions plus the functions
    /// for every `fn*` it contains.
    pub fn compile_unit(&mut self, node: &Node, an: &Analyzer) -> CResult<UnitIds> {
        let mut u = self.new_unit()?;
        let body = self.compile_body_fn(&mut u, node, &an.locals)?;
        while let Some((id, f)) = u.pending_fns.pop() {
            self.compile_fn(&mut u, id, &f, &an.locals)?;
        }
        self.finish_unit(&mut u)?;
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
            run_step(fc, own_init, fail, false);
            for unit in units {
                run_step(fc, unit.init, fail, false);
                run_step(fc, unit.body, fail, true);
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
        })
    }

    /// Compile the unit's `init` and define its data objects (after every function
    /// that may add constants has been compiled).
    fn finish_unit(&mut self, u: &mut UnitCtx) -> CResult<()> {
        self.compile_init(u)?;
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
            run_step(fc, own_init, fail, false);
            // Code ranges of every compiled fn, so exceptions carry fn-name traces.
            for f in &fns {
                let fref = fc.func_ref(f.id);
                let addr = fc.b.ins().func_addr(types::I64, fref);
                let size = fc.b.ins().iconst(types::I64, f.size as i64);
                let (np, nl) = fc.blob_ptr(&f.name);
                fc.call_rt(RtFn::RegisterCode, &[addr, size, np, nl]);
            }
            for unit in core_units {
                run_step(fc, unit.init, fail, false);
                run_step(fc, unit.body, fail, true);
            }
            for (key, f) in loaders {
                let (np, nl) = fc.blob_ptr(key);
                let fref = fc.func_ref(*f);
                let addr = fc.b.ins().func_addr(types::I64, fref);
                fc.call_rt(RtFn::RegisterNsLoader, &[np, nl, addr]);
            }
            for unit in main_units {
                run_step(fc, unit.init, fail, false);
                run_step(fc, unit.body, fail, true);
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
        let mut ctx = self.module.make_context();
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
            body(&mut fc)?;
            debug_assert!(
                fc.cleanup.is_empty(),
                "owned-temporary stack not empty at exit of {name}"
            );
            let config = fc.module.target_config();
            fc.b.finalize(config);
        }
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
        if record {
            let size = ctx.compiled_code().map_or(0, |c| c.code_info().total_size);
            let f = CompiledFn {
                id: func_id,
                name: name.to_string(),
                size,
            };
            u.fns.push(f.clone());
            self.registered_fns.push(f);
        }
        self.module.clear_context(&mut ctx);
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
            fc.liveness = liveness::last_uses(node);
            let op = fc.compile(node)?;
            let op = fc.make_owned(op);
            fc.b.ins().return_(&[op.val]);
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
            let (fv, argc, argv, owned) = (params[0], params[1], params[2], params[3]);
            fc.start(argv_cap, locals)?;
            for a in &f.arities {
                fc.liveness.extend(liveness::last_uses(&a.body));
            }
            // Stack-depth and interrupt guard. No locals are bound yet, but the
            // arguments the caller handed over must still be released on failure.
            let sp = fc.b.ins().get_stack_pointer(types::I64);
            let guard = fc.call_rt(RtFn::StackCheck, &[sp]).unwrap();
            let is_exc =
                fc.b.ins()
                    .icmp_imm_s(IntCC::Equal, guard, api::consts::EXC as i64);
            let fail = fc.b.create_block();
            let ok = fc.b.create_block();
            fc.b.ins().brif(is_exc, fail, &[], ok, &[]);
            fc.b.switch_to_block(fail);
            fc.b.seal_block(fail);
            fc.call_rt(RtFn::ReleaseArgs, &[argv, argc, owned]);
            fc.b.ins().return_(&[guard]);
            fc.b.switch_to_block(ok);
            fc.b.seal_block(ok);

            // Captures live in the fn object; the fn object is alive for the whole call.
            for (i, id) in f.captures.iter().enumerate() {
                let v = fc.b.ins().load(
                    types::I64,
                    MemFlagsData::trusted(),
                    fv,
                    api::consts::OFF_CAPTURES + 8 * i as i32,
                );
                fc.bind_local(*id, v, false);
            }
            if let Some(sl) = f.self_local {
                fc.bind_local(sl, fv, false);
            }

            // Arity dispatch.
            let blocks: Vec<Block> = f.arities.iter().map(|_| fc.b.create_block()).collect();
            for (i, a) in f.arities.iter().enumerate() {
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
            fc.call_rt(RtFn::ReleaseArgs, &[argv, argc, owned]);
            fc.b.ins().return_(&[r]);

            for (i, a) in f.arities.iter().enumerate() {
                fc.b.switch_to_block(blocks[i]);
                fc.b.seal_block(blocks[i]);
                fc.compile_arity(a, argv, argc, owned)?;
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
                    fc.call_rt(RtFn::MakeImmortal, &[v]);
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
fn run_step<M: Module>(fc: &mut FnCtx<'_, M>, f: FuncId, fail: Block, release_result: bool) {
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
    if release_result {
        fc.call_rt(RtFn::Release, &[v]);
    }
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
        Node::Def { init, .. } => init.as_ref().map_or(0, |n| max_args(n)),
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
        Node::Throw(e) => max_args(e),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Own {
    /// An immediate (nil, bool, char, fixnum): refcounting is a no-op.
    Imm,
    /// Belongs to someone else (a local, a capture, a constant table entry).
    Borrowed,
    /// A +1 reference the holder must release or hand over.
    Owned,
    /// A parameter passed on at its last use: owned exactly when the caller handed
    /// it over, which the runtime flag (non-zero: owned) records. Flows into the
    /// callee's mask unchanged, so pass-through of a borrowed argument costs nothing.
    Cond(Value),
}

#[derive(Clone, Copy, Debug)]
struct Operand {
    val: Value,
    own: Own,
}

impl Operand {
    fn imm(val: Value) -> Self {
        Operand { val, own: Own::Imm }
    }
    fn borrowed(val: Value) -> Self {
        Operand {
            val,
            own: Own::Borrowed,
        }
    }
    fn owned(val: Value) -> Self {
        Operand {
            val,
            own: Own::Owned,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Cleanup {
    Val(Value),
    Var(Variable),
    /// A parameter: owned only when its bit is set in the function's ownership mask.
    Param(Variable, u64),
    /// An already-read parameter value with its mask bit.
    ValIf(Value, u64),
    /// A temporary owned when the flag value is non-zero (`Own::Cond`).
    ValIfFlag(Value, Value),
}

/// How a value slot owns its contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owning {
    /// Borrowed from elsewhere (a capture, the fn object itself).
    No,
    /// A +1 reference released at the end of the scope.
    Yes,
    /// A parameter: owned when its bit is set in the function's ownership mask.
    Param(u64),
}

#[derive(Clone, Copy)]
struct LoopVar {
    var: Variable,
    /// Primitive loop variables hold raw bits.
    ty: Ty,
    /// For fn parameters: the bit in the ownership mask (set once `recur` rebinds).
    param_bit: Option<u64>,
}

#[derive(Clone)]
struct LoopCtx {
    header: Block,
    vars: Vec<LoopVar>,
    /// `cleanup.len()` right after the loop variables were bound: everything above it
    /// was bound or produced inside the body and must be released before `recur`.
    depth: usize,
}

#[derive(Clone, Copy)]
struct Slot {
    var: Variable,
    /// `Val`: a tagged value. `Long`/`Double`: raw `i64`/`f64` in the variable.
    ty: Ty,
    owning: Owning,
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
    cleanup: Vec<Cleanup>,
    loops: HashMap<LoopId, LoopCtx>,
    fn_recur: Option<LoopCtx>,
    argv_slot: Option<StackSlot>,
    table_base: Value,
    blob_base: Value,
    call_sig: SigRef,
    /// The analyzer's locals table (names for diagnostics, types for inference).
    local_infos: Vec<LocalInfo>,
    ty_cache: TyCache,
    /// Last uses of locals (see `liveness`): the value is transferred out and the
    /// variable cleared.
    liveness: Liveness,
    /// The `owned` argument mask, as a variable so `recur` can set bits.
    owned_mask: Option<Variable>,
    /// Active `try` handlers, innermost last.
    handlers: Vec<Handler>,
    flag_id: DataId,
    flag_addr: Option<Value>,
    scratch: Option<StackSlot>,
}

#[derive(Clone, Copy)]
struct Handler {
    block: Block,
    /// `cleanup.len()` when the try was entered: entries above it are released on
    /// the way to the handler, entries below stay live.
    depth: usize,
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
            cleanup: Vec::new(),
            loops: HashMap::new(),
            fn_recur: None,
            argv_slot: None,
            table_base: dummy,
            blob_base: dummy,
            call_sig,
            local_infos: Vec::new(),
            ty_cache: TyCache::new(),
            liveness: Liveness::default(),
            owned_mask: None,
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

    fn imm(&mut self, bits: u64) -> Operand {
        Operand::imm(self.b.ins().iconst(types::I64, bits as i64))
    }

    // ---- ownership helpers --------------------------------------------------------

    fn release(&mut self, op: Operand) {
        match op.own {
            Own::Owned => self.emit_release(op.val),
            Own::Cond(flag) => self.emit_release_if(flag, op.val),
            _ => {}
        }
    }

    /// Turn a borrowed operand into an owned one (+1). Immediates stay immediate.
    fn make_owned(&mut self, op: Operand) -> Operand {
        match op.own {
            Own::Borrowed => {
                self.emit_retain(op.val);
                Operand::owned(op.val)
            }
            Own::Cond(flag) => {
                self.emit_retain_if_not(flag, op.val);
                Operand::owned(op.val)
            }
            _ => op,
        }
    }

    /// Branch to a block in which `v` is known to be a mortal heap object (a pointer
    /// whose refcount is non-negative); immediates and immortals skip it. Returns the
    /// continuation block; the caller emits the mortal-case code, jumps to the
    /// continuation and switches to it.
    fn rc_guard(&mut self, v: Value) -> Block {
        let low = self.b.ins().band_imm_s(v, 3);
        let tag_ok = self.b.ins().icmp_imm_s(IntCC::Equal, low, 0);
        let nonzero = self.b.ins().icmp_imm_s(IntCC::NotEqual, v, 0);
        let is_ptr = self.b.ins().band(tag_ok, nonzero);
        let check = self.b.create_block();
        let mortal = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(is_ptr, check, &[], cont, &[]);
        self.b.switch_to_block(check);
        self.b.seal_block(check);
        let rc = self
            .b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), v, api::consts::OFF_RC);
        let is_mortal = self
            .b
            .ins()
            .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, rc, 0);
        self.b.ins().brif(is_mortal, mortal, &[], cont, &[]);
        self.b.switch_to_block(mortal);
        self.b.seal_block(mortal);
        cont
    }

    /// Release `v` (+1 → 0): a call only for mortal heap objects.
    fn emit_release(&mut self, v: Value) {
        let cont = self.rc_guard(v);
        self.call_rt(RtFn::Release, &[v]);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
        self.b.seal_block(cont);
    }

    /// Retain `v`: a call only for mortal heap objects.
    fn emit_retain(&mut self, v: Value) {
        let cont = self.rc_guard(v);
        self.call_rt(RtFn::Retain, &[v]);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
        self.b.seal_block(cont);
    }

    /// Like `make_owned` but the result is always tagged `Owned`, for storage in a
    /// slot whose later contents may be heap values (loop variables, var roots).
    fn make_owned_force(&mut self, op: Operand) -> Operand {
        let op = self.make_owned(op);
        Operand::owned(op.val)
    }

    fn push_tmp(&mut self, op: Operand) {
        match op.own {
            Own::Owned => self.cleanup.push(Cleanup::Val(op.val)),
            Own::Cond(flag) => self.cleanup.push(Cleanup::ValIfFlag(op.val, flag)),
            _ => {}
        }
    }

    fn pop_tmp(&mut self, op: Operand) {
        if matches!(op.own, Own::Owned | Own::Cond(_)) {
            let top = self.cleanup.pop();
            debug_assert!(
                matches!(top, Some(Cleanup::Val(v) | Cleanup::ValIfFlag(v, _)) if v == op.val),
                "owned-temporary stack mismatch"
            );
        }
    }

    fn bind_local(&mut self, id: LocalId, val: Value, owned: bool) -> Variable {
        let var = self.b.declare_var(types::I64);
        self.b.def_var(var, val);
        let owning = if owned { Owning::Yes } else { Owning::No };
        self.locals.insert(
            id,
            Slot {
                var,
                ty: Ty::Val,
                owning,
            },
        );
        if owned {
            self.cleanup.push(Cleanup::Var(var));
        }
        var
    }

    /// Bind parameter `j`, owned when bit `j` of the function's mask is set.
    fn bind_param(&mut self, id: LocalId, val: Value, j: usize) -> Variable {
        let var = self.b.declare_var(types::I64);
        self.b.def_var(var, val);
        let bit = 1u64 << j;
        self.locals.insert(
            id,
            Slot {
                var,
                ty: Ty::Val,
                owning: Owning::Param(bit),
            },
        );
        self.cleanup.push(Cleanup::Param(var, bit));
        var
    }

    /// `mask & bit != 0` for the function's ownership mask.
    fn owned_bit(&mut self, bit: u64) -> Value {
        let mask = self.owned_mask.expect("ownership mask");
        let m = self.b.use_var(mask);
        self.b.ins().band_imm_s(m, bit as i64)
    }

    /// Bind a primitive local holding raw bits (no refcounting).
    fn bind_typed(&mut self, id: LocalId, raw: Value, ty: Ty) -> Variable {
        let var = self.b.declare_var(raw_type(ty));
        self.b.def_var(var, raw);
        self.locals.insert(
            id,
            Slot {
                var,
                ty,
                owning: Owning::No,
            },
        );
        var
    }

    /// Release the value held by a cleanup entry.
    fn release_cleanup(&mut self, c: Cleanup) {
        match c {
            Cleanup::Val(v) => self.emit_release(v),
            Cleanup::Var(var) => {
                let v = self.b.use_var(var);
                self.emit_release(v);
            }
            Cleanup::Param(var, bit) => {
                let v = self.b.use_var(var);
                let f = self.owned_bit(bit);
                self.emit_release_if(f, v);
            }
            Cleanup::ValIf(v, bit) => {
                let f = self.owned_bit(bit);
                self.emit_release_if(f, v);
            }
            Cleanup::ValIfFlag(v, flag) => self.emit_release_if(flag, v),
        }
    }

    /// Release a run of cleanup entries (newest first). Parameters are all borrowed
    /// in the common case, so their conditional releases sit behind one test of the
    /// whole mask.
    fn release_cleanups(&mut self, items: &[Cleanup]) {
        let mut i = 0;
        while i < items.len() {
            if let Cleanup::Param(..) | Cleanup::ValIf(..) = items[i] {
                let mut j = i;
                while j < items.len() && matches!(items[j], Cleanup::Param(..) | Cleanup::ValIf(..))
                {
                    j += 1;
                }
                let mask = self.owned_mask.expect("ownership mask");
                let m = self.b.use_var(mask);
                let do_b = self.b.create_block();
                let cont = self.b.create_block();
                self.b.ins().brif(m, do_b, &[], cont, &[]);
                self.b.switch_to_block(do_b);
                self.b.seal_block(do_b);
                for c in &items[i..j] {
                    self.release_cleanup(*c);
                }
                self.b.ins().jump(cont, &[]);
                self.b.switch_to_block(cont);
                self.b.seal_block(cont);
                i = j;
            } else {
                self.release_cleanup(items[i]);
                i += 1;
            }
        }
    }

    /// `emit_release(v)` when `flag` is non-zero.
    fn emit_release_if(&mut self, flag: Value, v: Value) {
        let do_b = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(flag, do_b, &[], cont, &[]);
        self.b.switch_to_block(do_b);
        self.b.seal_block(do_b);
        self.emit_release(v);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
        self.b.seal_block(cont);
    }

    /// `emit_retain(v)` when `flag` is zero (a borrowed parameter being taken over).
    fn emit_retain_if_not(&mut self, flag: Value, v: Value) {
        let do_b = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(flag, cont, &[], do_b, &[]);
        self.b.switch_to_block(do_b);
        self.b.seal_block(do_b);
        self.emit_retain(v);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
        self.b.seal_block(cont);
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

    /// A local as a tagged value, boxing a primitive one. At the local's last use
    /// (`at`: the using node) an owned value is transferred out and the variable
    /// cleared, so the function stops holding it.
    fn read_local(&mut self, at: usize, id: LocalId) -> CResult<Operand> {
        let slot = self.slot(id)?;
        let raw = self.b.use_var(slot.var);
        if slot.ty != Ty::Val {
            return Ok(self.box_raw(raw, slot.ty));
        }
        if self.liveness.retain.contains(&(at, id)) {
            // Pending as an argument while a later argument transfers this local.
            return Ok(self.make_owned(Operand::borrowed(raw)));
        }
        if !self.liveness.last.contains(&(at, id)) {
            return Ok(Operand::borrowed(raw));
        }
        match slot.owning {
            Owning::No => Ok(Operand::borrowed(raw)),
            Owning::Yes => {
                let nil = self.b.ins().iconst(types::I64, 0);
                self.b.def_var(slot.var, nil);
                Ok(Operand::owned(raw))
            }
            Owning::Param(bit) => {
                // Owned exactly when the caller handed it over; the flag travels with
                // the value, so a borrowed argument is passed on without refcounting.
                let f = self.owned_bit(bit);
                let nil = self.b.ins().iconst(types::I64, 0);
                self.b.def_var(slot.var, nil);
                Ok(Operand {
                    val: raw,
                    own: Own::Cond(f),
                })
            }
        }
    }

    // ---- primitives -----------------------------------------------------------------

    /// Box raw bits: a fixnum when it fits (no allocation), else a heap number.
    fn box_raw(&mut self, raw: Value, ty: Ty) -> Operand {
        match ty {
            Ty::Double => Operand::owned(self.call_rt(RtFn::BoxDouble, &[raw]).unwrap()),
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
                Operand::owned(self.b.block_params(merge)[0])
            }
            Ty::Val => Operand::borrowed(raw),
        }
    }

    /// Unbox a tagged value into raw bits of `want`, throwing `ClassCastException`
    /// for non-numbers. Fixnums are converted inline; everything else goes through
    /// the runtime. Consumes `op`.
    fn unbox(&mut self, op: Operand, want: Ty, strict: bool) -> CResult<Value> {
        let v = op.val;
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
        self.push_tmp(op);
        let r = if want == Ty::Double {
            self.call_rt(RtFn::UnboxDouble, &[v, out]).unwrap()
        } else {
            let s = self.b.ins().iconst(types::I64, i64::from(strict));
            self.call_rt(RtFn::UnboxLong, &[v, out, s]).unwrap()
        };
        self.check_exc(r);
        self.pop_tmp(op);
        let loaded = self.b.ins().stack_load(types::I64, raw_type(want), slot, 0);
        self.b.ins().jump(merge, &[BlockArg::Value(loaded)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        let r = self.b.block_params(merge)[0];
        self.release(op);
        Ok(r)
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
        let r = self.call_rt(RtFn::ThrowPrim, &[c]).unwrap();
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
                let vars = self.bind_all(bindings, false)?;
                let r = self.compile_typed(body, want)?;
                self.release_locals(&vars);
                return Ok(r);
            }
            Node::Do(nodes) if !nodes.is_empty() => {
                for n in &nodes[..nodes.len() - 1] {
                    let op = self.compile(n)?;
                    self.release(op);
                }
                return self.compile_typed(&nodes[nodes.len() - 1], want);
            }
            Node::Recur { target, args } => {
                self.compile_recur(*target, args)?;
                return Ok(self.raw_zero(want));
            }
            Node::Throw(e) => {
                self.compile_throw(e)?;
                return Ok(self.raw_zero(want));
            }
            Node::BuiltinCall { desc, args, .. } => {
                let ty = self.ty_of(node);
                if ty != Ty::Val {
                    let raw = self.compile_prim_builtin(desc, args, ty)?;
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
    /// builtin. Unboxing runs newest first, keeping the earlier operands protected on
    /// the temporary stack while a later one may throw.
    fn prim_args(&mut self, args: &[Node], want: Ty) -> CResult<Vec<Value>> {
        enum Pre {
            Raw(Value),
            Boxed(Operand),
        }
        let mut pre = Vec::with_capacity(args.len());
        for a in args {
            if self.ty_of(a) != Ty::Val {
                pre.push(Pre::Raw(self.compile_typed(a, want)?));
            } else {
                let op = self.compile(a)?;
                self.push_tmp(op);
                pre.push(Pre::Boxed(op));
            }
        }
        let mut out: Vec<Option<Value>> = vec![None; pre.len()];
        for (i, p) in pre.into_iter().enumerate().rev() {
            out[i] = Some(match p {
                Pre::Raw(v) => v,
                Pre::Boxed(op) => {
                    self.pop_tmp(op);
                    self.unbox(op, want, want == Ty::Long)?
                }
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
                    let o = self.compile(a)?;
                    self.release(o);
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
        if any_double {
            let xs = self.prim_args(args, Ty::Double)?;
            Ok(Some(self.b.ins().fcmp(fcc, xs[0], xs[1])))
        } else if all_long {
            let xs = self.prim_args(args, Ty::Long)?;
            Ok(Some(self.b.ins().icmp(icc, xs[0], xs[1])))
        } else {
            Ok(None)
        }
    }

    /// Release and forget owned locals in reverse order of binding.
    fn release_locals(&mut self, vars: &[(LocalId, Variable, bool)]) {
        let mut items = Vec::new();
        for (id, var, owned) in vars.iter().rev() {
            if *owned {
                let top = self.cleanup.pop();
                debug_assert!(
                    matches!(top, Some(Cleanup::Var(v) | Cleanup::Param(v, _)) if v == *var),
                    "cleanup stack mismatch"
                );
                if let Some(c) = top {
                    items.push(c);
                }
            }
            self.locals.remove(id);
        }
        self.release_cleanups(&items);
    }

    /// After a call: branch to a cleanup block that releases every live owned operand
    /// and propagates `EXC`.
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

    /// An exception is pending (`r == EXC`): release what the current path owns and
    /// continue at the innermost `try` handler, or return `EXC` from the function.
    /// Ends the current block.
    fn emit_exc_path(&mut self, r: Value) {
        let (floor, target) = match self.handlers.last().copied() {
            Some(h) => (h.depth, Some(h.block)),
            None => (0, None),
        };
        let items: Vec<Cleanup> = self.cleanup[floor..].iter().rev().copied().collect();
        self.release_cleanups(&items);
        match target {
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

    /// Loop-header check of the interrupt flag (one byte load and branch).
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

    fn compile(&mut self, node: &Node) -> CResult<Operand> {
        match node {
            Node::Const(v) => self.compile_const(v),
            Node::Local(id) => self.read_local(node as *const Node as usize, *id),
            Node::VarRef(v) => {
                let var = v.get();
                let p = self.const_slot(ConstEntry::Var(var.ns.to_string(), var.name.to_string()));
                // Fast path: the var's lock-free word holds the root when it is an
                // immediate or immortal (never freed) value; otherwise take the root
                // under the lock with a +1.
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
                let v = self.b.block_params(merge)[0];
                // Releasing an immortal value is a no-op, so both paths may be owned.
                Ok(Operand::owned(v))
            }
            Node::TheVar(v) => {
                let var = v.get();
                let p = self.const_slot(ConstEntry::Var(var.ns.to_string(), var.name.to_string()));
                Ok(Operand::borrowed(p))
            }
            Node::CurrentNs => {
                let ns = self.call_rt(RtFn::CurrentNs, &[]).unwrap();
                Ok(Operand::borrowed(ns))
            }
            Node::Def { var, init, meta } => {
                let vo = var.get();
                let p = self.const_slot(ConstEntry::Var(vo.ns.to_string(), vo.name.to_string()));
                let result = match init {
                    None => p,
                    Some(init) => {
                        let op = self.compile(init)?;
                        let op = self.make_owned_force(op);
                        self.call_rt(RtFn::VarBindRoot, &[p, op.val]).unwrap()
                    }
                };
                // Metadata after the init, so a throwing init leaves the var untouched.
                // (The analyzer already applied `^:dynamic` at compile time; the root
                // is republished when the flag changes.)
                if let Some(m) = meta {
                    let mv = self.compile_const(m)?;
                    let mv = self.make_owned_force(mv);
                    self.call_rt(RtFn::VarSetMeta, &[p, mv.val]);
                }
                Ok(Operand::borrowed(result))
            }
            Node::VarSet { var, value } => {
                let vo = var.get();
                let p = self.const_slot(ConstEntry::Var(vo.ns.to_string(), vo.name.to_string()));
                let op = self.compile(value)?;
                let op = self.make_owned_force(op);
                let r = self.call_rt(RtFn::VarSet, &[p, op.val]).unwrap();
                self.check_exc(r);
                Ok(Operand::owned(r))
            }
            Node::If { test, then, else_ } => self.compile_if(test, then, else_),
            Node::Do(nodes) => {
                let mut last = self.imm(api::consts::NIL);
                for (i, n) in nodes.iter().enumerate() {
                    let op = self.compile(n)?;
                    if i + 1 < nodes.len() {
                        self.release(op);
                    } else {
                        last = op;
                    }
                }
                Ok(last)
            }
            Node::Let { bindings, body } => self.compile_let(bindings, body),
            Node::Loop { id, bindings, body } => self.compile_loop(*id, bindings, body),
            Node::Recur { target, args } => self.compile_recur(*target, args),
            Node::Fn(f) => self.compile_fn_value(node as *const Node as usize, f),
            Node::Call { f, args, .. } => self.compile_call(f, args),
            Node::BuiltinCall { desc, args, .. } => {
                let ty = self.ty_of(node);
                if ty != Ty::Val {
                    let raw = self.compile_prim_builtin(desc, args, ty)?;
                    return Ok(self.box_raw(raw, ty));
                }
                self.compile_builtin_call(desc, args, false)
            }
            Node::Coll { kind, items } => self.compile_coll(*kind, items),
            Node::Throw(e) => self.compile_throw(e),
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

    fn compile_throw(&mut self, e: &Node) -> CResult<Operand> {
        let op = self.compile(e)?;
        let op = self.make_owned_force(op);
        let r = self.call_rt(RtFn::Throw, &[op.val]).unwrap();
        self.emit_exc_path(r);
        self.dead_block();
        Ok(self.imm(api::consts::NIL))
    }

    /// Run `region` with a handler: on an exception the cleanup entries pushed inside
    /// the region are released and control reaches `handler` with the taken exception
    /// (owned). `handler` must end its block (jump to `merge` or propagate).
    fn protect(
        &mut self,
        region: &mut dyn FnMut(&mut Self) -> CResult<Operand>,
        handler: &mut dyn FnMut(&mut Self, Value, Block) -> CResult<()>,
    ) -> CResult<Operand> {
        let handler_block = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        self.handlers.push(Handler {
            block: handler_block,
            depth: self.cleanup.len(),
        });
        let op = region(self);
        self.handlers.pop();
        let op = self.make_owned(op?);
        self.b.ins().jump(merge, &[BlockArg::Value(op.val)]);

        self.b.switch_to_block(handler_block);
        self.b.seal_block(handler_block);
        let e = self.call_rt(RtFn::TakeException, &[]).unwrap();
        handler(self, e, merge)?;

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        Ok(Operand::owned(self.b.block_params(merge)[0]))
    }

    fn compile_try(
        &mut self,
        body: &Node,
        catches: &[CatchClause],
        finally: Option<&Node>,
    ) -> CResult<Operand> {
        if catches.is_empty() && finally.is_none() {
            // Owned, so a `(try x)` argument never passes a local through borrowed.
            let op = self.compile(body)?;
            return Ok(self.make_owned(op));
        }
        let mut inner = |fc: &mut Self| -> CResult<Operand> {
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
                let op = self.protect(&mut inner, &mut |fc, e, _merge| {
                    // Exceptional path: run finally with the exception parked (and
                    // owned), then rethrow it. If finally throws, its exception wins.
                    fc.cleanup.push(Cleanup::Val(e));
                    let f = fc.compile(fin)?;
                    fc.release(f);
                    fc.cleanup.pop();
                    let r = fc.call_rt(RtFn::Throw, &[e]).unwrap();
                    fc.emit_exc_path(r);
                    Ok(())
                })?;
                // Normal path: the result is live while finally runs.
                self.push_tmp(op);
                let f = self.compile(fin)?;
                self.release(f);
                self.pop_tmp(op);
                Ok(op)
            }
        }
    }

    /// Handler body for `catch` clauses: match classes in order, bind, run, merge;
    /// rethrow when nothing matches.
    fn catch_dispatch(&mut self, e: Value, merge: Block, catches: &[CatchClause]) -> CResult<()> {
        self.cleanup.push(Cleanup::Val(e));
        for clause in catches {
            let (cp, cl) = self.blob_ptr(&clause.class);
            let m = self.call_rt(RtFn::ExceptionInstance, &[e, cp, cl]).unwrap();
            let clause_b = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(m, clause_b, &[], next, &[]);

            self.b.switch_to_block(clause_b);
            self.b.seal_block(clause_b);
            // Ownership of `e` moves from the dispatch temporary to the catch local.
            self.cleanup.pop();
            let var = self.bind_local(clause.local, e, true);
            let r = self.compile(&clause.body)?;
            let r = self.make_owned(r);
            self.release_locals(&[(clause.local, var, true)]);
            self.b.ins().jump(merge, &[BlockArg::Value(r.val)]);

            self.b.switch_to_block(next);
            self.b.seal_block(next);
            self.cleanup.push(Cleanup::Val(e));
        }
        self.cleanup.pop();
        let r = self.call_rt(RtFn::Throw, &[e]).unwrap();
        self.emit_exc_path(r);
        Ok(())
    }

    fn native_cl_type(t: NativeTy) -> cranelift_codegen::ir::Type {
        match t {
            NativeTy::Void => types::INVALID,
            NativeTy::Long | NativeTy::Str | NativeTy::Ptr => types::I64,
            NativeTy::Int => types::I32,
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
    ) -> CResult<Operand> {
        let mut ops = Vec::with_capacity(args.len());
        for a in args {
            let op = self.compile(a)?;
            self.push_tmp(op);
            ops.push(op);
        }
        let fptr = self.const_slot(ConstEntry::Native(symbol.to_string()));
        let scratch = self.scratch_slot();
        let scratch_addr = self.b.ins().stack_addr(types::I64, scratch, 0);
        let mut sig = self.module.make_signature();
        let mut cargs = Vec::with_capacity(ops.len());
        for (op, ty) in ops.iter().zip(params) {
            let v = match ty {
                NativeTy::Long | NativeTy::Ptr | NativeTy::Int => {
                    let r = self.call_rt(RtFn::ToLong, &[op.val, scratch_addr]).unwrap();
                    self.check_exc(r);
                    let n = self.b.ins().stack_load(types::I64, types::I64, scratch, 0);
                    if *ty == NativeTy::Int {
                        self.b.ins().ireduce(types::I32, n)
                    } else {
                        n
                    }
                }
                NativeTy::Double | NativeTy::Float => {
                    let r = self
                        .call_rt(RtFn::ToDouble, &[op.val, scratch_addr])
                        .unwrap();
                    self.check_exc(r);
                    let d = self.b.ins().stack_load(types::I64, types::F64, scratch, 0);
                    if *ty == NativeTy::Float {
                        self.b.ins().fdemote(types::F32, d)
                    } else {
                        d
                    }
                }
                NativeTy::Bool => self.truthy(op.val),
                NativeTy::Str => {
                    let r = self.call_rt(RtFn::ToCstr, &[op.val, scratch_addr]).unwrap();
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
                Operand::owned(self.call_rt(RtFn::BoxLong, &[r]).unwrap())
            }
            NativeTy::Int => {
                let r = self.b.inst_results(inst)[0];
                let r = self.b.ins().sextend(types::I64, r);
                Operand::owned(self.call_rt(RtFn::BoxLong, &[r]).unwrap())
            }
            NativeTy::Double => {
                let r = self.b.inst_results(inst)[0];
                Operand::owned(self.call_rt(RtFn::BoxDouble, &[r]).unwrap())
            }
            NativeTy::Float => {
                let r = self.b.inst_results(inst)[0];
                let r = self.b.ins().fpromote(types::F64, r);
                Operand::owned(self.call_rt(RtFn::BoxDouble, &[r]).unwrap())
            }
            NativeTy::Bool => {
                let r = self.b.inst_results(inst)[0];
                let c = self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0);
                let t = self.b.ins().iconst(types::I64, api::consts::TRUE as i64);
                let f = self.b.ins().iconst(types::I64, api::consts::FALSE as i64);
                Operand::imm(self.b.ins().select(c, t, f))
            }
            NativeTy::Str => {
                let r = self.b.inst_results(inst)[0];
                Operand::owned(self.call_rt(RtFn::StringFromCstr, &[r]).unwrap())
            }
        };
        self.release_tmps(&ops);
        Ok(result)
    }

    fn compile_const(&mut self, v: &rscl_runtime::Value) -> CResult<Operand> {
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
            | Kind::LazySeq => ConstEntry::Edn(printer::pr_str(v)),
            k => {
                return Err(internal(format!(
                    "cannot embed a {} as a constant",
                    k.name()
                )));
            }
        };
        Ok(Operand::borrowed(self.const_slot(entry)))
    }

    /// An i8 condition for `test`: inline comparisons skip the boolean value.
    fn compile_cond(&mut self, test: &Node) -> CResult<Value> {
        if let Node::BuiltinCall { desc, args, .. } = test
            && (inline_op(desc, args.len()).is_some() || self.ty_of(test) == Ty::Val)
        {
            return Ok(self.compile_builtin_call(desc, args, true)?.val);
        }
        let t = self.compile(test)?;
        let c = self.truthy(t.val);
        self.release(t);
        Ok(c)
    }

    fn compile_if(&mut self, test: &Node, then: &Node, else_: &Node) -> CResult<Operand> {
        let c = self.compile_cond(test)?;
        let then_b = self.b.create_block();
        let else_b = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        self.b.ins().brif(c, then_b, &[], else_b, &[]);

        self.b.switch_to_block(then_b);
        self.b.seal_block(then_b);
        let a = self.compile(then)?;
        let a = self.make_owned(a);
        self.b.ins().jump(merge, &[BlockArg::Value(a.val)]);

        self.b.switch_to_block(else_b);
        self.b.seal_block(else_b);
        let e = self.compile(else_)?;
        let e = self.make_owned(e);
        self.b.ins().jump(merge, &[BlockArg::Value(e.val)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        let r = self.b.block_params(merge)[0];
        let own = if a.own == Own::Imm && e.own == Own::Imm {
            Own::Imm
        } else {
            Own::Owned
        };
        Ok(Operand { val: r, own })
    }

    fn bind_all(
        &mut self,
        bindings: &[(LocalId, Node)],
        force_owned: bool,
    ) -> CResult<Vec<(LocalId, Variable, bool)>> {
        let mut out = Vec::with_capacity(bindings.len());
        for (id, init) in bindings {
            let ty = self.local_ty(*id);
            if ty != Ty::Val {
                let raw = self.compile_typed(init, ty)?;
                let var = self.bind_typed(*id, raw, ty);
                out.push((*id, var, false));
                continue;
            }
            let op = self.compile(init)?;
            let op = if force_owned {
                self.make_owned_force(op)
            } else {
                self.make_owned(op)
            };
            let owned = op.own == Own::Owned;
            let var = self.bind_local(*id, op.val, owned);
            out.push((*id, var, owned));
        }
        Ok(out)
    }

    fn compile_let(&mut self, bindings: &[(LocalId, Node)], body: &Node) -> CResult<Operand> {
        let vars = self.bind_all(bindings, false)?;
        let res = self.compile(body)?;
        let res = self.make_owned(res);
        self.release_locals(&vars);
        Ok(res)
    }

    fn compile_loop(
        &mut self,
        id: LoopId,
        bindings: &[(LocalId, Node)],
        body: &Node,
    ) -> CResult<Operand> {
        let vars = self.bind_all(bindings, true)?;
        let header = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.b.switch_to_block(header);
        self.emit_interrupt_check();
        self.loops.insert(
            id,
            LoopCtx {
                header,
                vars: vars
                    .iter()
                    .map(|(id, v, _)| LoopVar {
                        var: *v,
                        ty: self.local_ty(*id),
                        param_bit: None,
                    })
                    .collect(),
                depth: self.cleanup.len(),
            },
        );
        let res = self.compile(body);
        self.loops.remove(&id);
        self.b.seal_block(header);
        let res = self.make_owned(res?);
        self.release_locals(&vars);
        Ok(res)
    }

    fn compile_recur(&mut self, target: RecurTarget, args: &[Node]) -> CResult<Operand> {
        let ctx = match target {
            RecurTarget::Loop(id) => self.loops.get(&id).cloned(),
            RecurTarget::Fn => self.fn_recur.clone(),
        }
        .ok_or_else(|| internal("recur target missing"))?;
        // Primitive loop variables take raw bits; the rest are evaluated as owned
        // values, kept on the temporary stack while later arguments run.
        let mut ops: Vec<Option<Operand>> = Vec::with_capacity(args.len());
        let mut raws: Vec<Value> = Vec::with_capacity(args.len());
        for (a, lv) in args.iter().zip(&ctx.vars) {
            if lv.ty == Ty::Val {
                let op = self.compile(a)?;
                self.push_tmp(op);
                ops.push(Some(op));
                raws.push(op.val);
            } else {
                let raw = self.compile_typed(a, lv.ty)?;
                ops.push(None);
                raws.push(raw);
            }
        }
        for op in ops.iter().rev().flatten() {
            self.pop_tmp(*op);
        }
        let new_vals: Vec<Value> = ops
            .iter()
            .zip(&raws)
            .map(|(op, raw)| match op {
                Some(op) => self.make_owned_force(*op).val,
                None => *raw,
            })
            .collect();
        // Locals bound inside the loop body (`let` etc.) are live on this path and
        // would otherwise leak once per iteration: release them, newest first. They
        // stay on the compile-time stack for the fall-through path.
        let inner: Vec<Cleanup> = self.cleanup[ctx.depth..].iter().rev().copied().collect();
        self.release_cleanups(&inner);
        // Old values before rebinding; parameters are released only if owned.
        let olds: Vec<Cleanup> = ctx
            .vars
            .iter()
            .filter(|lv| lv.ty == Ty::Val)
            .map(|lv| match lv.param_bit {
                Some(bit) => Cleanup::Param(lv.var, bit),
                None => Cleanup::Var(lv.var),
            })
            .collect();
        // The old values must be read before the variables are redefined.
        let old_vals: Vec<Cleanup> = olds
            .iter()
            .map(|c| match c {
                Cleanup::Var(var) => Cleanup::Val(self.b.use_var(*var)),
                Cleanup::Param(var, bit) => {
                    let v = self.b.use_var(*var);
                    Cleanup::ValIf(v, *bit)
                }
                c => *c,
            })
            .collect();
        let mut rebound_bits = 0u64;
        for (lv, val) in ctx.vars.iter().zip(new_vals) {
            self.b.def_var(lv.var, val);
            if let Some(bit) = lv.param_bit {
                rebound_bits |= bit;
            }
        }
        self.release_cleanups(&old_vals);
        if rebound_bits != 0 {
            // Rebound by recur: owned from now on.
            let mask = self.owned_mask.expect("ownership mask");
            let m = self.b.use_var(mask);
            let m = self.b.ins().bor_imm_s(m, rebound_bits as i64);
            self.b.def_var(mask, m);
        }
        self.b.ins().jump(ctx.header, &[]);
        self.dead_block();
        Ok(self.imm(api::consts::NIL))
    }

    fn compile_fn_value(&mut self, at: usize, f: &Rc<FnNode>) -> CResult<Operand> {
        let func = self.unit.fn_id(self.module, f)?;
        if f.captures.is_empty() {
            let fv = self.const_slot(ConstEntry::FnObj {
                func,
                name: f.name.clone(),
                mask: f.arity_mask(),
                vmin: f.variadic_min(),
            });
            return Ok(Operand::borrowed(fv));
        }
        let proto = self.const_slot(ConstEntry::Proto {
            func,
            name: f.name.clone(),
            mask: f.arity_mask(),
            vmin: f.variadic_min(),
        });
        let n = self.b.ins().iconst(types::I64, f.captures.len() as i64);
        let fv = self.call_rt(RtFn::FnNew, &[proto, n]).unwrap();
        for (i, id) in f.captures.iter().enumerate() {
            let op = self.read_local(at, *id)?;
            let op = self.make_owned_force(op);
            let i = self.b.ins().iconst(types::I64, i as i64);
            self.call_rt(RtFn::FnSetCapture, &[fv, i, op.val]);
        }
        Ok(Operand::owned(fv))
    }

    /// Evaluate `items` and store them in the argv slot; returns (addr, count).
    fn store_args(&mut self, items: &[Node]) -> CResult<(Value, Value, Vec<Operand>)> {
        let ops = self.eval_args(items)?;
        let (addr, n) = self.store_ops(&ops);
        Ok((addr, n, ops))
    }

    /// Evaluate arguments left to right, keeping each on the owned-temporary stack.
    fn eval_args(&mut self, items: &[Node]) -> CResult<Vec<Operand>> {
        let mut ops = Vec::with_capacity(items.len());
        for a in items {
            let op = self.compile(a)?;
            self.push_tmp(op);
            ops.push(op);
        }
        Ok(ops)
    }

    /// Write evaluated arguments into the argv slot; returns (addr, count).
    fn store_ops(&mut self, ops: &[Operand]) -> (Value, Value) {
        let addr = self.argv_addr();
        for (i, op) in ops.iter().enumerate() {
            self.b
                .ins()
                .store(MemFlagsData::trusted(), op.val, addr, 8 * i as i32);
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
    ) -> CResult<Operand> {
        if let Some(c) = self.prim_cond(desc, args)? {
            return Ok(if as_cond {
                Operand::imm(c)
            } else {
                let t = self.b.ins().iconst(types::I64, api::consts::TRUE as i64);
                let f = self.b.ins().iconst(types::I64, api::consts::FALSE as i64);
                Operand::imm(self.b.ins().select(c, t, f))
            });
        }
        let ops = self.eval_args(args)?;
        let op = inline_op(desc, args.len());
        let result = match op {
            Some(op) => {
                let r = self.compile_inline(op, desc, &ops, as_cond)?;
                self.release_tmps(&ops);
                r
            }
            None => {
                let r = self.direct_call(desc, &ops, true)?;
                self.release_untransferred(&ops);
                if as_cond {
                    // Only the truth of the result is needed; the value itself goes.
                    let c = self.truthy(r);
                    self.emit_release(r);
                    Operand::imm(c)
                } else {
                    Operand::owned(r)
                }
            }
        };
        Ok(result)
    }

    /// `builtin(null, argc, argv, owned)` with the exception check; returns the raw
    /// result. With `transfer`, owned operands are handed over (popped from the
    /// temporary stack); otherwise the builtin borrows and the caller releases.
    fn direct_call(
        &mut self,
        desc: &'static BuiltinDesc,
        ops: &[Operand],
        transfer: bool,
    ) -> CResult<Value> {
        let (argv, argc) = self.store_ops(ops);
        let owned = if transfer {
            self.transfer_tmps(ops)
        } else {
            self.b.ins().iconst(types::I64, 0)
        };
        let fref = self.builtin_ref(desc)?;
        let this = self.b.ins().iconst(types::I64, 0);
        let inst = self.b.ins().call(fref, &[this, argc, argv, owned]);
        let r = self.b.inst_results(inst)[0];
        self.check_exc(r);
        Ok(r)
    }

    /// Fixnum fast path for `op`, falling back to the builtin for anything else
    /// (boxed longs, doubles, non-numbers, overflow). Operands are already on the
    /// temporary stack; the caller releases them.
    fn compile_inline(
        &mut self,
        op: PrimOp,
        desc: &'static BuiltinDesc,
        ops: &[Operand],
        as_cond: bool,
    ) -> CResult<Operand> {
        let a = ops[0].val;
        let b = ops.get(1).map(|o| o.val);
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
        let is_pred = op.is_predicate();
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
        let r = self.direct_call(desc, ops, false)?;
        let r = if as_cond {
            let c = self.truthy(r);
            self.emit_release(r);
            c
        } else {
            r
        };
        self.b.ins().jump(merge, &[BlockArg::Value(r)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        let r = self.b.block_params(merge)[0];
        // Predicates yield booleans on both paths; arithmetic may yield a boxed long
        // from the builtin, so it is owned (releasing a fixnum is a no-op).
        Ok(if as_cond || is_pred {
            Operand::imm(r)
        } else {
            Operand::owned(r)
        })
    }

    fn release_tmps(&mut self, ops: &[Operand]) {
        for op in ops.iter().rev() {
            self.pop_tmp(*op);
            self.release(*op);
        }
    }

    /// Hand owned argument operands over to the callee: pop them from the temporary
    /// stack (the callee releases or keeps them) and return the ownership mask. With
    /// more than 64 arguments nothing is transferred.
    fn transfer_tmps(&mut self, ops: &[Operand]) -> Value {
        if ops.len() > 64 {
            return self.b.ins().iconst(types::I64, 0);
        }
        let mut mask: u64 = 0;
        let mut dynamic: Vec<(usize, Value)> = Vec::new();
        for (i, op) in ops.iter().enumerate().rev() {
            self.pop_tmp(*op);
            match op.own {
                Own::Owned => mask |= 1 << i,
                Own::Cond(flag) => dynamic.push((i, flag)),
                _ => {}
            }
        }
        let mut m = self.b.ins().iconst(types::I64, mask as i64);
        for (i, flag) in dynamic {
            let bit = self.b.ins().iconst(types::I64, (1u64 << i) as i64);
            let zero = self.b.ins().iconst(types::I64, 0);
            let b = self.b.ins().select(flag, bit, zero);
            m = self.b.ins().bor(m, b);
        }
        m
    }

    /// Release the arguments the callee did not take (more than 64 of them).
    fn release_untransferred(&mut self, ops: &[Operand]) {
        if ops.len() > 64 {
            self.release_tmps(ops);
        }
    }

    fn compile_call(&mut self, f: &Node, args: &[Node]) -> CResult<Operand> {
        let fop = self.compile(f)?;
        self.push_tmp(fop);
        let (argv, argc, ops) = self.store_args(args)?;
        let owned = self.transfer_tmps(&ops);
        let fv = fop.val;

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
        let inst = self
            .b
            .ins()
            .call_indirect(self.call_sig, entry, &[fv, argc, argv, owned]);
        let r1 = self.b.inst_results(inst)[0];
        self.b.ins().jump(merge, &[BlockArg::Value(r1)]);

        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let r2 = self
            .call_rt(RtFn::Invoke, &[fv, argc, argv, owned])
            .unwrap();
        self.b.ins().jump(merge, &[BlockArg::Value(r2)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        let r = self.b.block_params(merge)[0];
        self.check_exc(r);
        self.release_untransferred(&ops);
        self.pop_tmp(fop);
        self.release(fop);
        Ok(Operand::owned(r))
    }

    fn compile_coll(&mut self, kind: CollKind, items: &[Node]) -> CResult<Operand> {
        let (addr, n, ops) = self.store_args(items)?;
        let owned = self.transfer_tmps(&ops);
        let f = match kind {
            CollKind::Vector => RtFn::VectorFromSlice,
            CollKind::Map => RtFn::MapFromSlice,
            CollKind::Set => RtFn::SetFromSlice,
        };
        let r = self.call_rt(f, &[addr, n, owned]).unwrap();
        self.release_untransferred(&ops);
        Ok(Operand::owned(r))
    }

    /// Body of one fn arity: bind params (and rest), compile the body as the recur
    /// target, release owned locals, return.
    fn compile_arity(&mut self, a: &Arity, argv: Value, argc: Value, owned: Value) -> CResult<()> {
        let mut vars: Vec<(LocalId, Variable, bool)> = Vec::new();
        let mut bits: Vec<Option<u64>> = Vec::new();
        let mask = self.b.declare_var(types::I64);
        self.b.def_var(mask, owned);
        self.owned_mask = Some(mask);
        // Load every argument and protect the owned ones (and the rest list) before
        // anything can throw: unboxing a `^long`/`^double` parameter may fail, and the
        // exception path must then release the arguments that were handed over.
        let vals: Vec<Value> = (0..a.params.len())
            .map(|j| {
                self.b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), argv, 8 * j as i32)
            })
            .collect();
        // The analyzer caps fixed parameters at 63, so every bit fits the mask.
        debug_assert!(
            vals.len() < 64,
            "too many parameters for the ownership mask"
        );
        for (j, v) in vals.iter().enumerate() {
            self.cleanup.push(Cleanup::ValIf(*v, 1 << j));
        }
        let rest = a.rest.map(|_| {
            let nfixed = a.params.len() as i64;
            let extra = self.b.ins().iadd_imm_s(argc, -nfixed);
            let addr = self.b.ins().iadd_imm_s(argv, 8 * nfixed);
            let rest_owned = self.b.ins().ushr_imm_s(owned, nfixed);
            let r = self
                .call_rt(RtFn::ListFromSlice, &[addr, extra, rest_owned])
                .unwrap();
            self.cleanup.push(Cleanup::Val(r));
            r
        });
        let mut raws: Vec<Option<Value>> = Vec::with_capacity(vals.len());
        for (id, v) in a.params.iter().zip(&vals) {
            let ty = self.local_ty(*id);
            raws.push(if ty != Ty::Val {
                // ClassCastException for anything else, like a primitive-hinted
                // Clojure fn.
                Some(self.unbox(Operand::borrowed(*v), ty, true)?)
            } else {
                None
            });
        }
        // Now bind: the protection entries are replaced by the parameters' own.
        if rest.is_some() {
            self.cleanup.pop();
        }
        for _ in &vals {
            self.cleanup.pop();
        }
        for (j, (id, v)) in a.params.iter().zip(&vals).enumerate() {
            match raws[j] {
                Some(raw) => {
                    // The boxed argument goes right away if it was handed over.
                    let flag = self.owned_bit(1 << j);
                    self.emit_release_if(flag, *v);
                    let ty = self.local_ty(*id);
                    let var = self.bind_typed(*id, raw, ty);
                    vars.push((*id, var, false));
                    bits.push(None);
                }
                None => {
                    let var = self.bind_param(*id, *v, j);
                    vars.push((*id, var, true));
                    bits.push(Some(1 << j));
                }
            }
        }
        if let (Some(id), Some(r)) = (a.rest, rest) {
            let var = self.bind_local(id, r, true);
            vars.push((id, var, true));
            bits.push(None);
        }
        let header = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.b.switch_to_block(header);
        self.fn_recur = Some(LoopCtx {
            header,
            vars: vars
                .iter()
                .zip(&bits)
                .map(|((id, v, _), bit)| LoopVar {
                    var: *v,
                    ty: self.local_ty(*id),
                    param_bit: *bit,
                })
                .collect(),
            depth: self.cleanup.len(),
        });
        let res = self.compile(&a.body);
        self.fn_recur = None;
        self.b.seal_block(header);
        let res = self.make_owned(res?);
        self.release_locals(&vars);
        self.b.ins().return_(&[res.val]);
        Ok(())
    }
}
