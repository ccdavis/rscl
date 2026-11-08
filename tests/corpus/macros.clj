;; defmacro, syntax-quote, gensyms, macroexpand, and the core macros.
(defmacro unless [test & body]
  `(if ~test nil (do ~@body)))
(println (unless false :ran) (unless true :ran))
(println (macroexpand-1 '(unless x 1 2)))

(defmacro swap-pair [[a b]] `[~b ~a])
(println (swap-pair [1 2]))

(defmacro with-tmp [binding & body]
  `(let [tmp# ~binding] (println "tmp is" tmp#) ~@body tmp#))
(println (with-tmp (+ 1 2) :ignored))

(defmacro my-and
  ([] true)
  ([x] x)
  ([x & more] `(let [v# ~x] (if v# (my-and ~@more) v#))))
(println (my-and 1 2 3) (my-and 1 nil 3) (my-and))

(println (when true 1 2) (when-not false :ok) (if-not true :a :b) (cond (neg? 1) :neg (pos? 1) :pos))
(println (and) (and 1 2) (and 1 nil) (or) (or nil 2) (or false))
(println (-> 5 inc (* 2) str) (->> [1 2 3] (map inc) (reduce +)))
(println (if-let [x (first [7])] (* x 2) :none) (if-let [x nil] x :none) (when-let [[a b] [1 2]] (+ a b)))
(println (if-some [x false] x :nil) (when-some [x nil] :never))
(println (as-> 3 $ (+ $ 1) (* $ 2)) (some-> {:a {:b 1}} :a :b inc) (some-> nil :a inc) (cond-> 1 true inc false (* 10) true (* 2)))
(println (case 2 1 :one 2 :two :other) (case "s" "s" :string :other) (case 'x (a b) :ab x :x) (case 99 1 :one :default))
(println (condp = 3 1 :one 3 :three :none) (condp some [1 2 3] #{0 6} :none #{2 3} :>> (fn [m] (str "found " m))))
(println @(doto (atom []) (swap! conj 1) (swap! conj 2)))
(let [x (atom 0)] (dotimes [i 4] (swap! x + i)) (println @x))
(let [x (atom 3) out (atom [])] (while (pos? @x) (swap! out conj @x) (swap! x dec)) (println @out))
(println (try (assert (= 1 2) "math") (catch Exception e (ex-message e))))
(defonce once-val (do (println "computing once") 42))
(defonce once-val (do (println "never") 0))
(println once-val)
(declare later)
(defn uses-later [] (later))
(defn later [] :defined-later)
(println (uses-later))
(println (special-symbol? 'if) (special-symbol? 'inc) (macro? (var when)) (macro? (var inc)))
