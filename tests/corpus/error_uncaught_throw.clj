(println "before throw")
(throw (ex-info "custom failure" {:why :testing}))
(println "not reached")
