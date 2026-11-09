;; futures, promises, delays, pmap, locking, cancellation, binding conveyance.
(def f1 (future (+ 1 2)))
(println @f1 (deref f1) (future? f1) (realized? f1) (future-done? f1) (future-cancelled? f1) (future? 1))
(println (deref (future (do (sleep 300) :late)) 50 :timeout) (deref (future :fast) 1000 :timeout))

(def p (promise))
(def waiter (future (deref p)))
(println (realized? p) (boolean (deliver p 42)) (deliver p 43) @p @waiter (realized? p))

(def d (delay (println "computing once") :done))
(println (realized? d) (delay? d) (force d) @d (realized? d) (force 5))
(println (let [bad (delay (throw (ex-info "bad delay" {})))]
           [(try @bad (catch Exception e (ex-message e)))
            (try @bad (catch Exception e (str "again: " (ex-message e))))]))

;; exceptions travel back through deref
(println (try @(future (throw (ex-info "in future" {:a 1}))) (catch Exception e [(ex-message e) (ex-data e)])))
(println (try @(future (/ 1 0)) (catch ArithmeticException e (ex-message e))))

;; a future runs with the bindings in force where it was created
(def ^:dynamic *x* :root)
(println (binding [*x* :bound] @(future *x*)) @(future *x*) (binding [*x* :outer] @(future (binding [*x* :inner] *x*))))

;; parallel maps
(println (pmap inc (range 10)) (pmap + [1 2 3] [10 20 30]) (pvalues (+ 1 1) (* 2 3)) (pcalls (fn [] :a) (fn [] :b)))
(println (reduce + (pmap (fn [i] (reduce + (range i))) (range 200))) (count (pmap (fn [x] (* x x)) (range 1000))))

;; locking makes a read-modify-write on an atom race-free
(def counter (atom 0))
(def lockee (atom nil))
(let [fs (doall (map (fn [_] (future (dotimes [_ 1000] (locking lockee (reset! counter (inc @counter)))))) (range 8)))]
  (doseq [f fs] @f))
(println @counter (try (locking 5 :x) (catch Exception e (ex-message e))) (locking lockee (locking lockee :reentrant)))

;; cancellation interrupts a sleeping future
(def slow (future (sleep 5000) :never))
(println (future-cancel slow) (future-cancelled? slow) (future-cancel slow) (try @slow (catch Exception e [(ex-message e) (instance? Exception e)])))
(def done (future :done))
(println @done (future-cancel done) (future-cancelled? done))

;; futures may start futures, and threads have ids
(println @(future @(future @(future :nested))) (= (thread-id) @(future (thread-id))) (pos? (available-processors)))
(println (type f1) (type p) (type d) (pending-kind f1) (pending-kind p) (pending-kind d) (pending-kind 1))
