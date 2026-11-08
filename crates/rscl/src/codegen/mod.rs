//! `Node` → Cranelift IR, generic over `cranelift_module::Module` so the same code
//! serves the JIT and the AOT object writer.
//!
//! Invariants (see the plan): no raw address is embedded in IR; every call is followed
//! by an `EXC` check whose cleanup path releases every live owned operand; operands
//! carry their ownership (`Own`) and consumers decide whether to retain or release.
use std::collections::HashMap;
use std::rc::Rc;

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, FuncRef, InstBuilder, MemFlagsData, SigRef, Signature, StackSlot,
    StackSlotData, StackSlotKind, UserFuncName, Value, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use rscl_runtime::api::{self, RtFn, Ty};
use rscl_runtime::obj::Kind;
use rscl_runtime::{obj, printer};

use crate::analyzer::{
    Analyzer, Arity, CatchClause, CollKind, FnNode, LocalId, LocalInfo, LoopId, NativeTy, Node,
    RecurTarget,
};
use crate::errors::{CResult, CompileError};

pub struct UnitIds {
    pub init: FuncId,
    pub body: FuncId,
}

pub struct Compiler<M: Module> {
    pub module: M,
    fb_ctx: FunctionBuilderContext,
    rt_ids: HashMap<RtFn, FuncId>,
    debug_ir: bool,
    next_unit: u32,
    /// The runtime's `rscl_interrupt_flag` (an imported data symbol).
    flag_id: Option<DataId>,
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
pub fn fn_signature<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

fn thunk_signature<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

fn cl_type(t: Ty) -> cranelift_codegen::ir::Type {
    match t {
        Ty::I64 => types::I64,
        Ty::F64 => types::F64,
        Ty::I32 => types::I32,
    }
}

impl<M: Module> Compiler<M> {
    pub fn new(module: M) -> Self {
        Compiler {
            module,
            fb_ctx: FunctionBuilderContext::new(),
            rt_ids: HashMap::new(),
            debug_ir: std::env::var_os("RSCL_DEBUG_IR").is_some(),
            next_unit: 0,
            flag_id: None,
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
        })
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
    pub fn compile_entry(&mut self, units: &[UnitIds], main_ns: &str) -> CResult<FuncId> {
        let mut u = self.new_unit()?;
        let mut sig = self.module.make_signature();
        sig.returns.push(AbiParam::new(types::I32));
        let id = self
            .module
            .declare_function("rscl_entry", Linkage::Local, &sig)
            .map_err(internal)?;
        let own_init = u.init_id;
        let main_ns = main_ns.to_string();
        self.with_function(id, sig, "rscl_entry", &mut u, |fc| {
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
        self.with_function(id, sig, "main", &mut u, |fc| {
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
            let mut fc = FnCtx::new(b, &mut self.module, u, &mut self.rt_ids, flag_id)?;
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
        self.with_function(id, sig, &name, u, |fc| {
            let entry = fc.b.create_block();
            fc.b.switch_to_block(entry);
            fc.b.seal_block(entry);
            fc.start(argv_cap, locals)?;
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
        self.with_function(id, sig, &name, u, |fc| {
            let entry = fc.b.create_block();
            fc.b.append_block_params_for_function_params(entry);
            fc.b.switch_to_block(entry);
            fc.b.seal_block(entry);
            let params = fc.b.block_params(entry).to_vec();
            let (fv, argc, argv) = (params[0], params[1], params[2]);
            fc.start(argv_cap, locals)?;
            // Stack-depth and interrupt guard. Nothing is live yet, so the exception
            // path simply returns EXC.
            let sp = fc.b.ins().get_stack_pointer(types::I64);
            let guard = fc.call_rt(RtFn::StackCheck, &[sp]).unwrap();
            fc.check_exc(guard);

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
            fc.b.ins().return_(&[r]);

            for (i, a) in f.arities.iter().enumerate() {
                fc.b.switch_to_block(blocks[i]);
                fc.b.seal_block(blocks[i]);
                fc.compile_arity(a, argv, argc)?;
            }
            Ok(())
        })
    }

    fn compile_init(&mut self, u: &mut UnitCtx) -> CResult<FuncId> {
        let sig = thunk_signature(&self.module);
        let name = format!("u{}_init", u.unit);
        let id = u.init_id;
        let consts = u.consts.clone();
        self.with_function(id, sig, &name, u, |fc| {
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
                    } => {
                        let fref = fc.func_ref(*func);
                        let addr = fc.b.ins().func_addr(types::I64, fref);
                        let (np, nl) = fc.blob_ptr(name);
                        let mask = fc.b.ins().iconst(types::I64, *mask as i64);
                        let vmin = fc.b.ins().iconst(types::I64, *vmin);
                        fc.call_rt(RtFn::FnProtoNew, &[addr, np, nl, mask, vmin])
                            .unwrap()
                    }
                };
                if matches!(
                    c,
                    ConstEntry::Str(_)
                        | ConstEntry::Symbol(..)
                        | ConstEntry::Double(_)
                        | ConstEntry::Long(_)
                        | ConstEntry::Edn(_)
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
        Node::Const(_) | Node::Local(_) | Node::VarRef(_) | Node::TheVar(_) | Node::Fn(_) => 0,
        Node::Def { init, .. } => init.as_ref().map_or(0, |n| max_args(n)),
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
        Node::Coll { items, .. } => items
            .len()
            .max(items.iter().map(max_args).max().unwrap_or(0)),
    }
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
}

#[derive(Clone)]
struct LoopCtx {
    header: Block,
    vars: Vec<Variable>,
    /// `cleanup.len()` right after the loop variables were bound: everything above it
    /// was bound or produced inside the body and must be released before `recur`.
    depth: usize,
}

#[derive(Clone, Copy)]
struct Slot {
    var: Variable,
}

struct FnCtx<'a, M: Module> {
    b: FunctionBuilder<'a>,
    module: &'a mut M,
    unit: &'a mut UnitCtx,
    rt_ids: &'a mut HashMap<RtFn, FuncId>,
    rt_refs: HashMap<RtFn, FuncRef>,
    func_refs: HashMap<FuncId, FuncRef>,
    locals: HashMap<LocalId, Slot>,
    cleanup: Vec<Cleanup>,
    loops: HashMap<LoopId, LoopCtx>,
    fn_recur: Option<LoopCtx>,
    argv_slot: Option<StackSlot>,
    table_base: Value,
    blob_base: Value,
    call_sig: SigRef,
    local_names: Vec<String>,
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
            func_refs: HashMap::new(),
            locals: HashMap::new(),
            cleanup: Vec::new(),
            loops: HashMap::new(),
            fn_recur: None,
            argv_slot: None,
            table_base: dummy,
            blob_base: dummy,
            call_sig,
            local_names: Vec::new(),
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
        self.local_names = locals.iter().map(|l| l.name.clone()).collect();
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
        if op.own == Own::Owned {
            self.call_rt(RtFn::Release, &[op.val]);
        }
    }

    /// Turn a borrowed operand into an owned one (+1). Immediates stay immediate.
    fn make_owned(&mut self, op: Operand) -> Operand {
        match op.own {
            Own::Borrowed => {
                self.call_rt(RtFn::Retain, &[op.val]);
                Operand::owned(op.val)
            }
            _ => op,
        }
    }

    /// Like `make_owned` but the result is always tagged `Owned`, for storage in a
    /// slot whose later contents may be heap values (loop variables, var roots).
    fn make_owned_force(&mut self, op: Operand) -> Operand {
        let op = self.make_owned(op);
        Operand::owned(op.val)
    }

    fn push_tmp(&mut self, op: Operand) {
        if op.own == Own::Owned {
            self.cleanup.push(Cleanup::Val(op.val));
        }
    }

    fn pop_tmp(&mut self, op: Operand) {
        if op.own == Own::Owned {
            let top = self.cleanup.pop();
            debug_assert!(
                matches!(top, Some(Cleanup::Val(v)) if v == op.val),
                "owned-temporary stack mismatch"
            );
        }
    }

    fn bind_local(&mut self, id: LocalId, val: Value, owned: bool) -> Variable {
        let var = self.b.declare_var(types::I64);
        self.b.def_var(var, val);
        self.locals.insert(id, Slot { var });
        if owned {
            self.cleanup.push(Cleanup::Var(var));
        }
        var
    }

    /// Release and forget owned locals in reverse order of binding.
    fn release_locals(&mut self, vars: &[(LocalId, Variable, bool)]) {
        for (id, var, owned) in vars.iter().rev() {
            if *owned {
                let top = self.cleanup.pop();
                debug_assert!(
                    matches!(top, Some(Cleanup::Var(v)) if v == *var),
                    "cleanup stack mismatch"
                );
                let v = self.b.use_var(*var);
                self.call_rt(RtFn::Release, &[v]);
            }
            self.locals.remove(id);
        }
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
        for c in items {
            let v = match c {
                Cleanup::Val(v) => v,
                Cleanup::Var(var) => self.b.use_var(var),
            };
            self.call_rt(RtFn::Release, &[v]);
        }
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
        self.emit_exc_path(r);
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
            Node::Local(id) => {
                let slot = *self.locals.get(id).ok_or_else(|| {
                    internal(format!(
                        "unbound local #{id} ({})",
                        self.local_names
                            .get(*id as usize)
                            .map_or("?", |l| l.as_str())
                    ))
                })?;
                Ok(Operand::borrowed(self.b.use_var(slot.var)))
            }
            Node::VarRef(v) => {
                let var = v.get();
                let p = self.const_slot(ConstEntry::Var(var.ns.to_string(), var.name.to_string()));
                let r = self.call_rt(RtFn::VarRoot, &[p]).unwrap();
                self.check_exc(r);
                Ok(Operand::owned(r))
            }
            Node::TheVar(v) => {
                let var = v.get();
                let p = self.const_slot(ConstEntry::Var(var.ns.to_string(), var.name.to_string()));
                Ok(Operand::borrowed(p))
            }
            Node::Def { var, init } => {
                let vo = var.get();
                let p = self.const_slot(ConstEntry::Var(vo.ns.to_string(), vo.name.to_string()));
                match init {
                    None => Ok(Operand::borrowed(p)),
                    Some(init) => {
                        let op = self.compile(init)?;
                        let op = self.make_owned_force(op);
                        let r = self.call_rt(RtFn::VarBindRoot, &[p, op.val]).unwrap();
                        Ok(Operand::borrowed(r))
                    }
                }
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
            Node::Fn(f) => self.compile_fn_value(f),
            Node::Call { f, args, .. } => self.compile_call(f, args),
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
            return self.compile(body);
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
            Kind::List | Kind::Vector | Kind::Map | Kind::Set => {
                ConstEntry::Edn(printer::pr_str(v))
            }
            k => {
                return Err(internal(format!(
                    "cannot embed a {} as a constant",
                    k.name()
                )));
            }
        };
        Ok(Operand::borrowed(self.const_slot(entry)))
    }

    fn compile_if(&mut self, test: &Node, then: &Node, else_: &Node) -> CResult<Operand> {
        let t = self.compile(test)?;
        let c = self.truthy(t.val);
        self.release(t);
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
                vars: vars.iter().map(|(_, v, _)| *v).collect(),
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
        let mut ops = Vec::with_capacity(args.len());
        for a in args {
            let op = self.compile(a)?;
            self.push_tmp(op);
            ops.push(op);
        }
        for op in ops.iter().rev() {
            self.pop_tmp(*op);
        }
        let new_vals: Vec<Value> = ops
            .iter()
            .map(|op| self.make_owned_force(*op).val)
            .collect();
        // Locals bound inside the loop body (`let` etc.) are live on this path and
        // would otherwise leak once per iteration: release them, newest first. They
        // stay on the compile-time stack for the fall-through path.
        let inner: Vec<Cleanup> = self.cleanup[ctx.depth..].iter().rev().copied().collect();
        for c in inner {
            let v = match c {
                Cleanup::Val(v) => v,
                Cleanup::Var(var) => self.b.use_var(var),
            };
            self.call_rt(RtFn::Release, &[v]);
        }
        let olds: Vec<Value> = ctx.vars.iter().map(|v| self.b.use_var(*v)).collect();
        for (var, val) in ctx.vars.iter().zip(new_vals) {
            self.b.def_var(*var, val);
        }
        for old in olds {
            self.call_rt(RtFn::Release, &[old]);
        }
        self.b.ins().jump(ctx.header, &[]);
        self.dead_block();
        Ok(self.imm(api::consts::NIL))
    }

    fn compile_fn_value(&mut self, f: &Rc<FnNode>) -> CResult<Operand> {
        let func = self.unit.fn_id(self.module, f)?;
        let proto = self.const_slot(ConstEntry::Proto {
            func,
            name: f.name.clone(),
            mask: f.arity_mask(),
            vmin: f.variadic_min(),
        });
        let n = self.b.ins().iconst(types::I64, f.captures.len() as i64);
        let fv = self.call_rt(RtFn::FnNew, &[proto, n]).unwrap();
        for (i, id) in f.captures.iter().enumerate() {
            let slot = *self
                .locals
                .get(id)
                .ok_or_else(|| internal(format!("capture of unbound local #{id}")))?;
            let v = self.b.use_var(slot.var);
            self.call_rt(RtFn::Retain, &[v]);
            let i = self.b.ins().iconst(types::I64, i as i64);
            self.call_rt(RtFn::FnSetCapture, &[fv, i, v]);
        }
        Ok(Operand::owned(fv))
    }

    /// Evaluate `items` and store them in the argv slot; returns (addr, count).
    fn store_args(&mut self, items: &[Node]) -> CResult<(Value, Value, Vec<Operand>)> {
        let mut ops = Vec::with_capacity(items.len());
        for a in items {
            let op = self.compile(a)?;
            self.push_tmp(op);
            ops.push(op);
        }
        let addr = self.argv_addr();
        for (i, op) in ops.iter().enumerate() {
            self.b
                .ins()
                .store(MemFlagsData::trusted(), op.val, addr, 8 * i as i32);
        }
        let n = self.b.ins().iconst(types::I64, items.len() as i64);
        Ok((addr, n, ops))
    }

    fn release_tmps(&mut self, ops: &[Operand]) {
        for op in ops.iter().rev() {
            self.pop_tmp(*op);
            self.release(*op);
        }
    }

    fn compile_call(&mut self, f: &Node, args: &[Node]) -> CResult<Operand> {
        let fop = self.compile(f)?;
        self.push_tmp(fop);
        let (argv, argc, ops) = self.store_args(args)?;
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
            .call_indirect(self.call_sig, entry, &[fv, argc, argv]);
        let r1 = self.b.inst_results(inst)[0];
        self.b.ins().jump(merge, &[BlockArg::Value(r1)]);

        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let r2 = self.call_rt(RtFn::Invoke, &[fv, argc, argv]).unwrap();
        self.b.ins().jump(merge, &[BlockArg::Value(r2)]);

        self.b.switch_to_block(merge);
        self.b.seal_block(merge);
        let r = self.b.block_params(merge)[0];
        self.check_exc(r);
        self.release_tmps(&ops);
        self.pop_tmp(fop);
        self.release(fop);
        Ok(Operand::owned(r))
    }

    fn compile_coll(&mut self, kind: CollKind, items: &[Node]) -> CResult<Operand> {
        let (addr, n, ops) = self.store_args(items)?;
        let f = match kind {
            CollKind::Vector => RtFn::VectorFromSlice,
            CollKind::Map => RtFn::MapFromSlice,
            CollKind::Set => RtFn::SetFromSlice,
        };
        let r = self.call_rt(f, &[addr, n]).unwrap();
        self.release_tmps(&ops);
        Ok(Operand::owned(r))
    }

    /// Body of one fn arity: bind params (and rest), compile the body as the recur
    /// target, release owned locals, return.
    fn compile_arity(&mut self, a: &Arity, argv: Value, argc: Value) -> CResult<()> {
        let mut vars: Vec<(LocalId, Variable, bool)> = Vec::new();
        for (j, id) in a.params.iter().enumerate() {
            let v = self
                .b
                .ins()
                .load(types::I64, MemFlagsData::trusted(), argv, 8 * j as i32);
            if a.has_recur {
                self.call_rt(RtFn::Retain, &[v]);
            }
            let var = self.bind_local(*id, v, a.has_recur);
            vars.push((*id, var, a.has_recur));
        }
        if let Some(id) = a.rest {
            let nfixed = a.params.len() as i64;
            let extra = self.b.ins().iadd_imm_s(argc, -nfixed);
            let addr = self.b.ins().iadd_imm_s(argv, 8 * nfixed);
            let r = self.call_rt(RtFn::ListFromSlice, &[addr, extra]).unwrap();
            let var = self.bind_local(id, r, true);
            vars.push((id, var, true));
        }
        let header = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.b.switch_to_block(header);
        self.fn_recur = Some(LoopCtx {
            header,
            vars: vars.iter().map(|(_, v, _)| *v).collect(),
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
