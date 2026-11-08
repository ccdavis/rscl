//! Files, stdin, environment, the reader as a function, and native library loading.
use std::io::{BufRead, Write};

use crate::builtins;
use crate::core::args;
use crate::exc::err;
use crate::obj::exception::{ILLEGAL_ARGUMENT, RUNTIME};
use crate::obj::string;
use crate::value::Value;

builtins! {
    "slurp" => fn cf_slurp(args) [1] {
        let path = args::string(&args[0], "path")?;
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(string::from_string(s)),
            Err(e) => err(RUNTIME, format!("{path}: {e}")),
        }
    }
    "spit" => fn cf_spit(args) [2] {
        let path = args::string(&args[0], "path")?;
        let content = crate::printer::print_str(&args[1]);
        match std::fs::write(path, content) {
            Ok(()) => Ok(Value::NIL),
            Err(e) => err(RUNTIME, format!("{path}: {e}")),
        }
    }
    "read-line" => fn cf_read_line(args) [0] {
        let _ = args;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) => Ok(Value::NIL),
            Ok(_) => {
                if line.ends_with('\n') { line.pop(); if line.ends_with('\r') { line.pop(); } }
                Ok(string::from_string(line))
            }
            Err(e) => err(RUNTIME, format!("read-line: {e}")),
        }
    }
    "read-string" => fn cf_read_string(args) [1] {
        let s = args::string(&args[0], "s")?;
        match crate::reader::read_str(s) {
            Ok(v) => Ok(v),
            Err(e) => err(RUNTIME, format!("read-string: {e}")),
        }
    }
    "getenv" => fn cf_getenv(args) [0, 1] {
        if args.is_empty() {
            let mut m = crate::obj::map::empty_map();
            for (k, v) in std::env::vars() { m.insert(string::from_string(k), string::from_string(v)); }
            return Ok(crate::obj::map::new(m));
        }
        Ok(std::env::var(args::string(&args[0], "name")?).map_or(Value::NIL, string::from_string))
    }
    "exit" => fn cf_exit(args) [0, 1] {
        let code = if args.is_empty() { 0 } else { args::int(&args[0], "status")? as i32 };
        let _ = std::io::stdout().flush();
        std::process::exit(code)
    }
    "load-native" => fn cf_load_native(args) [1] {
        let path = args::string(&args[0], "path")?;
        crate::native::load_library(path).map(|_| Value::NIL)
    }
    "native-symbol?" => fn cf_native_symbol_p(args) [1] {
        Ok(Value::bool(!crate::native::lookup(args::string(&args[0], "name")?).is_null()))
    }
    "nano-time" => fn cf_nano_time(args) [0] {
        let _ = args;
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let start = START.get_or_init(std::time::Instant::now);
        Ok(Value::int(start.elapsed().as_nanos() as i64))
    }
    "sleep" => fn cf_sleep(args) [1] {
        let ms = args::int(&args[0], "millis")?;
        if ms < 0 { return err(ILLEGAL_ARGUMENT, "sleep requires a non-negative duration"); }
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
        Ok(Value::NIL)
    }
}
