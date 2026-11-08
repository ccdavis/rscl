//! rscl command line: REPL, `run FILE`, `build FILE -o OUT`, `-e EXPR`.
use std::io::Write;

use rscl_runtime::printer::pr_str;
use rscl_runtime::reader::{DefaultEnv, Reader};
use rscl_runtime::{exc, pos};

use rscl::jit::{EvalError, Jit};
use rscl::repl;

pub const EXIT_OK: i32 = 0;
pub const EXIT_EXCEPTION: i32 = 1;
pub const EXIT_COMPILE: i32 = 2;
pub const EXIT_READ: i32 = 3;
pub const EXIT_USAGE: i32 = 2;

const USAGE: &str = "usage: rscl                       start the REPL
       rscl run FILE [ARGS...]    compile FILE with the JIT and run it
       rscl build FILE [-o OUT] [--keep-obj] [--opt] [--native] [--no-eval]
                  [--link LIB] [--lib-path DIR]
                                  compile FILE to a native executable
       rscl -e EXPR               evaluate EXPR and print the result
       rscl --version | --help";

enum Cmd {
    Repl,
    Run { file: String, args: Vec<String> },
    Build(rscl::aot::BuildOptions),
    Eval(String),
}

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    match args.first().map(String::as_str) {
        None => Ok(Cmd::Repl),
        Some("--help" | "-h") => Err(USAGE.to_string()),
        Some("--version" | "-V") => {
            println!("rscl {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(EXIT_OK);
        }
        Some("-e") => match args.get(1) {
            Some(e) => Ok(Cmd::Eval(e.clone())),
            None => Err("-e requires an expression".to_string()),
        },
        Some("run") => match args.get(1) {
            Some(f) => Ok(Cmd::Run {
                file: f.clone(),
                args: args[2..].to_vec(),
            }),
            None => Err("run requires a file".to_string()),
        },
        Some("build") => {
            let file = args.get(1).ok_or("build requires a file")?.clone();
            let mut o = rscl::aot::BuildOptions::new(&file);
            let mut i = 2;
            while i < args.len() {
                let value = |i: usize| {
                    args.get(i + 1)
                        .cloned()
                        .ok_or_else(|| format!("{} requires a value", args[i]))
                };
                match args[i].as_str() {
                    "-o" => {
                        o.output = value(i)?.into();
                        i += 2;
                    }
                    "--keep-obj" => {
                        o.keep_obj = true;
                        i += 1;
                    }
                    "--opt" => {
                        o.opt = true;
                        i += 1;
                    }
                    "--native" => {
                        o.native = true;
                        i += 1;
                    }
                    "--no-eval" => {
                        o.no_eval = true;
                        i += 1;
                    }
                    "--link" => {
                        o.link_libs.push(value(i)?);
                        i += 2;
                    }
                    "--lib-path" => {
                        o.lib_paths.push(value(i)?);
                        i += 2;
                    }
                    other => return Err(format!("unknown build option: {other}")),
                }
            }
            Ok(Cmd::Build(o))
        }
        Some(f) if !f.starts_with('-') => Ok(Cmd::Run {
            file: f.to_string(),
            args: args[1..].to_vec(),
        }),
        Some(other) => Err(format!("unknown option: {other}\n{USAGE}")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse_args(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(EXIT_USAGE);
        }
    };
    let code = run_on_large_stack(move || match cmd {
        Cmd::Repl => repl::run(),
        Cmd::Run { file, args } => run_file(&file, &args),
        Cmd::Eval(expr) => eval_expr(&expr),
        Cmd::Build(opts) => match rscl::aot::build(&opts) {
            Ok(()) => EXIT_OK,
            Err(rscl::aot::BuildError::Eval(e)) => report(&e),
            Err(e) => {
                eprintln!("{e}");
                EXIT_EXCEPTION
            }
        },
    });
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// Everything runs on a thread with a large stack: deep recursion in user code is
/// expected and there is no stack guard yet.
fn run_on_large_stack<F: FnOnce() -> i32 + Send + 'static>(f: F) -> i32 {
    rscl_runtime::entry::run_on_main_thread(f)
}

fn report(e: &EvalError) -> i32 {
    let _ = std::io::stdout().flush();
    match e {
        EvalError::Read(r) => {
            eprintln!(
                "Syntax error reading source at ({}:{}): {}",
                r.line, r.col, r.msg
            );
            EXIT_READ
        }
        EvalError::Compile(c) => {
            eprintln!("Syntax error compiling: {c}");
            EXIT_COMPILE
        }
        EvalError::Exception(x) => {
            eprintln!("Uncaught exception: {}", exc::format_exception(x));
            EXIT_EXCEPTION
        }
    }
}

fn run_file(path: &str, args: &[String]) -> i32 {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return EXIT_EXCEPTION;
        }
    };
    rscl_runtime::entry::init();
    rscl_runtime::entry::set_command_line_args(args);
    let mut jit = match Jit::new() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("cannot start JIT: {e}");
            return EXIT_EXCEPTION;
        }
    };
    let file_id = pos::intern_file(path);
    let mut reader = Reader::new(&src, &DefaultEnv).with_file(file_id);
    loop {
        match reader.read() {
            Ok(Some(form)) => {
                if let Err(e) = jit.eval(&form) {
                    return report(&e);
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!(
                    "Syntax error reading source at ({path}:{}:{}): {}",
                    e.line, e.col, e.msg
                );
                return EXIT_READ;
            }
        }
    }
    // Like an AOT binary: call `-main` with the command-line arguments if it is bound.
    let main_var = rscl_runtime::ns::current().intern("-main");
    unsafe { rscl_runtime::ffi::rt_run_main(main_var as *const _) }
}

fn eval_expr(expr: &str) -> i32 {
    let mut jit = match Jit::new() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("cannot start JIT: {e}");
            return EXIT_EXCEPTION;
        }
    };
    let file_id = pos::intern_file("<expr>");
    let mut reader = Reader::new(expr, &DefaultEnv).with_file(file_id);
    loop {
        match reader.read() {
            Ok(Some(form)) => match jit.eval(&form) {
                Ok(v) => println!("{}", pr_str(&v)),
                Err(e) => return report(&e),
            },
            Ok(None) => return EXIT_OK,
            Err(e) => return report(&EvalError::Read(e)),
        }
    }
}
