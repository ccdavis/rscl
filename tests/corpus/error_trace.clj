;; An uncaught exception reports the rscl fns on the stack (stderr; status 1).
(defn inner [x] (/ x 0))
(defn middle [x] (inner (inc x)))
(defn outer [x] (middle (* x 2)))
(println "start")
(outer 3)
