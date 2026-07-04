//! Clojure lowering: reads source with [lispexp](https://docs.rs/lispexp) and
//! lowers the S-expression datum tree into the language-agnostic
//! [`cccc_core::ir`](https://docs.rs/cccc-core).
//!
//! The shared collector stack, `walk_regions`-driven traversal, and logical
//! folding live in the crate root ([`crate`]); this module supplies only the
//! Clojure reader preset ([`Options::clojure`]) and the special-form dispatch
//! ([`lower_list`]). The published [`cccc-clojure`](https://docs.rs/cccc-clojure)
//! crate is a thin façade that re-exports [`analyze_source`]/[`DEFAULT_EXTS`]
//! from here; the unified `cccc` binary dispatches `.clj`/`.cljs`/`.cljc` files
//! to it.
//!
//! ## Clojure-to-IR mapping
//!
//! - `defn` / `defn-` / `defmacro` / `defmethod` / `fn`, and the local-function
//!   form `letfn` → [`Node::Function`] (multiple arities of one `defn` collapse
//!   into that one unit).
//! - `if` / `if-not` / `if-let` / `if-some` → [`Node::Conditional`]; `when` /
//!   `when-not` / `when-let` / … → [`Node::Branch`]; `cond` → a flat `Branch`
//!   chain over test/expr **pairs** (a `:else` test is the terminal else);
//!   `case` / `condp` → [`Node::Switch`].
//! - `loop` / `doseq` / `dotimes` / `for` / `while` → [`Node::Loop`].
//! - `and` / `or` → folded [`Node::Logical`]; `try`'s `catch` clauses →
//!   [`Node::Catch`].
//! - a plain application → [`Node::Call`] (recursion detection).
//! - `let`/`binding`/`->`/`do`/… are transparent; `def` lowers its value;
//!   `defprotocol`/`deftype`/… and quoted data are skipped.
//!
//! Clojure uses three delimiter shapes — `()` lists, `[]` vectors, `{}` maps —
//! so [`lower_list`] inspects the [`Delim`] where it matters (a binding vector,
//! a metadata map, an arity list); only a `()` list is a special form.

use std::path::Path;

use crate::{
    Builder, Datum, DatumKind, Delim, FileReport, LogicalOp, Node, Options, SwitchCase, as_symbol,
    head_symbol,
};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["clj", "cljs", "cljc"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    crate::analyze(&Options::clojure(), lower_list, logical_op, path, source)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any reader diagnostics.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    crate::lower(&Options::clojure(), lower_list, logical_op, source)
}

/// The normalized logical operator named by a list head, if any.
fn logical_op(head: Option<&str>) -> Option<LogicalOp> {
    match head {
        Some("and") => Some(LogicalOp::And),
        Some("or") => Some(LogicalOp::Or),
        _ => None,
    }
}

fn lower_list(b: &mut Builder, d: &Datum, delim: Delim, items: &[Datum], tail: Option<&Datum>) {
    if items.is_empty() {
        return;
    }
    // Only a `()` list is an application/special form. A `[]` vector or `{}`
    // map / `#{}` set in code position is a data-structure literal whose
    // elements are code; lower them without dispatching on a "head".
    if delim != Delim::Round {
        b.lower_seq(items);
        if let Some(t) = tail {
            b.lower_datum(t);
        }
        return;
    }
    match head_symbol(items) {
        // ---- definitions ----
        Some("defn") | Some("defn-") | Some("defmacro") => lower_defn(b, d, items, "defn"),
        Some("fn") | Some("fn*") => lower_fn(b, d, items),
        Some("defmethod") => lower_defmethod(b, d, items),
        Some("letfn") => lower_letfn(b, items),
        // ---- conditionals ----
        Some("if") | Some("if-not") | Some("if-let") | Some("if-some") => lower_if(b, items),
        Some("when") | Some("when-not") | Some("when-let") | Some("when-some")
        | Some("when-first") => lower_when(b, items),
        Some("cond") => lower_cond(b, &items[1..]),
        Some("case") | Some("condp") => lower_case(b, items),
        Some("and") => b.lower_logical(LogicalOp::And, &items[1..]),
        Some("or") => b.lower_logical(LogicalOp::Or, &items[1..]),
        // ---- loops ----
        Some("loop") | Some("doseq") | Some("dotimes") | Some("for") => {
            lower_binding_loop(b, items)
        }
        Some("while") => lower_while(b, items),
        // ---- exceptions ----
        Some("try") => lower_try(b, items),
        // ---- binding / grouping: transparent ----
        Some("let")
        | Some("when-let*")
        | Some("if-let*")
        | Some("binding")
        | Some("with-open")
        | Some("with-local-vars")
        | Some("with-redefs")
        | Some("with-bindings")
        | Some("dosync") => lower_let_like(b, items),
        Some("do") | Some("doto") | Some("comment") | Some("->") | Some("->>") | Some("as->")
        | Some("some->") | Some("some->>") | Some("cond->") | Some("cond->>") | Some("delay")
        | Some("future") | Some("locking") | Some("time") | Some("doall") | Some("dorun")
        | Some("vary-meta") => b.lower_seq(&items[1..]),
        // ---- value definitions: lower the value form ----
        Some("def") | Some("defonce") => {
            b.lower_seq(items.get(2..).unwrap_or(&[]));
        }
        // ---- data / declarations / compile-time: skip ----
        Some("quote") => {}
        Some("ns") | Some("defprotocol") | Some("definterface") | Some("defrecord")
        | Some("deftype") | Some("defmulti") | Some("declare") | Some("import")
        | Some("require") | Some("use") | Some("gen-class") => {}
        // A plain application.
        _ => lower_call(b, items, tail),
    }
}

// ---- functions --------------------------------------------------------

/// `(defn name doc? attr-map? [args] body…)` or the multi-arity
/// `(defn name doc? ([args] body…)…)`.
fn lower_defn(b: &mut Builder, d: &Datum, items: &[Datum], kind: &'static str) {
    let name = items
        .get(1)
        .and_then(as_symbol)
        .unwrap_or("<defn>")
        .to_string();
    let rest = skip_doc_and_meta(items.get(2..).unwrap_or(&[]));
    emit_arities(b, name, kind, d.line, rest);
}

/// `(fn name? [args] body…)` or `(fn name? ([args] body…)…)`.
fn lower_fn(b: &mut Builder, d: &Datum, items: &[Datum]) {
    let (name, rest) = match items.get(1) {
        Some(x) if as_symbol(x).is_some() => (
            as_symbol(x).unwrap().to_string(),
            items.get(2..).unwrap_or(&[]),
        ),
        _ => ("<fn>".to_string(), items.get(1..).unwrap_or(&[])),
    };
    emit_arities(b, name, "fn", d.line, rest);
}

/// Emit one `Function` whose body is the body of every arity clause.
fn emit_arities(b: &mut Builder, name: String, kind: &'static str, line: u32, rest: &[Datum]) {
    let rest = rest.to_vec();
    b.emit_function(name, kind, line, |b| match rest.first().map(|d| &d.kind) {
        // Single arity: rest[0] is the `[args]` vector, rest[1..] the body.
        Some(DatumKind::List {
            delim: Delim::Square,
            ..
        }) => {
            b.lower_seq(rest.get(1..).unwrap_or(&[]));
        }
        // Multi-arity: each `([args] body…)` list; lower its body.
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

/// `(defmethod multifn dispatch-val [args] body…)`.
fn lower_defmethod(b: &mut Builder, d: &Datum, items: &[Datum]) {
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
    let dispatch = items.get(2..).unwrap_or(&[]).to_vec();
    let dispatch_end = arglist_pos.map(|(i, _)| i - 2).unwrap_or(0);
    b.emit_function(name, "defmethod", d.line, |b| {
        b.lower_seq(dispatch.get(..dispatch_end).unwrap_or(&[]));
        b.lower_seq(&body);
    });
}

/// `(letfn [(name [args] body…)…] body…)`.
fn lower_letfn(b: &mut Builder, items: &[Datum]) {
    if let Some(DatumKind::List { items: binds, .. }) = items.get(1).map(|d| &d.kind) {
        for bd in binds {
            if let DatumKind::List {
                delim: Delim::Round,
                items: bi,
                ..
            } = &bd.kind
                && let Some(name) = bi.first().and_then(as_symbol)
            {
                let name = name.to_string();
                let body = bi.get(2..).unwrap_or(&[]).to_vec();
                b.emit_function(name, "letfn", bd.line, |b| b.lower_seq(&body));
            }
        }
    }
    b.lower_seq(items.get(2..).unwrap_or(&[]));
}

// ---- binding forms ----------------------------------------------------

fn lower_let_like(b: &mut Builder, items: &[Datum]) {
    lower_binding_vector(b, items.get(1));
    b.lower_seq(items.get(2..).unwrap_or(&[]));
}

/// A Clojure binding vector is flat `[name init name init …]`; lower every
/// value (odd-indexed) element.
fn lower_binding_vector(b: &mut Builder, bindings: Option<&Datum>) {
    if let Some(DatumKind::List { items: binds, .. }) = bindings.map(|d| &d.kind) {
        let mut i = 1;
        while i < binds.len() {
            b.lower_datum(&binds[i]);
            i += 2;
        }
    }
}

// ---- branches ---------------------------------------------------------

fn lower_if(b: &mut Builder, items: &[Datum]) {
    let test = b.collect(|b| b.lower_opt(items.get(1)));
    let then = b.collect(|b| b.lower_opt(items.get(2)));
    let alternate = b.collect(|b| b.lower_opt(items.get(3)));
    b.emit(Node::Conditional {
        test,
        then,
        alternate,
    });
}

fn lower_when(b: &mut Builder, items: &[Datum]) {
    let test = b.collect(|b| b.lower_opt(items.get(1)));
    let then = b.collect(|b| b.lower_seq(items.get(2..).unwrap_or(&[])));
    b.emit(Node::Branch {
        test,
        then,
        alternate: None,
    });
}

/// Clojure `cond` is flat `test expr test expr … :else default`.
fn lower_cond(b: &mut Builder, rest: &[Datum]) {
    if let Some(node) = lower_cond_pairs(b, rest) {
        b.emit(*node);
    }
}

fn lower_cond_pairs(b: &mut Builder, rest: &[Datum]) -> Option<Box<Node>> {
    let test = rest.first()?;
    let expr = rest.get(1);
    if is_else(test) {
        let body = b.collect(|b| b.lower_opt(expr));
        return Some(Box::new(Node::Group(body)));
    }
    let test_nodes = b.collect(|b| b.lower_datum(test));
    let then = b.collect(|b| b.lower_opt(expr));
    let alternate = lower_cond_pairs(b, rest.get(2..).unwrap_or(&[]));
    Some(Box::new(Node::Branch {
        test: test_nodes,
        then,
        alternate,
    }))
}

/// Clojure `case`/`condp`: `(case e const result … default?)`.
fn lower_case(b: &mut Builder, items: &[Datum]) {
    // For `condp` the predicate + expr both run at the switch level; for `case`
    // only the keyform does.
    let (pre_end, clause_start) = if head_symbol(items) == Some("condp") {
        (3, 3)
    } else {
        (2, 2)
    };
    for d in items.get(1..pre_end).unwrap_or(&[]) {
        b.lower_datum(d);
    }
    let clauses = items.get(clause_start..).unwrap_or(&[]);
    let mut cases = Vec::new();
    let mut i = 0;
    while i < clauses.len() {
        if i + 1 < clauses.len() {
            // (const, result) — the const is data, the result is code.
            let body = b.collect(|b| b.lower_datum(&clauses[i + 1]));
            cases.push(SwitchCase {
                is_default: false,
                body,
            });
            i += 2;
        } else {
            // Odd trailing element: the default result.
            let body = b.collect(|b| b.lower_datum(&clauses[i]));
            cases.push(SwitchCase {
                is_default: true,
                body,
            });
            i += 1;
        }
    }
    b.emit(Node::Switch { cases });
}

// ---- loops ------------------------------------------------------------

fn lower_binding_loop(b: &mut Builder, items: &[Datum]) {
    lower_binding_vector(b, items.get(1));
    let body = b.collect(|b| b.lower_seq(items.get(2..).unwrap_or(&[])));
    b.emit(Node::Loop { body });
}

fn lower_while(b: &mut Builder, items: &[Datum]) {
    let body = b.collect(|b| {
        b.lower_opt(items.get(1));
        b.lower_seq(items.get(2..).unwrap_or(&[]));
    });
    b.emit(Node::Loop { body });
}

// ---- exceptions -------------------------------------------------------

/// `(try body… (catch Type e handler…)… (finally cleanup…))`.
fn lower_try(b: &mut Builder, items: &[Datum]) {
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
                    let body = b.collect(|b| b.lower_seq(ci.get(3..).unwrap_or(&[])));
                    b.emit(Node::Catch { body });
                    continue;
                }
                Some("finally") => {
                    b.lower_seq(ci.get(1..).unwrap_or(&[]));
                    continue;
                }
                _ => {}
            }
        }
        b.lower_datum(it);
    }
}

// ---- application ------------------------------------------------------

fn lower_call(b: &mut Builder, items: &[Datum], tail: Option<&Datum>) {
    b.emit(Node::Call {
        callee: head_symbol(items).map(str::to_string),
    });
    if let Some(op) = items.first()
        && as_symbol(op).is_none()
    {
        b.lower_datum(op);
    }
    for a in &items[1..] {
        b.lower_datum(a);
    }
    if let Some(t) = tail {
        b.lower_datum(t);
    }
}

/// True if a `cond` test is a catch-all: the `:else` (or `:default`) keyword.
fn is_else(d: &Datum) -> bool {
    matches!(d.kind, DatumKind::Keyword(k) if k == ":else" || k == ":default")
}

/// Skip a leading docstring (string) and/or metadata map in a `defn` tail.
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::FunctionReport;

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
