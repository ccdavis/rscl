//! Regular expressions: `re-pattern`, `re-find`, `re-matches`, `re-seq`.
use crate::builtins;
use crate::core::args;
use crate::exc::{RtResult, err};
use crate::obj::exception::CLASS_CAST;
use crate::obj::{Kind, list, regex, string, vector};
use crate::value::Value;

/// Accept a regex object or a string pattern.
pub fn pattern(v: &Value) -> RtResult<Value> {
    match v.kind() {
        Some(Kind::Regex) => Ok(v.clone()),
        Some(Kind::String) => regex::new(string::get(v)),
        _ => err(
            CLASS_CAST,
            format!("{} cannot be cast to Pattern", crate::core::type_name(v)),
        ),
    }
}

/// A match as Clojure returns it: the whole match when the pattern has no groups,
/// otherwise a vector of the whole match and every group (`nil` for unmatched ones).
fn match_value(caps: &::regex::Captures<'_>) -> Value {
    if caps.len() == 1 {
        return string::new(&caps[0]);
    }
    vector::from_iter(
        (0..caps.len())
            .map(|i| caps.get(i).map_or(Value::NIL, |m| string::new(m.as_str())))
            .collect::<Vec<_>>(),
    )
}

builtins! {
    "re-pattern" => fn cf_re_pattern(args) [1] { pattern(&args[0]) }
    "re-find" => fn cf_re_find(args) [2] {
        let p = pattern(&args[0])?;
        let s = args::string(&args[1], "s")?;
        Ok(regex::get(&p).re.captures(s).map_or(Value::NIL, |c| match_value(&c)))
    }
    "re-matches" => fn cf_re_matches(args) [2] {
        let p = pattern(&args[0])?;
        let s = args::string(&args[1], "s")?;
        match regex::get(&p).re.captures(s) {
            Some(c) if c.get(0).is_some_and(|m| m.start() == 0 && m.end() == s.len()) => Ok(match_value(&c)),
            _ => Ok(Value::NIL),
        }
    }
    "re-seq" => fn cf_re_seq(args) [2] {
        let p = pattern(&args[0])?;
        let s = args::string(&args[1], "s")?;
        let all: Vec<Value> = regex::get(&p).re.captures_iter(s).map(|c| match_value(&c)).collect();
        Ok(if all.is_empty() { Value::NIL } else { list::from_iter(all) })
    }
    "re-quote" => fn cf_re_quote(args) [1] { Ok(string::from_string(::regex::escape(args::string(&args[0], "s")?))) }
    "regex?" => fn cf_regex_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Regex))) }
}
