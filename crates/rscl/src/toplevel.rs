//! Top-level evaluation order shared by the JIT driver and the session (Clojure's
//! `compiler.eval`): macroexpand the form, then split a top-level `do` so that each
//! sub-form is compiled and run before the next one is analysed.
use rscl_runtime::obj::symbol;
use rscl_runtime::{Value, exc, pos};

use crate::errors::CompileError;
use crate::jit::EvalError;

/// Expand macros at the head of a top-level form until it is not a macro call.
pub fn macroexpand(form: &Value) -> Result<Value, EvalError> {
    let mut cur = form.clone();
    loop {
        match rscl_runtime::core::lazy::macroexpand_1(&cur) {
            Ok(next) => {
                if next.identical(&cur) {
                    return Ok(cur);
                }
                if pos::pos_id(&next) == 0 && next.header().is_some_and(|h| !h.is_immortal()) {
                    pos::set_pos(&next, pos::pos_id(form));
                }
                cur = next;
            }
            Err(_) => {
                let e = exc::take().unwrap_or(Value::NIL);
                return Err(CompileError::at(
                    form,
                    format!(
                        "Unexpected error macroexpanding: {}",
                        exc::format_exception(&e)
                    ),
                )
                .into());
            }
        }
    }
}

/// Evaluate a top-level form with `eval_unit`, expanding macros and splitting `do`s.
pub fn eval(
    form: &Value,
    eval_unit: &mut dyn FnMut(&Value) -> Result<Value, EvalError>,
) -> Result<Value, EvalError> {
    let expanded = macroexpand(form)?;
    if let Some(items) = crate::analyzer::seq_items(&expanded)
        && items
            .first()
            .and_then(symbol::as_sym)
            .is_some_and(|s| s.ns().is_none() && s.name() == "do")
    {
        let mut last = Value::NIL;
        for sub in &items[1..] {
            last = eval(sub, eval_unit)?;
        }
        return Ok(last);
    }
    eval_unit(&expanded)
}
