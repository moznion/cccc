//! Scheme (R7RS-small) adapter: reads source with
//! [lispexp](https://docs.rs/lispexp) and lowers the S-expression datum tree into
//! the language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core` and the pure-Rust
//! `lispexp` reader (no C toolchain, so cross-compilation stays clean), with no CLI
//! machinery. The unified `cccc` binary registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.scm`/`.ss`/`.sld` files
//! to it.
//!
//! This crate contains **no scoring logic** — it recognizes the R7RS special
//! forms the engine cares about and emits the matching IR nodes; every rule lives
//! in [`cccc_core::engine`].
//!
//! ## Lowering strategy
//!
//! `lispexp` produces a faithful, position-annotated datum tree. The
//! code-vs-data judgment — skip quoted *data* (`'(if x y)` is a literal list,
//! not an `if`) while still descending into the *code* under `unquote` — is
//! not reimplemented here: [`Builder::lower_datum`] delegates it to
//! [`lispexp::walk_regions`] (ADR-0026), lispexp's own pruning visitor, so
//! this adapter automatically tracks lispexp's considered ruling (including
//! cases beyond plain quote/quasiquote, like a `HashLiteral`'s contents)
//! instead of maintaining a parallel, potentially-diverging judgment. Its
//! three-way [`lispexp::Region`] (lispexp 0.4) also makes a subtlety that
//! bit an earlier version of this adapter impossible to get wrong by
//! construction — see `lower_datum`'s doc comment. We assemble the IR with a
//! stack of "collector" vectors (see [`Builder::collect`]) and dispatch each
//! `Region::Code` list on its head symbol.
//!
//! ## Scheme-to-IR mapping
//!
//! - `(define (f …) …)`, `(define f (lambda …))`, `lambda`, `case-lambda` →
//!   [`Node::Function`] (each its own unit; anonymous ones are `<lambda>` /
//!   `<case-lambda>`). A **named `let`** is idiomatic iteration → [`Node::Loop`].
//! - `if` → [`Node::Conditional`] (Scheme's `if` is a ternary expression, one
//!   decision); `when` / `unless` → [`Node::Branch`]; `cond` → a flat `Branch`
//!   chain (each clause after the first scores like `else if`); `case` →
//!   [`Node::Switch`].
//! - `do` and named `let` → [`Node::Loop`].
//! - `and` / `or` → folded [`Node::Logical`]; `guard` → [`Node::Catch`] (its
//!   clauses lowered as the handler's `cond`).
//! - a plain application `(f …)` → [`Node::Call`] (recursion is detected by the
//!   engine when the callee matches the enclosing `define`'s name).
//! - `quote`/`quasiquote` data is skipped; `begin`/`let`/`let*`/`parameterize`/…
//!   are transparent (their bodies score at the surrounding level); macro
//!   definitions (`define-syntax`, `syntax-rules`, …) and record definitions are
//!   skipped.
//!
//! ## Beyond R7RS-small: tolerating common Scheme-superset extensions
//!
//! Real `.scm` files are often not *pure* R7RS-small — [Gauche], one of the
//! most widely used implementations, extends the reader with forms like
//! `#[...]` (a char-set literal, e.g. `#[\(\[\{]`) and `#/regexp/` (a regexp
//! literal, e.g. `#/[\\\"]/`), whose payload contains raw delimiter bytes that
//! trip up a strict R7RS reader and can lose sync — and legitimate, unrelated
//! functions later in the same file — at the first one.
//!
//! We read every `.scm`/`.ss`/`.sld` file with [`Options::scheme_superset()`]
//! rather than the exact [`Options::scheme()`]. The superset is `lispexp`'s
//! own strict widening (ADR-0027 in the `lispexp` repository): R7RS-small
//! input reads identically either way, while Gauche/Mosh/Gambit's reader
//! extensions become opaque leaves instead of sync-losing errors. An audit
//! against a real Gauche checkout — recorded in the `lispexp` repository's
//! `docs/cccc/scheme-dialect-triage.md` — found this cascading failure,
//! motivated the fix, and confirmed the result: parse errors on that checkout
//! drop from 288 (40 files) to 3 (1 file, an unrelated `__DATA__`-after-
//! `(exit 0)` idiom no static reader can model).
//!
//! [Gauche]: https://practical-scheme.net/gauche/

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use lispexp::{Datum, DatumKind, Options, Region, Walk, parse, walk_regions};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["scm", "ss", "sld"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR use
/// [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any reader diagnostics. `lispexp` is fault-tolerant: it always yields
/// a (possibly partial) tree, so we lower whatever it recovered and surface the
/// diagnostics alongside.
///
/// Reads with [`Options::scheme_superset()`] rather than the exact-R7RS
/// [`Options::scheme()`]: real `.scm` files are frequently Gauche/Mosh/Gambit-
/// flavored rather than strict R7RS-small, and the superset is a strict
/// widening — R7RS-small input reads identically either way — that resyncs a
/// reader which would otherwise lose sync (and legitimate, unrelated
/// functions later in the file) on Gauche's `#[...]` char-set and
/// `#/regexp/` literals. See the `lispexp` repository's
/// `docs/cccc/scheme-dialect-triage.md` for the audit that motivated this,
/// and lispexp's own ADR-0027 for the reader-level fix.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let parsed = parse(source, &Options::scheme_superset());
    let mut builder = Builder::new();
    builder.lower_seq(&parsed.data);
    let errors = parsed.errors.iter().map(ToString::to_string).collect();
    (builder.finish(), errors)
}

/// Assembles the IR tree while we recurse the datum tree.
struct Builder {
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
}

impl Builder {
    fn new() -> Self {
        Self {
            stack: vec![Vec::new()], // module-level collector
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

    /// Lower each datum in `items` at the current level.
    fn lower_seq(&mut self, items: &[Datum]) {
        for d in items {
            self.lower_datum(d);
        }
    }

    /// Lower `d` if it sits in code position. Delegates the code-vs-data
    /// judgment entirely to [`lispexp::walk_regions`] (ADR-0026) rather than
    /// hand-rolling quote/quasiquote/unquote nesting rules: it finds every
    /// `Region::Code` list within `d` (skipping quoted/quasiquoted data,
    /// `HashLiteral`s, discards, … per lispexp's own ruling table, and
    /// re-entering code at `unquote`/`unquote-splicing`, however deep) and
    /// hands each one to [`Builder::lower_list`], which does its own targeted
    /// recursion into that special form's sub-expressions (also through
    /// `lower_datum`, so the same judgment applies at every nesting level).
    ///
    /// `Region` (lispexp 0.4) makes the one subtlety that matters here
    /// impossible to get wrong by construction: a plain `Class::Data` can't
    /// tell "safe to skip" (`Region::SealedData` — a hard `quote`, a
    /// `HashLiteral`, discarded content) apart from "data *here*, but a
    /// nested `unquote` can flip it back to code" (`Region::PorousData` — a
    /// quasiquote template). Only `Region::SealedData` is `is_prunable()`; a
    /// `Code` list we've handed to `lower_list` also returns `Walk::Skip`
    /// (it already did its own targeted recursion, so `walk_regions` must not
    /// *also* auto-descend into the same elements — that would double-count
    /// them); everything else — `PorousData`, or a `Code` non-list — returns
    /// `Walk::Descend`.
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
        // `()` is not an application; nothing to score.
        if items.is_empty() {
            return;
        }
        match head_symbol(items) {
            Some("define") => self.lower_define(d, items),
            Some("define-values") => self.lower_seq(items.get(2..).unwrap_or(&[])),
            Some("lambda") => self.emit_callable("<lambda>".to_string(), "lambda", items, d.line),
            Some("case-lambda") => {
                self.emit_callable("<case-lambda>".to_string(), "case-lambda", items, d.line)
            }
            Some("let") => self.lower_let(items),
            Some("let*") | Some("letrec") | Some("letrec*") | Some("let-values")
            | Some("let*-values") => self.lower_binding_body(items),
            Some("if") => self.lower_if(items),
            Some("when") | Some("unless") => self.lower_when(items),
            Some("cond") => {
                if let Some(node) = self.lower_cond_clauses(&items[1..]) {
                    self.emit(*node);
                }
            }
            Some("case") => self.lower_case(items),
            Some("and") => self.lower_logical(LogicalOp::And, &items[1..]),
            Some("or") => self.lower_logical(LogicalOp::Or, &items[1..]),
            Some("do") => self.lower_do(items),
            Some("guard") => self.lower_guard(items),
            Some("set!") => {
                if let Some(v) = items.get(2) {
                    self.lower_datum(v);
                }
            }
            // Transparent grouping forms: bodies score at the surrounding level.
            Some("begin") | Some("parameterize") | Some("dynamic-wind") | Some("delay")
            | Some("delay-force") | Some("fluid-let") => self.lower_seq(&items[1..]),
            // Pure data / compile-time only: nothing to measure. In practice
            // lispexp always folds the longhand `(quote x)`/`(quasiquote x)`
            // into a shorthand `Prefixed` datum for Scheme dialects (ADR-0002),
            // so `lower_datum`'s `walk`-based dispatch (see below) already
            // handles both; these two arms are the defensive fallback if a
            // future reader config ever leaves them unfolded. Treating an
            // unfolded `quasiquote` as inert too (rather than falling through
            // to `lower_call`, which would wrongly score its template as code)
            // keeps that fallback safe either way.
            Some("quote") | Some("quasiquote") => {}
            Some("define-syntax")
            | Some("define-syntax-rule")
            | Some("let-syntax")
            | Some("letrec-syntax")
            | Some("syntax-rules")
            | Some("define-record-type") => {}
            // A plain application.
            _ => self.lower_call(items, tail),
        }
    }

    // ---- functions --------------------------------------------------------

    fn lower_define(&mut self, d: &Datum, items: &[Datum]) {
        match items.get(1).map(|x| &x.kind) {
            // (define (name . args) body...)   — also curried (define ((f a) b) …)
            Some(DatumKind::List { items: sig, .. }) => {
                let name = leading_symbol(sig).unwrap_or("<define>").to_string();
                let body = items.get(2..).unwrap_or(&[]).to_vec();
                self.emit_function(name, "define", d.line, |b| b.lower_seq(&body));
            }
            // (define name value)
            Some(DatumKind::Symbol(name)) => {
                let name = name.to_string();
                if let Some(v) = items.get(2) {
                    if let DatumKind::List { items: vi, .. } = &v.kind
                        && matches!(head_symbol(vi), Some("lambda") | Some("case-lambda"))
                    {
                        self.emit_callable(name, "define", vi, v.line);
                        return;
                    }
                    // A non-procedure binding: its value is ordinary code.
                    self.lower_datum(v);
                }
            }
            _ => self.lower_seq(items.get(1..).unwrap_or(&[])),
        }
    }

    /// Emit a `Function` from a `lambda` / `case-lambda` list, under `name`.
    fn emit_callable(&mut self, name: String, kind: &'static str, items: &[Datum], line: u32) {
        match head_symbol(items) {
            Some("lambda") => {
                let body = items.get(2..).unwrap_or(&[]).to_vec();
                self.emit_function(name, kind, line, |b| b.lower_seq(&body));
            }
            Some("case-lambda") => {
                let clauses = items.get(1..).unwrap_or(&[]).to_vec();
                self.emit_function(name, kind, line, |b| {
                    for cl in &clauses {
                        if let DatumKind::List { items: ci, .. } = &cl.kind {
                            b.lower_seq(ci.get(1..).unwrap_or(&[]));
                        }
                    }
                });
            }
            _ => {}
        }
    }

    // ---- let / binding forms ---------------------------------------------

    fn lower_let(&mut self, items: &[Datum]) {
        match items.get(1).map(|x| &x.kind) {
            // Named let: idiomatic iteration → Loop.
            Some(DatumKind::Symbol(_)) => {
                self.lower_binding_inits(items.get(2));
                let body = items.get(3..).unwrap_or(&[]).to_vec();
                let loop_body = self.collect(|b| b.lower_seq(&body));
                self.emit(Node::Loop { body: loop_body });
            }
            // Plain let: transparent.
            _ => {
                self.lower_binding_inits(items.get(1));
                self.lower_seq(items.get(2..).unwrap_or(&[]));
            }
        }
    }

    /// `let*` / `letrec` / `let-values` …: transparent scoping.
    fn lower_binding_body(&mut self, items: &[Datum]) {
        self.lower_binding_inits(items.get(1));
        self.lower_seq(items.get(2..).unwrap_or(&[]));
    }

    /// Lower the initializer expressions of a `((var init) …)` binding list.
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

    /// Scheme's `if` is a conditional *expression* (`(if c a b)` is the ternary
    /// analog), so it scores as a single decision like `?:` — one increment, the
    /// `else` arm not a second one. Mapped to [`Node::Conditional`].
    fn lower_if(&mut self, items: &[Datum]) {
        let test = self.collect(|b| {
            if let Some(t) = items.get(1) {
                b.lower_datum(t);
            }
        });
        let then = self.collect(|b| {
            if let Some(t) = items.get(2) {
                b.lower_datum(t);
            }
        });
        let alternate = self.collect(|b| {
            if let Some(e) = items.get(3) {
                b.lower_datum(e);
            }
        });
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    fn lower_when(&mut self, items: &[Datum]) {
        let test = self.collect(|b| {
            if let Some(t) = items.get(1) {
                b.lower_datum(t);
            }
        });
        let then = self.collect(|b| b.lower_seq(items.get(2..).unwrap_or(&[])));
        self.emit(Node::Branch {
            test,
            then,
            alternate: None,
        });
    }

    /// Lower a `cond` clause list into a flat `Branch` chain (each clause after
    /// the first scores like `else if`; an `else` clause is the final `else`).
    fn lower_cond_clauses(&mut self, clauses: &[Datum]) -> Option<Box<Node>> {
        let (first, rest) = clauses.split_first()?;
        let DatumKind::List { items: ci, .. } = &first.kind else {
            return self.lower_cond_clauses(rest);
        };
        if head_symbol(ci) == Some("else") {
            let body = self.collect(|b| b.lower_cond_body(&ci[1..]));
            return Some(Box::new(Node::Group(body)));
        }
        let test = self.collect(|b| {
            if let Some(t) = ci.first() {
                b.lower_datum(t);
            }
        });
        let then = self.collect(|b| b.lower_cond_body(ci.get(1..).unwrap_or(&[])));
        let alternate = self.lower_cond_clauses(rest);
        Some(Box::new(Node::Branch {
            test,
            then,
            alternate,
        }))
    }

    /// A `cond`/`case` clause body: `expr …`, or `=> receiver` (lower the
    /// receiver, skip the `=>` marker).
    fn lower_cond_body(&mut self, rest: &[Datum]) {
        if head_symbol(rest) == Some("=>") {
            self.lower_seq(rest.get(1..).unwrap_or(&[]));
        } else {
            self.lower_seq(rest);
        }
    }

    fn lower_case(&mut self, items: &[Datum]) {
        // The key runs at the switch's own level, before the clauses.
        if let Some(k) = items.get(1) {
            self.lower_datum(k);
        }
        let mut cases = Vec::new();
        for cl in items.get(2..).unwrap_or(&[]) {
            if let DatumKind::List { items: ci, .. } = &cl.kind {
                let is_default = head_symbol(ci) == Some("else");
                // ci[0] is the datum list (literal data) or `else`; ci[1..] the body.
                let body = self.collect(|b| b.lower_cond_body(ci.get(1..).unwrap_or(&[])));
                cases.push(SwitchCase { is_default, body });
            }
        }
        self.emit(Node::Switch { cases });
    }

    // ---- loops ------------------------------------------------------------

    fn lower_do(&mut self, items: &[Datum]) {
        // (do ((var init step)...) (test result...) command...)
        // Inits run once at the surrounding level; steps/test/commands loop.
        self.lower_do_specs(items.get(1), /* init */ 1);
        let items_owned = items.to_vec();
        let body = self.collect(|b| {
            b.lower_do_specs(items_owned.get(1), /* step */ 2);
            if let Some(DatumKind::List { items: tr, .. }) = items_owned.get(2).map(|d| &d.kind) {
                b.lower_seq(tr);
            }
            b.lower_seq(items_owned.get(3..).unwrap_or(&[]));
        });
        self.emit(Node::Loop { body });
    }

    /// Lower the `index`-th element (init=1, step=2) of each `(var init step)`
    /// spec in a `do` variable list.
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

    fn lower_guard(&mut self, items: &[Datum]) {
        // (guard (var clause...) body...) — body runs at the surrounding level;
        // the clauses are the handler, a `cond` over the raised condition.
        self.lower_seq(items.get(2..).unwrap_or(&[]));
        if let Some(DatumKind::List { items: spec, .. }) = items.get(1).map(|d| &d.kind) {
            let body = self.collect(|b| {
                if let Some(node) = b.lower_cond_clauses(&spec[1..]) {
                    b.emit(*node);
                }
            });
            self.emit(Node::Catch { body });
        }
    }

    // ---- logical ----------------------------------------------------------

    fn lower_logical(&mut self, op: LogicalOp, args: &[Datum]) {
        let mut operands = Vec::new();
        for a in args {
            self.collect_logical(op, a, &mut operands);
        }
        // A 0- or 1-operand `and`/`or` is not a decision point: splice its
        // contents rather than emit a degenerate `Logical` (also avoids the
        // engine's `operands.len() - 1` underflowing).
        if operands.len() >= 2 {
            self.emit(Node::Logical { op, operands });
        } else {
            for n in operands {
                self.emit(n);
            }
        }
    }

    /// Flatten a run of like operators; a different operator nests as its own
    /// `Logical`; anything else becomes a `Group` of its sub-nodes.
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
            callee: head_symbol(items).map(str::to_string),
        });
        // If the operator is itself an expression (e.g. a `lambda` in operator
        // position), measure it too.
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

/// The leftmost symbol of a `define` signature, descending curried heads
/// (`(define ((f a) b) …)` → `f`).
fn leading_symbol<'a>(sig: &[Datum<'a>]) -> Option<&'a str> {
    match sig.first().map(|d| &d.kind) {
        Some(DatumKind::Symbol(s)) => Some(s),
        Some(DatumKind::List { items, .. }) => leading_symbol(items),
        _ => None,
    }
}

/// The normalized logical operator named by a list head, if any.
fn logical_op(head: Option<&str>) -> Option<LogicalOp> {
    match head {
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
        analyze_source(Path::new("test.scm"), src)
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
            (define (fact n)
              (if (< n 2)
                  1
                  (* n (fact (- n 1)))))
        "#;
        // if(+1) + recursive call to fact(+1) = 2
        assert_eq!(cognitive_of(src, "fact"), 2);
        // base 1 + if = 2
        assert_eq!(cyclomatic_of(src, "fact"), 2);
        assert_eq!(
            find(&analyze(src).functions, "fact").unwrap().kind,
            "define"
        );
    }

    #[test]
    fn cond_is_a_flat_branch_chain() {
        let src = r#"
            (define (classify n)
              (cond ((< n 0) 'neg)
                    ((= n 0) 'zero)
                    (else 'pos)))
        "#;
        // first clause(+1) + second clause(+1 flat) + else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "classify"), 3);
        // base 1 + 2 test clauses = 3 (else is not a decision point)
        assert_eq!(cyclomatic_of(src, "classify"), 3);
    }

    #[test]
    fn case_scores_like_a_switch() {
        let src = r#"
            (define (name n)
              (case n
                ((1) "one")
                ((2 3) "few")
                (else "many")))
        "#;
        assert_eq!(cognitive_of(src, "name"), 1);
        // base 1 + 2 non-default clauses = 3
        assert_eq!(cyclomatic_of(src, "name"), 3);
    }

    #[test]
    fn when_and_unless_are_branches() {
        let src = r#"
            (define (f x)
              (when x (display 1))
              (unless x (display 2)))
        "#;
        assert_eq!(cognitive_of(src, "f"), 2);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn and_or_fold_and_nest() {
        let src = r#"
            (define (f a b c d)
              (if (or (and a b) (and c d)) 1 0))
        "#;
        // if(+1) + or(+1) + and(+1) + and(+1) = 4
        assert_eq!(cognitive_of(src, "f"), 4);
        // base 1 + if 1 + or(+1) + and(+1) + and(+1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn single_operand_and_is_not_a_decision() {
        let src = r#"
            (define (f a) (and a))
        "#;
        assert_eq!(cognitive_of(src, "f"), 0);
        assert_eq!(cyclomatic_of(src, "f"), 1);
    }

    #[test]
    fn do_loop_counts() {
        let src = r#"
            (define (sum n)
              (do ((i 0 (+ i 1))
                   (acc 0 (+ acc i)))
                  ((= i n) acc)))
        "#;
        assert_eq!(cognitive_of(src, "sum"), 1);
        assert_eq!(cyclomatic_of(src, "sum"), 2);
    }

    #[test]
    fn named_let_is_a_loop_not_recursion() {
        let src = r#"
            (define (count n)
              (let loop ((i 0))
                (if (< i n)
                    (loop (+ i 1))
                    i)))
        "#;
        // named-let loop(+1) + nested if(+2) = 3 (the (loop …) call is iteration,
        // not self-recursion of `count`)
        assert_eq!(cognitive_of(src, "count"), 3);
        assert_eq!(cyclomatic_of(src, "count"), 3);
    }

    #[test]
    fn guard_is_a_catch() {
        let src = r#"
            (define (safe thunk)
              (guard (e ((error-object? e) 'err))
                (thunk)))
        "#;
        // catch(+1) + the handler clause branch at nesting 1(+2) = 3
        assert_eq!(cognitive_of(src, "safe"), 3);
        // base 1 + catch + one handler clause = 3
        assert_eq!(cyclomatic_of(src, "safe"), 3);
    }

    #[test]
    fn lambda_is_its_own_anonymous_unit() {
        let src = r#"
            (define (make)
              (lambda (x) (if x 1 0)))
        "#;
        assert_eq!(cognitive_of(src, "make"), 0);
        assert_eq!(cognitive_of(src, "<lambda>"), 1);
        assert_eq!(
            find(&analyze(src).functions, "<lambda>").unwrap().kind,
            "lambda"
        );
    }

    #[test]
    fn quoted_data_is_not_code() {
        let src = r#"
            (define (f)
              (list 'if 'cond '(a b c) `(x ,(g) y)))
        "#;
        // The quoted forms are data. Only the unquoted (g) is code — a plain
        // call with no decisions — so f has zero complexity.
        assert_eq!(cognitive_of(src, "f"), 0);
    }

    #[test]
    fn nested_quasiquote_needs_matching_unquote_depth_to_reach_code() {
        // A single unquote inside a *doubly*-nested quasiquote steps back only
        // one level — it's still data at the outer quasiquote's level, not
        // code — so the `if` here must not be scored (it's an inert template
        // fragment, never evaluated as a branch). This is the depth-tracked
        // rule `lispexp::walk` implements (ADR-0026); a naive "any unquote
        // means code" recursion (what this adapter used to hand-roll) gets it
        // wrong and would count it.
        let one_unquote = r#"
            (define (g)
              `(a `(b ,(if x 1 2))))
        "#;
        assert_eq!(cognitive_of(one_unquote, "g"), 0);

        // A *second*, stacked unquote (`,,`) does escape all the way to code.
        let two_unquotes = r#"
            (define (h)
              `(a `(b ,,(if x 1 2))))
        "#;
        assert_eq!(cognitive_of(two_unquotes, "h"), 1);
    }

    #[test]
    fn nested_define_is_its_own_unit_with_its_own_line() {
        let src = "(define (outer x)\n  (define (inner y) (if y 1 0))\n  (inner x))";
        assert_eq!(cognitive_of(src, "outer"), 0);
        assert_eq!(cognitive_of(src, "inner"), 1);
        let report = analyze(src);
        let inner = find(&report.functions, "inner").unwrap();
        assert_eq!(inner.line, 2);
    }

    #[test]
    fn define_with_lambda_value_borrows_the_name() {
        let src = r#"
            (define add
              (lambda (a b)
                (if (and a b) (+ a b) 0)))
        "#;
        // if(+1) + and(+1) = 2, reported under `add`
        assert_eq!(cognitive_of(src, "add"), 2);
        assert_eq!(find(&analyze(src).functions, "add").unwrap().kind, "define");
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
            (define (a x) (if x 1 2))
            (define (b y) (if y 3 4))
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn parse_error_is_reported() {
        // lispexp is fault-tolerant: it yields a partial tree and a diagnostic.
        let (_nodes, errors) = to_ir(Path::new("bad.scm"), "(define (f x");
        assert!(!errors.is_empty());
    }

    // ---- Gauche `#[...]` / `#/regexp/` tolerance (via `scheme_superset()`) ---
    //
    // These reproduce the exact shapes an audit against a real Gauche
    // checkout found breaking the plain R7RS-small reader (see the lispexp
    // repository's docs/cccc/scheme-dialect-triage.md) and confirm *this
    // adapter's* wiring — that `to_ir` actually reads with
    // `Options::scheme_superset()` and correctly lowers what it returns. The
    // reader's own lexical handling of these forms (string/comment
    // containment, `#\[` disambiguation, POSIX classes, unterminated
    // literals, …) is `lispexp`'s concern and covered by its own test suite,
    // not duplicated here.

    #[test]
    fn gauche_charset_literal_does_not_break_the_rest_of_the_file() {
        let src = r#"
            (define begin-list
              ($seq0 ($. #[\(\[\{]) ws))
            (define (after x) (if x 1 2))
        "#;
        // The charset-bearing `define` isn't itself scored (it's a plain
        // application chain, no branches), but the *following* define must
        // still be found and correctly scored — proof the reader resynced.
        assert_eq!(cognitive_of(src, "after"), 1);
    }

    #[test]
    fn gauche_regexp_literal_does_not_break_the_rest_of_the_file() {
        let src = r#"
            (define (escape line)
              (regexp-replace-all #/[\\\"]/ line "\\\\\\0"))
            (define (after x) (if x 1 2))
        "#;
        assert_eq!(cognitive_of(src, "after"), 1);
    }

    #[test]
    fn gauche_extensions_preserve_line_numbers() {
        let src = "(define (a) #[\\(\\[\\{])\n(define (b)\n  #/x+/\n  (if #t 1 2))\n";
        let report = analyze(src);
        let f = find(&report.functions, "b").expect("b found");
        assert_eq!(
            f.line, 2,
            "line of `b` must be unaffected by the superset reader"
        );
    }
}
