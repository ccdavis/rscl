;; Value lifetimes: a function stops referring to a value after its last use (so a
;; lazy sequence handed to a consumer is collected as it is walked), and none of
;; that changes what programs compute.
(defn total [coll] (reduce + coll))
(println (total (range 100000)) (count (filter even? (range 100000))) (last (map inc (range 100000))))
(println (nth (map (fn [i] (* i i)) (range 1000)) 999) (frequencies (map (fn [i] (rem i 3)) (range 30))))

;; a local used twice: it is still there for the second use
(defn twice [coll] [(count coll) (first coll)])
(println (twice (range 5)) (twice [1 2 3]))

;; closures capture at the fn expression; the outer local may be used again after
(defn make-adder [n] (let [f (fn [x] (+ x n))] [(f 1) n (f 2)]))
(println (make-adder 10))

;; loops: outer locals used in a loop body stay alive across iterations
(defn sum-with [base coll] (loop [s (seq coll) acc 0] (if s (recur (next s) (+ acc base (first s))) acc)))
(println (sum-with 100 (range 5)) (let [v [1 2 3]] (loop [i 0 acc 0] (if (< i (count v)) (recur (inc i) (+ acc (nth v i))) acc))))

;; a loop local rebound by recur
(println (loop [xs (range 10) acc []] (if (seq xs) (recur (rest xs) (conj acc (first xs))) acc)))

;; fn-level recur over a parameter only the callee refers to, and over a shared one
(defn walk [s n] (if (seq s) (recur (next s) (inc n)) n))
(def shared (range 20))
(println (walk (range 10) 0) (walk shared 0) (walk shared 0) (count shared))

;; rest arguments; apply spreads a sequence into them
(defn pack [& xs] (vec xs))
(println (pack (str "a") (str "b")) (apply pack (map str (range 3))) (pack))
(defn kw-call [m] (:k m))
(println (kw-call (hash-map :k (str "v"))) ((hash-map :a 1) :a) ((fn [f] (f (vector 1 2))) count))

;; try/catch/finally keep locals needed by handlers alive
(defn guarded [x]
  (try (if (= x :boom) (throw (ex-info "boom" {:x x})) (str "ok " x))
       (catch Exception e (str "caught " (:x (ex-data e)) " " x))
       (finally (println "finally" x))))
(println (guarded 1) (guarded :boom))

;; values passed to collection literals and to builtins live on in the result
(defn build [a b] [a {:b b} #{a} (list a b)])
(println (build (str "x") (str "y")))
(println (let [s (str "once")] (vector s s s)))

;; shadowing and nested lets
(println (let [x (str "outer")] (let [x (str "inner")] x)) (let [x (str "v") y x] [y (count y)]))
(println (let [x (str "a")] (if (= (count x) 1) (str x "!") x)))

;; primitives mix with values
(defn mixed [^long n coll] (+ n (count coll)))
(println (mixed 5 (range 7)) (mixed 1 (vector 1 2)))
