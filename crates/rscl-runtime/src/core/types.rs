//! Types, protocols, multimethods, dynamic bindings and sorted collections.
//!
//! The macros in `core.clj` (`deftype`, `defrecord`, `defprotocol`, `extend-type`,
//! `defmulti`, `binding`, ...) expand into calls to the starred builtins here.
use crate::coll;
use crate::core::args;
use crate::exc::{RtResult, err};
use crate::obj::exception::{ILLEGAL_ARGUMENT, ILLEGAL_STATE};
use crate::obj::func;
use crate::obj::types::{self, KEY_NIL, KEY_OBJECT};
use crate::obj::{self, Kind, atom, keyword, list, map, sorted, string, symbol, var, vector};
use crate::value::{VRef, Value};
use crate::{builtins, native_fns};

/// A native closure with its own immortal prototype, so each protocol fn and
/// multimethod prints and traces under its own name.
fn closure(entry: func::Entry, name: &str, vmin: i64, captures: Vec<Value>) -> Value {
    let p = func::new_proto(entry, name, 0, vmin);
    let f = func::new(p, captures.len());
    for (i, c) in captures.into_iter().enumerate() {
        unsafe { func::set_capture(&f, i, c) }
    }
    f
}

fn cap(this: VRef, i: usize) -> Value {
    func::capture(&this, i).clone()
}

/// The name of a symbol, keyword or string argument.
fn named(v: &Value, what: &str) -> RtResult<String> {
    if let Some(s) = symbol::as_sym(v) {
        return Ok(s.name().to_string());
    }
    if let Some(k) = keyword::as_kw(v) {
        return Ok(k.name().to_string());
    }
    if let Some(s) = string::as_str(v) {
        return Ok(s.to_string());
    }
    err(
        ILLEGAL_ARGUMENT,
        format!("{what} must be a symbol, keyword or string"),
    )
}

fn names(v: &Value, what: &str) -> RtResult<Vec<String>> {
    coll::to_vec(v)?.iter().map(|x| named(x, what)).collect()
}

fn expect_type<'a>(v: &'a Value, what: &str) -> RtResult<&'a types::TypeObj> {
    types::as_type(v).ok_or_else(|| {
        crate::exc::throw_new(
            ILLEGAL_ARGUMENT,
            format!("{what} must be a type, got {}", crate::core::type_name(v)),
        )
    })
}

fn expect_protocol<'a>(v: &'a Value, what: &str) -> RtResult<&'a types::ProtocolObj> {
    types::as_protocol(v).ok_or_else(|| {
        crate::exc::throw_new(
            ILLEGAL_ARGUMENT,
            format!(
                "{what} must be a protocol, got {}",
                crate::core::type_name(v)
            ),
        )
    })
}

/// Dispatch key of a type argument; `nil` names the type of `nil`.
fn type_arg_key(v: &Value) -> RtResult<u64> {
    if v.is_nil() {
        return Ok(KEY_NIL);
    }
    Ok(expect_type(v, "type")?.id)
}

fn expect_var<'a>(v: &'a Value, what: &str) -> RtResult<&'a var::VarObj> {
    var::as_var(v).ok_or_else(|| {
        crate::exc::throw_new(
            ILLEGAL_ARGUMENT,
            format!("{what} must be a var, got {}", crate::core::type_name(v)),
        )
    })
}

native_fns! {
    // captures: [protocol, method-name]
    "protocol-fn" => @gc fn nf_protocol_fn(args, this) [; 0] {
        if args.is_empty() {
            let name = func::get(&this).proto().name.clone();
            return Err(crate::exc::Exc_from(crate::core::arity_error(&name, 0)));
        }
        let p = cap(this, 0);
        let method = cap(this, 1);
        let proto = types::proto(&p);
        let m = string::get(&method);
        let target = &args[0];
        let Some(f) = proto.find(types::type_key(target), m) else {
            return err(
                ILLEGAL_ARGUMENT,
                format!(
                    "No implementation of method: :{m} of protocol: #'{} found for class: {}",
                    proto.name,
                    crate::core::type_name(target)
                ),
            );
        };
        let owned: Vec<Value> = args.iter().map(|a| a.owned()).collect();
        f.call(&owned)
    }
    // captures: [name, dispatch-fn, methods-atom, default-dispatch-value]
    "multi-fn" => @gc fn nf_multi_fn(args, this) [; 0] {
        let owned: Vec<Value> = args.iter().map(|a| a.owned()).collect();
        let dv = cap(this, 1).call(&owned)?;
        let table = atom::get(&cap(this, 2)).deref();
        let mut f = coll::get(&table, &dv, Some(&Value::UNBOUND))?;
        if f.is_unbound() {
            f = coll::get(&table, &cap(this, 3), Some(&Value::UNBOUND))?;
        }
        if f.is_unbound() {
            return err(
                ILLEGAL_ARGUMENT,
                format!(
                    "No method in multimethod '{}' for dispatch value: {}",
                    crate::printer::print_str(&cap(this, 0)),
                    crate::printer::pr_str(&dv)
                ),
            );
        }
        f.call(&owned)
    }
}

fn is_multi(v: &Value) -> bool {
    func::as_fn(v)
        .is_some_and(|f| std::ptr::fn_addr_eq(f.proto().entry, nf_multi_fn as func::Entry))
}

/// `(name, method-table atom, default dispatch value)` of a multimethod.
fn multi_parts(v: &Value) -> RtResult<(Value, Value, Value)> {
    if !is_multi(v) {
        return err(
            ILLEGAL_ARGUMENT,
            format!("{} is not a multimethod", crate::core::type_name(v)),
        );
    }
    Ok((
        func::capture(v, 0).clone(),
        func::capture(v, 2).clone(),
        func::capture(v, 3).clone(),
    ))
}

builtins! {
    // ---- types and records -----------------------------------------------------------
    "create-type*" => fn ct_create_type(args) [3] {
        let name = named(&args[0], "type name")?;
        let fields = names(&args[1], "field")?;
        Ok(types::new_type(&name, fields, args[2].truthy()))
    }
    "instance*" => fn ct_instance(args) [; 1] {
        let fields: Vec<Value> = args[1..].iter().map(|a| a.owned()).collect();
        types::instance(&args[0], fields, Value::NIL)
    }
    "map->instance*" => fn ct_map_instance(args) [2] {
        let t = expect_type(&args[0], "type")?;
        let m = &args[1];
        let entries = coll::map_entries(m, &format!("map->{} argument", t.name))?;
        let mut fields = Vec::with_capacity(t.fields.len());
        for k in &t.field_kws {
            fields.push(coll::get(m, k, None)?);
        }
        let mut ext = map::empty_map();
        for (k, v) in entries {
            if !t.field_kws.iter().any(|fk| fk.identical(&k)) {
                map::put(&mut ext, k, v);
            }
        }
        let ext = if ext.is_empty() { Value::NIL } else { map::new(ext) };
        types::instance(&args[0], fields, ext)
    }
    "field*" => fn ct_field(args) [2] {
        let Some(i) = types::as_instance(&args[0]) else {
            return err(
                ILLEGAL_ARGUMENT,
                format!("No matching field found: {} for {}", crate::printer::pr_str(&args[1]), crate::core::type_name(&args[0])),
            );
        };
        let name = named(&args[1], "field")?;
        match i.field_index(&name) {
            Some(ix) => Ok(i.fields[ix].clone()),
            None => err(ILLEGAL_ARGUMENT, format!("No matching field found: {name} for class {}", i.ty().name)),
        }
    }
    // `(field-at* inst i)`: field by declaration index; deftype/defrecord method bodies
    // use it since the macro knows the index.
    "field-at*" => fn ct_field_at(args) [2] {
        let Some(i) = types::as_instance(&args[0]) else {
            return err(ILLEGAL_ARGUMENT, format!("{} has no fields", crate::core::type_name(&args[0])));
        };
        let ix = args::int(&args[1], "field index")?;
        match usize::try_from(ix).ok().and_then(|ix| i.fields.get(ix)) {
            Some(v) => Ok(v.clone()),
            None => err(ILLEGAL_ARGUMENT, format!("No field {ix} in class {}", i.ty().name)),
        }
    }
    "type" => fn ct_type(args) [1] { Ok(types::type_of(&args[0])) }
    "class" => fn ct_class(args) [1] { Ok(types::type_of(&args[0])) }
    "instance?" => fn ct_instance_p(args) [2] {
        let t = expect_type(&args[0], "class")?;
        Ok(Value::bool(!args[1].is_nil() && (t.id == KEY_OBJECT || types::type_key(&args[1]) == t.id)))
    }
    "record?" => fn ct_record_p(args) [1] { Ok(Value::bool(types::is_record(&args[0]))) }
    "type?" => fn ct_type_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Type))) }
    "type-name" => fn ct_type_name(args) [1] {
        Ok(string::new(&expect_type(&args[0], "type")?.name))
    }
    "record-fields" => fn ct_record_fields(args) [1] {
        let t = expect_type(&args[0], "type")?;
        Ok(vector::from_iter(t.fields.iter().map(|f| symbol::new(None, f))))
    }

    // ---- protocols ------------------------------------------------------------------
    "protocol*" => fn ct_protocol(args) [2] {
        let name = match symbol::as_sym(&args[0]) {
            Some(s) => s.qualified(),
            None => named(&args[0], "protocol name")?,
        };
        Ok(types::protocol_new(&name, names(&args[1], "method name")?))
    }
    "protocol-fn*" => fn ct_protocol_fn(args) [2] {
        let p = expect_protocol(&args[0], "protocol")?;
        let m = named(&args[1], "method name")?;
        if !p.methods.iter().any(|x| **x == *m) {
            return err(ILLEGAL_ARGUMENT, format!("Protocol {} has no method {m}", p.name));
        }
        let name = format!("{}/{m}", p.name);
        Ok(closure(nf_protocol_fn, &name, 0, vec![args[0].owned(), string::new(&m)]))
    }
    "extend*" => fn ct_extend(args) [3] {
        let p = expect_protocol(&args[0], "protocol")?;
        let key = type_arg_key(&args[1])?;
        for (k, f) in coll::map_entries(&args[2], "method map")? {
            let m = named(&k, "method name")?;
            if !p.methods.iter().any(|x| **x == *m) {
                return err(ILLEGAL_ARGUMENT, format!("Protocol {} has no method {m}", p.name));
            }
            if func::as_fn(&f).is_none() {
                return err(ILLEGAL_ARGUMENT, format!("Implementation of {m} must be a fn"));
            }
            p.extend(key, &m, f);
        }
        Ok(Value::NIL)
    }
    "satisfies?" => fn ct_satisfies_p(args) [2] {
        let p = expect_protocol(&args[0], "protocol")?;
        Ok(Value::bool(p.satisfied_by(types::type_key(&args[1]))))
    }
    "extends?" => fn ct_extends_p(args) [2] {
        let p = expect_protocol(&args[0], "protocol")?;
        Ok(Value::bool(p.extends(type_arg_key(&args[1])?)))
    }
    "extenders" => fn ct_extenders(args) [1] {
        let p = expect_protocol(&args[0], "protocol")?;
        let ts: Vec<Value> = p.extenders().into_iter().map(|k| {
            if k == KEY_NIL { Value::NIL } else { types::type_by_key(k) }
        }).collect();
        Ok(if ts.is_empty() { Value::NIL } else { list::from_iter(ts) })
    }
    "protocol?" => fn ct_protocol_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Protocol))) }
    "set-print-protocol!" => fn ct_set_print_protocol(args) [1] {
        expect_protocol(&args[0], "protocol")?;
        crate::printer::set_print_protocol(&args[0]);
        Ok(Value::NIL)
    }

    // ---- multimethods ---------------------------------------------------------------
    "multi*" => fn ct_multi(args) [3] {
        let name = match symbol::as_sym(&args[0]) {
            Some(s) => s.qualified(),
            None => named(&args[0], "multimethod name")?,
        };
        if func::as_fn(&args[1]).is_none() && !matches!(args[1].kind(), Some(Kind::Keyword | Kind::Map | Kind::Set | Kind::Vector | Kind::Var)) {
            return err(ILLEGAL_ARGUMENT, "dispatch fn must be callable");
        }
        let table = atom::new(map::new(map::empty_map()));
        Ok(closure(nf_multi_fn, &name, 0, vec![args[0].owned(), args[1].owned(), table, args[2].owned()]))
    }
    "multi?" => fn ct_multi_p(args) [1] { Ok(Value::bool(is_multi(&args[0]))) }
    "add-method*" => fn ct_add_method(args) [3] {
        let (_, table, _) = multi_parts(&args[0])?;
        let a = atom::get(&table);
        loop {
            let old = a.deref();
            let new = coll::assoc(&old, args[1].owned(), args[2].owned())?;
            if a.compare_and_set(&old, new) { break; }
        }
        Ok(args[0].owned())
    }
    "remove-method" => fn ct_remove_method(args) [2] {
        let (_, table, _) = multi_parts(&args[0])?;
        let a = atom::get(&table);
        loop {
            let old = a.deref();
            let new = coll::dissoc(&old, &args[1])?;
            if a.compare_and_set(&old, new) { break; }
        }
        Ok(args[0].owned())
    }
    "remove-all-methods" => fn ct_remove_all_methods(args) [1] {
        let (_, table, _) = multi_parts(&args[0])?;
        atom::get(&table).reset(map::new(map::empty_map()));
        Ok(args[0].owned())
    }
    "methods" => fn ct_methods(args) [1] {
        let (_, table, _) = multi_parts(&args[0])?;
        Ok(atom::get(&table).deref())
    }
    "get-method" => fn ct_get_method(args) [2] {
        let (_, table, default) = multi_parts(&args[0])?;
        let t = atom::get(&table).deref();
        let f = coll::get(&t, &args[1], Some(&Value::UNBOUND))?;
        if !f.is_unbound() { return Ok(f); }
        coll::get(&t, &default, None)
    }

    // ---- dynamic bindings -----------------------------------------------------------
    "push-thread-bindings" => fn ct_push_thread_bindings(args) [1] {
        let mut frame = Vec::new();
        for (k, val) in coll::map_entries(&args[0], "binding map")? {
            let v = expect_var(&k, "binding target")?;
            if !v.has_flag(var::FLAG_DYNAMIC) {
                return err(ILLEGAL_STATE, format!("Can't dynamically bind non-dynamic var: {}", v.qualified()));
            }
            // SAFETY: vars are immortal, so the reference outlives the entry.
            let v: &'static var::VarObj = unsafe { &*(v as *const var::VarObj) };
            frame.push((v, val));
        }
        var::push_bindings(frame);
        Ok(Value::NIL)
    }
    "pop-thread-bindings" => fn ct_pop_thread_bindings(args) [0] {
        let _ = args;
        if !var::pop_bindings() {
            return err(ILLEGAL_STATE, "Pop without matching push");
        }
        Ok(Value::NIL)
    }
    "thread-bound?" => fn ct_thread_bound_p(args) [; 1] {
        for a in args {
            if !var::thread_bound(expect_var(a, "var")?) { return Ok(Value::FALSE); }
        }
        Ok(Value::TRUE)
    }
    "set-dynamic!" => fn ct_set_dynamic(args) [1, 2] {
        let v = expect_var(&args[0], "var")?;
        v.set_flag(var::FLAG_DYNAMIC, args.get(1).is_none_or(|b| b.truthy()));
        Ok(args[0].owned())
    }
    "dynamic?" => fn ct_dynamic_p(args) [1] {
        Ok(Value::bool(expect_var(&args[0], "var")?.has_flag(var::FLAG_DYNAMIC)))
    }
    "alter-meta!" => fn ct_alter_meta(args) [; 2] {
        let mut call_args: Vec<Value> = vec![obj::meta(&args[0]).unwrap_or(Value::NIL)];
        call_args.extend(args[2..].iter().map(|a| a.owned()));
        let m = args[1].call(&call_args)?;
        set_ref_meta(&args[0], m.clone())?;
        Ok(m)
    }
    "reset-meta!" => fn ct_reset_meta(args) [2] {
        set_ref_meta(&args[0], args[1].owned())?;
        Ok(args[1].owned())
    }

    // ---- sorted collections ---------------------------------------------------------
    "sorted-map" => fn ct_sorted_map(args) [; 0] {
        if !args.len().is_multiple_of(2) {
            return err(ILLEGAL_ARGUMENT, "sorted-map requires an even number of arguments");
        }
        let mut m = sorted::OMap::new();
        for kv in args.chunks(2) {
            m = sorted::map_assoc(&m, kv[0].owned(), kv[1].owned())?;
        }
        Ok(sorted::map_new(m))
    }
    "sorted-set" => fn ct_sorted_set(args) [; 0] {
        let mut s = sorted::OSet::new();
        for x in args {
            s = sorted::set_insert(&s, x.owned())?;
        }
        Ok(sorted::set_new(s))
    }
    "sorted?" => fn ct_sorted_p(args) [1] {
        Ok(Value::bool(matches!(args[0].kind(), Some(Kind::SortedMap | Kind::SortedSet))))
    }
    "rseq" => fn ct_rseq(args) [1] {
        if !matches!(args[0].kind(), Some(Kind::Vector | Kind::SortedMap | Kind::SortedSet)) {
            return err(ILLEGAL_ARGUMENT, format!("rseq not supported on {}", crate::core::type_name(&args[0])));
        }
        let mut v = coll::to_vec(&args[0])?;
        v.reverse();
        Ok(if v.is_empty() { Value::NIL } else { list::from_iter(v) })
    }
    "subseq" => fn ct_subseq(args) [3, 5] {
        // (subseq coll test key) / (subseq coll start-test start-key end-test end-key)
        let items = coll::to_vec(&args[0])?;
        let key_of = |e: &Value| -> Value {
            if args[0].is_kind(Kind::SortedMap) { vector::get(e).v[0].clone() } else { e.clone() }
        };
        let mut out = Vec::new();
        for e in &items {
            let k = key_of(e);
            if args[1].call(&[k.clone(), args[2].owned()])?.truthy()
                && (args.len() == 3 || args[3].call(&[k, args[4].owned()])?.truthy())
            {
                out.push(e.clone());
            }
        }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
}

/// `alter-meta!`/`reset-meta!` targets: vars and atoms (reference types).
fn set_ref_meta(r: &Value, m: Value) -> RtResult<()> {
    if !m.is_nil() && !m.is_kind(Kind::Map) {
        return err(ILLEGAL_ARGUMENT, "metadata must be a map or nil");
    }
    match r.kind() {
        Some(Kind::Var) => {
            var::get(r).set_meta(m);
            Ok(())
        }
        Some(Kind::Atom) => {
            atom::get(r).set_meta(m);
            Ok(())
        }
        _ => err(
            ILLEGAL_ARGUMENT,
            format!("{} cannot be cast to IReference", crate::core::type_name(r)),
        ),
    }
}
