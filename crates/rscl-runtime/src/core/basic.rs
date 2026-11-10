//! Equality, predicates, strings and printing.
use std::io::Write;

use crate::builtins;
use crate::obj::{self, Kind, string};
use crate::value::{VRef, Value};

fn join_args(args: &[VRef], readably: bool, sep: &str) -> String {
    let mut out = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        crate::printer::write(&mut out, a, readably, 0);
    }
    out
}

/// Write to stdout, ignoring a closed pipe.
pub fn emit(s: &str) {
    let out = std::io::stdout();
    let mut lock = out.lock();
    let _ = lock.write_all(s.as_bytes());
}

builtins! {
    "=" => fn cf_eq(args) [; 1] {
        let first = &args[0];
        Ok(Value::bool(args[1..].iter().all(|a| crate::equiv::equiv(first, a))))
    }
    "not=" => fn cf_not_eq(args) [; 1] {
        let first = &args[0];
        Ok(Value::bool(!args[1..].iter().all(|a| crate::equiv::equiv(first, a))))
    }
    "identical?" => fn cf_identical_p(args) [2] { Ok(Value::bool(args[0].identical(&args[1]))) }
    "not" => fn cf_not(args) [1] { Ok(Value::bool(!args[0].truthy())) }
    "boolean" => fn cf_boolean(args) [1] { Ok(Value::bool(args[0].truthy())) }
    "identity" => fn cf_identity(args) [1] { Ok(args[0].owned()) }
    "nil?" => fn cf_nil_p(args) [1] { Ok(Value::bool(args[0].is_nil())) }
    "some?" => fn cf_some_p(args) [1] { Ok(Value::bool(!args[0].is_nil())) }
    "true?" => fn cf_true_p(args) [1] { Ok(Value::bool(args[0].is_true())) }
    "false?" => fn cf_false_p(args) [1] { Ok(Value::bool(args[0].is_false())) }
    "boolean?" => fn cf_boolean_p(args) [1] { Ok(Value::bool(args[0].as_bool().is_some())) }
    "number?" => fn cf_number_p(args) [1] { Ok(Value::bool(crate::num::num(&args[0]).is_some())) }
    "integer?" => fn cf_integer_p(args) [1] { Ok(Value::bool(args[0].as_int().is_some() || args[0].is_kind(Kind::BigInt))) }
    "int?" => fn cf_int_p(args) [1] { Ok(Value::bool(args[0].as_int().is_some())) }
    "float?" => fn cf_float_p(args) [1] { Ok(Value::bool(args[0].as_double().is_some())) }
    "double?" => fn cf_double_p(args) [1] { Ok(Value::bool(args[0].as_double().is_some())) }
    "string?" => fn cf_string_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::String))) }
    "char?" => fn cf_char_p(args) [1] { Ok(Value::bool(args[0].as_char().is_some())) }
    "keyword?" => fn cf_keyword_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Keyword))) }
    "symbol?" => fn cf_symbol_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Symbol))) }
    "fn?" => fn cf_fn_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Fn))) }
    "var?" => fn cf_var_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Var))) }
    "str" => fn cf_str(args) [; 0] {
        let mut out = String::new();
        for a in args {
            crate::printer::write_str(&mut out, a);
        }
        Ok(string::from_string(out))
    }
    "pr-str" => fn cf_pr_str(args) [; 0] { Ok(string::from_string(join_args(args, true, " "))) }
    "print-str" => fn cf_print_str(args) [; 0] { Ok(string::from_string(join_args(args, false, " "))) }
    "pr" => fn cf_pr(args) [; 0] { emit(&join_args(args, true, " ")); Ok(Value::NIL) }
    "prn" => fn cf_prn(args) [; 0] { let mut s = join_args(args, true, " "); s.push('\n'); emit(&s); Ok(Value::NIL) }
    "print" => fn cf_print(args) [; 0] { emit(&join_args(args, false, " ")); Ok(Value::NIL) }
    "println" => fn cf_println(args) [; 0] { let mut s = join_args(args, false, " "); s.push('\n'); emit(&s); Ok(Value::NIL) }
    "newline" => fn cf_newline(args) [0] { let _ = args; emit("\n"); Ok(Value::NIL) }
    "flush" => fn cf_flush(args) [0] { let _ = args; let _ = std::io::stdout().flush(); Ok(Value::NIL) }
    "name" => fn cf_name(args) [1] {
        let v = &args[0];
        match v.kind() {
            Some(Kind::String) => Ok(v.owned()),
            Some(Kind::Symbol) => Ok(string::new(obj::symbol::get(v).name())),
            Some(Kind::Keyword) => Ok(string::new(obj::keyword::get(v).name())),
            _ => crate::exc::err(obj::exception::CLASS_CAST, format!("{} cannot be cast to Named", crate::core::type_name(v))),
        }
    }
    "namespace" => fn cf_namespace(args) [1] {
        let v = &args[0];
        let ns = match v.kind() {
            Some(Kind::Symbol) => obj::symbol::get(v).ns().map(string::new),
            Some(Kind::Keyword) => obj::keyword::get(v).ns().map(string::new),
            _ => return crate::exc::err(obj::exception::CLASS_CAST, format!("{} cannot be cast to Named", crate::core::type_name(v))),
        };
        Ok(ns.unwrap_or(Value::NIL))
    }
    "keyword" => fn cf_keyword(args) [1, 2] {
        if args.len() == 2 {
            let ns = string::as_str(&args[0]).map(str::to_owned);
            let name = string::as_str(&args[1]).ok_or_else(|| crate::exc::throw_new(obj::exception::CLASS_CAST, "keyword name must be a string"))?;
            return Ok(obj::keyword::intern(ns.as_deref(), name));
        }
        let v = &args[0];
        match v.kind() {
            Some(Kind::Keyword) => Ok(v.owned()),
            Some(Kind::String) => Ok(obj::keyword::parse(string::get(v))),
            Some(Kind::Symbol) => { let s = obj::symbol::get(v); Ok(obj::keyword::intern(s.ns(), s.name())) }
            _ => Ok(Value::NIL),
        }
    }
    "symbol" => fn cf_symbol(args) [1, 2] {
        if args.len() == 2 {
            let ns = string::as_str(&args[0]).map(str::to_owned);
            let name = string::as_str(&args[1]).ok_or_else(|| crate::exc::throw_new(obj::exception::CLASS_CAST, "symbol name must be a string"))?;
            return Ok(obj::symbol::new(ns.as_deref(), name));
        }
        let v = &args[0];
        match v.kind() {
            Some(Kind::Symbol) => Ok(v.owned()),
            Some(Kind::String) => Ok(obj::symbol::parse(string::get(v))),
            Some(Kind::Keyword) => { let k = obj::keyword::get(v); Ok(obj::symbol::new(k.ns(), k.name())) }
            _ => crate::exc::err(obj::exception::ILLEGAL_ARGUMENT, format!("no conversion to symbol from {}", crate::core::type_name(v))),
        }
    }
    "ex-info" => fn cf_ex_info(args) [2, 3] {
        let msg = string::as_str(&args[0]).ok_or_else(|| crate::exc::throw_new(obj::exception::CLASS_CAST, "ex-info message must be a string"))?;
        if !args[1].is_kind(Kind::Map) {
            return crate::exc::err(obj::exception::ILLEGAL_ARGUMENT, "ex-info data must be a map");
        }
        let cause = if args.len() == 3 { args[2].owned() } else { Value::NIL };
        Ok(obj::exception::new(obj::exception::EXCEPTION_INFO, Some(msg), args[1].owned(), cause))
    }
    "ex-message" => fn cf_ex_message(args) [1] {
        Ok(obj::exception::as_exc(&args[0]).map_or(Value::NIL, |e| e.message.clone()))
    }
    "ex-data" => fn cf_ex_data(args) [1] {
        Ok(obj::exception::as_exc(&args[0]).map_or(Value::NIL, |e| e.data.clone()))
    }
    "ex-cause" => fn cf_ex_cause(args) [1] {
        Ok(obj::exception::as_exc(&args[0]).map_or(Value::NIL, |e| e.cause.clone()))
    }
}
