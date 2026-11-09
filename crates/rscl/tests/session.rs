//! Session-level behaviour: `require` through the loader hook, `load-file`, reload,
//! built-in namespaces, and macros that load code during expansion.
use std::path::PathBuf;

mod common;

use rscl::session;
use rscl_runtime::printer::pr_str;

/// The session and the namespaces are process-global: run these serially.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .canonicalize()
        .unwrap()
}

fn start() {
    if !session::is_active() {
        session::start(session::SessionConfig {
            paths: vec![corpus_dir()],
            aot: None,
            no_eval: false,
        })
        .expect("session");
    }
}

fn eval(src: &str) -> String {
    match session::eval_source(src, "<test>") {
        Ok(v) => pr_str(&v),
        Err(rscl::jit::EvalError::Exception(e)) => {
            format!("EXC {}", rscl_runtime::exc::format_exception(&e))
        }
        Err(e) => format!("ERR {e}"),
    }
}

#[test]
fn builtin_namespaces_can_be_required() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    start();
    assert_eq!(
        eval("(require '[clojure.string :as s]) (s/upper-case \"ok\")"),
        "\"OK\""
    );
    assert_eq!(
        eval("(ns t.builtin (:require [clojure.string :as str :refer [join]])) (join \",\" [1 2])"),
        "\"1,2\""
    );
    eval("(in-ns 'user)");
}

#[test]
fn require_loads_from_the_source_path_once() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    start();
    assert_eq!(eval("(require '[mylib.util :as u]) (u/double-it 4)"), "8");
    assert_eq!(eval("(contains? (loaded-libs) 'mylib.strings)"), "true");
    assert!(eval("(require 'nope.missing)").contains("Could not locate nope/missing.clj"));
}

#[test]
fn load_file_reevaluates_every_time() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    start();
    let path = corpus_dir().join("mylib/counter.clj");
    std::fs::write(&path, "(ns mylib.counter)\n(def loads (atom 0))\n").unwrap();
    let path_str = path.to_string_lossy().into_owned();
    eval("(def load-count (atom 0))");
    std::fs::write(&path, "(ns mylib.counter)\n(swap! user/load-count inc)\n").unwrap();
    eval(&format!("(load-file \"{path_str}\")"));
    eval(&format!("(load-file \"{path_str}\")"));
    assert_eq!(eval("@load-count"), "2");
    eval("(require 'mylib.counter)"); // already loaded via load-file? no: different key, loads once more
    assert_eq!(eval("@load-count"), "3");
    eval("(require 'mylib.counter :reload)");
    assert_eq!(eval("@load-count"), "4");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn macros_may_require_during_expansion() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    start();
    assert_eq!(
        eval(
            "(defmacro needs-strings [] (require 'mylib.strings) `(mylib.strings/wrap \"m\")) (defn uses [] (needs-strings)) (uses)"
        ),
        "\"<m>\""
    );
}

#[test]
fn native_library_reload_rebinds_vars() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    start();
    let lib = common::target_dir().join("librscl_demo.so");
    let copy = std::env::temp_dir().join(format!("rscl_reload_{}.so", std::process::id()));
    std::fs::copy(&lib, &copy).expect("copy demo library");
    let path = copy.to_string_lossy().into_owned();
    assert_eq!(
        eval(&format!("(load-native \"{path}\" :as rl) (rl/add 2 3)")),
        "5"
    );
    eval("(def first-add rl/add)");
    eval("(def old-counter (rl/make-counter 0))");
    // A rebuilt library at the same path is a new file: load it again and the vars
    // point at the new copy's functions.
    let _ = std::fs::remove_file(&copy);
    std::fs::copy(&lib, &copy).expect("copy demo library again");
    assert_eq!(eval(&format!("(load-native \"{path}\") (rl/add 4 5)")), "9");
    assert_eq!(eval("(identical? first-add rl/add)"), "false");
    assert_eq!(eval("(first-add 1 1)"), "2"); // the old copy stays mapped
    // A native object belongs to the library instance that made it: the new copy
    // must refuse it rather than reinterpret the old allocation.
    let rejected = eval("(rl/counter-inc old-counter)");
    assert!(
        rejected.starts_with("EXC ClassCastException"),
        "got: {rejected}"
    );
    assert_eq!(eval("(rl/counter-inc (rl/make-counter 5))"), "6");
    let _ = std::fs::remove_file(&copy);
}
