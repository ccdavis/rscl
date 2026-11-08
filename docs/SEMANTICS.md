# rscl semantics

Decisions that pin behaviour, each covered by tests under `tests/corpus` or in the
crates' unit tests. When the code and this document disagree, fix one of them.

## Values

- `nil` and `false` are the only falsy values.
- Integers are 64-bit. Fixnums cover ±2^62; larger values are boxed transparently.
  Arithmetic that overflows 64 bits throws `ArithmeticException` (Clojure's unprimed
  operators). BigInt (`N` literals, `+'`) is not supported yet.
- `/` on integers that divide exactly gives an integer; otherwise it gives a double
  (**divergence**: Clojure gives a Ratio). Ratio literals are rejected by the reader.
- `(= 1 1.0)` is false; `(== 1 1.0)` is true. `=` on collections is structural;
  lists and vectors are equal when their elements are. Fns, vars and atoms compare by
  identity.
- Strings count Unicode scalar values, not UTF-16 units. `(str nil)` is `""`.
- Keywords are interned and immortal (the table only grows). Symbols are not interned.
- Doubles print like Java (`1.0`, `1.0E7`, `1.5E-7`, `##Inf`, `##NaN`).

## Functions and evaluation

- A fn is called with the uniform convention `entry(fn, argc, argv)`. Arity is checked
  by the callee; mismatches throw `ArityException`.
- Calling a non-fn: keywords, maps, sets and vectors are callable as in Clojure; vars
  call their root; anything else throws `ClassCastException`.
- A variadic fn's rest argument is `nil` when no extra arguments are passed.
- `recur` is allowed only in tail position, must match the target's arity, and may not
  cross `try`.
- `def` interns the var before its init is compiled, so a fn body may refer to its own
  name. Unresolved symbols are compile errors.
- A top-level `(do ...)` is compiled one sub-form at a time.
- Redefinition (`def` again) is visible to every caller immediately: calls go through
  the var's root.

## Exceptions

- Thrown values propagate as an `EXC` sentinel return checked after every call; the
  runtime never unwinds across generated code. Uncaught exceptions print
  `Uncaught exception: Class: message` on stderr and exit with status 1.
- Exception classes are a small built-in taxonomy standing in for JVM classes:
  `ExceptionInfo`, `ArithmeticException`, `ClassCastException`, `ArityException`,
  `IllegalArgumentException`, `IllegalStateException`, `IndexOutOfBoundsException`,
  `UnsupportedOperationException`, `StackOverflowError`, `InterruptedException`,
  `RuntimeException`. `Throwable`, `Exception` and `Error` catch everything.

## Macros and sequences

- `defmacro` fns receive `&form` and `&env` (always `nil`) before their declared params.
  Macros are expanded by the analyzer by calling the macro fn at compile time; an
  exception inside a macro is reported as a compile error at the call site. Special
  forms win over macros; a local shadows a macro of the same name.
- Syntax-quote qualifies unqualified symbols with the namespace of the var they resolve
  to (else the current namespace); `x#` makes one gensym per syntax-quote.
- Lazy seqs are unchunked: `(map f coll)` realises one element at a time. `rest` of a
  cons cell returns its tail unrealised. Printing, equality, `count` and `hash` realise
  the whole seq. A lazy seq whose body throws stays unrealised and can be retried.
- `(range 0)` and other empty lazy seqs are `()`, not `nil`; `(seq ())` is `nil`.
- Sequential collections of every kind (list, vector, cons, lazy seq) are `=` when
  their elements are, and hash alike.
- `letfn` and transducers are not implemented yet.

## Exceptions, continued

- `catch` clauses match in order; `catch Throwable`/`Exception`/`Error` catch
  everything, including non-exception values thrown with `throw`. An exception thrown
  inside a `catch` body is not seen by sibling clauses. `finally` runs on both paths; if
  it throws, its exception replaces the pending one.
- Deep recursion raises `StackOverflowError` (the guard keeps a 1 MB margin below the
  thread's stack bounds, discovered with pthreads). Ctrl-C in the REPL raises
  `InterruptedException` at the next fn entry or loop iteration.

## Native FFI

- `(defnative name :ret [^type params...] :symbol "cname")` declares a C function. The
  symbol is resolved with `dlsym` over the whole process when the defining form is
  initialised, so `load-native` (an `RTLD_GLOBAL` `dlopen`) must precede it; a missing
  symbol is a `RuntimeException`. `:int` is a C `int`, `:long` a 64-bit integer,
  `:str` a `const char*` (strings are copied in and out), `:ptr` a raw address as an
  integer, `:bool` a C `_Bool`.

## Memory

- Heap objects are reference counted (atomic). Compile-time constants are immortal.
- Cycles leak. The known sources are `letfn` (mutually recursive closures) and an
  atom that holds a closure referring back to the atom.

## Programs

- `rscl run FILE` and an AOT-built binary behave identically: every top-level form
  runs in order, then `-main` is called with the command-line arguments if it is
  bound. Exit status: 0 ok, 1 uncaught exception, 2 compile error, 3 reader error.
- `build` evaluates every top-level form at build time in a JIT as it compiles it
  (Clojure AOT semantics), so macros and `def`s earlier in the file are available to
  later forms; side effects therefore run at build time and again at program start.
  `--no-eval` turns that off (only `core.clj` macros are then usable). `core.clj` is
  compiled into every binary.
- AOT binaries have no `eval`.
