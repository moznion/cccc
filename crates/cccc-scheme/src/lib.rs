//! Scheme (R7RS-small) adapter: reads source with [lispexp](https://docs.rs/lispexp)
//! (via the shared [`cccc_lisp_kit`]) and lowers the S-expression datum tree
//! into the language-agnostic [`cccc_core::ir`](https://docs.rs/cccc-core).
//!
//! This is a pure library — it depends only on `cccc-lisp-kit` (which re-exports
//! `cccc-core`'s IR and the pure-Rust `lispexp` reader), no C toolchain, no CLI
//! machinery. The unified `cccc` binary registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.scm`/`.ss`/`.sld` files
//! to it.
//!
//! This crate contains **no scoring logic and no shared plumbing** — the
//! collector stack, the `walk_regions`-driven code-vs-data traversal, and the
//! logical folding all live in [`cccc_lisp_kit`]. This module supplies only the
//! Scheme reader preset ([`Options::scheme_superset`]) and the R7RS special-form
//! dispatch ([`lower_list`]).
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
//! - `and` / `or` → folded [`Node::Logical`]; `guard` → [`Node::Catch`].
//! - a plain application `(f …)` → [`Node::Call`] (recursion detection).
//! - `quote`/`quasiquote` data is skipped; `begin`/`let`/`let*`/… are
//!   transparent; macro and record definitions are skipped.
//!
//! ## Beyond R7RS-small: tolerating common Scheme-superset extensions
//!
//! Real `.scm` files are often not *pure* R7RS-small — [Gauche] extends the
//! reader with `#[...]` char-set and `#/regexp/` literals whose payload trips up
//! a strict R7RS reader. We read every file with [`Options::scheme_superset()`]
//! rather than the exact [`Options::scheme()`]: a strict widening (R7RS-small
//! reads identically either way) that keeps those extensions from losing sync.
//! See the `lispexp` repository's `docs/cccc/scheme-dialect-triage.md` for the
//! audit that motivated this, and lispexp's ADR-0027.
//!
//! [Gauche]: https://practical-scheme.net/gauche/

use std::path::Path;

use cccc_lisp_kit::{
    Builder, Datum, DatumKind, Delim, FileReport, LogicalOp, Node, Options, SwitchCase, as_symbol,
    head_symbol,
};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["scm", "ss", "sld"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    cccc_lisp_kit::analyze(
        &Options::scheme_superset(),
        lower_list,
        logical_op,
        path,
        source,
    )
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any reader diagnostics.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    cccc_lisp_kit::lower(&Options::scheme_superset(), lower_list, logical_op, source)
}

/// The normalized logical operator named by a list head, if any.
fn logical_op(head: Option<&str>) -> Option<LogicalOp> {
    match head {
        Some("and") => Some(LogicalOp::And),
        Some("or") => Some(LogicalOp::Or),
        _ => None,
    }
}

/// Dispatch a code-position list on its head symbol.
fn lower_list(b: &mut Builder, d: &Datum, _delim: Delim, items: &[Datum], tail: Option<&Datum>) {
    // `()` is not an application; nothing to score.
    if items.is_empty() {
        return;
    }
    match head_symbol(items) {
        Some("define") => lower_define(b, d, items),
        Some("define-values") => b.lower_seq(items.get(2..).unwrap_or(&[])),
        Some("lambda") => emit_callable(b, "<lambda>".to_string(), "lambda", items, d.line),
        Some("case-lambda") => {
            emit_callable(b, "<case-lambda>".to_string(), "case-lambda", items, d.line)
        }
        Some("let") => lower_let(b, items),
        Some("let*") | Some("letrec") | Some("letrec*") | Some("let-values")
        | Some("let*-values") => lower_binding_body(b, items),
        Some("if") => lower_if(b, items),
        Some("when") | Some("unless") => lower_when(b, items),
        Some("cond") => {
            if let Some(node) = lower_cond_clauses(b, &items[1..]) {
                b.emit(*node);
            }
        }
        Some("case") => lower_case(b, items),
        Some("and") => b.lower_logical(LogicalOp::And, &items[1..]),
        Some("or") => b.lower_logical(LogicalOp::Or, &items[1..]),
        Some("do") => lower_do(b, items),
        Some("guard") => lower_guard(b, items),
        Some("set!") => {
            if let Some(v) = items.get(2) {
                b.lower_datum(v);
            }
        }
        // Transparent grouping forms: bodies score at the surrounding level.
        Some("begin") | Some("parameterize") | Some("dynamic-wind") | Some("delay")
        | Some("delay-force") | Some("fluid-let") => b.lower_seq(&items[1..]),
        // Pure data / compile-time only: nothing to measure.
        Some("quote") | Some("quasiquote") => {}
        Some("define-syntax")
        | Some("define-syntax-rule")
        | Some("let-syntax")
        | Some("letrec-syntax")
        | Some("syntax-rules")
        | Some("define-record-type") => {}
        // A plain application.
        _ => lower_call(b, items, tail),
    }
}

// ---- functions --------------------------------------------------------

fn lower_define(b: &mut Builder, d: &Datum, items: &[Datum]) {
    match items.get(1).map(|x| &x.kind) {
        // (define (name . args) body...)   — also curried (define ((f a) b) …)
        Some(DatumKind::List { items: sig, .. }) => {
            let name = leading_symbol(sig).unwrap_or("<define>").to_string();
            let body = items.get(2..).unwrap_or(&[]).to_vec();
            b.emit_function(name, "define", d.line, |b| b.lower_seq(&body));
        }
        // (define name value)
        Some(DatumKind::Symbol(name)) => {
            let name = name.to_string();
            if let Some(v) = items.get(2) {
                if let DatumKind::List { items: vi, .. } = &v.kind
                    && matches!(head_symbol(vi), Some("lambda") | Some("case-lambda"))
                {
                    emit_callable(b, name, "define", vi, v.line);
                    return;
                }
                b.lower_datum(v);
            }
        }
        _ => b.lower_seq(items.get(1..).unwrap_or(&[])),
    }
}

/// Emit a `Function` from a `lambda` / `case-lambda` list, under `name`.
fn emit_callable(b: &mut Builder, name: String, kind: &'static str, items: &[Datum], line: u32) {
    match head_symbol(items) {
        Some("lambda") => {
            let body = items.get(2..).unwrap_or(&[]).to_vec();
            b.emit_function(name, kind, line, |b| b.lower_seq(&body));
        }
        Some("case-lambda") => {
            let clauses = items.get(1..).unwrap_or(&[]).to_vec();
            b.emit_function(name, kind, line, |b| {
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

fn lower_let(b: &mut Builder, items: &[Datum]) {
    match items.get(1).map(|x| &x.kind) {
        // Named let: idiomatic iteration → Loop.
        Some(DatumKind::Symbol(_)) => {
            lower_binding_inits(b, items.get(2));
            let body = items.get(3..).unwrap_or(&[]).to_vec();
            let loop_body = b.collect(|b| b.lower_seq(&body));
            b.emit(Node::Loop { body: loop_body });
        }
        // Plain let: transparent.
        _ => {
            lower_binding_inits(b, items.get(1));
            b.lower_seq(items.get(2..).unwrap_or(&[]));
        }
    }
}

/// `let*` / `letrec` / `let-values` …: transparent scoping.
fn lower_binding_body(b: &mut Builder, items: &[Datum]) {
    lower_binding_inits(b, items.get(1));
    b.lower_seq(items.get(2..).unwrap_or(&[]));
}

/// Lower the initializer expressions of a `((var init) …)` binding list.
fn lower_binding_inits(b: &mut Builder, bindings: Option<&Datum>) {
    if let Some(DatumKind::List { items: binds, .. }) = bindings.map(|d| &d.kind) {
        for bd in binds {
            if let DatumKind::List { items: kv, .. } = &bd.kind
                && let Some(init) = kv.get(1)
            {
                b.lower_datum(init);
            }
        }
    }
}

// ---- branches ---------------------------------------------------------

/// Scheme's `if` is a conditional *expression* → [`Node::Conditional`].
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

/// Lower a `cond` clause list into a flat `Branch` chain.
fn lower_cond_clauses(b: &mut Builder, clauses: &[Datum]) -> Option<Box<Node>> {
    let (first, rest) = clauses.split_first()?;
    let DatumKind::List { items: ci, .. } = &first.kind else {
        return lower_cond_clauses(b, rest);
    };
    if head_symbol(ci) == Some("else") {
        let body = b.collect(|b| lower_cond_body(b, &ci[1..]));
        return Some(Box::new(Node::Group(body)));
    }
    let test = b.collect(|b| {
        if let Some(t) = ci.first() {
            b.lower_datum(t);
        }
    });
    let then = b.collect(|b| lower_cond_body(b, ci.get(1..).unwrap_or(&[])));
    let alternate = lower_cond_clauses(b, rest);
    Some(Box::new(Node::Branch {
        test,
        then,
        alternate,
    }))
}

/// A `cond`/`case` clause body: `expr …`, or `=> receiver`.
fn lower_cond_body(b: &mut Builder, rest: &[Datum]) {
    if head_symbol(rest) == Some("=>") {
        b.lower_seq(rest.get(1..).unwrap_or(&[]));
    } else {
        b.lower_seq(rest);
    }
}

fn lower_case(b: &mut Builder, items: &[Datum]) {
    // The key runs at the switch's own level, before the clauses.
    if let Some(k) = items.get(1) {
        b.lower_datum(k);
    }
    let mut cases = Vec::new();
    for cl in items.get(2..).unwrap_or(&[]) {
        if let DatumKind::List { items: ci, .. } = &cl.kind {
            let is_default = head_symbol(ci) == Some("else");
            let body = b.collect(|b| lower_cond_body(b, ci.get(1..).unwrap_or(&[])));
            cases.push(SwitchCase { is_default, body });
        }
    }
    b.emit(Node::Switch { cases });
}

// ---- loops ------------------------------------------------------------

fn lower_do(b: &mut Builder, items: &[Datum]) {
    // (do ((var init step)...) (test result...) command...)
    lower_do_specs(b, items.get(1), /* init */ 1);
    let items_owned = items.to_vec();
    let body = b.collect(|b| {
        lower_do_specs(b, items_owned.get(1), /* step */ 2);
        if let Some(DatumKind::List { items: tr, .. }) = items_owned.get(2).map(|d| &d.kind) {
            b.lower_seq(tr);
        }
        b.lower_seq(items_owned.get(3..).unwrap_or(&[]));
    });
    b.emit(Node::Loop { body });
}

fn lower_do_specs(b: &mut Builder, specs: Option<&Datum>, index: usize) {
    if let Some(DatumKind::List { items: specs, .. }) = specs.map(|d| &d.kind) {
        for s in specs {
            if let DatumKind::List { items: kv, .. } = &s.kind
                && let Some(e) = kv.get(index)
            {
                b.lower_datum(e);
            }
        }
    }
}

// ---- exceptions -------------------------------------------------------

fn lower_guard(b: &mut Builder, items: &[Datum]) {
    // (guard (var clause...) body...) — body at surrounding level; clauses = cond.
    b.lower_seq(items.get(2..).unwrap_or(&[]));
    if let Some(DatumKind::List { items: spec, .. }) = items.get(1).map(|d| &d.kind) {
        let body = b.collect(|b| {
            if let Some(node) = lower_cond_clauses(b, &spec[1..]) {
                b.emit(*node);
            }
        });
        b.emit(Node::Catch { body });
    }
}

// ---- application ------------------------------------------------------

fn lower_call(b: &mut Builder, items: &[Datum], tail: Option<&Datum>) {
    b.emit(Node::Call {
        callee: head_symbol(items).map(str::to_string),
    });
    // If the operator is itself an expression (e.g. a `lambda` in op position).
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

/// The leftmost symbol of a `define` signature, descending curried heads
/// (`(define ((f a) b) …)` → `f`).
fn leading_symbol<'a>(sig: &[Datum<'a>]) -> Option<&'a str> {
    match sig.first().map(|d| &d.kind) {
        Some(DatumKind::Symbol(s)) => Some(s),
        Some(DatumKind::List { items, .. }) => leading_symbol(items),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use cccc_lisp_kit::FunctionReport;

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
