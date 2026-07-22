//! Rust adapter: parses source with [syn](https://docs.rs/syn) and lowers the
//! AST into the language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core` and `syn`, with no
//! CLI machinery, so embedders pay nothing for clap/ignore/rayon. The unified
//! `cccc` binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] in its language registry and dispatches
//! `.rs` files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about (functions/methods/closures, `if`/`else`, `match`,
//! loops, labelled jumps, `&&`/`||` sequences, calls) and emits the matching IR
//! nodes. All complexity rules live in [`cccc_core::engine`].
//!
//! ## Why a `Visit`-driven builder
//!
//! Lowering is driven by syn's [`Visit`] trait. Its default `visit_*` methods
//! traverse the *entire* AST; we override only the nodes that produce IR, so a
//! nested function or logical operator appearing in any expression position is
//! still reached — we never have to enumerate every node kind by hand. The IR
//! tree is assembled with a stack of "collectors": [`Builder::collect`] pushes a
//! fresh child vector, runs a sub-traversal, and pops the nodes it gathered.
//!
//! ## Rust-to-IR mapping notes
//!
//! - `fn` / `impl` method / trait default method / closure → [`Node::Function`].
//! - `if` / `else if` / `else` → [`Node::Branch`] (chaining `else if` as a nested
//!   `Branch` so it scores flat). `if let` / `while let` are just the same nodes.
//! - `for` / `while` / `loop` → [`Node::Loop`].
//! - `match` → [`Node::Switch`]; an unguarded `_` (or bare binding) arm is the
//!   `default`. An arm guard (`pat if cond`) is visited inside the case body and
//!   keeps the arm a decision point.
//! - labelled `break 'a` / `continue 'a` → [`Node::Jump`] (`labeled: true`).
//! - `&&` / `||` runs → folded [`Node::Logical`] (one node per like-operator run).
//! - calls (`f(..)`, `obj.m(..)`) → [`Node::Call`] for recursion detection.
//!
//! Rust has no ternary (`if` is an expression instead) and no `try`/`catch`
//! (errors propagate via `?`), so no `Conditional`/`Catch` nodes are emitted.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    BinOp, Expr, ExprBinary, ExprBreak, ExprCall, ExprClosure, ExprContinue, ExprForLoop, ExprIf,
    ExprLoop, ExprMatch, ExprMethodCall, ExprWhile, ImplItemFn, ItemFn, Local, Pat, TraitItemFn,
};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["rs"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any parser error messages. `syn` parses a whole file at once and
/// does not recover from syntax errors, so a parse failure yields an empty node
/// list and a single error string.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    match syn::parse_file(source) {
        Ok(file) => {
            let mut builder = Builder::new();
            for item in &file.items {
                builder.visit_item(item);
            }
            (builder.finish(), Vec::new())
        }
        Err(e) => (Vec::new(), vec![e.to_string()]),
    }
}

/// Assembles the IR tree while syn's `Visit` drives a complete AST traversal.
struct Builder {
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
    /// Name captured from a `let` binding to label the next closure.
    pending_name: Option<String>,
}

impl Builder {
    fn new() -> Self {
        Self {
            stack: vec![Vec::new()], // module-level collector
            pending_name: None,
        }
    }

    /// The module-level node list (the single remaining collector).
    fn finish(mut self) -> Vec<Node> {
        self.stack.pop().expect("module collector")
    }

    /// Append a node to the current collector.
    fn emit(&mut self, node: Node) {
        self.stack.last_mut().expect("collector").push(node);
    }

    /// Run `f` against a fresh collector and return the nodes it gathered.
    fn collect<F: FnOnce(&mut Self)>(&mut self, f: F) -> Vec<Node> {
        self.stack.push(Vec::new());
        f(self);
        self.stack.pop().expect("collector")
    }

    /// Emit a `Function` whose body is whatever `walk` gathers in a sub-traversal.
    fn emit_function<F: FnOnce(&mut Self)>(
        &mut self,
        name: String,
        kind: &'static str,
        line: u32,
        walk: F,
    ) {
        let body = self.collect(walk);
        self.emit(Node::Function {
            name,
            kind: kind.to_string(),
            line,
            body,
        });
    }

    /// Build an `if` (recursively, so `else if` becomes a nested `Branch`).
    fn lower_if(&mut self, it: &ExprIf) -> Node {
        let test = self.collect(|s| s.visit_expr(&it.cond));
        let then = self.collect(|s| s.visit_block(&it.then_branch));
        let alternate = it
            .else_branch
            .as_ref()
            .map(|(_, alt)| Box::new(self.lower_alternate(alt)));
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// `else if` → nested `Branch`; plain `else { .. }` → `Group`.
    fn lower_alternate(&mut self, expr: &Expr) -> Node {
        match expr {
            Expr::If(elif) => self.lower_if(elif),
            other => Node::Group(self.collect(|s| s.visit_expr(other))),
        }
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    fn collect_logical(&mut self, expr: &ExprBinary, op: LogicalOp, operands: &mut Vec<Node>) {
        self.collect_logical_side(&expr.left, op, operands);
        self.collect_logical_side(&expr.right, op, operands);
    }

    fn collect_logical_side(&mut self, side: &Expr, op: LogicalOp, operands: &mut Vec<Node>) {
        match side {
            Expr::Binary(inner) => match logical_op(&inner.op) {
                Some(inner_op) if inner_op == op => self.collect_logical(inner, op, operands),
                Some(inner_op) => {
                    let mut sub = Vec::new();
                    self.collect_logical(inner, inner_op, &mut sub);
                    operands.push(Node::Logical {
                        op: inner_op,
                        operands: sub,
                    });
                }
                None => operands.push(Node::Group(self.collect(|s| s.visit_expr(side)))),
            },
            Expr::Paren(p) => self.collect_logical_side(&p.expr, op, operands),
            Expr::Group(g) => self.collect_logical_side(&g.expr, op, operands),
            _ => operands.push(Node::Group(self.collect(|s| s.visit_expr(side)))),
        }
    }
}

impl<'ast> Visit<'ast> for Builder {
    fn visit_item_fn(&mut self, it: &'ast ItemFn) {
        let name = it.sig.ident.to_string();
        let line = line_of(&it.sig.ident);
        self.emit_function(name, "function", line, |s| visit::visit_item_fn(s, it));
    }

    fn visit_impl_item_fn(&mut self, it: &'ast ImplItemFn) {
        let name = it.sig.ident.to_string();
        let line = line_of(&it.sig.ident);
        self.emit_function(name, "method", line, |s| visit::visit_impl_item_fn(s, it));
    }

    fn visit_trait_item_fn(&mut self, it: &'ast TraitItemFn) {
        // Only a default-bodied trait method is a measurable unit; a bare
        // signature carries no complexity, so don't report it as a function.
        if it.default.is_some() {
            let name = it.sig.ident.to_string();
            let line = line_of(&it.sig.ident);
            self.emit_function(name, "method", line, |s| visit::visit_trait_item_fn(s, it));
        }
    }

    fn visit_local(&mut self, it: &'ast Local) {
        if let Some(init) = &it.init
            && matches!(&*init.expr, Expr::Closure(_))
        {
            self.pending_name = pat_name(&it.pat);
        }
        visit::visit_local(self, it);
    }

    fn visit_expr_closure(&mut self, it: &'ast ExprClosure) {
        let name = self
            .pending_name
            .take()
            .unwrap_or_else(|| "<closure>".to_string());
        let line = line_of(it);
        self.emit_function(name, "closure", line, |s| visit::visit_expr_closure(s, it));
    }

    fn visit_expr_if(&mut self, it: &'ast ExprIf) {
        let node = self.lower_if(it);
        self.emit(node);
    }

    fn visit_expr_match(&mut self, it: &'ast ExprMatch) {
        // Visit the scrutinee at the match's own level (matches walk order),
        // then gather each arm body.
        let head = self.collect(|s| s.visit_expr(&it.expr));
        for node in head {
            self.emit(node);
        }
        let mut cases = Vec::new();
        for arm in &it.arms {
            let body = self.collect(|s| {
                if let Some(guard) = arm_guard(&arm.pat) {
                    s.visit_expr(guard);
                }
                s.visit_expr(&arm.body);
            });
            cases.push(SwitchCase {
                is_default: is_catch_all(&arm.pat),
                body,
            });
        }
        self.emit(Node::Switch { cases });
    }

    fn visit_expr_for_loop(&mut self, it: &'ast ExprForLoop) {
        let body = self.collect(|s| visit::visit_expr_for_loop(s, it));
        self.emit(Node::Loop { body });
    }

    fn visit_expr_while(&mut self, it: &'ast ExprWhile) {
        let body = self.collect(|s| visit::visit_expr_while(s, it));
        self.emit(Node::Loop { body });
    }

    fn visit_expr_loop(&mut self, it: &'ast ExprLoop) {
        let body = self.collect(|s| visit::visit_expr_loop(s, it));
        self.emit(Node::Loop { body });
    }

    fn visit_expr_break(&mut self, it: &'ast ExprBreak) {
        self.emit(Node::Jump {
            labeled: it.label.is_some(),
        });
        visit::visit_expr_break(self, it);
    }

    fn visit_expr_continue(&mut self, it: &'ast ExprContinue) {
        self.emit(Node::Jump {
            labeled: it.label.is_some(),
        });
    }

    fn visit_expr_binary(&mut self, it: &'ast ExprBinary) {
        match logical_op(&it.op) {
            Some(op) => {
                let mut operands = Vec::new();
                self.collect_logical(it, op, &mut operands);
                self.emit(Node::Logical { op, operands });
            }
            None => visit::visit_expr_binary(self, it),
        }
    }

    fn visit_expr_call(&mut self, it: &'ast ExprCall) {
        self.emit(Node::Call {
            callee: call_path_name(&it.func),
        });
        visit::visit_expr_call(self, it);
    }

    fn visit_expr_method_call(&mut self, it: &'ast ExprMethodCall) {
        self.emit(Node::Call {
            callee: Some(it.method.to_string()),
        });
        visit::visit_expr_method_call(self, it);
    }
}

/// 1-based start line of any spanned node (requires proc-macro2 span-locations).
fn line_of<T: Spanned>(node: &T) -> u32 {
    node.span().start().line as u32
}

/// `&&` / `||` map to the normalized logical ops; everything else is not a
/// logical sequence. (Rust has no nullish-coalescing operator.)
fn logical_op(op: &BinOp) -> Option<LogicalOp> {
    match op {
        BinOp::And(_) => Some(LogicalOp::And),
        BinOp::Or(_) => Some(LogicalOp::Or),
        _ => None,
    }
}

/// The guard of a guarded arm pattern (`pat if cond => ..`). Since syn 3 the
/// guard hangs off the pattern as [`Pat::Guard`] rather than off the `Arm`.
fn arm_guard(pat: &Pat) -> Option<&Expr> {
    match pat {
        Pat::Guard(g) => Some(&g.guard),
        _ => None,
    }
}

/// A `match` arm that always matches: `_` or a bare binding (`other => ..`).
/// A guard makes the arm conditional, so `Pat::Guard` is never a catch-all.
fn is_catch_all(pat: &Pat) -> bool {
    match pat {
        Pat::Wild(_) => true,
        Pat::Ident(p) => p.subpat.is_none(),
        _ => false,
    }
}

/// Name bound by a `let` pattern, unwrapping a type ascription.
fn pat_name(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(p) => Some(p.ident.to_string()),
        Pat::Type(p) => pat_name(&p.pat),
        _ => None,
    }
}

/// Simple name of a directly-called callee (`foo(..)` or `path::foo(..)`), used
/// for recursion detection. Returns the last path segment.
fn call_path_name(func: &Expr) -> Option<String> {
    match func {
        Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.rs"), src)
    }

    fn find<'a>(
        fns: &'a [cccc_core::report::FunctionReport],
        name: &str,
    ) -> Option<&'a cccc_core::report::FunctionReport> {
        for f in fns {
            if f.name == name {
                return Some(f);
            }
            if let Some(found) = find(&f.children, name) {
                return Some(found);
            }
        }
        None
    }

    fn cognitive_of(src: &str, name: &str) -> u32 {
        find(&analyze(src).functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cognitive
    }

    fn cyclomatic_of(src: &str, name: &str) -> u32 {
        find(&analyze(src).functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cyclomatic
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        let src = r#"
            fn sum_of_primes(max: u32) -> u32 {
                let mut total = 0;
                'out: for i in 1..=max {
                    for j in 2..i {
                        if i % j == 0 {
                            continue 'out;
                        }
                    }
                    total += i;
                }
                total
            }
        "#;
        // for(+1) + nested for(+2) + nested if(+3) + labelled continue(+1) = 7
        assert_eq!(cognitive_of(src, "sum_of_primes"), 7);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
            fn get_words(number: u32) -> &'static str {
                match number {
                    1 => "one",
                    2 => "a couple",
                    _ => "lots",
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "get_words"), 1);
        // base 1 + 2 non-default arms = 3
        assert_eq!(cyclomatic_of(src, "get_words"), 3);
    }

    #[test]
    fn guarded_arms_are_decision_points() {
        let src = r#"
            fn f(n: i32) -> i32 {
                match n {
                    x if x > 10 && x < 20 => 1,
                    _ if n == 0 => 2,
                    _ => 3,
                }
            }
        "#;
        // match(+1) + && seq inside the guard(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + 2 guarded (non-default) arms + (&& 2 operands => +1) = 4;
        // only the bare `_` arm is the default.
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            fn f(a: bool, b: bool, c: bool) {
                if a { if b { if c {} } }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 6);
    }

    #[test]
    fn else_if_else_are_flat() {
        let src = r#"
            fn f(a: bool, b: bool) {
                if a {} else if b {} else {}
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences() {
        let src = r#"
            fn f(a: bool, b: bool, c: bool, d: bool) {
                if a && b && c || d {}
            }
        "#;
        // if(+1) + && seq(+1) + || seq(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
            fn fib(n: u64) -> u64 {
                if n < 2 { return n; }
                fib(n - 1) + fib(n - 2)
            }
        "#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn method_recursion_is_detected() {
        let src = r#"
            struct S;
            impl S {
                fn walk(&self, n: u64) -> u64 {
                    if n == 0 { 0 } else { self.walk(n - 1) }
                }
            }
        "#;
        // if/else: if(+1) + else(+1) + recursion(+1) = 3
        assert_eq!(cognitive_of(src, "walk"), 3);
    }

    #[test]
    fn nested_function_is_independent_unit() {
        let src = r#"
            fn outer() {
                fn inner() { if true {} }
            }
        "#;
        assert_eq!(cognitive_of(src, "outer"), 0);
        assert_eq!(cognitive_of(src, "inner"), 1);
    }

    #[test]
    fn closure_is_its_own_unit() {
        let src = r#"
            fn host() {
                let pick = |a: bool, b: bool| if a && b { 1 } else { 0 };
            }
        "#;
        // host owns no structural complexity; the closure does.
        assert_eq!(cognitive_of(src, "host"), 0);
        // if(+1) + && seq(+1) + else(+1) = 3
        assert_eq!(cognitive_of(src, "pick"), 3);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            fn f() {
                while true {}
                for _ in 0..3 {}
                loop { break; }
            }
        "#;
        // three loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn cyclomatic_basic() {
        let src = r#"
            fn f(a: bool, b: bool) {
                if a && b { for _ in 0..1 {} } else if b {}
            }
        "#;
        // base 1 + if 1 + (&& => +1) + for 1 + else if 1 = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn names_methods_and_closures() {
        let src = r#"
            fn free() {}
            struct C;
            impl C { fn method(&self) {} }
            fn host() { let lambda = |x: u32| x + 1; }
        "#;
        let r = analyze(src);
        assert_eq!(find(&r.functions, "free").unwrap().kind, "function");
        assert_eq!(find(&r.functions, "method").unwrap().kind, "method");
        assert_eq!(find(&r.functions, "lambda").unwrap().kind, "closure");
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
            fn a() { if true {} }
            fn b() { if true {} }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn parse_error_is_reported() {
        let (nodes, errors) = to_ir(Path::new("bad.rs"), "fn f( {");
        assert!(nodes.is_empty());
        assert_eq!(errors.len(), 1);
    }
}
