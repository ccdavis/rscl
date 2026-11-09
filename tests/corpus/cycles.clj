;; Reference cycles through atoms, promises and lazy seqs are collected.
(defn make-cycles [n]
  (dotimes [_ n]
    (let [a (atom nil)] (reset! a (fn [] @a)))
    (let [a (atom {})] (swap! a assoc :f (fn [] @a) :v (vec (range 10))))
    (let [a (atom nil) p (promise)] (deliver p (fn [] [@a @p])) (reset! a p))
    (let [a (atom nil)] (reset! a (lazy-seq (cons 1 @a))))))
(make-cycles 5000)
(println (pos? (gc-count)) (>= (gc) 0))
;; live data reached from a candidate survives, shared structure included
(def keep (atom {}))
(let [m (assoc @keep :f (fn [] 1) :xs (vec (range 5))) snapshot m]
  (reset! keep m)
  (gc)
  (println ((:f snapshot)) (:xs @keep) (= m @keep) ((:f @keep))))
(def cyc (let [a (atom nil)] (reset! a (fn [] @a)) a))
(println (fn? @cyc) (= 0 (gc)) (fn? @cyc))
(println @(future (do (dotimes [_ 1000] (let [a (atom nil)] (reset! a (fn [] @a)))) (>= (gc) 0))))
