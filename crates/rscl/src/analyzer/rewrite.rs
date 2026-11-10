//! Built-in rewrite macros: the bootstrap primitives `defmacro`, `defn`, `fn`, `let`,
//! `loop` (no destructuring) and `defnative`. `core.clj` redefines the first five as
//! real macros with destructuring, which then take precedence because macro vars are
//! checked before these rewrites. Consulted only when the head symbol is not a local.
use rscl_runtime::obj::{Kind, keyword, list, symbol, vector};
use rscl_runtime::{Value, printer};

use crate::errors::{CResult, fail};

fn sym(name: &str) -> Value {
    symbol::new(None, name)
}

fn lst(items: &[Value]) -> Value {
    list::from_slice(items)
}

/// Expand `form` whose head is the unqualified symbol `name`; `None` if `name` is not
/// a built-in rewrite.
pub fn expand(name: &str, form: &Value, items: &[Value]) -> CResult<Option<Value>> {
    let args = &items[1..];
    let out = match name {
        "defn" => defn(form, args)?,
        "fn" => {
            let mut v = vec![sym("fn*")];
            v.extend_from_slice(args);
            lst(&v)
        }
        "let" => {
            let mut v = vec![sym("let*")];
            v.extend_from_slice(args);
            lst(&v)
        }
        "loop" => {
            let mut v = vec![sym("loop*")];
            v.extend_from_slice(args);
            lst(&v)
        }
        "defmacro" => defmacro(form, args)?,
        "defnative" => defnative(form, args)?,
        _ => return Ok(None),
    };
    Ok(Some(out))
}

/// `(defnative name :ret [^type param ...] & {:symbol "cname"})` →
/// `(def name (fn* name [param ...] (native-call* "cname" :ret [:type ...] param ...)))`
fn defnative(form: &Value, args: &[Value]) -> CResult<Value> {
    let Some(name) = args.first().filter(|a| a.is_kind(Kind::Symbol)) else {
        return fail(form, "First argument to defnative must be a symbol");
    };
    let Some(ret) = args.get(1) else {
        return fail(form, "defnative requires a return type, e.g. :double");
    };
    let Some(params) = args.get(2).and_then(vector::as_vec) else {
        return fail(
            form,
            "defnative requires a parameter vector, e.g. [^double x]",
        );
    };
    let mut cname = symbol::get(name).name().to_string();
    let mut i = 3;
    while i < args.len() {
        let is_symbol_opt =
            keyword::as_kw(&args[i]).is_some_and(|k| k.ns().is_none() && k.name() == "symbol");
        if is_symbol_opt && i + 1 < args.len() {
            cname = rscl_runtime::obj::string::as_str(&args[i + 1])
                .map(str::to_string)
                .ok_or_else(|| {
                    crate::errors::CompileError::at(&args[i + 1], ":symbol option must be a string")
                })?;
            i += 2;
        } else {
            return fail(
                &args[i],
                "unknown defnative option (expected :symbol \"cname\")",
            );
        }
    }
    let mut param_syms = Vec::new();
    let mut types = Vec::new();
    for p in params.v.iter() {
        let Some(sym) = symbol::as_sym(p) else {
            return fail(p, "defnative parameters must be symbols with a ^type tag");
        };
        let tag = rscl_runtime::obj::map::as_map(&sym.meta).and_then(|m| {
            m.m.get(&keyword::intern(None, "tag")).cloned().or_else(|| {
                m.m.iter()
                    .find(|(_, v)| v.is_true())
                    .map(|(k, _)| k.clone())
            })
        });
        let Some(tag) = tag else {
            return fail(
                p,
                format!(
                    "parameter {} needs a type tag such as ^double or ^:long",
                    sym.name()
                ),
            );
        };
        let tag_name = match tag.kind() {
            Some(Kind::Symbol) => symbol::get(&tag).name().to_string(),
            Some(Kind::Keyword) => keyword::get(&tag).name().to_string(),
            _ => return fail(p, "type tag must be a symbol or keyword"),
        };
        types.push(keyword::intern(None, &tag_name));
        param_syms.push(symbol::new(None, sym.name()));
    }
    let mut call = vec![
        sym("native-call*"),
        rscl_runtime::obj::string::new(&cname),
        ret.clone(),
        vector::from_slice(&types),
    ];
    call.extend(param_syms.iter().cloned());
    let f = lst(&[
        sym("fn*"),
        name.clone(),
        vector::from_slice(&param_syms),
        lst(&call),
    ]);
    Ok(lst(&[sym("def"), name.clone(), f]))
}

/// `(defmacro name doc? attr? [params] body...)` or multi-arity →
/// `(do (def name (fn* ([&form &env params...] body...)...)) (set-macro! (var name)))`
fn defmacro(form: &Value, args: &[Value]) -> CResult<Value> {
    let Some(name) = args.first().filter(|a| a.is_kind(Kind::Symbol)) else {
        return fail(form, "First argument to defmacro must be a symbol");
    };
    let mut rest = &args[1..];
    if rest.first().is_some_and(|f| f.is_kind(Kind::String)) {
        rest = &rest[1..];
    }
    if rest.first().is_some_and(|f| f.is_kind(Kind::Map)) {
        rest = &rest[1..];
    }
    if rest.is_empty() {
        return fail(form, "defmacro requires a parameter vector");
    }
    let arities: Vec<Vec<Value>> = if rest[0].is_kind(Kind::Vector) {
        vec![rest.to_vec()]
    } else {
        let mut out = Vec::new();
        for a in rest {
            let Some(items) = super::seq_items(a) else {
                return fail(a, "defmacro arity must be a list ([params] body...)");
            };
            out.push(items);
        }
        out
    };
    // Unnamed: recursion goes through the var (the analyzer still displays the fn
    // under the var's name).
    let mut f = vec![sym("fn*")];
    for a in &arities {
        let Some(params) = vector::as_vec(&a[0]) else {
            return fail(&a[0], "defmacro requires a parameter vector");
        };
        let mut ps = vec![sym("&form"), sym("&env")];
        ps.extend(params.v.iter().cloned());
        let mut arity = vec![vector::from_slice(&ps)];
        arity.extend_from_slice(&a[1..]);
        f.push(lst(&arity));
    }
    Ok(lst(&[
        sym("do"),
        lst(&[sym("def"), name.clone(), lst(&f)]),
        lst(&[
            symbol::new(Some("clojure.core"), "set-macro!"),
            lst(&[sym("var"), name.clone()]),
        ]),
    ]))
}

fn defn(form: &Value, args: &[Value]) -> CResult<Value> {
    let Some(name) = args.first().filter(|a| a.is_kind(Kind::Symbol)) else {
        return fail(form, "First argument to defn must be a symbol");
    };
    let mut rest = &args[1..];
    // Optional docstring and attribute map.
    if rest.first().is_some_and(|f| f.is_kind(Kind::String)) {
        rest = &rest[1..];
    }
    if rest.first().is_some_and(|f| f.is_kind(Kind::Map)) {
        rest = &rest[1..];
    }
    if rest.is_empty() {
        return fail(
            form,
            format!("defn {} requires a parameter vector", printer::pr_str(name)),
        );
    }
    // Unnamed, like the `defn` of core.clj: recursion goes through the var.
    let mut f = vec![sym("fn*")];
    f.extend_from_slice(rest);
    Ok(lst(&[sym("def"), name.clone(), lst(&f)]))
}
