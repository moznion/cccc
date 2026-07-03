//! Common Lisp adapter: reads source with [lispexp](https://docs.rs/lispexp)
//! and lowers the S-expression datum tree into the language-agnostic
//! [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core` and the pure-Rust
//! `lispexp` reader (no C toolchain, so cross-compilation stays clean), with no
//! CLI machinery. The unified `cccc` binary registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.lisp`/`.lsp`/`.cl` files
//! to it.
//!
//! This crate contains **no scoring logic** — it recognizes the Common Lisp
//! special forms/macros the engine cares about and emits the matching IR
//! nodes; every rule lives in [`cccc_core::engine`].
//!
//! ## Lowering strategy
//!
//! The skeleton mirrors [`cccc-scheme`](https://docs.rs/cccc-scheme): a stack of
//! "collector" vectors builds the IR while [`lispexp::walk_regions`] (ADR-0026)
//! makes the code-vs-data judgment (skip quoted data, descend into the code
//! carried by `` ` ``/`,`), so the adapter never re-derives quote/quasiquote
//! nesting rules. Each `Region::Code` list is dispatched on its head symbol.
//!
//! Common Lisp's reader is **case-insensitive** (`DEFUN` == `defun`), so the
//! head symbol is lower-cased before dispatch.
//!
//! ## Common-Lisp-to-IR mapping
//!
//! - `defun` / `defmacro` / `defmethod` / `lambda`, and the local-function
//!   forms `flet` / `labels` → [`Node::Function`] (each its own unit).
//! - `if` → [`Node::Conditional`] (Lisp's `if` is a single-decision
//!   expression, like `?:`); `when` / `unless` → [`Node::Branch`]; `cond` → a
//!   flat `Branch` chain (a `t` clause is the terminal `else`); `case` /
//!   `ecase` / `ccase` / `typecase` / `etypecase` / `ctypecase` →
//!   [`Node::Switch`] (`t` / `otherwise` = default).
//! - `loop` / `do` / `do*` / `dotimes` / `dolist` → [`Node::Loop`].
//! - `and` / `or` → folded [`Node::Logical`].
//! - `handler-case` clauses → [`Node::Catch`] (the protected form scores at the
//!   surrounding level; a `:no-error` clause is transparent).
//! - `go` (a `tagbody` goto) → a labelled [`Node::Jump`].
//! - a plain application → [`Node::Call`] (recursion detection).
//! - `let`/`let*`/`progn`/`multiple-value-bind`/… are transparent; `defvar`/
//!   `defparameter`/`defconstant` lower their init form; `defstruct`/`defclass`/
//!   `defpackage`/`declaim`/… and `quote` data are skipped.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use lispexp::{Datum, DatumKind, Options, Region, Walk, parse, walk_regions};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["lisp", "lsp", "cl"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR use
/// [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any reader diagnostics. `lispexp` is fault-tolerant: it always
/// yields a (possibly partial) tree, so we lower whatever it recovered and
/// surface the diagnostics alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let parsed = parse(source, &Options::common_lisp());
    let mut builder = Builder::new();
    builder.lower_seq(&parsed.data);
    let errors = parsed.errors.iter().map(ToString::to_string).collect();
    (builder.finish(), errors)
}

/// Assembles the IR tree while we recurse the datum tree.
struct Builder {
    stack: Vec<Vec<Node>>,
}

impl Builder {
    fn new() -> Self {
        Self {
            stack: vec![Vec::new()],
        }
    }

    fn finish(mut self) -> Vec<Node> {
        self.stack.pop().expect("module collector")
    }

    fn emit(&mut self, node: Node) {
        self.stack.last_mut().expect("collector").push(node);
    }

    fn collect<F: FnOnce(&mut Self)>(&mut self, f: F) -> Vec<Node> {
        self.stack.push(Vec::new());
        f(self);
        self.stack.pop().expect("collector")
    }

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

    fn lower_seq(&mut self, items: &[Datum]) {
        for d in items {
            self.lower_datum(d);
        }
    }

    /// Lower `d` if it sits in code position, delegating code-vs-data to
    /// [`lispexp::walk_regions`] (see the `cccc-scheme` adapter for the full
    /// rationale). Only a `Region::Code` list we hand to `lower_list` returns
    /// `Walk::Skip` (it does its own targeted recursion); sealed data prunes;
    /// everything else descends.
    fn lower_datum(&mut self, d: &Datum) {
        walk_regions(std::slice::from_ref(d), |dd, region| {
            if region == Region::Code
                && let DatumKind::List { items, tail, .. } = &dd.kind
            {
                self.lower_list(dd, items, tail.as_deref());
                return Walk::Skip;
            }
            if region.is_prunable() {
                return Walk::Skip;
            }
            Walk::Descend
        });
    }

    fn lower_list(&mut self, d: &Datum, items: &[Datum], tail: Option<&Datum>) {
        if items.is_empty() {
            return;
        }
        // The CL reader is case-insensitive, so match on the lower-cased head.
        let head = head_symbol(items).map(str::to_ascii_lowercase);
        match head.as_deref() {
            // ---- definitions ----
            Some("defun") | Some("defmacro") => self.lower_defun(d, items, "defun"),
            Some("defmethod") => self.lower_defmethod(d, items),
            Some("lambda") => self.emit_lambda("<lambda>".to_string(), items, d.line),
            Some("flet") | Some("labels") | Some("macrolet") => self.lower_flet(items),
            // ---- conditionals ----
            Some("if") => self.lower_if(items),
            Some("when") | Some("unless") => self.lower_when(items),
            Some("cond") => {
                if let Some(node) = self.lower_cond_clauses(&items[1..]) {
                    self.emit(*node);
                }
            }
            Some("case") | Some("ccase") | Some("ecase") | Some("typecase") | Some("etypecase")
            | Some("ctypecase") => self.lower_case(items),
            Some("and") => self.lower_logical(LogicalOp::And, &items[1..]),
            Some("or") => self.lower_logical(LogicalOp::Or, &items[1..]),
            // ---- loops ----
            Some("loop") => {
                let body = self.collect(|b| b.lower_seq(&items[1..]));
                self.emit(Node::Loop { body });
            }
            Some("dotimes") | Some("dolist") => self.lower_iter_loop(items),
            Some("do") | Some("do*") => self.lower_do(items),
            // ---- exceptions ----
            Some("handler-case") => self.lower_handler_case(items),
            // `go` transfers control to a `tagbody` tag — a genuine goto.
            Some("go") => self.emit(Node::Jump { labeled: true }),
            // ---- binding / grouping: transparent ----
            Some("let") | Some("let*") => self.lower_let(items),
            Some("multiple-value-bind") | Some("destructuring-bind") => {
                // (… (vars) values-form body…) — lower values-form + body.
                self.lower_seq(items.get(2..).unwrap_or(&[]));
            }
            Some("progn")
            | Some("prog1")
            | Some("prog2")
            | Some("progv")
            | Some("locally")
            | Some("the")
            | Some("values")
            | Some("multiple-value-call")
            | Some("multiple-value-prog1")
            | Some("eval-when")
            | Some("block")
            | Some("tagbody")
            | Some("unwind-protect")
            | Some("catch")
            | Some("with-open-file")
            | Some("with-output-to-string")
            | Some("with-input-from-string")
            | Some("with-slots")
            | Some("with-accessors")
            | Some("setf")
            | Some("setq")
            | Some("psetf")
            | Some("psetq")
            | Some("push")
            | Some("pushnew")
            | Some("pop")
            | Some("incf")
            | Some("decf")
            | Some("return")
            | Some("return-from")
            | Some("throw") => self.lower_seq(&items[1..]),
            // ---- value definitions: lower the init form ----
            Some("defvar") | Some("defparameter") | Some("defconstant") => {
                self.lower_seq(items.get(2..).unwrap_or(&[]));
            }
            // ---- data / declarations / compile-time: skip ----
            Some("quote") => {}
            Some("defstruct")
            | Some("defclass")
            | Some("defgeneric")
            | Some("defpackage")
            | Some("define-condition")
            | Some("in-package")
            | Some("declaim")
            | Some("proclaim")
            | Some("declare")
            | Some("deftype")
            | Some("defsetf")
            | Some("define-setf-expander")
            | Some("define-symbol-macro")
            | Some("define-compiler-macro") => {}
            // A plain application.
            _ => self.lower_call(items, tail),
        }
    }

    // ---- functions --------------------------------------------------------

    /// `(defun name (args) body…)` / `(defmacro name (args) body…)`.
    fn lower_defun(&mut self, d: &Datum, items: &[Datum], kind: &'static str) {
        let name = items
            .get(1)
            .and_then(as_symbol)
            .unwrap_or("<defun>")
            .to_string();
        let body = items.get(3..).unwrap_or(&[]).to_vec();
        self.emit_function(name, kind, d.line, |b| b.lower_seq(&body));
    }

    /// `(defmethod name [qualifier…] (specialized-args) body…)` — an optional
    /// run of qualifiers (symbols/keywords) precedes the specialized arg list.
    fn lower_defmethod(&mut self, d: &Datum, items: &[Datum]) {
        let name = items
            .get(1)
            .and_then(as_symbol)
            .unwrap_or("<defmethod>")
            .to_string();
        // The specialized arglist is the first *list* after the name.
        let arglist_pos = items
            .iter()
            .enumerate()
            .skip(2)
            .find(|(_, it)| matches!(it.kind, DatumKind::List { .. }))
            .map(|(i, _)| i);
        let body = match arglist_pos {
            Some(i) => items.get(i + 1..).unwrap_or(&[]).to_vec(),
            None => Vec::new(),
        };
        self.emit_function(name, "defmethod", d.line, |b| b.lower_seq(&body));
    }

    /// `(lambda (args) body…)`, under `name`.
    fn emit_lambda(&mut self, name: String, items: &[Datum], line: u32) {
        let body = items.get(2..).unwrap_or(&[]).to_vec();
        self.emit_function(name, "lambda", line, |b| b.lower_seq(&body));
    }

    /// `(flet ((name (args) body…)…) body…)` — each local binding is its own
    /// function unit; the `flet` body scores at the surrounding level.
    fn lower_flet(&mut self, items: &[Datum]) {
        if let Some(DatumKind::List { items: binds, .. }) = items.get(1).map(|d| &d.kind) {
            for b in binds {
                if let DatumKind::List { items: bi, .. } = &b.kind
                    && let Some(name) = bi.first().and_then(as_symbol)
                {
                    let name = name.to_string();
                    let body = bi.get(2..).unwrap_or(&[]).to_vec();
                    self.emit_function(name, "flet", b.line, |bl| bl.lower_seq(&body));
                }
            }
        }
        self.lower_seq(items.get(2..).unwrap_or(&[]));
    }

    // ---- let --------------------------------------------------------------

    /// `let` / `let*`: transparent scoping; lower binding initializers + body.
    fn lower_let(&mut self, items: &[Datum]) {
        self.lower_binding_inits(items.get(1));
        self.lower_seq(items.get(2..).unwrap_or(&[]));
    }

    /// Lower the initializer of each `(var init)` binding (a bare `var` has none).
    fn lower_binding_inits(&mut self, bindings: Option<&Datum>) {
        if let Some(DatumKind::List { items: binds, .. }) = bindings.map(|d| &d.kind) {
            for b in binds {
                if let DatumKind::List { items: kv, .. } = &b.kind
                    && let Some(init) = kv.get(1)
                {
                    self.lower_datum(init);
                }
            }
        }
    }

    // ---- branches ---------------------------------------------------------

    /// `(if test then else)` — a single-decision expression → [`Node::Conditional`].
    fn lower_if(&mut self, items: &[Datum]) {
        let test = self.collect(|b| b.lower_opt(items.get(1)));
        let then = self.collect(|b| b.lower_opt(items.get(2)));
        let alternate = self.collect(|b| b.lower_opt(items.get(3)));
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    fn lower_when(&mut self, items: &[Datum]) {
        let test = self.collect(|b| b.lower_opt(items.get(1)));
        let then = self.collect(|b| b.lower_seq(items.get(2..).unwrap_or(&[])));
        self.emit(Node::Branch {
            test,
            then,
            alternate: None,
        });
    }

    /// Lower a `cond` clause list into a flat `Branch` chain. A `t` clause is
    /// the terminal `else` (flat, no decision point).
    fn lower_cond_clauses(&mut self, clauses: &[Datum]) -> Option<Box<Node>> {
        let (first, rest) = clauses.split_first()?;
        let DatumKind::List { items: ci, .. } = &first.kind else {
            return self.lower_cond_clauses(rest);
        };
        if is_else(ci.first()) {
            let body = self.collect(|b| b.lower_seq(ci.get(1..).unwrap_or(&[])));
            return Some(Box::new(Node::Group(body)));
        }
        let test = self.collect(|b| b.lower_opt(ci.first()));
        let then = self.collect(|b| b.lower_seq(ci.get(1..).unwrap_or(&[])));
        let alternate = self.lower_cond_clauses(rest);
        Some(Box::new(Node::Branch {
            test,
            then,
            alternate,
        }))
    }

    fn lower_case(&mut self, items: &[Datum]) {
        // The keyform runs at the switch's own level, before the clauses.
        self.lower_opt(items.get(1));
        let mut cases = Vec::new();
        for cl in items.get(2..).unwrap_or(&[]) {
            if let DatumKind::List { items: ci, .. } = &cl.kind {
                let is_default = is_else(ci.first()) || is_otherwise(ci.first());
                let body = self.collect(|b| b.lower_seq(ci.get(1..).unwrap_or(&[])));
                cases.push(SwitchCase { is_default, body });
            }
        }
        self.emit(Node::Switch { cases });
    }

    // ---- loops ------------------------------------------------------------

    /// `(dotimes (var count) body…)` / `(dolist (var list) body…)`: the spec's
    /// count/list form runs once at the surrounding level; the body loops.
    fn lower_iter_loop(&mut self, items: &[Datum]) {
        if let Some(DatumKind::List { items: spec, .. }) = items.get(1).map(|d| &d.kind) {
            self.lower_seq(spec.get(1..).unwrap_or(&[]));
        }
        let body = self.collect(|b| b.lower_seq(items.get(2..).unwrap_or(&[])));
        self.emit(Node::Loop { body });
    }

    /// `(do ((var init step)…) (end result…) body…)` — inits run once at the
    /// surrounding level; steps/end/body loop.
    fn lower_do(&mut self, items: &[Datum]) {
        self.lower_do_specs(items.get(1), 1);
        let items_owned = items.to_vec();
        let body = self.collect(|b| {
            b.lower_do_specs(items_owned.get(1), 2);
            if let Some(DatumKind::List { items: tr, .. }) = items_owned.get(2).map(|d| &d.kind) {
                b.lower_seq(tr);
            }
            b.lower_seq(items_owned.get(3..).unwrap_or(&[]));
        });
        self.emit(Node::Loop { body });
    }

    fn lower_do_specs(&mut self, specs: Option<&Datum>, index: usize) {
        if let Some(DatumKind::List { items: specs, .. }) = specs.map(|d| &d.kind) {
            for s in specs {
                if let DatumKind::List { items: kv, .. } = &s.kind
                    && let Some(e) = kv.get(index)
                {
                    self.lower_datum(e);
                }
            }
        }
    }

    // ---- exceptions -------------------------------------------------------

    /// `(handler-case protected (type (var) body…)…)` — protected form at the
    /// surrounding level; each error clause is a `Catch`. A `:no-error` clause
    /// is a success continuation, not a handler.
    fn lower_handler_case(&mut self, items: &[Datum]) {
        self.lower_opt(items.get(1));
        for cl in items.get(2..).unwrap_or(&[]) {
            if let DatumKind::List { items: ci, .. } = &cl.kind {
                if matches_ci(ci.first(), ":no-error") {
                    self.lower_seq(ci.get(2..).unwrap_or(&[]));
                    continue;
                }
                // (type (var) body…) — body is items[2..]; the (var) list at [1]
                // is a binding, not code.
                let body = self.collect(|b| b.lower_seq(ci.get(2..).unwrap_or(&[])));
                self.emit(Node::Catch { body });
            }
        }
    }

    // ---- logical ----------------------------------------------------------

    fn lower_logical(&mut self, op: LogicalOp, args: &[Datum]) {
        let mut operands = Vec::new();
        for a in args {
            self.collect_logical(op, a, &mut operands);
        }
        if operands.len() >= 2 {
            self.emit(Node::Logical { op, operands });
        } else {
            for n in operands {
                self.emit(n);
            }
        }
    }

    fn collect_logical(&mut self, op: LogicalOp, arg: &Datum, operands: &mut Vec<Node>) {
        if let DatumKind::List { items, .. } = &arg.kind
            && let Some(arg_op) = logical_op(head_symbol(items))
        {
            if arg_op == op {
                for a in &items[1..] {
                    self.collect_logical(op, a, operands);
                }
            } else {
                let mut sub = Vec::new();
                for a in &items[1..] {
                    self.collect_logical(arg_op, a, &mut sub);
                }
                if sub.len() >= 2 {
                    operands.push(Node::Logical {
                        op: arg_op,
                        operands: sub,
                    });
                } else {
                    operands.extend(sub);
                }
            }
            return;
        }
        let nodes = self.collect(|b| b.lower_datum(arg));
        operands.push(Node::Group(nodes));
    }

    // ---- application ------------------------------------------------------

    fn lower_call(&mut self, items: &[Datum], tail: Option<&Datum>) {
        self.emit(Node::Call {
            callee: head_symbol(items).map(str::to_ascii_lowercase),
        });
        if let Some(op) = items.first()
            && as_symbol(op).is_none()
        {
            self.lower_datum(op);
        }
        for a in &items[1..] {
            self.lower_datum(a);
        }
        if let Some(t) = tail {
            self.lower_datum(t);
        }
    }

    /// Lower an optional datum (a missing element contributes nothing).
    fn lower_opt(&mut self, d: Option<&Datum>) {
        if let Some(d) = d {
            self.lower_datum(d);
        }
    }
}

/// The symbol text of a datum, if it is a symbol.
fn as_symbol<'a>(d: &Datum<'a>) -> Option<&'a str> {
    match d.kind {
        DatumKind::Symbol(s) => Some(s),
        _ => None,
    }
}

/// The head (operator) symbol of a list's elements.
fn head_symbol<'a>(items: &[Datum<'a>]) -> Option<&'a str> {
    items.first().and_then(as_symbol)
}

/// True if a datum is the symbol `s` (case-insensitively).
fn matches_ci(d: Option<&Datum>, s: &str) -> bool {
    d.and_then(as_symbol)
        .is_some_and(|t| t.eq_ignore_ascii_case(s))
}

/// A `cond` clause / `case` clause catch-all: the constant-true symbol `t`.
fn is_else(d: Option<&Datum>) -> bool {
    matches_ci(d, "t")
}

/// A `case` clause default marker.
fn is_otherwise(d: Option<&Datum>) -> bool {
    matches_ci(d, "otherwise")
}

/// The normalized logical operator named by a list head (case-insensitive).
fn logical_op(head: Option<&str>) -> Option<LogicalOp> {
    match head.map(str::to_ascii_lowercase).as_deref() {
        Some("and") => Some(LogicalOp::And),
        Some("or") => Some(LogicalOp::Or),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.lisp"), src)
    }

    fn find<'a>(fns: &'a [FunctionReport], name: &str) -> Option<&'a FunctionReport> {
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
    fn if_and_recursion() {
        let src = r#"
            (defun fact (n)
              (if (< n 2) 1 (* n (fact (- n 1)))))
        "#;
        // if(+1) + recursive call(+1) = 2
        assert_eq!(cognitive_of(src, "fact"), 2);
        assert_eq!(cyclomatic_of(src, "fact"), 2);
        assert_eq!(find(&analyze(src).functions, "fact").unwrap().kind, "defun");
    }

    #[test]
    fn cond_is_a_flat_branch_chain() {
        let src = r#"
            (defun classify (n)
              (cond ((< n 0) 'neg)
                    ((= n 0) 'zero)
                    (t 'pos)))
        "#;
        // first(+1) + second(+1 flat) + t-else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "classify"), 3);
        assert_eq!(cyclomatic_of(src, "classify"), 3);
    }

    #[test]
    fn case_scores_like_a_switch() {
        let src = r#"
            (defun name (n)
              (case n
                (1 "one")
                ((2 3) "few")
                (otherwise "many")))
        "#;
        assert_eq!(cognitive_of(src, "name"), 1);
        // base 1 + 2 non-default clauses = 3
        assert_eq!(cyclomatic_of(src, "name"), 3);
    }

    #[test]
    fn when_and_unless_are_branches() {
        let src = r#"
            (defun f (x)
              (when x (foo))
              (unless x (bar)))
        "#;
        assert_eq!(cognitive_of(src, "f"), 2);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn loops_count_and_nest() {
        let src = r#"
            (defun f (xs)
              (dolist (x xs)
                (when (pred x) (process x))))
        "#;
        // dolist(+1) + nested when(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn do_loop_counts() {
        let src = r#"
            (defun sum (n)
              (do ((i 0 (+ i 1))
                   (acc 0 (+ acc i)))
                  ((= i n) acc)))
        "#;
        assert_eq!(cognitive_of(src, "sum"), 1);
        assert_eq!(cyclomatic_of(src, "sum"), 2);
    }

    #[test]
    fn and_or_fold_and_nest() {
        let src = r#"
            (defun f (a b c d)
              (if (or (and a b) (and c d)) 1 0))
        "#;
        // if(+1) + or(+1) + and(+1) + and(+1) = 4
        assert_eq!(cognitive_of(src, "f"), 4);
        // base 1 + if + or + and + and = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn handler_case_is_a_catch() {
        let src = r#"
            (defun safe (thunk)
              (handler-case (funcall thunk)
                (error (e)
                  (if (recoverable-p e) (retry) (abort)))))
        "#;
        // catch(+1) + handler if at nesting 1(+2) = 3
        assert_eq!(cognitive_of(src, "safe"), 3);
        assert_eq!(cyclomatic_of(src, "safe"), 3);
    }

    #[test]
    fn lambda_is_its_own_unit() {
        let src = r#"
            (defun host ()
              (mapcar (lambda (x) (if x 1 0)) items))
        "#;
        // host owns no structural complexity; the lambda does.
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "<lambda>"), 1);
        assert_eq!(
            find(&analyze(src).functions, "<lambda>").unwrap().kind,
            "lambda"
        );
    }

    #[test]
    fn flet_locals_are_their_own_units() {
        let src = r#"
            (defun host (xs)
              (flet ((helper (x) (if x 1 0)))
                (mapcar #'helper xs)))
        "#;
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "helper"), 1);
        assert_eq!(
            find(&analyze(src).functions, "helper").unwrap().kind,
            "flet"
        );
    }

    #[test]
    fn defmethod_with_qualifier() {
        let src = r#"
            (defmethod handle :before ((obj account) amount)
              (when (minusp amount) (error "no")))
        "#;
        // when(+1) = 1
        assert_eq!(cognitive_of(src, "handle"), 1);
        assert_eq!(
            find(&analyze(src).functions, "handle").unwrap().kind,
            "defmethod"
        );
    }

    #[test]
    fn go_is_a_labelled_jump() {
        let src = r#"
            (defun f ()
              (tagbody
               top
                 (when (test) (go top))))
        "#;
        // when(+1) + go labelled jump(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
    }

    #[test]
    fn reader_is_case_insensitive() {
        let src = r#"
            (DEFUN F (X) (IF X 1 2))
        "#;
        assert_eq!(cognitive_of(src, "F"), 1);
    }

    #[test]
    fn quoted_data_is_not_code() {
        let src = r#"
            (defun f ()
              (list '(if a b c) '(cond (x y))))
        "#;
        assert_eq!(cognitive_of(src, "f"), 0);
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
            (defun a (x) (if x 1 2))
            (defun b (y) (if y 3 4))
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn parse_error_is_reported() {
        let (_nodes, errors) = to_ir(Path::new("bad.lisp"), "(defun f (x");
        assert!(!errors.is_empty());
    }
}
