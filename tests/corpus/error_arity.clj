;; Output before the failure must be flushed; wrong arity is an uncaught exception.
(println "before")
(defn one [a] a)
(one 1 2)
(println "not reached")
