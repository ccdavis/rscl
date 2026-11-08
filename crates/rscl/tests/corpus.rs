//! Differential corpus: every `tests/corpus/*.clj` runs under every mode (JIT now, AOT
//! once `build` exists); stdout must match across modes and `tests/expected/NAME.out`.
//! A program whose name contains `error` must exit non-zero with the same status in
//! every mode; its expected file holds the exit status on the first line.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

struct Outcome {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run_with_timeout(mut cmd: Command) -> Outcome {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().expect("spawn");
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut out, &mut s).ok();
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut err, &mut s).ok();
        s
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().expect("wait") {
            break st.code().unwrap_or(-1);
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break -2;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Outcome {
        status,
        stdout: out_t.join().unwrap(),
        stderr: err_t.join().unwrap(),
    }
}

fn run_jit(file: &Path) -> Outcome {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rscl"));
    cmd.arg("run").arg(file);
    run_with_timeout(cmd)
}

fn run_aot(file: &Path) -> Outcome {
    let name = file.file_stem().unwrap().to_string_lossy().into_owned();
    let exe = std::env::temp_dir().join(format!("rscl_corpus_{}_{name}", std::process::id()));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rscl"));
    cmd.arg("build").arg(file).arg("-o").arg(&exe);
    let built = run_with_timeout(cmd);
    if built.status != 0 {
        return built;
    }
    let o = run_with_timeout(Command::new(&exe));
    let _ = std::fs::remove_file(&exe);
    o
}

type Runner = fn(&Path) -> Outcome;

fn modes() -> Vec<(&'static str, Runner)> {
    vec![("jit", run_jit as Runner), ("aot", run_aot as Runner)]
}

#[test]
fn corpus() {
    let dir = root().join("tests/corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("tests/corpus")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "clj"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no corpus programs in {}", dir.display());
    let mut failures = Vec::new();
    for file in &files {
        let name = file.file_stem().unwrap().to_string_lossy().into_owned();
        let is_error = name.contains("error");
        let expected_path = root().join("tests/expected").join(format!("{name}.out"));
        let expected = std::fs::read_to_string(&expected_path).ok();
        let mut outputs: Vec<(&str, Outcome)> = Vec::new();
        for (mode, run) in modes() {
            outputs.push((mode, run(file)));
        }
        for (mode, o) in &outputs {
            if o.status == -2 {
                failures.push(format!("{name} [{mode}]: timed out"));
            }
            if is_error {
                if o.status == 0 {
                    failures.push(format!("{name} [{mode}]: expected a failure, exited 0"));
                }
            } else if o.status != 0 {
                failures.push(format!(
                    "{name} [{mode}]: exit {}\nstderr:\n{}",
                    o.status, o.stderr
                ));
            }
        }
        // All modes must agree with each other (for error programs only the exit
        // status: a compile error stops the JIT mid-file but `build` prints nothing).
        let first = &outputs[0].1;
        for (mode, o) in &outputs[1..] {
            if (!is_error && o.stdout != first.stdout) || o.status != first.status {
                failures.push(format!(
                    "{name}: {} and {mode} disagree\n--- {}:\n{}\n--- {mode}:\n{}",
                    outputs[0].0, outputs[0].0, first.stdout, o.stdout
                ));
            }
        }
        match expected {
            Some(exp) => {
                let want = if is_error {
                    let mut lines = exp.lines();
                    let code: i32 = lines
                        .next()
                        .and_then(|l| l.trim().parse().ok())
                        .unwrap_or(1);
                    if first.status != code {
                        failures.push(format!(
                            "{name}: expected exit {code}, got {}\nstderr:\n{}",
                            first.status, first.stderr
                        ));
                    }
                    lines.collect::<Vec<_>>().join("\n")
                } else {
                    exp.clone()
                };
                if first.stdout.trim_end() != want.trim_end() {
                    failures.push(format!(
                        "{name}: stdout differs from expected\n--- expected:\n{}\n--- got:\n{}",
                        want, first.stdout
                    ));
                }
            }
            None => failures.push(format!(
                "{name}: missing {} (run scripts/validate.sh with UPDATE_EXPECTED=1)",
                expected_path.display()
            )),
        }
    }
    if !failures.is_empty() {
        panic!(
            "{} corpus failure(s):\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}
