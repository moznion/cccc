//! PHP adapter: parses source with [php-rs-parser](https://docs.rs/php-rs-parser)
//! and lowers the AST into the language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core` and the pure-Rust
//! `php-rs-parser` / `php-ast` crates (no C toolchain, so cross-compilation stays
//! clean), with no CLI machinery. The `cccc-php` binary lives in the separate
//! `cccc-php-cli` crate, which combines [`analyze_source`]/[`DEFAULT_EXTS`] with
//! the shared `cccc-cli` runner.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about (functions/methods/closures/arrows/property-hooks,
//! `if`/`elseif`/`else`, `switch`/`match`, loops, `try`/`catch`, multi-level
//! `break`/`continue`/`goto`, `&&`/`||`/`and`/`or`/`??` sequences, calls) and
//! emits the matching IR nodes. All complexity rules live in
//! [`cccc_core::engine`].
//!
//! ## Lowering strategy
//!
//! `php-ast` ships an [`OwnedVisitor`] trait whose default `walk_owned_*`
//! methods recurse into *every* child. We drive lowering from it so no construct
//! in an unexpected position (a closure in a default argument, an operator inside
//! an index) is silently missed: we override [`OwnedVisitor::visit_stmt`] and
//! [`OwnedVisitor::visit_expr`], handle the structural nodes explicitly (building
//! the IR with a stack of "collectors", see [`Builder::collect`]), and for every
//! other node fall through to the parser's complete default walk. The owned
//! (lifetime-free) AST lets us avoid juggling the parser's arena.
//!
//! ## PHP-to-IR mapping notes
//!
//! - `function` / method / closure / `fn` arrow / property hook →
//!   [`Node::Function`] (`"function"` / `"method"` / `"closure"` / `"arrow"` /
//!   `"hook"`). A closure/arrow bound to a variable (`$f = fn() => …`) borrows
//!   that name.
//! - `if` / `elseif` / `else` → [`Node::Branch`] (chaining `elseif` as a nested
//!   `Branch` so it scores flat).
//! - ternary `?:` (and the short `?:`) → [`Node::Conditional`]; `??` →
//!   [`Node::Logical`] with [`LogicalOp::Coalesce`].
//! - `while` / `do`-`while` / `for` / `foreach` → [`Node::Loop`].
//! - `switch` and the `match` expression → [`Node::Switch`]; the `default` arm is
//!   not a cyclomatic decision point.
//! - `catch` clauses → [`Node::Catch`] (the `try` and `finally` bodies score at
//!   the surrounding level).
//! - `goto` and multi-level `break N` / `continue N` (`N >= 2`) → labelled
//!   [`Node::Jump`]; plain `break` / `continue` score flat.
//! - `&&` / `and` / `||` / `or` / `??` runs → folded [`Node::Logical`] (one node
//!   per like-operator run).
//! - calls (`f(..)`, `$o->m(..)`, `C::m(..)`) → [`Node::Call`] for recursion
//!   detection.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use php_ast::ast::BinaryOp;
use php_ast::owned::visitor::{OwnedVisitor, walk_owned_expr, walk_owned_stmt};
use php_ast::owned::{
    ClassDecl, ClassMemberKind, EnumMemberKind, Expr, ExprKind, Ident, NamespaceBody, PropertyHook,
    PropertyHookBody, Stmt, StmtKind,
};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["php"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any parser error messages. `php-rs-parser` is fault-tolerant: it
/// always yields a (possibly partial) AST, so we lower whatever it recovered and
/// surface the diagnostics alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let result = php_rs_parser::parse(source);
    let mut builder = Builder::new(source);
    for stmt in result.program.stmts.iter() {
        let _ = builder.visit_stmt(stmt);
    }
    let parse_errors = result.errors.iter().map(|e| e.to_string()).collect();
    (builder.finish(), parse_errors)
}

/// Assembles the IR tree while `php-ast`'s [`OwnedVisitor`] drives a complete
/// AST traversal.
struct Builder {
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
    /// Byte offset of the start of each 1-based line, for offset → line lookup.
    line_starts: Vec<u32>,
    /// Name captured from `$x = fn()…` / `$x = function()…` to label the next
    /// closure or arrow function.
    pending_name: Option<String>,
}

impl Builder {
    fn new(source: &str) -> Self {
        // line_starts[0] = 0 (line 1); each '\n' begins the next line.
        let mut line_starts = vec![0u32];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        Self {
            stack: vec![Vec::new()], // module-level collector
            line_starts,
            pending_name: None,
        }
    }

    /// 1-based line containing byte offset `pos`.
    fn line(&self, pos: u32) -> u32 {
        self.line_starts.partition_point(|&start| start <= pos) as u32
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

    // ---- statements -------------------------------------------------------

    fn lower_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Function(f) => {
                let line = self.line(stmt.span.start);
                self.emit_function(ident_name(&f.name, "<function>"), "function", line, |b| {
                    for s in f.body.stmts.iter() {
                        let _ = b.visit_stmt(s);
                    }
                });
            }
            StmtKind::If(i) => {
                let test = self.collect(|b| {
                    let _ = b.visit_expr(&i.condition);
                });
                let then = self.collect(|b| {
                    let _ = b.visit_stmt(&i.then_branch);
                });
                let alternate = self.lower_else(&i.elseif_branches, &i.else_branch);
                self.emit(Node::Branch {
                    test,
                    then,
                    alternate,
                });
            }
            StmtKind::While(w) => {
                let body = self.collect(|b| {
                    let _ = b.visit_expr(&w.condition);
                    let _ = b.visit_stmt(&w.body);
                });
                self.emit(Node::Loop { body });
            }
            StmtKind::DoWhile(d) => {
                let body = self.collect(|b| {
                    let _ = b.visit_stmt(&d.body);
                    let _ = b.visit_expr(&d.condition);
                });
                self.emit(Node::Loop { body });
            }
            StmtKind::For(f) => {
                let body = self.collect(|b| {
                    for e in f
                        .init
                        .iter()
                        .chain(f.condition.iter())
                        .chain(f.update.iter())
                    {
                        let _ = b.visit_expr(e);
                    }
                    let _ = b.visit_stmt(&f.body);
                });
                self.emit(Node::Loop { body });
            }
            StmtKind::Foreach(f) => {
                let body = self.collect(|b| {
                    let _ = b.visit_expr(&f.expr);
                    if let Some(key) = &f.key {
                        let _ = b.visit_expr(key);
                    }
                    let _ = b.visit_expr(&f.value);
                    let _ = b.visit_stmt(&f.body);
                });
                self.emit(Node::Loop { body });
            }
            StmtKind::Switch(sw) => {
                // The subject runs at the switch's own level, before its cases.
                let _ = self.visit_expr(&sw.expr);
                let mut cases = Vec::new();
                for case in sw.body.cases.iter() {
                    let body = self.collect(|b| {
                        if let Some(value) = &case.value {
                            let _ = b.visit_expr(value);
                        }
                        for s in case.body.iter() {
                            let _ = b.visit_stmt(s);
                        }
                    });
                    cases.push(SwitchCase {
                        is_default: case.value.is_none(),
                        body,
                    });
                }
                self.emit(Node::Switch { cases });
            }
            StmtKind::TryCatch(t) => {
                for s in t.body.stmts.iter() {
                    let _ = self.visit_stmt(s);
                }
                for catch in t.catches.iter() {
                    let body = self.collect(|b| {
                        for s in catch.body.stmts.iter() {
                            let _ = b.visit_stmt(s);
                        }
                    });
                    self.emit(Node::Catch { body });
                }
                if let Some(finally) = &t.finally {
                    for s in finally.stmts.iter() {
                        let _ = self.visit_stmt(s);
                    }
                }
            }
            StmtKind::Break(level) => self.emit(Node::Jump {
                labeled: is_multi_level(level),
            }),
            StmtKind::Continue(level) => self.emit(Node::Jump {
                labeled: is_multi_level(level),
            }),
            // `goto` always names a label, so it scores one flat point.
            StmtKind::Goto(_) => self.emit(Node::Jump { labeled: true }),
            StmtKind::Class(c) => self.lower_class(c),
            StmtKind::Interface(i) => self.lower_members(&i.body.members),
            StmtKind::Trait(t) => self.lower_members(&t.body.members),
            StmtKind::Enum(e) => {
                for member in e.body.members.iter() {
                    if let EnumMemberKind::Method(m) = &member.kind
                        && let Some(body) = &m.body
                    {
                        let line = self.line(member.span.start);
                        self.emit_function(ident_name(&m.name, "<method>"), "method", line, |b| {
                            for s in body.stmts.iter() {
                                let _ = b.visit_stmt(s);
                            }
                        });
                    }
                }
            }
            StmtKind::Namespace(n) => {
                if let NamespaceBody::Braced(block) = &n.body {
                    for s in block.stmts.iter() {
                        let _ = self.visit_stmt(s);
                    }
                }
            }
            StmtKind::Declare(d) => {
                if let Some(body) = &d.body {
                    let _ = self.visit_stmt(body);
                }
            }
            // Everything else (expression statements, echo, return, throw, …)
            // carries no structural score of its own; the parser's default walk
            // recurses into its sub-expressions so nested calls / closures /
            // operators are still reached.
            _ => {
                let _ = walk_owned_stmt(self, stmt);
            }
        }
    }

    /// Fold an `elseif`/`else` tail into the outer branch's `alternate`: each
    /// `elseif` is a nested `Branch` (so it scores flat), a plain `else` is a
    /// `Group`.
    fn lower_else(
        &mut self,
        elseifs: &[php_ast::owned::ElseIfBranch],
        else_branch: &Option<Box<Stmt>>,
    ) -> Option<Box<Node>> {
        if let Some((first, rest)) = elseifs.split_first() {
            let test = self.collect(|b| {
                let _ = b.visit_expr(&first.condition);
            });
            let then = self.collect(|b| {
                let _ = b.visit_stmt(&first.body);
            });
            let alternate = self.lower_else(rest, else_branch);
            Some(Box::new(Node::Branch {
                test,
                then,
                alternate,
            }))
        } else {
            else_branch.as_ref().map(|else_stmt| {
                Box::new(Node::Group(self.collect(|b| {
                    let _ = b.visit_stmt(else_stmt);
                })))
            })
        }
    }

    fn lower_class(&mut self, class: &ClassDecl) {
        self.lower_members(&class.body.members);
    }

    fn lower_members(&mut self, members: &[php_ast::owned::ClassMember]) {
        for member in members {
            match &member.kind {
                ClassMemberKind::Method(m) => {
                    // A bodyless method (abstract / interface) is not a
                    // measurable unit, so don't report it as a function.
                    if let Some(body) = &m.body {
                        let line = self.line(member.span.start);
                        self.emit_function(ident_name(&m.name, "<method>"), "method", line, |b| {
                            for s in body.stmts.iter() {
                                let _ = b.visit_stmt(s);
                            }
                        });
                    }
                }
                // PHP 8.4 property hooks (`get`/`set`) hold runnable code.
                ClassMemberKind::Property(p) => self.lower_hooks(&p.hooks),
                ClassMemberKind::ClassConst(_) | ClassMemberKind::TraitUse(_) => {}
            }
        }
    }

    fn lower_hooks(&mut self, hooks: &[PropertyHook]) {
        for hook in hooks {
            let line = self.line(hook.span.start);
            match &hook.body {
                PropertyHookBody::Block(block) => {
                    self.emit_function("<hook>".to_string(), "hook", line, |b| {
                        for s in block.stmts.iter() {
                            let _ = b.visit_stmt(s);
                        }
                    });
                }
                PropertyHookBody::Expression(expr) => {
                    self.emit_function("<hook>".to_string(), "hook", line, |b| {
                        let _ = b.visit_expr(expr);
                    });
                }
                PropertyHookBody::Abstract => {}
            }
        }
    }

    // ---- expressions ------------------------------------------------------

    fn lower_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Closure(c) => {
                let name = self
                    .pending_name
                    .take()
                    .unwrap_or_else(|| "<closure>".into());
                let line = self.line(expr.span.start);
                self.emit_function(name, "closure", line, |b| {
                    for s in c.body.stmts.iter() {
                        let _ = b.visit_stmt(s);
                    }
                });
            }
            ExprKind::ArrowFunction(a) => {
                let name = self.pending_name.take().unwrap_or_else(|| "<arrow>".into());
                let line = self.line(expr.span.start);
                self.emit_function(name, "arrow", line, |b| {
                    let _ = b.visit_expr(&a.body);
                });
            }
            ExprKind::Ternary(t) => {
                let test = self.collect(|b| {
                    let _ = b.visit_expr(&t.condition);
                });
                // The short ternary `$a ?: $b` has no `then` expression.
                let then = match &t.then_expr {
                    Some(e) => self.collect(|b| {
                        let _ = b.visit_expr(e);
                    }),
                    None => Vec::new(),
                };
                let alternate = self.collect(|b| {
                    let _ = b.visit_expr(&t.else_expr);
                });
                self.emit(Node::Conditional {
                    test,
                    then,
                    alternate,
                });
            }
            ExprKind::Match(m) => {
                // The subject runs at the match's own level, before its arms.
                let _ = self.visit_expr(&m.subject);
                let mut cases = Vec::new();
                for arm in m.arms.iter() {
                    let body = self.collect(|b| {
                        if let Some(conditions) = &arm.conditions {
                            for c in conditions.iter() {
                                let _ = b.visit_expr(c);
                            }
                        }
                        let _ = b.visit_expr(&arm.body);
                    });
                    cases.push(SwitchCase {
                        is_default: arm.conditions.is_none(),
                        body,
                    });
                }
                self.emit(Node::Switch { cases });
            }
            // `&&` / `and` / `||` / `or` and `??` start a folded logical run.
            _ if logical_op(&expr.kind).is_some() => {
                let op = logical_op(&expr.kind).expect("checked");
                let (left, right) = logical_children(&expr.kind).expect("checked");
                let mut operands = Vec::new();
                self.collect_logical_side(left, op, &mut operands);
                self.collect_logical_side(right, op, &mut operands);
                self.emit(Node::Logical { op, operands });
            }
            ExprKind::FunctionCall(_)
            | ExprKind::MethodCall(_)
            | ExprKind::NullsafeMethodCall(_)
            | ExprKind::StaticMethodCall(_)
            | ExprKind::StaticDynMethodCall(_) => {
                self.emit(Node::Call {
                    callee: callee_name(&expr.kind),
                });
                // Recurse into receiver + arguments (nested calls / closures).
                let _ = walk_owned_expr(self, expr);
            }
            // `new class { … }` lowers its members exactly like a named class:
            // each method becomes its own `Function` unit rather than folding
            // into the surrounding frame.
            ExprKind::AnonymousClass(c) => {
                self.lower_class(c);
                // Still recurse into the `new`'s arguments for nested calls /
                // closures, but skip the class body (already lowered above).
            }
            ExprKind::Assign(a) => {
                // `$x = fn() …` / `$x = function() …` labels the next closure.
                if let ExprKind::Variable(name) = &a.target.kind
                    && matches!(
                        a.value.kind,
                        ExprKind::Closure(_) | ExprKind::ArrowFunction(_)
                    )
                {
                    self.pending_name = Some(name.to_string());
                }
                let _ = walk_owned_expr(self, expr);
            }
            // Any other expression carries no score of its own; recurse into its
            // children so nested constructs are still reached.
            _ => {
                let _ = walk_owned_expr(self, expr);
            }
        }
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    /// Parentheses are transparent (`a && (b && c)` folds to one run).
    fn collect_logical_side(&mut self, side: &Expr, op: LogicalOp, operands: &mut Vec<Node>) {
        let side = unwrap_parens(side);
        match logical_op(&side.kind) {
            Some(side_op) if side_op == op => {
                let (left, right) = logical_children(&side.kind).expect("checked");
                self.collect_logical_side(left, op, operands);
                self.collect_logical_side(right, op, operands);
            }
            Some(side_op) => {
                let (left, right) = logical_children(&side.kind).expect("checked");
                let mut sub = Vec::new();
                self.collect_logical_side(left, side_op, &mut sub);
                self.collect_logical_side(right, side_op, &mut sub);
                operands.push(Node::Logical {
                    op: side_op,
                    operands: sub,
                });
            }
            None => {
                let nodes = self.collect(|b| {
                    let _ = b.visit_expr(side);
                });
                operands.push(Node::Group(nodes));
            }
        }
    }
}

impl OwnedVisitor for Builder {
    fn visit_stmt(&mut self, stmt: &Stmt) -> std::ops::ControlFlow<()> {
        self.lower_stmt(stmt);
        std::ops::ControlFlow::Continue(())
    }

    fn visit_expr(&mut self, expr: &Expr) -> std::ops::ControlFlow<()> {
        self.lower_expr(expr);
        std::ops::ControlFlow::Continue(())
    }
}

/// Follow `Parenthesized` wrappers to the inner expression.
fn unwrap_parens(expr: &Expr) -> &Expr {
    match &expr.kind {
        ExprKind::Parenthesized(inner) => unwrap_parens(inner),
        _ => expr,
    }
}

/// The normalized logical operator of an expression, if it is one. PHP has two
/// precedence tiers that mean the same thing (`&&`/`and`, `||`/`or`); both
/// normalize to the same [`LogicalOp`], and `??` maps to [`LogicalOp::Coalesce`].
fn logical_op(kind: &ExprKind) -> Option<LogicalOp> {
    match kind {
        ExprKind::Binary(b) => match b.op {
            BinaryOp::BooleanAnd | BinaryOp::LogicalAnd => Some(LogicalOp::And),
            BinaryOp::BooleanOr | BinaryOp::LogicalOr => Some(LogicalOp::Or),
            _ => None,
        },
        ExprKind::NullCoalesce(_) => Some(LogicalOp::Coalesce),
        _ => None,
    }
}

/// The two operands of a logical expression (`Binary` or `??`).
fn logical_children(kind: &ExprKind) -> Option<(&Expr, &Expr)> {
    match kind {
        ExprKind::Binary(b) => Some((&b.left, &b.right)),
        ExprKind::NullCoalesce(nc) => Some((&nc.left, &nc.right)),
        _ => None,
    }
}

/// Simple name of a directly-called callee, used for recursion detection.
/// Returns the trailing identifier (`foo()` and `$o->foo()` and `C::foo()` all
/// yield `Some("foo")`); a namespaced `a\b\foo()` yields `Some("foo")`.
fn callee_name(kind: &ExprKind) -> Option<String> {
    let name_expr = match kind {
        ExprKind::FunctionCall(c) => &c.name,
        ExprKind::MethodCall(c) | ExprKind::NullsafeMethodCall(c) => &c.method,
        ExprKind::StaticMethodCall(c) => &c.method,
        ExprKind::StaticDynMethodCall(c) => &c.method,
        _ => return None,
    };
    match &name_expr.kind {
        ExprKind::Identifier(s) => Some(s.rsplit('\\').next().unwrap_or(s).to_string()),
        _ => None,
    }
}

/// `true` for a multi-level `break N` / `continue N` (`N >= 2`), which jumps out
/// of more than one enclosing loop/switch and so scores like a labelled jump.
fn is_multi_level(level: &Option<Box<Expr>>) -> bool {
    matches!(level.as_deref(), Some(Expr { kind: ExprKind::Int(n), .. }) if *n >= 2)
}

/// A declaration name, or `default` when the parser could not recover one.
fn ident_name(ident: &Ident, default: &str) -> String {
    ident
        .as_ref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.php"), src)
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
    fn sonar_sum_of_primes_is_7() {
        let src = r#"<?php
            function sumOfPrimes($max) {
                $total = 0;
                for ($i = 2; $i <= $max; $i++) {
                    for ($j = 2; $j < $i; $j++) {
                        if ($i % $j == 0) {
                            continue 2;
                        }
                    }
                    $total += $i;
                }
                return $total;
            }
        "#;
        // for(+1) + nested for(+2) + nested if(+3) + multi-level continue(+1) = 7
        assert_eq!(cognitive_of(src, "sumOfPrimes"), 7);
        // base 1 + for + for + if = 4
        assert_eq!(cyclomatic_of(src, "sumOfPrimes"), 4);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"<?php
            function getWords($number) {
                switch ($number) {
                    case 1:
                        return "one";
                    case 2:
                        return "a couple";
                    default:
                        return "lots";
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "getWords"), 1);
        // base 1 + 2 non-default cases = 3
        assert_eq!(cyclomatic_of(src, "getWords"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"<?php
            function f($a, $b, $c) {
                if ($a) {
                    if ($b) {
                        if ($c) {
                        }
                    }
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 6);
    }

    #[test]
    fn elseif_else_are_flat() {
        let src = r#"<?php
            function f($a, $b) {
                if ($a) {
                } elseif ($b) {
                } else {
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if + elseif = 3 (the else is not a decision point)
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"<?php
            function f($a, $items) {
                while ($a) {
                }
                for ($i = 0; $i < 3; $i++) {
                }
                foreach ($items as $i) {
                }
                do {
                } while ($a);
            }
        "#;
        // four loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn logical_sequences_fold_by_operator() {
        let src = r#"<?php
            function f($a, $b, $c, $d) {
                if ($a && $b && $c || $d) {
                }
            }
        "#;
        // if(+1) + && run(+1) + || run(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn keyword_and_or_are_logical_and_fold_with_symbols() {
        let src = r#"<?php
            function f($a, $b, $c) {
                if ($a && $b and $c) {
                }
            }
        "#;
        // `&&` and `and` are the same normalized operator → one folded run.
        // if(+1) + and-run(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
    }

    #[test]
    fn null_coalesce_is_one_logical_run() {
        let src = r#"<?php
            function f($a, $b, $c) {
                return $a ?? $b ?? $c;
            }
        "#;
        // a single folded `??` run = +1
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn ternary_is_a_conditional() {
        let src = r#"<?php
            function f($a) {
                return $a ? 1 : 2;
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn match_scores_like_a_switch() {
        let src = r#"<?php
            function f($x) {
                return match ($x) {
                    1, 2 => "a",
                    3 => "b",
                    default => "c",
                };
            }
        "#;
        // match(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + 2 non-default arms = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn catch_clause_counts() {
        let src = r#"<?php
            function f() {
                try {
                    risky();
                } catch (\Exception $e) {
                    handle();
                }
            }
        "#;
        // catch(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + catch 1 = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"<?php
            function fib($n) {
                if ($n < 2) {
                    return $n;
                }
                return fib($n - 1) + fib($n - 2);
            }
        "#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn method_recursion_is_detected() {
        let src = r#"<?php
            class S {
                function walk($n) {
                    if ($n == 0) {
                        return 0;
                    } else {
                        return $this->walk($n - 1);
                    }
                }
            }
        "#;
        // if(+1) + else(+1) + recursion(+1) = 3
        assert_eq!(cognitive_of(src, "walk"), 3);
        assert_eq!(
            find(&analyze(src).functions, "walk").unwrap().kind,
            "method"
        );
    }

    #[test]
    fn static_method_recursion_is_detected() {
        let src = r#"<?php
            class S {
                static function fib($n) {
                    if ($n < 2) {
                        return $n;
                    }
                    return self::fib($n - 1) + self::fib($n - 2);
                }
            }
        "#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn closure_is_its_own_unit_and_named() {
        let src = r#"<?php
            function host() {
                $pick = function ($a, $b) {
                    if ($a && $b) {
                        return 1;
                    } else {
                        return 0;
                    }
                };
                return $pick;
            }
        "#;
        // host owns no structural complexity; the closure does.
        assert_eq!(cognitive_of(src, "host"), 0);
        // if(+1) + && run(+1) + else(+1) = 3
        assert_eq!(cognitive_of(src, "pick"), 3);
        assert_eq!(
            find(&analyze(src).functions, "pick").unwrap().kind,
            "closure"
        );
    }

    #[test]
    fn arrow_function_is_its_own_unit_and_named() {
        let src = r#"<?php
            function host() {
                $f = fn ($x) => $x && $x;
                return $f;
            }
        "#;
        assert_eq!(cognitive_of(src, "host"), 0);
        // && run(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(find(&analyze(src).functions, "f").unwrap().kind, "arrow");
    }

    #[test]
    fn multi_level_break_is_labelled_but_plain_break_is_flat() {
        let labelled = r#"<?php
            function f($items) {
                foreach ($items as $i) {
                    while (true) {
                        break 2;
                    }
                }
            }
        "#;
        // foreach(+1) + while(+2) + multi-level break(+1) = 4
        assert_eq!(cognitive_of(labelled, "f"), 4);

        let plain = r#"<?php
            function f($items) {
                foreach ($items as $i) {
                    if ($i) {
                        break;
                    }
                }
            }
        "#;
        // foreach(+1) + if(+2) + plain break(0) = 3
        assert_eq!(cognitive_of(plain, "f"), 3);
    }

    #[test]
    fn anonymous_class_methods_are_their_own_units() {
        let src = r#"<?php
            function host($cb) {
                $obj = new class($cb, fn ($z) => $z && $z) {
                    public function m($x) {
                        if ($x) {
                            return 1;
                        }
                        return 0;
                    }
                    public function n($y) {
                        return $y ?? 0;
                    }
                };
                return $obj;
            }
        "#;
        // The anonymous class's methods must each be their own Function unit,
        // not folded into `host`, which itself owns no structural complexity.
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "m"), 1); // if
        assert_eq!(cognitive_of(src, "n"), 1); // ??
        assert_eq!(find(&analyze(src).functions, "m").unwrap().kind, "method");
        // A closure passed as a constructor argument is still reached.
        assert_eq!(cognitive_of(src, "<arrow>"), 1); // &&
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"<?php
            function a($x) {
                if ($x) {
                }
            }
            function b($y) {
                if ($y) {
                }
            }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn parse_error_is_reported() {
        // `php-rs-parser` is fault-tolerant: it still yields a (partial) AST, but
        // surfaces a diagnostic for the broken input.
        let (_nodes, errors) = to_ir(Path::new("bad.php"), "<?php function f( {");
        assert!(!errors.is_empty());
    }
}
