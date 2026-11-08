//! The REPL over a pipe: multi-line forms, redefinition, error recovery.
use std::io::Write;
use std::process::{Command, Stdio};

fn repl(input: &str) -> (String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rscl"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rscl");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn state_persists_and_multiline_forms_work() {
    let (out, _err) = repl("(def a 41)\n(defn f [x]\n  (+ x 1))\n(f a)\n");
    assert_eq!(out, "#'user/a\n#'user/f\n42\n");
}

#[test]
fn errors_do_not_end_the_session() {
    let (out, err) = repl("(+ 1 nil)\n(f 1)\n)\n(+ 1 2)\n");
    assert_eq!(out, "3\n");
    assert!(err.contains("ClassCastException"), "{err}");
    assert!(err.contains("Unable to resolve symbol: f"), "{err}");
    assert!(err.contains("Unmatched delimiter"), "{err}");
}

#[test]
fn redefinition_is_visible_to_callers() {
    let (out, _) = repl("(defn g [] 1)\n(defn h [] (g))\n(h)\n(defn g [] 2)\n(h)\n");
    assert_eq!(out, "#'user/g\n#'user/h\n1\n#'user/g\n2\n");
}
