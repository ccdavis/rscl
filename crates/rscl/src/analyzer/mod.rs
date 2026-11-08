//! Forms (runtime `Value`s) → `Node` AST.
//!
//! Handles special forms, symbol resolution (locals, captures, vars), built-in
//! rewrite macros (`defn`, `let`, `when`, ... until `core.clj` defines the real ones),
//! closure conversion (free variables become captures) and `recur` validation.
use std::rc::Rc;

use rscl_runtime::obj::var::VarObj;
use rscl_runtime::obj::{Kind, list, map, symbol, vector};
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
    Def {
        var: VarPtr,
        init: Option<Box<Node>>,
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
    /// A vector/map/set literal with non-constant parts (maps: alternating k, v).
    Coll {
        kind: CollKind,
        items: Vec<Node>,
    },
    Throw(Box<Node>),
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
}

impl NativeTy {
    pub fn parse(name: &str) -> Option<NativeTy> {
        Some(match name {
            "void" => NativeTy::Void,
            "long" | "i64" | "u64" | "size_t" => NativeTy::Long,
            "int" | "i32" | "u32" => NativeTy::Int,
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

#[derive(Debug, Clone)]
pub struct LocalInfo {
    pub name: String,
}

struct RecurCtx {
    target: RecurTarget,
    arity: usize,
    used: bool,
    /// `try` nesting depth when the target was entered; `recur` may not cross a `try`.
    try_depth: usize,
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
    next_sym: u32,
    fn_counter: u32,
}

const SPECIAL_FORMS: &[&str] = &[
    "def",
    "if",
    "do",
    "let*",
    "loop*",
    "recur",
    "fn*",
    "quote",
    "var",
    "throw",
    "try",
    "set!",
    "letfn*",
    "case*",
    "native-call*",
];

pub fn is_special(name: &str) -> bool {
    SPECIAL_FORMS.contains(&name)
}

impl Analyzer {
    pub fn new(ns: &'static ns::NsObj) -> Self {
        Analyzer {
            ns,
            locals: Vec::new(),
            frames: Vec::new(),
            next_loop: 0,
            next_sym: 0,
            fn_counter: 0,
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
        let id = self.locals.len() as LocalId;
        self.locals.push(LocalInfo {
            name: name.to_string(),
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

    pub fn gensym(&mut self, prefix: &str) -> Value {
        self.next_sym += 1;
        symbol::new(None, &format!("{prefix}__{}__auto__", self.next_sym))
    }

    // ---- dispatch ----------------------------------------------------------------

    pub fn analyze(&mut self, form: &Value, tail: bool) -> CResult<Node> {
        match form.kind() {
            Some(Kind::Symbol) => self.analyze_symbol(form),
            Some(Kind::List) => self.analyze_list(form, tail),
            Some(Kind::Vector) => {
                let items = vector::get(form).v.iter().cloned().collect::<Vec<_>>();
                self.analyze_coll(form, CollKind::Vector, &items)
            }
            Some(Kind::Map) => {
                let mut items = Vec::new();
                for (k, v) in map::get(form).m.iter() {
                    items.push(k.clone());
                    items.push(v.clone());
                }
                self.analyze_coll(form, CollKind::Map, &items)
            }
            Some(Kind::Set) => {
                let items = rscl_runtime::obj::set::get(form)
                    .s
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                self.analyze_coll(form, CollKind::Set, &items)
            }
            _ => Ok(Node::Const(form.clone())),
        }
    }

    fn analyze_coll(&mut self, form: &Value, kind: CollKind, items: &[Value]) -> CResult<Node> {
        if items.iter().all(is_constant) {
            return Ok(Node::Const(form.clone()));
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
        let l = list::get(form);
        if l.is_empty() {
            return Ok(Node::Const(form.clone()));
        }
        let items = l.to_vec();
        let head = &items[0];
        if let Some(sym) = symbol::as_sym(head) {
            if sym.ns().is_none() && is_special(sym.name()) {
                return self.analyze_special(sym.name(), form, &items, tail);
            }
            if sym.ns().is_none()
                && !self.is_local(sym.name())
                && let Some(expanded) = rewrite::expand(self, sym.name(), form, &items)?
            {
                // A rewrite may hand back one of the original sub-forms (`(or x)` → x);
                // only a freshly built form takes the position of the form it replaces.
                if pos::pos_id(&expanded) == 0
                    && expanded.header().is_some_and(|h| !h.is_immortal())
                {
                    pos::set_pos(&expanded, pos::pos_id(form));
                }
                return self.analyze(&expanded, tail);
            }
        }
        let f = self.analyze(head, false)?;
        let args = items[1..]
            .iter()
            .map(|a| self.analyze(a, false))
            .collect::<CResult<Vec<_>>>()?;
        Ok(Node::Call {
            f: Box::new(f),
            args,
            pos: pos::pos_id(form),
        })
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
                Ok(Node::Const(items[1].clone()))
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
                Ok(Node::Throw(Box::new(self.analyze(&items[1], false)?)))
            }
            "try" => self.analyze_try(items),
            "native-call*" => self.analyze_native_call(form, items),
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
        let var = VarPtr(self.ns.intern(sym.name()) as *const VarObj);
        let init = match items.len() {
            2 => None,
            3 => Some(&items[2]),
            _ => {
                if !items[2].is_kind(Kind::String) {
                    return fail(form, "Too many arguments to def");
                }
                Some(&items[3])
            }
        };
        let init = match init {
            Some(f) => Some(Box::new(self.analyze(f, false)?)),
            None => None,
        };
        Ok(Node::Def { var, init })
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
        if bv.v.len() % 2 != 0 {
            return fail(
                form,
                format!("{what} requires an even number of forms in binding vector"),
            );
        }
        self.push_scope();
        let mut bindings = Vec::new();
        let pairs: Vec<Value> = bv.v.iter().cloned().collect();
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
            let id = self.declare_local(sym.name());
            bindings.push((id, init));
        }
        let loop_id = self.next_loop;
        if is_loop {
            self.next_loop += 1;
            let try_depth = self.frame().try_depth;
            self.frame().recur.push(RecurCtx {
                target: RecurTarget::Loop(loop_id),
                arity: bindings.len(),
                used: false,
                try_depth,
            });
        }
        let body = self.analyze_body(&items[2..], tail || is_loop);
        if is_loop {
            self.frame().recur.pop();
        }
        self.pop_scope();
        let body = body?;
        Ok(if is_loop {
            Node::Loop {
                id: loop_id,
                bindings,
                body: Box::new(body),
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
        self.frame().recur.last_mut().unwrap().used = true;
        Ok(Node::Recur { target, args })
    }

    fn analyze_fn(&mut self, form: &Value, items: &[Value]) -> CResult<Node> {
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
                let Some(l) = list::as_list(a) else {
                    return fail(a, "fn arity must be a list ([params] body...)");
                };
                out.push(l.to_vec());
            }
            out
        };
        self.fn_counter += 1;
        let display = match &name {
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
        let result = self.analyze_arities(form, &arity_forms);
        let frame = self.frames.pop().unwrap();
        let arities = result?;
        Ok(Node::Fn(Rc::new(FnNode {
            name: display,
            arities,
            captures: frame.captures,
            self_local,
        })))
    }

    fn analyze_arities(&mut self, form: &Value, arity_forms: &[Vec<Value>]) -> CResult<Vec<Arity>> {
        let mut arities: Vec<Arity> = Vec::new();
        for af in arity_forms {
            let Some(pv) = af.first().and_then(vector::as_vec) else {
                return fail(form, "fn arity requires a parameter vector");
            };
            let params_src: Vec<Value> = pv.v.iter().cloned().collect();
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
                params.push(self.declare_local(sym.name()));
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
            list::as_list(v)
                .and_then(|l| l.iter().next().cloned())
                .and_then(|h| symbol::as_sym(&h).map(|s| s.ns().is_none() && s.name() == name))
                .unwrap_or(false)
        };
        let mut body_forms = Vec::new();
        let mut catches = Vec::new();
        let mut finally = None;
        for (i, it) in items.iter().enumerate().skip(1) {
            if is_clause(it, "catch") {
                let parts = list::get(it).to_vec();
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
                let parts = list::get(it).to_vec();
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
                body_forms.push(it.clone());
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
        let params = tv.v.iter().map(native_ty).collect::<CResult<Vec<_>>>()?;
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
    if let Some(l) = list::as_list(form)
        && let Some(head) = l.iter().next()
        && symbol::as_sym(head).is_some_and(|s| s.ns().is_none() && s.name() == "do")
    {
        return l.iter().skip(1).flat_map(split_do).collect();
    }
    vec![form.clone()]
}

/// Self-evaluating form that needs no code to construct.
pub fn is_constant(v: &Value) -> bool {
    match v.kind() {
        None => true,
        Some(Kind::Long | Kind::Double | Kind::String | Kind::Keyword) => true,
        Some(Kind::Vector) => vector::get(v).v.iter().all(is_constant),
        Some(Kind::Map) => map::get(v)
            .m
            .iter()
            .all(|(k, val)| is_constant(k) && is_constant(val)),
        Some(Kind::Set) => rscl_runtime::obj::set::get(v).s.iter().all(is_constant),
        _ => false,
    }
}
