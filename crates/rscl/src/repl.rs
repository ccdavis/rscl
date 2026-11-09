//! The interactive REPL: reads forms (continuing across lines until balanced),
//! compiles each with the JIT, prints results, and keeps going after errors.
use rscl_runtime::printer::pr_str;
use rscl_runtime::reader::{DefaultEnv, ReadErrorKind, Reader};
use rscl_runtime::{exc, ns, pos};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::jit::EvalError;

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

extern "C" fn on_sigint(_: i32) {
    rscl_runtime::native::request_interrupt();
}

/// Shift the REPL history vars: `*3` ← `*2` ← `*1` ← the new result.
fn remember(v: rscl_runtime::Value) {
    let user = ns::current();
    let v2 = user.intern("*2").root();
    user.intern("*3").bind_root(if v2.is_unbound() {
        rscl_runtime::Value::NIL
    } else {
        v2
    });
    let v1 = user.intern("*1").root();
    user.intern("*2").bind_root(if v1.is_unbound() {
        rscl_runtime::Value::NIL
    } else {
        v1
    });
    user.intern("*1").bind_root(v);
}

pub fn run(paths: &[String]) -> i32 {
    // Ctrl-C during an evaluation raises InterruptedException instead of killing the
    // process; at the prompt rustyline reports it as `Interrupted`.
    unsafe { signal(2, on_sigint as *const () as usize) };
    let cfg = crate::session::SessionConfig {
        paths: crate::session::source_paths(None, paths),
        aot: None,
        no_eval: false,
    };
    if let Err(e) = crate::session::start(cfg) {
        eprintln!("cannot start: {e}");
        return 1;
    }
    let mut rl = match DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("cannot start line editor: {e}");
            return 1;
        }
    };
    let history =
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".rscl_history"));
    if let Some(h) = &history {
        let _ = rl.load_history(h);
    }
    eprintln!("rscl {} — Ctrl-D to exit", env!("CARGO_PKG_VERSION"));
    for name in ["*1", "*2", "*3", "*e"] {
        ns::current()
            .intern(name)
            .bind_root(rscl_runtime::Value::NIL);
    }

    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() {
            format!("{}=> ", ns::current().name)
        } else {
            "  #_=> ".to_string()
        };
        rscl_runtime::gc::enter_idle();
        let line = rl.readline(&prompt);
        rscl_runtime::gc::leave_idle();
        match line {
            Ok(line) => {
                buffer.push_str(&line);
                buffer.push('\n');
                let mut forms = Vec::new();
                let mut need_more = false;
                {
                    let mut reader =
                        Reader::new(&buffer, &DefaultEnv).with_file(pos::intern_file("<repl>"));
                    loop {
                        match reader.read() {
                            Ok(Some(f)) => forms.push(f),
                            Ok(None) => break,
                            Err(e) if e.kind == ReadErrorKind::Eof => {
                                need_more = true;
                                break;
                            }
                            Err(e) => {
                                eprintln!(
                                    "Syntax error reading source at ({}:{}): {}",
                                    e.line, e.col, e.msg
                                );
                                forms.clear();
                                break;
                            }
                        }
                    }
                }
                if need_more {
                    continue;
                }
                let _ = rl.add_history_entry(buffer.trim_end());
                buffer.clear();
                for form in forms {
                    match crate::session::eval_form(&form) {
                        Ok(v) => {
                            println!("{}", pr_str(&v));
                            remember(v);
                        }
                        Err(EvalError::Exception(e)) => {
                            eprintln!("Execution error: {}", exc::format_exception_with_trace(&e));
                            ns::current().intern("*e").bind_root(e);
                        }
                        Err(e) => eprintln!("{e}"),
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("{e}");
                break;
            }
        }
    }
    if let Some(h) = &history {
        let _ = rl.save_history(h);
    }
    0
}
