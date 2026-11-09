//! A bridge library. Build with `cargo build -p rscl-demo`, then in rscl:
//!
//! ```clojure
//! (load-native "librscl_demo.so" :as demo)   ; found next to the rscl binary
//! (demo/add 1 2)
//! ```
use std::collections::HashMap;
use std::sync::Mutex;

use rscl_bridge::{Native, Val};

pub struct Counter {
    n: Mutex<i64>,
}

rscl_bridge::exports! {
    ns "demo";

    fn add(a: i64, b: i64) -> i64 { a + b }

    fn greet(name: &str) -> String { format!("Hello, {name}!") }

    fn fib(n: i64) -> i64 {
        fn f(n: i64) -> i64 { if n < 2 { n } else { f(n - 1) + f(n - 2) } }
        f(n)
    }

    fn parse_int(s: &str) -> Result<i64, String> {
        s.trim().parse().map_err(|e| format!("cannot parse {s:?}: {e}"))
    }

    fn sum(xs: Vec<i64>) -> i64 { xs.iter().sum() }

    fn mean(xs: Vec<f64>) -> Option<f64> {
        if xs.is_empty() { None } else { Some(xs.iter().sum::<f64>() / xs.len() as f64) }
    }

    fn word_lengths(s: &str) -> HashMap<String, i64> {
        s.split_whitespace().map(|w| (w.to_string(), w.chars().count() as i64)).collect()
    }

    fn describe(m: HashMap<String, Val>) -> String {
        let mut keys: Vec<&String> = m.keys().collect();
        keys.sort();
        keys.iter().map(|k| format!("{k}={}", m[*k].pr_str())).collect::<Vec<_>>().join(" ")
    }

    fn apply_twice(f: Val, x: Val) -> rscl_bridge::Result<Val> {
        let once = f.call(&[x])?;
        f.call(&[once])
    }

    fn make_counter(start: i64) -> Native<Counter> {
        Native(Counter { n: Mutex::new(start) })
    }

    fn counter_inc(c: &Native<Counter>) -> i64 {
        let mut n = c.n.lock().unwrap();
        *n += 1;
        *n
    }

    fn boom() -> i64 { panic!("deliberate panic") }
}
