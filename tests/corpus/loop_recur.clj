;; loop/recur, fn-level recur, recur with strings and variadic fns.
(println (loop [i 0 acc 0] (if (< i 1000) (recur (inc i) (+ acc i)) acc)))

(defn count-down [n] (if (zero? n) :liftoff (recur (dec n))))
(println (count-down 100000))

(defn describe [sep & xs]
  (str sep xs))
(println (describe "-" 1 2) (describe "-"))

(defn sum-to [n]
  (loop [i 1 total 0]
    (if (> i n) total (recur (inc i) (+ total i)))))
(println (sum-to 100) (sum-to 0))

(println (loop [s "" i 0] (if (< i 5) (recur (str s i) (inc i)) s)))
