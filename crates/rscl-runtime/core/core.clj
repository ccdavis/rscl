;; clojure.core for rscl — the bootstrap library, compiled into every session and every
;; AOT binary. Written from scratch for rscl (MIT). Only constructs that already exist
;; at each point in the file may be used: the Rust builtins, the special forms, and the
;; bootstrap rewrites `defmacro` `defn` `fn` `let` `loop` (symbols only, no
;; destructuring) until the real macros are defined further down.

;;; ---- basic macros --------------------------------------------------------------

(defmacro when [test & body]
  (list 'if test (cons 'do body)))

(defmacro when-not [test & body]
  (list 'if test nil (cons 'do body)))

(defmacro if-not
  ([test then] (list 'if test nil then))
  ([test then else] (list 'if test else then)))

(defmacro cond [& clauses]
  (when clauses
    (list 'if (first clauses)
          (if (next clauses)
            (second clauses)
            (throw (ex-info "cond requires an even number of forms" {})))
          (cons 'clojure.core/cond (next (next clauses))))))

(defmacro and
  ([] true)
  ([x] x)
  ([x & next]
   `(let* [and# ~x] (if and# (and ~@next) and#))))

(defmacro or
  ([] nil)
  ([x] x)
  ([x & next]
   `(let* [or# ~x] (if or# or# (or ~@next)))))

(defmacro comment [& body] nil)

(defmacro -> [x & forms]
  (loop [x x forms forms]
    (if forms
      (let [form (first forms)
            threaded (if (seq? form)
                       (with-meta `(~(first form) ~x ~@(next form)) (meta form))
                       (list form x))]
        (recur threaded (next forms)))
      x)))

(defmacro ->> [x & forms]
  (loop [x x forms forms]
    (if forms
      (let [form (first forms)
            threaded (if (seq? form)
                       (with-meta `(~(first form) ~@(next form) ~x) (meta form))
                       (list form x))]
        (recur threaded (next forms)))
      x)))

(defmacro lazy-seq [& body]
  `(lazy-seq* (fn* [] ~@body)))

(defmacro defonce [name expr]
  `(do (def ~name)
       (when-not (bound? (var ~name)) (def ~name ~expr))))

(defmacro dotimes [bindings & body]
  (let [i (first bindings) n (second bindings)]
    `(let* [n# ~n]
       (loop* [~i 0]
         (when (< ~i n#)
           ~@body
           (recur (inc ~i)))))))

(defmacro while [test & body]
  `(loop* [] (when ~test ~@body (recur))))

(defmacro assert
  ([x] `(when-not ~x (throw (ex-info (str "Assert failed: " (pr-str '~x)) {}))))
  ([x message] `(when-not ~x (throw (ex-info (str "Assert failed: " ~message "\n" (pr-str '~x)) {})))))

(defmacro time [expr]
  `(let* [start# (nano-time) ret# ~expr]
     (println (str "Elapsed time: " (/ (- (nano-time) start#) 1000000.0) " msecs"))
     ret#))

;;; ---- sequence library (lazy) -----------------------------------------------------

(defn ffirst [x] (first (first x)))
(defn fnext [x] (first (next x)))
(defn nfirst [x] (next (first x)))
(defn nnext [x] (next (next x)))

(defn nthrest [coll n]
  (loop [n n xs coll]
    (if (and (pos? n) (seq xs))
      (recur (dec n) (rest xs))
      xs)))

(defn nthnext [coll n]
  (loop [n n xs (seq coll)]
    (if (and (pos? n) xs)
      (recur (dec n) (next xs))
      xs)))

(defn some [pred coll]
  (loop [s (seq coll)]
    (when s
      (or (pred (first s)) (recur (next s))))))

(defn every? [pred coll]
  (loop [s (seq coll)]
    (cond (nil? s) true
          (pred (first s)) (recur (next s))
          :else false)))

(defn not-every? [pred coll] (not (every? pred coll)))
(defn not-any? [pred coll] (not (some pred coll)))

(defn map
  ([f coll]
   (lazy-seq
     (let [s (seq coll)]
       (when s (cons (f (first s)) (map f (rest s)))))))
  ([f c1 c2]
   (lazy-seq
     (let [s1 (seq c1) s2 (seq c2)]
       (when (and s1 s2)
         (cons (f (first s1) (first s2)) (map f (rest s1) (rest s2)))))))
  ([f c1 c2 c3]
   (lazy-seq
     (let [s1 (seq c1) s2 (seq c2) s3 (seq c3)]
       (when (and s1 s2 s3)
         (cons (f (first s1) (first s2) (first s3)) (map f (rest s1) (rest s2) (rest s3)))))))
  ([f c1 c2 c3 & colls]
   (let [step (fn step [cs]
                (lazy-seq
                  (let [ss (map seq cs)]
                    (when (every? identity ss)
                      (cons (map first ss) (step (map rest ss)))))))]
     (map (fn [xs] (apply f xs)) (step (cons c1 (cons c2 (cons c3 colls))))))))

(defn filter [pred coll]
  (lazy-seq
    (loop [s (seq coll)]
      (when s
        (let [f (first s)]
          (if (pred f)
            (cons f (filter pred (rest s)))
            (recur (next s))))))))

(defn remove [pred coll]
  (filter (complement pred) coll))

(defn keep [f coll]
  (lazy-seq
    (loop [s (seq coll)]
      (when s
        (let [x (f (first s))]
          (if (nil? x)
            (recur (next s))
            (cons x (keep f (rest s)))))))))

(defn keep-indexed [f coll]
  (let [step (fn step [i s]
               (lazy-seq
                 (loop [i i s (seq s)]
                   (when s
                     (let [x (f i (first s))]
                       (if (nil? x)
                         (recur (inc i) (next s))
                         (cons x (step (inc i) (rest s)))))))))]
    (step 0 coll)))

(defn map-indexed [f coll]
  (let [step (fn step [i s]
               (lazy-seq
                 (let [s (seq s)]
                   (when s (cons (f i (first s)) (step (inc i) (rest s)))))))]
    (step 0 coll)))

(defn concat
  ([] (lazy-seq nil))
  ([x] (lazy-seq x))
  ([x y]
   (lazy-seq
     (let [s (seq x)]
       (if s (cons (first s) (concat (rest s) y)) y))))
  ([x y & zs]
   (let [cat (fn cat [xys zs]
               (lazy-seq
                 (let [xys (seq xys)]
                   (if xys
                     (cons (first xys) (cat (rest xys) zs))
                     (when zs (cat (first zs) (next zs)))))))]
     (cat (concat x y) zs))))

(defn mapcat [f & colls]
  (apply concat (apply map f colls)))

(defn take [n coll]
  (lazy-seq
    (when (pos? n)
      (let [s (seq coll)]
        (when s (cons (first s) (take (dec n) (rest s))))))))

(defn drop [n coll]
  (let [step (fn [n coll]
               (let [s (seq coll)]
                 (if (and (pos? n) s)
                   (recur (dec n) (rest s))
                   s)))]
    (lazy-seq (step n coll))))

(defn take-while [pred coll]
  (lazy-seq
    (let [s (seq coll)]
      (when (and s (pred (first s)))
        (cons (first s) (take-while pred (rest s)))))))

(defn drop-while [pred coll]
  (let [step (fn [pred coll]
               (let [s (seq coll)]
                 (if (and s (pred (first s)))
                   (recur pred (rest s))
                   s)))]
    (lazy-seq (step pred coll))))

(defn take-nth [n coll]
  (lazy-seq
    (let [s (seq coll)]
      (when s (cons (first s) (take-nth n (drop n s)))))))

(defn take-last [n coll]
  (loop [s (seq coll) lead (seq (drop n coll))]
    (if lead
      (recur (next s) (next lead))
      s)))

(defn drop-last
  ([coll] (drop-last 1 coll))
  ([n coll] (map (fn [x _] x) coll (drop n coll))))

(defn split-at [n coll] [(take n coll) (drop n coll)])
(defn split-with [pred coll] [(take-while pred coll) (drop-while pred coll)])

(defn repeat
  ([x] (lazy-seq (cons x (repeat x))))
  ([n x] (take n (repeat x))))

(defn repeatedly
  ([f] (lazy-seq (cons (f) (repeatedly f))))
  ([n f] (take n (repeatedly f))))

(defn iterate [f x]
  (cons x (lazy-seq (iterate f (f x)))))

(defn range
  ([] (iterate inc 0))
  ([end] (range 0 end 1))
  ([start end] (range start end 1))
  ([start end step]
   (lazy-seq
     (when (or (and (pos? step) (< start end))
               (and (neg? step) (> start end)))
       (cons start (range (+ start step) end step))))))

(defn cycle [coll]
  (lazy-seq
    (when (seq coll)
      (let [step (fn step [s]
                   (lazy-seq
                     (if (seq s)
                       (cons (first s) (step (rest s)))
                       (step coll))))]
        (step coll)))))

(defn interleave
  ([] ())
  ([c1] (lazy-seq c1))
  ([c1 c2]
   (lazy-seq
     (let [s1 (seq c1) s2 (seq c2)]
       (when (and s1 s2)
         (cons (first s1) (cons (first s2) (interleave (rest s1) (rest s2))))))))
  ([c1 c2 & colls]
   (lazy-seq
     (let [ss (map seq (cons c1 (cons c2 colls)))]
       (when (every? identity ss)
         (concat (map first ss) (apply interleave (map rest ss))))))))

(defn interpose [sep coll]
  (drop 1 (interleave (repeat sep) coll)))

(defn partition
  ([n coll] (partition n n coll))
  ([n step coll]
   (lazy-seq
     (let [s (seq coll)]
       (when s
         (let [p (doall (take n s))]
           (when (= n (count p))
             (cons p (partition n step (nthrest s step)))))))))
  ([n step pad coll]
   (lazy-seq
     (let [s (seq coll)]
       (when s
         (let [p (doall (take n s))]
           (if (= n (count p))
             (cons p (partition n step pad (nthrest s step)))
             (list (take n (concat p pad))))))))))

(defn partition-all
  ([n coll] (partition-all n n coll))
  ([n step coll]
   (lazy-seq
     (let [s (seq coll)]
       (when s
         (cons (doall (take n s)) (partition-all n step (nthrest s step))))))))

(defn partition-by [f coll]
  (lazy-seq
    (let [s (seq coll)]
      (when s
        (let [fst (first s)
              fv (f fst)
              run (cons fst (take-while (fn [x] (= fv (f x))) (next s)))]
          (cons run (partition-by f (drop (count run) s))))))))

(defn distinct [coll]
  (let [step (fn step [xs seen]
               (lazy-seq
                 (loop [xs (seq xs) seen seen]
                   (when xs
                     (let [f (first xs)]
                       (if (contains? seen f)
                         (recur (next xs) seen)
                         (cons f (step (rest xs) (conj seen f)))))))))]
    (step coll #{})))

(defn dedupe [coll]
  (let [step (fn step [s prev]
               (lazy-seq
                 (loop [s (seq s) prev prev]
                   (when s
                     (let [f (first s)]
                       (if (= f prev)
                         (recur (next s) prev)
                         (cons f (step (rest s) f))))))))]
    (step coll ::none)))

(defn tree-seq [branch? children root]
  (let [walk (fn walk [node]
               (lazy-seq
                 (cons node
                       (when (branch? node)
                         (mapcat walk (children node))))))]
    (walk root)))

(defn flatten [x]
  (filter (complement sequential?)
          (rest (tree-seq sequential? seq x))))

(defn run! [proc coll]
  (loop [s (seq coll)]
    (when s
      (proc (first s))
      (recur (next s)))))

(defn mapv [f & colls] (vec (apply map f colls)))
(defn filterv [pred coll] (vec (filter pred coll)))

(defn max-key
  ([k x] x)
  ([k x y] (if (> (k x) (k y)) x y))
  ([k x y & more] (reduce (fn [a b] (max-key k a b)) (max-key k x y) more)))

(defn min-key
  ([k x] x)
  ([k x y] (if (< (k x) (k y)) x y))
  ([k x y & more] (reduce (fn [a b] (min-key k a b)) (min-key k x y) more)))

(defn rand-nth [coll] (nth coll (rand-int (count coll))))

(defn vary-meta [obj f & args] (with-meta obj (apply f (meta obj) args)))

(defn trampoline
  ([f]
   (let [ret (f)]
     (if (fn? ret) (recur ret) ret)))
  ([f & args] (trampoline (fn [] (apply f args)))))

;;; ---- destructuring and the real let / loop / fn / defn / defmacro ---------------------

(defmacro declare [& names]
  (cons 'do (map (fn [n] (list 'def n)) names)))

(declare destructure-vec destructure-map)

(defn destructure-pattern [bvec b v]
  (cond (symbol? b) (conj bvec b v)
        (vector? b) (destructure-vec bvec b v)
        (map? b) (destructure-map bvec b v)
        :else (throw (ex-info (str "Unsupported binding form: " (pr-str b)) {}))))

(defn destructure-vec [bvec b init]
  (let [gvec (gensym "vec__")
        gseq (gensym "seq__")
        gfirst (gensym "first__")
        has-rest (some (fn [x] (= x '&)) b)]
    (loop [ret (let [ret (conj bvec gvec init)]
                 (if has-rest (conj ret gseq (list 'clojure.core/seq gvec)) ret))
           n 0
           bs (seq b)
           seen-rest? false]
      (if bs
        (let [firstb (first bs)]
          (cond (= firstb '&)
                (recur (destructure-pattern ret (second bs) gseq) n (nnext bs) true)
                (= firstb :as)
                (destructure-pattern ret (second bs) gvec)
                :else
                (if seen-rest?
                  (throw (ex-info "Unsupported binding form, only :as can follow & parameter" {}))
                  (recur (if has-rest
                           (destructure-pattern
                             (conj ret gfirst (list 'clojure.core/first gseq)
                                       gseq (list 'clojure.core/next gseq))
                             firstb gfirst)
                           (destructure-pattern ret firstb (list 'clojure.core/nth gvec n nil)))
                         (inc n)
                         (next bs)
                         seen-rest?))))
        ret))))

(defn destructure-map [bvec b init]
  (let [gmap (gensym "map__")
        defaults (get b :or)
        ret (conj bvec gmap init)
        ret (if (get b :as) (destructure-pattern ret (get b :as) gmap) ret)
        bind (fn [ret pat kw]
               (destructure-pattern ret pat
                 (if (and (symbol? pat) (contains? defaults pat))
                   (list 'clojure.core/get gmap kw (get defaults pat))
                   (list 'clojure.core/get gmap kw))))
        ret (reduce (fn [ret k]
                      (let [sym (symbol (name k))
                            kw (if (keyword? k) k (keyword (namespace k) (name k)))]
                        (bind ret sym kw)))
                    ret (get b :keys))
        ret (reduce (fn [ret k] (bind ret k (name k))) ret (get b :strs))
        ret (reduce (fn [ret k] (bind ret k (list 'quote k))) ret (get b :syms))]
    (reduce (fn [ret e]
              (let [pat (key e) k (val e)]
                (if (keyword? pat)
                  ret
                  (bind ret pat k))))
            ret (seq b))))

(defn destructure [bindings]
  (let [pairs (partition 2 bindings)]
    (reduce (fn [bvec pair] (destructure-pattern bvec (first pair) (second pair)))
            [] pairs)))

(defmacro let [bindings & body]
  (when-not (and (vector? bindings) (even? (count bindings)))
    (throw (ex-info "let requires a vector of an even number of forms" {})))
  `(let* ~(destructure bindings) ~@body))

(defmacro loop [bindings & body]
  (when-not (and (vector? bindings) (even? (count bindings)))
    (throw (ex-info "loop requires a vector of an even number of forms" {})))
  (let [db (destructure bindings)]
    (if (= db bindings)
      `(loop* ~bindings ~@body)
      (let [vs (take-nth 2 (drop 1 bindings))
            bs (take-nth 2 bindings)
            gs (map (fn [b] (if (symbol? b) b (gensym))) bs)
            bfs (reduce (fn [ret t]
                          (let [b (nth t 0) v (nth t 1) g (nth t 2)]
                            (if (symbol? b)
                              (conj ret g v)
                              (conj ret g v b g))))
                        [] (map vector bs vs gs))]
        `(let ~bfs
           (loop* ~(vec (interleave gs gs))
             (let ~(vec (interleave bs gs))
               ~@body)))))))

(defmacro fn [& sigs]
  (let [name (if (symbol? (first sigs)) (first sigs) nil)
        sigs (if name (next sigs) sigs)
        sigs (if (vector? (first sigs)) (list sigs) sigs)
        psig (fn* [sig]
               (let [params (first sig)
                     body (next sig)]
                 (if (every? symbol? params)
                   sig
                   (loop [params (seq params) new-params [] lets []]
                     (if params
                       (let [p (first params)]
                         (cond (= p '&) (recur (next params) (conj new-params '&) lets)
                               (symbol? p) (recur (next params) (conj new-params p) lets)
                               :else (let [g (gensym "p__")]
                                       (recur (next params) (conj new-params g) (conj lets p g)))))
                       (cons new-params (list (cons 'clojure.core/let (cons lets body)))))))))
        new-sigs (map psig sigs)]
    (if name
      (cons 'fn* (cons name new-sigs))
      (cons 'fn* new-sigs))))

(defmacro defn [name & fdecl]
  (let [fdecl (if (string? (first fdecl)) (next fdecl) fdecl)
        fdecl (if (map? (first fdecl)) (next fdecl) fdecl)]
    (list 'def name (cons 'clojure.core/fn (cons name fdecl)))))

(defmacro defn- [name & fdecl]
  (cons 'clojure.core/defn (cons name fdecl)))

(defmacro defmacro [name & body]
  (let [body (if (string? (first body)) (next body) body)
        body (if (map? (first body)) (next body) body)
        sigs (if (vector? (first body)) (list body) body)
        add-implicit (fn [sig] (cons (vec (concat ['&form '&env] (first sig))) (next sig)))]
    `(do (def ~name (fn ~name ~@(map add-implicit sigs)))
         (set-macro! (var ~name))
         (var ~name))))

;;; ---- namespaces --------------------------------------------------------------------

(defmacro refer-clojure [& args] nil)

(defmacro ns [name & references]
  (let [refs (if (string? (first references)) (next references) references)
        refs (if (map? (first refs)) (next refs) refs)
        quote-all (fn [args] (map (fn [a] (list 'quote a)) args))
        process (fn [r]
                  (let [kind (first r) args (rest r)]
                    (cond (= kind :require) (cons 'clojure.core/require (quote-all args))
                          (= kind :use) (cons 'clojure.core/use (quote-all args))
                          (= kind :refer-clojure) nil
                          (= kind :import) nil
                          (= kind :gen-class) nil
                          :else (throw (ex-info (str "Unsupported ns reference: " kind) {})))))]
    `(do (clojure.core/in-ns '~name)
         ~@(filter identity (map process refs)))))

(defmacro load-native
  "Load a native library; a bridge library's fns become vars in its namespace.
  (load-native \"libfoo.so\" :as foo) aliases that namespace."
  ([path] `(load-native* ~path))
  ([path as alias] `(load-native* ~path ~as '~alias)))

;;; ---- macros that use destructuring -----------------------------------------------

(defmacro if-let
  ([bindings then] `(if-let ~bindings ~then nil))
  ([bindings then else]
   (let [form (first bindings) tst (second bindings)]
     `(let [temp# ~tst]
        (if temp#
          (let [~form temp#] ~then)
          ~else)))))

(defmacro when-let [bindings & body]
  (let [form (first bindings) tst (second bindings)]
    `(let [temp# ~tst]
       (when temp#
         (let [~form temp#] ~@body)))))

(defmacro if-some
  ([bindings then] `(if-some ~bindings ~then nil))
  ([bindings then else]
   (let [form (first bindings) tst (second bindings)]
     `(let [temp# ~tst]
        (if (nil? temp#)
          ~else
          (let [~form temp#] ~then))))))

(defmacro when-some [bindings & body]
  (let [form (first bindings) tst (second bindings)]
    `(let [temp# ~tst]
       (if (nil? temp#)
         nil
         (let [~form temp#] ~@body)))))

(defmacro as-> [expr name & forms]
  `(let [~name ~expr
         ~@(interleave (repeat name) (butlast forms))]
     ~(if (empty? forms) name (last forms))))

(defmacro some-> [expr & forms]
  (let [g (gensym)
        steps (map (fn [step] `(if (nil? ~g) nil (-> ~g ~step))) forms)]
    `(let [~g ~expr
           ~@(interleave (repeat g) (butlast steps))]
       ~(if (empty? steps) g (last steps)))))

(defmacro some->> [expr & forms]
  (let [g (gensym)
        steps (map (fn [step] `(if (nil? ~g) nil (->> ~g ~step))) forms)]
    `(let [~g ~expr
           ~@(interleave (repeat g) (butlast steps))]
       ~(if (empty? steps) g (last steps)))))

(defmacro cond-> [expr & clauses]
  (let [g (gensym)
        steps (map (fn [p] (let [test (first p) step (second p)] `(if ~test (-> ~g ~step) ~g)))
                   (partition 2 clauses))]
    `(let [~g ~expr
           ~@(interleave (repeat g) (butlast steps))]
       ~(if (empty? steps) g (last steps)))))

(defmacro cond->> [expr & clauses]
  (let [g (gensym)
        steps (map (fn [p] (let [test (first p) step (second p)] `(if ~test (->> ~g ~step) ~g)))
                   (partition 2 clauses))]
    `(let [~g ~expr
           ~@(interleave (repeat g) (butlast steps))]
       ~(if (empty? steps) g (last steps)))))

(defmacro doseq [seq-exprs & body]
  (if (empty? seq-exprs)
    `(do ~@body nil)
    (let [k (first seq-exprs) v (second seq-exprs) more (nnext seq-exprs)]
      (cond (= k :let) `(let ~v (doseq ~more ~@body))
            (= k :when) `(when ~v (doseq ~more ~@body))
            (= k :while) `(when ~v (doseq ~more ~@body))
            :else `(loop* [s# (seq ~v)]
                     (when s#
                       (let [~k (first s#)]
                         (doseq ~more ~@body))
                       (recur (next s#))))))))

(defmacro for [seq-exprs body-expr]
  (if (empty? seq-exprs)
    `(list ~body-expr)
    (let [k (first seq-exprs) v (second seq-exprs) more (nnext seq-exprs)]
      (cond (= k :let) `(let ~v (for ~more ~body-expr))
            (= k :when) `(when ~v (for ~more ~body-expr))
            (= k :while) `(when ~v (for ~more ~body-expr))
            :else `(mapcat (fn [~k] (for ~more ~body-expr)) ~v)))))

(defmacro case [e & clauses]
  (let [ge (gensym "case__")
        default (if (odd? (count clauses))
                  (last clauses)
                  `(throw (ex-info (str "No matching clause: " ~ge) {})))
        pairs (partition 2 clauses)
        test (fn [c]
               (if (seq? c)
                 `(or ~@(map (fn [x] `(= ~ge '~x)) c))
                 `(= ~ge '~c)))]
    `(let [~ge ~e]
       (cond ~@(mapcat (fn [p] [(test (first p)) (second p)]) pairs)
             :else ~default))))

(defmacro condp [pred expr & clauses]
  (let [gpred (gensym "pred__")
        gexpr (gensym "expr__")
        emit (fn emit [args]
               (cond (empty? args) `(throw (ex-info (str "No matching clause: " ~gexpr) {}))
                     (= 1 (count args)) (first args)
                     (= :>> (second args))
                     `(let [r# (~gpred ~(first args) ~gexpr)]
                        (if r# (~(nth args 2) r#) ~(emit (drop 3 args))))
                     :else
                     `(if (~gpred ~(first args) ~gexpr)
                        ~(second args)
                        ~(emit (drop 2 args)))))]
    `(let [~gpred ~pred ~gexpr ~expr] ~(emit clauses))))

(defmacro doto [x & forms]
  (let [gx (gensym)]
    `(let [~gx ~x]
       ~@(map (fn [f] (if (seq? f) `(~(first f) ~gx ~@(next f)) `(~f ~gx))) forms)
       ~gx)))

(defn memoize [f]
  (let [mem (atom {})]
    (fn [& args]
      (if-let [e (find @mem args)]
        (val e)
        (let [ret (apply f args)]
          (swap! mem assoc args ret)
          ret)))))
