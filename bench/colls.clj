;; Collection-heavy work: vectors, maps, lazy seqs, sorting, strings.
(defn build-vec [n] (loop [i 0 v []] (if (< i n) (recur (inc i) (conj v (* i 2))) v)))
(defn build-map [n] (loop [i 0 m {}] (if (< i n) (recur (inc i) (assoc m i (str "v" i))) m)))
(let [v (build-vec 200000)
      m (build-map 100000)]
  (println (count v) (reduce + v) (nth v 1234))
  (println (count m) (get m 777) (count (filter (fn [e] (even? (key e))) m)))
  (println (reduce + (map (fn [x] (* x x)) (filter odd? (range 300000)))))
  (println (count (sort (map (fn [i] (rem (* i 7919) 10007)) (range 100000)))))
  (println (count (apply str (map str (range 20000)))))
  (println (reduce (fn [acc k] (if (contains? m k) (inc acc) acc)) 0 (range 0 100000 3))))
