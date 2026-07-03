//! Common Lisp lowering. Case-**insensitive** reader (`DEFUN` == `defun`), so
//! head symbols are lower-cased before dispatch.
//!
//! Common-Lisp-to-IR mapping: `defun`/`defmacro`/`defmethod`/`lambda` and the
//! local-function forms `flet`/`labels` → `Function`; `if` → `Conditional`;
//! `when`/`unless` → `Branch`; `cond` → a flat `Branch` chain (a `t` clause is
//! the terminal `else`); `case`/`ecase`/`ccase`/`typecase` family → `Switch`
//! (`t`/`otherwise` = default); `loop`/`do`/`do*`/`dotimes`/`dolist` → `Loop`;
//! `and`/`or` → folded `Logical`; `handler-case` clauses → `Catch`; `go` (a
//! `tagbody` goto) → a labelled `Jump`; a plain application → `Call`.

use std::path::Path;

use cccc_lisp_kit::{
    Builder, Datum, DatumKind, Delim, FileReport, LogicalOp, Node, Options, SwitchCase, as_symbol,
    head_symbol,
};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["lisp", "lsp", "cl"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    cccc_lisp_kit::analyze(
        &Options::common_lisp(),
        lower_list,
        logical_op,
        path,
        source,
    )
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any reader diagnostics.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    cccc_lisp_kit::lower(&Options::common_lisp(), lower_list, logical_op, source)
}

/// The normalized logical operator named by a list head (case-insensitive).
fn logical_op(head: Option<&str>) -> Option<LogicalOp> {
    match head.map(str::to_ascii_lowercase).as_deref() {
        Some("and") => Some(LogicalOp::And),
        Some("or") => Some(LogicalOp::Or),
        _ => None,
    }
}

fn lower_list(b: &mut Builder, d: &Datum, _delim: Delim, items: &[Datum], tail: Option<&Datum>) {
    if items.is_empty() {
        return;
    }
    // The CL reader is case-insensitive, so match on the lower-cased head.
    let head = head_symbol(items).map(str::to_ascii_lowercase);
    match head.as_deref() {
        // ---- definitions ----
        Some("defun") | Some("defmacro") => lower_defun(b, d, items, "defun"),
        Some("defmethod") => lower_defmethod(b, d, items),
        Some("lambda") => emit_lambda(b, "<lambda>".to_string(), items, d.line),
        Some("flet") | Some("labels") | Some("macrolet") => lower_flet(b, items),
        // ---- conditionals ----
        Some("if") => lower_if(b, items),
        Some("when") | Some("unless") => lower_when(b, items),
        Some("cond") => {
            if let Some(node) = lower_cond_clauses(b, &items[1..]) {
                b.emit(*node);
            }
        }
        Some("case") | Some("ccase") | Some("ecase") | Some("typecase") | Some("etypecase")
        | Some("ctypecase") => lower_case(b, items),
        Some("and") => b.lower_logical(LogicalOp::And, &items[1..]),
        Some("or") => b.lower_logical(LogicalOp::Or, &items[1..]),
        // ---- loops ----
        Some("loop") => {
            let body = b.collect(|b| b.lower_seq(&items[1..]));
            b.emit(Node::Loop { body });
        }
        Some("dotimes") | Some("dolist") => lower_iter_loop(b, items),
        Some("do") | Some("do*") => lower_do(b, items),
        // ---- exceptions ----
        Some("handler-case") => lower_handler_case(b, items),
        // `go` transfers control to a `tagbody` tag — a genuine goto.
        Some("go") => b.emit(Node::Jump { labeled: true }),
        // ---- binding / grouping: transparent ----
        Some("let") | Some("let*") => lower_let(b, items),
        Some("multiple-value-bind") | Some("destructuring-bind") => {
            b.lower_seq(items.get(2..).unwrap_or(&[]));
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
        | Some("throw") => b.lower_seq(&items[1..]),
        // ---- value definitions: lower the init form ----
        Some("defvar") | Some("defparameter") | Some("defconstant") => {
            b.lower_seq(items.get(2..).unwrap_or(&[]));
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
        _ => lower_call(b, items, tail),
    }
}

// ---- functions --------------------------------------------------------

/// `(defun name (args) body…)` / `(defmacro name (args) body…)`.
fn lower_defun(b: &mut Builder, d: &Datum, items: &[Datum], kind: &'static str) {
    let name = items
        .get(1)
        .and_then(as_symbol)
        .unwrap_or("<defun>")
        .to_string();
    let body = items.get(3..).unwrap_or(&[]).to_vec();
    b.emit_function(name, kind, d.line, |b| b.lower_seq(&body));
}

/// `(defmethod name [qualifier…] (specialized-args) body…)`.
fn lower_defmethod(b: &mut Builder, d: &Datum, items: &[Datum]) {
    let name = items
        .get(1)
        .and_then(as_symbol)
        .unwrap_or("<defmethod>")
        .to_string();
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
    b.emit_function(name, "defmethod", d.line, |b| b.lower_seq(&body));
}

/// `(lambda (args) body…)`, under `name`.
fn emit_lambda(b: &mut Builder, name: String, items: &[Datum], line: u32) {
    let body = items.get(2..).unwrap_or(&[]).to_vec();
    b.emit_function(name, "lambda", line, |b| b.lower_seq(&body));
}

/// `(flet ((name (args) body…)…) body…)` — each local binding is its own unit;
/// the `flet` body scores at the surrounding level.
fn lower_flet(b: &mut Builder, items: &[Datum]) {
    if let Some(DatumKind::List { items: binds, .. }) = items.get(1).map(|d| &d.kind) {
        for bd in binds {
            if let DatumKind::List { items: bi, .. } = &bd.kind
                && let Some(name) = bi.first().and_then(as_symbol)
            {
                let name = name.to_string();
                let body = bi.get(2..).unwrap_or(&[]).to_vec();
                b.emit_function(name, "flet", bd.line, |bl| bl.lower_seq(&body));
            }
        }
    }
    b.lower_seq(items.get(2..).unwrap_or(&[]));
}

// ---- let --------------------------------------------------------------

/// `let` / `let*`: transparent scoping; lower binding initializers + body.
fn lower_let(b: &mut Builder, items: &[Datum]) {
    lower_binding_inits(b, items.get(1));
    b.lower_seq(items.get(2..).unwrap_or(&[]));
}

/// Lower the initializer of each `(var init)` binding (a bare `var` has none).
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

/// `(if test then else)` — a single-decision expression → [`Node::Conditional`].
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

/// Lower a `cond` clause list into a flat `Branch` chain. A `t` clause is the
/// terminal `else`.
fn lower_cond_clauses(b: &mut Builder, clauses: &[Datum]) -> Option<Box<Node>> {
    let (first, rest) = clauses.split_first()?;
    let DatumKind::List { items: ci, .. } = &first.kind else {
        return lower_cond_clauses(b, rest);
    };
    if is_else(ci.first()) {
        let body = b.collect(|b| b.lower_seq(ci.get(1..).unwrap_or(&[])));
        return Some(Box::new(Node::Group(body)));
    }
    let test = b.collect(|b| b.lower_opt(ci.first()));
    let then = b.collect(|b| b.lower_seq(ci.get(1..).unwrap_or(&[])));
    let alternate = lower_cond_clauses(b, rest);
    Some(Box::new(Node::Branch {
        test,
        then,
        alternate,
    }))
}

fn lower_case(b: &mut Builder, items: &[Datum]) {
    // The keyform runs at the switch's own level, before the clauses.
    b.lower_opt(items.get(1));
    let mut cases = Vec::new();
    for cl in items.get(2..).unwrap_or(&[]) {
        if let DatumKind::List { items: ci, .. } = &cl.kind {
            let is_default = is_else(ci.first()) || is_otherwise(ci.first());
            let body = b.collect(|b| b.lower_seq(ci.get(1..).unwrap_or(&[])));
            cases.push(SwitchCase { is_default, body });
        }
    }
    b.emit(Node::Switch { cases });
}

// ---- loops ------------------------------------------------------------

/// `(dotimes (var count) body…)` / `(dolist (var list) body…)`.
fn lower_iter_loop(b: &mut Builder, items: &[Datum]) {
    if let Some(DatumKind::List { items: spec, .. }) = items.get(1).map(|d| &d.kind) {
        b.lower_seq(spec.get(1..).unwrap_or(&[]));
    }
    let body = b.collect(|b| b.lower_seq(items.get(2..).unwrap_or(&[])));
    b.emit(Node::Loop { body });
}

/// `(do ((var init step)…) (end result…) body…)`.
fn lower_do(b: &mut Builder, items: &[Datum]) {
    lower_do_specs(b, items.get(1), 1);
    let items_owned = items.to_vec();
    let body = b.collect(|b| {
        lower_do_specs(b, items_owned.get(1), 2);
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

/// `(handler-case protected (type (var) body…)…)`.
fn lower_handler_case(b: &mut Builder, items: &[Datum]) {
    b.lower_opt(items.get(1));
    for cl in items.get(2..).unwrap_or(&[]) {
        if let DatumKind::List { items: ci, .. } = &cl.kind {
            if matches_ci(ci.first(), ":no-error") {
                b.lower_seq(ci.get(2..).unwrap_or(&[]));
                continue;
            }
            // (type (var) body…) — body is items[2..]; the (var) list at [1] is
            // a binding, not code.
            let body = b.collect(|b| b.lower_seq(ci.get(2..).unwrap_or(&[])));
            b.emit(Node::Catch { body });
        }
    }
}

// ---- application ------------------------------------------------------

fn lower_call(b: &mut Builder, items: &[Datum], tail: Option<&Datum>) {
    b.emit(Node::Call {
        callee: head_symbol(items).map(str::to_ascii_lowercase),
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

/// True if a datum is the symbol `s` (case-insensitively).
fn matches_ci(d: Option<&Datum>, s: &str) -> bool {
    d.and_then(as_symbol)
        .is_some_and(|t| t.eq_ignore_ascii_case(s))
}

/// A `cond` / `case` clause catch-all: the constant-true symbol `t`.
fn is_else(d: Option<&Datum>) -> bool {
    matches_ci(d, "t")
}

/// A `case` clause default marker.
fn is_otherwise(d: Option<&Datum>) -> bool {
    matches_ci(d, "otherwise")
}
#[cfg(test)]
mod tests {
    use super::*;
    use cccc_lisp_kit::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(std::path::Path::new("test.lisp"), src)
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
        let (_nodes, errors) = to_ir(std::path::Path::new("bad.lisp"), "(defun f (x");
        assert!(!errors.is_empty());
    }
}
