//! `pr-str` / `print-str`. Output is byte-identical between the JIT and AOT because
//! both use this code; it approximates Clojure's (Java's) formatting.
use crate::obj::{self, Kind};
use crate::value::Value;

const MAX_DEPTH: usize = 256;

pub fn pr_str(v: &Value) -> String {
    let mut s = String::new();
    write(&mut s, v, true, 0);
    s
}

pub fn print_str(v: &Value) -> String {
    let mut s = String::new();
    write(&mut s, v, false, 0);
    s
}

pub fn write(out: &mut String, v: &Value, readably: bool, depth: usize) {
    if depth > MAX_DEPTH {
        out.push_str("...");
        return;
    }
    if v.is_nil() {
        out.push_str("nil");
        return;
    }
    if let Some(b) = v.as_bool() {
        out.push_str(if b { "true" } else { "false" });
        return;
    }
    if let Some(n) = v.as_fixnum() {
        out.push_str(&n.to_string());
        return;
    }
    if let Some(c) = v.as_char() {
        write_char(out, c, readably);
        return;
    }
    if v.is_exc() {
        out.push_str("#<EXC>");
        return;
    }
    if v.is_unbound() {
        out.push_str("#<Unbound>");
        return;
    }
    let Some(kind) = v.kind() else {
        out.push_str("#<?>");
        return;
    };
    match kind {
        Kind::Long => out.push_str(&obj::long::get(v).to_string()),
        Kind::Double => out.push_str(&format_double(obj::double::get(v))),
        Kind::String => {
            let s = obj::string::get(v);
            if readably {
                write_string_readably(out, s)
            } else {
                out.push_str(s)
            }
        }
        Kind::Symbol => out.push_str(&obj::symbol::get(v).qualified()),
        Kind::Keyword => {
            out.push(':');
            out.push_str(&obj::keyword::get(v).qualified());
        }
        Kind::List => {
            out.push('(');
            write_items(out, obj::list::get(v).iter(), readably, depth);
            out.push(')');
        }
        Kind::Vector => {
            out.push('[');
            write_items(out, obj::vector::get(v).v.iter(), readably, depth);
            out.push(']');
        }
        Kind::Map => {
            out.push('{');
            let mut first = true;
            for (k, val) in obj::map::get(v).m.iter() {
                if !first {
                    out.push_str(", ");
                }
                first = false;
                write(out, k, readably, depth + 1);
                out.push(' ');
                write(out, val, readably, depth + 1);
            }
            out.push('}');
        }
        Kind::Set => {
            out.push_str("#{");
            write_items(out, obj::set::get(v).s.iter(), readably, depth);
            out.push('}');
        }
        Kind::Fn => {
            let f = obj::func::get(v);
            out.push_str(&format!("#object[{} 0x{:x}]", f.proto().name, v.bits()));
        }
        Kind::Var => {
            out.push_str("#'");
            out.push_str(&obj::var::get(v).qualified());
        }
        Kind::Exception => {
            let e = obj::exception::get(v);
            out.push_str("#error {:class ");
            out.push_str(&e.class);
            out.push_str(", :message ");
            write(out, &e.message, true, depth + 1);
            if !e.data.is_nil() {
                out.push_str(", :data ");
                write(out, &e.data, true, depth + 1);
            }
            if !e.cause.is_nil() {
                out.push_str(", :cause ");
                write(out, &e.cause, true, depth + 1);
            }
            out.push('}');
        }
        Kind::Atom => {
            out.push_str("#atom[");
            write(out, &obj::atom::get(v).deref(), readably, depth + 1);
            out.push_str(&format!(" 0x{:x}]", v.bits()));
        }
        Kind::Cons | Kind::LazySeq => {
            out.push('(');
            let mut first = true;
            for x in crate::coll::seq_iter(v) {
                match x {
                    Ok(x) => {
                        if !first {
                            out.push(' ');
                        }
                        first = false;
                        write(out, &x, readably, depth + 1);
                    }
                    Err(_) => {
                        crate::exc::take();
                        out.push_str(if first {
                            "#<error realising seq>"
                        } else {
                            " #<error realising seq>"
                        });
                        break;
                    }
                }
            }
            out.push(')');
        }
        Kind::Regex => {
            out.push_str("#\"");
            out.push_str(&obj::regex::get(v).source);
            out.push('"');
        }
        Kind::Namespace => {
            out.push_str("#namespace[");
            out.push_str(&crate::ns::get(v).name);
            out.push(']');
        }
    }
}

fn write_items<'a>(
    out: &mut String,
    items: impl Iterator<Item = &'a Value>,
    readably: bool,
    depth: usize,
) {
    let mut first = true;
    for x in items {
        if !first {
            out.push(' ');
        }
        first = false;
        write(out, x, readably, depth + 1);
    }
}

fn write_char(out: &mut String, c: char, readably: bool) {
    if !readably {
        out.push(c);
        return;
    }
    out.push('\\');
    match c {
        '\n' => out.push_str("newline"),
        ' ' => out.push_str("space"),
        '\t' => out.push_str("tab"),
        '\r' => out.push_str("return"),
        '\u{8}' => out.push_str("backspace"),
        '\u{c}' => out.push_str("formfeed"),
        _ => out.push(c),
    }
}

pub fn write_string_readably(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            _ => out.push(c),
        }
    }
    out.push('"');
}

/// Java-style double formatting: `1.0`, `3.14`, `1.0E20`, `1.5E-7`, `##Inf`, `##NaN`.
pub fn format_double(d: f64) -> String {
    if d.is_nan() {
        return "##NaN".to_string();
    }
    if d.is_infinite() {
        return if d > 0.0 { "##Inf" } else { "##-Inf" }.to_string();
    }
    let a = d.abs();
    if a != 0.0 && !(1e-3..1e7).contains(&a) {
        let s = format!("{d:e}");
        let (mant, exp) = s.split_once('e').unwrap();
        let mant = if mant.contains('.') {
            mant.to_string()
        } else {
            format!("{mant}.0")
        };
        return format!("{mant}E{exp}");
    }
    let s = format!("{d}");
    if s.contains('.') { s } else { format!("{s}.0") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj::{keyword, list, map, string, symbol, vector};

    #[test]
    fn doubles() {
        assert_eq!(format_double(1.0), "1.0");
        assert_eq!(format_double(-0.5), "-0.5");
        assert_eq!(format_double(2.75), "2.75");
        assert_eq!(format_double(1e7), "1.0E7");
        assert_eq!(format_double(12345678.9), "1.23456789E7");
        assert_eq!(format_double(0.001), "0.001");
        assert_eq!(format_double(0.0001), "1.0E-4");
        assert_eq!(format_double(1.5e-7), "1.5E-7");
        assert_eq!(format_double(0.0), "0.0");
        assert_eq!(format_double(f64::INFINITY), "##Inf");
        assert_eq!(format_double(0.1 + 0.2), "0.30000000000000004");
    }

    #[test]
    fn collections_and_atoms() {
        let v = vector::from_slice(&[
            Value::int(1),
            string::new("a"),
            keyword::parse("k"),
            Value::char('x'),
        ]);
        assert_eq!(pr_str(&v), "[1 \"a\" :k \\x]");
        assert_eq!(print_str(&v), "[1 a :k x]");
        let l = list::from_slice(&[symbol::parse("f"), Value::NIL, Value::TRUE]);
        assert_eq!(pr_str(&l), "(f nil true)");
        assert_eq!(pr_str(&list::empty()), "()");
        let m = map::from_pairs(&[keyword::parse("a"), Value::int(1)]);
        assert_eq!(pr_str(&m), "{:a 1}");
        assert_eq!(pr_str(&string::new("q\"\n")), "\"q\\\"\\n\"");
        assert_eq!(pr_str(&Value::char('\n')), "\\newline");
    }
}
