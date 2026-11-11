//! In-process JIT tests. Every evaluation is followed by a full collection before
//! its result is printed, so a value the collector lost track of shows up here.
use rscl::jit::{EvalError, Jit};
use rscl_runtime::exc;
use rscl_runtime::printer::pr_str;

/// The `user` namespace is process-global, so these tests must not run
/// concurrently.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_jit<F: FnOnce(&mut Jit)>(f: F) {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // Every collection in these tests checks its own work (see `RSCL_GC_VERIFY`).
    // SAFETY: set under the lock, before the runtime reads its settings.
    unsafe { std::env::set_var("RSCL_GC_VERIFY", "1") };
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

/// Evaluate `src`, collect, and return `pr-str` of the last value: the result must
/// survive a collection while only this frame refers to it.
fn eval(jit: &mut Jit, src: &str) -> String {
    let result = jit.eval_str(src);
    let _ = jit.eval_str("(gc)");
    match result {
        Ok(v) => pr_str(&v),
        Err(EvalError::Exception(e)) => format!("EXC {}", exc::format_exception(&e)),
        Err(e) => format!("ERR {e}"),
    }
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
fn recur_with_locals_bound_in_the_body() {
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
        eval(j, "(def x \"second\")");
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
fn exceptions_with_live_temporaries() {
    with_jit(|j| {
        // The string "a" is a live temporary when the second argument throws.
        assert_eq!(
            eval(j, "(str (str \"a\") (+ 1 nil))"),
            "EXC ClassCastException: nil cannot be cast to Number"
        );
        assert_eq!(
            eval(j, "(let [s (str \"local\")] (+ s 1))"),
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
        // Uncaught through a try without a matching clause, with a live temporary.
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
        // Catch body throwing propagates outward.
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
fn primitive_locals_and_hints() {
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

#[test]
fn value_lifetimes_across_calls() {
    with_jit(|j| {
        // Consumers stream: the head of a lazy seq is not held by the caller.
        assert_eq!(eval(j, "(reduce + (range 100000))"), "4999950000");
        assert_eq!(eval(j, "(count (filter even? (range 100000)))"), "50000");
        assert_eq!(eval(j, "(last (map inc (range 1000)))"), "1000");
        // An operand pending while a later argument uses the same local.
        assert_eq!(
            eval(j, "(let [r (map inc (range 3))] (cons r (list (count r))))"),
            "((1 2 3) 3)"
        );
        assert_eq!(
            eval(j, "(let [s (str \"v\") y s] [y (count y)])"),
            "[\"v\" 1]"
        );
        assert_eq!(
            eval(j, "(let [s (str \"v\")] (vector (do s) (count s)))"),
            "[\"v\" 1]"
        );
        assert!(eval(j, "(let [f (fn [x] x)] (f (do f)))").starts_with("#object[user/fn__"));
        // The callee operand stays alive across its own arguments.
        def(j, "(defn self-arg [f] (f (count [f])))");
        assert_eq!(eval(j, "(self-arg (fn [n] (* n 10)))"), "10");
        // Parameters: borrowed from a caller that keeps them, or handed over.
        def(
            j,
            "(defn walk [s n] (if (seq s) (recur (next s) (inc n)) n))",
        );
        def(j, "(def shared (doall (map inc (range 10))))");
        assert_eq!(
            eval(
                j,
                "[(walk shared 0) (walk shared 0) (walk (range 5) 0) (count shared)]"
            ),
            "[10 10 5 10]"
        );
        def(j, "(defn pack [& xs] (vec xs))");
        assert_eq!(
            eval(
                j,
                "[(pack (str \"a\") (str \"b\")) (apply pack (map str (range 2))) (pack)]"
            ),
            "[[\"a\" \"b\"] [\"0\" \"1\"] []]"
        );
        // Exceptions thrown part-way through evaluating arguments.
        assert_eq!(
            eval(
                j,
                "(try (let [s (str \"x\")] (count s) (throw (ex-info s {}))) (catch Exception e (ex-message e)))"
            ),
            "\"x\""
        );
        assert_eq!(
            eval(
                j,
                "(let [v (vector (str \"a\"))] (try (nth v 5) (catch Exception e (count v))))"
            ),
            "1"
        );
        // Closures capture at creation; the outer local can still be used after.
        assert_eq!(
            eval(j, "(let [n (str \"n\") f (fn [] n)] [(f) n (f)])"),
            "[\"n\" \"n\" \"n\"]"
        );
        // A loop local cleared inside the body and rebound by recur.
        assert_eq!(
            eval(
                j,
                "(loop [xs (map str (range 3)) acc []] (if (seq xs) (recur (rest xs) (conj acc (first xs))) acc))"
            ),
            "[\"0\" \"1\" \"2\"]"
        );
        assert_eq!(
            eval(
                j,
                "(let [m (hash-map :a (str \"v\"))] [(:a m) (m :a) (get m :a)])"
            ),
            "[\"v\" \"v\" \"v\"]"
        );
    });
}

#[test]
fn dynamic_keywords_are_collected() {
    with_jit(|j| {
        // Keywords made at run time die with their last reference (no table growth);
        // a later keyword of the same name is the same keyword as a literal.
        assert_eq!(
            eval(j, "(count (map keyword (map str (range 500))))"),
            "500"
        );
        assert_eq!(
            eval(
                j,
                "(let [k (keyword (str \"dy\" \"n\"))] [(= k :dyn) (identical? k :dyn) (name k)])"
            ),
            "[true true \"dyn\"]"
        );
        assert_eq!(eval(j, "(get (hash-map (keyword (str \"k\")) 1) :k)"), "1");
    });
}

#[test]
fn futures_promises_delays() {
    with_jit(|j| {
        assert_eq!(eval(j, "@(future (str \"a\" \"b\"))"), "\"ab\"");
        assert_eq!(
            eval(
                j,
                "(let [p (promise)] (deliver p (str \"v\")) [@p (realized? p)])"
            ),
            "[\"v\" true]"
        );
        assert_eq!(
            eval(j, "(let [d (delay (str \"d\"))] [(realized? d) @d @d])"),
            "[false \"d\" \"d\"]"
        );
        // The future outlives the timed-out deref; wait for it so nothing is in flight
        // when the next evaluation starts.
        assert_eq!(
            eval(
                j,
                "(let [f (future (sleep 100) 1)] [(deref f 10 (str \"late\")) @f])"
            ),
            "[\"late\" 1]"
        );
        assert_eq!(
            eval(
                j,
                "(try @(future (throw (ex-info (str \"boom\") {}))) (catch Exception e (ex-message e)))"
            ),
            "\"boom\""
        );
        // A cancelled future's thread stops at its next sleep slice; give it time to
        // exit (and drop its reference to the future) before the count is compared.
        assert_eq!(
            eval(
                j,
                "(let [f (future (sleep 2000) 1) r [(future-cancel f) (try @f (catch Exception e (ex-message e)))]] (sleep 150) r)"
            ),
            "[true \"future was cancelled\"]"
        );
        assert_eq!(
            eval(j, "(vec (pmap (fn [i] (str i)) (range 20)))"),
            "[\"0\" \"1\" \"2\" \"3\" \"4\" \"5\" \"6\" \"7\" \"8\" \"9\" \"10\" \"11\" \"12\" \"13\" \"14\" \"15\" \"16\" \"17\" \"18\" \"19\"]"
        );
        def(j, "(def ^:dynamic *d* (str \"root\"))");
        assert_eq!(
            eval(j, "(binding [*d* (str \"bound\")] @(future *d*))"),
            "\"bound\""
        );
        assert_eq!(
            eval(
                j,
                "(let [l (atom 0) r (atom [])] (doseq [f (doall (map (fn [i] (future (locking l (swap! r conj (str i))))) (range 4)))] @f) (count @r))"
            ),
            "4"
        );
    });
}

#[test]
fn cycles_are_collected() {
    with_jit(|j| {
        // Each cycle is unreachable after its form; `(gc)` frees it.
        let cycles = [
            "(let [a (atom nil)] (reset! a (fn [] @a)) nil)",
            "(let [a (atom {})] (swap! a assoc :f (fn [] @a) :s (str \"s\")) nil)",
            "(let [a (atom nil) p (promise)] (deliver p (fn [] [@a @p])) (reset! a p) nil)",
            "(let [a (atom nil)] (reset! a (lazy-seq (cons (str \"x\") @a))) nil)",
            "(let [a (atom nil)] (reset! a (vector (fn [] @a) (hash-map :k (fn [] @a)))) nil)",
            // The closure is still referenced when the first collection looks.
            "(let [a (atom nil) f (fn [] @a)] (reset! a f) (gc) (f) nil)",
        ];
        for src in cycles {
            def(j, "(gc)");
            def(j, src);
            let freed: i64 = def(j, "(gc)").parse().unwrap();
            assert!(freed > 0, "{src}: nothing freed");
        }
        // Live structure reached from a candidate survives, shared nodes included.
        def(j, "(def keep (atom {}))");
        assert_eq!(
            eval(
                j,
                "(let [m (assoc @keep :f (fn [] (str \"f\")) :xs (vec (map str (range 5)))) snap m] (reset! keep m) (gc) (reset! keep {}) [((:f snap)) (:xs snap)])"
            ),
            "[\"f\" [\"0\" \"1\" \"2\" \"3\" \"4\"]]"
        );
        // An atom that is not garbage is not touched by a collection.
        def(j, "(def cyc (let [a (atom nil)] (reset! a (fn [] @a)) a))");
        assert_eq!(eval(j, "(do (gc) (fn? @cyc))"), "true");
    });
}

#[test]
fn bigints() {
    with_jit(|j| {
        assert_eq!(
            eval(j, "(+' 9223372036854775807 1)"),
            "9223372036854775808N"
        );
        assert_eq!(
            eval(j, "(* 2 (+' 9223372036854775807 1))"),
            "18446744073709551616N"
        );
        // BigInts stay BigInts (Clojure does not demote).
        assert_eq!(
            eval(j, "(- (+' 9223372036854775807 1) 1)"),
            "9223372036854775807N"
        );
        assert_eq!(eval(j, "(= 1N 1)"), "true");
        assert_eq!(eval(j, "(get {1N :a} 1)"), ":a");
        assert_eq!(
            eval(j, "(str (reduce *' (range 1 26)))"),
            "\"15511210043330985984000000\""
        );
        assert_eq!(
            eval(
                j,
                "(try (+ 9223372036854775807 1) (catch ArithmeticException e (ex-message e)))"
            ),
            "\"integer overflow\""
        );
        assert_eq!(
            eval(
                j,
                "(loop [i 0 acc 1N] (if (< i 70) (recur (inc i) (* acc 2)) acc))"
            ),
            "1180591620717411303424N"
        );
        assert_eq!(eval(j, "(long 5N)"), "5");
        // A BigInt that fits a long is an index like any other.
        assert_eq!(
            eval(
                j,
                "[(get [1 2 3] 1N) (nth [1 2 3] (mod 31N 3)) (subs \"hello\" 1N) ([1 2 3] 1N) (assoc [1 2 3] 1N :x)]"
            ),
            "[2 2 \"ello\" 2 [1 :x 3]]"
        );
        // An equal key keeps its stored form.
        assert_eq!(
            eval(
                j,
                "[(conj #{1} 1N) (assoc {1 :a} 1N :b) (assoc {1N :a} 1 :b)]"
            ),
            "[#{1} {1 :b} {1N :b}]"
        );
        assert_eq!(
            eval(
                j,
                "[(sort (fn [a b] (- a b)) [3N 1N 2N]) (sort (fn [a b] (- a b)) [3.0 1.0 2.0])]"
            ),
            "[(1N 2N 3N) (1.0 2.0 3.0)]"
        );
        assert_eq!(
            eval(
                j,
                "[(clojure.string/join \",\" [1N 2N nil]) (clojure.string/upper-case 12N)]"
            ),
            "[\"1,2,\" \"12\"]"
        );
        assert_eq!(
            eval(
                j,
                "(let [b (reduce *' (repeat 340 10))] [(== b ##Inf) (< b ##Inf) (> b ##-Inf) (< 2N 2.5)])"
            ),
            "[false true true true]"
        );
        assert_eq!(eval(j, "(== (/ 273N 1343) (/ 273 1343))"), "true");
        assert_eq!(
            eval(
                j,
                "[(try (long 1e30) (catch Exception e :range)) (try (long (identity ##NaN)) (catch Exception e :nan)) (long -3.9)]"
            ),
            "[:range :nan -3]"
        );
        assert_eq!(
            eval(j, "[(int? 1N) (integer? 1N) (get {1 :a} 1N)]"),
            "[false true :a]"
        );
        assert_eq!(
            eval(j, "[(str 1N) (str [1N]) (print-str 1N)]"),
            "[\"1\" \"[1N]\" \"1N\"]"
        );
        assert_eq!(
            eval(j, "[36rZN (-' 0.0) (-' -9223372036854775808)]"),
            "[1283 -0.0 9223372036854775808N]"
        );
        assert_eq!(
            eval(
                j,
                "(let [a (reduce *' (repeat 400 10))] (/ (inc a) (quot a 10)))"
            ),
            "10.0"
        );
        assert_eq!(
            eval(j, "(bigint \"-12345678901234567890\")"),
            "-12345678901234567890N"
        );
    });
}

#[test]
fn second_pass_review_regressions() {
    with_jit(|j| {
        // A cycle that outlives several collections is still collected once its
        // last outside reference goes.
        def(
            j,
            "(defn mk-cycle [] (let [a (atom nil)] (reset! a (fn [] @a)) @a))",
        );
        def(j, "(def held (mk-cycle))");
        def(j, "(do (gc) (gc) (gc) (gc))");
        assert_eq!(eval(j, "(fn? (held))"), "true");
        def(j, "(def held nil)");
        let freed: i64 = def(j, "(gc)").parse().unwrap();
        assert!(freed > 0, "nothing freed");
        // Printing a nested map while a lazy seq in it realises (collections run
        // meanwhile) leaves the structure intact.
        def(j, "(def nested (atom nil))");
        assert_eq!(
            eval(
                j,
                "(do (reset! nested (list {:k (map inc (range 200000)) :z 1})) (let [n (count (pr-str @nested))] (reset! nested nil) (> n 1000000)))"
            ),
            "true"
        );
        assert_eq!(
            eval(
                j,
                "[(re-matches #\"a|ab\" \"ab\") (re-matches #\"a\" \"ab\")]"
            ),
            "[\"ab\" nil]"
        );
        assert_eq!(
            eval(
                j,
                "[(conj {1 :a} [1N :b]) (hash-map 1 :a 1N :b) (merge {1 :a} {1N :b}) (hash-set 1 1N)]"
            ),
            "[{1 :b} {1 :b} {1 :b} #{1}]"
        );
        assert_eq!(eval(j, "(get {0.0 :a} -0.0)"), ":a");
        assert_eq!(
            eval(j, "(count (sort (fn [a b] (< (rand) 0.5)) (range 2000)))"),
            "2000"
        );
        assert_eq!(
            eval(j, "(sort-by first > [[1 :a] [2 :b] [1 :c]])"),
            "([2 :b] [1 :a] [1 :c])"
        );
        assert_eq!(
            eval(
                j,
                "(try (quot ##Inf 2.0) (catch ArithmeticException e (ex-message e)))"
            ),
            "\"Infinite or NaN\""
        );
    });
}

#[test]
fn compiler_review_regressions() {
    with_jit(|j| {
        // A failed compilation must not poison the next one.
        def(j, "(defmacro bad-macro [] inc)");
        assert!(eval(j, "(bad-macro)").starts_with("ERR"));
        assert_eq!(eval(j, "(+ 1 2)"), "3");
        assert!(eval(j, "(fn [] (bad-macro))").starts_with("ERR"));
        assert_eq!(eval(j, "(+ 2 3)"), "5");
        // An exact fixed arity wins over a variadic one declared first.
        assert_eq!(
            eval(
                j,
                "(let [f (fn ([a & r] :variadic) ([a] :fixed))] [(f 1) (f 1 2)])"
            ),
            "[:fixed :variadic]"
        );
        // Map and set literals evaluate in source order.
        def(j, "(def order (atom []))");
        assert_eq!(
            eval(
                j,
                "(do {:a (swap! order conj 1) :b (swap! order conj 2) :c (swap! order conj 3) :d (swap! order conj 4)} (let [o @order] (reset! order []) o))"
            ),
            "[1 2 3 4]"
        );
        // A recursive defn goes through its var.
        def(j, "(defn rec-f [n] (if (zero? n) :old (rec-f (dec n))))");
        def(j, "(def rec-g rec-f)");
        def(j, "(defn rec-f [n] :new)");
        assert_eq!(eval(j, "(rec-g 1)"), ":new");
        // Var and literal metadata are evaluated; quoted metadata survives.
        def(j, "(def ^{:doc (str \"a\" \"b\")} with-doc 1)");
        assert_eq!(eval(j, "(:doc (meta (var with-doc)))"), "\"ab\"");
        def(j, "(defn with-args \"d\" [a b] a)");
        assert_eq!(
            eval(
                j,
                "[(:arglists (meta (var with-args))) (:doc (meta (var with-args)))]"
            ),
            "[([a b]) \"d\"]"
        );
        assert_eq!(
            eval(
                j,
                "[(meta ^:foo [1 2]) (let [x 1] (meta ^{:n (inc x)} [x])) (meta (quote ^:foo sym)) (meta [1])]"
            ),
            "[{:foo true} {:n 2} {:foo true} nil]"
        );
        // A long recur argument does not turn into the loop local's double.
        assert_eq!(
            eval(j, "(loop [x 1.5 i 0] (if (< i 3) (recur i (inc i)) x))"),
            "2"
        );
    });
}

#[test]
fn third_pass_review_regressions() {
    with_jit(|j| {
        // One written atom must not make a native loop collect per element.
        def(j, "(def storm (atom 0))");
        def(j, "(reset! storm 1)");
        let before: i64 = def(j, "(gc-count)").parse().unwrap();
        assert_eq!(
            eval(
                j,
                "(reduce (fn [acc x] (count [acc x]) (+ acc x)) 0 (range 200000))"
            ),
            "19999900000"
        );
        let after: i64 = def(j, "(gc-count)").parse().unwrap();
        assert!(after - before < 20, "{} collections", after - before);
        // Syntax-quoted and plain map literals evaluate in source order.
        def(j, "(def order (atom []))");
        assert_eq!(
            eval(
                j,
                "(do `{:a ~(swap! order conj 1) :b ~(swap! order conj 2) :c ~(swap! order conj 3) :d ~(swap! order conj 4) :e ~(swap! order conj 5) :f ~(swap! order conj 6) :g ~(swap! order conj 7) :h ~(swap! order conj 8) :i ~(swap! order conj 9)} (let [o @order] (reset! order []) o))"
            ),
            "[1 2 3 4 5 6 7 8 9]"
        );
        // Constants with unreadable metadata, type hints on literals, embedded vars.
        def(
            j,
            "(defmacro unreadable-meta [] (list 'quote (with-meta 's {:f inc})))",
        );
        assert_eq!(eval(j, "(unreadable-meta)"), "s");
        assert_eq!(eval(j, "(let [x 1] (meta ^ints [x]))"), "{:tag ints}");
        def(j, "(defn embedded-f [] 7)");
        def(j, "(defmacro call-var [] (list (resolve 'embedded-f)))");
        assert_eq!(eval(j, "(call-var)"), "7");
        def(j, "(defmacro live-object [] (atom 1))");
        assert!(eval(j, "(live-object)").contains("cannot be embedded"));
        // Equal keys: literals agree with constructors, lookups return what is stored.
        assert_eq!(
            eval(
                j,
                "(let [a 1 b 1N] [#{a b} {a :x b :y} (find {1 :a} 1N) (#{1} 1N)])"
            ),
            "[#{1} {1 :y} [1 :a] 1]"
        );
        def(j, "(defrecord MergeRec [a])");
        assert_eq!(
            eval(
                j,
                "[(merge {:a 1} (sorted-map :b 2)) (merge {:x 1} (->MergeRec 5))]"
            ),
            "[{:b 2, :a 1} {:x 1, :a 5}]"
        );
        // Deep nesting: a reader error, a catchable overflow, an iterative free.
        assert_eq!(
            eval(
                j,
                "(try (read-string (apply str (repeat 300000 \"[\"))) (catch Exception e :too-deep))"
            ),
            ":too-deep"
        );
        assert_eq!(
            eval(
                j,
                "(let [v (loop [v [] i 0] (if (< i 2000000) (recur [v] (inc i)) v))] [(number? (hash v)) (try (= v [v]) (catch StackOverflowError e :so))])"
            ),
            "[true :so]"
        );
    });
}

#[test]
fn old_objects_written_after_a_collection_keep_what_they_refer_to() {
    with_jit(|j| {
        // Each store below goes into an object that has survived two collections
        // (it is old) and refers to objects made afterwards. A minor collection
        // does not look inside old objects, so only the write barrier keeps the
        // new objects alive, and it has to keep doing so while they are young.
        def(j, "(def a (atom nil))");
        def(j, "(def v nil)");
        def(j, "(def p (promise))");
        def(j, "(def d (delay (mapv str (range 30))))");
        def(j, "(def s (map str (range 200)))");
        def(j, "(defprotocol Shape (area [x]))");
        def(j, "(defrecord Sq [side])");
        def(j, "(do (gc) (gc))");
        def(j, "(reset! a (mapv str (range 50)))");
        def(j, "(alter-meta! a assoc :note (str \"n\" 1))");
        def(j, "(def v (vec (map str (range 20))))");
        def(j, "(deliver p (str \"x\" 1))");
        def(
            j,
            "(extend-type Sq Shape (area [x] (str (* (:side x) (:side x)))))",
        );
        def(j, "(gc :minor)");
        // Realised and forced after they became old, then collected again.
        def(j, "(first s)");
        def(j, "(force d)");
        def(j, "(do (gc :minor) (gc :minor) (gc :minor))");
        assert_eq!(
            eval(j, "[(count @a) (first @a) (last @a)]"),
            "[50 \"0\" \"49\"]"
        );
        assert_eq!(eval(j, "(:note (meta a))"), "\"n1\"");
        assert_eq!(eval(j, "[(count v) (v 19)]"), "[20 \"19\"]");
        assert_eq!(eval(j, "@p"), "\"x1\"");
        assert_eq!(eval(j, "(area (->Sq 7))"), "\"49\"");
        assert_eq!(eval(j, "[(count s) (last s)]"), "[200 \"199\"]");
        assert_eq!(eval(j, "(count @d)"), "30");
        // swap! in a loop across minor collections.
        def(j, "(def acc (atom []))");
        def(j, "(do (gc) (gc))");
        assert_eq!(
            eval(
                j,
                "(do (dotimes [i 300] (swap! acc conj (str i)) (when (zero? (rem i 50)) (gc :minor))) [(count @acc) (@acc 0) (@acc 299)])"
            ),
            "[300 \"0\" \"299\"]"
        );
        // An object that has survived one collection is still young: a minor
        // collection frees it once it is garbage.
        def(j, "(def once (vec (map str (range 1000))))");
        def(j, "(gc :minor)");
        def(j, "(def once nil)");
        let minor: i64 = def(j, "(gc :minor)").parse().unwrap();
        assert!(
            minor >= 1000,
            "aged garbage survived a minor collection: {minor}"
        );
        // After two it is old, and only a full collection finds it.
        def(j, "(def big (vec (map str (range 1000))))");
        def(j, "(do (gc :minor) (gc :minor))");
        def(j, "(def big nil)");
        let minor: i64 = def(j, "(gc :minor)").parse().unwrap();
        let full: i64 = def(j, "(gc)").parse().unwrap();
        assert!(
            minor < 1000,
            "a minor collection freed old objects: {minor}"
        );
        assert!(full >= 1000, "a full collection left old garbage: {full}");
    });
}

#[test]
fn natives_that_let_collections_run_keep_what_they_hold() {
    with_jit(|j| {
        // A closure made for one call is dead in its caller once it is called;
        // the native behind it must keep it (and the fns it captured) alive while
        // those run and collect.
        assert_eq!(
            eval(
                j,
                "((comp (partial + (count (str \"ab\"))) (fn [x] (gc) (inc x)) (fn [x] (gc) (* x 2))) 5)"
            ),
            "13"
        );
        assert_eq!(
            eval(
                j,
                "((juxt (fn [x] (gc) (str x)) (fn [x] (gc) (str x x)) (fn [x] (gc) (str x x x))) 7)"
            ),
            "[\"7\" \"77\" \"777\"]"
        );
        assert_eq!(
            eval(
                j,
                "[((every-pred (fn [x] (gc) (pos? x)) (fn [x] (gc) (odd? x))) 3 5) ((some-fn (fn [x] (gc) nil) (fn [x] (gc) (str x))) 4)]"
            ),
            "[true \"4\"]"
        );
        // Natives that gather values in Rust vectors and maps while a key fn, a
        // comparator or a lazy seq runs (and collects).
        assert_eq!(
            eval(j, "(sort-by (fn [x] (gc) (str x)) [3 1 2])"),
            "(1 2 3)"
        );
        assert_eq!(
            eval(
                j,
                "(sort (fn [a b] (gc :minor) (compare (str a) (str b))) (map str [3 1 2]))"
            ),
            "(\"1\" \"2\" \"3\")"
        );
        assert_eq!(
            eval(
                j,
                "(let [g (group-by (fn [x] (gc) (str (odd? x))) (range 6))] [(get g \"false\") (get g \"true\")])"
            ),
            "[[0 2 4] [1 3 5]]"
        );
        assert_eq!(
            eval(
                j,
                "(sort (vec (frequencies (map (fn [x] (gc :minor) (str (rem x 3))) (range 9)))))"
            ),
            "([\"0\" 3] [\"1\" 3] [\"2\" 3])"
        );
        assert_eq!(
            eval(j, "(count (set (map (fn [x] (gc) (str x)) (range 5))))"),
            "5"
        );
        assert_eq!(
            eval(
                j,
                "(get (zipmap (map (fn [x] (gc) (str x)) (range 4)) (map (fn [x] (gc) (str x x)) (range 4))) \"3\")"
            ),
            "\"33\""
        );
        assert_eq!(
            eval(
                j,
                "[(pr-str (map (fn [x] (gc) (str x)) (range 3))) (str (map (fn [x] (gc) x) (range 3))) (count (concat {:a (str 1)} (map (fn [x] (gc) x) (range 3))))]"
            ),
            "[\"(\\\"0\\\" \\\"1\\\" \\\"2\\\")\" \"(0 1 2)\" 4]"
        );
        // (gc) from several threads at once: each waits for the others' collections
        // and then runs its own.
        assert_eq!(
            eval(
                j,
                "(let [fs (doall (map (fn [_] (future (dotimes [_ 20] (gc)) :ok)) (range 4)))] (mapv deref fs))"
            ),
            "[:ok :ok :ok :ok]"
        );
    });
}
