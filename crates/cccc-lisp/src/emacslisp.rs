//! Emacs Lisp lowering. Case-**sensitive** reader; `[…]` is a data vector.
//!
//! Emacs-Lisp-to-IR mapping: `defun`/`defmacro`/`defsubst`/`cl-defun`/
//! `cl-defmacro`/`cl-defmethod`/`lambda` and `cl-flet`/`cl-labels` → `Function`;
//! `if` → `Conditional` (its `else` arm may be several trailing forms);
//! `when`/`unless` → `Branch`; `cond` → a flat `Branch` chain (a `t` clause is
//! the terminal `else`); `pcase`/`cl-case`/`cl-typecase` → `Switch` (a `_`/`t`
//! pattern is the default); `while`/`dotimes`/`dolist`/`cl-loop` → `Loop`;
//! `and`/`or` → folded `Logical`; `condition-case` handlers → `Catch`; `throw`
//! (a non-local exit to a `catch` tag) → a labelled `Jump`; a plain
//! application → `Call`.

use std::path::Path;

use cccc_lisp_kit::{
    Builder, Datum, DatumKind, Delim, FileReport, LogicalOp, Node, Options, SwitchCase, as_symbol,
    head_symbol,
};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["el"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    cccc_lisp_kit::analyze(&Options::emacs_lisp(), lower_list, logical_op, path, source)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any reader diagnostics.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    cccc_lisp_kit::lower(&Options::emacs_lisp(), lower_list, logical_op, source)
}

/// The normalized logical operator named by a list head.
fn logical_op(head: Option<&str>) -> Option<LogicalOp> {
    match head {
        Some("and") => Some(LogicalOp::And),
        Some("or") => Some(LogicalOp::Or),
        _ => None,
    }
}

fn lower_list(b: &mut Builder, d: &Datum, _delim: Delim, items: &[Datum], tail: Option<&Datum>) {
    if items.is_empty() {
        return;
    }
    match head_symbol(items) {
        // ---- definitions ----
        Some("defun") | Some("defmacro") | Some("defsubst") | Some("cl-defun")
        | Some("cl-defmacro") | Some("cl-defsubst") | Some("iter-defun") => {
            lower_defun(b, d, items)
        }
        Some("cl-defmethod") | Some("cl-defgeneric") => lower_defmethod(b, d, items),
        Some("lambda") => emit_lambda(b, "<lambda>".to_string(), items, d.line),
        Some("cl-flet") | Some("cl-labels") | Some("cl-macrolet") | Some("cl-flet*") => {
            lower_flet(b, items)
        }
        // ---- conditionals ----
        Some("if") => lower_if(b, items),
        Some("when") | Some("unless") => lower_when(b, items),
        Some("cond") => {
            if let Some(node) = lower_cond_clauses(b, &items[1..]) {
                b.emit(*node);
            }
        }
        Some("pcase")
        | Some("pcase-exhaustive")
        | Some("cl-case")
        | Some("cl-ecase")
        | Some("cl-typecase")
        | Some("cl-etypecase") => lower_case(b, items),
        Some("and") => b.lower_logical(LogicalOp::And, &items[1..]),
        Some("or") => b.lower_logical(LogicalOp::Or, &items[1..]),
        // ---- loops ----
        Some("while") => lower_while(b, items),
        Some("dotimes") | Some("dolist") | Some("cl-dotimes") | Some("cl-dolist") => {
            lower_iter_loop(b, items)
        }
        Some("cl-loop") => {
            let body = b.collect(|b| b.lower_seq(&items[1..]));
            b.emit(Node::Loop { body });
        }
        Some("cl-do") | Some("cl-do*") => lower_do(b, items),
        // ---- exceptions / control ----
        Some("condition-case") => lower_condition_case(b, items),
        // `throw` transfers control to a matching `catch` tag — a non-local jump.
        Some("throw") => b.emit(Node::Jump { labeled: true }),
        // ---- binding / grouping: transparent ----
        Some("let")
        | Some("let*")
        | Some("cl-letf")
        | Some("cl-letf*")
        | Some("letrec")
        | Some("if-let")
        | Some("if-let*")
        | Some("when-let")
        | Some("when-let*")
        | Some("pcase-let")
        | Some("pcase-let*")
        | Some("seq-let")
        | Some("cl-destructuring-bind")
        | Some("cl-multiple-value-bind") => lower_let_like(b, items),
        Some("progn")
        | Some("prog1")
        | Some("prog2")
        | Some("catch")
        | Some("unwind-protect")
        | Some("save-excursion")
        | Some("save-restriction")
        | Some("save-match-data")
        | Some("save-current-buffer")
        | Some("with-current-buffer")
        | Some("with-temp-buffer")
        | Some("with-temp-file")
        | Some("with-output-to-string")
        | Some("with-output-to-temp-buffer")
        | Some("with-syntax-table")
        | Some("with-silent-modifications")
        | Some("atomic-change-group")
        | Some("combine-after-change-calls")
        | Some("setq")
        | Some("setq-local")
        | Some("setq-default")
        | Some("setf")
        | Some("cl-incf")
        | Some("cl-decf")
        | Some("push")
        | Some("pop")
        | Some("prog")
        | Some("cl-block")
        | Some("cl-return")
        | Some("cl-return-from")
        | Some("eval-when-compile")
        | Some("eval-and-compile") => b.lower_seq(&items[1..]),
        // ---- value definitions: lower the init form ----
        Some("defvar") | Some("defvar-local") | Some("defconst") | Some("defcustom") => {
            if let Some(init) = items.get(2) {
                b.lower_datum(init);
            }
        }
        // ---- data / declarations / compile-time: skip ----
        Some("quote") => {}
        Some("declare")
        | Some("declare-function")
        | Some("defgroup")
        | Some("defface")
        | Some("provide")
        | Some("require")
        | Some("defalias")
        | Some("cl-defstruct")
        | Some("define-error") => {}
        // A plain application.
        _ => lower_call(b, items, tail),
    }
}

// ---- functions --------------------------------------------------------

/// `(defun name (args) [doc] body…)`.
fn lower_defun(b: &mut Builder, d: &Datum, items: &[Datum]) {
    let name = items
        .get(1)
        .and_then(as_symbol)
        .unwrap_or("<defun>")
        .to_string();
    let body = items.get(3..).unwrap_or(&[]).to_vec();
    b.emit_function(name, "defun", d.line, |b| b.lower_seq(&body));
}

/// `(cl-defmethod name [qualifier…] (specialized-args) body…)`.
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
    b.emit_function(name, "cl-defmethod", d.line, |b| b.lower_seq(&body));
}

fn emit_lambda(b: &mut Builder, name: String, items: &[Datum], line: u32) {
    let body = items.get(2..).unwrap_or(&[]).to_vec();
    b.emit_function(name, "lambda", line, |b| b.lower_seq(&body));
}

/// `(cl-flet ((name (args) body…)…) body…)`.
fn lower_flet(b: &mut Builder, items: &[Datum]) {
    if let Some(DatumKind::List { items: binds, .. }) = items.get(1).map(|d| &d.kind) {
        for bd in binds {
            if let DatumKind::List { items: bi, .. } = &bd.kind
                && let Some(name) = bi.first().and_then(as_symbol)
            {
                let name = name.to_string();
                let body = bi.get(2..).unwrap_or(&[]).to_vec();
                b.emit_function(name, "cl-flet", bd.line, |bl| bl.lower_seq(&body));
            }
        }
    }
    b.lower_seq(items.get(2..).unwrap_or(&[]));
}

// ---- let --------------------------------------------------------------

/// `let`/`let*`/`when-let`/…: transparent scoping; lower binding initializers +
/// body.
fn lower_let_like(b: &mut Builder, items: &[Datum]) {
    lower_binding_inits(b, items.get(1));
    b.lower_seq(items.get(2..).unwrap_or(&[]));
}

/// Lower the initializer of each `(var init)` binding (a bare `var` symbol has
/// none).
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

/// `(if test then else…)` — a single-decision expression → `Conditional`.
fn lower_if(b: &mut Builder, items: &[Datum]) {
    let test = b.collect(|b| b.lower_opt(items.get(1)));
    let then = b.collect(|b| b.lower_opt(items.get(2)));
    let alternate = b.collect(|b| b.lower_seq(items.get(3..).unwrap_or(&[])));
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

/// Lower a `cond` clause list into a flat `Branch` chain (a `t` clause is the
/// terminal `else`).
fn lower_cond_clauses(b: &mut Builder, clauses: &[Datum]) -> Option<Box<Node>> {
    let (first, rest) = clauses.split_first()?;
    let DatumKind::List { items: ci, .. } = &first.kind else {
        return lower_cond_clauses(b, rest);
    };
    if is_true(ci.first()) {
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

/// `pcase` / `cl-case` clauses. A `_` or `t` pattern is the default; only the
/// clause body is lowered.
fn lower_case(b: &mut Builder, items: &[Datum]) {
    b.lower_opt(items.get(1));
    let mut cases = Vec::new();
    for cl in items.get(2..).unwrap_or(&[]) {
        if let DatumKind::List { items: ci, .. } = &cl.kind {
            let is_default = is_wildcard(ci.first()) || is_true(ci.first());
            let body = b.collect(|b| b.lower_seq(ci.get(1..).unwrap_or(&[])));
            cases.push(SwitchCase { is_default, body });
        }
    }
    b.emit(Node::Switch { cases });
}

// ---- loops ------------------------------------------------------------

fn lower_while(b: &mut Builder, items: &[Datum]) {
    let body = b.collect(|b| {
        b.lower_opt(items.get(1));
        b.lower_seq(items.get(2..).unwrap_or(&[]));
    });
    b.emit(Node::Loop { body });
}

/// `(dotimes (var count) body…)` / `(dolist (var list) body…)`.
fn lower_iter_loop(b: &mut Builder, items: &[Datum]) {
    if let Some(DatumKind::List { items: spec, .. }) = items.get(1).map(|d| &d.kind) {
        b.lower_seq(spec.get(1..).unwrap_or(&[]));
    }
    let body = b.collect(|b| b.lower_seq(items.get(2..).unwrap_or(&[])));
    b.emit(Node::Loop { body });
}

/// `(cl-do ((var init step)…) (end result…) body…)`.
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

/// `(condition-case var protected handler…)`.
fn lower_condition_case(b: &mut Builder, items: &[Datum]) {
    b.lower_opt(items.get(2));
    for cl in items.get(3..).unwrap_or(&[]) {
        if let DatumKind::List { items: ci, .. } = &cl.kind {
            if matches_kw(ci.first(), ":success") {
                b.lower_seq(ci.get(1..).unwrap_or(&[]));
                continue;
            }
            let body = b.collect(|b| b.lower_seq(ci.get(1..).unwrap_or(&[])));
            b.emit(Node::Catch { body });
        }
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

/// True if the datum is the exact symbol `t` (the constant-true catch-all).
fn is_true(d: Option<&Datum>) -> bool {
    d.and_then(as_symbol) == Some("t")
}

/// True if the datum is the `pcase` wildcard pattern `_`.
fn is_wildcard(d: Option<&Datum>) -> bool {
    d.and_then(as_symbol) == Some("_")
}

/// True if the datum is the keyword `kw` (e.g. `":success"`). lispexp stores a
/// keyword's text verbatim, including the leading colon.
fn matches_kw(d: Option<&Datum>, kw: &str) -> bool {
    matches!(d.map(|d| &d.kind), Some(DatumKind::Keyword(k)) if *k == kw)
}
#[cfg(test)]
mod tests {
    use super::*;
    use cccc_lisp_kit::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(std::path::Path::new("test.el"), src)
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
    fn if_with_multiple_else_forms() {
        let src = r#"
            (defun f (x)
              (if x
                  (foo)
                (bar)
                (baz)))
        "#;
        // Single decision → Conditional(+1); the extra else forms are not
        // separate decisions.
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn cond_is_a_flat_branch_chain() {
        let src = r#"
            (defun classify (n)
              (cond ((< n 0) 'neg)
                    ((= n 0) 'zero)
                    (t 'pos)))
        "#;
        assert_eq!(cognitive_of(src, "classify"), 3);
        assert_eq!(cyclomatic_of(src, "classify"), 3);
    }

    #[test]
    fn pcase_scores_like_a_switch() {
        let src = r#"
            (defun kind (x)
              (pcase x
                (1 "one")
                (2 "two")
                (_ "many")))
        "#;
        assert_eq!(cognitive_of(src, "kind"), 1);
        // base 1 + 2 non-default clauses = 3
        assert_eq!(cyclomatic_of(src, "kind"), 3);
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
    fn while_loop_counts_and_nests() {
        let src = r#"
            (defun f (n)
              (while (> n 0)
                (when (foo n) (bar n))
                (setq n (1- n))))
        "#;
        // while(+1) + nested when(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn dolist_is_a_loop() {
        let src = r#"
            (defun f (xs)
              (dolist (x xs)
                (when (pred x) (process x))))
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn and_or_fold_and_nest() {
        let src = r#"
            (defun f (a b c d)
              (if (or (and a b) (and c d)) 1 0))
        "#;
        assert_eq!(cognitive_of(src, "f"), 4);
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn condition_case_is_a_catch() {
        let src = r#"
            (defun safe (thunk)
              (condition-case err
                  (funcall thunk)
                (error
                 (if (recoverable-p err) (retry) (abort)))))
        "#;
        // catch(+1) + handler if at nesting 1(+2) = 3
        assert_eq!(cognitive_of(src, "safe"), 3);
        assert_eq!(cyclomatic_of(src, "safe"), 3);
    }

    #[test]
    fn lambda_is_its_own_unit() {
        let src = r#"
            (defun host (items)
              (mapcar (lambda (x) (if x 1 0)) items))
        "#;
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "<lambda>"), 1);
    }

    #[test]
    fn cl_flet_locals_are_their_own_units() {
        let src = r#"
            (defun host (xs)
              (cl-flet ((helper (x) (if x 1 0)))
                (mapcar #'helper xs)))
        "#;
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "helper"), 1);
        assert_eq!(
            find(&analyze(src).functions, "helper").unwrap().kind,
            "cl-flet"
        );
    }

    #[test]
    fn throw_is_a_labelled_jump() {
        let src = r#"
            (defun f (xs)
              (catch 'found
                (dolist (x xs)
                  (when (match-p x) (throw 'found x)))))
        "#;
        // catch is transparent; dolist(+1) + when nested(+2) + throw jump(+1) = 4
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn defvar_lowers_its_init() {
        let src = r#"
            (defvar my-var (if (feature-p) 1 2))
        "#;
        // The init `if` runs at module level, so it is not attributed to a
        // function; but it must not error and the file total counts it.
        assert_eq!(analyze(src).cognitive, 1);
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
        let (_nodes, errors) = to_ir(std::path::Path::new("bad.el"), "(defun f (x");
        assert!(!errors.is_empty());
    }
}
