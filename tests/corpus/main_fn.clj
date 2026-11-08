;; A -main function runs after the top-level forms, in `run` and in built binaries.
(println "loading")
(defn -main [& args]
  (println "main called with" args))
(println "loaded")
