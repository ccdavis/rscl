;; Calling a protocol method on a type without an implementation is an uncaught error.
(defprotocol P (m [this]))
(extend-protocol P Long (m [n] n))
(println (m 1))
(println (m "str"))
(println :never)
