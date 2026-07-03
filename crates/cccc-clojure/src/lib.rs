//! Clojure adapter: reads source with [lispexp](https://docs.rs/lispexp) and
//! lowers the S-expression datum tree into the language-agnostic
//! [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core` and the pure-Rust
//! `lispexp` reader (no C toolchain, so cross-compilation stays clean), with no
//! CLI machinery. The unified `cccc` binary registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.clj`/`.cljs`/`.cljc`
//! files to it.
//!
//! This crate contains **no scoring logic** — it recognizes the Clojure special
//! forms/macros the engine cares about and emits the matching IR nodes; every
//! rule lives in [`cccc_core::engine`].
//!
//! ## Lowering strategy
//!
//! The skeleton mirrors [`cccc-scheme`](https://docs.rs/cccc-scheme): a stack of
//! "collector" vectors builds the IR while [`lispexp::walk_regions`] (ADR-0026)
//! makes the code-vs-data judgment (skip quoted data, descend into the code
//! carried by the syntax-quote `` ` ``/`~`), so the adapter never re-derives
//! quote nesting rules. Each `Region::Code` list is dispatched on its head
//! symbol.
//!
//! Clojure uses three delimiter shapes — `()` lists, `[]` vectors, `{}` maps —
//! so the adapter inspects a list's [`lispexp::Delim`] where it matters (a
//! binding *vector*, a metadata *map*, an arity *list*).
//!
//! ## Clojure-to-IR mapping
//!
//! - `defn` / `defn-` / `defmacro` / `defmethod` / `fn`, and the local-function
//!   form `letfn` → [`Node::Function`] (each its own unit; multiple arities of
//!   one `defn` collapse into that one unit).
//! - `if` / `if-not` / `if-let` / `if-some` → [`Node::Conditional`] (a
//!   single-decision expression); `when` / `when-not` / `when-let` / … →
//!   [`Node::Branch`]; `cond` → a flat `Branch` chain over test/expr **pairs**
//!   (a `:else` test is the terminal else); `case` / `condp` → [`Node::Switch`].
//! - `loop` / `doseq` / `dotimes` / `for` / `while` → [`Node::Loop`].
//! - `and` / `or` → folded [`Node::Logical`].
//! - `try`'s `catch` clauses → [`Node::Catch`] (the body and `finally` score at
//!   the surrounding level).
//! - a plain application → [`Node::Call`] (recursion detection).
//! - `let`/`binding`/`->`/`->>`/`do`/… are transparent (a `let` binding vector
//!   lowers its init expressions); `def` lowers its value; `defprotocol`/
//!   `deftype`/… and quoted data are skipped.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use lispexp::{Datum, DatumKind, Delim, Options, Region, Walk, parse, walk_regions};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["clj", "cljs", "cljc"];

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
    let parsed = parse(source, &Options::clojure());
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
                && let DatumKind::List {
                    delim, items, tail, ..
                } = &dd.kind
            {
                self.lower_list(dd, *delim, items, tail.as_deref());
                return Walk::Skip;
            }
            if region.is_prunable() {
                return Walk::Skip;
            }
            Walk::Descend
        });
    }

    fn lower_list(&mut self, d: &Datum, delim: Delim, items: &[Datum], tail: Option<&Datum>) {
        if items.is_empty() {
            return;
        }
        // Only a `()` list is an application/special form. A `[]` vector or `{}`
        // map / `#{}` set in code position is a data-structure literal whose
        // elements are code; lower them without dispatching on a "head".
        if delim != Delim::Round {
            self.lower_seq(items);
            if let Some(t) = tail {
                self.lower_datum(t);
            }
            return;
        }
        match head_symbol(items) {
            // ---- definitions ----
            Some("defn") | Some("defn-") | Some("defmacro") => self.lower_defn(d, items, "defn"),
            Some("fn") | Some("fn*") => self.lower_fn(d, items),
            Some("defmethod") => self.lower_defmethod(d, items),
            Some("letfn") => self.lower_letfn(items),
            // ---- conditionals ----
            Some("if") | Some("if-not") | Some("if-let") | Some("if-some") => self.lower_if(items),
            Some("when") | Some("when-not") | Some("when-let") | Some("when-some")
            | Some("when-first") => self.lower_when(items),
            Some("cond") => self.lower_cond(&items[1..]),
            Some("case") | Some("condp") => self.lower_case(items),
            Some("and") => self.lower_logical(LogicalOp::And, &items[1..]),
            Some("or") => self.lower_logical(LogicalOp::Or, &items[1..]),
            // ---- loops ----
            Some("loop") | Some("doseq") | Some("dotimes") | Some("for") => {
                self.lower_binding_loop(items)
            }
            Some("while") => self.lower_while(items),
            // ---- exceptions ----
            Some("try") => self.lower_try(items),
            // ---- binding / grouping: transparent ----
            Some("let")
            | Some("when-let*")
            | Some("if-let*")
            | Some("binding")
            | Some("with-open")
            | Some("with-local-vars")
            | Some("with-redefs")
            | Some("with-bindings")
            | Some("dosync") => self.lower_let_like(items),
            Some("do") | Some("doto") | Some("comment") | Some("->") | Some("->>")
            | Some("as->") | Some("some->") | Some("some->>") | Some("cond->")
            | Some("cond->>") | Some("delay") | Some("future") | Some("locking") | Some("time")
            | Some("doall") | Some("dorun") | Some("vary-meta") => self.lower_seq(&items[1..]),
            // ---- value definitions: lower the value form ----
            Some("def") | Some("defonce") => {
                self.lower_seq(items.get(2..).unwrap_or(&[]));
            }
            // ---- data / declarations / compile-time: skip ----
            Some("quote") => {}
            Some("ns") | Some("defprotocol") | Some("definterface") | Some("defrecord")
            | Some("deftype") | Some("defmulti") | Some("declare") | Some("import")
            | Some("require") | Some("use") | Some("gen-class") => {}
            // A plain application.
            _ => self.lower_call(items, tail),
        }
    }

    // ---- functions --------------------------------------------------------

    /// `(defn name doc? attr-map? [args] body…)` or the multi-arity
    /// `(defn name doc? ([args] body…)…)`.
    fn lower_defn(&mut self, d: &Datum, items: &[Datum], kind: &'static str) {
        let name = items
            .get(1)
            .and_then(as_symbol)
            .unwrap_or("<defn>")
            .to_string();
        let rest = skip_doc_and_meta(items.get(2..).unwrap_or(&[]));
        self.emit_arities(name, kind, d.line, rest);
    }

    /// `(fn name? [args] body…)` or `(fn name? ([args] body…)…)`.
    fn lower_fn(&mut self, d: &Datum, items: &[Datum]) {
        let (name, rest) = match items.get(1) {
            Some(x) if as_symbol(x).is_some() => (
                as_symbol(x).unwrap().to_string(),
                items.get(2..).unwrap_or(&[]),
            ),
            _ => ("<fn>".to_string(), items.get(1..).unwrap_or(&[])),
        };
        self.emit_arities(name, "fn", d.line, rest);
    }

    /// Emit one `Function` whose body is the body of every arity clause. `rest`
    /// starts at the arglist vector (single arity) or the first `([args] …)`
    /// arity list (multi-arity).
    fn emit_arities(&mut self, name: String, kind: &'static str, line: u32, rest: &[Datum]) {
        let rest = rest.to_vec();
        self.emit_function(name, kind, line, |b| match rest.first().map(|d| &d.kind) {
            // Single arity: rest[0] is the `[args]` vector, rest[1..] the body.
            Some(DatumKind::List {
                delim: Delim::Square,
                ..
            }) => {
                b.lower_seq(rest.get(1..).unwrap_or(&[]));
            }
            // Multi-arity: each `([args] body…)` list; lower its body (skip the
            // leading arglist vector).
            _ => {
                for clause in &rest {
                    if let DatumKind::List {
                        delim: Delim::Round,
                        items: ci,
                        ..
                    } = &clause.kind
                    {
                        b.lower_seq(ci.get(1..).unwrap_or(&[]));
                    }
                }
            }
        });
    }

    /// `(defmethod multifn dispatch-val [args] body…)` — the body follows the
    /// first `[args]` vector.
    fn lower_defmethod(&mut self, d: &Datum, items: &[Datum]) {
        let name = items
            .get(1)
            .and_then(as_symbol)
            .unwrap_or("<defmethod>")
            .to_string();
        let arglist_pos = items.iter().enumerate().skip(2).find(|(_, it)| {
            matches!(
                it.kind,
                DatumKind::List {
                    delim: Delim::Square,
                    ..
                }
            )
        });
        let body = match arglist_pos {
            Some((i, _)) => items.get(i + 1..).unwrap_or(&[]).to_vec(),
            None => Vec::new(),
        };
        // The dispatch-val (items[2..arglist]) is an ordinary expression too.
        let dispatch = items.get(2..).unwrap_or(&[]).to_vec();
        let dispatch_end = arglist_pos.map(|(i, _)| i - 2).unwrap_or(0);
        self.emit_function(name, "defmethod", d.line, |b| {
            b.lower_seq(dispatch.get(..dispatch_end).unwrap_or(&[]));
            b.lower_seq(&body);
        });
    }

    /// `(letfn [(name [args] body…)…] body…)` — each binding is its own unit;
    /// the `letfn` body scores at the surrounding level.
    fn lower_letfn(&mut self, items: &[Datum]) {
        if let Some(DatumKind::List { items: binds, .. }) = items.get(1).map(|d| &d.kind) {
            for b in binds {
                if let DatumKind::List {
                    delim: Delim::Round,
                    items: bi,
                    ..
                } = &b.kind
                    && let Some(name) = bi.first().and_then(as_symbol)
                {
                    let name = name.to_string();
                    // (name [args] body…) — body follows the arglist vector.
                    let body = bi.get(2..).unwrap_or(&[]).to_vec();
                    self.emit_function(name, "letfn", b.line, |bl| bl.lower_seq(&body));
                }
            }
        }
        self.lower_seq(items.get(2..).unwrap_or(&[]));
    }

    // ---- binding forms ----------------------------------------------------

    /// `let`/`binding`/… : transparent; lower the init expressions of the
    /// binding vector + the body.
    fn lower_let_like(&mut self, items: &[Datum]) {
        self.lower_binding_vector(items.get(1));
        self.lower_seq(items.get(2..).unwrap_or(&[]));
    }

    /// A Clojure binding vector is flat `[name init name init …]`; lower every
    /// value (odd-indexed) element. Modifier keywords (`:when`/`:while`/`:let`
    /// in `for`/`doseq`) and their following forms are lowered too, which is
    /// harmless — they carry no branch of their own here.
    fn lower_binding_vector(&mut self, bindings: Option<&Datum>) {
        if let Some(DatumKind::List { items: binds, .. }) = bindings.map(|d| &d.kind) {
            let mut i = 1;
            while i < binds.len() {
                self.lower_datum(&binds[i]);
                i += 2;
            }
        }
    }

    // ---- branches ---------------------------------------------------------

    /// `(if test then else)` — a single-decision expression → `Conditional`.
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

    /// Clojure `cond` is flat `test expr test expr … :else default`. Lower the
    /// test/expr pairs into a `Branch` chain; a `:else` (or `:default`) test is
    /// the terminal else.
    fn lower_cond(&mut self, rest: &[Datum]) {
        if let Some(node) = self.lower_cond_pairs(rest) {
            self.emit(*node);
        }
    }

    fn lower_cond_pairs(&mut self, rest: &[Datum]) -> Option<Box<Node>> {
        let test = rest.first()?;
        let expr = rest.get(1);
        if is_else(test) {
            let body = self.collect(|b| b.lower_opt(expr));
            return Some(Box::new(Node::Group(body)));
        }
        let test_nodes = self.collect(|b| b.lower_datum(test));
        let then = self.collect(|b| b.lower_opt(expr));
        let alternate = self.lower_cond_pairs(rest.get(2..).unwrap_or(&[]));
        Some(Box::new(Node::Branch {
            test: test_nodes,
            then,
            alternate,
        }))
    }

    /// Clojure `case`/`condp`: `(case e const result const result … default?)`
    /// — the constants are data, so only the keyform, each result, and a
    /// trailing default are lowered.
    fn lower_case(&mut self, items: &[Datum]) {
        // For `condp` the "predicate" (items[1]) and expr (items[2]) both run
        // at the switch level; for `case` only the keyform (items[1]) does.
        // Treat items[1..key_end] as pre-clause code, then const/result pairs.
        let (pre_end, clause_start) = if head_symbol(items) == Some("condp") {
            (3, 3)
        } else {
            (2, 2)
        };
        for d in items.get(1..pre_end).unwrap_or(&[]) {
            self.lower_datum(d);
        }
        let clauses = items.get(clause_start..).unwrap_or(&[]);
        let mut cases = Vec::new();
        let mut i = 0;
        while i < clauses.len() {
            if i + 1 < clauses.len() {
                // (const, result) — the const is data, the result is code.
                let body = self.collect(|b| b.lower_datum(&clauses[i + 1]));
                cases.push(SwitchCase {
                    is_default: false,
                    body,
                });
                i += 2;
            } else {
                // Odd trailing element: the default result.
                let body = self.collect(|b| b.lower_datum(&clauses[i]));
                cases.push(SwitchCase {
                    is_default: true,
                    body,
                });
                i += 1;
            }
        }
        self.emit(Node::Switch { cases });
    }

    // ---- loops ------------------------------------------------------------

    /// `(loop [bindings] body…)` / `(doseq [bindings] body…)` /
    /// `(dotimes [i n] body…)` / `(for [bindings] body])`: the binding vector's
    /// init expressions run at the surrounding level; the body loops.
    fn lower_binding_loop(&mut self, items: &[Datum]) {
        self.lower_binding_vector(items.get(1));
        let body = self.collect(|b| b.lower_seq(items.get(2..).unwrap_or(&[])));
        self.emit(Node::Loop { body });
    }

    fn lower_while(&mut self, items: &[Datum]) {
        let body = self.collect(|b| {
            b.lower_opt(items.get(1));
            b.lower_seq(items.get(2..).unwrap_or(&[]));
        });
        self.emit(Node::Loop { body });
    }

    // ---- exceptions -------------------------------------------------------

    /// `(try body… (catch Type e handler…)… (finally cleanup…))` — the body and
    /// `finally` score at the surrounding level; each `catch` is a `Catch`.
    fn lower_try(&mut self, items: &[Datum]) {
        for it in items.get(1..).unwrap_or(&[]) {
            if let DatumKind::List {
                delim: Delim::Round,
                items: ci,
                ..
            } = &it.kind
            {
                match head_symbol(ci) {
                    Some("catch") => {
                        // (catch Type binding handler…) — handler is ci[3..].
                        let body = self.collect(|b| b.lower_seq(ci.get(3..).unwrap_or(&[])));
                        self.emit(Node::Catch { body });
                        continue;
                    }
                    Some("finally") => {
                        self.lower_seq(ci.get(1..).unwrap_or(&[]));
                        continue;
                    }
                    _ => {}
                }
            }
            self.lower_datum(it);
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
        if let DatumKind::List {
            delim: Delim::Round,
            items,
            ..
        } = &arg.kind
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

/// True if a `cond` test is a catch-all: the `:else` (or `:default`) keyword.
fn is_else(d: &Datum) -> bool {
    matches!(d.kind, DatumKind::Keyword(k) if k == ":else" || k == ":default")
}

/// Skip a leading docstring (string) and/or metadata map in a `defn` tail,
/// returning the slice starting at the arglist vector or the arity clauses.
fn skip_doc_and_meta<'a>(mut rest: &'a [Datum<'a>]) -> &'a [Datum<'a>] {
    if matches!(rest.first().map(|d| &d.kind), Some(DatumKind::Str(_))) {
        rest = &rest[1..];
    }
    if matches!(
        rest.first().map(|d| &d.kind),
        Some(DatumKind::List {
            delim: Delim::Curly,
            ..
        })
    ) {
        rest = &rest[1..];
    }
    rest
}

/// The normalized logical operator named by a list head.
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
        analyze_source(Path::new("test.clj"), src)
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
    fn defn_if_and_recursion() {
        let src = r#"
            (defn fact [n]
              (if (< n 2) 1 (* n (fact (- n 1)))))
        "#;
        // if(+1) + recursive call(+1) = 2
        assert_eq!(cognitive_of(src, "fact"), 2);
        assert_eq!(cyclomatic_of(src, "fact"), 2);
        assert_eq!(find(&analyze(src).functions, "fact").unwrap().kind, "defn");
    }

    #[test]
    fn defn_docstring_and_metadata_are_skipped() {
        let src = r#"
            (defn fact
              "computes n!"
              {:added "1.0"}
              [n]
              (if (< n 2) 1 (* n (fact (- n 1)))))
        "#;
        assert_eq!(cognitive_of(src, "fact"), 2);
    }

    #[test]
    fn multi_arity_defn_is_one_unit() {
        let src = r#"
            (defn greet
              ([] (greet "world"))
              ([name] (if name (str "hi " name) "hi")))
        "#;
        // Two arities collapse to one unit: the `if` in the 2-arity body(+1)
        // plus the recursive `greet` call in the 0-arity body(+1) = 2.
        assert_eq!(cognitive_of(src, "greet"), 2);
    }

    #[test]
    fn clojure_cond_is_flat_pairs() {
        let src = r#"
            (defn classify [n]
              (cond
                (< n 0) :neg
                (= n 0) :zero
                :else :pos))
        "#;
        // first(+1) + second(+1 flat) + :else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "classify"), 3);
        // base 1 + 2 test clauses = 3
        assert_eq!(cyclomatic_of(src, "classify"), 3);
    }

    #[test]
    fn clojure_case_scores_like_a_switch() {
        let src = r#"
            (defn kind [n]
              (case n
                1 "one"
                2 "two"
                "many"))
        "#;
        assert_eq!(cognitive_of(src, "kind"), 1);
        // base 1 + 2 non-default clauses = 3 (the trailing default is not one)
        assert_eq!(cyclomatic_of(src, "kind"), 3);
    }

    #[test]
    fn when_is_a_branch() {
        let src = r#"
            (defn f [x]
              (when x (foo))
              (when-not x (bar)))
        "#;
        assert_eq!(cognitive_of(src, "f"), 2);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn loops_count_and_nest() {
        let src = r#"
            (defn f [xs]
              (doseq [x xs]
                (when (pred x) (process x))))
        "#;
        // doseq(+1) + nested when(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn and_or_fold_and_nest() {
        let src = r#"
            (defn f [a b c d]
              (if (or (and a b) (and c d)) 1 0))
        "#;
        assert_eq!(cognitive_of(src, "f"), 4);
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn try_catch_is_a_catch() {
        let src = r#"
            (defn safe [thunk]
              (try
                (thunk)
                (catch Exception e
                  (if (recoverable? e) (retry) (abort)))
                (finally (cleanup))))
        "#;
        // catch(+1) + handler if at nesting 1(+2) = 3
        assert_eq!(cognitive_of(src, "safe"), 3);
        assert_eq!(cyclomatic_of(src, "safe"), 3);
    }

    #[test]
    fn fn_is_its_own_unit() {
        let src = r#"
            (defn host [items]
              (map (fn [x] (if x 1 0)) items))
        "#;
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "<fn>"), 1);
    }

    #[test]
    fn letfn_locals_are_their_own_units() {
        let src = r#"
            (defn host [xs]
              (letfn [(helper [x] (if x 1 0))]
                (map helper xs)))
        "#;
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "helper"), 1);
        assert_eq!(
            find(&analyze(src).functions, "helper").unwrap().kind,
            "letfn"
        );
    }

    #[test]
    fn defmethod_is_a_unit() {
        let src = r#"
            (defmethod area :circle [shape]
              (if (valid? shape) (* 3.14 (:r shape)) 0))
        "#;
        assert_eq!(cognitive_of(src, "area"), 1);
        assert_eq!(
            find(&analyze(src).functions, "area").unwrap().kind,
            "defmethod"
        );
    }

    #[test]
    fn let_binding_vector_inits_are_lowered() {
        let src = r#"
            (defn f [xs]
              (let [n (if (seq xs) 1 0)]
                n))
        "#;
        // the `if` in the binding init counts
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn quoted_data_is_not_code() {
        let src = r#"
            (defn f []
              (list '(if a b c) '(cond x y)))
        "#;
        assert_eq!(cognitive_of(src, "f"), 0);
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
            (defn a [x] (if x 1 2))
            (defn b [y] (if y 3 4))
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn parse_error_is_reported() {
        let (_nodes, errors) = to_ir(Path::new("bad.clj"), "(defn f [x");
        assert!(!errors.is_empty());
    }
}
