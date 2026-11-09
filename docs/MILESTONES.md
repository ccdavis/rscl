# Milestones

Each phase ends with a demo transcript that must keep working.

## Phase 1 — JIT REPL and file runner (done)

```
$ target/debug/rscl
user=> (defn fib [n] (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))
#'user/fib
user=> (fib 30)
832040
user=> (defn fib [n] :redefined)
#'user/fib
user=> (fib 30)
:redefined
user=> (def x 10) (+ x 1)
#'user/x
11
user=> (+ 1 nil)
Execution error: ClassCastException: nil cannot be cast to Number
user=> ((fn [a] a) 1 2)
Execution error: ArityException: Wrong number of args (2) passed to: user/fn__1
user=> (1 2)
Execution error: ClassCastException: Long cannot be cast to IFn
user=> (let [k 3] (defn addk [x] (+ x k)))
#'user/addk
user=> (addk 4)
7
user=> (loop [i 0 acc 0] (if (< i 1000000) (recur (inc i) (+ acc i)) acc))
499999500000
```

## Phase 2 — AOT build (done)

```
$ target/debug/rscl build tests/corpus/fib.clj -o fib && ./fib
75025
0 1 55
2880067194370816120
$ scripts/validate.sh        # every corpus program agrees under jit and aot
```

## Phase 3 — Core data, exceptions, typed FFI (done)

```
user=> (map inc [1 2 3])
(2 3 4)
user=> (reduce + (filter odd? (range 10)))
25
user=> (:b (assoc {:a 1} :b 2))
2
user=> (clojure.string/join ", " (sort ["pear" "apple"]))
"apple, pear"
user=> (try (/ 1 0) (catch ArithmeticException e (ex-message e)) (finally (println "done")))
done
"Divide by zero"
user=> (defnative cos :double [^double x])
#'user/cos
user=> (cos 0.0)
1.0
user=> (defn deep [n] (+ 1 (deep (inc n))))
#'user/deep
user=> (try (deep 0) (catch StackOverflowError e :caught))
:caught
user=> (loop [] (recur))      ; Ctrl-C
Execution error: InterruptedException: interrupted
```

`scripts/valgrind_check.sh tests/corpus/*.clj` leak-checks every corpus program under both
modes (release build).

## Phase 4 — Macros, lazy seqs, core.clj (done)

```
user=> (defmacro unless [test & body] `(if ~test nil (do ~@body)))
#'user/unless
user=> (macroexpand-1 '(unless x 1 2))
(if x nil (do 1 2))
user=> (take 5 (map #(* % %) (filter odd? (range))))
(1 9 25 49 81)
user=> (let [[a & rest :as all] [1 2 3] {:keys [x] :or {x 9}} {}] [a rest all x])
[1 (2 3) [1 2 3] 9]
user=> (for [x (range 3) y (range 3) :when (< x y)] [x y])
([0 1] [0 2] [1 2])
user=> (def a (atom 0))  (swap! a + 5)  @a
#'user/a
5
5
user=> (re-seq #"\d+" "a1 b22 c333")
("1" "22" "333")
user=> *1
("1" "22" "333")
```

`core.clj` (about 650 lines, written from scratch) is compiled into every session and
into every `rscl build` binary; `build` now also evaluates each top-level form at build
time so macros defined in the program work (`--no-eval` opts out).

## Phase 5 — Namespaces and multi-file programs (done)

```
$ cat mylib/util.clj
(ns mylib.util (:require [mylib.strings :as s]))
(defn shout [x] (str (s/wrap (clojure.string/upper-case x)) "!"))

$ cat main.clj
(ns demo.main (:require [mylib.util :as u :refer [shout]]))
(println (shout "hi") (ns-name *ns*))
(defn -main [& args] (println "args:" args))

$ rscl run main.clj a b             # finds mylib/util.clj next to main.clj (or --path, RSCL_PATH)
<HI>! demo.main
args: (a b)
$ rscl build main.clj -o demo && ./demo x   # required namespaces are compiled into the binary
<HI>! demo.main
args: (x)

user=> (defn inner [x] (/ x 0)) (defn outer [x] (inner x)) (outer 1)
Execution error: ArithmeticException: Divide by zero
    at user/inner
    at user/outer
```

## Phase 6 — Rust library bridge (done)

```rust
// examples/native-demo/src/lib.rs, built as a cdylib
rscl_bridge::exports! {
    ns "demo";
    fn add(a: i64, b: i64) -> i64 { a + b }
    fn greet(name: &str) -> String { format!("Hello, {name}!") }
    fn parse_int(s: &str) -> Result<i64, String> { s.trim().parse().map_err(|e| format!("{e}")) }
    fn make_counter(start: i64) -> Native<Counter> { Native(Counter { n: Mutex::new(start) }) }
    fn counter_inc(c: &Native<Counter>) -> i64 { /* ... */ }
    fn apply_twice(f: Val, x: Val) -> rscl_bridge::Result<Val> { let y = f.call(&[x])?; f.call(&[y]) }
}
```

```
user=> (load-native "librscl_demo.so" :as d)      ; searched as given, RSCL_NATIVE_PATH, next to rscl
demo
user=> (d/add 1 2)
3
user=> (d/parse-int "x")
Execution error: RuntimeException: cannot parse "x": invalid digit found in string
user=> (def c (d/make-counter 10))  (d/counter-inc c)
#'user/c
11
user=> (d/apply-twice inc 5)
7
;; edit the Rust, `cargo build -p rscl-demo`, then:
user=> (load-native "librscl_demo.so")
demo                                          ; vars now point at the new code
```

A built binary that calls `load-native` loads the same `.so` at start-up.

## Phase 7 — Types, protocols, multimethods, dynamic vars, sorted collections (done)

```
user=> (defprotocol Shape (area [this]) (scale [this k]))
Shape
user=> (defrecord Rect [w h] Shape (area [this] (* w h)) (scale [this k] (->Rect (* w k) (* h k))))
user.Rect
user=> (def r (->Rect 2 3))  (area r)  (scale r 2)  (:w r)  (assoc r :tag :big)
#'user/r
6
#user.Rect{:w 4, :h 6}
2
#user.Rect{:w 2, :h 3, :tag :big}
user=> (deftype Circle [r] Shape (area [this] (* 3 r r)) (scale [this k] (Circle. (* r k))))  (.-r (scale (Circle. 2) 5))
user.Circle
10
user=> (extend-protocol Shape nil (area [_] 0) (scale [_ k] nil) String (area [s] (count s)) (scale [s k] (apply str (repeat k s))))
nil
user=> [(area nil) (area "abc") (scale "ab" 3) (satisfies? Shape 1)]
[0 3 "ababab" false]
user=> (area :kw)
Execution error: IllegalArgumentException: No implementation of method: :area of protocol: #'user/Shape found for class: Keyword
user=> (defrecord Named [n] Printable (-pr [this readably?] (str "<" n ">")))  (prn [(->Named "x")])
user.Named
[<x>]
user=> (defmulti speak (fn [a & _] (:kind a)))  (defmethod speak :dog [a] "woof")  (defmethod speak :default [a] "...")
#'user/speak
#object[user/speak 0x...]
#object[user/speak 0x...]
user=> [(speak {:kind :dog}) (speak {:kind :fish})]
["woof" "..."]
user=> (def ^:dynamic *depth* 0)  (defn show [] *depth*)  [(show) (binding [*depth* 1] (show)) (show)]
#'user/*depth*
#'user/show
[0 1 0]
user=> (sorted-map :b 2 :a 1)  (into (sorted-set) [3 1 2])  (subseq (sorted-set 1 2 3 4) > 2)
{:a 1, :b 2}
#{1 2 3}
(3 4)
user=> (defn documented "Adds things." [a b] (+ a b))  (doc documented)
#'user/documented
-------------------------
user/documented
([a b])
  Adds things.
nil
```


## Phase 8 — Performance (done)

Unboxed `long`/`double` locals (inferred from literals and arithmetic, or forced with
`^long`/`^double`), inline fixnum and primitive arithmetic and comparisons, direct
calls to Rust builtins, lock-free var reads for immortal roots, capture-less fns as
shared immortal objects, inline refcount guards, mimalloc, the single-pass register
allocator for the JIT, and `--gc-sections` for built binaries.

```
$ scripts/bench.sh --runs 5          # bench/*.clj; same source for Clojure 1.12 on OpenJDK 21; CPython 3.14
benchmark        python    clojure   rscl-jit   rscl-aot   rscl-opt
colls                47        643        606        307        312
fib                 131        491        347         41         46
loop_sum            382        485        325         23         20
mandelbrot          122        492        312         10         10
(median of 5 runs, milliseconds; the JVM time includes its start-up)
```

`rscl-jit` is `rscl run`, which includes about 270 ms to compile `core.clj` at start-up;
`rscl-aot` is a binary from `rscl build`, `rscl-opt` one from `rscl build --opt`.
`fib` is `(fib 32)`, `loop_sum` ten million iterations of integer arithmetic,
`mandelbrot` a 320×200 grid with `^double` parameters, `colls` persistent
vectors/maps, lazy seqs, sorting and string building. Before this phase the AOT
times were 414, 1183, 696 and 489 ms. The remaining gap on `colls` is allocation in
the persistent collections and per-element lazy-seq cells (chunked seqs are backlog).

```
user=> (defn sum-to [^long n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (rem (* i i) 7))) acc)))
#'user/sum-to
user=> (time (sum-to 10000000))
Elapsed time: 18.934731 msecs
19999999
user=> (let [x 9223372036854775807] (inc x))         ; primitive arithmetic keeps the semantics
Execution error: ArithmeticException: integer overflow
user=> (loop [x 1.0 i 0] (if (< i 3) (recur (* x 2) (inc i)) x))   ; x and i stay unboxed
8.0
```

## Phase 9 — Concurrency (done)

```
user=> (def f (future (sleep 100) (+ 1 2)))  [(realized? f) @f (realized? f)]
#'user/f
[false 3 true]
user=> (def p (promise))  (future (deliver p (str "hello from " (thread-id))))  @p
#'user/p
#object[Future 0x... {:status :pending}]
"hello from 3"
user=> (pmap (fn [n] (reduce + (range n))) [1000 2000 3000])
(499500 1999000 4498500)
user=> (def ^:dynamic *who* "main")  (binding [*who* "bound"] @(future *who*))
#'user/*who*
"bound"
user=> (let [c (atom 0) l (atom nil)] (doseq [f (doall (repeatedly 8 #(future (dotimes [_ 1000] (locking l (reset! c (inc @c)))))))] @f) @c)
8000
user=> (let [f (future (sleep 10000))] [(future-cancel f) (try @f (catch Exception e (ex-message e)))])
[true "future was cancelled"]
```

Memory hardening landed with this phase: locals are cleared at their last use and
callers hand arguments over to callees, so `(reduce + (range 3000000))` runs in 13 MB
(it took 273 MB before); reference cycles through atoms, promises and lazy seqs are
collected (`(let [a (atom nil)] (reset! a (fn [] @a)))` two hundred thousand times
runs in flat memory); keywords are interned weakly; `RSCL_HASH_SEED=random` protects
maps keyed by untrusted input.

