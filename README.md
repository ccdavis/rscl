# rscl

A Clojure variant written in Rust, compiled with [Cranelift](https://cranelift.dev).
The same code generator drives a JIT (for the REPL and `rscl run`) and an
ahead-of-time compiler (`rscl build`) that links a standalone native executable.
Where Clojure on the JVM calls Java, rscl calls Rust: the core library is Rust
functions exposed as first-class fns, and later phases add a typed C-ABI FFI and a
bridge for exposing whole Rust crates.

Status: Phases 1–9 of the plan are done (see `docs/MILESTONES.md`): a working REPL
with per-form incremental compilation and redefinition, `rscl run`, `rscl build`
producing native binaries, `defmacro` with syntax-quote, lazy sequences, destructuring,
atoms, regular expressions, a `core.clj` written from scratch on top of the Rust
builtins, namespaces with `ns`/`require`/`in-ns` and multi-file programs (required
namespaces are compiled into built binaries), fn-name stack traces on uncaught
exceptions, `try`/`catch`/`finally`, a typed C FFI (`defnative`), stack-overflow and
Ctrl-C guards, a bridge for exposing Rust crates as rscl namespaces, `deftype`,
`defrecord`, protocols, multimethods, dynamic vars with `binding`, var metadata and
`doc`, sorted maps and sets, unboxed `long`/`double` locals with `^long`/`^double`
hints and inline arithmetic, direct calls to Rust builtins, futures, promises,
delays, `pmap` and `locking`, locals clearing so lazy sequences stream in constant
memory, a cycle collector, weakly interned keywords, BigInt arithmetic (`123N`, `+'`),
source lines in stack traces, a heap limit that turns exhaustion into a catchable
`OutOfMemoryError`, and a differential test corpus that runs every program under both
the JIT and the AOT compiler.

## Build and run

```bash
cargo build                                  # target/debug/rscl + librscl_runtime.a
target/debug/rscl                            # REPL
target/debug/rscl run tests/corpus/fib.clj   # JIT-compile and run a file
target/debug/rscl -e '(+ 1 2)'               # evaluate an expression
target/debug/rscl build prog.clj -o prog     # native executable (needs `cc`)
./prog
```

`build` options: `--opt` (Cranelift `speed`), `--native` (host CPU features instead
of baseline x86-64), `--keep-obj`, `--link LIB`, `--lib-path DIR`, `--no-eval`.
`scripts/bench.sh` times `bench/*.clj` against Python and the JVM (see
`docs/MILESTONES.md`, Phase 8). The
linker needs `librscl_runtime.a`, found next to the `rscl` binary or via
`RSCL_RUNTIME_LIB`. `require` looks for `foo/bar.clj` in the program's directory, the
working directory, each `--path DIR`, and `RSCL_PATH`.

Exit status: 0 ok, 1 uncaught exception, 2 compile error, 3 reader error.
`RSCL_DEBUG_IR=1` dumps the Cranelift IR of every compiled function.
`RSCL_MAX_HEAP_MB` caps the heap (default: half of physical memory; `0` for none);
crossing it throws `OutOfMemoryError` instead of letting the kernel kill the process.
`RSCL_HASH_SEED=random` randomises map hashing for untrusted keys.

## Tests

```bash
cargo test                 # runtime unit tests, in-process JIT leak tests, REPL tests, corpus
scripts/validate.sh        # every tests/corpus/*.clj under JIT and AOT, diffed against tests/expected/
UPDATE_EXPECTED=1 scripts/validate.sh tests/corpus/x.clj   # (re)write an expected file
```

## Calling Rust and C

```clojure
(defnative cos :double [^double x])          ; libm, resolved with dlsym at load time
(defnative strlen :long [^str s])
(defnative c-toupper :int [^int c] :symbol "toupper")
(load-native "target/debug/libmylib.so")     ; then defnative its exported functions
```

Types: `:void :long :int :double :float :bool :str :ptr`. Every core function
(`+`, `conj`, `map`, ...) is itself a Rust function with the same calling convention
as compiled rscl fns, so it is a first-class value with no wrapper.

For whole Rust libraries, the `rscl-bridge` crate turns ordinary Rust functions into
an rscl namespace (see `examples/native-demo`):

```rust
rscl_bridge::exports! {
    ns "demo";
    fn add(a: i64, b: i64) -> i64 { a + b }
    fn greet(name: &str) -> String { format!("Hello, {name}!") }
    fn parse_int(s: &str) -> Result<i64, String> { s.parse().map_err(|e| e.to_string()) }
}
```

Build it as a `cdylib`, then `(load-native "libdemo.so" :as d)` gives `(d/add 1 2)`,
`(d/parse-int "7")` and so on. Supported types: integers, floats, `bool`, `&str`/`String`,
`Option<T>`, `Vec<T>`, `HashMap<String, T>`, raw `Val` (with `.call` for callbacks),
and `Native<T>` for opaque Rust objects. `Err(msg)` and panics become rscl exceptions
(keep the default `panic = "unwind"` in the bridge crate).
The library never links the runtime: it receives a table of function pointers at
registration, so there is exactly one runtime in the process. Rebuild the crate and
call `load-native` again to pick up changes without restarting the REPL.

## Layout

- `crates/rscl-runtime` — values, objects, reader, printer, namespaces, exceptions,
  core builtins, and `core/core.clj` (the part of the standard library written in rscl).
  No Cranelift dependency; also built as a static library that AOT binaries link against.
- `crates/rscl` — analyzer (forms → `Node`), Cranelift codegen (generic over
  `Module`), JIT driver, AOT driver, REPL, CLI.
- `tests/corpus` + `tests/expected` — the differential program corpus.
- `docs/SEMANTICS.md` — decided semantics and divergences from Clojure.
- `docs/MILESTONES.md` — per-phase demos.

## Design in one paragraph

Values are 64-bit tagged words (63-bit fixnums, immediates for nil/booleans/chars,
pointers to refcounted heap objects). Memory is managed by atomic reference counting
with an explicit owned/borrowed discipline in the code generator. Every function uses
one calling convention, `entry(fn, argc, argv) -> value`, so Rust builtins and
compiled fns are interchangeable and `def` can rebind a var at any time. Exceptions
are a sentinel return value checked after every call; nothing unwinds across the
FFI boundary. Compile-time constants live in per-unit data tables filled by a
generated `init` function, so no raw address is ever embedded in IR and the JIT and
AOT paths share one code generator.
