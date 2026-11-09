;; deftype, defrecord, defprotocol, extend-type, extend-protocol, Printable.
(defprotocol Shape
  "Geometric shapes"
  (area [this] "Area of the shape")
  (perimeter [this])
  (scale [this k] [this kx ky]))

(defrecord Rect [w h]
  Shape
  (area [this] (* w h))
  (perimeter [_] (* 2 (+ w h)))
  (scale ([this k] (->Rect (* w k) (* h k)))
         ([this kx ky] (->Rect (* w kx) (* h ky)))))

(deftype Circle [r]
  Shape
  (area [this] (* 3 r r))
  (perimeter [this] (* 6 r))
  (scale ([this k] (Circle. (* r k))) ([this kx ky] (Circle. (* r kx)))))

(def r (->Rect 2 3))
(def c (Circle. 2))
(println (area r) (perimeter r) (area c) (perimeter c))
(println (scale r 2) (area (scale c 2 3)) (.-r (scale c 10)))
(println (satisfies? Shape r) (satisfies? Shape c) (satisfies? Shape 1) (extends? Shape Rect) (extends? Shape Long))
(println (type r) (type c) (instance? Rect r) (instance? Rect c) (instance? Object c) (instance? Object nil) (type nil) (record? r) (record? c))

;; records are maps
(println (:w r) (get r :h) (get r :zz :default) (contains? r :w) (contains? r :z) (assoc r :z 9) (:z (assoc r :z 9)))
(println (= r (->Rect 2 3)) (= r (assoc r :z 1)) (= (hash r) (hash (->Rect 2 3))) (= r {:w 2 :h 3}) (= c (Circle. 2)))
(println (map->Rect {:w 1 :h 2 :extra 3}) (keys (map->Rect {:w 1 :h 2 :extra 3})) (vals r) (count r) (seq r) (into {} r))
(println (dissoc r :w) (type (dissoc r :w)) (dissoc (assoc r :z 1) :z) (merge r {:h 10}) (type (merge r {:h 10})))
(println (meta (with-meta r {:a 1})) (:w (with-meta r {:a 1})) (vary-meta r assoc :b 2) (= r (with-meta r {:a 1})))
(println (map :w [r (->Rect 5 5)]) (reduce + (map area [r (->Rect 1 1)])) (:w (first (sort-by :w [(->Rect 9 1) r]))))
(println (try (.-nope r) (catch Exception e (ex-message e))))
(println (try (->Rect 1) (catch Exception e (ex-message e))))
(println (try (r :w) (catch Exception e (ex-message e))))
(println (try (get c :r) (catch Exception e (ex-message e))) (try (count c) (catch Exception e (ex-message e))))

;; extending built-in types, nil and Object
(extend-protocol Shape
  nil
  (area [_] 0)
  (perimeter [_] 0)
  (scale [_ k] nil)
  Long
  (area [n] n)
  (perimeter [n] (* 4 n))
  (scale [n k] (* n k)))
(println (area nil) (perimeter 5) (scale 5 3) (satisfies? Shape nil) (satisfies? Shape 1) (extends? Shape nil))
(extend-type String Shape (area [s] (count s)) (perimeter [s] 0) (scale [s k] (apply str (repeat k s))))
(println (area "hello") (scale "ab" 3))
(println (try (area :kw) (catch Exception e (ex-message e))))
(extend Keyword Shape {:area (fn [k] (count (name k))) :perimeter (fn [k] 0) :scale (fn [k n] k)})
(println (area :abc))
(extend-protocol Shape Object (area [o] -1) (perimeter [o] -1) (scale [o k] o))
(println (area 1.5) (area [1 2]) (satisfies? Shape :kw) (extends? Shape Double))
(println (sort (map type-name (remove nil? (extenders Shape)))))

;; a protocol method with one arity, dispatching on the first argument only
(defprotocol Greeter (greet [this other]))
(extend-protocol Greeter
  Rect (greet [this other] (str "rect greets " (pr-str other)))
  String (greet [s other] (str s " greets " other)))
(println (greet r 42) (greet "bob" "alice"))

;; printing via Printable
(defrecord Named [n]
  Printable
  (-pr [this readably?] (if readably? (str "#<Named " (pr-str n) ">") (str "Named:" n))))
(prn (->Named "x"))
(println (->Named "x") (str (->Named "y")) (pr-str [(->Named "z")]) (pr-str {:k (->Named "k")}))
(deftype Opaque [secret] Printable (-pr [this _] "#<Opaque>"))
(prn (Opaque. 1))
(println (.-secret (Opaque. 7)))

;; fields shadowed by parameters, and methods calling the protocol recursively
(defprotocol Depth (depth [this]))
(defrecord Node [left right]
  Depth
  (depth [this] (inc (max (depth left) (depth right)))))
(extend-protocol Depth nil (depth [_] 0))
(println (depth (->Node (->Node nil nil) (->Node (->Node nil nil) nil))))
(defrecord Pair [a b] Shape (area [this] (* a b)) (perimeter [a] (:b a)) (scale [this a] a))
(println (area (->Pair 3 4)) (perimeter (->Pair 3 4)) (scale (->Pair 3 4) :param-wins))

;; redefining a type at the REPL creates a new type; old instances keep the old one
(def old (->Rect 1 1))
(defrecord Rect [w h d])
(println (= old (->Rect 1 1 1)) (type old) (:d (->Rect 1 1 1)) (count (->Rect 1 1 1)))
(println (try (map->Rect [1 2]) (catch Exception e (ex-message e))) (try (extend Rect Shape [:area 1]) (catch Exception e (ex-message e))) (map? (->Rect 1 2 3)) (coll? (->Rect 1 2 3)))
