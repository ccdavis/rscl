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
  `Uncaught exception: Class: message` on stderr, followed by one `    at ns/fn` line per
  rscl fn on the stack when the exception was first thrown (found by walking frame
  pointers; Rust frames in between are omitted), and exit with status 1.
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

## Namespaces

- `(ns name (:require ...))` expands to `in-ns` plus `require` calls; `:refer-clojure`,
  `:import` and `:gen-class` clauses are accepted and ignored. Every namespace sees
  `clojure.core` without referring it; defining a core name locally shadows it.
- `require` loads `foo/bar.clj` (dashes become underscores) from the source path once
  per process (`:reload` loads again); a cycle is an error. Loading a lib never changes
  the caller's current namespace. In a built binary the required namespaces are
  compiled in and `require` runs the compiled copy; `load-file` likewise works for
  files loaded at build time, and re-evaluates every time it is called.
- Namespaces provided by the runtime (`clojure.core`, `clojure.string`) are always
  loaded; a macro may call `require` while it expands.
- `*ns*` is the current namespace object; `(in-ns 'x)` creates and switches to it.
- Private vars (`defn-`) are not enforced yet.

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

## Native bridge libraries

- `(load-native path)` opens a shared library (as given, then under each
  `RSCL_NATIVE_PATH` directory, then next to the rscl executable). If it exports
  `rscl_module_register`, its functions become vars in the namespace it names;
  `(load-native path :as alias)` also aliases that namespace. Loading the same path
  again (after a rebuild) maps a private copy of the file and registers it, so the
  vars point at the new code; the old code stays mapped for values that still refer
  to it.
- Exported fn names have underscores turned into hyphens. Arity is fixed; wrong
  arity or an unconvertible argument throws `ArityException`/`ClassCastException`;
  `Err(msg)` throws `RuntimeException`; a Rust panic throws `RuntimeException` too,
  which requires the default `panic = "unwind"` (the `exports!` macro refuses to
  compile under `panic = "abort"`). `Vec<T>` and `HashMap<String, T>` parameters
  accept `nil` as empty; a returned `HashMap` has string keys.
- Native objects (`Native<T>`) print as `#object[type 0x...]`, compare by identity,
  and are dropped (running `T`'s destructor in the library) when unreferenced. They
  belong to the library instance that created them: after a reload, a handle made by
  the old copy is a `ClassCastException` to the new one, never a reinterpretation.
- A plain C library (no `rscl_module_register`) is opened globally so its symbols are
  visible to `defnative`, to other libraries and to `dlsym`; bridge libraries stay
  local so reloaded copies cannot clash.
- A compiled program loads bridge libraries at start-up through the same
  `load-native` call; static linking of bridge libraries is not supported yet.

## Types, protocols and multimethods

- `(deftype Name [fields] Protocol (method [this ...] ...) ...)` defines a type whose
  instances compare by identity and print as `#object[ns.Name 0x...]`. Fields are
  read with `(.-field x)`; inside a method body the fields are locals (a parameter of
  the same name shadows a field). `(Name. args)` and `(->Name args)` construct.
- `(defrecord Name [fields] ...)` instances are maps: keyword lookup, `get`, `assoc`,
  `dissoc`, `keys`, `vals`, `seq`, `count`, `into`, `=` and `hash` by value (two
  records are equal only when they have the same type, fields and extra keys; a
  record never equals a plain map). Extra keys live beside the declared fields;
  `dissoc` of a declared field yields a plain map. Records are not callable. They
  print as `#ns.Name{:field value, ...}`. `map->Name` builds from a map.
- `(type x)` / `(class x)` return a type object (`Long`, `String`, `user.Point`,
  ...); `(type nil)` is `nil`. Built-in types are vars in `clojure.core`: `Object`
  `Boolean` `Character` `Long` `Double` `String` `Symbol` `Keyword` `PersistentList`
  `PersistentVector` `PersistentHashMap` `PersistentHashSet` `PersistentTreeMap`
  `PersistentTreeSet` `Fn` `Var` `Atom` `LazySeq` `Cons` `Pattern` `Exception`
  `Namespace` `Native` `Type` `Protocol`. There is no class hierarchy: `instance?`
  is exact, except that every non-nil value is an `Object`.
- `defprotocol` creates a protocol and one dispatching fn per method; dispatch is on
  the type of the first argument. `extend-type`, `extend-protocol` and `extend` add
  implementations for any type, including `nil` and `Object` (the fallback for
  types without their own implementation). A missing implementation throws
  `IllegalArgumentException`. Implementations are looked up at call time, so a later
  `extend-type` affects existing fns and instances.
- `Printable` is the printing hook: a type that extends it (`(-pr this readably?)`
  returns a string) controls how `pr`, `prn`, `print`, `println` and `str` render its
  instances. There is no `print-method` multimethod.
- `defmulti` builds a multimethod from a dispatch fn and an optional `:default`
  dispatch value (`:default` by default); `defmethod` adds a method, `remove-method`,
  `remove-all-methods`, `methods` and `get-method` work as in Clojure. Dispatch values
  compare with `=`. There are no hierarchies (`derive`, `isa?`). As in Clojure, a
  second `defmulti` for an existing multimethod is ignored.
- Redefining a type at the REPL creates a fresh type; existing instances keep the old
  one, and protocol/multimethod implementations registered for the old type stay with
  it.

## Metadata and dynamic vars

- `def` metadata comes from the symbol's metadata (`^:dynamic`, `^:private`,
  `^{:doc ...}`) and the docstring; it is literal data, not evaluated. `defn` and
  `defmacro` add `:doc`, `:arglists` and the attr map. `(doc name)` prints it.
- `with-meta`/`vary-meta` work on symbols, lists, vectors, maps, sets, sorted
  collections and record/type instances; `alter-meta!`/`reset-meta!` on vars and atoms.
- A var declared `^:dynamic` can be rebound with `binding` on the current thread for
  the dynamic extent of the body (also across calls into other fns); `set!` replaces
  the innermost binding. Binding a non-dynamic var, or `set!` outside a binding,
  throws `IllegalStateException`. A lazy seq realised after the `binding` form exits
  sees the root value, as in Clojure. Bindings are per thread and not inherited by
  threads started inside the body.

## Sorted collections

- `sorted-map` and `sorted-set` order keys with `compare`; the key types in one
  collection must be mutually comparable (`ClassCastException` otherwise). They
  behave as maps/sets for `get`, `assoc`, `dissoc`, `conj`, `disj`, `contains?`,
  `seq`, `keys`, `vals`, `rseq`, `subseq`, `=` and `hash` (equal to the unsorted
  collection with the same contents) and are callable. `sorted-map-by` and
  `sorted-set-by` (custom comparators) are not supported yet.

## Primitives and performance

- Locals have a static type: a tagged value, or an unboxed `long`/`double`. A `let`
  or `loop` local whose initialiser is a numeric literal or arithmetic on typed
  operands is primitive; `^long`/`^double` on a binding or fn parameter forces it
  (a non-number, or a double for `^long`, is a `ClassCastException` at bind time).
  A loop local stays primitive only if every `recur` rebinds it with the same
  primitive (a long may widen to a double); otherwise it is an ordinary value.
- Arithmetic on primitives is inlined: `+ - * inc dec rem quot mod` on longs with
  the same overflow and division-by-zero `ArithmeticException`s as the boxed
  operators; `+ - * / inc dec` on doubles; `< <= > >= == zero? pos? neg?` and `=`
  when the operand types are known. Mixing a double with a value operand coerces the
  value as `(double x)` would. Results are boxed where a value is needed (a fixnum
  costs nothing, a double allocates), so the inference never changes a program's
  result or its exceptions, only its speed.
- Calls to `clojure.core` fns implemented in Rust (`conj`, `get`, `str`, ...) whose
  var still holds the runtime's fn are linked directly, like Clojure's
  direct-linking: redefining such a var later does not affect code compiled before,
  and the decision is made per top-level form, so a rebinding inside a form is not
  seen by calls later in that same form (dynamic vars and macros are exempt). User
  fns are always called through their var, so REPL redefinition works; `+ - * inc
  dec < <= > >= = == not= zero? pos? neg?` on two fixnums are inlined with a
  fallback to the fn.
- A `fn` without captures is one immortal object shared by every evaluation of the
  form (**divergence**: Clojure creates a new object per evaluation, so two
  evaluations are not `identical?` there; here they are). A var whose root can never
  be freed (an immediate or an immortal object) is read without locking or
  refcounting.
- `rscl build --opt` compiles with Cranelift's `speed` level and drops unreferenced
  functions at link time. The JIT uses the fast single-pass register allocator.

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
  `--no-eval` turns that off: namespace directives (`ns`, `in-ns`, `require`, ...)
  still run so libs are found and compiled in, but macros defined by the program or
  its required libs cannot be used. `core.clj` is compiled into every binary.
- AOT binaries have no `eval`.
