//! `clojure.string` plus the string-related core fns (`subs`, `char`, `int`).
use crate::builtins;
use crate::core::args;
use crate::exc::err;
use crate::obj::exception::INDEX_OUT_OF_BOUNDS;
use crate::obj::{list, string};
use crate::value::Value;

builtins! {
    ns = "clojure.string";
    "upper-case" => fn cs_upper(args) [1] { Ok(string::from_string(crate::printer::print_str(&args[0]).to_uppercase())) }
    "lower-case" => fn cs_lower(args) [1] { Ok(string::from_string(crate::printer::print_str(&args[0]).to_lowercase())) }
    "capitalize" => fn cs_capitalize(args) [1] {
        let s = crate::printer::print_str(&args[0]);
        let mut cs = s.chars();
        Ok(string::from_string(match cs.next() {
            Some(f) => f.to_uppercase().collect::<String>() + &cs.as_str().to_lowercase(),
            None => String::new(),
        }))
    }
    "trim" => fn cs_trim(args) [1] { Ok(string::new(args::string(&args[0], "s")?.trim())) }
    "triml" => fn cs_triml(args) [1] { Ok(string::new(args::string(&args[0], "s")?.trim_start())) }
    "trimr" => fn cs_trimr(args) [1] { Ok(string::new(args::string(&args[0], "s")?.trim_end())) }
    "blank?" => fn cs_blank_p(args) [1] {
        Ok(Value::bool(args[0].is_nil() || args::string(&args[0], "s")?.trim().is_empty()))
    }
    "starts-with?" => fn cs_starts_with(args) [2] {
        Ok(Value::bool(args::string(&args[0], "s")?.starts_with(&crate::printer::print_str(&args[1]))))
    }
    "ends-with?" => fn cs_ends_with(args) [2] {
        Ok(Value::bool(args::string(&args[0], "s")?.ends_with(&crate::printer::print_str(&args[1]))))
    }
    "includes?" => fn cs_includes(args) [2] {
        Ok(Value::bool(args::string(&args[0], "s")?.contains(&crate::printer::print_str(&args[1]))))
    }
    "index-of" => fn cs_index_of(args) [2, 3] {
        let s = args::string(&args[0], "s")?;
        let needle = crate::printer::print_str(&args[1]);
        let from = if args.len() == 3 { args::int(&args[2], "from")?.max(0) as usize } else { 0 };
        let chars: Vec<(usize, char)> = s.char_indices().collect();
        if from > chars.len() { return Ok(Value::NIL); }
        let byte_from = chars.get(from).map_or(s.len(), |(b, _)| *b);
        Ok(match s[byte_from..].find(&needle) {
            Some(b) => Value::int(s[..byte_from + b].chars().count() as i64),
            None => Value::NIL,
        })
    }
    "split" => fn cs_split(args) [2, 3] {
        let s = args::string(&args[0], "s")?;
        let limit = if args.len() == 3 { args::int(&args[2], "limit")? } else { 0 };
        let mut parts: Vec<&str> = if let Some(re) = crate::obj::regex::as_regex(&args[1]) {
            if limit > 0 { re.re.splitn(s, limit as usize).collect() } else { re.re.split(s).collect() }
        } else {
            let sep = crate::printer::print_str(&args[1]);
            if limit > 0 { s.splitn(limit as usize, sep.as_str()).collect() } else { s.split(sep.as_str()).collect() }
        };
        if limit <= 0 {
            while parts.len() > 1 && parts.last().is_some_and(|x| x.is_empty()) { parts.pop(); }
        }
        Ok(crate::obj::vector::from_iter(parts.into_iter().map(string::new).collect::<Vec<_>>()))
    }
    "split-lines" => fn cs_split_lines(args) [1] {
        Ok(crate::obj::vector::from_iter(args::string(&args[0], "s")?.lines().map(string::new).collect::<Vec<_>>()))
    }
    "join" => fn cs_join(args) [1, 2] {
        let (sep, coll) = if args.len() == 2 { (crate::printer::print_str(&args[0]), &args[1]) } else { (String::new(), &args[0]) };
        let parts: Vec<String> = crate::coll::to_vec(coll)?.iter().map(|x| if x.is_nil() { String::new() } else { crate::printer::print_str(x) }).collect();
        Ok(string::from_string(parts.join(&sep)))
    }
    "replace" => fn cs_replace(args) [3] {
        let s = args::string(&args[0], "s")?;
        let to = crate::printer::print_str(&args[2]);
        if let Some(re) = crate::obj::regex::as_regex(&args[1]) {
            return Ok(string::from_string(re.re.replace_all(s, to.as_str()).into_owned()));
        }
        let from = crate::printer::print_str(&args[1]);
        Ok(string::from_string(s.replace(&from, &to)))
    }
    "replace-first" => fn cs_replace_first(args) [3] {
        let s = args::string(&args[0], "s")?;
        let to = crate::printer::print_str(&args[2]);
        if let Some(re) = crate::obj::regex::as_regex(&args[1]) {
            return Ok(string::from_string(re.re.replace(s, to.as_str()).into_owned()));
        }
        let from = crate::printer::print_str(&args[1]);
        Ok(string::from_string(s.replacen(&from, &to, 1)))
    }
    "reverse" => fn cs_reverse(args) [1] { Ok(string::from_string(args::string(&args[0], "s")?.chars().rev().collect())) }
    "escape" => fn cs_escape(args) [2] {
        let s = args::string(&args[0], "s")?;
        let mut out = String::new();
        for c in s.chars() {
            let r = crate::coll::get(&args[1], &Value::char(c), None)?;
            if r.is_nil() { out.push(c) } else { out.push_str(&crate::printer::print_str(&r)) }
        }
        Ok(string::from_string(out))
    }
}

pub mod core_fns {
    use super::*;
    use crate::obj::exception::ILLEGAL_ARGUMENT;

    builtins! {
        "subs" => fn cf_subs(args) [2, 3] {
            let s = args::string(&args[0], "s")?;
            let n = s.chars().count() as i64;
            let start = args::int(&args[1], "start")?;
            let end = if args.len() == 3 { args::int(&args[2], "end")? } else { n };
            if start < 0 || end > n || start > end {
                return err(INDEX_OUT_OF_BOUNDS, format!("String index out of range: start {start}, end {end}, length {n}"));
            }
            Ok(string::from_string(s.chars().skip(start as usize).take((end - start) as usize).collect()))
        }
        "char" => fn cf_char(args) [1] {
            if let Some(c) = args[0].as_char() { return Ok(Value::char(c)); }
            let n = args::int(&args[0], "code point")?;
            u32::try_from(n).ok().and_then(char::from_u32).map(Value::char)
                .map_or_else(|| err(ILLEGAL_ARGUMENT, format!("Invalid code point: {n}")), Ok)
        }
        "int" => fn cf_int(args) [1] {
            if let Some(c) = args[0].as_char() { return Ok(Value::int(c as i64)); }
            Ok(Value::int(crate::num::to_long(&crate::num::expect_num(&args[0])?)?))
        }
        "long" => fn cf_long(args) [1] {
            Ok(Value::int(crate::num::to_long(&crate::num::expect_num(&args[0])?)?))
        }
        "double" => fn cf_double(args) [1] {
            Ok(Value::double(crate::num::f(&crate::num::expect_num(&args[0])?)))
        }
        "chars" => fn cf_chars(args) [1] {
            let s = args::string(&args[0], "s")?;
            Ok(list::from_iter(s.chars().map(Value::char).collect::<Vec<_>>()))
        }
        "parse-long" => fn cf_parse_long(args) [1] {
            Ok(args::string(&args[0], "s")?.trim().parse::<i64>().map_or(Value::NIL, Value::int))
        }
        "parse-double" => fn cf_parse_double(args) [1] {
            Ok(args::string(&args[0], "s")?.trim().parse::<f64>().map_or(Value::NIL, Value::double))
        }
    }
}
