//! ECMAScript (TypeScript/JavaScript) adapter: parses source with
//! [oxc](https://oxc.rs) and lowers the AST into the language-agnostic
//! [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core` and oxc, with no CLI
//! machinery, so embedders pay nothing for clap/ignore/rayon. The unified
//! `cccc` binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] in its language registry and dispatches
//! ECMAScript files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about (functions, branches, loops, switches, catches,
//! labelled jumps, logical-operator sequences, calls) and emits the matching IR
//! nodes. All complexity rules live in [`cccc_core::engine`].
//!
//! ## Why a `Visit`-driven builder
//!
//! Lowering is driven by oxc's [`Visit`] trait. Its default `walk_*` methods
//! traverse the *entire* AST; we override only the nodes that produce IR, so a
//! nested function or logical operator appearing in any expression position is
//! still reached — we never have to enumerate every node kind by hand. The IR
//! tree is assembled with a stack of "collectors": [`Builder::collect`] pushes a
//! fresh child vector, runs a sub-traversal, and pops the nodes it gathered.
//!
//! The one non-trivial lowering is logical-operator folding: a run of like
//! operators (`a && b && c`) becomes a single [`Node::Logical`] with all its
//! operands, so the engine can count one cognitive point per sequence.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::operator::LogicalOperator;
use oxc_syntax::scope::ScopeFlags;

/// File extensions analyzed by default (when `--ext` is not given): TypeScript,
/// JavaScript, and their module/CommonJS variants.
pub const DEFAULT_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

/// Parse `source` (typed by `path`'s extension) and produce its [`FileReport`],
/// scoring via the core engine. This is the convenience entry point used by the
/// CLI; for the raw IR (e.g. to feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any parser error messages (parsing is best-effort and continues
/// past recoverable errors).
pub fn to_ir(path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let ret = Parser::new(&allocator, source, source_type).parse();

    let mut builder = Builder::new(source);
    builder.visit_program(&ret.program);
    let nodes = builder.finish();
    let parse_errors = ret.diagnostics.iter().map(|e| e.to_string()).collect();
    (nodes, parse_errors)
}

/// Assembles the IR tree while oxc's `Visit` drives a complete AST traversal.
struct Builder {
    line_starts: Vec<u32>,
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
    /// Name/kind captured from a declarator/property to label the next function.
    pending_name: Option<String>,
    pending_kind: Option<&'static str>,
}

impl Builder {
    fn new(source: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        Self {
            line_starts,
            stack: vec![Vec::new()], // module-level collector
            pending_name: None,
            pending_kind: None,
        }
    }

    /// The module-level node list (the single remaining collector).
    fn finish(mut self) -> Vec<Node> {
        self.stack.pop().expect("module collector")
    }

    /// 1-based line number for a byte offset.
    fn line(&self, offset: u32) -> u32 {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => (i as u32) + 1,
            Err(i) => i as u32,
        }
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

    /// Build an `if` (recursively, so `else if` becomes a nested `Branch`).
    fn lower_if<'a>(&mut self, it: &IfStatement<'a>) -> Node {
        let test = self.collect(|s| s.visit_expression(&it.test));
        let then = self.collect(|s| s.visit_statement(&it.consequent));
        let alternate = it
            .alternate
            .as_ref()
            .map(|alt| Box::new(self.lower_alternate(alt)));
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// `else if` → nested `Branch`; plain `else` → `Group`.
    fn lower_alternate<'a>(&mut self, stmt: &Statement<'a>) -> Node {
        match stmt {
            Statement::IfStatement(elif) => self.lower_if(elif),
            other => Node::Group(self.collect(|s| s.visit_statement(other))),
        }
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    fn collect_logical_operands<'a>(
        &mut self,
        expr: &LogicalExpression<'a>,
        op: LogicalOperator,
        operands: &mut Vec<Node>,
    ) {
        self.collect_logical_side(&expr.left, op, operands);
        self.collect_logical_side(&expr.right, op, operands);
    }

    fn collect_logical_side<'a>(
        &mut self,
        side: &Expression<'a>,
        op: LogicalOperator,
        operands: &mut Vec<Node>,
    ) {
        match side {
            Expression::LogicalExpression(inner) if inner.operator == op => {
                self.collect_logical_operands(inner, op, operands);
            }
            Expression::LogicalExpression(inner) => {
                let mut sub = Vec::new();
                self.collect_logical_operands(inner, inner.operator, &mut sub);
                operands.push(Node::Logical {
                    op: map_logical_op(inner.operator),
                    operands: sub,
                });
            }
            Expression::ParenthesizedExpression(p) => {
                self.collect_logical_side(&p.expression, op, operands);
            }
            other => {
                operands.push(Node::Group(self.collect(|s| s.visit_expression(other))));
            }
        }
    }
}

impl<'a> Visit<'a> for Builder {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let name = it
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .or_else(|| self.pending_name.take())
            .unwrap_or_else(|| "<anonymous>".to_string());
        let kind = self.pending_kind.take().unwrap_or("function").to_string();
        let line = self.line(it.span.start);
        self.pending_name = None;
        let body = self.collect(|s| walk::walk_function(s, it, flags));
        self.emit(Node::Function {
            name,
            kind,
            line,
            body,
        });
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let name = self
            .pending_name
            .take()
            .unwrap_or_else(|| "<anonymous>".to_string());
        let kind = self.pending_kind.take().unwrap_or("arrow").to_string();
        let line = self.line(it.span.start);
        let body = self.collect(|s| walk::walk_arrow_function_expression(s, it));
        self.emit(Node::Function {
            name,
            kind,
            line,
            body,
        });
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        if let Some(init) = &it.init
            && is_function_like(init)
        {
            self.pending_name = binding_name(&it.id);
        }
        walk::walk_variable_declarator(self, it);
    }

    fn visit_method_definition(&mut self, it: &MethodDefinition<'a>) {
        self.pending_name = prop_key_name(&it.key);
        self.pending_kind = Some(match it.kind {
            MethodDefinitionKind::Get => "getter",
            MethodDefinitionKind::Set => "setter",
            MethodDefinitionKind::Constructor => "constructor",
            MethodDefinitionKind::Method => "method",
        });
        walk::walk_method_definition(self, it);
    }

    fn visit_property_definition(&mut self, it: &PropertyDefinition<'a>) {
        if let Some(value) = &it.value
            && is_function_like(value)
        {
            self.pending_name = prop_key_name(&it.key);
        }
        walk::walk_property_definition(self, it);
    }

    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        if is_function_like(&it.value) {
            self.pending_name = prop_key_name(&it.key);
            if it.method {
                self.pending_kind = Some("method");
            }
        }
        walk::walk_object_property(self, it);
    }

    fn visit_if_statement(&mut self, it: &IfStatement<'a>) {
        let node = self.lower_if(it);
        self.emit(node);
    }

    fn visit_conditional_expression(&mut self, it: &ConditionalExpression<'a>) {
        let test = self.collect(|s| s.visit_expression(&it.test));
        let then = self.collect(|s| s.visit_expression(&it.consequent));
        let alternate = self.collect(|s| s.visit_expression(&it.alternate));
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    fn visit_for_statement(&mut self, it: &ForStatement<'a>) {
        let body = self.collect(|s| walk::walk_for_statement(s, it));
        self.emit(Node::Loop { body });
    }

    fn visit_for_in_statement(&mut self, it: &ForInStatement<'a>) {
        let body = self.collect(|s| walk::walk_for_in_statement(s, it));
        self.emit(Node::Loop { body });
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        let body = self.collect(|s| walk::walk_for_of_statement(s, it));
        self.emit(Node::Loop { body });
    }

    fn visit_while_statement(&mut self, it: &WhileStatement<'a>) {
        let body = self.collect(|s| walk::walk_while_statement(s, it));
        self.emit(Node::Loop { body });
    }

    fn visit_do_while_statement(&mut self, it: &DoWhileStatement<'a>) {
        let body = self.collect(|s| walk::walk_do_while_statement(s, it));
        self.emit(Node::Loop { body });
    }

    fn visit_switch_statement(&mut self, it: &SwitchStatement<'a>) {
        // Visit the discriminant at the switch's own level (matches walk order),
        // then gather each case body.
        let head = self.collect(|s| s.visit_expression(&it.discriminant));
        for node in head {
            self.emit(node);
        }
        let mut cases = Vec::new();
        for case in &it.cases {
            let body = self.collect(|s| {
                if let Some(test) = &case.test {
                    s.visit_expression(test);
                }
                for stmt in &case.consequent {
                    s.visit_statement(stmt);
                }
            });
            cases.push(SwitchCase {
                is_default: case.test.is_none(),
                body,
            });
        }
        self.emit(Node::Switch { cases });
    }

    fn visit_catch_clause(&mut self, it: &CatchClause<'a>) {
        let body = self.collect(|s| walk::walk_catch_clause(s, it));
        self.emit(Node::Catch { body });
    }

    fn visit_break_statement(&mut self, it: &BreakStatement<'a>) {
        self.emit(Node::Jump {
            labeled: it.label.is_some(),
        });
    }

    fn visit_continue_statement(&mut self, it: &ContinueStatement<'a>) {
        self.emit(Node::Jump {
            labeled: it.label.is_some(),
        });
    }

    fn visit_logical_expression(&mut self, it: &LogicalExpression<'a>) {
        let mut operands = Vec::new();
        self.collect_logical_operands(it, it.operator, &mut operands);
        self.emit(Node::Logical {
            op: map_logical_op(it.operator),
            operands,
        });
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        self.emit(Node::Call {
            callee: callee_name(&it.callee),
        });
        walk::walk_call_expression(self, it);
    }
}

fn map_logical_op(op: LogicalOperator) -> LogicalOp {
    match op {
        LogicalOperator::And => LogicalOp::And,
        LogicalOperator::Or => LogicalOp::Or,
        LogicalOperator::Coalesce => LogicalOp::Coalesce,
    }
}

/// Best-effort name of a property key (identifier, private, or string literal).
fn prop_key_name(key: &PropertyKey) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.as_str().to_string()),
        PropertyKey::PrivateIdentifier(id) => Some(format!("#{}", id.name.as_str())),
        PropertyKey::StringLiteral(s) => Some(s.value.as_str().to_string()),
        _ => None,
    }
}

fn binding_name(pat: &BindingPattern) -> Option<String> {
    pat.get_identifier_name().map(|a| a.to_string())
}

fn is_function_like(e: &Expression) -> bool {
    matches!(
        e,
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
    )
}

/// Name of a directly-called callee (`foo()` or `obj.foo()`), used for recursion.
fn callee_name(callee: &Expression) -> Option<String> {
    match callee {
        Expression::Identifier(id) => Some(id.name.as_str().to_string()),
        Expression::StaticMemberExpression(m) => Some(m.property.name.as_str().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.ts"), src)
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

    #[test]
    fn sonar_sum_of_primes_is_7() {
        let src = r#"
            function sumOfPrimes(max) {
                let total = 0;
                OUT: for (let i = 1; i <= max; ++i) {
                    for (let j = 2; j < i; ++j) {
                        if (i % j === 0) {
                            continue OUT;
                        }
                    }
                    total += i;
                }
                return total;
            }
        "#;
        assert_eq!(cognitive_of(src, "sumOfPrimes"), 7);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
            function getWords(number) {
                switch (number) {
                    case 1: return "one";
                    case 2: return "a couple";
                    default: return "lots";
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "getWords"), 1);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            function f(a, b, c) {
                if (a) { if (b) { if (c) {} } }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 6);
    }

    #[test]
    fn else_if_else_are_flat() {
        let src = r#"
            function f(a, b) {
                if (a) {} else if (b) {} else {}
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences() {
        let src = r#"
            function f(a, b, c, d) {
                if (a && b && c || d) {}
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn logical_nested_in_call_is_separate_sequence() {
        let src = r#"
            function f(a, b, x, y) {
                if (a && g(x && y)) {}
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn recursion_adds_one() {
        let src = r#"
            function fib(n) {
                if (n < 2) return n;
                return fib(n - 1) + fib(n - 2);
            }
        "#;
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn nested_function_is_independent_unit() {
        let src = r#"
            function outer() {
                function inner() { if (x) {} }
            }
        "#;
        assert_eq!(cognitive_of(src, "outer"), 0);
        assert_eq!(cognitive_of(src, "inner"), 1);
    }

    #[test]
    fn cyclomatic_basic() {
        let src = r#"
            function f(a, b) {
                if (a && b) { for (;;) {} } else if (b) {}
                try {} catch (e) {}
            }
        "#;
        let r = analyze(src);
        assert_eq!(find(&r.functions, "f").unwrap().cyclomatic, 6);
    }

    #[test]
    fn cyclomatic_switch_cases() {
        let src = r#"
            function f(n) {
                switch (n) { case 1: break; case 2: break; default: break; }
            }
        "#;
        let r = analyze(src);
        assert_eq!(find(&r.functions, "f").unwrap().cyclomatic, 3);
    }

    #[test]
    fn names_methods_and_arrows() {
        let src = r#"
            const add = (a, b) => a + b;
            class C { method() {} get x() { return 1; } }
            const obj = { foo() {}, bar: () => {} };
        "#;
        let r = analyze(src);
        assert_eq!(find(&r.functions, "add").unwrap().kind, "arrow");
        assert_eq!(find(&r.functions, "method").unwrap().kind, "method");
        assert_eq!(find(&r.functions, "x").unwrap().kind, "getter");
        assert!(find(&r.functions, "foo").is_some());
        assert!(find(&r.functions, "bar").is_some());
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
            function a() { if (x) {} }
            function b() { if (x) {} }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn nested_functions_appear_as_children() {
        let src = r#"
            function outer() { function inner() {} }
        "#;
        let r = analyze(src);
        let outer = find(&r.functions, "outer").unwrap();
        assert_eq!(outer.children.len(), 1);
        assert_eq!(outer.children[0].name, "inner");
    }

    #[test]
    fn parse_error_is_reported() {
        // A hard syntax error makes oxc bail on the whole file (empty program),
        // so nothing is measured — but the diagnostic must still be surfaced.
        let (nodes, errors) = to_ir(Path::new("bad.ts"), "function ok() {}\nfunction bad( {\n");
        assert!(nodes.is_empty());
        assert!(!errors.is_empty());
    }
}
