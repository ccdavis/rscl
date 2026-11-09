//! Last-use analysis for locals clearing.
//!
//! A local's last use in a function body transfers the value out of the variable
//! (the variable becomes `nil`), so a function holding the head of a long lazy
//! sequence stops holding it as soon as it has passed it on. Clojure clears locals
//! the same way; without it `(reduce + (range n))` keeps every realised cell alive.
//!
//! The walk is backward over the evaluation order the code generator uses, with the
//! set of locals still needed afterwards. Inside a `loop` body, locals bound outside
//! the loop are needed on every iteration and are never cleared there; loop locals
//! themselves are rebound by every `recur`, so their last use in the body is a real
//! last use. Locals used in a `catch` or `finally` stay live throughout the `try`.
use std::collections::HashSet;

use crate::analyzer::{FnNode, LocalId, Node};

/// Per-use decisions, keyed by `(node address, local)`.
#[derive(Default)]
pub struct Liveness {
    /// The use of the local at that node is its last: transfer the value out.
    pub last: HashSet<(usize, LocalId)>,
    /// The local is passed as a borrowed operand that stays pending while a later
    /// argument of the same call transfers it away: retain it for the operand.
    pub retain: HashSet<(usize, LocalId)>,
    /// Last uses in marking order (for finding the ones inside an argument).
    log: Vec<LocalId>,
}

impl Liveness {
    pub fn extend(&mut self, other: Liveness) {
        self.last.extend(other.last);
        self.retain.extend(other.retain);
    }

    fn mark_last(&mut self, node: &Node, id: LocalId) {
        self.last.insert((addr(node), id));
        self.log.push(id);
    }
}

pub fn last_uses(body: &Node) -> Liveness {
    let mut out = Liveness::default();
    let mut live = HashSet::new();
    walk(body, &mut live, &mut out);
    out
}

fn addr(n: &Node) -> usize {
    n as *const Node as usize
}

/// The local whose variable an argument expression reads as its own (borrowed)
/// operand: a bare local, or a `do` ending in one. Other expressions produce owned
/// results.
fn passes_through(node: &Node) -> Option<(&Node, LocalId)> {
    match node {
        Node::Local(id) => Some((node, *id)),
        Node::Do(nodes) => nodes.last().and_then(passes_through),
        _ => None,
    }
}

/// Walk an argument list (evaluated left to right) backward. A borrowed local
/// operand evaluated before a later argument that transfers the same local out would
/// dangle, so such operands are marked for retention.
fn walk_args<'a>(
    args: impl Iterator<Item = &'a Node>,
    live: &mut HashSet<LocalId>,
    out: &mut Liveness,
) {
    let mut later: HashSet<LocalId> = HashSet::new();
    for a in args {
        if let Some((local_node, x)) = passes_through(a)
            && live.contains(&x)
            && later.contains(&x)
        {
            out.retain.insert((addr(local_node), x));
        }
        let before = out.log.len();
        walk(a, live, out);
        later.extend(out.log[before..].iter().copied());
    }
}

fn walk(node: &Node, live: &mut HashSet<LocalId>, out: &mut Liveness) {
    match node {
        Node::Const(_) | Node::VarRef(_) | Node::TheVar(_) | Node::CurrentNs => {}
        Node::Local(id) => {
            if live.insert(*id) {
                out.mark_last(node, *id);
            }
        }
        Node::Def { init, .. } => {
            if let Some(i) = init {
                walk(i, live, out);
            }
        }
        Node::VarSet { value, .. } => walk(value, live, out),
        Node::If { test, then, else_ } => {
            let mut l1 = live.clone();
            walk(then, &mut l1, out);
            let mut l2 = live.clone();
            walk(else_, &mut l2, out);
            live.extend(l1);
            live.extend(l2);
            walk(test, live, out);
        }
        Node::Do(nodes) => {
            for n in nodes.iter().rev() {
                walk(n, live, out);
            }
        }
        Node::Let { bindings, body } => {
            walk(body, live, out);
            for (id, init) in bindings.iter().rev() {
                live.remove(id);
                walk(init, live, out);
            }
        }
        Node::Loop { bindings, body, .. } => {
            // Outer locals the body uses are needed again on the next iteration.
            let loop_ids: HashSet<LocalId> = bindings.iter().map(|(id, _)| *id).collect();
            let mut used = HashSet::new();
            let mut bound = HashSet::new();
            collect(body, &mut used, &mut bound);
            let outer: Vec<LocalId> = used
                .into_iter()
                .filter(|id| !bound.contains(id) && !loop_ids.contains(id))
                .collect();
            live.extend(outer.iter().copied());
            walk(body, live, out);
            live.extend(outer);
            for (id, init) in bindings.iter().rev() {
                live.remove(id);
                walk(init, live, out);
            }
        }
        Node::Recur { args, .. } => walk_args(args.iter().rev(), live, out),
        Node::Fn(f) => {
            // Captures are copied when the closure is created: that is the use.
            for id in f.captures.iter().rev() {
                if live.insert(*id) {
                    out.mark_last(node, *id);
                }
            }
        }
        // The callee operand is evaluated first and stays pending during the arguments.
        Node::Call { f, args, .. } => {
            walk_args(args.iter().rev().chain(std::iter::once(&**f)), live, out)
        }
        Node::BuiltinCall { args, .. } | Node::NativeCall { args, .. } => {
            walk_args(args.iter().rev(), live, out)
        }
        Node::Coll { items, .. } => walk_args(items.iter().rev(), live, out),
        Node::Throw(e) => walk(e, live, out),
        Node::Try {
            body,
            catches,
            finally,
        } => {
            if let Some(f) = finally {
                walk(f, live, out);
            }
            // Any local a handler needs may be needed after any point of the body.
            let after = live.clone();
            let mut handlers = HashSet::new();
            for c in catches {
                let mut lc = after.clone();
                walk(&c.body, &mut lc, out);
                lc.remove(&c.local);
                handlers.extend(lc);
            }
            live.extend(handlers);
            walk(body, live, out);
        }
    }
}

/// Locals referenced anywhere below `node` (captures included) and locals bound
/// below it.
fn collect(node: &Node, used: &mut HashSet<LocalId>, bound: &mut HashSet<LocalId>) {
    match node {
        Node::Const(_) | Node::VarRef(_) | Node::TheVar(_) | Node::CurrentNs => {}
        Node::Local(id) => {
            used.insert(*id);
        }
        Node::Def { init, .. } => {
            if let Some(i) = init {
                collect(i, used, bound);
            }
        }
        Node::VarSet { value, .. } => collect(value, used, bound),
        Node::If { test, then, else_ } => {
            collect(test, used, bound);
            collect(then, used, bound);
            collect(else_, used, bound);
        }
        Node::Do(nodes) => {
            for n in nodes {
                collect(n, used, bound);
            }
        }
        Node::Let { bindings, body } | Node::Loop { bindings, body, .. } => {
            for (id, init) in bindings {
                bound.insert(*id);
                collect(init, used, bound);
            }
            collect(body, used, bound);
        }
        Node::Recur { args, .. } => {
            for a in args {
                collect(a, used, bound);
            }
        }
        Node::Fn(f) => {
            let f: &FnNode = f;
            used.extend(f.captures.iter().copied());
        }
        Node::Call { f, args, .. } => {
            collect(f, used, bound);
            for a in args {
                collect(a, used, bound);
            }
        }
        Node::BuiltinCall { args, .. } | Node::NativeCall { args, .. } => {
            for a in args {
                collect(a, used, bound);
            }
        }
        Node::Coll { items, .. } => {
            for a in items {
                collect(a, used, bound);
            }
        }
        Node::Throw(e) => collect(e, used, bound),
        Node::Try {
            body,
            catches,
            finally,
        } => {
            collect(body, used, bound);
            for c in catches {
                bound.insert(c.local);
                collect(&c.body, used, bound);
            }
            if let Some(f) = finally {
                collect(f, used, bound);
            }
        }
    }
}
