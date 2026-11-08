# rscl

A Clojure variant written in Rust, compiled with [Cranelift](https://cranelift.dev).
The same code generator drives a JIT (for the REPL and `rscl run`) and an
ahead-of-time compiler (`rscl build`) that links a standalone native executable.
Where Clojure on the JVM calls Java, rscl calls Rust: the core library is Rust
functions exposed as first-class fns, and later phases add a typed C-ABI FFI and a
bridge for exposing whole Rust crates.

Status: Phases 1–4 of the plan are done (see `docs/MILESTONES.md`): a working REPL
with per-form incremental compilation and redefinition, `rscl run`, `rscl build`
producing native binaries, `defmacro` with syntax-quote, lazy sequences, destructuring,
atoms, regular expressions, a `core.clj` written from scratch on top of the Rust
builtins, `try`/`catch`/`finally`, a typed C FFI (`defnative`), stack-overflow and
Ctrl-C guards, and a differential test corpus that runs every program under both the
JIT and the AOT compiler. Next: namespaces and multi-file programs (Phase 5).

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
of baseline x86-64), `--keep-obj`, `--link LIB`, `--lib-path DIR`. The linker needs
`librscl_runtime.a`, found next to the `rscl` binary or via `RSCL_RUNTIME_LIB`.

Exit status: 0 ok, 1 uncaught exception, 2 compile error, 3 reader error.
`RSCL_DEBUG_IR=1` dumps the Cranelift IR of every compiled function.

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
