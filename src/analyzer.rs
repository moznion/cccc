//! Cognitive Complexity (SonarSource / G. Ann Campbell) and Cyclomatic
//! Complexity (McCabe) computed in a single AST traversal using oxc's `Visit`.
//!
//! ## Measurement model
//!
//! Every function-like unit (function declaration/expression, arrow, method,
//! getter/setter, constructor) is measured **independently**: its nesting level
//! starts at 0 at its own boundary, and a structural increment is attributed
//! only to the nearest enclosing function. Nested functions therefore do *not*
//! inflate the enclosing function's own score; they are reported as `children`
//! instead. A file's total is module-level code plus every function at every
//! depth (each structural increment counted exactly once).
//!
//! ## Cyclomatic Complexity (McCabe)
//!
//! Base 1 per function; +1 for each `if`/`else if`, ternary, `for`/`for-in`/
//! `for-of`/`while`/`do-while`, `case` (with a test; `default` excluded),
//! `catch`, and each `&&`/`||`/`??` operator.
//!
//! ## Cognitive Complexity (SonarSource)
//!
//! - +1 and +nesting bonus for: `if`, ternary, `switch`, loops, `catch`.
//! - +1 flat (no bonus) for: `else` / `else if`, labelled `break`/`continue`,
//!   each sequence of like logical operators, and recursion (a call to the
//!   nearest enclosing function's own name).
//! - Nesting increases inside: `if`/`else`/ternary/`switch`/loop/`catch` bodies
//!   and nested function bodies.

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::operator::LogicalOperator;
use oxc_syntax::scope::ScopeFlags;

use crate::report::{FileReport, FunctionReport};

/// An in-progress accumulator for one function-like unit (or the module root).
struct Frame {
    name: String,
    kind: &'static str,
    line: u32,
    cognitive: u32,
    cyclomatic: u32,
    nesting: u32,
    children: Vec<FunctionReport>,
}

/// Owns no AST-borrowed data, so it carries no lifetime; the AST lifetime `'a`
/// lives only on the `Visit<'a>` impl below.
struct Analyzer {
    line_starts: Vec<u32>,
    /// `stack[0]` is always the module frame; deeper entries are functions.
    stack: Vec<Frame>,
    /// Operator of the logical expression directly enclosing the current node,
    /// used to collapse runs of like operators into a single cognitive point.
    logical_parent: Option<LogicalOperator>,
    /// Name/kind captured from a declarator/property to label the next function.
    pending_name: Option<String>,
    pending_kind: Option<&'static str>,
}

impl Analyzer {
    fn new(source: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        let module = Frame {
            name: "<module>".to_string(),
            kind: "module",
            line: 1,
            cognitive: 0,
            cyclomatic: 0,
            nesting: 0,
            children: Vec::new(),
        };
        Self {
            line_starts,
            stack: vec![module],
            logical_parent: None,
            pending_name: None,
            pending_kind: None,
        }
    }

    /// 1-based line number for a byte offset.
    fn line(&self, offset: u32) -> u32 {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => (i as u32) + 1,
            Err(i) => i as u32,
        }
    }

    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("stack never empty")
    }

    fn top_nesting(&self) -> u32 {
        self.stack.last().expect("stack never empty").nesting
    }

    fn add_cognitive(&mut self, amount: u32) {
        self.top().cognitive += amount;
    }

    fn add_cyclomatic(&mut self) {
        self.top().cyclomatic += 1;
    }

    fn enter_nesting(&mut self) {
        self.top().nesting += 1;
    }

    fn leave_nesting(&mut self) {
        self.top().nesting -= 1;
    }

    /// Apply the SonarSource structural increment for a construct that nests its
    /// body: +1 plus the current nesting bonus to cognitive, optionally +1 to
    /// cyclomatic, then run `body` with nesting raised by one. Used by every loop
    /// plus `catch` (`add_cyclomatic = true`) and `switch` (`false` — the switch
    /// itself is not a McCabe decision point; its `case`s are).
    fn nested<F: FnOnce(&mut Self)>(&mut self, add_cyclomatic: bool, body: F) {
        let n = self.top_nesting();
        self.add_cognitive(1 + n);
        if add_cyclomatic {
            self.add_cyclomatic();
        }
        self.enter_nesting();
        body(self);
        self.leave_nesting();
    }

    /// Push a function frame, run `body`, then pop and attach it to its parent.
    fn with_function<F: FnOnce(&mut Self)>(
        &mut self,
        name: String,
        kind: &'static str,
        line: u32,
        body: F,
    ) {
        self.stack.push(Frame {
            name,
            kind,
            line,
            cognitive: 0,
            cyclomatic: 1, // McCabe base
            nesting: 0,
            children: Vec::new(),
        });
        let saved_logical = self.logical_parent.take();
        body(self);
        self.logical_parent = saved_logical;
        let frame = self.stack.pop().expect("function frame");
        let report = FunctionReport {
            name: frame.name,
            kind: frame.kind.to_string(),
            line: frame.line,
            cognitive: frame.cognitive,
            cyclomatic: frame.cyclomatic,
            children: frame.children,
        };
        self.top().children.push(report);
    }

    /// Process an `if` alternate, distinguishing `else if` (own decision) from a
    /// plain `else`. Both add a flat cognitive point (no nesting bonus).
    fn visit_alternate<'a>(&mut self, alt: &Option<Statement<'a>>) {
        let Some(stmt) = alt else { return };
        match stmt {
            Statement::IfStatement(elif) => {
                self.add_cognitive(1); // else if (flat)
                self.add_cyclomatic();
                self.visit_expression(&elif.test);
                self.enter_nesting();
                self.visit_statement(&elif.consequent);
                self.leave_nesting();
                self.visit_alternate(&elif.alternate);
            }
            other => {
                self.add_cognitive(1); // else (flat)
                self.enter_nesting();
                self.visit_statement(other);
                self.leave_nesting();
            }
        }
    }

    /// Visit an operand of a logical expression, preserving the operator context
    /// for like-operator runs but resetting it when descending into a
    /// non-logical sub-expression (so a nested chain starts a fresh sequence).
    fn visit_logical_operand<'a>(&mut self, e: &Expression<'a>) {
        match e {
            Expression::LogicalExpression(inner) => self.visit_logical_expression(inner),
            other => {
                let saved = self.logical_parent.take();
                self.visit_expression(other);
                self.logical_parent = saved;
            }
        }
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
    // `BindingPattern`'s inner `kind` is a private phantom in oxc's
    // raw-transfer layout; data is read through accessor methods.
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

impl<'a> Visit<'a> for Analyzer {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let name = it
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .or_else(|| self.pending_name.take())
            .unwrap_or_else(|| "<anonymous>".to_string());
        let kind = self.pending_kind.take().unwrap_or("function");
        let line = self.line(it.span.start);
        self.pending_name = None;
        self.with_function(name, kind, line, |s| walk::walk_function(s, it, flags));
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let name = self
            .pending_name
            .take()
            .unwrap_or_else(|| "<anonymous>".to_string());
        let kind = self.pending_kind.take().unwrap_or("arrow");
        let line = self.line(it.span.start);
        self.with_function(name, kind, line, |s| {
            walk::walk_arrow_function_expression(s, it)
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
        let n = self.top_nesting();
        self.add_cognitive(1 + n);
        self.add_cyclomatic();
        self.visit_expression(&it.test);
        self.enter_nesting();
        self.visit_statement(&it.consequent);
        self.leave_nesting();
        self.visit_alternate(&it.alternate);
    }

    fn visit_conditional_expression(&mut self, it: &ConditionalExpression<'a>) {
        let n = self.top_nesting();
        self.add_cognitive(1 + n);
        self.add_cyclomatic();
        self.visit_expression(&it.test);
        self.enter_nesting();
        self.visit_expression(&it.consequent);
        self.visit_expression(&it.alternate);
        self.leave_nesting();
    }

    fn visit_for_statement(&mut self, it: &ForStatement<'a>) {
        self.nested(true, |s| walk::walk_for_statement(s, it));
    }

    fn visit_for_in_statement(&mut self, it: &ForInStatement<'a>) {
        self.nested(true, |s| walk::walk_for_in_statement(s, it));
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        self.nested(true, |s| walk::walk_for_of_statement(s, it));
    }

    fn visit_while_statement(&mut self, it: &WhileStatement<'a>) {
        self.nested(true, |s| walk::walk_while_statement(s, it));
    }

    fn visit_do_while_statement(&mut self, it: &DoWhileStatement<'a>) {
        self.nested(true, |s| walk::walk_do_while_statement(s, it));
    }

    fn visit_switch_statement(&mut self, it: &SwitchStatement<'a>) {
        self.nested(false, |s| walk::walk_switch_statement(s, it));
    }

    fn visit_switch_case(&mut self, it: &SwitchCase<'a>) {
        if it.test.is_some() {
            self.add_cyclomatic(); // a `case` (not `default`) is a decision point
        }
        walk::walk_switch_case(self, it);
    }

    fn visit_catch_clause(&mut self, it: &CatchClause<'a>) {
        self.nested(true, |s| walk::walk_catch_clause(s, it));
    }

    fn visit_break_statement(&mut self, it: &BreakStatement<'a>) {
        if it.label.is_some() {
            self.add_cognitive(1);
        }
        walk::walk_break_statement(self, it);
    }

    fn visit_continue_statement(&mut self, it: &ContinueStatement<'a>) {
        if it.label.is_some() {
            self.add_cognitive(1);
        }
        walk::walk_continue_statement(self, it);
    }

    fn visit_logical_expression(&mut self, it: &LogicalExpression<'a>) {
        self.add_cyclomatic();
        if self.logical_parent != Some(it.operator) {
            self.add_cognitive(1); // new sequence of like operators
        }
        let saved = self.logical_parent;
        self.logical_parent = Some(it.operator);
        self.visit_logical_operand(&it.left);
        self.visit_logical_operand(&it.right);
        self.logical_parent = saved;
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Some(name) = callee_name(&it.callee)
            && let Some(top) = self.stack.last()
            && top.kind != "module"
            && top.name == name
        {
            self.add_cognitive(1); // recursion
        }
        walk::walk_call_expression(self, it);
    }
}

/// Sum every function (all depths) into the running totals.
fn sum_tree(fns: &[FunctionReport], cog: &mut u32, cyc: &mut u32) {
    for f in fns {
        *cog += f.cognitive;
        *cyc += f.cyclomatic;
        sum_tree(&f.children, cog, cyc);
    }
}

/// Parse `source` (typed by `path`'s extension) and produce its `FileReport`.
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let ret = Parser::new(&allocator, source, source_type).parse();

    let mut analyzer = Analyzer::new(source);
    analyzer.visit_program(&ret.program);
    let module = analyzer.stack.pop().expect("module frame");

    let functions = module.children;
    let mut cognitive = module.cognitive;
    let mut cyclomatic = module.cyclomatic;
    sum_tree(&functions, &mut cognitive, &mut cyclomatic);

    let parse_errors = ret.errors.iter().map(|e| e.to_string()).collect();

    FileReport {
        path: path.display().to_string(),
        cognitive,
        cyclomatic,
        functions,
        parse_errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.ts"), src)
    }

    /// Find a function by name anywhere in the tree.
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
        let r = analyze(src);
        find(&r.functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cognitive
    }

    // --- SonarSource white paper examples -------------------------------------

    #[test]
    fn sonar_sum_of_primes_is_7() {
        // Appendix A of the Cognitive Complexity white paper.
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
        // for(+1) + for(+2 nested) + if(+3 nested) + continue OUT(+1) = 7
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
        // single switch = +1
        assert_eq!(cognitive_of(src, "getWords"), 1);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            function f(a, b, c) {
                if (a) {           // +1
                    if (b) {       // +2
                        if (c) {   // +3
                        }
                    }
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 6);
    }

    #[test]
    fn else_if_else_are_flat() {
        let src = r#"
            function f(a, b) {
                if (a) {            // +1
                } else if (b) {     // +1
                } else {            // +1
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences() {
        let src = r#"
            function f(a, b, c, d) {
                if (a && b && c || d) { }  // if(+1) + (&& seq +1) + (|| seq +1) = 3
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn logical_nested_in_call_is_separate_sequence() {
        let src = r#"
            function f(a, b, x, y) {
                if (a && g(x && y)) { }  // if(+1) + outer && (+1) + inner && (+1) = 3
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn recursion_adds_one() {
        let src = r#"
            function fib(n) {
                if (n < 2) return n;             // +1
                return fib(n - 1) + fib(n - 2);  // +1 +1 recursion
            }
        "#;
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn nested_function_is_independent_unit() {
        let src = r#"
            function outer() {
                function inner() {
                    if (x) {}   // inner: +1 (nesting 0 within inner)
                }
            }
        "#;
        // independent model: outer's own score excludes inner
        assert_eq!(cognitive_of(src, "outer"), 0);
        assert_eq!(cognitive_of(src, "inner"), 1);
    }

    // --- Cyclomatic -----------------------------------------------------------

    #[test]
    fn cyclomatic_basic() {
        let src = r#"
            function f(a, b) {
                if (a && b) {       // if +1, && +1
                    for (;;) {}     // for +1
                } else if (b) {}    // else-if +1
                try {} catch (e) {} // catch +1
            }
        "#;
        let r = analyze(src);
        // base 1 + if 1 + && 1 + for 1 + else-if 1 + catch 1 = 6
        assert_eq!(find(&r.functions, "f").unwrap().cyclomatic, 6);
    }

    #[test]
    fn cyclomatic_switch_cases() {
        let src = r#"
            function f(n) {
                switch (n) {
                    case 1: break;
                    case 2: break;
                    default: break;
                }
            }
        "#;
        let r = analyze(src);
        // base 1 + 2 cases (default excluded) = 3
        assert_eq!(find(&r.functions, "f").unwrap().cyclomatic, 3);
    }

    // --- Naming / structure ---------------------------------------------------

    #[test]
    fn names_methods_and_arrows() {
        let src = r#"
            const add = (a, b) => a + b;
            class C {
                method() {}
                get x() { return 1; }
            }
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
            function a() { if (x) {} }   // 1
            function b() { if (x) {} }   // 1
        "#;
        let r = analyze(src);
        assert_eq!(r.cognitive, 2);
    }

    #[test]
    fn nested_functions_appear_as_children() {
        let src = r#"
            function outer() {
                function inner() {}
            }
        "#;
        let r = analyze(src);
        let outer = find(&r.functions, "outer").unwrap();
        assert_eq!(outer.children.len(), 1);
        assert_eq!(outer.children[0].name, "inner");
    }
}
