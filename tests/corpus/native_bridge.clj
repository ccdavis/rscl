;; The Rust bridge: functions exported with rscl_bridge::exports! (examples/native-demo).
;; The harness sets RSCL_NATIVE_PATH to the directory holding librscl_demo.so.
(load-native "librscl_demo.so" :as d)
(println (d/add 1 2) (d/greet "rscl") (d/fib 20))
(println (d/parse-int " 42 ") (try (d/parse-int "x") (catch Exception e (ex-message e))))
(println (d/sum [1 2 3]) (d/sum (range 5)) (d/sum nil) (d/mean [1 2 3 4]) (d/mean []))
(println (sort (keys (d/word-lengths "hello big world"))) (d/describe {:a 1 "b" [2 3] :c "s"}))
(println (d/apply-twice inc 5) (d/apply-twice (fn [x] (str x "!")) "hi") (try (d/apply-twice (fn [x] (/ x 0)) 1) (catch ArithmeticException e :caught)))
(def c (d/make-counter 10))
(println (d/counter-inc c) (d/counter-inc c) (type c) (try (d/counter-inc 5) (catch ClassCastException e (ex-message e))))
(println (try (d/add 1) (catch Exception e (ex-message e))) (try (d/add "a" 1) (catch Exception e (ex-message e))))
(println (fn? d/add) (= 'demo (load-native "librscl_demo.so")) (ns-name (find-ns 'demo)))
(println (map d/add [1 2 3] [10 20 30]) (reduce d/add 0 (range 101)))
;; A lazy seq realised inside a library call (collections are held off there), and a
;; native object freed by the collector (its destructor runs after the collection).
(println (d/sum (map (fn [x] (gc) x) (range 5))) (do (d/make-counter 1) (gc) (gc) (gc) :freed))
