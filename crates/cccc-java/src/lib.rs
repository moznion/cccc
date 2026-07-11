//! Java adapter: parses source with the official [tree-sitter]
//! `tree-sitter/tree-sitter-java` grammar and lowers the concrete syntax tree
//! into the language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the Java grammar (whose C source is compiled by `cc`, so unlike `cccc-rb`
//! there is no `libclang`/bindgen requirement), with no CLI machinery. The
//! unified `cccc` binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.java` files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about and emits the matching IR nodes. All complexity rules
//! live in [`cccc_core::engine`].
//!
//! ## Why a `kind()`-dispatch with a full-recursion default
//!
//! tree-sitter does not offer a "walk every child" visitor trait, so lowering is
//! driven by an explicit recursion. To keep the completeness guarantee a visitor
//! would give us (see the warning in `docs/ADDING_A_LANGUAGE.md`), [`Builder::visit`]
//! matches only the node kinds that produce IR and its **default arm recurses
//! into every named child**. Nothing is silently dropped: an unrecognized
//! construct is transparent, and a logical operator or lambda in any position is
//! still reached. The IR tree is assembled with a stack of "collectors":
//! [`Builder::collect`] pushes a fresh child vector, runs a sub-traversal, and
//! pops the nodes it gathered.
//!
//! ## Java-to-IR mapping notes
//!
//! - method (incl. one in an anonymous class or interface `default` method),
//!   constructor, record compact constructor, and lambda → [`Node::Function`].
//!   Static/instance initializer blocks are transparent: their statements score
//!   at the surrounding (module) level, like other non-function class-body code.
//! - `if` statement → [`Node::Branch`] (`else if` — the `alternative` field is
//!   directly an `if_statement` — chains as a nested `Branch` so it scores flat).
//! - ternary `?:` → [`Node::Conditional`].
//! - `switch` statement / expression (colon groups and `->` rules alike) →
//!   [`Node::Switch`]; an arm whose label says `default` (including
//!   `case null, default`) is the non-decision `default` arm.
//! - `for` / enhanced `for` / `while` / `do`-`while` → [`Node::Loop`].
//! - `try` / `try`-with-resources: one [`Node::Catch`] per `catch` clause (the
//!   `try` body, resources, and `finally` run at the surrounding level).
//! - labelled `break L` / `continue L` → [`Node::Jump`] (`labeled: true`); plain
//!   `break` / `continue` score flat. `return` / `throw` / `yield` are transparent.
//! - `&&` / `||` runs → folded [`Node::Logical`] (Java has no `??`-style
//!   coalescing operator).
//! - method invocations (`f(..)`, `obj.m(..)`) → [`Node::Call`] for recursion
//!   detection.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["java"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any syntax-error messages. tree-sitter always yields a tree (it
/// recovers from errors by inserting `ERROR`/`MISSING` nodes), so we still lower
/// what parsed and report the error locations alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load Java grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse Java source".to_string()]);
    };

    let src = source.as_bytes();
    let mut errors = Vec::new();
    collect_errors(tree.root_node(), &mut errors);

    let mut builder = Builder::new(src);
    builder.visit(tree.root_node());
    (builder.finish(), errors)
}

/// Collect the 1-based lines of every `ERROR`/`MISSING` node so a partially
/// parsed file surfaces its syntax problems (deduplicated, order preserved).
fn collect_errors(node: TsNode, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    if node.is_error() || node.is_missing() {
        let msg = format!("syntax error at line {}", node.start_position().row + 1);
        if !out.contains(&msg) {
            out.push(msg);
        }
    }
    for child in node.children(&mut cursor) {
        collect_errors(child, out);
    }
}

/// Assembles the IR tree while an explicit recursion walks the tree-sitter CST.
struct Builder<'a> {
    /// Source bytes, for extracting identifier text.
    src: &'a [u8],
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
}

impl<'a> Builder<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self {
            src,
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

    /// The UTF-8 text of `node`, or `""` if it is not valid UTF-8.
    fn text(&self, node: TsNode) -> &str {
        node.utf8_text(self.src).unwrap_or("")
    }

    /// Recurse into every named child of `node` (skipping `extras`, i.e.
    /// comments — see [`named_children`]). This is the "transparent" step shared
    /// by every arm that carries no score of its own: a fresh cursor walk with
    /// no intermediate `Vec` allocation.
    fn visit_named_children(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !child.is_extra() {
                self.visit(child);
            }
        }
    }

    /// A function-like unit: emit a `Function` whose body walks *all* named
    /// children (so a lambda hiding in a default parameter value is still
    /// reached), scored in its own frame.
    fn emit_function_node(&mut self, name: String, kind: &'static str, node: TsNode) {
        let line = node.start_position().row as u32 + 1;
        let body = self.collect(|b| b.visit_named_children(node));
        self.emit(Node::Function {
            name,
            kind: kind.to_string(),
            line,
            body,
        });
    }

    /// The text of the declaration's `name` field, if present.
    fn name_of(&self, node: TsNode) -> Option<String> {
        node.child_by_field_name("name")
            .map(|c| self.text(c).to_string())
    }

    // ---- traversal --------------------------------------------------------

    fn visit(&mut self, node: TsNode) {
        match node.kind() {
            "method_declaration" => {
                let name = self.name_of(node).unwrap_or_else(|| "<method>".into());
                self.emit_function_node(name, "method", node);
            }
            "constructor_declaration" | "compact_constructor_declaration" => {
                let name = self.name_of(node).unwrap_or_else(|| "<constructor>".into());
                self.emit_function_node(name, "constructor", node);
            }
            "lambda_expression" => self.emit_function_node("<lambda>".into(), "lambda", node),

            "if_statement" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "ternary_expression" => self.visit_ternary(node),
            "switch_expression" => self.visit_switch(node),
            "for_statement" | "enhanced_for_statement" | "while_statement" | "do_statement" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }
            "catch_clause" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Catch { body });
            }
            "break_statement" | "continue_statement" => {
                // `break L;` / `continue L;` carry a label `identifier` child.
                let labeled = named_children(node)
                    .iter()
                    .any(|c| c.kind() == "identifier");
                self.emit(Node::Jump { labeled });
            }

            "binary_expression" => match logical_op_of(node) {
                Some(op) => self.visit_logical(node, op),
                None => self.visit_named_children(node),
            },

            "method_invocation" => self.visit_call(node),

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// Build a `Branch` from an `if_statement` (recursively, so an `else if`
    /// becomes a nested `Branch` and thus scores flat).
    ///
    /// The grammar tags the three parts with fields (`condition`, `consequence`,
    /// `alternative`), so we address them by field rather than by position:
    /// comments can sit between the parts and would shift a positional index.
    fn lower_if(&mut self, node: TsNode) -> Node {
        let field = |name| node.child_by_field_name(name);
        let test = field("condition").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let then = field("consequence").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let alternate = field("alternative").map(|alt| Box::new(self.lower_alternate(alt)));
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// Lower the statement after an `else`. The grammar puts an `else if`'s
    /// `if_statement` directly in the `alternative` field (no block wrapper), so
    /// it chains as a nested `Branch`; anything else is a plain `else` → `Group`.
    fn lower_alternate(&mut self, alt: TsNode) -> Node {
        if alt.kind() == "if_statement" {
            return self.lower_if(alt);
        }
        Node::Group(self.collect(|b| b.visit(alt)))
    }

    /// A ternary `cond ? a : b` becomes a `Conditional` (fields `condition`,
    /// `consequence`, `alternative` in the grammar).
    fn visit_ternary(&mut self, node: TsNode) {
        let field = |name| node.child_by_field_name(name);
        let test = field("condition").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let then = field("consequence").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let alternate =
            field("alternative").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    /// A `switch` (statement or expression — the grammar uses one
    /// `switch_expression` node for both) becomes a `Switch`: one `SwitchCase`
    /// per colon-style `switch_block_statement_group` or arrow-style
    /// `switch_rule`, with `default`-labelled arms marked `is_default`. The
    /// subject expression runs at the switch's own level first. Labels are
    /// visited inside the case body (transparently), so a pattern guard
    /// (`case Integer i when i > 0 && ...`) still contributes its operators.
    fn visit_switch(&mut self, node: TsNode) {
        if let Some(cond) = node.child_by_field_name("condition") {
            self.visit(cond);
        }
        let mut cases = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            for arm in named_children(body) {
                if !matches!(arm.kind(), "switch_block_statement_group" | "switch_rule") {
                    continue;
                }
                let is_default = named_children(arm)
                    .iter()
                    .any(|c| c.kind() == "switch_label" && self.is_default_label(*c));
                let body = self.collect(|b| b.visit_named_children(arm));
                cases.push(SwitchCase { is_default, body });
            }
        }
        self.emit(Node::Switch { cases });
    }

    /// One folded [`Node::Logical`] for a run of like operators (`&&` / `||`).
    /// A different operator nested inside starts a fresh `Logical`.
    fn visit_logical(&mut self, node: TsNode, op: LogicalOp) {
        let mut operands = Vec::new();
        for side in named_children(node) {
            self.collect_logical_side(side, op, &mut operands);
        }
        self.emit(Node::Logical { op, operands });
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    fn collect_logical_side(&mut self, side: TsNode, op: LogicalOp, operands: &mut Vec<Node>) {
        let side = unwrap_parens(side);
        match logical_op_of(side) {
            Some(side_op) => {
                let kids = named_children(side);
                if side_op == op {
                    for k in kids {
                        self.collect_logical_side(k, op, operands);
                    }
                } else {
                    let mut sub = Vec::new();
                    for k in kids {
                        self.collect_logical_side(k, side_op, &mut sub);
                    }
                    operands.push(Node::Logical {
                        op: side_op,
                        operands: sub,
                    });
                }
            }
            None => operands.push(Node::Group(self.collect(|b| b.visit(side)))),
        }
    }

    /// True if a `switch_label` names the `default` arm. The plain `default:` /
    /// `default ->` label has a `default` keyword token child, but in the
    /// `case null, default` combination the grammar parses `default` as an
    /// `identifier` — checking its text is safe because `default` is a reserved
    /// word and can never be a real identifier.
    fn is_default_label(&self, label: TsNode) -> bool {
        let mut cursor = label.walk();
        label.children(&mut cursor).any(|c| {
            c.kind() == "default" || (c.kind() == "identifier" && self.text(c) == "default")
        })
    }

    /// Emit a `Call` (with the invoked method's simple name for recursion
    /// detection — the `name` field covers both `f(..)` and `obj.f(..)`), then
    /// recurse into the receiver and arguments (which may contain further
    /// constructs, e.g. a lambda argument).
    fn visit_call(&mut self, node: TsNode) {
        let callee = self.name_of(node);
        self.emit(Node::Call { callee });
        self.visit_named_children(node);
    }
}

/// The named children of `node` (skipping `extras`), collected into a `Vec` so
/// the caller can index or slice-match them without holding the cursor's borrow.
/// For the common "just recurse into all of them" case use
/// [`Builder::visit_named_children`], which walks with the cursor directly and
/// allocates nothing.
///
/// Comments are `extras` in this grammar: they are named nodes that can appear
/// *between* any two children. We drop them here so slice-shape checks
/// (`unwrap_parens`' single-child unwrap) are not thrown off by an interleaved
/// comment — and because a comment never lowers to IR anyway.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}

/// The normalized logical operator a `binary_expression` represents, if any.
/// Java has no coalescing operator, so only `&&` and `||` qualify.
fn logical_op_of(node: TsNode) -> Option<LogicalOp> {
    if node.kind() != "binary_expression" {
        return None;
    }
    match node.child_by_field_name("operator").map(|o| o.kind()) {
        Some("&&") => Some(LogicalOp::And),
        Some("||") => Some(LogicalOp::Or),
        _ => None,
    }
}

/// Follow a single-child `parenthesized_expression` to the inner expression so
/// `a && (b && c)` folds into one run.
fn unwrap_parens(node: TsNode) -> TsNode {
    if node.kind() == "parenthesized_expression"
        && let [inner] = named_children(node).as_slice()
    {
        return unwrap_parens(*inner);
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("Test.java"), src)
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

    fn parse_errors(src: &str) -> Vec<String> {
        to_ir(Path::new("T.java"), src).1
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        let src = r#"
            class C {
                static int sumOfPrimes(int max) {
                    int total = 0;
                    OUT:
                    for (int i = 2; i <= max; ++i) {
                        for (int j = 2; j < i; ++j) {
                            if (i % j == 0) {
                                continue OUT;
                            }
                        }
                        total += i;
                    }
                    return total;
                }
            }
        "#;
        // for(+1) + nested for(+2) + nested if(+3) + labelled continue(+1) = 7
        assert_eq!(cognitive_of(src, "sumOfPrimes"), 7);
        // base 1 + for + for + if = 4
        assert_eq!(cyclomatic_of(src, "sumOfPrimes"), 4);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
            class C {
                String getWords(int number) {
                    switch (number) {
                        case 1:
                            return "one";
                        case 2:
                            return "a couple";
                        default:
                            return "lots";
                    }
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "getWords"), 1);
        // base 1 + 2 non-default cases = 3
        assert_eq!(cyclomatic_of(src, "getWords"), 3);
    }

    #[test]
    fn arrow_switch_scores_like_colon_switch() {
        let src = r#"
            class C {
                String getWords(int number) {
                    return switch (number) {
                        case 1 -> "one";
                        case 2 -> "a couple";
                        default -> "lots";
                    };
                }
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        assert_eq!(cognitive_of(src, "getWords"), 1);
        assert_eq!(cyclomatic_of(src, "getWords"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            class C {
                void f(boolean a, boolean b, boolean c) {
                    if (a) {
                        if (b) {
                            if (c) {
                            }
                        }
                    }
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 6); // +1 +2 +3
    }

    #[test]
    fn else_if_else_are_flat() {
        let src = r#"
            class C {
                void f(boolean a, boolean b) {
                    if (a) {
                    } else if (b) {
                    } else {
                    }
                }
            }
        "#;
        // if(+1) + else if(+1 flat) + else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if + else if = 3 (else is not a decision point)
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences_fold() {
        let src = r#"
            class C {
                void f(boolean a, boolean b, boolean c, boolean d) {
                    if (a && b && c || d) {
                    }
                }
            }
        "#;
        // if(+1) + && seq(+1) + || seq(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn parenthesized_like_operators_fold_into_one_run() {
        let src = r#"
            class C {
                void f(boolean a, boolean b, boolean c) {
                    if (a && (b && c)) {
                    }
                }
            }
        "#;
        // if(+1) + one folded && run(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + if 1 + (&& 3 operands => +2) = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn ternary_counts_and_nests() {
        let src = r#"
            class C {
                int f(boolean a, boolean b) {
                    return a ? (b ? 1 : 2) : 3;
                }
            }
        "#;
        // outer ternary(+1) + nested ternary(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + two ternaries = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            class C {
                void f(boolean a, int[] items) {
                    while (a) { }
                    for (int i = 0; i < 3; i++) { }
                    for (int x : items) { }
                    do { } while (a);
                }
            }
        "#;
        // four loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 4);
        // base 1 + four loops = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn catch_clauses_count() {
        let src = r#"
            class C {
                void f() {
                    try {
                        risky();
                    } catch (IllegalStateException e) {
                    } catch (RuntimeException | Error e) {
                    } finally {
                        cleanup();
                    }
                }
            }
        "#;
        // two catch clauses, each +1 at nesting 0 (multi-catch is one clause)
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + two catches = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn try_with_resources_body_is_transparent() {
        let src = r#"
            class C {
                void f() {
                    try (AutoCloseable r = open()) {
                        if (ready(r)) {
                        }
                    } catch (Exception e) {
                    }
                }
            }
        "#;
        // the try body runs at the surrounding level, so if(+1) + catch(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn plain_break_and_continue_are_flat() {
        let src = r#"
            class C {
                void f(int n) {
                    for (int i = 0; i < n; i++) {
                        if (i == 1) {
                            continue;
                        }
                        break;
                    }
                }
            }
        "#;
        // for(+1) + if(+2); unlabelled continue/break add nothing
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
            class C {
                int fib(int n) {
                    if (n < 2) {
                        return n;
                    }
                    return fib(n - 1) + fib(n - 2);
                }
            }
        "#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn method_recursion_through_this() {
        let src = r#"
            class C {
                int walk(int n) {
                    if (n == 0) {
                        return 0;
                    }
                    return this.walk(n - 1);
                }
            }
        "#;
        // if(+1) + recursion via this.walk(+1) = 2
        assert_eq!(cognitive_of(src, "walk"), 2);
        assert_eq!(
            find(&analyze(src).functions, "walk").unwrap().kind,
            "method"
        );
    }

    #[test]
    fn constructor_is_its_own_unit() {
        let src = r#"
            class C {
                C(boolean flag) {
                    if (flag) {
                    }
                }
            }
        "#;
        let report = analyze(src);
        let ctor = find(&report.functions, "C").expect("constructor");
        assert_eq!(ctor.kind, "constructor");
        assert_eq!(ctor.cognitive, 1);
    }

    #[test]
    fn lambda_is_its_own_unit() {
        let src = r#"
            class C {
                void host(java.util.List<Integer> items) {
                    items.forEach(x -> {
                        if (x > 0 && x < 10) {
                        }
                    });
                }
            }
        "#;
        // host owns no structural complexity; the lambda does.
        assert_eq!(cognitive_of(src, "host"), 0);
        // if(+1) + && seq(+1) = 2
        assert_eq!(cognitive_of(src, "<lambda>"), 2);
        assert_eq!(
            find(&analyze(src).functions, "<lambda>").unwrap().kind,
            "lambda"
        );
    }

    #[test]
    fn anonymous_class_method_is_a_unit() {
        let src = r#"
            class C {
                Runnable r = new Runnable() {
                    public void run() {
                        if (ready()) {
                        }
                    }
                };
            }
        "#;
        let report = analyze(src);
        let run = find(&report.functions, "run").expect("run");
        assert_eq!(run.kind, "method");
        assert_eq!(run.cognitive, 1);
    }

    #[test]
    fn interface_default_method_is_a_unit() {
        let src = r#"
            interface I {
                default int f(boolean x) {
                    if (x) {
                        return 1;
                    }
                    return 0;
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn record_compact_constructor_is_a_unit() {
        let src = r#"
            record Point(int x, int y) {
                Point {
                    if (x < 0) {
                        throw new IllegalArgumentException();
                    }
                }
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        let report = analyze(src);
        let ctor = find(&report.functions, "Point").expect("compact constructor");
        assert_eq!(ctor.kind, "constructor");
        assert_eq!(ctor.cognitive, 1);
    }

    #[test]
    fn pattern_switch_with_guard_parses_and_scores() {
        let src = r#"
            class C {
                String f(Object o, boolean p) {
                    return switch (o) {
                        case Integer i when i > 0 && p -> "positive int";
                        case String s -> "string";
                        case null, default -> "other";
                    };
                }
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        // switch(+1) + guard && run(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + 2 non-default arms + (&& 2 operands => +1) = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn annotations_do_not_change_score() {
        let annotated = r#"
            class C {
                @Override
                @SuppressWarnings({"unchecked", "deprecation"})
                int f(boolean a, boolean b) {
                    if (a && b) return 1;
                    return 0;
                }
            }
        "#;
        let plain = r#"
            class C {
                int f(boolean a, boolean b) {
                    if (a && b) return 1;
                    return 0;
                }
            }
        "#;
        // if(+1) + &&(+1) = 2 in both
        assert_eq!(cognitive_of(annotated, "f"), 2);
        assert_eq!(cognitive_of(annotated, "f"), cognitive_of(plain, "f"));
    }

    // Comments are `extras`: they can appear between an `if`'s condition and its
    // body. Lowering addresses the parts by field (not by named-child position)
    // so an interleaved comment does not shift the bodies and mis-score.
    #[test]
    fn comment_between_if_parts_does_not_change_score() {
        let plain = "class C { void f(boolean a) { if (a) { if (a) { } } } }";
        let commented = "class C { void f(boolean a) { if (/* c */ a) /* c */ { if (a) { } } } }";
        // if(+1) + nested if(+2) = 3, with or without the comments
        assert_eq!(cognitive_of(plain, "f"), 3);
        assert_eq!(cognitive_of(commented, "f"), cognitive_of(plain, "f"));
    }

    #[test]
    fn file_total_sums_all_methods() {
        let src = r#"
            class C {
                void a(boolean x) {
                    if (x) {
                    }
                }
                void b(boolean y) {
                    if (y) {
                    }
                }
            }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn syntax_error_is_reported() {
        let errors = parse_errors("class C { void f( {");
        assert!(!errors.is_empty());
    }
}
