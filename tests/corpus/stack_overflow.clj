;; Unbounded recursion becomes a catchable StackOverflowError instead of a crash.
(defn deep [n] (+ 1 (deep (inc n))))
(println (try (deep 0) (catch StackOverflowError e :caught)))
(println "still alive")
