//! The reader: text → forms (runtime `Value`s), so the compiler is homoiconic and
//! `read-string` is available at run time. Records source positions in object
//! headers. Implements syntax-quote, `#()` and `#""` regex literals.
use std::collections::{BTreeMap, HashMap};

use crate::obj::{self, Kind, keyword, list, map, set, string, symbol, vector};
use crate::pos::{self, SrcPos};
use crate::value::Value;

/// Special forms: never qualified by syntax-quote and never shadowed by macros.
pub const SPECIAL_FORMS: &[&str] = &[
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
    // Not forms of their own, but special symbols (Clojure lists them too) so
    // syntax-quote leaves them unqualified: `(try ~@body (finally ...))`.
    "catch",
    "finally",
    "&",
];

pub fn is_special(name: &str) -> bool {
    SPECIAL_FORMS.contains(&name)
}

/// What the reader needs from its environment: `::kw` resolution and symbol
/// qualification inside syntax-quote.
pub trait ReaderEnv {
    fn current_ns(&self) -> String;
    fn resolve_alias(&self, alias: &str) -> Option<String>;
    /// Namespace an unqualified symbol is qualified with inside syntax-quote: the
    /// namespace of the var it resolves to, else the current namespace.
    fn resolve_symbol_ns(&self, _name: &str) -> String {
        self.current_ns()
    }
}

/// Uses the runtime's current namespace.
pub struct DefaultEnv;

impl ReaderEnv for DefaultEnv {
    fn current_ns(&self) -> String {
        crate::ns::current().name.to_string()
    }
    fn resolve_alias(&self, alias: &str) -> Option<String> {
        crate::ns::current()
            .resolve_alias(alias)
            .map(|n| n.name.to_string())
    }
    fn resolve_symbol_ns(&self, name: &str) -> String {
        let cur = crate::ns::current();
        match crate::ns::resolve(cur, None, name) {
            Some(v) => v.ns.to_string(),
            None => cur.name.to_string(),
        }
    }
}

fn core_sym(name: &str) -> Value {
    symbol::new(Some("clojure.core"), name)
}

fn is_core_call(items: &[Value], name: &str) -> bool {
    items
        .first()
        .and_then(symbol::as_sym)
        .is_some_and(|s| s.is(Some("clojure.core"), name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadErrorKind {
    /// End of input inside a form: a REPL should ask for more input.
    Eof,
    Syntax,
}

#[derive(Debug, Clone)]
pub struct ReadError {
    pub kind: ReadErrorKind,
    pub line: u32,
    pub col: u32,
    pub msg: String,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}, col {}: {}", self.line, self.col, self.msg)
    }
}
impl std::error::Error for ReadError {}

pub type ReadResult<T> = Result<T, ReadError>;

pub struct Reader<'a> {
    src: &'a str,
    pos: usize,
    line: u32,
    col: u32,
    file: u32,
    /// Record source positions (only for source that is being compiled; run-time
    /// `read-string` must not grow the global position table).
    record_pos: bool,
    env: &'a dyn ReaderEnv,
    /// One auto-gensym table per enclosing syntax-quote (`x#` → the same symbol
    /// within one quote).
    gensyms: Vec<HashMap<String, Value>>,
    /// `%`-argument table while reading a `#(...)` body (index → symbol; -1 is `%&`).
    arg_env: Option<BTreeMap<i64, Value>>,
}

/// Read exactly one form from `s`.
pub fn read_str(s: &str) -> ReadResult<Value> {
    let mut r = Reader::new(s, &DefaultEnv);
    match r.read()? {
        Some(v) => Ok(v),
        None => Err(r.error(ReadErrorKind::Eof, "EOF while reading")),
    }
}

fn is_ws(c: char) -> bool {
    c.is_whitespace() || c == ','
}

/// Characters that end a token.
fn is_terminator(c: char) -> bool {
    is_ws(c)
        || matches!(
            c,
            '(' | ')' | '[' | ']' | '{' | '}' | '"' | ';' | '`' | '~' | '@' | '^' | '\\'
        )
}

impl<'a> Reader<'a> {
    pub fn new(src: &'a str, env: &'a dyn ReaderEnv) -> Self {
        Reader {
            src,
            pos: 0,
            line: 1,
            col: 1,
            file: 0,
            record_pos: false,
            env,
            gensyms: Vec::new(),
            arg_env: None,
        }
    }

    /// Name the source file and start recording positions on the forms read.
    pub fn with_file(mut self, file: u32) -> Self {
        self.file = file;
        self.record_pos = true;
        self
    }

    /// Byte offset of the next unread character.
    pub fn offset(&self) -> usize {
        self.pos
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek2(&self) -> Option<char> {
        let mut it = self.src[self.pos..].chars();
        it.next();
        it.next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn error(&self, kind: ReadErrorKind, msg: impl Into<String>) -> ReadError {
        ReadError {
            kind,
            line: self.line,
            col: self.col,
            msg: msg.into(),
        }
    }

    fn syntax(&self, msg: impl Into<String>) -> ReadError {
        self.error(ReadErrorKind::Syntax, msg)
    }

    fn eof(&self, what: &str) -> ReadError {
        self.error(ReadErrorKind::Eof, format!("EOF while reading {what}"))
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(c) if is_ws(c) => {
                    self.bump();
                }
                Some(';') => self.skip_line(),
                Some('#') if self.peek2() == Some('!') => self.skip_line(),
                _ => return,
            }
        }
    }

    fn skip_line(&mut self) {
        while let Some(c) = self.bump() {
            if c == '\n' {
                break;
            }
        }
    }

    fn here(&self) -> SrcPos {
        SrcPos {
            file: self.file,
            line: self.line,
            col: self.col,
        }
    }

    fn stamp(&self, v: Value, at: SrcPos) -> Value {
        // Interned keywords (and other permanent objects) are shared: never write
        // into their headers.
        if self.record_pos && v.header().is_some_and(|h| !h.is_permanent()) {
            pos::set_pos(&v, pos::add(at));
        }
        v
    }

    /// Read the next form; `Ok(None)` at a clean end of input.
    pub fn read(&mut self) -> ReadResult<Option<Value>> {
        loop {
            self.skip_ws_and_comments();
            let Some(c) = self.peek() else {
                return Ok(None);
            };
            let at = self.here();
            // `#_` discards the next form and keeps going.
            if c == '#' && self.peek2() == Some('_') {
                self.bump();
                self.bump();
                match self.read()? {
                    Some(_) => continue,
                    None => return Err(self.eof("form after #_")),
                }
            }
            let v = self.read_form(c)?;
            return Ok(Some(self.stamp(v, at)));
        }
    }

    fn read_required(&mut self, what: &str) -> ReadResult<Value> {
        match self.read()? {
            Some(v) => Ok(v),
            None => Err(self.eof(what)),
        }
    }

    fn read_form(&mut self, c: char) -> ReadResult<Value> {
        // The reader recurses per nesting level: refuse input that would run the
        // native stack out instead of crashing.
        if crate::native::stack_low() {
            return Err(self.syntax("Forms are nested too deeply"));
        }
        match c {
            '(' => {
                self.bump();
                let items = self.read_seq(')')?;
                Ok(list::from_slice(&items))
            }
            '[' => {
                self.bump();
                let items = self.read_seq(']')?;
                Ok(vector::from_slice(&items))
            }
            '{' => {
                self.bump();
                let items = self.read_seq('}')?;
                self.make_map(items)
            }
            ')' | ']' | '}' => Err(self.syntax(format!("Unmatched delimiter: {c}"))),
            '"' => {
                self.bump();
                self.read_string()
            }
            '\\' => {
                self.bump();
                self.read_char()
            }
            '\'' => {
                self.bump();
                let f = self.read_required("quoted form")?;
                Ok(list::from_slice(&[symbol::new(None, "quote"), f]))
            }
            '@' => {
                self.bump();
                let f = self.read_required("deref form")?;
                Ok(list::from_slice(&[
                    symbol::new(Some("clojure.core"), "deref"),
                    f,
                ]))
            }
            '`' => {
                self.bump();
                self.gensyms.push(HashMap::new());
                let f = self.read_required("syntax-quoted form");
                let r = f.and_then(|f| self.syntax_quote(&f));
                self.gensyms.pop();
                r
            }
            '~' => {
                self.bump();
                if self.peek() == Some('@') {
                    self.bump();
                    let f = self.read_required("unquote-splicing form")?;
                    Ok(list::from_slice(&[core_sym("unquote-splicing"), f]))
                } else {
                    let f = self.read_required("unquote form")?;
                    Ok(list::from_slice(&[core_sym("unquote"), f]))
                }
            }
            '^' => {
                self.bump();
                self.read_meta()
            }
            '#' => {
                self.bump();
                self.read_dispatch()
            }
            ':' => {
                self.bump();
                self.read_keyword()
            }
            _ => {
                let tok = self.read_token();
                if tok.is_empty() {
                    // A lone terminator we do not handle (should not happen).
                    self.bump();
                    return Err(self.syntax(format!("Unexpected character: {c}")));
                }
                self.interpret_token(&tok)
            }
        }
    }

    fn read_seq(&mut self, close: char) -> ReadResult<Vec<Value>> {
        let mut items = Vec::new();
        loop {
            self.skip_ws_and_comments();
            match self.peek() {
                None => return Err(self.eof(&format!("sequence, expected {close}"))),
                Some(c) if c == close => {
                    self.bump();
                    return Ok(items);
                }
                Some(c) if matches!(c, ')' | ']' | '}') => {
                    return Err(self.syntax(format!("Unmatched delimiter: {c}, expected {close}")));
                }
                Some(_) => match self.read()? {
                    Some(v) => items.push(v),
                    None => return Err(self.eof(&format!("sequence, expected {close}"))),
                },
            }
        }
    }

    fn make_map(&self, items: Vec<Value>) -> ReadResult<Value> {
        if !items.len().is_multiple_of(2) {
            return Err(self.syntax("Map literal must contain an even number of forms"));
        }
        let mut m = map::empty_map();
        for kv in items.chunks(2) {
            if m.contains_key(&kv[0]) {
                return Err(
                    self.syntax(format!("Duplicate key: {}", crate::printer::pr_str(&kv[0])))
                );
            }
            m.insert(kv[0], kv[1]);
        }
        // The source order of the entries, for the compiler and syntax-quote.
        let keys: Vec<Value> = items.iter().step_by(2).cloned().collect();
        Ok(map::new_ordered(m, keys))
    }

    fn read_token(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if is_terminator(c) {
                break;
            }
            self.bump();
        }
        self.src[start..self.pos].to_string()
    }

    fn read_string(&mut self) -> ReadResult<Value> {
        let mut s = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(self.eof("string"));
            };
            match c {
                '"' => return Ok(string::from_string(s)),
                '\\' => {
                    let Some(e) = self.bump() else {
                        return Err(self.eof("string escape"));
                    };
                    match e {
                        't' => s.push('\t'),
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        'b' => s.push('\u{8}'),
                        'f' => s.push('\u{c}'),
                        '"' => s.push('"'),
                        '\\' => s.push('\\'),
                        '\'' => s.push('\''),
                        'u' => s.push(self.read_unicode_escape(4)?),
                        '0'..='7' => {
                            let mut n = e.to_digit(8).unwrap();
                            for _ in 0..2 {
                                match self.peek().and_then(|c| c.to_digit(8)) {
                                    Some(d) => {
                                        self.bump();
                                        n = n * 8 + d;
                                    }
                                    None => break,
                                }
                            }
                            if n > 0o377 {
                                return Err(
                                    self.syntax("Octal escape sequence must be in range [0, 377]")
                                );
                            }
                            s.push(char::from_u32(n).unwrap());
                        }
                        _ => {
                            return Err(self.syntax(format!("Unsupported escape character: \\{e}")));
                        }
                    }
                }
                _ => s.push(c),
            }
        }
    }

    fn read_unicode_escape(&mut self, digits: usize) -> ReadResult<char> {
        let mut n = 0u32;
        for _ in 0..digits {
            let Some(c) = self.bump() else {
                return Err(self.eof("unicode escape"));
            };
            let Some(d) = c.to_digit(16) else {
                return Err(self.syntax("Invalid unicode escape"));
            };
            n = n * 16 + d;
        }
        char::from_u32(n).ok_or_else(|| self.syntax("Invalid unicode code point"))
    }

    fn read_char(&mut self) -> ReadResult<Value> {
        let Some(first) = self.bump() else {
            return Err(self.eof("character"));
        };
        let mut tok = String::new();
        tok.push(first);
        while let Some(c) = self.peek() {
            if is_terminator(c) {
                break;
            }
            self.bump();
            tok.push(c);
        }
        if tok.chars().count() == 1 {
            return Ok(Value::char(first));
        }
        let c = match tok.as_str() {
            "newline" => '\n',
            "space" => ' ',
            "tab" => '\t',
            "backspace" => '\u{8}',
            "formfeed" => '\u{c}',
            "return" => '\r',
            t if t.starts_with('u') && t.len() == 5 => {
                let n = u32::from_str_radix(&t[1..], 16)
                    .map_err(|_| self.syntax(format!("Invalid character: \\{t}")))?;
                char::from_u32(n).ok_or_else(|| self.syntax("Invalid unicode code point"))?
            }
            t if t.starts_with('o') && t.len() >= 2 && t.len() <= 4 => {
                let n = u32::from_str_radix(&t[1..], 8)
                    .map_err(|_| self.syntax(format!("Invalid octal character: \\{t}")))?;
                if n > 0o377 {
                    return Err(self.syntax("Octal escape sequence must be in range [0, 377]"));
                }
                char::from_u32(n).unwrap()
            }
            t => return Err(self.syntax(format!("Unsupported character: \\{t}"))),
        };
        Ok(Value::char(c))
    }

    fn read_keyword(&mut self) -> ReadResult<Value> {
        let auto = self.peek() == Some(':');
        if auto {
            self.bump();
        }
        let tok = self.read_token();
        if tok.is_empty() {
            return Err(self.syntax("Invalid token: :"));
        }
        if tok.contains("::") || tok.ends_with(':') {
            return Err(self.syntax(format!("Invalid token: :{tok}")));
        }
        let (ns, name) = symbol::split_ns(&tok);
        if !auto {
            return Ok(keyword::intern(ns, name));
        }
        let ns = match ns {
            None => self.env.current_ns(),
            Some(alias) => self.env.resolve_alias(alias).ok_or_else(|| {
                self.syntax(format!(
                    "Invalid token: ::{tok} (no such namespace alias: {alias})"
                ))
            })?,
        };
        Ok(keyword::intern(Some(&ns), name))
    }

    fn read_meta(&mut self) -> ReadResult<Value> {
        let meta = self.read_required("metadata")?;
        let meta = match meta.kind() {
            Some(Kind::Map) => meta,
            Some(Kind::Keyword) => map::from_pairs(&[meta, Value::TRUE]),
            Some(Kind::Symbol) | Some(Kind::String) => {
                map::from_pairs(&[keyword::intern(None, "tag"), meta])
            }
            _ => return Err(self.syntax("Metadata must be Symbol, Keyword, String or Map")),
        };
        let target = self.read_required("form after metadata")?;
        match target.kind() {
            Some(Kind::List | Kind::Vector | Kind::Map | Kind::Set | Kind::Symbol) => {
                let merged = match obj::meta(&target) {
                    Some(existing) if !existing.is_nil() => {
                        let mut m = map::get(&existing).m;
                        for (k, v) in map::get(&meta).m.iter() {
                            m.insert(*k, *v);
                        }
                        map::new(m)
                    }
                    _ => meta,
                };
                obj::set_meta_unshared(&target, merged);
                Ok(target)
            }
            _ => Err(self.syntax("Metadata can only be applied to collections and symbols")),
        }
    }

    fn read_dispatch(&mut self) -> ReadResult<Value> {
        let Some(c) = self.peek() else {
            return Err(self.eof("dispatch"));
        };
        match c {
            '{' => {
                self.bump();
                let items = self.read_seq('}')?;
                let mut s = set::empty_set();
                for x in &items {
                    if s.contains(x) {
                        return Err(
                            self.syntax(format!("Duplicate key: {}", crate::printer::pr_str(x)))
                        );
                    }
                    s.insert(*x);
                }
                Ok(set::new_ordered(s, items))
            }
            '\'' => {
                self.bump();
                let f = self.read_required("var form")?;
                Ok(list::from_slice(&[symbol::new(None, "var"), f]))
            }
            '#' => {
                self.bump();
                let tok = self.read_token();
                match tok.as_str() {
                    "Inf" => Ok(Value::double(f64::INFINITY)),
                    "-Inf" => Ok(Value::double(f64::NEG_INFINITY)),
                    "NaN" => Ok(Value::double(f64::NAN)),
                    _ => Err(self.syntax(format!("Unknown symbolic value: ##{tok}"))),
                }
            }
            '(' => {
                if self.arg_env.is_some() {
                    return Err(self.syntax("Nested #()s are not allowed"));
                }
                self.arg_env = Some(BTreeMap::new());
                self.bump();
                let body = self.read_seq(')');
                let env = self.arg_env.take().unwrap_or_default();
                let body = body?;
                let max_arg = env.keys().copied().filter(|k| *k > 0).max().unwrap_or(0);
                let mut params = Vec::new();
                for i in 1..=max_arg {
                    params.push(env.get(&i).cloned().unwrap_or_else(|| {
                        symbol::new(
                            None,
                            &format!("p{i}__{}#", crate::core::lazy::next_gensym()),
                        )
                    }));
                }
                if let Some(rest) = env.get(&-1) {
                    params.push(symbol::new(None, "&"));
                    params.push(*rest);
                }
                Ok(list::from_slice(&[
                    symbol::new(None, "fn*"),
                    vector::from_slice(&params),
                    list::from_slice(&body),
                ]))
            }
            '"' => {
                self.bump();
                self.read_regex()
            }
            '?' => Err(self.syntax("reader conditionals are not supported")),
            _ => Err(self.syntax(format!("No dispatch macro for: {c}"))),
        }
    }

    fn interpret_token(&mut self, tok: &str) -> ReadResult<Value> {
        match tok {
            "nil" => return Ok(Value::NIL),
            "true" => return Ok(Value::TRUE),
            "false" => return Ok(Value::FALSE),
            _ => {}
        }
        if self.arg_env.is_some() && tok.starts_with('%') {
            return self.fn_arg(tok);
        }
        let first = tok.chars().next().unwrap();
        let second = tok.chars().nth(1);
        if first.is_ascii_digit()
            || ((first == '+' || first == '-') && second.is_some_and(|c| c.is_ascii_digit()))
        {
            return self.parse_number(tok);
        }
        if tok.contains("::") || (tok.ends_with(':') && tok != ":") {
            return Err(self.syntax(format!("Invalid token: {tok}")));
        }
        Ok(symbol::parse(tok))
    }

    /// `%`, `%N` and `%&` inside `#(...)`.
    fn fn_arg(&mut self, tok: &str) -> ReadResult<Value> {
        let idx: i64 = match &tok[1..] {
            "" => 1,
            "&" => -1,
            n => n.parse().map_err(|_| {
                self.syntax(format!("arg literal must be %, %& or %integer: {tok}"))
            })?,
        };
        if idx == 0 || idx > 20 {
            return Err(self.syntax(format!("arg literal must be %, %& or %1..%20: {tok}")));
        }
        let env = self.arg_env.as_mut().unwrap();
        if let Some(s) = env.get(&idx) {
            return Ok(*s);
        }
        let name = if idx == -1 {
            "rest".to_string()
        } else {
            format!("p{idx}")
        };
        let s = symbol::new(
            None,
            &format!("{name}__{}#", crate::core::lazy::next_gensym()),
        );
        env.insert(idx, s);
        Ok(s)
    }

    /// `#"..."`: backslash sequences are kept verbatim for the regex engine.
    fn read_regex(&mut self) -> ReadResult<Value> {
        let mut src = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(self.eof("regex"));
            };
            match c {
                '"' => break,
                '\\' => {
                    src.push('\\');
                    let Some(n) = self.bump() else {
                        return Err(self.eof("regex"));
                    };
                    src.push(n);
                }
                _ => src.push(c),
            }
        }
        obj::regex::new(&src).map_err(|_| {
            let msg = crate::exc::take()
                .map(|e| crate::exc::format_exception(&e))
                .unwrap_or_default();
            self.syntax(msg)
        })
    }

    // ---- syntax-quote -----------------------------------------------------------

    fn gensym_for(&mut self, name: &str) -> Value {
        let base = &name[..name.len() - 1];
        let table = self.gensyms.last_mut().expect("inside syntax-quote");
        if let Some(s) = table.get(name) {
            return *s;
        }
        let s = symbol::new(
            None,
            &format!("{base}__{}__auto__", crate::core::lazy::next_gensym()),
        );
        table.insert(name.to_string(), s);
        s
    }

    /// Clojure's syntax-quote transformation: symbols are qualified (or gensym'd),
    /// `~` and `~@` are spliced, and collections become `seq`/`concat` templates.
    fn syntax_quote(&mut self, form: &Value) -> ReadResult<Value> {
        let quote = |v: Value| list::from_slice(&[symbol::new(None, "quote"), v]);
        match form.kind() {
            Some(Kind::Symbol) => {
                let s = symbol::get(form);
                let resolved = if s.ns().is_some() || is_special(s.name()) {
                    *form
                } else if s.name().ends_with('#') && s.name().len() > 1 {
                    self.gensym_for(s.name())
                } else {
                    let ns = self.env.resolve_symbol_ns(s.name());
                    symbol::new(Some(&ns), s.name())
                };
                Ok(quote(resolved))
            }
            Some(Kind::List) => {
                let items = list::get(form).to_vec();
                if items.is_empty() {
                    return Ok(list::from_slice(&[core_sym("list")]));
                }
                if is_core_call(&items, "unquote") {
                    return Ok(items.get(1).cloned().unwrap_or(Value::NIL));
                }
                if is_core_call(&items, "unquote-splicing") {
                    return Err(self.syntax("splice not in list"));
                }
                let c = self.sq_concat(&items)?;
                Ok(list::from_slice(&[core_sym("seq"), c]))
            }
            Some(Kind::Vector) => {
                let items: Vec<Value> = vector::get(form).iter().cloned().collect();
                let c = self.sq_concat(&items)?;
                Ok(list::from_slice(&[
                    core_sym("apply"),
                    core_sym("vector"),
                    list::from_slice(&[core_sym("seq"), c]),
                ]))
            }
            Some(Kind::Map) => {
                let mut items = Vec::new();
                for (k, v) in map::ordered_entries(form) {
                    items.push(k);
                    items.push(v);
                }
                let c = self.sq_concat(&items)?;
                Ok(list::from_slice(&[
                    core_sym("apply"),
                    core_sym("hash-map"),
                    list::from_slice(&[core_sym("seq"), c]),
                ]))
            }
            Some(Kind::Set) => {
                let items: Vec<Value> = set::ordered_items(form);
                let c = self.sq_concat(&items)?;
                Ok(list::from_slice(&[
                    core_sym("apply"),
                    core_sym("hash-set"),
                    list::from_slice(&[core_sym("seq"), c]),
                ]))
            }
            None | Some(Kind::Keyword | Kind::String | Kind::Long | Kind::Double | Kind::Regex) => {
                Ok(*form)
            }
            _ => Ok(quote(*form)),
        }
    }

    /// `(clojure.core/concat part...)` where each part is `(list x)`, `(list ~x)` or a
    /// spliced `~@x`.
    fn sq_concat(&mut self, items: &[Value]) -> ReadResult<Value> {
        let mut parts = vec![core_sym("concat")];
        for it in items {
            if let Some(l) = list::as_list(it) {
                let v = l.to_vec();
                if is_core_call(&v, "unquote") {
                    parts.push(list::from_slice(&[
                        core_sym("list"),
                        v.get(1).cloned().unwrap_or(Value::NIL),
                    ]));
                    continue;
                }
                if is_core_call(&v, "unquote-splicing") {
                    parts.push(v.get(1).cloned().unwrap_or(Value::NIL));
                    continue;
                }
            }
            let q = self.syntax_quote(it)?;
            parts.push(list::from_slice(&[core_sym("list"), q]));
        }
        Ok(list::from_slice(&parts))
    }

    fn parse_number(&self, tok: &str) -> ReadResult<Value> {
        let bad = || self.syntax(format!("Invalid number: {tok}"));
        let (neg, body) = match tok.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, tok.strip_prefix('+').unwrap_or(tok)),
        };
        // `36rZN`: in a radix literal a trailing `N` is a digit, never the suffix.
        let radix_at = body.find(['r', 'R']).filter(|&i| {
            !body.starts_with("0x")
                && !body.starts_with("0X")
                && i > 0
                && body[..i].chars().all(|c| c.is_ascii_digit())
        });
        let (body, force_big) = match body.strip_suffix('N') {
            Some(b) if radix_at.is_none() => (b, true),
            _ => (body, false),
        };
        if radix_at.is_none() && body.ends_with('M') {
            return Err(self.syntax(format!("BigDecimal literals are not supported yet: {tok}")));
        }
        if body.contains('/') {
            return Err(self.syntax(format!("Ratio literals are not supported yet: {tok}")));
        }
        // An integer literal: `N` forces a BigInt, and one that does not fit in 64 bits
        // becomes a BigInt by itself, as in Clojure.
        let int = |digits: &str, radix: u32| -> ReadResult<Value> {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return Err(bad());
            }
            let big = num_bigint::BigInt::parse_bytes(digits.as_bytes(), radix).ok_or_else(bad)?;
            let big = if neg { -big } else { big };
            if !force_big && let Some(n) = num_traits::ToPrimitive::to_i64(&big) {
                return Ok(Value::int(n));
            }
            Ok(crate::obj::bigint::new(big))
        };
        if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            return int(hex, 16);
        }
        if let Some(i) = radix_at {
            let radix: u32 = body[..i].parse().map_err(|_| bad())?;
            if !(2..=36).contains(&radix) {
                return Err(self.syntax(format!("Radix out of range: {tok}")));
            }
            return int(&body[i + 1..], radix);
        }
        let is_float = !force_big && body.contains(['.', 'e', 'E']);
        if is_float {
            let d: f64 = body.parse().map_err(|_| bad())?;
            return Ok(Value::double(if neg { -d } else { d }));
        }
        if body.len() > 1 && body.starts_with('0') {
            return int(&body[1..], 8);
        }
        int(body, 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::printer::pr_str;

    fn rt(s: &str) -> String {
        // Syntax-quote resolves symbols against the registered core vars.
        crate::entry::init();
        pr_str(&read_str(s).unwrap())
    }

    #[test]
    fn round_trips() {
        for s in [
            "nil",
            "true",
            "false",
            "42",
            "-7",
            "3.5",
            "1.0E20",
            "\"hi\\n\"",
            "\\a",
            "\\newline",
            ":k",
            ":ns/k",
            "sym",
            "ns/sym",
            "(f 1 2)",
            "[1 [2 3]]",
            "{:a 1}",
            "#{1}",
            "()",
            "[]",
            "{}",
            "(quote x)",
            "/",
            "clojure.core//",
        ] {
            assert_eq!(rt(s), s, "round trip of {s}");
        }
    }

    #[test]
    fn numbers() {
        assert_eq!(rt("0x1F"), "31");
        assert_eq!(rt("2r1010"), "10");
        assert_eq!(rt("017"), "15");
        assert_eq!(rt("1e3"), "1000.0");
        assert_eq!(rt("+5"), "5");
        assert_eq!(rt("9223372036854775807"), "9223372036854775807");
        assert_eq!(rt("-9223372036854775808"), "-9223372036854775808");
        assert_eq!(rt("9223372036854775808"), "9223372036854775808N");
        assert_eq!(rt("1N"), "1N");
        assert_eq!(rt("-0xffN"), "-255N");
        assert_eq!(
            rt("123456789012345678901234567890"),
            "123456789012345678901234567890N"
        );
        assert!(read_str("1.5N").is_err());
        assert_eq!(rt("##Inf"), "##Inf");
    }

    #[test]
    fn macros_and_meta() {
        assert_eq!(rt("'x"), "(quote x)");
        assert_eq!(rt("@a"), "(clojure.core/deref a)");
        assert_eq!(rt("#'a"), "(var a)");
        assert_eq!(rt("[1 #_2 3]"), "[1 3]");
        assert_eq!(rt("(a ; comment\n b)"), "(a b)");
        assert_eq!(rt("[1,2]"), "[1 2]");
        let v = read_str("^:k [1]").unwrap();
        assert_eq!(pr_str(&obj::meta(&v).unwrap()), "{:k true}");
        let v = read_str("^{:a 1} ^:b x").unwrap();
        let m = pr_str(&obj::meta(&v).unwrap());
        assert!(m.contains(":a 1") && m.contains(":b true"), "{m}");
    }

    #[test]
    fn errors() {
        let e = read_str("(1 2").unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Eof);
        let e = read_str("\"abc").unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Eof);
        let e = read_str(")").unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Syntax);
        assert!(read_str("{:a}").is_err());
        assert!(read_str("{:a 1 :a 2}").is_err());
        assert!(read_str("~x").is_ok());
        assert_eq!(
            rt("`x"),
            "(clojure.core/seq (clojure.core/concat (clojure.core/list (quote user/x))))".replace(
                "(clojure.core/seq (clojure.core/concat (clojure.core/list (quote user/x))))",
                "(quote user/x)"
            )
        );
        assert_eq!(
            rt("`(a ~b ~@c)"),
            "(clojure.core/seq (clojure.core/concat (clojure.core/list (quote user/a)) (clojure.core/list b) c))"
        );
        assert_eq!(rt("`inc"), "(quote clojure.core/inc)");
        assert_eq!(rt("`if"), "(quote if)");
        assert_eq!(
            rt("`[1 ~x]"),
            "(clojure.core/apply clojure.core/vector (clojure.core/seq (clojure.core/concat (clojure.core/list 1) (clojure.core/list x))))"
        );
        let g = rt("`(x# x#)");
        assert!(g.contains("__auto__"), "{g}");
        let a = rt("#(+ % %2)");
        assert!(a.starts_with("(fn* [p1__") && a.contains("p2__"), "{a}");
        assert_eq!(rt("#\"a\\d+\""), "#\"a\\d+\"");
    }

    #[test]
    fn positions_and_multiple_forms() {
        let mut r = Reader::new("(a)\n  [b]", &DefaultEnv).with_file(pos::intern_file("<test>"));
        let mut forms = Vec::new();
        while let Some(f) = r.read().unwrap() {
            forms.push(f);
        }
        assert_eq!(forms.len(), 2);
        let p = pos::pos_of(&forms[1]).unwrap();
        assert_eq!((p.line, p.col), (2, 3));
        let a = *list::get(&forms[0]).iter().next().unwrap();
        assert_eq!(pos::pos_of(&a).map(|p| (p.line, p.col)), Some((1, 2)));
    }

    #[test]
    fn auto_keywords() {
        assert_eq!(rt("::k"), ":user/k");
    }
}
