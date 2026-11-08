;; A compile error: exit status 2 and nothing printed.
(println "compiled first form")
(defn broken [] (undefined-function 1))
