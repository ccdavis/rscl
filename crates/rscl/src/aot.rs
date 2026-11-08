//! Ahead-of-time compilation: the same codegen into a `cranelift-object` module, an
//! emitted `main`, and a link step against the runtime static library.
//!
//! Until macros exist (Phase 4) `build` is a pure compile: forms are analysed and
//! compiled but not executed at build time.
use std::path::{Path, PathBuf};
use std::process::Command;

use cranelift_codegen::settings::{self, Configurable};
use cranelift_module::default_libcall_names;
use cranelift_object::{ObjectBuilder, ObjectModule};
use rscl_runtime::reader::{DefaultEnv, Reader};
use rscl_runtime::{ns, pos};

use crate::analyzer::Analyzer;
use crate::codegen::{Compiler, UnitIds};
use crate::jit::EvalError;

/// Libraries a Rust `staticlib` needs on Linux (from
/// `cargo rustc -p rscl-runtime --lib -- --print native-static-libs`).
const NATIVE_STATIC_LIBS: &[&str] = &[
    "-lgcc_s",
    "-lutil",
    "-lrt",
    "-lpthread",
    "-lm",
    "-ldl",
    "-lc",
];

pub struct BuildOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub keep_obj: bool,
    pub opt: bool,
    /// Use the host CPU's features instead of baseline x86-64.
    pub native: bool,
    pub link_libs: Vec<String>,
    pub lib_paths: Vec<String>,
}

impl BuildOptions {
    pub fn new(input: &str) -> Self {
        let input = PathBuf::from(input);
        let output = input
            .file_stem()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("a.out"));
        BuildOptions {
            input,
            output,
            keep_obj: false,
            opt: false,
            native: false,
            link_libs: Vec::new(),
            lib_paths: Vec::new(),
        }
    }
}

pub enum BuildError {
    Io(String),
    Eval(EvalError),
    Link(String),
    Internal(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Io(e) => write!(f, "{e}"),
            BuildError::Eval(e) => write!(f, "{e}"),
            BuildError::Link(e) => write!(f, "link failed: {e}"),
            BuildError::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl From<EvalError> for BuildError {
    fn from(e: EvalError) -> Self {
        BuildError::Eval(e)
    }
}

impl From<crate::errors::CompileError> for BuildError {
    fn from(e: crate::errors::CompileError) -> Self {
        BuildError::Eval(EvalError::Compile(e))
    }
}

fn internal(e: impl std::fmt::Display) -> BuildError {
    BuildError::Internal(e.to_string())
}

pub fn build(o: &BuildOptions) -> Result<(), BuildError> {
    rscl_runtime::entry::init();
    let path = o.input.to_string_lossy().into_owned();
    let src = std::fs::read_to_string(&o.input)
        .map_err(|e| BuildError::Io(format!("cannot read {path}: {e}")))?;

    let mut compiler = Compiler::new(object_module(o)?);
    let file_id = pos::intern_file(&path);
    let mut reader = Reader::new(&src, &DefaultEnv).with_file(file_id);
    let mut units: Vec<UnitIds> = Vec::new();
    loop {
        match reader.read() {
            Ok(Some(form)) => {
                for unit_form in crate::analyzer::split_do(&form) {
                    let mut an = Analyzer::new(ns::current());
                    let node = an.analyze_toplevel(&unit_form)?;
                    units.push(compiler.compile_unit(&node, &an)?);
                }
            }
            Ok(None) => break,
            Err(e) => return Err(EvalError::Read(e).into()),
        }
    }
    let entry = compiler.compile_entry(&units, &ns::current().name)?;
    compiler.compile_c_main(entry)?;

    let product = compiler.into_module().finish();
    let bytes = product.emit().map_err(internal)?;
    let obj_path = o.output.with_extension("o");
    std::fs::write(&obj_path, &bytes)
        .map_err(|e| BuildError::Io(format!("cannot write {}: {e}", obj_path.display())))?;

    let result = link(&obj_path, &o.output, o);
    if !o.keep_obj {
        let _ = std::fs::remove_file(&obj_path);
    }
    result
}

fn object_module(o: &BuildOptions) -> Result<ObjectModule, BuildError> {
    let mut flags = settings::builder();
    flags.set("is_pic", "true").map_err(internal)?;
    flags
        .set("opt_level", if o.opt { "speed" } else { "none" })
        .map_err(internal)?;
    let isa_builder = if o.native {
        cranelift_native::builder().map_err(internal)?
    } else {
        cranelift_codegen::isa::lookup_by_name("x86_64-unknown-linux-gnu").map_err(internal)?
    };
    let isa = isa_builder
        .finish(settings::Flags::new(flags))
        .map_err(internal)?;
    let builder =
        ObjectBuilder::new(isa, "rscl_program", default_libcall_names()).map_err(internal)?;
    Ok(ObjectModule::new(builder))
}

/// Locate `librscl_runtime.a`: `$RSCL_RUNTIME_LIB`, else next to the `rscl` binary.
pub fn find_runtime_lib() -> Result<PathBuf, BuildError> {
    if let Some(p) = std::env::var_os("RSCL_RUNTIME_LIB") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(BuildError::Link(format!(
            "RSCL_RUNTIME_LIB={} does not exist",
            p.display()
        )));
    }
    let exe = std::env::current_exe()
        .map_err(|e| BuildError::Link(format!("cannot locate rscl executable: {e}")))?;
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        candidates.push(dir.join("librscl_runtime.a"));
        candidates.push(dir.join("../lib/librscl_runtime.a"));
    }
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(BuildError::Link(format!(
        "cannot find librscl_runtime.a (looked in {}); build it with `cargo build -p rscl-runtime` or set RSCL_RUNTIME_LIB",
        candidates
            .iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

fn link(obj: &Path, out: &Path, o: &BuildOptions) -> Result<(), BuildError> {
    let lib = find_runtime_lib()?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let mut cmd = Command::new(&cc);
    cmd.arg(obj).arg("-o").arg(out);
    // Export symbols so the typed FFI can resolve them with dlsym at start-up, and keep
    // libm/libdl linked even when nothing references them statically.
    cmd.arg("-rdynamic").arg("-Wl,--no-as-needed");
    for p in &o.lib_paths {
        cmd.arg(format!("-L{p}"));
    }
    for l in &o.link_libs {
        cmd.arg(format!("-l{l}"));
    }
    cmd.arg(&lib);
    for l in NATIVE_STATIC_LIBS {
        cmd.arg(l);
    }
    if let Ok(extra) = std::env::var("RSCL_LINK_ARGS") {
        cmd.args(extra.split_whitespace());
    }
    let output = cmd
        .output()
        .map_err(|e| BuildError::Link(format!("cannot run {cc}: {e}")))?;
    if !output.status.success() {
        return Err(BuildError::Link(format!(
            "{cc} exited with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}
