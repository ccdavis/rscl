;; Multi-arity and variadic functions.
(defn f
  ([] :zero)
  ([a] (str "one:" a))
  ([a b] (str "two:" a b))
  ([a b & more] (str "many:" a b more)))
(println (f) (f 1) (f 1 2) (f 1 2 3 4))

(defn g [& xs] xs)
(println (g) (g 1) (g 1 2))

