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
