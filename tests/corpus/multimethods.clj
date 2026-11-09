;; defmulti / defmethod with keyword, value and composite dispatch.
(defmulti speak (fn [animal & _] (:kind animal)))
(defmethod speak :dog [a & more] (str (:name a) " says woof" (apply str more)))
(defmethod speak :cat [a] (str (:name a) " says meow"))
(defmethod speak :default [a] (str (:name a) " is silent"))
(println (speak {:kind :dog :name "Rex"} "!" "!") (speak {:kind :cat :name "Tom"}) (speak {:kind :fish :name "Nemo"}))

(defmulti sz "size class" count :default :big)
(defmethod sz 0 [_] :empty)
(defmethod sz 1 [_] :one)
(defmethod sz :big [_] :many)
(println (sz []) (sz [1]) (sz [1 2 3]) (sort-by str (keys (methods sz))) (fn? (get-method sz 0)) (fn? (get-method sz 99)) (nil? (get-method speak :bird)))
(remove-method sz 1)
(println (sz [1]))

(defmulti strict identity)
(println (try (strict 1) (catch Exception e (ex-message e))))
(defmethod strict 1 [x] :one)
(defmulti strict (fn [x] :changed))   ; like Clojure: an existing multimethod is kept
(println (strict 1) (:doc (meta (var sz))))
(println (multi? strict) (multi? sz) (multi? inc) (try (add-method* inc 1 inc) (catch Exception e (ex-message e))))

;; composite dispatch values and vector dispatch
(defmulti encounter (fn [x y] [(:species x) (:species y)]))
(defmethod encounter [:bunny :lion] [b l] :run-away)
(defmethod encounter [:lion :bunny] [l b] :eat)
(defmethod encounter [:lion :lion] [l1 l2] :fight)
(defmethod encounter :default [x y] :ignore)
(def b {:species :bunny})
(def l {:species :lion})
(println (encounter b l) (encounter l b) (encounter l l) (encounter b b))

;; dispatch on type, including records
(defrecord Celsius [deg])
(defrecord Fahrenheit [deg])
(defmulti to-celsius type)
(defmethod to-celsius Celsius [t] (:deg t))
(defmethod to-celsius Fahrenheit [t] (/ (* (- (:deg t) 32) 5) 9))
(defmethod to-celsius Long [n] n)
(defmethod to-celsius nil [_] :no-temp)
(println (to-celsius (->Celsius 20)) (to-celsius (->Fahrenheit 212)) (to-celsius 7) (to-celsius nil))

;; methods are closures and can recurse through the multimethod
(defmulti fact (fn [n] (if (zero? n) :base :step)))
(defmethod fact :base [_] 1)
(defmethod fact :step [n] (* n (fact (dec n))))
(println (fact 10) (map fact (range 6)))
(let [extra 100]
  (defmethod speak :robot [a] (str (:name a) " beeps " extra)))
(println (speak {:kind :robot :name "R2"}))
(remove-all-methods fact)
(println (try (fact 1) (catch Exception e (ex-message e))))
