;; A Scheme interpreter: reader, evaluator with proper tail calls, closures,
;; non-hygienic macros, escaping continuations, error handling, vectors and hash
;; tables, plus a suite of Scheme programs run through it. A larger, more
;; "real-world" program than the rest of the corpus: records, protocols,
;; multimethods, atoms, exceptions, lazy seqs and BigInts all meet here.
(ns scheme
  (:require [clojure.string :as str]))

;;; ---- data ---------------------------------------------------------------------------

(defrecord Pair [car cdr])          ; an improper pair; proper lists are rscl lists
(defrecord Lambda [params rest-param body env name])
(defrecord Prim [name arity f])
(defrecord Macro [expander])
(defrecord Cont [id])               ; an escaping continuation
(defrecord SVec [items])            ; a mutable vector: items is an atom
(defrecord STable [entries])        ; a mutable hash table: entries is an atom
(defrecord SChar [ch])
(defrecord Tail [expr env])         ; "evaluate this in tail position"
(defrecord Env [vars parent])

(def unspecified (symbol "#<unspecified>"))
(def eof (symbol "#<eof>"))

(defn scm-error [msg & irritants]
  (throw (ex-info msg {:scheme-error true :irritants (vec irritants)})))

;;; ---- reader -------------------------------------------------------------------------

(def delimiters #{\( \) \space \newline \tab \" \; \'})

(defn digit? [c] (and c (<= 48 (int c) 57)))

(defn parse-atom [tok]
  (cond
    (= tok "#t") true
    (= tok "#f") false
    (str/starts-with? tok "#\\")
    (let [n (subs tok 2)]
      (->SChar (cond (= n "space") \space
                     (= n "newline") \newline
                     :else (first n))))
    :else
    (let [c (first tok)
          c2 (second tok)]
      (if (or (digit? c) (and (or (= c \-) (= c \+) (= c \.)) (digit? c2)))
        (if (or (str/includes? tok ".") (str/includes? tok "e"))
          (parse-double tok)
          (bigint tok))
        (symbol tok)))))

(defn normalize-int
  "Integers are read as BigInts so literals of any size work; small ones become longs."
  [n]
  (if (and (bigint? n) (< -4611686018427387904 n 4611686018427387904)) (long n) n))

(defn tokenize
  "Source text -> vector of tokens. Strings become [:string s]."
  [src]
  (let [cs (vec src)
        n (count cs)]
    (loop [i 0 toks []]
      (if (>= i n)
        toks
        (let [c (nth cs i)]
          (cond
            (or (= c \space) (= c \newline) (= c \tab)) (recur (inc i) toks)
            (= c \;) (recur (loop [j i] (if (and (< j n) (not= (nth cs j) \newline)) (recur (inc j)) j))
                            toks)
            (or (= c \() (= c \))) (recur (inc i) (conj toks (str c)))
            (= c \') (recur (inc i) (conj toks "'"))
            (= c \`) (recur (inc i) (conj toks "`"))
            (= c \,) (if (and (< (inc i) n) (= (nth cs (inc i)) \@))
                       (recur (+ i 2) (conj toks ",@"))
                       (recur (inc i) (conj toks ",")))
            (= c \") (let [[s j] (loop [j (inc i) acc []]
                                   (when (>= j n) (scm-error "unterminated string"))
                                   (let [d (nth cs j)]
                                     (cond
                                       (= d \") [(apply str acc) (inc j)]
                                       (= d \\) (let [e (nth cs (inc j))]
                                                  (recur (+ j 2)
                                                         (conj acc (case e \n \newline \t \tab e))))
                                       :else (recur (inc j) (conj acc d)))))]
                       (recur j (conj toks [:string s])))
            (and (= c \#) (< (inc i) n) (= (nth cs (inc i)) \())
            (recur (+ i 2) (conj toks "#("))
            (and (= c \#) (< (inc i) n) (= (nth cs (inc i)) \\))
            ;; a character: #\x or #\space; take the char after the backslash even
            ;; when it is a delimiter
            (let [j (loop [j (+ i 3)]
                      (if (and (< j n) (not (contains? delimiters (nth cs j)))) (recur (inc j)) j))]
              (recur j (conj toks (apply str (subvec cs i j)))))
            :else
            (let [j (loop [j i]
                      (if (and (< j n) (not (contains? delimiters (nth cs j)))) (recur (inc j)) j))]
              (recur j (conj toks (apply str (subvec cs i j)))))))))))

(declare read-form)

(defn read-list
  "Read forms up to the closing paren. Returns [list next-index]; handles dotted tails."
  [toks i]
  (loop [i i acc []]
    (when (>= i (count toks)) (scm-error "unexpected end of input"))
    (let [t (nth toks i)]
      (cond
        (= t ")") [(apply list acc) (inc i)]
        (= t ".") (let [[tail j] (read-form toks (inc i))]
                    (when (not= (nth toks j) ")") (scm-error "bad dotted list"))
                    [(reduce (fn [t x] (if (seq? t) (cons x t) (->Pair x t)))
                             tail (reverse acc))
                     (inc j)])
        :else (let [[x j] (read-form toks i)] (recur j (conj acc x)))))))

(defn read-form [toks i]
  (when (>= i (count toks)) (scm-error "unexpected end of input"))
  (let [t (nth toks i)]
    (cond
      (vector? t) [(second t) (inc i)]
      (= t "(") (read-list toks (inc i))
      (= t "#(") (let [[xs j] (read-list toks (inc i))] [(->SVec (atom (vec xs))) j])
      (= t ")") (scm-error "unexpected )")
      (= t "'") (let [[x j] (read-form toks (inc i))] [(list 'quote x) j])
      (= t "`") (let [[x j] (read-form toks (inc i))] [(list 'quasiquote x) j])
      (= t ",") (let [[x j] (read-form toks (inc i))] [(list 'unquote x) j])
      (= t ",@") (let [[x j] (read-form toks (inc i))] [(list 'unquote-splicing x) j])
      :else (let [a (parse-atom t)] [(if (number? a) (normalize-int a) a) (inc i)]))))

(defn read-all [src]
  (let [toks (tokenize src)]
    (loop [i 0 forms []]
      (if (>= i (count toks))
        forms
        (let [[x j] (read-form toks i)] (recur j (conj forms x)))))))

;;; ---- printer ------------------------------------------------------------------------

(defprotocol Show
  (show [x readable?] "The printed form of a Scheme value."))

(declare show-value)

(defn show-list [xs readable?]
  (str "(" (str/join " " (map #(show-value % readable?) xs)) ")"))

(extend-protocol Show
  Pair
  (show [p readable?]
    (loop [p p parts []]
      (cond
        (instance? Pair p) (recur (:cdr p) (conj parts (show-value (:car p) readable?)))
        :else (str "(" (str/join " " parts) " . " (show-value p readable?) ")"))))
  Lambda
  (show [f _] (str "#<procedure " (or (:name f) "anonymous") ">"))
  Prim
  (show [f _] (str "#<primitive " (:name f) ">"))
  Macro
  (show [_ _] "#<macro>")
  Cont
  (show [_ _] "#<continuation>")
  SVec
  (show [v readable?] (str "#" (show-list @(:items v) readable?)))
  STable
  (show [t _] (str "#<hash-table " (count @(:entries t)) ">"))
  SChar
  (show [c readable?]
    (if readable?
      (str "#\\" (case (:ch c) \space "space" \newline "newline" (:ch c)))
      (str (:ch c)))))

(defn show-value [x readable?]
  (cond
    (true? x) "#t"
    (false? x) "#f"
    (nil? x) "()"
    (string? x) (if readable? (pr-str x) x)
    (bigint? x) (str x)
    (number? x) (str x)
    (symbol? x) (str x)
    (seq? x) (show-list x readable?)
    (satisfies? Show x) (show x readable?)
    :else (str "#<" (pr-str x) ">")))

;;; ---- environments -------------------------------------------------------------------

(defn new-env [parent] (->Env (atom {}) parent))

(defn lookup [env sym]
  (loop [e env]
    (if (nil? e)
      (scm-error (str "unbound variable: " sym))
      (let [vars @(:vars e)]
        (if (contains? vars sym) (get vars sym) (recur (:parent e)))))))

(defn define! [env sym v]
  (swap! (:vars env) assoc sym v)
  unspecified)

(defn set-var! [env sym v]
  (loop [e env]
    (cond
      (nil? e) (scm-error (str "set!: unbound variable: " sym))
      (contains? @(:vars e) sym) (do (swap! (:vars e) assoc sym v) unspecified)
      :else (recur (:parent e)))))

(defn bind-params [f args]
  (let [params (:params f)
        np (count params)
        na (count args)
        env (new-env (:env f))]
    (when (or (< na np) (and (> na np) (nil? (:rest-param f))))
      (scm-error (str (or (:name f) "procedure") ": expected " np
                      (if (:rest-param f) " or more" "") " arguments, got " na)))
    (reset! (:vars env)
            (cond-> (zipmap params args)
              (:rest-param f) (assoc (:rest-param f) (apply list (drop np args)))))
    env))

;;; ---- evaluator ----------------------------------------------------------------------

(declare scm-eval scm-apply)

(defn truthy? [x] (not (false? x)))

(defn eval-body
  "Evaluate all but the last form; the last one is returned as a tail call."
  [body env]
  (if (empty? body)
    unspecified
    (loop [body body]
      (if (empty? (rest body))
        (->Tail (first body) env)
        (do (scm-eval (first body) env) (recur (rest body)))))))

(defn parse-params
  "A Scheme parameter list -> [fixed-params rest-param]."
  [params]
  (cond
    (symbol? params) [[] params]
    (seq? params) [(vec params) nil]
    (nil? params) [[] nil]
    :else (loop [p params fixed []]
            (if (instance? Pair p)
              (recur (:cdr p) (conj fixed (:car p)))
              [fixed p]))))

(defn make-lambda [params body env name]
  (let [[fixed rest-param] (parse-params params)]
    (->Lambda fixed rest-param body env name)))

(defn quasi [x env depth]
  (cond
    (not (seq? x)) x
    (empty? x) x
    (= (first x) 'unquote)
    (if (= depth 1) (scm-eval (second x) env) (list 'unquote (quasi (second x) env (dec depth))))
    (= (first x) 'quasiquote) (list 'quasiquote (quasi (second x) env (inc depth)))
    :else
    (apply list
           (mapcat (fn [item]
                     (if (and (seq? item) (seq item) (= (first item) 'unquote-splicing) (= depth 1))
                       (scm-eval (second item) env)
                       [(quasi item env depth)]))
                   x))))

;; Special forms dispatch on the head symbol. Each method returns a value or a Tail.
(defmulti special (fn [x env] (first x)))

(defmethod special 'quote [x env] (second x))
(defmethod special 'quasiquote [x env] (quasi (second x) env 1))

(defmethod special 'if [x env]
  (let [[_ test then else] x]
    (if (truthy? (scm-eval test env))
      (->Tail then env)
      (if (> (count x) 3) (->Tail else env) unspecified))))

(defmethod special 'define [x env]
  (let [target (second x)]
    (if (symbol? target)
      (define! env target (if (> (count x) 2) (scm-eval (nth x 2) env) unspecified))
      ;; (define (name . params) body...)
      (let [[name params] (if (seq? target)
                            [(first target) (apply list (rest target))]
                            [(:car target) (:cdr target)])]
        (define! env name (make-lambda params (drop 2 x) env (str name)))))))

(defmethod special 'set! [x env]
  (set-var! env (second x) (scm-eval (nth x 2) env)))

(defmethod special 'lambda [x env]
  (make-lambda (second x) (drop 2 x) env nil))

(defmethod special 'begin [x env] (eval-body (rest x) env))

(defmethod special 'let [x env]
  (if (symbol? (second x))
    ;; named let: a local recursive procedure, called once
    (let [[_ name bindings & body] x
          loop-env (new-env env)
          f (make-lambda (apply list (map first bindings)) body loop-env (str name))]
      (define! loop-env name f)
      (->Tail (cons name (map second bindings)) loop-env))
    (let [[_ bindings & body] x
          inner (new-env env)]
      (doseq [[name init] bindings]
        (define! inner name (scm-eval init env)))
      (eval-body body inner))))

(defmethod special 'let* [x env]
  (let [[_ bindings & body] x
        inner (reduce (fn [e [name init]]
                        (let [e2 (new-env e)]
                          (define! e2 name (scm-eval init e))
                          e2))
                      env bindings)]
    (eval-body body (new-env inner))))

(defmethod special 'letrec [x env]
  (let [[_ bindings & body] x
        inner (new-env env)]
    (doseq [[name init] bindings]
      (define! inner name (scm-eval init inner)))
    (eval-body body inner)))

(defmethod special 'cond [x env]
  (loop [clauses (rest x)]
    (if (empty? clauses)
      unspecified
      (let [[test & body] (first clauses)]
        (if (= test 'else)
          (eval-body body env)
          (let [v (scm-eval test env)]
            (cond
              (not (truthy? v)) (recur (rest clauses))
              (empty? body) v
              (= (first body) '=>) (scm-apply (scm-eval (second body) env) [v])
              :else (eval-body body env))))))))

(defmethod special 'case [x env]
  (let [k (scm-eval (second x) env)]
    (loop [clauses (drop 2 x)]
      (if (empty? clauses)
        unspecified
        (let [[data & body] (first clauses)]
          (if (or (= data 'else) (some #(= % k) data))
            (eval-body body env)
            (recur (rest clauses))))))))

(defmethod special 'and [x env]
  (loop [forms (rest x)]
    (cond
      (empty? forms) true
      (empty? (rest forms)) (->Tail (first forms) env)
      (truthy? (scm-eval (first forms) env)) (recur (rest forms))
      :else false)))

(defmethod special 'or [x env]
  (loop [forms (rest x)]
    (cond
      (empty? forms) false
      (empty? (rest forms)) (->Tail (first forms) env)
      :else (let [v (scm-eval (first forms) env)]
              (if (truthy? v) v (recur (rest forms)))))))

(defmethod special 'when [x env]
  (if (truthy? (scm-eval (second x) env)) (eval-body (drop 2 x) env) unspecified))

(defmethod special 'unless [x env]
  (if (truthy? (scm-eval (second x) env)) unspecified (eval-body (drop 2 x) env)))

(defmethod special 'do [x env]
  ;; (do ((var init step)...) (test result...) body...)
  (let [[_ specs [test & result] & body] x
        vars (map first specs)]
    (loop [vals (mapv #(scm-eval (second %) env) specs)]
      (let [e (new-env env)]
        (reset! (:vars e) (zipmap vars vals))
        (if (truthy? (scm-eval test e))
          (eval-body result e)
          (do (doseq [form body] (scm-eval form e))
              (recur (mapv (fn [spec v]
                             (if (> (count spec) 2) (scm-eval (nth spec 2) e) v))
                           specs vals))))))))

(defmethod special 'define-macro [x env]
  ;; (define-macro (name . params) body...): the body computes the expansion
  (let [target (second x)
        [name params] (if (seq? target)
                        [(first target) (apply list (rest target))]
                        [(:car target) (:cdr target)])]
    (define! env name (->Macro (make-lambda params (drop 2 x) env (str name))))))

(defmethod special 'delay [x env]
  (let [state (atom nil)
        expr (second x)]
    (->Prim "promise" 0 (fn []
                          (when (nil? @state) (reset! state [(scm-eval expr env)]))
                          (first @state)))))

(defmethod special 'guard [x env]
  ;; (guard (e clause...) body...): clauses are cond clauses with e bound
  (let [[_ [evar & clauses] & body] x]
    (try
      (scm-eval (cons 'begin body) env)
      (catch ExceptionInfo ex
        (if (:scheme-escape (ex-data ex))
          (throw ex)
          (let [handler-env (new-env env)
                condition (if (contains? (ex-data ex) :raised)
                            (:raised (ex-data ex))
                            (ex-message ex))]
            (define! handler-env evar condition)
            (define! handler-env 'condition/irritants
              (apply list (:irritants (ex-data ex))))
            (scm-eval (cons 'cond clauses) handler-env)))))))

(def special-names (set (keys (methods special))))

(defn find-macro [env sym]
  (loop [e env]
    (when e
      (let [vars @(:vars e)]
        (if (contains? vars sym)
          (let [v (get vars sym)] (when (instance? Macro v) v))
          (recur (:parent e)))))))

(defn shadowed? [env sym]
  (loop [e env]
    (cond
      (nil? e) false
      (contains? @(:vars e) sym) true
      :else (recur (:parent e)))))

(defn scm-eval [x env]
  (loop [x x env env]
    (cond
      (symbol? x) (lookup env x)
      (not (seq? x)) x
      (empty? x) (scm-error "cannot evaluate ()")
      :else
      (let [op (first x)]
        (if (and (symbol? op) (contains? special-names op) (not (shadowed? env op)))
          (let [r (special x env)]
            (if (instance? Tail r) (recur (:expr r) (:env r)) r))
          (let [macro (when (symbol? op) (find-macro env op))]
            (if macro
              (recur (scm-apply (:expander macro) (rest x)) env)
              (let [f (scm-eval op env)
                    args (mapv #(scm-eval % env) (rest x))]
                (if (instance? Lambda f)
                  ;; a tail call: bind and loop instead of growing the stack
                  (let [call-env (bind-params f args)
                        body (:body f)]
                    (if (empty? body)
                      unspecified
                      (do (doseq [form (butlast body)] (scm-eval form call-env))
                          (recur (last body) call-env))))
                  (scm-apply f args))))))))))

(defn scm-apply [f args]
  (cond
    (instance? Lambda f)
    (let [env (bind-params f args)
          body (:body f)]
      (if (empty? body)
        unspecified
        (do (doseq [form (butlast body)] (scm-eval form env))
            (scm-eval (last body) env))))
    (instance? Prim f)
    (do (when (and (:arity f) (not= (:arity f) (count args)))
          (scm-error (str (:name f) ": expected " (:arity f) " arguments, got " (count args))))
        (apply (:f f) args))
    (instance? Cont f)
    (throw (ex-info "continuation" {:scheme-escape (:id f) :value (first args)}))
    :else (scm-error (str "not a procedure: " (show-value f true)))))

;;; ---- primitives ---------------------------------------------------------------------

(defn scm-list? [x] (or (nil? x) (seq? x)))

(defn scm-car [x]
  (cond
    (instance? Pair x) (:car x)
    (and (seq? x) (seq x)) (first x)
    :else (scm-error (str "car: not a pair: " (show-value x true)))))

(defn scm-cdr [x]
  (cond
    (instance? Pair x) (:cdr x)
    (and (seq? x) (seq x)) (apply list (rest x))
    :else (scm-error (str "cdr: not a pair: " (show-value x true)))))

(defn scm-cons [a d]
  (if (scm-list? d) (cons a (or d ())) (->Pair a d)))

(defn check-numbers [name xs]
  (doseq [x xs]
    (when-not (number? x) (scm-error (str name ": not a number: " (show-value x true))))))

(defn numeric [name f]
  (fn [& xs] (check-numbers name xs) (normalize-int (apply f xs))))

(defn compare-chain [name f]
  (fn [& xs] (check-numbers name xs) (apply f xs)))

(defn scm-equal? [a b]
  (cond
    (and (instance? SVec a) (instance? SVec b)) (= @(:items a) @(:items b))
    (and (number? a) (number? b)) (and (= (integer? a) (integer? b)) (== a b))
    :else (= a b)))

(defn scm-eqv? [a b]
  (cond
    (and (number? a) (number? b)) (scm-equal? a b)
    (and (instance? SChar a) (instance? SChar b)) (= a b)
    (or (symbol? a) (true? a) (false? a) (nil? a)) (= a b)
    (and (seq? a) (empty? a) (seq? b) (empty? b)) true
    :else (identical? a b)))

(def output (atom []))

(defn emit [s] (swap! output conj s) unspecified)

(def cont-counter (atom 0))

(defn call-cc [f]
  (let [id (swap! cont-counter inc)]
    (try
      (scm-apply f [(->Cont id)])
      (catch ExceptionInfo ex
        (if (= (:scheme-escape (ex-data ex)) id)
          (:value (ex-data ex))
          (throw ex))))))

(defn prim [name arity f] [(symbol name) (->Prim name arity f)])

(def primitives
  (into {}
        [(prim "+" nil (numeric "+" +'))
         (prim "-" nil (numeric "-" -'))
         (prim "*" nil (numeric "*" *'))
         (prim "/" nil (numeric "/" /))
         (prim "quotient" 2 (numeric "quotient" quot))
         (prim "remainder" 2 (numeric "remainder" rem))
         (prim "modulo" 2 (numeric "modulo" mod))
         (prim "=" nil (compare-chain "=" ==))
         (prim "<" nil (compare-chain "<" <))
         (prim ">" nil (compare-chain ">" >))
         (prim "<=" nil (compare-chain "<=" <=))
         (prim ">=" nil (compare-chain ">=" >=))
         (prim "abs" 1 (fn [x] (if (neg? x) (-' x) x)))
         (prim "min" nil min)
         (prim "max" nil max)
         (prim "number?" 1 number?)
         (prim "integer?" 1 integer?)
         (prim "zero?" 1 zero?)
         (prim "positive?" 1 pos?)
         (prim "negative?" 1 neg?)
         (prim "even?" 1 even?)
         (prim "odd?" 1 odd?)
         (prim "exact->inexact" 1 double)
         (prim "truncate" 1 (fn [x] (long x)))
         (prim "square" 1 (fn [x] (normalize-int (*' x x))))
         (prim "number->string" 1 str)
         (prim "string->number" 1 (fn [s] (or (parse-long s) (parse-double s) false)))
         (prim "not" 1 false?)
         (prim "eq?" 2 scm-eqv?)
         (prim "eqv?" 2 scm-eqv?)
         (prim "equal?" 2 scm-equal?)
         (prim "boolean?" 1 boolean?)
         (prim "symbol?" 1 symbol?)
         (prim "string?" 1 string?)
         (prim "char?" 1 (fn [x] (instance? SChar x)))
         (prim "procedure?" 1 (fn [x] (or (instance? Lambda x) (instance? Prim x) (instance? Cont x))))
         (prim "null?" 1 (fn [x] (or (nil? x) (and (seq? x) (empty? x)))))
         (prim "pair?" 1 (fn [x] (or (instance? Pair x) (and (seq? x) (boolean (seq x))))))
         (prim "list?" 1 scm-list?)
         (prim "cons" 2 scm-cons)
         (prim "car" 1 scm-car)
         (prim "cdr" 1 scm-cdr)
         (prim "cadr" 1 (fn [x] (scm-car (scm-cdr x))))
         (prim "cddr" 1 (fn [x] (scm-cdr (scm-cdr x))))
         (prim "caddr" 1 (fn [x] (scm-car (scm-cdr (scm-cdr x)))))
         (prim "list" nil (fn [& xs] (apply list xs)))
         (prim "length" 1 count)
         (prim "append" nil (fn [& ls] (apply list (apply concat ls))))
         (prim "reverse" 1 (fn [l] (apply list (reverse l))))
         (prim "list-ref" 2 (fn [l i] (nth l i)))
         (prim "list-tail" 2 (fn [l k] (apply list (drop k l))))
         (prim "memq" 2 (fn [x l] (let [t (drop-while #(not (scm-eqv? x %)) l)]
                                    (if (seq t) (apply list t) false))))
         (prim "member" 2 (fn [x l] (let [t (drop-while #(not (scm-equal? x %)) l)]
                                      (if (seq t) (apply list t) false))))
         (prim "assq" 2 (fn [k al] (or (first (filter #(scm-eqv? k (scm-car %)) al)) false)))
         (prim "assoc" 2 (fn [k al] (or (first (filter #(scm-equal? k (scm-car %)) al)) false)))
         (prim "map" nil (fn [f & ls] (apply list (apply map (fn [& xs] (scm-apply f xs)) ls))))
         (prim "for-each" nil (fn [f & ls]
                                (dorun (apply map (fn [& xs] (scm-apply f xs)) ls))
                                unspecified))
         (prim "filter" 2 (fn [f l] (apply list (filter #(truthy? (scm-apply f [%])) l))))
         (prim "fold-left" 3 (fn [f init l] (reduce (fn [acc x] (scm-apply f [acc x])) init l)))
         (prim "fold-right" 3 (fn [f init l]
                                (reduce (fn [acc x] (scm-apply f [x acc])) init (reverse l))))
         (prim "apply" nil (fn [f & args]
                             (scm-apply f (concat (butlast args) (last args)))))
         (prim "sort" 2 (fn [l less?]
                          (apply list (sort (fn [a b]
                                              (cond (truthy? (scm-apply less? [a b])) -1
                                                    (truthy? (scm-apply less? [b a])) 1
                                                    :else 0))
                                            l))))
         (prim "iota" nil (fn [n & [start]] (apply list (range (or start 0) (+ (or start 0) n)))))
         (prim "string-append" nil str)
         (prim "string-length" 1 count)
         (prim "substring" 3 subs)
         (prim "string-upcase" 1 str/upper-case)
         (prim "string-downcase" 1 str/lower-case)
         (prim "string=?" 2 =)
         (prim "string<?" 2 (fn [a b] (neg? (compare a b))))
         (prim "string->symbol" 1 symbol)
         (prim "symbol->string" 1 str)
         (prim "string->list" 1 (fn [s] (apply list (map ->SChar s))))
         (prim "list->string" 1 (fn [l] (apply str (map :ch l))))
         (prim "string-split" 2 (fn [s sep] (apply list (remove empty? (str/split s (re-pattern sep))))))
         (prim "string-join" 2 (fn [l sep] (str/join sep l)))
         (prim "char->integer" 1 (fn [c] (int (:ch c))))
         (prim "integer->char" 1 (fn [n] (->SChar (char n))))
         (prim "char-upcase" 1 (fn [c] (->SChar (first (str/upper-case (str (:ch c)))))))
         (prim "vector" nil (fn [& xs] (->SVec (atom (vec xs)))))
         (prim "make-vector" 2 (fn [n fill] (->SVec (atom (vec (repeat n fill))))))
         (prim "vector?" 1 (fn [x] (instance? SVec x)))
         (prim "vector-length" 1 (fn [v] (count @(:items v))))
         (prim "vector-ref" 2 (fn [v i]
                                (let [items @(:items v)]
                                  (if (and (integer? i) (< -1 i (count items)))
                                    (nth items i)
                                    (scm-error (str "vector-ref: index out of range: " i))))))
         (prim "vector-set!" 3 (fn [v i x] (swap! (:items v) assoc i x) unspecified))
         (prim "vector->list" 1 (fn [v] (apply list @(:items v))))
         (prim "list->vector" 1 (fn [l] (->SVec (atom (vec l)))))
         (prim "make-hash-table" 0 (fn [] (->STable (atom {}))))
         (prim "hash-table-set!" 3 (fn [t k v] (swap! (:entries t) assoc k v) unspecified))
         (prim "hash-table-ref" 3 (fn [t k default] (get @(:entries t) k default)))
         (prim "hash-table-update!" 4 (fn [t k f default]
                                        (swap! (:entries t)
                                               (fn [m] (assoc m k (scm-apply f [(get m k default)]))))
                                        unspecified))
         (prim "hash-table-count" 1 (fn [t] (count @(:entries t))))
         (prim "hash-table-keys" 1 (fn [t] (apply list (keys @(:entries t)))))
         (prim "hash-table->alist" 1 (fn [t] (apply list (map (fn [[k v]] (->Pair k v)) @(:entries t)))))
         (prim "display" 1 (fn [x] (emit (show-value x false))))
         (prim "write" 1 (fn [x] (emit (show-value x true))))
         (prim "newline" 0 (fn [] (emit "\n")))
         (prim "error" nil (fn [msg & irritants] (apply scm-error msg irritants)))
         (prim "raise" 1 (fn [x] (throw (ex-info "raise" {:raised x :irritants []}))))
         (prim "force" 1 (fn [p] (scm-apply p [])))
         (prim "call/cc" 1 call-cc)
         (prim "call-with-current-continuation" 1 call-cc)
         (prim "gensym" 0 (fn [] (gensym "g")))
         (prim "void" 0 (fn [] unspecified))]))

(defn global-env []
  (let [env (new-env nil)]
    (reset! (:vars env) primitives)
    env))

;;; ---- driver -------------------------------------------------------------------------

(defn run
  "Evaluate every form of `src` in a fresh global environment. Returns the output
  the program wrote, followed by the printed value of its last form."
  [src]
  (reset! output [])
  (let [env (global-env)
        result (try
                 (reduce (fn [_ form] (scm-eval form env)) unspecified (read-all src))
                 (catch ExceptionInfo ex
                   (if (:scheme-escape (ex-data ex))
                     (symbol "#<stale continuation>")
                     (symbol (str "#<error: " (ex-message ex)
                                  (apply str (map #(str " " (show-value % true))
                                                  (:irritants (ex-data ex))))
                                  ">")))))]
    (str (apply str @output)
         (if (= result unspecified) "" (str "=> " (show-value result true))))))

(def programs
  [["arithmetic"
    "(display (+ 1 2 3)) (newline)
     (display (* 1.5 4)) (newline)
     (display (/ 10 4)) (newline)
     (display (quotient 17 5)) (display \" \") (display (remainder 17 5)) (display \" \")
     (display (modulo -7 3)) (newline)
     (list (max 3 9 2) (min 3 9 2) (abs -5) (square 12) (exact->inexact 1))"]

   ["factorial"
    "(define (fact n) (if (= n 0) 1 (* n (fact (- n 1)))))
     (display (fact 10)) (newline)
     (fact 30)"]

   ["fibonacci"
    "(define (fib n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))
     (map fib (iota 15))"]

   ["tail-calls"
    ";; deep iteration: only works with proper tail calls
     (define (count-up n acc) (if (= n 0) acc (count-up (- n 1) (+ acc n))))
     (define (my-even? n) (if (= n 0) #t (my-odd? (- n 1))))
     (define (my-odd? n) (if (= n 0) #f (my-even? (- n 1))))
     (list (count-up 30000 0) (my-even? 10000) (my-odd? 10000))"]

   ["closures"
    "(define (make-counter)
       (let ((n 0))
         (lambda () (set! n (+ n 1)) n)))
     (define c1 (make-counter))
     (define c2 (make-counter))
     (c1) (c1) (c2)
     (define (make-adder k) (lambda (x) (+ x k)))
     (list (c1) (c2) ((make-adder 10) 5) (map (make-adder 100) '(1 2 3)))"]

   ["lists"
    "(define xs '(3 1 4 1 5 9 2 6))
     (display (length xs)) (newline)
     (display (reverse xs)) (newline)
     (display (append '(a b) '(c) '() '(d e))) (newline)
     (display (cons 1 2)) (display (cons 1 '(2 3))) (display '(1 2 . 3)) (newline)
     (display (list (car xs) (cadr xs) (cddr xs) (list-ref xs 4) (list-tail xs 6))) (newline)
     (display (assq 'b '((a 1) (b 2) (c 3)))) (display (member 4 xs)) (display (memq 'z '(x y)))
     (newline)
     (list (filter odd? xs) (fold-left + 0 xs) (fold-right cons '() xs) (apply + 1 2 '(3 4)))"]

   ["sorting"
    "(define (quicksort l)
       (if (null? l)
           '()
           (let ((p (car l)) (rest (cdr l)))
             (append (quicksort (filter (lambda (x) (< x p)) rest))
                     (list p)
                     (quicksort (filter (lambda (x) (>= x p)) rest))))))
     (define (merge a b)
       (cond ((null? a) b)
             ((null? b) a)
             ((< (car a) (car b)) (cons (car a) (merge (cdr a) b)))
             (else (cons (car b) (merge a (cdr b))))))
     (define (split l)
       (if (or (null? l) (null? (cdr l)))
           (list l '())
           (let ((r (split (cddr l))))
             (list (cons (car l) (car r)) (cons (cadr l) (cadr r))))))
     (define (merge-sort l)
       (if (or (null? l) (null? (cdr l)))
           l
           (let ((halves (split l)))
             (merge (merge-sort (car halves)) (merge-sort (cadr halves))))))
     (define data '(31 4 15 92 65 35 89 79 3 23 84 62 64 33))
     (display (quicksort data)) (newline)
     (display (merge-sort data)) (newline)
     (equal? (quicksort data) (sort data <))"]

   ["let-forms"
    "(define (f x)
       (let* ((a (+ x 1)) (b (* a 2)))
         (letrec ((even? (lambda (n) (if (= n 0) #t (odd? (- n 1)))))
                  (odd? (lambda (n) (if (= n 0) #f (even? (- n 1))))))
           (list a b (even? b)))))
     (display (f 4)) (newline)
     (define (sum-to n)
       (let loop ((i 1) (acc 0))
         (if (> i n) acc (loop (+ i 1) (+ acc i)))))
     (display (sum-to 100)) (newline)
     (do ((i 0 (+ i 1)) (acc '() (cons (* i i) acc)))
         ((= i 6) (reverse acc)))"]

   ["conditionals"
    "(define (classify n)
       (cond ((< n 0) 'negative)
             ((= n 0) 'zero)
             ((assv-like n) => (lambda (name) name))
             (else 'many)))
     (define (assv-like n) (case n ((1) 'one) ((2 3) 'few) (else #f)))
     (display (map classify '(-5 0 1 3 10))) (newline)
     (display (list (and 1 2 3) (and 1 #f 3) (and) (or #f 2) (or #f #f) (or))) (newline)
     (when (> 3 2) (display \"when-yes\"))
     (unless (> 3 2) (display \"unless-no\"))
     (newline)
     (if #f #f)"]

   ["quasiquote-and-macros"
    "(define x 5)
     (display `(x is ,x and twice is ,(* 2 x) list ,@(list 1 2 3) end)) (newline)
     (define-macro (swap! a b)
       (let ((tmp (gensym)))
         `(let ((,tmp ,a)) (set! ,a ,b) (set! ,b ,tmp))))
     (define-macro (while test . body)
       `(let loop () (when ,test ,@body (loop))))
     (define-macro (my-or a b)
       `(let ((t ,a)) (if t t ,b)))
     (define p 1) (define q 2)
     (swap! p q)
     (define i 0) (define total 0)
     (while (< i 10) (set! total (+ total i)) (set! i (+ i 1)))
     (list p q total (my-or #f 7))"]

   ["vectors"
    "(define (sieve n)
       (let ((marks (make-vector (+ n 1) #t)))
         (let loop ((i 2) (primes '()))
           (if (> i n)
               (reverse primes)
               (if (vector-ref marks i)
                   (begin
                     (do ((j (* i i) (+ j i))) ((> j n)) (vector-set! marks j #f))
                     (loop (+ i 1) (cons i primes)))
                   (loop (+ i 1) primes))))))
     (display (sieve 60)) (newline)
     (define v (vector 1 2 3))
     (vector-set! v 1 'two)
     (list v (vector-length v) (vector->list v) (list->vector '(a b)) #(1 (2) \"s\"))"]

   ["hash-tables"
    "(define (word-frequencies text)
       (let ((table (make-hash-table)))
         (for-each (lambda (w) (hash-table-update! table w (lambda (n) (+ n 1)) 0))
                   (string-split text \" \"))
         table))
     (define t (word-frequencies \"the quick brown fox jumps over the lazy dog the end\"))
     (display (hash-table-count t)) (newline)
     (display (hash-table-ref t \"the\" 0)) (display (hash-table-ref t \"cat\" 'none)) (newline)
     (sort (map car (filter (lambda (p) (> (cdr p) 1)) (hash-table->alist t))) string<?)"]

   ["strings-and-chars"
    "(define s \"Hello, Scheme\")
     (display (string-length s)) (newline)
     (display (string-upcase (substring s 7 13))) (newline)
     (write (string-append s \"!\" (number->string 42))) (newline)
     (write (map char-upcase (string->list \"abc\"))) (newline)
     (write (list (list->string (reverse (string->list \"stressed\")))
                  (string->number \"12\") (string->number \"x\")
                  (symbol->string 'sym) (string->symbol \"made\")
                  (char->integer #\\A) (integer->char 97) #\\space))"]

   ["continuations"
    "(define (find-first pred l)
       (call/cc (lambda (return)
                  (for-each (lambda (x) (when (pred x) (return x))) l)
                  #f)))
     (display (find-first negative? '(3 1 -4 1 -5))) (newline)
     (display (find-first negative? '(3 1 4))) (newline)
     (define (product l)
       (call/cc (lambda (k)
                  (let loop ((l l))
                    (cond ((null? l) 1)
                          ((= (car l) 0) (k 0))
                          (else (* (car l) (loop (cdr l)))))))))
     (list (product '(1 2 3 4)) (product '(1 2 0 4)) (+ 1 (call/cc (lambda (k) (* 10 (k 5))))))"]

   ["error-handling"
    "(define (safe-div a b)
       (guard (e ((string? e) (list 'error e condition/irritants))
                 ((symbol? e) (list 'raised e)))
         (if (= b 0) (error \"division by zero\" a) (/ a b))))
     (display (safe-div 10 2)) (newline)
     (display (safe-div 1 0)) (newline)
     (display (guard (e (#t (list 'caught e))) (raise 'oops))) (newline)
     (display (guard (e (#t (list 'caught e))) (car '()))) (newline)
     (display (guard (e (#t (list 'caught e))) (undefined-fn 1))) (newline)
     (display (guard (e (#t (list 'caught e))) ((lambda (a b) a) 1))) (newline)
     (vector-ref (vector 1 2) 5)"]

   ["higher-order"
    "(define (compose . fs)
       (if (null? fs)
           (lambda (x) x)
           (lambda (x) ((car fs) ((apply compose (cdr fs)) x)))))
     (define (curry f . args) (lambda more (apply f (append args more))))
     (define Y
       (lambda (f)
         ((lambda (x) (f (lambda (v) ((x x) v))))
          (lambda (x) (f (lambda (v) ((x x) v)))))))
     (define y-fact (Y (lambda (self) (lambda (n) (if (= n 0) 1 (* n (self (- n 1))))))))
     (define zero (lambda (f) (lambda (x) x)))
     (define (succ n) (lambda (f) (lambda (x) (f ((n f) x)))))
     (define (church->int n) ((n (lambda (k) (+ k 1))) 0))
     (list ((compose (curry + 1) (curry * 2)) 5)
           (y-fact 6)
           (church->int (succ (succ (succ zero))))
           (map (lambda (x y) (* x y)) '(1 2 3) '(4 5 6)))"]

   ["n-queens"
    "(define (queens board-size)
       (define (safe? q dist placed)
         (or (null? placed)
             (and (not (= (car placed) q))
                  (not (= (abs (- (car placed) q)) dist))
                  (safe? q (+ dist 1) (cdr placed)))))
       (define (place k placed)
         (if (= k 0)
             1
             (fold-left + 0
                        (map (lambda (q) (if (safe? q 1 placed) (place (- k 1) (cons q placed)) 0))
                             (iota board-size)))))
       (place board-size '()))
     (map queens '(1 2 3 4 5 6))"]

   ["objects"
    "(define (make-account balance)
       (define history '())
       (define (record! what) (set! history (cons what history)))
       (define (withdraw amount)
         (if (> amount balance)
             (error \"insufficient funds\" amount balance)
             (begin (set! balance (- balance amount)) (record! (list 'withdraw amount)) balance)))
       (define (deposit amount)
         (set! balance (+ balance amount)) (record! (list 'deposit amount)) balance)
       (lambda (msg . args)
         (case msg
           ((withdraw) (apply withdraw args))
           ((deposit) (apply deposit args))
           ((balance) balance)
           ((history) (reverse history))
           (else (error \"unknown message\" msg)))))
     (define acc (make-account 100))
     (acc 'deposit 50)
     (acc 'withdraw 30)
     (display (acc 'balance)) (newline)
     (display (acc 'history)) (newline)
     (display (guard (e (#t (list e condition/irritants))) (acc 'withdraw 500))) (newline)
     (acc 'frobnicate)"]

   ["streams"
    "(define-macro (cons-stream a b) `(cons ,a (delay ,b)))
     (define (stream-cdr s) (force (cdr s)))
     (define (integers-from n) (cons-stream n (integers-from (+ n 1))))
     (define (stream-filter pred s)
       (if (pred (car s))
           (cons-stream (car s) (stream-filter pred (stream-cdr s)))
           (stream-filter pred (stream-cdr s))))
     (define (take s n)
       (if (= n 0) '() (cons (car s) (take (stream-cdr s) (- n 1)))))
     (define (sieve s)
       (cons-stream (car s)
                    (sieve (stream-filter (lambda (x) (not (= 0 (remainder x (car s)))))
                                          (stream-cdr s)))))
     (take (sieve (integers-from 2)) 12)"]

   ["metacircular"
    ";; a tiny evaluator for arithmetic and lambda, written in the interpreted Scheme
     (define (lookup name env)
       (cond ((null? env) (error \"unbound\" name))
             ((eq? (car (car env)) name) (cdr (car env)))
             (else (lookup name (cdr env)))))
     (define (m-eval e env)
       (cond ((number? e) e)
             ((symbol? e) (lookup e env))
             ((eq? (car e) 'lambda) (list 'closure (cadr e) (caddr e) env))
             ((eq? (car e) 'if) (if (m-eval (cadr e) env) (m-eval (caddr e) env)
                                    (m-eval (car (cdr (cddr e))) env)))
             (else (m-apply (m-eval (car e) env)
                            (map (lambda (a) (m-eval a env)) (cdr e))))))
     (define (m-apply f args)
       (if (procedure? f)
           (apply f args)
           (m-eval (caddr f)
                   (append (map cons (cadr f) args) (car (cdr (cddr f)))))))
     (define base (list (cons '+ +) (cons '* *) (cons '- -) (cons '< <)))
     (m-eval '(((lambda (f) (lambda (n) ((f f) n)))
                (lambda (self) (lambda (n) (if (< n 2) 1 (* n ((self self) (- n 1)))))))
               10)
             base)"]

   ["reader-errors"
    "(display \"before\") (newline) (car (cdr (list 1"]])

(doseq [[name src] programs]
  (println (str "--- " name))
  (println (run src)))

;; The interpreter state is rebuilt per program; environments and closures refer to
;; each other through atoms, so this also exercises the cycle collector.
(println "--- summary")
(println (count programs) "programs," (count primitives) "primitives," (count special-names) "special forms")
(println "collected:" (pos? (gc)))
