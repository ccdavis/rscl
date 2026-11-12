//! Forms (runtime `Value`s) → `Node` AST.
//!
//! Handles special forms, symbol resolution (locals, captures, vars), built-in
//! rewrite macros (`defn`, `let`, `when`, ... until `core.clj` defines the real ones),
//! closure conversion (free variables become captures) and `recur` validation.
use std::collections::HashMap;
use std::rc::Rc;

use rscl_runtime::core::BuiltinDesc;
use rscl_runtime::obj::var::{FLAG_DYNAMIC, VarObj};
use rscl_runtime::obj::{Kind, func, keyword, list, map, symbol, vector};
use rscl_runtime::{Value, ns, pos};

use crate::errors::{CResult, fail};

mod rewrite;

pub type LocalId = u32;
pub type LoopId = u32;

#[derive(Clone, Copy, Debug)]
pub struct VarPtr(pub *const VarObj);

impl VarPtr {
    pub fn get(&self) -> &'static VarObj {
        unsafe { &*self.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollKind {
    Vector,
    Map,
    Set,
}

#[derive(Debug)]
pub enum Node {
    Const(Value),
    Local(LocalId),
    VarRef(VarPtr),
    TheVar(VarPtr),
    /// `*ns*`: the current namespace object.
    CurrentNs,
    Def {
        var: VarPtr,
        init: Option<Box<Node>>,
        /// Metadata for the var (`^:dynamic`, `:doc`, ...): a map expression,
        /// evaluated and applied after binding.
        meta: Option<Box<Node>>,
    },
    /// `(set! var value)` on a dynamic var.
    VarSet {
        var: VarPtr,
        value: Box<Node>,
    },
    If {
        test: Box<Node>,
        then: Box<Node>,
        else_: Box<Node>,
    },
    Do(Vec<Node>),
    Let {
        bindings: Vec<(LocalId, Node)>,
        body: Box<Node>,
    },
    Loop {
        id: LoopId,
        bindings: Vec<(LocalId, Node)>,
        body: Box<Node>,
        /// Position of the `loop*` form (for the interrupt check at its header).
        pos: u32,
    },
    Recur {
        target: RecurTarget,
        args: Vec<Node>,
    },
    Fn(Rc<FnNode>),
    Call {
        f: Box<Node>,
        args: Vec<Node>,
        pos: u32,
    },
    /// A call to a Rust builtin through its var, resolved at compile time (the var's
    /// root is that builtin and the var is neither dynamic nor a macro). Compiled as a
    /// direct call, with inline fast paths for fixnum arithmetic and comparisons.
    BuiltinCall {
        desc: &'static BuiltinDesc,
        args: Vec<Node>,
        pos: u32,
    },
    /// A vector/map/set literal with non-constant parts (maps: alternating k, v).
    Coll {
        kind: CollKind,
        items: Vec<Node>,
    },
    Throw {
        expr: Box<Node>,
        pos: u32,
    },
    Try {
        body: Box<Node>,
        catches: Vec<CatchClause>,
        finally: Option<Box<Node>>,
    },
    /// A typed call to a C-ABI symbol resolved at unit initialisation.
    NativeCall {
        symbol: String,
        ret: NativeTy,
        params: Vec<NativeTy>,
        args: Vec<Node>,
    },
}

#[derive(Debug)]
pub struct CatchClause {
    /// Exception class name (`Exception`, `ArithmeticException`, ...).
    pub class: String,
    pub local: LocalId,
    pub body: Node,
}

/// C types understood by the typed FFI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTy {
    Void,
    Long,
    Int,
    Double,
    Float,
    Bool,
    Str,
    Ptr,
    /// 32-bit unsigned: zero-extended on return.
    UInt,
}

impl NativeTy {
    pub fn parse(name: &str) -> Option<NativeTy> {
        Some(match name {
            "void" => NativeTy::Void,
            "long" | "i64" | "u64" | "size_t" => NativeTy::Long,
            "int" | "i32" => NativeTy::Int,
            "u32" => NativeTy::UInt,
            "double" | "f64" => NativeTy::Double,
            "float" | "f32" => NativeTy::Float,
            "bool" => NativeTy::Bool,
            "str" | "string" | "cstr" => NativeTy::Str,
            "ptr" | "pointer" => NativeTy::Ptr,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurTarget {
    Loop(LoopId),
    /// The enclosing fn arity's parameters (plus the rest param, if any).
    Fn,
}

#[derive(Debug)]
pub struct FnNode {
    /// Qualified display name, e.g. `user/fib` or `user/fn__12`.
    pub name: String,
    pub arities: Vec<Arity>,
    /// Outer locals this fn closes over, in capture-slot order.
    pub captures: Vec<LocalId>,
    /// The local bound to the fn object itself for `(fn name [..] ..)`.
    pub self_local: Option<LocalId>,
    /// Position of the `fn*` form (for the stack and interrupt check at entry).
    pub pos: u32,
}

#[derive(Debug)]
pub struct Arity {
    pub params: Vec<LocalId>,
    pub rest: Option<LocalId>,
    pub body: Node,
    pub has_recur: bool,
}

impl FnNode {
    pub fn arity_mask(&self) -> u64 {
        self.arities
            .iter()
            .filter(|a| a.rest.is_none())
            .fold(0, |m, a| m | (1u64 << a.params.len()))
    }
    pub fn variadic_min(&self) -> i64 {
        self.arities
            .iter()
            .find(|a| a.rest.is_some())
            .map_or(-1, |a| a.params.len() as i64)
    }
}

/// Static type of a local or expression: a tagged value, or an unboxed primitive
/// (`^long`/`^double` hints, numeric literals and arithmetic on typed operands).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Val,
    Long,
    Double,
}

#[derive(Debug, Clone)]
pub struct LocalInfo {
    pub name: String,
    pub ty: Ty,
}

struct RecurCtx {
    target: RecurTarget,
    arity: usize,
    used: bool,
    /// `try` nesting depth when the target was entered; `recur` may not cross a `try`.
    try_depth: usize,
    /// Static types of the arguments of every `recur` to this target, for checking
    /// that primitive loop locals stay primitive.
    arg_tys: Vec<Vec<Ty>>,
}

/// `^long` / `^double` on a binding symbol.
fn hint_of(form: &Value) -> Option<Ty> {
    let sym = symbol::as_sym(form)?;
    let m = map::as_map(&sym.meta)?;
    let tag = m.m.get(&keyword::intern(None, "tag"))?;
    match symbol::as_sym(tag)?.name() {
        "double" => Some(Ty::Double),
        "long" => Some(Ty::Long),
        _ => None,
    }
}

/// `recur` or `throw` never produce a value, so they take the type of the other branch.
fn is_bottom(n: &Node) -> bool {
    matches!(n, Node::Recur { .. } | Node::Throw { .. })
}

/// Memo of expression types keyed by node address (the tree is not mutated while a
/// function is compiled).
pub type TyCache = HashMap<usize, Ty>;

/// The static type of an expression given the locals' types. Sound by construction:
/// a `Long` expression always evaluates to an integer (or throws), a `Double` one to
/// a double (or throws), so unboxed evaluation cannot change a program's result.
pub fn ty_of(node: &Node, locals: &[LocalInfo]) -> Ty {
    ty_of_memo(node, locals, &mut TyCache::new())
}

/// `ty_of` with a memo, for code generation (the analyzer's locals may still change
/// while it infers, so it uses a fresh memo per query).
pub fn ty_of_memo(node: &Node, locals: &[LocalInfo], cache: &mut TyCache) -> Ty {
    let key = node as *const Node as usize;
    if let Some(&t) = cache.get(&key) {
        return t;
    }
    let t = match node {
        Node::Const(v) => {
            // A BigInt constant is a value even when it fits: it stays a BigInt.
            if v.as_fixnum().is_some() || v.is_kind(Kind::Long) {
                Ty::Long
            } else if v.as_double().is_some() {
                Ty::Double
            } else {
                Ty::Val
            }
        }
        Node::Local(id) => locals.get(*id as usize).map_or(Ty::Val, |l| l.ty),
        Node::If { then, else_, .. } => {
            let t = ty_of_memo(then, locals, cache);
            let e = ty_of_memo(else_, locals, cache);
            if is_bottom(then) {
                e
            } else if is_bottom(else_) || t == e {
                t
            } else {
                Ty::Val
            }
        }
        Node::Do(nodes) => nodes
            .last()
            .map_or(Ty::Val, |n| ty_of_memo(n, locals, cache)),
        Node::Let { body, .. } | Node::Loop { body, .. } => ty_of_memo(body, locals, cache),
        Node::BuiltinCall { desc, args, .. } => match PrimOp::of(desc, args.len()) {
            Some(op) => {
                let tys: Vec<Ty> = args.iter().map(|a| ty_of_memo(a, locals, cache)).collect();
                op.result_ty(&tys)
            }
            None => Ty::Val,
        },
        _ => Ty::Val,
    };
    cache.insert(key, t);
    t
}

/// Core operators the compiler implements itself: fixnum fast paths on tagged values
/// and unboxed arithmetic on primitives. One table drives type inference and code
/// generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimOp {
    Add,
    Sub,
    Mul,
    Div,
    Inc,
    Dec,
    Rem,
    Quot,
    Mod,
    ToDouble,
    ToLong,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    NumEq,
    Ne,
    Zero,
    Pos,
    Neg,
}

impl PrimOp {
    pub fn of(desc: &BuiltinDesc, argc: usize) -> Option<PrimOp> {
        if desc.ns != "clojure.core" {
            return None;
        }
        Some(match (desc.name, argc) {
            ("+", _) => PrimOp::Add,
            ("*", _) => PrimOp::Mul,
            ("-", n) if n >= 1 => PrimOp::Sub,
            ("/", n) if n >= 1 => PrimOp::Div,
            ("inc", 1) => PrimOp::Inc,
            ("dec", 1) => PrimOp::Dec,
            ("rem", 2) => PrimOp::Rem,
            ("quot", 2) => PrimOp::Quot,
            ("mod", 2) => PrimOp::Mod,
            ("double", 1) => PrimOp::ToDouble,
            ("long", 1) => PrimOp::ToLong,
            ("<", 2) => PrimOp::Lt,
            ("<=", 2) => PrimOp::Le,
            (">", 2) => PrimOp::Gt,
            (">=", 2) => PrimOp::Ge,
            ("=", 2) => PrimOp::Eq,
            ("==", 2) => PrimOp::NumEq,
            ("not=", 2) => PrimOp::Ne,
            ("zero?", 1) => PrimOp::Zero,
            ("pos?", 1) => PrimOp::Pos,
            ("neg?", 1) => PrimOp::Neg,
            _ => return None,
        })
    }

    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            PrimOp::Lt
                | PrimOp::Le
                | PrimOp::Gt
                | PrimOp::Ge
                | PrimOp::Eq
                | PrimOp::NumEq
                | PrimOp::Ne
        )
    }

    pub fn is_sign_test(self) -> bool {
        matches!(self, PrimOp::Zero | PrimOp::Pos | PrimOp::Neg)
    }

    /// Comparisons and sign tests: the result is a boolean on every path.
    pub fn is_predicate(self) -> bool {
        self.is_comparison() || self.is_sign_test()
    }

    /// Operators with a fast path on tagged fixnums when the operand types are not
    /// statically known.
    pub fn has_fixnum_path(self, argc: usize) -> bool {
        match self {
            PrimOp::Add | PrimOp::Sub | PrimOp::Mul => argc == 2,
            PrimOp::Inc | PrimOp::Dec => true,
            _ => self.is_predicate(),
        }
    }

    /// Result type of an arithmetic operator: `Long` when every operand is a long
    /// (integer overflow throws as it would in the builtin), `Double` when any operand
    /// is a double (the rest are coerced, with a `ClassCastException` for non-numbers,
    /// as the builtin would throw), `Val` otherwise. Predicates are values (booleans).
    pub fn result_ty(self, tys: &[Ty]) -> Ty {
        let all_long = tys.iter().all(|t| *t == Ty::Long);
        let any_double = tys.contains(&Ty::Double);
        match self {
            PrimOp::Add | PrimOp::Mul if tys.is_empty() => Ty::Long,
            PrimOp::Add | PrimOp::Sub | PrimOp::Mul => {
                if any_double {
                    Ty::Double
                } else if all_long {
                    Ty::Long
                } else {
                    Ty::Val
                }
            }
            PrimOp::Div if any_double => Ty::Double,
            PrimOp::Inc | PrimOp::Dec if tys[0] != Ty::Val => tys[0],
            PrimOp::Rem | PrimOp::Quot | PrimOp::Mod if all_long => Ty::Long,
            PrimOp::ToDouble => Ty::Double,
            PrimOp::ToLong => Ty::Long,
            _ => Ty::Val,
        }
    }
}

struct Frame {
    scopes: Vec<Vec<(String, LocalId)>>,
    captures: Vec<LocalId>,
    recur: Vec<RecurCtx>,
    try_depth: usize,
}

pub struct Analyzer {
    pub ns: &'static ns::NsObj,
    pub locals: Vec<LocalInfo>,
    frames: Vec<Frame>,
    next_loop: LoopId,
    fn_counter: u32,
    /// Display name for the fn a `def` is about to analyse as its init.
    fn_name_hint: Option<String>,
    /// Vars whose `def` is being analysed (innermost last).
    defining: Vec<VarPtr>,
}

pub use rscl_runtime::reader::is_special;

/// The builtin a var call may be linked to directly: the var must currently hold the
/// runtime's own fn for that name (so a redefinition before this point is honoured),
/// and must not be dynamic or a macro. Later rebinding of such a var is not seen by
/// code compiled earlier (direct linking, as `clojure.compiler.direct-linking` does).
fn direct_builtin(var: &VarObj) -> Option<&'static BuiltinDesc> {
    if var.has_flag(FLAG_DYNAMIC) || var.is_macro() {
        return None;
    }
    let desc = rscl_runtime::core::find(&var.ns, &var.name)?;
    let root = var.root();
    let is_that_builtin =
        func::as_fn(&root).is_some_and(|f| std::ptr::fn_addr_eq(f.proto().entry, desc.entry));
    is_that_builtin.then_some(desc)
}

impl Analyzer {
    pub fn new(ns: &'static ns::NsObj) -> Self {
        Analyzer {
            ns,
            locals: Vec::new(),
            frames: Vec::new(),
            next_loop: 0,
            fn_counter: 0,
            fn_name_hint: None,
            defining: Vec::new(),
        }
    }

    /// Analyze one top-level form. (`do` at top level is split by the driver.)
    pub fn analyze_toplevel(&mut self, form: &Value) -> CResult<Node> {
        self.frames.clear();
        self.frames.push(Frame {
            scopes: vec![Vec::new()],
            captures: Vec::new(),
            recur: Vec::new(),
            try_depth: 0,
        });
        let node = self.analyze(form, true)?;
        self.frames.pop();
        Ok(node)
    }

    // ---- environment -------------------------------------------------------------

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("analyzer frame")
    }

    fn declare_local(&mut self, name: &str) -> LocalId {
        self.declare_typed(name, Ty::Val)
    }

    fn declare_typed(&mut self, name: &str, ty: Ty) -> LocalId {
        let id = self.locals.len() as LocalId;
        self.locals.push(LocalInfo {
            name: name.to_string(),
            ty,
        });
        self.frame()
            .scopes
            .last_mut()
            .unwrap()
            .push((name.to_string(), id));
        id
    }

    fn push_scope(&mut self) {
        self.frame().scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.frame().scopes.pop();
    }

    /// Find a local by name, registering captures through intermediate fn frames.
    fn lookup_local(&mut self, name: &str) -> Option<LocalId> {
        let cur = self.frames.len() - 1;
        for k in (0..=cur).rev() {
            let found = self.frames[k].scopes.iter().rev().find_map(|scope| {
                scope
                    .iter()
                    .rev()
                    .find(|(n, _)| n == name)
                    .map(|(_, id)| *id)
            });
            if let Some(id) = found {
                for j in (k + 1)..=cur {
                    if !self.frames[j].captures.contains(&id) {
                        self.frames[j].captures.push(id);
                    }
                }
                return Some(id);
            }
        }
        None
    }

    fn is_local(&self, name: &str) -> bool {
        self.frames
            .iter()
            .any(|f| f.scopes.iter().any(|s| s.iter().any(|(n, _)| n == name)))
    }

    // ---- dispatch ----------------------------------------------------------------

    pub fn analyze(&mut self, form: &Value, tail: bool) -> CResult<Node> {
        // `^{:k (f)} [x]`: a literal's metadata is evaluated and attached at run
        // time, unless literal and metadata are constant (then it is embedded).
        if matches!(form.kind(), Some(Kind::Vector | Kind::Map | Kind::Set))
            && let Some(m) = rscl_runtime::obj::meta(form)
            && !m.is_nil()
            && !(is_constant(form) && is_constant(&m))
        {
            let with_meta = ns::core().intern("with-meta").root();
            let bare = with_meta
                .call(&[*form, Value::NIL])
                .or_else(|_| fail(form, "cannot strip literal metadata"))?;
            let call = list::from_slice(&[
                symbol::new(Some("clojure.core"), "with-meta"),
                bare,
                quote_tag(&m),
            ]);
            return self.analyze_list(&call, tail);
        }
        match form.kind() {
            Some(Kind::Symbol) => self.analyze_symbol(form),
            // Macro expansions may return cons cells or lazy seqs: any seq is a call form.
            Some(Kind::List | Kind::Cons | Kind::LazySeq | Kind::VecSeq) => {
                self.analyze_list(form, tail)
            }
            Some(Kind::Vector) => {
                let items = vector::get(form).iter().cloned().collect::<Vec<_>>();
                self.analyze_coll(form, CollKind::Vector, &items)
            }
            Some(Kind::Map) => {
                if is_constant(form) {
                    return Ok(Node::Const(*form));
                }
                // In the order the entries were written (the reader records it), so
                // the value expressions run left to right.
                let mut items = Vec::new();
                for (k, v) in map::ordered_entries(form) {
                    items.push(k);
                    items.push(v);
                }
                self.analyze_coll(form, CollKind::Map, &items)
            }
            Some(Kind::Set) => {
                if is_constant(form) {
                    return Ok(Node::Const(*form));
                }
                let items = rscl_runtime::obj::set::ordered_items(form);
                self.analyze_coll(form, CollKind::Set, &items)
            }
            // Sorted collections only come from macro expansions (there is no literal
            // syntax); compile them as a constructor call so sortedness survives.
            Some(Kind::SortedMap) => {
                let mut items = vec![symbol::new(Some("clojure.core"), "sorted-map")];
                for (k, v) in rscl_runtime::obj::sorted::map_get(form).m.iter() {
                    items.push(k.0);
                    items.push(*v);
                }
                self.analyze_list(&list::from_iter(items), tail)
            }
            Some(Kind::SortedSet) => {
                let mut items = vec![symbol::new(Some("clojure.core"), "sorted-set")];
                items.extend(
                    rscl_runtime::obj::sorted::set_get(form)
                        .s
                        .iter()
                        .map(|k| k.0),
                );
                self.analyze_list(&list::from_iter(items), tail)
            }
            Some(Kind::Instance | Kind::Type | Kind::Protocol) => fail(
                form,
                format!(
                    "a {} value cannot be embedded in compiled code; return a form that constructs it (a macro expansion may not contain one)",
                    rscl_runtime::core::type_name(form)
                ),
            ),
            // A var object (a macro returned `(resolve 'f)`) evaluates to itself.
            Some(Kind::Var) => Ok(Node::TheVar(VarPtr(form.ptr() as *const VarObj))),
            _ => {
                check_embeddable(form, form)?;
                Ok(Node::Const(*form))
            }
        }
    }

    fn analyze_coll(&mut self, form: &Value, kind: CollKind, items: &[Value]) -> CResult<Node> {
        if items.iter().all(is_constant) {
            return Ok(Node::Const(*form));
        }
        let nodes = items
            .iter()
            .map(|x| self.analyze(x, false))
            .collect::<CResult<Vec<_>>>()?;
        Ok(Node::Coll { kind, items: nodes })
    }

    fn analyze_symbol(&mut self, form: &Value) -> CResult<Node> {
        let sym = symbol::get(form);
        if sym.ns().is_none()
            && let Some(id) = self.lookup_local(sym.name())
        {
            return Ok(Node::Local(id));
        }
        if sym.ns().is_none() && sym.name() == "*ns*" {
            return Ok(Node::CurrentNs);
        }
        match self.resolve_var(sym.ns(), sym.name()) {
            Some(v) => Ok(Node::VarRef(v)),
            None => match sym.ns() {
                Some(q) if self.ns.resolve_alias(q).is_none() && ns::find(q).is_none() => {
                    fail(form, format!("No such namespace: {q}"))
                }
                Some(_) => fail(form, format!("No such var: {}", sym.qualified())),
                None => fail(
                    form,
                    format!("Unable to resolve symbol: {} in this context", sym.name()),
                ),
            },
        }
    }

    fn resolve_var(&self, sym_ns: Option<&str>, name: &str) -> Option<VarPtr> {
        ns::resolve(self.ns, sym_ns, name).map(|v| VarPtr(v as *const VarObj))
    }

    fn analyze_list(&mut self, form: &Value, tail: bool) -> CResult<Node> {
        let Some(items) = seq_items(form) else {
            return fail(form, "cannot realise sequence form");
        };
        if items.is_empty() {
            return Ok(Node::Const(list::empty()));
        }
        let head = &items[0];
        if let Some(sym) = symbol::as_sym(head) {
            if sym.ns().is_none() && is_special(sym.name()) {
                return self.analyze_special(sym.name(), form, &items, tail);
            }
            // Interop-style sugar: `(.-x obj)` reads a field, `(Point. 1 2)` constructs.
            if sym.ns().is_none() && !self.is_local(sym.name()) {
                let name = sym.name();
                if let Some(field) = name.strip_prefix(".-")
                    && !field.is_empty()
                    && items.len() == 2
                {
                    let expanded = list::from_slice(&[
                        symbol::new(Some("clojure.core"), "field*"),
                        items[1],
                        list::from_slice(&[symbol::new(None, "quote"), symbol::new(None, field)]),
                    ]);
                    self.stamp_expansion(form, &expanded);
                    return self.analyze(&expanded, tail);
                }
                if let Some(tname) = name.strip_suffix('.')
                    && !tname.is_empty()
                {
                    let mut v = vec![symbol::new(None, &format!("->{tname}"))];
                    v.extend_from_slice(&items[1..]);
                    let expanded = list::from_slice(&v);
                    self.stamp_expansion(form, &expanded);
                    return self.analyze(&expanded, tail);
                }
            }
            let shadowed = sym.ns().is_none() && self.is_local(sym.name());
            if !shadowed {
                // A macro var: call it at compile time on the unevaluated forms.
                if let Some(var) = self.resolve_var(sym.ns(), sym.name())
                    && var.get().is_macro()
                {
                    let expanded = self.macroexpand(var, form, &items)?;
                    self.stamp_expansion(form, &expanded);
                    return self.analyze(&expanded, tail);
                }
                // Built-in rewrites: bootstrap fallbacks until core.clj defines the macro.
                if sym.ns().is_none()
                    && let Some(expanded) = rewrite::expand(sym.name(), form, &items)?
                {
                    self.stamp_expansion(form, &expanded);
                    return self.analyze(&expanded, tail);
                }
            }
        }
        let f = self.analyze(head, false)?;
        let args = items[1..]
            .iter()
            .map(|a| self.analyze(a, false))
            .collect::<CResult<Vec<_>>>()?;
        if let Node::VarRef(v) = &f
            // A var being defined right now (`(defn count ..)` in clojure.core) still
            // has the builtin as its root: calls must wait for the new definition.
            && !self.defining.iter().any(|d| std::ptr::eq(d.0, v.0))
            && let Some(desc) = direct_builtin(v.get())
        {
            return Ok(Node::BuiltinCall {
                desc,
                args,
                pos: pos::pos_id(form),
            });
        }
        Ok(Node::Call {
            f: Box::new(f),
            args,
            pos: pos::pos_id(form),
        })
    }

    /// Run a macro: `(macro-fn &form &env args...)`. An exception thrown by the macro
    /// is a compile error at the call site.
    fn macroexpand(&mut self, var: VarPtr, form: &Value, items: &[Value]) -> CResult<Value> {
        let f = var.get().root();
        if f.is_unbound() {
            return fail(form, format!("macro {} is unbound", var.get().qualified()));
        }
        let mut args = Vec::with_capacity(items.len() + 1);
        args.push(*form);
        args.push(Value::NIL);
        args.extend(items[1..].iter().cloned());
        match f.call(&args) {
            Ok(v) => Ok(v),
            Err(_) => {
                let e = rscl_runtime::exc::take().unwrap_or(Value::NIL);
                fail(
                    form,
                    format!(
                        "Unexpected error macroexpanding {}: {}",
                        var.get().qualified(),
                        rscl_runtime::exc::format_exception(&e)
                    ),
                )
            }
        }
    }

    /// A macro or rewrite may hand back one of the original sub-forms (`(or x)` → x);
    /// only a freshly built form takes the position of the form it replaces.
    fn stamp_expansion(&self, form: &Value, expanded: &Value) {
        if pos::pos_id(expanded) == 0 && expanded.header().is_some_and(|h| !h.is_permanent()) {
            pos::set_pos(expanded, pos::pos_id(form));
        }
    }

    // ---- special forms -------------------------------------------------------------

    fn analyze_special(
        &mut self,
        name: &str,
        form: &Value,
        items: &[Value],
        tail: bool,
    ) -> CResult<Node> {
        match name {
            "quote" => {
                if items.len() != 2 {
                    return fail(form, "Wrong number of args to quote");
                }
                check_embeddable(form, &items[1])?;
                Ok(Node::Const(items[1]))
            }
            "if" => {
                if items.len() < 3 || items.len() > 4 {
                    return fail(
                        form,
                        format!("Wrong number of args ({}) passed to: if", items.len() - 1),
                    );
                }
                let test = self.analyze(&items[1], false)?;
                let then = self.analyze(&items[2], tail)?;
                let else_ = if items.len() == 4 {
                    self.analyze(&items[3], tail)?
                } else {
                    Node::Const(Value::NIL)
                };
                Ok(Node::If {
                    test: Box::new(test),
                    then: Box::new(then),
                    else_: Box::new(else_),
                })
            }
            "do" => self.analyze_body(&items[1..], tail),
            "def" => self.analyze_def(form, items),
            "var" => {
                if items.len() != 2 {
                    return fail(form, "Wrong number of args to var");
                }
                let Some(sym) = symbol::as_sym(&items[1]) else {
                    return fail(form, "var requires a symbol");
                };
                match self.resolve_var(sym.ns(), sym.name()) {
                    Some(v) => Ok(Node::TheVar(v)),
                    None => fail(
                        &items[1],
                        format!("Unable to resolve var: {} in this context", sym.qualified()),
                    ),
                }
            }
            "let*" => self.analyze_let(form, items, tail, false),
            "loop*" => self.analyze_let(form, items, tail, true),
            "recur" => self.analyze_recur(form, items, tail),
            "fn*" => self.analyze_fn(form, items),
            "throw" => {
                if items.len() != 2 {
                    return fail(form, "Wrong number of args to throw");
                }
                Ok(Node::Throw {
                    expr: Box::new(self.analyze(&items[1], false)?),
                    pos: pos::pos_id(form),
                })
            }
            "try" => self.analyze_try(items),
            "native-call*" => self.analyze_native_call(form, items),
            "set!" => {
                if items.len() != 3 {
                    return fail(form, "Wrong number of args to set!");
                }
                let Some(target) = symbol::as_sym(&items[1]) else {
                    return fail(
                        &items[1],
                        "set! target must be a var (fields are immutable)",
                    );
                };
                if target.ns().is_none() && self.lookup_local(target.name()).is_some() {
                    return fail(
                        &items[1],
                        format!("Cannot assign to non-mutable: {}", target.name()),
                    );
                }
                let Some(var) = self.resolve_var(target.ns(), target.name()) else {
                    return fail(
                        &items[1],
                        format!(
                            "Unable to resolve var: {} in this context",
                            target.qualified()
                        ),
                    );
                };
                let value = self.analyze(&items[2], false)?;
                Ok(Node::VarSet {
                    var,
                    value: Box::new(value),
                })
            }
            "catch" | "finally" => fail(form, format!("{name} must appear inside try")),
            "&" => fail(form, "& is only valid in a parameter or binding vector"),
            _ => fail(form, format!("special form {name} is not supported yet")),
        }
    }

    fn analyze_body(&mut self, forms: &[Value], tail: bool) -> CResult<Node> {
        if forms.is_empty() {
            return Ok(Node::Const(Value::NIL));
        }
        let mut nodes = Vec::with_capacity(forms.len());
        for (i, f) in forms.iter().enumerate() {
            let last = i + 1 == forms.len();
            nodes.push(self.analyze(f, tail && last)?);
        }
        if nodes.len() == 1 {
            Ok(nodes.pop().unwrap())
        } else {
            Ok(Node::Do(nodes))
        }
    }

    fn analyze_def(&mut self, form: &Value, items: &[Value]) -> CResult<Node> {
        if items.len() < 2 || items.len() > 4 {
            return fail(form, "Wrong number of args to def");
        }
        let Some(sym) = symbol::as_sym(&items[1]) else {
            return fail(form, "First argument to def must be a Symbol");
        };
        if let Some(q) = sym.ns()
            && q != &*self.ns.name
        {
            return fail(form, "Can't create defs outside of current ns");
        }
        // Interned before the init is analyzed so a fn body can refer to its own name.
        let var_obj = self.ns.intern(sym.name());
        let var = VarPtr(var_obj as *const VarObj);
        let (doc, init) = match items.len() {
            2 => (None, None),
            3 => (None, Some(&items[2])),
            _ => {
                if !items[2].is_kind(Kind::String) {
                    return fail(form, "Too many arguments to def");
                }
                (Some(items[2]), Some(&items[3]))
            }
        };
        // Symbol metadata (`^:dynamic`, `^{:doc ..}`) plus a docstring become var meta.
        let mut meta = rscl_runtime::obj::map::as_map(&sym.meta).map(|m| m.m);
        if let Some(d) = doc {
            let mut m = meta.unwrap_or_else(rscl_runtime::obj::map::empty_map);
            m.insert(rscl_runtime::obj::keyword::intern(None, "doc"), d);
            meta = Some(m);
        }
        // Everything in the metadata but a type hint is an expression, evaluated
        // when the def runs (as in Clojure).
        let meta = meta.map(|m| quote_tag(&rscl_runtime::obj::map::new(m)));
        // Like Clojure, `^:dynamic` takes effect at compile time (a `binding` later in
        // the same form); the rest of the metadata is applied when the def runs, so a
        // throwing init leaves it untouched.
        if let Some(m) = &meta
            && rscl_runtime::obj::map::get(m)
                .m
                .get(&keyword::intern(None, "dynamic"))
                .is_some_and(Value::truthy)
        {
            var_obj.set_flag(FLAG_DYNAMIC, true);
        }
        // `(def f (fn [..] ..))`: the fn is displayed under the var's name. It is not
        // bound to that name inside itself, so a recursive call goes through the var
        // and sees a later redefinition (`(def fib (memoize fib))`).
        self.fn_name_hint = init
            .and_then(seq_items)
            .and_then(|it| {
                it.first()
                    .and_then(symbol::as_sym)
                    .map(|s| s.name().to_string())
            })
            .filter(|n| n == "fn" || n == "fn*")
            .map(|_| sym.name().to_string());
        self.defining.push(VarPtr(var_obj as *const VarObj));
        let init = init.map(|f| self.analyze(f, false));
        self.defining.pop();
        self.fn_name_hint = None;
        let init = match init {
            Some(n) => Some(Box::new(n?)),
            None => None,
        };
        let meta = match meta {
            Some(m) => Some(Box::new(self.analyze(&m, false)?)),
            None => None,
        };
        Ok(Node::Def { var, init, meta })
    }

    fn analyze_let(
        &mut self,
        form: &Value,
        items: &[Value],
        tail: bool,
        is_loop: bool,
    ) -> CResult<Node> {
        let what = if is_loop { "loop" } else { "let" };
        if items.len() < 2 {
            return fail(form, format!("{what} requires a binding vector"));
        }
        let Some(bv) = vector::as_vec(&items[1]) else {
            return fail(form, format!("{what} requires a vector for its binding"));
        };
        if bv.len() % 2 != 0 {
            return fail(
                form,
                format!("{what} requires an even number of forms in binding vector"),
            );
        }
        self.push_scope();
        let mut bindings = Vec::new();
        let mut hinted = Vec::new();
        let pairs: Vec<Value> = bv.iter().cloned().collect();
        for kv in pairs.chunks(2) {
            let Some(sym) = symbol::as_sym(&kv[0]) else {
                self.pop_scope();
                return fail(
                    &kv[0],
                    format!(
                        "Bad binding form, expected symbol, got: {}",
                        rscl_runtime::printer::pr_str(&kv[0])
                    ),
                );
            };
            if sym.ns().is_some() {
                self.pop_scope();
                return fail(&kv[0], "Can't let qualified name");
            }
            let init = match self.analyze(&kv[1], false) {
                Ok(n) => n,
                Err(e) => {
                    self.pop_scope();
                    return Err(e);
                }
            };
            // A hint fixes the type; otherwise a primitive initialiser makes the
            // local primitive (for a loop, provisionally: see below).
            let hint = hint_of(&kv[0]);
            let ty = hint.unwrap_or_else(|| ty_of(&init, &self.locals));
            let id = self.declare_typed(sym.name(), ty);
            hinted.push(hint.is_some());
            bindings.push((id, init));
        }
        let loop_id = self.next_loop;
        if is_loop {
            self.next_loop += 1;
        }
        let try_depth = self.frame().try_depth;
        // A loop local inferred primitive must be rebound with the same primitive by
        // every `recur`; otherwise it is demoted to a value and the body analysed
        // again, since the types of the locals bound inside it may change.
        let body = loop {
            if is_loop {
                self.frame().recur.push(RecurCtx {
                    target: RecurTarget::Loop(loop_id),
                    arity: bindings.len(),
                    used: false,
                    try_depth,
                    arg_tys: Vec::new(),
                });
            }
            let body = self.analyze_body(&items[2..], tail || is_loop);
            let ctx = if is_loop {
                self.frame().recur.pop()
            } else {
                None
            };
            let body = match body {
                Ok(b) => b,
                Err(e) => {
                    self.pop_scope();
                    return Err(e);
                }
            };
            let Some(ctx) = ctx else { break body };
            let mut demoted = false;
            for (i, (id, _)) in bindings.iter().enumerate() {
                let ty = self.locals[*id as usize].ty;
                if ty == Ty::Val || hinted[i] {
                    continue;
                }
                // An inferred local keeps its type only if every `recur` rebinds it
                // with that same type (a long is not widened to a double: that would
                // change the value the untyped loop computes).
                if !ctx.arg_tys.iter().all(|tys| tys[i] == ty) {
                    self.locals[*id as usize].ty = Ty::Val;
                    demoted = true;
                }
            }
            if !demoted {
                break body;
            }
        };
        self.pop_scope();
        Ok(if is_loop {
            Node::Loop {
                id: loop_id,
                bindings,
                body: Box::new(body),
                pos: pos::pos_id(form),
            }
        } else {
            Node::Let {
                bindings,
                body: Box::new(body),
            }
        })
    }

    fn analyze_recur(&mut self, form: &Value, items: &[Value], tail: bool) -> CResult<Node> {
        let Some(ctx) = self.frame().recur.last() else {
            return fail(form, "Can only recur from within loop or fn");
        };
        let (target, arity, try_depth) = (ctx.target, ctx.arity, ctx.try_depth);
        if self.frame().try_depth > try_depth {
            return fail(form, "Cannot recur across try");
        }
        if !tail {
            return fail(form, "Can only recur from tail position");
        }
        if items.len() - 1 != arity {
            return fail(
                form,
                format!(
                    "Mismatched argument count to recur, expected: {arity} args, got: {}",
                    items.len() - 1
                ),
            );
        }
        let args = items[1..]
            .iter()
            .map(|a| self.analyze(a, false))
            .collect::<CResult<Vec<_>>>()?;
        let tys: Vec<Ty> = args.iter().map(|a| ty_of(a, &self.locals)).collect();
        let ctx = self.frame().recur.last_mut().unwrap();
        ctx.used = true;
        ctx.arg_tys.push(tys);
        Ok(Node::Recur { target, args })
    }

    fn analyze_fn(&mut self, form: &Value, items: &[Value]) -> CResult<Node> {
        let hint = self.fn_name_hint.take();
        let mut rest = &items[1..];
        let mut name: Option<String> = None;
        if let Some(sym) = rest.first().and_then(symbol::as_sym) {
            name = Some(sym.name().to_string());
            rest = &rest[1..];
        }
        if rest.is_empty() {
            return fail(form, "fn requires a parameter vector");
        }
        // Normalise to a list of arity forms: ([params] body...) ...
        let arity_forms: Vec<Vec<Value>> = if rest[0].is_kind(Kind::Vector) {
            vec![rest.to_vec()]
        } else {
            let mut out = Vec::new();
            for a in rest {
                let Some(items) = seq_items(a) else {
                    return fail(a, "fn arity must be a list ([params] body...)");
                };
                out.push(items);
            }
            out
        };
        self.fn_counter += 1;
        let display = match name.as_ref().or(hint.as_ref()) {
            Some(n) => format!("{}/{}", self.ns.name, n),
            None => format!("{}/fn__{}", self.ns.name, self.fn_counter),
        };
        self.frames.push(Frame {
            scopes: vec![Vec::new()],
            captures: Vec::new(),
            recur: Vec::new(),
            try_depth: 0,
        });
        let self_local = name.as_deref().map(|n| self.declare_local(n));
        // A macro-generated `fn*` form (`defn`) carries no position: fall back to the
        // first parameter vector or body form that does.
        let pos = std::iter::once(form)
            .chain(arity_forms.iter().flatten())
            .map(pos::pos_id)
            .find(|&p| p != 0)
            .unwrap_or(0);
        let result = self.analyze_arities(form, &arity_forms);
        let frame = self.frames.pop().unwrap();
        let arities = result?;
        Ok(Node::Fn(Rc::new(FnNode {
            name: display,
            arities,
            captures: frame.captures,
            self_local,
            pos,
        })))
    }

    fn analyze_arities(&mut self, form: &Value, arity_forms: &[Vec<Value>]) -> CResult<Vec<Arity>> {
        let mut arities: Vec<Arity> = Vec::new();
        for af in arity_forms {
            let Some(pv) = af.first().and_then(vector::as_vec) else {
                return fail(form, "fn arity requires a parameter vector");
            };
            let params_src: Vec<Value> = pv.iter().cloned().collect();
            self.push_scope();
            let mut params = Vec::new();
            let mut rest = None;
            let mut i = 0;
            while i < params_src.len() {
                let p = &params_src[i];
                let Some(sym) = symbol::as_sym(p) else {
                    self.pop_scope();
                    return fail(
                        p,
                        "destructuring in fn params is not supported yet (params must be symbols)",
                    );
                };
                if sym.name() == "&" {
                    if i + 2 != params_src.len() {
                        self.pop_scope();
                        return fail(
                            p,
                            "Invalid parameter list: & must be followed by exactly one symbol",
                        );
                    }
                    let Some(rs) = symbol::as_sym(&params_src[i + 1]) else {
                        self.pop_scope();
                        return fail(&params_src[i + 1], "rest parameter must be a symbol");
                    };
                    rest = Some(self.declare_local(rs.name()));
                    break;
                }
                if sym.ns().is_some() {
                    self.pop_scope();
                    return fail(p, "Can't use qualified name as parameter");
                }
                let ty = hint_of(p).unwrap_or(Ty::Val);
                params.push(self.declare_typed(sym.name(), ty));
                i += 1;
            }
            if params.len() >= 64 {
                self.pop_scope();
                return fail(form, "Can't specify more than 63 params");
            }
            let n_recur = params.len() + usize::from(rest.is_some());
            let try_depth = self.frame().try_depth;
            self.frame().recur.push(RecurCtx {
                target: RecurTarget::Fn,
                arity: n_recur,
                used: false,
                try_depth,
                arg_tys: Vec::new(),
            });
            let body = self.analyze_body(&af[1..], true);
            let ctx = self.frame().recur.pop().unwrap();
            self.pop_scope();
            let body = body?;
            if rest.is_none()
                && arities
                    .iter()
                    .any(|a| a.rest.is_none() && a.params.len() == params.len())
            {
                return fail(form, "Can't have 2 overloads with same arity");
            }
            if rest.is_some() && arities.iter().any(|a| a.rest.is_some()) {
                return fail(form, "Can't have more than 1 variadic overload");
            }
            arities.push(Arity {
                params,
                rest,
                body,
                has_recur: ctx.used,
            });
        }
        if let Some(va) = arities.iter().find(|a| a.rest.is_some())
            && arities
                .iter()
                .any(|a| a.rest.is_none() && a.params.len() > va.params.len())
        {
            return fail(
                form,
                "Can't have fixed arity function with more params than variadic function",
            );
        }
        Ok(arities)
    }
}

impl Analyzer {
    fn analyze_try(&mut self, items: &[Value]) -> CResult<Node> {
        let is_clause = |v: &Value, name: &str| {
            seq_items(v)
                .and_then(|items| items.first().cloned())
                .and_then(|h| symbol::as_sym(&h).map(|s| s.ns().is_none() && s.name() == name))
                .unwrap_or(false)
        };
        let mut body_forms = Vec::new();
        let mut catches = Vec::new();
        let mut finally = None;
        for (i, it) in items.iter().enumerate().skip(1) {
            if is_clause(it, "catch") {
                let parts = seq_items(it).unwrap_or_default();
                if parts.len() < 3 {
                    return fail(
                        it,
                        "catch requires a class and a binding: (catch Class e body...)",
                    );
                }
                let Some(class) = symbol::as_sym(&parts[1]) else {
                    return fail(&parts[1], "catch class must be a symbol");
                };
                let class = class
                    .name()
                    .rsplit('.')
                    .next()
                    .unwrap_or(class.name())
                    .to_string();
                let Some(binding) = symbol::as_sym(&parts[2]) else {
                    return fail(&parts[2], "catch binding must be a symbol");
                };
                self.frame().try_depth += 1;
                self.push_scope();
                let local = self.declare_local(binding.name());
                let body = self.analyze_body(&parts[3..], false);
                self.pop_scope();
                self.frame().try_depth -= 1;
                catches.push(CatchClause {
                    class,
                    local,
                    body: body?,
                });
            } else if is_clause(it, "finally") {
                if i + 1 != items.len() {
                    return fail(it, "finally clause must be last in try expression");
                }
                let parts = seq_items(it).unwrap_or_default();
                self.frame().try_depth += 1;
                let f = self.analyze_body(&parts[1..], false);
                self.frame().try_depth -= 1;
                finally = Some(Box::new(f?));
            } else {
                if !catches.is_empty() || finally.is_some() {
                    return fail(
                        it,
                        "Only catch or finally clause can follow catch in try expression",
                    );
                }
                body_forms.push(*it);
            }
        }
        self.frame().try_depth += 1;
        let body = self.analyze_body(&body_forms, false);
        self.frame().try_depth -= 1;
        Ok(Node::Try {
            body: Box::new(body?),
            catches,
            finally,
        })
    }

    /// `(native-call* "symbol" :ret-type [:arg-types...] args...)`
    fn analyze_native_call(&mut self, form: &Value, items: &[Value]) -> CResult<Node> {
        if items.len() < 4 {
            return fail(
                form,
                "native-call* requires a symbol name, a return type and an argument type vector",
            );
        }
        let Some(symbol) = rscl_runtime::obj::string::as_str(&items[1]) else {
            return fail(&items[1], "native symbol name must be a string");
        };
        let ret = native_ty(&items[2])?;
        let Some(tv) = vector::as_vec(&items[3]) else {
            return fail(&items[3], "native argument types must be a vector");
        };
        let params = tv.iter().map(native_ty).collect::<CResult<Vec<_>>>()?;
        if params.contains(&NativeTy::Void) {
            return fail(&items[3], "void is not a valid argument type");
        }
        let args = items[4..]
            .iter()
            .map(|a| self.analyze(a, false))
            .collect::<CResult<Vec<_>>>()?;
        if args.len() != params.len() {
            return fail(
                form,
                format!(
                    "native call to {symbol} expects {} arguments, got {}",
                    params.len(),
                    args.len()
                ),
            );
        }
        Ok(Node::NativeCall {
            symbol: symbol.to_string(),
            ret,
            params,
            args,
        })
    }
}

/// A native type written as a keyword or symbol (`:double`, `double`).
pub fn native_ty(v: &Value) -> CResult<NativeTy> {
    let name = match v.kind() {
        Some(Kind::Keyword) => rscl_runtime::obj::keyword::get(v).name().to_string(),
        Some(Kind::Symbol) => symbol::get(v).name().to_string(),
        _ => return fail(v, "native type must be a keyword like :double"),
    };
    NativeTy::parse(&name)
        .map_or_else(|| fail(v, format!("unknown native type: {name} (use :void :long :int :double :float :bool :str :ptr)")), Ok)
}

/// A top-level `(do a b c)` is compiled as separate units so earlier definitions and
/// macros are visible to later sub-forms (Clojure semantics). Nested `do`s inside are
/// flattened too.
pub fn split_do(form: &Value) -> Vec<Value> {
    if let Some(items) = seq_items(form)
        && let Some(head) = items.first()
        && symbol::as_sym(head).is_some_and(|s| s.ns().is_none() && s.name() == "do")
    {
        return items.iter().skip(1).flat_map(split_do).collect();
    }
    vec![*form]
}

/// The elements of a list, cons or lazy seq form (`None` for anything else). Lazy
/// seqs produced by macros are realised here.
pub fn seq_items(v: &Value) -> Option<Vec<Value>> {
    if rscl_runtime::coll::is_seq(v) {
        match rscl_runtime::coll::to_vec(v) {
            Ok(items) => Some(items),
            Err(_) => {
                rscl_runtime::exc::take();
                None
            }
        }
    } else {
        None
    }
}

/// `:tag` in a metadata map is a type hint, kept as written: quote a symbol there so
/// evaluating the map leaves it alone.
fn quote_tag(meta: &Value) -> Value {
    let Some(m) = map::as_map(meta) else {
        return *meta;
    };
    let tag = keyword::intern(None, "tag");
    match m.m.get(&tag) {
        Some(t) if t.is_kind(Kind::Symbol) => {
            let mut out = m.m;
            out.insert(tag, list::from_slice(&[symbol::new(None, "quote"), *t]));
            map::new(out)
        }
        _ => *meta,
    }
}

/// Can `v` be written into compiled code as a constant? Data can (numbers, strings,
/// keywords, symbols, regexes and collections of those); a fn, atom, namespace or
/// other live object cannot, and a macro that returns one gets a positioned error.
fn check_embeddable(form: &Value, v: &Value) -> CResult<()> {
    let ok = match v.kind() {
        None => true,
        Some(
            Kind::Long
            | Kind::Double
            | Kind::BigInt
            | Kind::String
            | Kind::Keyword
            | Kind::Symbol
            | Kind::Regex,
        ) => true,
        Some(Kind::List | Kind::Cons | Kind::LazySeq | Kind::VecSeq | Kind::Vector | Kind::Set) => {
            let Some(items) = seq_items(v).or_else(|| rscl_runtime::coll::to_vec(v).ok()) else {
                return fail(form, "cannot realise a sequence in a constant");
            };
            for x in &items {
                check_embeddable(form, x)?;
            }
            true
        }
        Some(Kind::Map) => {
            for (k, x) in map::get(v).m.iter() {
                check_embeddable(form, k)?;
                check_embeddable(form, x)?;
            }
            true
        }
        Some(_) => false,
    };
    if ok {
        Ok(())
    } else {
        fail(
            form,
            format!(
                "a {} value cannot be embedded in compiled code; return a form that constructs it (a macro expansion may not contain one)",
                rscl_runtime::core::type_name(v)
            ),
        )
    }
}

/// Self-evaluating form that needs no code to construct.
pub fn is_constant(v: &Value) -> bool {
    // Metadata is part of a literal: a non-constant one makes the literal a runtime
    // construction.
    if matches!(v.kind(), Some(Kind::Vector | Kind::Map | Kind::Set))
        && rscl_runtime::obj::meta(v).is_some_and(|m| !m.is_nil() && !is_constant(&m))
    {
        return false;
    }
    match v.kind() {
        None => true,
        Some(Kind::Long | Kind::Double | Kind::BigInt | Kind::String | Kind::Keyword) => true,
        Some(Kind::Vector) => vector::get(v).iter().all(is_constant),
        Some(Kind::Map) => map::get(v)
            .m
            .iter()
            .all(|(k, val)| is_constant(k) && is_constant(val)),
        Some(Kind::Set) => rscl_runtime::obj::set::get(v).s.iter().all(is_constant),
        _ => false,
    }
}
