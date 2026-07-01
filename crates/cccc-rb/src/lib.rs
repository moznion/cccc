//! Ruby adapter: parses source with [ruby-prism](https://docs.rs/ruby-prism)
//! (the Rust binding to Ruby's official Prism parser) and lowers the AST into the
//! language-agnostic [`cccc_core::ir`].
//!
//! Unlike the other adapters this one is **not** pure Rust: `ruby-prism` is an
//! FFI binding to the vendored Prism C source, so building this crate needs a C99
//! compiler and libclang (bindgen). In exchange we get Ruby's canonical parser
//! and its [`Visit`] full-traversal trait, which we drive lowering from — exactly
//! as `cccc-php` drives lowering from `php-ast`'s visitor.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about (`def`/blocks/lambdas, `if`/`elsif`/`else`/`unless`,
//! ternary, `while`/`until`/`for`, `case`/`when` and `case`/`in`, `rescue`,
//! `&&`/`and`/`||`/`or`, calls) and emits the matching IR nodes. All complexity
//! rules live in [`cccc_core::engine`].
//!
//! ## Lowering strategy
//!
//! Prism's [`Visit`] trait has one `visit_*_node` method per node type, each with
//! a default that walks every child. We override only the structural nodes and
//! assemble the IR with a stack of "collectors" ([`Builder::collect`]); anything
//! we don't override is reached by the default walk, so a construct in an
//! unexpected position (a block in an argument, an operator inside an index) is
//! never silently missed.
//!
//! ## Ruby-to-IR mapping notes
//!
//! - `def` (incl. `def self.m`) → [`Node::Function`] (`"method"`); a block
//!   (`{ }` / `do…end`) → `"block"`; a `-> { }` lambda → `"lambda"`. Each is its
//!   own unit (nesting resets), matching how the ES/PHP adapters treat
//!   arrows/closures. A block borrows the name of the method it is passed to
//!   (`xs.each { }` → `each`).
//! - `if` / `elsif` / `else` and `unless` → [`Node::Branch`] (chaining `elsif` as
//!   a nested `Branch` so it scores flat). The ternary `a ? b : c` is a keyword-
//!   less `IfNode`, lowered to [`Node::Conditional`] so its `else` is not a second
//!   increment.
//! - `while` / `until` / `for` (and their modifier forms) → [`Node::Loop`].
//! - `case`/`when` and `case`/`in` (pattern matching) → [`Node::Switch`]; the
//!   `else` arm is not a cyclomatic decision point.
//! - `rescue` clauses (and the modifier `x rescue y`) → [`Node::Catch`]; the
//!   `begin`/`else`/`ensure` bodies score at the surrounding level.
//! - `&&` / `and` / `||` / `or` runs → folded [`Node::Logical`] (one node per
//!   like-operator run; `and`/`&&` normalize to the same operator, as do
//!   `or`/`||`). Ruby has no `??`.
//! - calls → [`Node::Call`] for recursion detection (`m()`, `obj.m()`,
//!   `self.m()` all yield `Some("m")`).
//!
//! Ruby has no labelled `break`/`next`, so those never produce a cognitive point
//! and are left to the default walk. Method-based iteration (`loop { }`,
//! `n.times { }`) is modelled as a block unit, not a `Loop`, since only the
//! syntactic loop keywords are recognized — the same syntax-only stance the other
//! adapters take.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use ruby_prism::{
    AndNode, BeginNode, BlockNode, CallNode, CaseMatchNode, CaseNode, DefNode, ForNode, IfNode,
    LambdaNode, Node as PrismNode, OrNode, RescueModifierNode, StatementsNode, UnlessNode,
    UntilNode, Visit, WhileNode, parse,
};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["rb"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR use
/// [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any parser error messages. Prism is fault-tolerant: it always
/// yields a (possibly partial) AST, so we lower whatever it recovered and surface
/// the diagnostics alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let result = parse(source.as_bytes());
    let parse_errors: Vec<String> = result.errors().map(|e| e.message().to_string()).collect();

    let mut builder = Builder::new(source);
    let root = result.node();
    builder.visit(&root);
    (builder.finish(), parse_errors)
}

/// Assembles the IR tree while Prism's [`Visit`] trait drives a complete AST
/// traversal.
struct Builder {
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
    /// Byte offset of the start of each 1-based line, for offset → line lookup.
    line_starts: Vec<u32>,
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
            stack: vec![Vec::new()], // module-level collector
            line_starts,
        }
    }

    /// 1-based line containing byte offset `pos`.
    fn line(&self, pos: usize) -> u32 {
        self.line_starts
            .partition_point(|&start| start as usize <= pos) as u32
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

    /// Visit every statement of a `StatementsNode` body (if present).
    fn visit_stmts(&mut self, stmts: &Option<StatementsNode<'_>>) {
        if let Some(stmts) = stmts {
            for s in &stmts.body() {
                self.visit(&s);
            }
        }
    }

    // ---- branches ---------------------------------------------------------

    /// Lower an `if` (or `elsif`) node to a `Branch`.
    fn lower_if(&mut self, node: &IfNode) -> Node {
        let test = self.collect(|b| b.visit(&node.predicate()));
        let then = self.collect(|b| b.visit_stmts(&node.statements()));
        let alternate = self.lower_subsequent(node.subsequent());
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// Fold an `if` node's `subsequent` (the `elsif`/`else` tail) into the outer
    /// branch's `alternate`: an `elsif` is a nested `Branch` (so it scores flat),
    /// a plain `else` becomes a `Group`.
    fn lower_subsequent(&mut self, subsequent: Option<PrismNode>) -> Option<Box<Node>> {
        let node = subsequent?;
        if let Some(elsif) = node.as_if_node() {
            Some(Box::new(self.lower_if(&elsif)))
        } else if let Some(els) = node.as_else_node() {
            let stmts = els.statements();
            Some(Box::new(Node::Group(
                self.collect(|b| b.visit_stmts(&stmts)),
            )))
        } else {
            // Defensive: any other tail shape scores as a flat `else`.
            Some(Box::new(Node::Group(self.collect(|b| b.visit(&node)))))
        }
    }
}

impl<'pr> Visit<'pr> for Builder {
    fn visit_if_node(&mut self, node: &IfNode<'pr>) {
        // A keyword-less `IfNode` is the ternary `a ? b : c`: model it as a
        // `Conditional` so the `:` branch is not counted as a second `else`.
        if node.if_keyword_loc().is_none() {
            let test = self.collect(|b| b.visit(&node.predicate()));
            let then = self.collect(|b| b.visit_stmts(&node.statements()));
            let alternate = self.collect(|b| {
                if let Some(sub) = node.subsequent() {
                    if let Some(els) = sub.as_else_node() {
                        let stmts = els.statements();
                        b.visit_stmts(&stmts);
                    } else {
                        b.visit(&sub);
                    }
                }
            });
            self.emit(Node::Conditional {
                test,
                then,
                alternate,
            });
        } else {
            let branch = self.lower_if(node);
            self.emit(branch);
        }
    }

    fn visit_unless_node(&mut self, node: &UnlessNode<'pr>) {
        let test = self.collect(|b| b.visit(&node.predicate()));
        let then = self.collect(|b| b.visit_stmts(&node.statements()));
        let else_clause = node.else_clause();
        let alternate = else_clause.map(|els| {
            let stmts = els.statements();
            Box::new(Node::Group(self.collect(|b| b.visit_stmts(&stmts))))
        });
        self.emit(Node::Branch {
            test,
            then,
            alternate,
        });
    }

    fn visit_while_node(&mut self, node: &WhileNode<'pr>) {
        let body = self.collect(|b| {
            b.visit(&node.predicate());
            b.visit_stmts(&node.statements());
        });
        self.emit(Node::Loop { body });
    }

    fn visit_until_node(&mut self, node: &UntilNode<'pr>) {
        let body = self.collect(|b| {
            b.visit(&node.predicate());
            b.visit_stmts(&node.statements());
        });
        self.emit(Node::Loop { body });
    }

    fn visit_for_node(&mut self, node: &ForNode<'pr>) {
        let body = self.collect(|b| {
            b.visit(&node.collection());
            b.visit_stmts(&node.statements());
        });
        self.emit(Node::Loop { body });
    }

    fn visit_case_node(&mut self, node: &CaseNode<'pr>) {
        // The subject runs at the case's own level, before its `when` arms.
        if let Some(pred) = node.predicate() {
            self.visit(&pred);
        }
        let mut cases = Vec::new();
        for cond in &node.conditions() {
            if let Some(when) = cond.as_when_node() {
                let stmts = when.statements();
                let body = self.collect(|b| {
                    for c in &when.conditions() {
                        b.visit(&c);
                    }
                    b.visit_stmts(&stmts);
                });
                cases.push(SwitchCase {
                    is_default: false,
                    body,
                });
            }
        }
        if let Some(els) = node.else_clause() {
            let stmts = els.statements();
            cases.push(SwitchCase {
                is_default: true,
                body: self.collect(|b| b.visit_stmts(&stmts)),
            });
        }
        self.emit(Node::Switch { cases });
    }

    fn visit_case_match_node(&mut self, node: &CaseMatchNode<'pr>) {
        // `case … in …` pattern matching: each `in` clause is a decision point.
        if let Some(pred) = node.predicate() {
            self.visit(&pred);
        }
        let mut cases = Vec::new();
        for cond in &node.conditions() {
            if let Some(in_clause) = cond.as_in_node() {
                let stmts = in_clause.statements();
                let body = self.collect(|b| {
                    b.visit(&in_clause.pattern());
                    b.visit_stmts(&stmts);
                });
                cases.push(SwitchCase {
                    is_default: false,
                    body,
                });
            }
        }
        if let Some(els) = node.else_clause() {
            let stmts = els.statements();
            cases.push(SwitchCase {
                is_default: true,
                body: self.collect(|b| b.visit_stmts(&stmts)),
            });
        }
        self.emit(Node::Switch { cases });
    }

    fn visit_begin_node(&mut self, node: &BeginNode<'pr>) {
        // The `begin` body, `else`, and `ensure` all run at the surrounding
        // level; only each `rescue` clause is a decision point.
        self.visit_stmts(&node.statements());
        let mut rescue = node.rescue_clause();
        while let Some(clause) = rescue {
            let stmts = clause.statements();
            let body = self.collect(|b| {
                for ex in &clause.exceptions() {
                    b.visit(&ex);
                }
                b.visit_stmts(&stmts);
            });
            self.emit(Node::Catch { body });
            rescue = clause.subsequent();
        }
        if let Some(els) = node.else_clause() {
            let stmts = els.statements();
            self.visit_stmts(&stmts);
        }
        if let Some(ens) = node.ensure_clause() {
            let stmts = ens.statements();
            self.visit_stmts(&stmts);
        }
    }

    fn visit_rescue_modifier_node(&mut self, node: &RescueModifierNode<'pr>) {
        // `x rescue y`: `x` runs at the surrounding level, `y` is the handler.
        self.visit(&node.expression());
        let body = self.collect(|b| b.visit(&node.rescue_expression()));
        self.emit(Node::Catch { body });
    }

    fn visit_and_node(&mut self, node: &AndNode<'pr>) {
        let mut operands = Vec::new();
        collect_logical(self, node.as_node(), LogicalOp::And, &mut operands);
        self.emit(Node::Logical {
            op: LogicalOp::And,
            operands,
        });
    }

    fn visit_or_node(&mut self, node: &OrNode<'pr>) {
        let mut operands = Vec::new();
        collect_logical(self, node.as_node(), LogicalOp::Or, &mut operands);
        self.emit(Node::Logical {
            op: LogicalOp::Or,
            operands,
        });
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let name = constant_name(node.name().as_slice(), "<method>");
        let line = self.line(node.location().start_offset());
        let body = node.body();
        self.emit_function(name, "method", line, |b| {
            if let Some(body) = &body {
                b.visit(body);
            }
        });
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        // Reached only for blocks not attached to a plain call (e.g. `super`);
        // call-attached blocks are handled in `visit_call_node` so they can
        // borrow the method name.
        let line = self.line(node.location().start_offset());
        let body = node.body();
        self.emit_function("<block>".to_string(), "block", line, |b| {
            if let Some(body) = &body {
                b.visit(body);
            }
        });
    }

    fn visit_lambda_node(&mut self, node: &LambdaNode<'pr>) {
        let line = self.line(node.location().start_offset());
        let body = node.body();
        self.emit_function("<lambda>".to_string(), "lambda", line, |b| {
            if let Some(body) = &body {
                b.visit(body);
            }
        });
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let name = constant_name(node.name().as_slice(), "<call>");
        // Safe-navigation (`a&.b`) is still a call for recursion purposes.
        self.emit(Node::Call {
            callee: Some(name.clone()),
        });

        // Manually walk the children so we can turn a `{ }` / `do…end` block into
        // its own `Function` unit named after this method (and not double-visit).
        if let Some(recv) = node.receiver() {
            self.visit(&recv);
        }
        if let Some(args) = node.arguments() {
            for a in &args.arguments() {
                self.visit(&a);
            }
        }
        if let Some(block) = node.block() {
            if let Some(block) = block.as_block_node() {
                let line = self.line(block.location().start_offset());
                let body = block.body();
                self.emit_function(name, "block", line, |b| {
                    if let Some(body) = &body {
                        b.visit(body);
                    }
                });
            } else {
                // A `&blk` block argument: just an expression to recurse into.
                self.visit(&block);
            }
        }
    }
}

/// Flatten a run of like logical operators into `operands`. A same-operator
/// child recurses (folding `a && b && c` into one run); a different operator
/// nests as its own `Logical`; any other expression becomes a `Group` of its
/// sub-nodes. Single-statement parentheses are transparent (`a && (b && c)`
/// folds to one run).
fn collect_logical<'pr>(
    builder: &mut Builder,
    node: PrismNode<'pr>,
    op: LogicalOp,
    operands: &mut Vec<Node>,
) {
    let node = unwrap_parens(node);
    match logical_of(&node) {
        Some((side_op, left, right)) if side_op == op => {
            collect_logical(builder, left, op, operands);
            collect_logical(builder, right, op, operands);
        }
        Some((side_op, left, right)) => {
            let mut sub = Vec::new();
            collect_logical(builder, left, side_op, &mut sub);
            collect_logical(builder, right, side_op, &mut sub);
            operands.push(Node::Logical {
                op: side_op,
                operands: sub,
            });
        }
        None => {
            let nodes = builder.collect(|b| b.visit(&node));
            operands.push(Node::Group(nodes));
        }
    }
}

/// The normalized operator and operands of a logical node (`&&`/`and` → `And`,
/// `||`/`or` → `Or`), if it is one.
fn logical_of<'pr>(node: &PrismNode<'pr>) -> Option<(LogicalOp, PrismNode<'pr>, PrismNode<'pr>)> {
    if let Some(n) = node.as_and_node() {
        Some((LogicalOp::And, n.left(), n.right()))
    } else {
        node.as_or_node()
            .map(|n| (LogicalOp::Or, n.left(), n.right()))
    }
}

/// Follow single-statement `( … )` wrappers to the inner expression.
fn unwrap_parens<'pr>(node: PrismNode<'pr>) -> PrismNode<'pr> {
    if let Some(parens) = node.as_parentheses_node()
        && let Some(body) = parens.body()
        && let Some(stmts) = body.as_statements_node()
    {
        let mut iter = stmts.body().iter();
        let (first, second) = (iter.next(), iter.next());
        if let (Some(only), None) = (first, second) {
            return unwrap_parens(only);
        }
    }
    node
}

/// A constant/identifier name as UTF-8, or `default` if it is empty/undecodable.
fn constant_name(bytes: &[u8], default: &str) -> String {
    if bytes.is_empty() {
        default.to_string()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.rb"), src)
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
    fn nested_if_adds_nesting() {
        let src = r#"
            def f(a, b, c)
              if a
                if b
                  if c
                  end
                end
              end
            end
        "#;
        // if(+1) + nested if(+2) + nested if(+3) = 6
        assert_eq!(cognitive_of(src, "f"), 6);
        // base 1 + three ifs = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn elsif_else_are_flat() {
        let src = r#"
            def f(a, b)
              if a
              elsif b
              else
              end
            end
        "#;
        // if(+1) + elsif(+1 flat) + else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if + elsif = 3 (else is not a decision point)
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn unless_with_else_scores_like_a_branch() {
        let src = r#"
            def f(a)
              unless a
                1
              else
                2
              end
            end
        "#;
        // unless(+1) + else(+1 flat) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            def f(a, items)
              while a
              end
              for i in items
              end
              until a
              end
            end
        "#;
        // three loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + three loops = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn modifier_while_is_a_loop() {
        let src = r#"
            def f(a)
              x = 0
              x += 1 while a
            end
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn logical_sequences_fold_by_operator() {
        let src = r#"
            def f(a, b, c, d)
              if a && b && c || d
              end
            end
        "#;
        // if(+1) + && run(+1) + || run(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn keyword_and_folds_with_symbol_and() {
        let src = r#"
            def f(a, b, c)
              if a && b and c
              end
            end
        "#;
        // `&&` and `and` are the same normalized operator → one folded run.
        // if(+1) + and-run(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
    }

    #[test]
    fn ternary_is_a_conditional() {
        let src = r#"
            def f(a)
              a ? 1 : 2
            end
        "#;
        // A ternary is a single increment (the `:` is not a second `else`).
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn case_when_scores_like_a_switch() {
        let src = r#"
            def f(x)
              case x
              when 1 then "one"
              when 2 then "a couple"
              else "lots"
              end
            end
        "#;
        // case(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + 2 non-default whens = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn case_in_pattern_match_scores_like_a_switch() {
        let src = r#"
            def f(x)
              case x
              in Integer then 1
              in String then 2
              else 3
              end
            end
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + 2 non-default ins = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn rescue_clause_counts() {
        let src = r#"
            def f
              begin
                risky
              rescue => e
                handle
              end
            end
        "#;
        // rescue(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + rescue 1 = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn implicit_begin_rescue_in_def_counts() {
        let src = r#"
            def f
              risky
            rescue
              handle
            end
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn modifier_rescue_counts() {
        let src = r#"
            def f
              risky rescue nil
            end
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
            def fib(n)
              return n if n < 2
              fib(n - 1) + fib(n - 2)
            end
        "#;
        // modifier if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
        assert_eq!(find(&analyze(src).functions, "fib").unwrap().kind, "method");
    }

    #[test]
    fn method_recursion_via_self_is_detected() {
        let src = r#"
            class S
              def walk(n)
                if n == 0
                  0
                else
                  self.walk(n - 1)
                end
              end
            end
        "#;
        // if(+1) + else(+1) + recursion(+1) = 3
        assert_eq!(cognitive_of(src, "walk"), 3);
        assert_eq!(
            find(&analyze(src).functions, "walk").unwrap().kind,
            "method"
        );
    }

    #[test]
    fn block_is_its_own_unit_named_after_the_method() {
        let src = r#"
            def host(xs)
              xs.each do |x|
                if x && x
                  1
                end
              end
            end
        "#;
        // host owns no structural complexity; the block does.
        assert_eq!(cognitive_of(src, "host"), 0);
        // if(+1) + && run(+1) = 2
        assert_eq!(cognitive_of(src, "each"), 2);
        assert_eq!(find(&analyze(src).functions, "each").unwrap().kind, "block");
    }

    #[test]
    fn lambda_is_its_own_unit() {
        let src = r#"
            def host
              f = ->(x) { x && x }
              f
            end
        "#;
        assert_eq!(cognitive_of(src, "host"), 0);
        // && run(+1) = 1
        assert_eq!(cognitive_of(src, "<lambda>"), 1);
        assert_eq!(
            find(&analyze(src).functions, "<lambda>").unwrap().kind,
            "lambda"
        );
    }

    #[test]
    fn singleton_method_is_a_unit() {
        let src = r#"
            def self.create(x)
              if x
                1
              end
            end
        "#;
        assert_eq!(cognitive_of(src, "create"), 1);
    }

    #[test]
    fn file_total_sums_all_methods() {
        let src = r#"
            def a(x)
              if x
              end
            end
            def b(y)
              if y
              end
            end
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn parse_error_is_reported() {
        // Prism is fault-tolerant: it still yields a (partial) AST but surfaces a
        // diagnostic for the broken input.
        let (_nodes, errors) = to_ir(Path::new("bad.rb"), "def f(\n");
        assert!(!errors.is_empty());
    }
}
