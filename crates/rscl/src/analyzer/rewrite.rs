//! Built-in rewrite macros. Each turns a form into another form and is used only
//! until `core.clj` defines the real macro (Phase 4), after which the entry here is
//! deleted. They are consulted only when the head symbol is not a local.
use rscl_runtime::obj::{Kind, keyword, list, symbol, vector};
use rscl_runtime::{Value, printer};

use super::Analyzer;
use crate::errors::{CResult, fail};

fn sym(name: &str) -> Value {
    symbol::new(None, name)
}

fn lst(items: &[Value]) -> Value {
    list::from_slice(items)
}

/// Expand `form` whose head is the unqualified symbol `name`; `None` if `name` is not
/// a built-in rewrite.
pub fn expand(
    an: &mut Analyzer,
    name: &str,
    form: &Value,
    items: &[Value],
) -> CResult<Option<Value>> {
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
        "when" => {
            if args.is_empty() {
                return fail(form, "when requires a test");
            }
            let mut body = vec![sym("do")];
            body.extend_from_slice(&args[1..]);
            lst(&[sym("if"), args[0].clone(), lst(&body)])
        }
        "when-not" => {
            if args.is_empty() {
                return fail(form, "when-not requires a test");
            }
            let mut body = vec![sym("do")];
            body.extend_from_slice(&args[1..]);
            lst(&[sym("if"), args[0].clone(), Value::NIL, lst(&body)])
        }
        "if-not" => {
            if args.len() < 2 || args.len() > 3 {
                return fail(form, "if-not takes 2 or 3 args");
            }
            let else_ = args.get(2).cloned().unwrap_or(Value::NIL);
            lst(&[sym("if"), args[0].clone(), else_, args[1].clone()])
        }
        "cond" => {
            if args.is_empty() {
                return Ok(Some(Value::NIL));
            }
            if !args.len().is_multiple_of(2) {
                return fail(form, "cond requires an even number of forms");
            }
            let mut rest = vec![sym("cond")];
            rest.extend_from_slice(&args[2..]);
            lst(&[sym("if"), args[0].clone(), args[1].clone(), lst(&rest)])
        }
        "and" => match args.len() {
            0 => Value::TRUE,
            1 => args[0].clone(),
            _ => {
                let t = an.gensym("and");
                let mut rest = vec![sym("and")];
                rest.extend_from_slice(&args[1..]);
                lst(&[
                    sym("let*"),
                    vector::from_slice(&[t.clone(), args[0].clone()]),
                    lst(&[sym("if"), t.clone(), lst(&rest), t]),
                ])
            }
        },
        "or" => match args.len() {
            0 => Value::NIL,
            1 => args[0].clone(),
            _ => {
                let t = an.gensym("or");
                let mut rest = vec![sym("or")];
                rest.extend_from_slice(&args[1..]);
                lst(&[
                    sym("let*"),
                    vector::from_slice(&[t.clone(), args[0].clone()]),
                    lst(&[sym("if"), t.clone(), t, lst(&rest)]),
                ])
            }
        },
        "->" | "->>" => {
            if args.is_empty() {
                return fail(form, format!("{name} requires at least one form"));
            }
            let mut acc = args[0].clone();
            for step in &args[1..] {
                acc = match list::as_list(step) {
                    Some(l) if !l.is_empty() => {
                        let mut v = l.to_vec();
                        if name == "->" {
                            v.insert(1, acc)
                        } else {
                            v.push(acc)
                        }
                        lst(&v)
                    }
                    _ => lst(&[step.clone(), acc]),
                };
            }
            acc
        }
        "comment" => Value::NIL,
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
    let mut f = vec![sym("fn*"), name.clone()];
    f.extend_from_slice(rest);
    Ok(lst(&[sym("def"), name.clone(), lst(&f)]))
}
