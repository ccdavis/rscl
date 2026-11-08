;; Recursive fib through a var; redefinition visible to callers.
(defn fib [n]
  (if (< n 2)
    n
    (+ (fib (- n 1)) (fib (- n 2)))))

(println (fib 25))
(println (fib 0) (fib 1) (fib 10))

(defn fib-iter [n]
  (loop [i 0 a 0 b 1]
    (if (= i n) a (recur (inc i) b (+ a b)))))

(println (fib-iter 90))
