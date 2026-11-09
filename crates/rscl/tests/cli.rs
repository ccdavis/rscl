//! The command line in both modes: stack traces with source lines, and the heap
//! limit turning runaway allocation into a catchable `OutOfMemoryError`.
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::target_dir;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rscl(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rscl"));
    cmd.args(args).env("RSCL_NATIVE_PATH", target_dir());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run rscl")
}

/// Run `file` with `rscl run` and as a built binary; both outputs.
fn both_modes(file: &Path, env: &[(&str, &str)]) -> Vec<(&'static str, Output)> {
    let jit = rscl(&["run", file.to_str().unwrap()], env);
    let exe = std::env::temp_dir().join(format!(
        "rscl_cli_{}_{}",
        std::process::id(),
        file.file_stem().unwrap().to_string_lossy()
    ));
    let built = rscl(
        &["build", file.to_str().unwrap(), "-o", exe.to_str().unwrap()],
        env,
    );
    // Top-level forms run at build time, so a program that throws fails there: the
    // build's output is then the AOT outcome (as in the corpus harness).
    if !built.status.success() {
        return vec![("jit", jit), ("aot (build)", built)];
    }
    let mut cmd = Command::new(&exe);
    cmd.env("RSCL_NATIVE_PATH", target_dir());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let aot = cmd.output().expect("run built binary");
    let _ = std::fs::remove_file(&exe);
    vec![("jit", jit), ("aot", aot)]
}

#[test]
fn uncaught_exceptions_report_fn_names_and_source_lines() {
    let file = root().join("tests/corpus/error_trace.clj");
    for (mode, out) in both_modes(&file, &[]) {
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{mode}: {err}");
        assert!(
            err.contains("ArithmeticException: Divide by zero"),
            "{mode}: {err}"
        );
        for (f, line) in [("inner", 2), ("middle", 3), ("outer", 4)] {
            let want = format!("    at user/{f} ({}:{line})", file.display());
            assert!(err.contains(&want), "{mode}: missing {want:?} in {err}");
        }
    }
}

#[test]
fn heap_limit_raises_a_catchable_out_of_memory_error() {
    let dir = std::env::temp_dir().join(format!("rscl_cli_oom_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("oom.clj");
    std::fs::write(
        &file,
        r#"(defn fill [n]
  (loop [i 0 acc []]
    (if (< i n) (recur (inc i) (conj acc (str "item-" i))) acc)))
(println (try (count (fill 100000000))
              (catch OutOfMemoryError e (subs (ex-message e) 0 19))))
(def f (future (count (fill 100000000))))
(println (try @f (catch Exception e (subs (ex-message e) 0 19))))
;; The heap is usable again after the error.
(println (count (fill 1000)))
"#,
    )
    .unwrap();
    for (mode, out) in both_modes(&file, &[("RSCL_MAX_HEAP_MB", "64")]) {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{mode}: {err}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "heap limit exceeded\nheap limit exceeded\n1000\n",
            "{mode}: {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
