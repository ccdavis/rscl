;; Unboxed long/double locals: inference from literals and arithmetic, ^long/^double hints,
;; overflow and division errors, mixed-type comparisons, loop demotion and boxing at boundaries.
(println (let [x 1 y 2.5] [(+ x y) (* x 2) (- y) (/ x 2) (/ y 2) (inc x) (dec y) (double x) (long y) (+ x 1) (rem 7 3) (quot -7 2) (mod -7 3) (mod 7 -3)]))
(println (loop [i 0 acc 0] (if (< i 10) (recur (inc i) (+ acc i)) acc)))
(println (loop [zr 0.0 i 0] (if (< i 5) (recur (+ zr 0.5) (inc i)) [zr i])))
(println (loop [x 1 y 1.0] (if (< x 3) (recur (inc x) (* y 2)) [x y])))
(defn f [^double a ^long b] [(+ a b) (* a 2) (inc b) (type a) (type b)])
(println (f 1 2) (f 1.5 3) (try (f "s" 1) (catch Exception e (ex-message e))) (try (f 1 1.5) (catch Exception e (ex-message e))))
(defn g [x] (let [d (double x)] (if (zero? d) :zero (if (pos? d) :pos :neg))))
(println (g 0) (g 3) (g -2.5) (try (g "a") (catch Exception e (ex-message e))))
(println (let [x 4611686018427387903] (+ x 1)) (let [x 9223372036854775807] (try (+ x 1) (catch Exception e (ex-message e)))) (try (let [x 1] (rem x 0)) (catch Exception e (ex-message e))))
(println (let [a -9223372036854775808 b -1] [(rem a b) (mod a b) (try (quot a b) (catch Exception e (ex-message e)))]))
(println (= 1 1.0) (let [a 1 b 1.0] (= a b)) (let [a 1.0 b 1.0] (= a b)) (let [a 2 b 2] (= a b)) (== 1 1.0) (let [a 1 b 1.0] (== a b)) (< 1 2.5) (let [a 1.5 b 1] (> a b)))
(println (loop [i 0 acc []] (if (< i 3) (recur (inc i) (conj acc (* i 1.5))) acc)))
(println (let [x 5] (str x) ) (let [d 2.5] (str d)) (let [f (fn [] (let [k 3.5] (fn [] (* k 2))))] ((f))))
(println (loop [i 0 s nil] (if (< i 3) (recur (inc i) (str s i)) s)))
(println (loop [x 1.0] (if (< x 100) (recur (* x 3)) x)) (loop [x 1] (if (< x 100) (recur (* x 3.0)) x)))
(println (let [^long n 7] (* n n)) (let [^double d 3] (* d d)) (try (let [^long n 1.5] n) (catch Exception e (ex-message e))))
(println (loop [i 0] (when (< i 3) (recur (inc i)))) (let [v [1 2 3]] (loop [i 0 s 0] (if (< i (count v)) (recur (inc i) (+ s (nth v i))) s))))
(println (reduce (fn [acc x] (+ acc (* x x))) 0 (range 5)) (let [m {:a 1}] (+ (:a m) 2.0)) (let [x 3] (if (> x 2) (* x 1.5) x)))
(println (- 5) (- 5.5) (let [x 5] (- x)) (let [x -9223372036854775808] (try (- x) (catch Exception e (ex-message e)))) (+) (*) (let [x 2.0] (/ x)) (let [x 2] (/ x)))
(defn fib [n] (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))
(println (fib 20) (let [n 10] (fib n)))
(defn iter [^double cr ^double ci max-iter] (loop [zr 0.0 zi 0.0 i 0] (if (or (>= i max-iter) (> (+ (* zr zr) (* zi zi)) 4.0)) i (recur (+ (- (* zr zr) (* zi zi)) cr) (+ (* 2.0 zr zi) ci) (inc i)))))
(println (iter 0.0 0.0 50) (iter 1.0 1.0 50) (iter -0.5 0.5 100))
;; a loop local demoted to a value because a recur rebinds it with a non-number
(println (loop [x 1 n 0] (if (< n 3) (recur (if (= n 1) "s" x) (inc n)) x)))
;; primitives escaping into collections, strings and closures keep their types
(println (let [i 3 d 1.5] [(vector i d) (str i "," d) ((fn [] [i d])) (type i) (type d)]))
;; hinted params in a fn that recurs
(defn count-down [^long n acc] (if (zero? n) acc (recur (dec n) (conj acc n))))
(println (count-down 3 []) (try (count-down 2.0 []) (catch Exception e (ex-message e))))
;; `=` across primitive types and with values
(println (let [a 1 b 1.0 c 1] [(= a b) (= a c) (= b 1.0) (= a (first [1])) (== a b)]))
