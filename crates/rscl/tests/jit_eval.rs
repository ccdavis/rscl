//! In-process JIT tests. Every evaluation must leave the live heap-object count where
//! it found it (constants are immortal and do not count), which catches refcount leaks
//! without valgrind.
use rscl::jit::{EvalError, Jit};
use rscl_runtime::printer::pr_str;
use rscl_runtime::{exc, live_objects};

/// The live-object counter and the `user` namespace are process-global, so leak
/// tests must not run concurrently.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_jit<F: FnOnce(&mut Jit)>(f: F) {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = Jit::new().expect("jit");
    f(&mut jit);
}

/// Evaluate a definition (which legitimately keeps its value alive in a var).
fn def(jit: &mut Jit, src: &str) -> String {
    match jit.eval_str(src) {
        Ok(v) => pr_str(&v),
        Err(e) => panic!("{src}: {e}"),
    }
}

/// Evaluate `src`, return `pr-str` of the last value, and assert no object leaked.
fn eval(jit: &mut Jit, src: &str) -> String {
    let before = live_objects();
    let out = match jit.eval_str(src) {
        Ok(v) => {
            let s = pr_str(&v);
            drop(v);
            s
        }
        Err(EvalError::Exception(e)) => {
            let s = format!("EXC {}", exc::format_exception(&e));
            drop(e);
            s
        }
        Err(e) => format!("ERR {e}"),
    };
    let after = live_objects();
    assert_eq!(
        before, after,
        "live objects changed evaluating {src:?} (result {out})"
    );
    out
}

#[test]
fn arithmetic_and_literals() {
    with_jit(|j| {
        assert_eq!(eval(j, "(+ 1 2)"), "3");
        assert_eq!(eval(j, "(* 1.5 2)"), "3.0");
        assert_eq!(eval(j, "(/ 1 3)"), "0.3333333333333333");
        assert_eq!(eval(j, "(- 5)"), "-5");
        assert_eq!(eval(j, "(+ 9223372036854775807 0)"), "9223372036854775807");
        assert_eq!(
            eval(j, "(* 4611686018427387904 2)"),
            "EXC ArithmeticException: integer overflow"
        );
        assert_eq!(eval(j, "\"a\\nb\""), "\"a\\nb\"");
        assert_eq!(
            eval(j, "[1 :k \"s\" \\c nil true 2.5]"),
            "[1 :k \"s\" \\c nil true 2.5]"
        );
        assert_eq!(eval(j, "'(1 (2 3) [4] {:a 1})"), "(1 (2 3) [4] {:a 1})");
        assert_eq!(
            eval(j, "[(+ 1 1) {:k (str \"x\" 1)} #{(inc 1)}]"),
            "[2 {:k \"x1\"} #{2}]"
        );
    });
}

#[test]
fn locals_loops_closures() {
    with_jit(|j| {
        assert_eq!(eval(j, "(let [a 1 b (+ a 1)] (* a b))"), "2");
        assert_eq!(eval(j, "(let [s \"str\"] (let [t s] t))"), "\"str\"");
        assert_eq!(
            eval(
                j,
                "(loop [i 0 acc 0] (if (< i 100) (recur (inc i) (+ acc i)) acc))"
            ),
            "4950"
        );
        assert_eq!(
            eval(
                j,
                "(loop [i 0 acc \"\"] (if (< i 3) (recur (inc i) (str acc i)) acc))"
            ),
            "\"012\""
        );
        assert_eq!(def(j, "(defn adder [x] (fn [y] (+ x y)))"), "#'user/adder");
        assert_eq!(eval(j, "((adder 10) 5)"), "15");
        assert_eq!(
            eval(j, "(let [s \"cap\"] ((fn [] (str s \"!\"))))"),
            "\"cap!\""
        );
        assert_eq!(
            def(j, "(defn outer [a] (fn [b] (fn [c] (str a b c))))"),
            "#'user/outer"
        );
        assert_eq!(eval(j, "(((outer \"x\") \"y\") \"z\")"), "\"xyz\"");
        assert_eq!(
            eval(j, "((fn self [n] (if (zero? n) :done (self (dec n)))) 10)"),
            ":done"
        );
        assert_eq!(
            eval(j, "((fn [n] (if (zero? n) \"done\" (recur (dec n)))) 10)"),
            "\"done\""
        );
    });
}

#[test]
fn recur_releases_locals_bound_in_the_body() {
    with_jit(|j| {
        assert_eq!(
            eval(
                j,
                "(loop [i 0] (let [s (str \"x\" i)] (if (< i 50) (recur (inc i)) s)))"
            ),
            "\"x50\""
        );
        def(
            j,
            "(defn count-with-let [n] (let [s (str n)] (if (pos? n) (recur (dec n)) s)))",
        );
        assert_eq!(eval(j, "(count-with-let 20)"), "\"0\"");
        assert_eq!(
            eval(
                j,
                "(loop [i 0 acc nil] (let [s (str i)] (if (< i 3) (recur (inc i) s) [acc s])))"
            ),
            "[\"2\" \"3\"]"
        );
        assert_eq!(eval(j, "(sort (fn [a b] (< a b)) [3 1 2 1])"), "(1 1 2 3)");
        assert_eq!(eval(j, "(get-in {:a nil} [:a] :nf)"), "nil");
        assert_eq!(eval(j, "(get-in {:a {:b nil}} [:a :b] 0)"), "nil");
        assert_eq!(eval(j, "(get-in {:a 1} [:x :y] :nf)"), ":nf");
    });
}

#[test]
fn arities_and_vars() {
    with_jit(|j| {
        def(j, "(defn f ([] :zero) ([a] a) ([a b & more] more))");
        assert_eq!(
            eval(j, "[(f) (f \"one\") (f 1 2) (f 1 2 \"three\" 4)]"),
            "[:zero \"one\" nil (\"three\" 4)]"
        );
        assert_eq!(eval(j, "(f 1 2 3)"), "(3)");
        assert_eq!(def(j, "(def x \"first\")"), "#'user/x");
        def(j, "(defn get-x [] x)");
        assert_eq!(eval(j, "(get-x)"), "\"first\"");
        eval(j, "(def x \"second\")"); // rebinding releases the old root: no growth
        assert_eq!(eval(j, "(get-x)"), "\"second\"");
        assert_eq!(eval(j, "(var x)"), "#'user/x");
        assert_eq!(def(j, "(def y)"), "#'user/y");
        assert_eq!(
            eval(j, "(y)"),
            "EXC IllegalStateException: Attempting to call unbound fn: #'user/y"
        );
    });
}

#[test]
fn exceptions_release_temporaries() {
    with_jit(|j| {
        // The owned string "a" must be released when the second argument throws.
        assert_eq!(
            eval(j, "(str (str \"a\") (+ 1 nil))"),
            "EXC ClassCastException: nil cannot be cast to Number"
        );
        assert_eq!(
            eval(j, "(let [s (str \"owned\")] (+ s 1))"),
            "EXC ClassCastException: String cannot be cast to Number"
        );
        assert_eq!(
            eval(
                j,
                "(loop [i 0 s (str \"x\")] (if (< i 2) (recur (inc i) (str s i)) (+ s 1)))"
            ),
            "EXC ClassCastException: String cannot be cast to Number"
        );
        assert_eq!(
            eval(j, "((fn [a] a))"),
            "EXC ArityException: Wrong number of args (0) passed to: user/fn__1"
        );
        assert_eq!(
            eval(j, "(\"notfn\" 1)"),
            "EXC ClassCastException: String cannot be cast to IFn"
        );
        assert_eq!(def(j, "(defn thrower [s] (+ s nil))"), "#'user/thrower");
        assert_eq!(
            eval(j, "(thrower (str \"a\" \"b\"))"),
            "EXC ClassCastException: String cannot be cast to Number"
        );
    });
}

#[test]
fn compile_errors() {
    with_jit(|j| {
        assert!(eval(j, "nope").starts_with("ERR Compile error"));
        assert!(eval(j, "(recur 1)").contains("recur"));
        assert!(eval(j, "(fn [a] (if a (recur 1) 2) 3)").contains("tail position"));
        assert!(eval(j, "(let [1 2] 3)").contains("Unsupported binding form"));
        assert!(eval(j, "(fn ([a] a) ([b] b))").contains("same arity"));
    });
}

#[test]
fn rewrites() {
    with_jit(|j| {
        assert_eq!(eval(j, "(when (< 1 2) 1 2)"), "2");
        assert_eq!(eval(j, "(and 1 2 3)"), "3");
        assert_eq!(eval(j, "(and 1 nil 3)"), "nil");
        assert_eq!(eval(j, "(or nil false)"), "false");
        assert_eq!(eval(j, "(or nil 7)"), "7");
        assert_eq!(eval(j, "(cond (< 2 1) :a (> 2 1) :b)"), ":b");
        assert_eq!(eval(j, "(cond)"), "nil");
        assert_eq!(eval(j, "(-> 1 inc (* 10))"), "20");
        assert_eq!(eval(j, "(->> 1 inc (- 10))"), "8");
        assert_eq!(def(j, "(do (def d 1) (defn dd [] d) (dd))"), "1");
    });
}

#[test]
fn exceptions_try_catch_finally() {
    with_jit(|j| {
        assert_eq!(
            eval(
                j,
                "(try (/ 1 0) (catch ArithmeticException e (ex-message e)))"
            ),
            "\"Divide by zero\""
        );
        assert_eq!(
            eval(
                j,
                "(try (throw (ex-info \"boom\" {:a 1})) (catch ExceptionInfo e (ex-data e)))"
            ),
            "{:a 1}"
        );
        assert_eq!(
            eval(
                j,
                "(try (throw (ex-info \"boom\" {})) (catch ArithmeticException e :no) (catch Exception e :yes))"
            ),
            ":yes"
        );
        assert_eq!(
            eval(j, "(try (str \"a\" \"b\") (finally (str \"side\")))"),
            "\"ab\""
        );
        assert_eq!(
            eval(
                j,
                "(try (throw (ex-info \"x\" {})) (catch Exception e (str (ex-message e) \"!\")) (finally (str \"f\")))"
            ),
            "\"x!\""
        );
        // Uncaught through a try without a matching clause: temporaries still released.
        assert_eq!(
            eval(
                j,
                "(str (str \"tmp\") (try (+ 1 nil) (catch ArithmeticException e :no)))"
            ),
            "EXC ClassCastException: nil cannot be cast to Number"
        );
        // Finally that throws replaces the exception.
        assert_eq!(
            eval(
                j,
                "(try (throw (ex-info \"first\" {})) (finally (throw (ex-info \"second\" {}))))"
            ),
            "EXC ExceptionInfo: second"
        );
        // Catch body throwing propagates outward with locals released.
        assert_eq!(
            eval(
                j,
                "(let [s (str \"loc\")] (try (throw (ex-info \"a\" {})) (catch Exception e (throw (ex-info (str s (ex-message e)) {})))))"
            ),
            "EXC ExceptionInfo: loca"
        );
        assert_eq!(eval(j, "(try (throw 42) (catch Throwable e e))"), "42");
        assert_eq!(
            eval(
                j,
                "(loop [i 0 acc (str \"\")] (if (< i 3) (recur (inc i) (str acc (try (if (= i 1) (throw (ex-info \"o\" {})) i) (catch Exception e \"e\")))) acc))"
            ),
            "\"0e2\""
        );
        assert!(
            eval(j, "(loop [] (try (recur) (catch Exception e nil)))").contains("recur across try")
        );
    });
}

#[test]
fn collections_and_strings() {
    with_jit(|j| {
        assert_eq!(eval(j, "(conj [1] 2)"), "[1 2]");
        assert_eq!(
            eval(j, "(assoc {:a 1} :b (str \"two\"))"),
            "{:b \"two\", :a 1}"
        );
        assert_eq!(
            eval(j, "(map (fn [x] (str x \"!\")) [1 2])"),
            "(\"1!\" \"2!\")"
        );
        assert_eq!(
            eval(j, "(reduce (fn [acc x] (str acc x)) \"\" [1 2 3])"),
            "\"123\""
        );
        assert_eq!(
            eval(j, "(sort-by count [\"ccc\" \"a\" \"bb\"])"),
            "(\"a\" \"bb\" \"ccc\")"
        );
        assert_eq!(eval(j, "((partial str \"a\") \"b\" \"c\")"), "\"abc\"");
        assert_eq!(eval(j, "((comp count str) 12345)"), "5");
        assert_eq!(
            eval(j, "(apply str (interpose \", \" [1 2 3]))"),
            "\"1, 2, 3\""
        );
        assert_eq!(eval(j, "(clojure.string/upper-case \"abc\")"), "\"ABC\"");
        assert_eq!(eval(j, "(:a {:a (str \"v\")})"), "\"v\"");
        assert_eq!(eval(j, "(get-in {:a {:b [10 20]}} [:a :b 1])"), "20");
        assert_eq!(
            eval(j, "(nth [1 2] 5)"),
            "EXC IndexOutOfBoundsException: index 5 out of bounds for length 2"
        );
    });
}

#[test]
fn native_calls() {
    with_jit(|j| {
        def(j, "(defnative sqrt :double [^double x])");
        def(j, "(defnative strlen :long [^str s])");
        def(j, "(defnative abs :int [^int n])");
        assert_eq!(eval(j, "(sqrt 2.25)"), "1.5");
        assert_eq!(eval(j, "(sqrt 9)"), "3.0");
        assert_eq!(eval(j, "(strlen (str \"ab\" \"cd\"))"), "4");
        assert_eq!(eval(j, "(abs -7)"), "7");
        assert_eq!(
            eval(j, "(strlen 1)"),
            "EXC ClassCastException: Long cannot be passed as a native string"
        );
        assert!(
            eval(j, "(defnative nope-nope :void [])")
                .starts_with("EXC RuntimeException: unresolved native symbol")
        );
    });
}

#[test]
fn stack_overflow_is_an_exception() {
    with_jit(|j| {
        def(j, "(defn deep [n] (+ 1 (deep (inc n))))");
        assert_eq!(
            eval(j, "(try (deep 0) (catch StackOverflowError e :caught))"),
            ":caught"
        );
    });
}

#[test]
fn macros_and_lazy_seqs() {
    with_jit(|j| {
        def(j, "(defmacro unless [t & body] `(if ~t nil (do ~@body)))");
        assert_eq!(eval(j, "(unless false (str \"ran\"))"), "\"ran\"");
        assert_eq!(
            eval(j, "(macroexpand-1 '(unless x 1))"),
            "(if x nil (do 1))"
        );
        assert_eq!(
            eval(j, "(take 3 (map (fn [x] (str x)) (range)))"),
            "(\"0\" \"1\" \"2\")"
        );
        assert_eq!(eval(j, "(reduce + (filter even? (range 100)))"), "2450");
        assert_eq!(
            eval(j, "(doall (take 5 (iterate (fn [s] (str s \"a\")) \"\")))"),
            "(\"\" \"a\" \"aa\" \"aaa\" \"aaaa\")"
        );
        assert_eq!(
            eval(
                j,
                "(let [[a b & r] [(str \"x\") 2 3 4] {:keys [k]} {:k (str \"v\")}] [a b r k])"
            ),
            "[\"x\" 2 (3 4) \"v\"]"
        );
        assert_eq!(eval(j, "((fn [[a b]] (str a b)) [\"p\" \"q\"])"), "\"pq\"");
        assert_eq!(
            eval(j, "(for [x (range 3) :when (odd? x)] (str x))"),
            "(\"1\")"
        );
        assert_eq!(
            eval(j, "(let [a (atom (str \"s\"))] (swap! a str \"t\") @a)"),
            "\"st\""
        );
        assert_eq!(eval(j, "(re-seq #\"\\d\" \"a1b2\")"), "(\"1\" \"2\")");
        assert_eq!(eval(j, "(count (doall (range 20000)))"), "20000");
        assert_eq!(
            eval(
                j,
                "(let [s (map (fn [x] (if (= x 5) (throw (ex-info \"five\" {})) x)) (range 10))] (try (doall s) (catch Exception e (ex-message e))))"
            ),
            "\"five\""
        );
        assert_eq!(eval(j, "(#(str % %2) \"a\" \"b\")"), "\"ab\"");
        assert_eq!(eval(j, "(case (str \"a\") \"a\" :a :other)"), ":a");
    });
}

#[test]
fn types_protocols_multimethods() {
    with_jit(|j| {
        def(j, "(defprotocol Shape (area [this]) (scale [this k]))");
        def(
            j,
            "(defrecord Rect [w h] Shape (area [this] (* w h)) (scale [this k] (->Rect (* w k) (* h k))))",
        );
        def(
            j,
            "(deftype Circle [r] Shape (area [this] (* 3 r r)) (scale [this k] (Circle. (* r k))))",
        );
        assert_eq!(eval(j, "(area (->Rect 2 3))"), "6");
        assert_eq!(eval(j, "(area (scale (->Rect 2 3) 2))"), "24");
        assert_eq!(
            eval(j, "(scale (->Rect (str \"a\") 1) 1)"),
            "EXC ClassCastException: String cannot be cast to Number"
        );
        assert_eq!(eval(j, "(area (Circle. 2))"), "12");
        assert_eq!(eval(j, "(.-r (scale (Circle. 2) 5))"), "10");
        assert_eq!(eval(j, "(:w (assoc (->Rect 1 2) :w (str \"x\")))"), "\"x\"");
        assert_eq!(eval(j, "(:z (assoc (->Rect 1 2) :z (str \"z\")))"), "\"z\"");
        assert_eq!(
            eval(j, "(dissoc (assoc (->Rect 1 2) :z 3) :w)"),
            "{:h 2, :z 3}"
        );
        assert_eq!(
            eval(j, "(pr-str (map->Rect {:w 1 :h (str \"h\")}))"),
            "\"#user.Rect{:w 1, :h \\\"h\\\"}\""
        );
        assert_eq!(eval(j, "(= (->Rect 1 2) (->Rect 1 2))"), "true");
        assert_eq!(eval(j, "(into {} (->Rect 1 2))"), "{:h 2, :w 1}");
        assert_eq!(
            eval(j, "(meta (with-meta (->Rect 1 2) {:m (str \"m\")}))"),
            "{:m \"m\"}"
        );
        assert_eq!(eval(j, "(type (->Rect 1 2))"), "user.Rect");
        assert_eq!(
            eval(
                j,
                "[(type 1) (type \"s\") (type nil) (type :k) (instance? String (str \"s\"))]"
            ),
            "[Long String nil Keyword true]"
        );
        assert_eq!(
            eval(j, "(area :kw)"),
            "EXC IllegalArgumentException: No implementation of method: :area of protocol: #'user/Shape found for class: Keyword"
        );
        def(
            j,
            "(extend-protocol Shape nil (area [_] 0) (scale [_ k] nil) String (area [s] (count s)) (scale [s k] (apply str (repeat k s))))",
        );
        assert_eq!(
            eval(
                j,
                "[(area nil) (area (str \"abc\")) (scale (str \"ab\") 2) (satisfies? Shape 1) (satisfies? Shape nil)]"
            ),
            "[0 3 \"abab\" false true]"
        );
        def(
            j,
            "(defrecord Named [n] Printable (-pr [this r?] (str \"<\" n \">\")))",
        );
        assert_eq!(eval(j, "(pr-str [(->Named (str \"x\"))])"), "\"[<x>]\"");
        assert_eq!(
            eval(
                j,
                "(try (.-nope (->Rect 1 2)) (catch Exception e (ex-message e)))"
            ),
            "\"No matching field found: nope for class user.Rect\""
        );

        def(j, "(defmulti speak (fn [a & _] (:kind a)))");
        def(j, "(defmethod speak :dog [a] (str (:name a) \" woofs\"))");
        def(
            j,
            "(defmethod speak :default [a] (str (:name a) \" is silent\"))",
        );
        assert_eq!(
            eval(j, "(speak {:kind :dog :name (str \"Rex\")})"),
            "\"Rex woofs\""
        );
        assert_eq!(
            eval(j, "(speak {:kind :cat :name (str \"Tom\")})"),
            "\"Tom is silent\""
        );
        def(j, "(defmulti strict identity)");
        assert_eq!(
            eval(j, "(strict (str \"x\"))"),
            "EXC IllegalArgumentException: No method in multimethod 'user/strict' for dispatch value: \"x\""
        );
        def(j, "(defmethod strict \"x\" [x] (str x x))");
        assert_eq!(eval(j, "(strict (str \"x\"))"), "\"xx\"");
        // Removing the method frees it (it lived in the method table), so no leak check.
        def(j, "(remove-method strict \"x\")");
        assert_eq!(eval(j, "(count (methods strict))"), "0");
        def(j, "(defmulti by-type type)");
        def(j, "(defmethod by-type Rect [r] :rect)");
        def(j, "(defmethod by-type String [s] :string)");
        assert_eq!(
            eval(j, "[(by-type (->Rect 1 2)) (by-type (str \"s\"))]"),
            "[:rect :string]"
        );
    });
}

#[test]
fn dynamic_vars_and_sorted_colls() {
    with_jit(|j| {
        def(j, "(def ^:dynamic *d* (str \"root\"))");
        def(j, "(defn show [] *d*)");
        assert_eq!(
            eval(j, "(binding [*d* (str \"bound\")] (show))"),
            "\"bound\""
        );
        assert_eq!(
            eval(
                j,
                "[(show) (binding [*d* 1] (binding [*d* (inc *d*)] (show))) (show)]"
            ),
            "[\"root\" 2 \"root\"]"
        );
        assert_eq!(
            eval(j, "(binding [*d* 1] (set! *d* (str \"set\")) (show))"),
            "\"set\""
        );
        assert_eq!(
            eval(
                j,
                "(try (binding [*d* (str \"b\")] (throw (ex-info (str \"boom\") {}))) (catch Exception e [(ex-message e) (show)]))"
            ),
            "[\"boom\" \"root\"]"
        );
        assert_eq!(
            eval(j, "(set! *d* 1)"),
            "EXC IllegalStateException: Can't change/establish root binding of: user/*d* with set"
        );
        assert_eq!(
            eval(j, "(binding [show 1] 2)"),
            "EXC IllegalStateException: Can't dynamically bind non-dynamic var: user/show"
        );
        assert_eq!(
            eval(j, "(binding [*d* (str \"x\")] (thread-bound? (var *d*)))"),
            "true"
        );
        assert_eq!(eval(j, "(thread-bound? (var *d*))"), "false");
        def(j, "(defn documented \"Doc.\" [a] a)");
        assert_eq!(
            eval(
                j,
                "[(:doc (meta (var documented))) (:arglists (meta (var documented)))]"
            ),
            "[\"Doc.\" ([a])]"
        );

        assert_eq!(
            eval(j, "(sorted-map :b (str \"2\") :a 1)"),
            "{:a 1, :b \"2\"}"
        );
        assert_eq!(
            eval(j, "(assoc (sorted-map 2 :b) 1 (str \"a\"))"),
            "{1 \"a\", 2 :b}"
        );
        assert_eq!(
            eval(j, "(dissoc (sorted-map 1 (str \"a\") 2 :b) 1)"),
            "{2 :b}"
        );
        assert_eq!(
            eval(j, "(conj (sorted-set 3 1) (str \"x\"))"),
            "EXC ClassCastException: String cannot be compared with Long"
        );
        assert_eq!(
            eval(j, "(into (sorted-set) [(str \"b\") (str \"a\")])"),
            "#{\"a\" \"b\"}"
        );
        assert_eq!(
            eval(j, "(= (sorted-map :a (str \"v\")) {:a \"v\"})"),
            "true"
        );
        assert_eq!(
            eval(j, "(rseq (sorted-map 1 (str \"a\") 2 (str \"b\")))"),
            "([2 \"b\"] [1 \"a\"])"
        );
        assert_eq!(eval(j, "(subseq (sorted-set 1 2 3) > 1)"), "(2 3)");
        assert_eq!(eval(j, "(get (sorted-map (str \"k\") 1) (str \"k\"))"), "1");
        assert_eq!(
            eval(
                j,
                "(keys (into (sorted-map) (map (fn [i] [i (str i)]) (range 5))))"
            ),
            "(0 1 2 3 4)"
        );
    });
}

#[test]
fn primitives_do_not_leak() {
    with_jit(|j| {
        assert_eq!(
            eval(
                j,
                "(loop [i 0 acc 0] (if (< i 1000) (recur (inc i) (+ acc i)) acc))"
            ),
            "499500"
        );
        assert_eq!(
            eval(
                j,
                "(loop [x 0.0 i 0] (if (< i 10) (recur (+ x 0.5) (inc i)) x))"
            ),
            "5.0"
        );
        // Boxed at the boundary: doubles into a vector, longs beyond the fixnum range.
        assert_eq!(
            eval(
                j,
                "(loop [i 0 v []] (if (< i 3) (recur (inc i) (conj v (* i 1.5))) v))"
            ),
            "[0.0 1.5 3.0]"
        );
        assert_eq!(
            eval(j, "(let [x 4611686018427387903] (+ x 1))"),
            "4611686018427387904"
        );
        assert_eq!(
            eval(j, "(let [x 1] (+ x nil))"),
            "EXC ClassCastException: nil cannot be cast to Number"
        );
        assert_eq!(
            eval(j, "(let [d 1.5] (+ d (str \"x\")))"),
            "EXC ClassCastException: String cannot be cast to Number"
        );
        assert_eq!(
            eval(j, "(let [x 9223372036854775807] (inc x))"),
            "EXC ArithmeticException: integer overflow"
        );
        assert_eq!(
            eval(j, "(let [a 1 b 0] (rem a b))"),
            "EXC ArithmeticException: Divide by zero"
        );
        def(j, "(defn hinted [^double a ^long b] (+ a b))");
        assert_eq!(eval(j, "(hinted 1 2)"), "3.0");
        assert_eq!(
            eval(j, "(hinted (str \"a\") 2)"),
            "EXC ClassCastException: String cannot be cast to Number"
        );
        assert_eq!(
            eval(j, "(hinted 1.0 (double 2))"),
            "EXC ClassCastException: Double cannot be cast to Long"
        );
        // A primitive local captured by a closure is boxed once.
        assert_eq!(
            eval(j, "(let [d 2.5 f (fn [] (* d 2))] [(f) (f)])"),
            "[5.0 5.0]"
        );
        // Demoted loop local (rebound with a string) behaves as a value.
        assert_eq!(
            eval(
                j,
                "(loop [x 1 n 0] (if (< n 2) (recur (if (= n 0) (str \"s\") x) (inc n)) x))"
            ),
            "\"s\""
        );
        // Inline comparisons on boxed values, including the fallback to the builtin.
        assert_eq!(
            eval(j, "(let [s (str \"a\")] (if (= s \"a\") :yes :no))"),
            ":yes"
        );
        assert_eq!(eval(j, "(if (next (list 1 2)) :more :done)"), ":more");
        assert_eq!(eval(j, "(cond (< 2 1) :a (> 2 1) :b)"), ":b");
    });
}
