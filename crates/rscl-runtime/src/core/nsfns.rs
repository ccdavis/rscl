//! Namespace builtins: `in-ns`, `require`, `load-file`, `ns-name`, ...
use crate::builtins;
use crate::coll;
use crate::core::args;
use crate::exc::{RtResult, err};
use crate::ns::{self, NsObj};
use crate::obj::exception::{ILLEGAL_ARGUMENT, RUNTIME};
use crate::obj::{Kind, keyword, list, map, symbol, var, vector};
use crate::value::Value;

fn expect_ns_name(v: &Value) -> RtResult<String> {
    match v.kind() {
        Some(Kind::Symbol) => Ok(symbol::get(v).qualified()),
        Some(Kind::String) => Ok(crate::obj::string::get(v).to_string()),
        Some(Kind::Namespace) => Ok(ns::get(v).name.to_string()),
        _ => err(
            ILLEGAL_ARGUMENT,
            format!(
                "expected a namespace name, got {}",
                crate::core::type_name(v)
            ),
        ),
    }
}

fn the_ns(v: &Value) -> RtResult<&'static NsObj> {
    let name = expect_ns_name(v)?;
    ns::find(&name).map_or_else(|| err(RUNTIME, format!("No namespace: {name} found")), Ok)
}

/// `(require '[ns :as alias :refer [a b]] 'other :reload)`
fn require_spec(spec: &Value, force: bool) -> RtResult<()> {
    if keyword::as_kw(spec).is_some() {
        return Ok(()); // flags (:reload, :verbose, ...) are handled by the caller
    }
    let (lib, opts): (String, Vec<Value>) = match spec.kind() {
        Some(Kind::Symbol) => (symbol::get(spec).qualified(), Vec::new()),
        Some(Kind::Vector) => {
            let items = coll::to_vec(spec)?;
            let Some(first) = items.first().filter(|f| f.is_kind(Kind::Symbol)) else {
                return err(
                    ILLEGAL_ARGUMENT,
                    "require spec vector must start with a namespace symbol",
                );
            };
            (symbol::get(first).qualified(), items[1..].to_vec())
        }
        _ => {
            return err(
                ILLEGAL_ARGUMENT,
                format!("bad require spec: {}", crate::printer::pr_str(spec)),
            );
        }
    };
    ns::load_lib_opt(&lib, force)?;
    let Some(target) = ns::find(&lib) else {
        return err(
            RUNTIME,
            format!(
                "namespace '{lib}' not found after loading {}",
                ns::ns_to_relative_path(&lib)
            ),
        );
    };
    let cur = ns::current();
    let mut i = 0;
    while i < opts.len() {
        let Some(k) = keyword::as_kw(&opts[i]) else {
            return err(
                ILLEGAL_ARGUMENT,
                format!("bad require option: {}", crate::printer::pr_str(&opts[i])),
            );
        };
        let arg = opts.get(i + 1);
        match k.name() {
            "as" | "as-alias" => {
                let Some(a) = arg.and_then(|a| symbol::as_sym(a)) else {
                    return err(ILLEGAL_ARGUMENT, ":as requires a symbol");
                };
                cur.alias(&a.qualified(), target);
            }
            "refer" => {
                let Some(a) = arg else {
                    return err(ILLEGAL_ARGUMENT, ":refer requires a vector or :all");
                };
                if keyword::as_kw(a).is_some_and(|kw| kw.name() == "all") {
                    for v in target.own_vars() {
                        cur.refer(&v.name, v);
                    }
                } else {
                    for s in coll::to_vec(a)? {
                        let Some(sym) = symbol::as_sym(&s) else {
                            return err(ILLEGAL_ARGUMENT, ":refer names must be symbols");
                        };
                        match target.lookup(sym.name()) {
                            Some(v) => cur.refer(sym.name(), v),
                            None => {
                                return err(
                                    RUNTIME,
                                    format!("{} does not exist in namespace {lib}", sym.name()),
                                );
                            }
                        }
                    }
                }
            }
            "exclude" | "rename" | "only" | "reload" | "reload-all" | "verbose" => {}
            other => {
                return err(
                    ILLEGAL_ARGUMENT,
                    format!("unsupported require option :{other}"),
                );
            }
        }
        i += 2;
    }
    Ok(())
}

builtins! {
    "in-ns" => fn cf_in_ns(args) [1] {
        let name = expect_ns_name(&args[0])?;
        let n = ns::find_or_create(&name);
        ns::set_current(n);
        Ok(n.as_value())
    }
    "require" => @gc fn cf_require(args) [; 0] {
        let force = args.iter().any(|a| keyword::as_kw(a).is_some_and(|k| matches!(k.name(), "reload" | "reload-all")));
        for spec in args {
            if let Some(items) = list::as_list(spec).map(|l| l.to_vec()) && items.first().is_some_and(|f| f.is_kind(Kind::Symbol)) {
                // Prefix list: (prefix lib1 lib2) → prefix.lib1, prefix.lib2
                let prefix = symbol::get(&items[0]).qualified();
                for sub in &items[1..] {
                    let full = match sub.kind() {
                        Some(Kind::Symbol) => symbol::new(None, &format!("{prefix}.{}", symbol::get(sub).qualified())),
                        Some(Kind::Vector) => {
                            let mut v = coll::to_vec(sub)?;
                            if let Some(f) = v.first_mut() { *f = symbol::new(None, &format!("{prefix}.{}", symbol::get(f).qualified())); }
                            vector::from_iter(v)
                        }
                        _ => return err(ILLEGAL_ARGUMENT, "bad prefix list in require"),
                    };
                    require_spec(&full, force)?;
                }
                continue;
            }
            require_spec(spec, force)?;
        }
        Ok(Value::NIL)
    }
    "use" => fn cf_use(args) [; 0] {
        for spec in args {
            let spec = match spec.kind() {
                Some(Kind::Symbol) => vector::from_slice(&[*spec, keyword::intern(None, "refer"), keyword::intern(None, "all")]),
                _ => *spec,
            };
            require_spec(&spec, false)?;
        }
        Ok(Value::NIL)
    }
    "load-file" => @gc fn cf_load_file(args) [1] {
        // Always re-evaluates, like Clojure (the edit-then-load-file REPL workflow).
        let path = args::string(&args[0], "path")?;
        ns::load_lib_opt(&format!("file:{path}"), true)?;
        Ok(Value::NIL)
    }
    "loaded-libs" => fn cf_loaded_libs(args) [0] {
        let _ = args;
        let mut s = crate::obj::set::empty_set();
        for n in ns::loaded_libs() {
            s.insert(symbol::new(None, &n));
        }
        Ok(crate::obj::set::new(s))
    }
    "ns-name" => fn cf_ns_name(args) [1] { Ok(symbol::new(None, &the_ns(&args[0])?.name)) }
    "find-ns" => fn cf_find_ns(args) [1] {
        let name = expect_ns_name(&args[0])?;
        Ok(ns::find(&name).map_or(Value::NIL, |n| n.as_value()))
    }
    "the-ns" => fn cf_the_ns(args) [1] { Ok(the_ns(&args[0])?.as_value()) }
    "create-ns" => fn cf_create_ns(args) [1] { Ok(ns::find_or_create(&expect_ns_name(&args[0])?).as_value()) }
    "all-ns" => fn cf_all_ns(args) [0] {
        let _ = args;
        let mut all = ns::all();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list::from_iter(all.iter().map(|n| n.as_value()).collect::<Vec<_>>()))
    }
    "ns-publics" => fn cf_ns_publics(args) [1] {
        let n = the_ns(&args[0])?;
        let mut m = map::empty_map();
        for v in n.own_vars() {
            if !v.has_flag(var::FLAG_PRIVATE) { m.insert(symbol::new(None, &v.name), v.as_value()); }
        }
        Ok(map::new(m))
    }
    "ns-interns" => fn cf_ns_interns(args) [1] {
        let n = the_ns(&args[0])?;
        let mut m = map::empty_map();
        for v in n.own_vars() { m.insert(symbol::new(None, &v.name), v.as_value()); }
        Ok(map::new(m))
    }
    "ns-aliases" => fn cf_ns_aliases(args) [1] {
        let n = the_ns(&args[0])?;
        let mut m = map::empty_map();
        for (a, t) in n.aliases() { m.insert(symbol::new(None, &a), t.as_value()); }
        Ok(map::new(m))
    }
    "alias" => fn cf_alias(args) [2] {
        let Some(a) = symbol::as_sym(&args[0]) else { return err(ILLEGAL_ARGUMENT, "alias requires a symbol") };
        ns::current().alias(&a.qualified(), the_ns(&args[1])?);
        Ok(Value::NIL)
    }
    "refer" => fn cf_refer(args) [; 1] {
        let target = the_ns(&args[0])?;
        let mut spec = vec![symbol::new(None, &target.name)];
        if args.len() == 1 { spec.push(keyword::intern(None, "refer")); spec.push(keyword::intern(None, "all")); }
        spec.extend(args[1..].iter().copied());
        require_spec(&vector::from_iter(spec), false).map(|_| Value::NIL)
    }
    "ns-resolve" => fn cf_ns_resolve(args) [2] {
        let n = the_ns(&args[0])?;
        let Some(s) = symbol::as_sym(&args[1]) else { return err(ILLEGAL_ARGUMENT, "ns-resolve requires a symbol") };
        Ok(ns::resolve(n, s.ns(), s.name()).map_or(Value::NIL, |v| v.as_value()))
    }
    "ns?" => fn cf_ns_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Namespace))) }
}
