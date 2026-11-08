;; Closures capturing locals at several depths, named fns, self recursion.
(defn adder [x] (fn [y] (+ x y)))
(def add10 (adder 10))
(println (add10 5) ((adder 1) 2))

(defn compose2 [f g] (fn [x] (f (g x))))
(println ((compose2 inc (adder 100)) 1))

(let [prefix "pre-"]
  (defn tag [s] (str prefix s)))
(println (tag "fix"))

(defn outer [a]
  (fn [b]
    (fn [c] (str a b c))))
(println (((outer "x") "y") "z"))

(println ((fn fact [n] (if (<= n 1) 1 (* n (fact (dec n))))) 10))

(defn make-counter-fn [start]
  (fn [step] (loop [i start acc nil] (if (> i (+ start 2)) acc (recur (+ i step) (str acc i))))))
(println ((make-counter-fn 1) 1))
