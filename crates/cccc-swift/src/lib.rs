//! Swift adapter: parses source with the [tree-sitter] `alex-pinkus/tree-sitter-swift`
//! grammar (the `tree-sitter-swift` crate) and lowers the concrete syntax tree
//! into the language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the Swift grammar (whose C source is compiled by `cc`, so unlike `cccc-rb`
//! there is no `libclang`/bindgen requirement), with no CLI machinery. The
//! unified `cccc` binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.swift` files to it.
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
//! construct is transparent, and a logical operator or closure in any position is
//! still reached. The IR tree is assembled with a stack of "collectors":
//! [`Builder::collect`] pushes a fresh child vector, runs a sub-traversal, and
//! pops the nodes it gathered.
//!
//! ## Swift-to-IR mapping notes
//!
//! - `func` declaration / method / local `func`, closures (`lambda_literal`),
//!   `init` / `deinit`, `subscript`, computed-property `get` / `set` (explicit or
//!   implicit getter-only body), and `willSet` / `didSet` observers →
//!   [`Node::Function`].
//! - `if` statement → [`Node::Branch`] (`else if` — an `if_statement` following
//!   the `else` marker — chains as a nested `Branch` so it scores flat).
//!   `guard` … `else` → a [`Node::Branch`] whose `then` is the `else` block, so
//!   it scores exactly like an `if` (one point plus nesting).
//! - `switch` → [`Node::Switch`]; the `default` entry is the non-decision arm
//!   (`case` patterns, `where` clauses, and bodies all run inside their entry).
//! - `for`-`in` (including its `where` clause) / `while` / `repeat`-`while` →
//!   [`Node::Loop`].
//! - `do` / `catch` → one [`Node::Catch`] per `catch` block (the `do` body runs
//!   at the surrounding level).
//! - labelled `break l` / `continue l` → [`Node::Jump`] (`labeled: true`); plain
//!   `break` / `continue` score flat. `return` / `throw` are transparent.
//! - ternary `a ? b : c` → [`Node::Conditional`].
//! - `&&` / `||` runs → folded [`Node::Logical`]; nil-coalescing `??` folds as a
//!   `Coalesce` run (mirroring how `cccc-php` treats `??`).
//! - calls (`f(..)`, `obj.m(..)`) → [`Node::Call`] for recursion detection.
//! - `#if` compilation directives are transparent: every branch's code parses as
//!   ordinary statements and is scored where it stands.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["swift"];

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
        .set_language(&tree_sitter_swift::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load Swift grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse Swift source".to_string()]);
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
    /// comments and `#if` directives — see [`named_children`]). This is the
    /// "transparent" step shared by every arm that carries no score of its own:
    /// a fresh cursor walk with no intermediate `Vec` allocation.
    fn visit_named_children(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !child.is_extra() {
                self.visit(child);
            }
        }
    }

    /// A function-like unit: emit a `Function` whose body walks *all* named
    /// children (so a closure hiding in a default parameter value is still
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

    // ---- traversal --------------------------------------------------------

    fn visit(&mut self, node: TsNode) {
        match node.kind() {
            "function_declaration" => {
                // A `func` declared directly in a class / struct / enum /
                // actor / extension body is a method; anything else (top-level,
                // or local to a function body) is a plain function.
                let kind = if matches!(
                    node.parent().map(|p| p.kind()),
                    Some("class_body" | "enum_class_body")
                ) {
                    "method"
                } else {
                    "function"
                };
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n).to_string())
                    .unwrap_or_else(|| "<function>".into());
                self.emit_function_node(name, kind, node);
            }
            "init_declaration" => self.emit_function_node("<init>".into(), "constructor", node),
            "deinit_declaration" => self.emit_function_node("<deinit>".into(), "deinit", node),
            "lambda_literal" => self.emit_function_node("<closure>".into(), "closure", node),
            "computed_getter" => self.emit_function_node("<getter>".into(), "getter", node),
            "computed_setter" => self.emit_function_node("<setter>".into(), "setter", node),
            "willset_clause" => self.emit_function_node("<willSet>".into(), "observer", node),
            "didset_clause" => self.emit_function_node("<didSet>".into(), "observer", node),
            "subscript_declaration" => {
                self.emit_function_node("<subscript>".into(), "subscript", node)
            }
            "computed_property" => self.visit_computed_property(node),

            "if_statement" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "guard_statement" => self.visit_guard(node),
            "switch_statement" => self.visit_switch(node),
            "for_statement" | "while_statement" | "repeat_while_statement" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }
            "do_statement" => self.visit_do(node),
            "control_transfer_statement" => self.visit_jump(node),
            "ternary_expression" => self.visit_ternary(node),

            "conjunction_expression" => self.visit_logical(node, LogicalOp::And),
            "disjunction_expression" => self.visit_logical(node, LogicalOp::Or),
            "nil_coalescing_expression" => self.visit_logical(node, LogicalOp::Coalesce),

            "call_expression" => self.visit_call(node),

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// A `computed_property` block. Three shapes:
    /// - explicit `get` / `set` accessors → transparent; each accessor emits its
    ///   own `Function` when visited.
    /// - the body of a `subscript` → transparent; the enclosing
    ///   `subscript_declaration` already opened the function frame.
    /// - a bare getter-only body (`var x: T { …statements… }`) → an implicit
    ///   getter, emitted as its own `Function`.
    fn visit_computed_property(&mut self, node: TsNode) {
        let has_accessors = named_children(node)
            .iter()
            .any(|c| matches!(c.kind(), "computed_getter" | "computed_setter"));
        let in_subscript = node
            .parent()
            .is_some_and(|p| p.kind() == "subscript_declaration");
        if has_accessors || in_subscript {
            self.visit_named_children(node);
        } else {
            self.emit_function_node("<getter>".into(), "getter", node);
        }
    }

    /// Build a `Branch` from an `if_statement` (recursively, so an `else if`
    /// becomes a nested `Branch` and thus scores flat).
    ///
    /// The grammar lays an `if_statement`'s named children out linearly:
    /// condition part(s) (there may be several — `if let v = x, cond`), the
    /// `then` block's `statements`, then a named `"else"` marker followed by
    /// either a nested `if_statement` (`else if`) or the else block's
    /// `statements`. Splitting at the marker (rather than counting positions)
    /// stays correct when a block is empty and its `statements` node is absent.
    fn lower_if(&mut self, node: TsNode) -> Node {
        let kids = named_children(node);
        let else_pos = kids.iter().position(|c| c.kind() == "else");
        let (head, tail) = match else_pos {
            Some(p) => (&kids[..p], &kids[p + 1..]),
            None => (&kids[..], &kids[0..0]),
        };

        let (then_block, cond_parts): (Vec<TsNode>, Vec<TsNode>) =
            head.iter().copied().partition(|c| c.kind() == "statements");
        let test = self.collect(|b| {
            for part in &cond_parts {
                b.visit(*part);
            }
        });
        let then = self.collect(|b| {
            for block in &then_block {
                b.visit_named_children(*block);
            }
        });

        let alternate = else_pos.map(|_| {
            if let [only] = tail
                && only.kind() == "if_statement"
            {
                return Box::new(self.lower_if(*only));
            }
            Box::new(Node::Group(self.collect(|b| {
                for t in tail {
                    b.visit(*t);
                }
            })))
        });

        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// `guard <conditions> else { <body> }` scores exactly like an `if`: one
    /// branch whose `then` is the `else` block (the only body a guard has).
    fn visit_guard(&mut self, node: TsNode) {
        let kids = named_children(node);
        let else_pos = kids.iter().position(|c| c.kind() == "else");
        let (head, tail) = match else_pos {
            Some(p) => (&kids[..p], &kids[p + 1..]),
            None => (&kids[..], &kids[0..0]),
        };
        let test = self.collect(|b| {
            for part in head {
                b.visit(*part);
            }
        });
        let then = self.collect(|b| {
            for t in tail {
                b.visit(*t);
            }
        });
        self.emit(Node::Branch {
            test,
            then,
            alternate: None,
        });
    }

    /// A `switch` becomes a `Switch`: one `SwitchCase` per `switch_entry`, with
    /// the `default` entry marked `is_default` (recognized by its
    /// `default_keyword` child). The subject expression runs at the switch's own
    /// level first. A multi-pattern `case a, b:` is still one decision arm.
    fn visit_switch(&mut self, node: TsNode) {
        let mut cases = Vec::new();
        for child in named_children(node) {
            if child.kind() == "switch_entry" {
                let is_default = named_children(child)
                    .iter()
                    .any(|c| c.kind() == "default_keyword");
                let body = self.collect(|b| b.visit_named_children(child));
                cases.push(SwitchCase { is_default, body });
            } else {
                self.visit(child);
            }
        }
        self.emit(Node::Switch { cases });
    }

    /// The `do` body runs at the surrounding level; each `catch` block is a
    /// `Node::Catch` decision point (its pattern and `where` clause included).
    fn visit_do(&mut self, node: TsNode) {
        for child in named_children(node) {
            if child.kind() == "catch_block" {
                let body = self.collect(|b| b.visit_named_children(child));
                self.emit(Node::Catch { body });
            } else {
                self.visit(child);
            }
        }
    }

    /// A labelled `break l` / `continue l` (the label is the `result` field)
    /// scores one flat cognitive point; plain `break` / `continue` do not.
    /// `return` / `throw` carry no jump point but may hold a sub-expression, so
    /// recurse into their children.
    fn visit_jump(&mut self, node: TsNode) {
        let keyword = node.child(0).map(|c| c.kind()).unwrap_or("");
        match keyword {
            "break" | "continue" => {
                self.emit(Node::Jump {
                    labeled: node.child_by_field_name("result").is_some(),
                });
            }
            _ => self.visit_named_children(node),
        }
    }

    /// The ternary `a ? b : c` → `Node::Conditional` (branch-like: +1 plus
    /// nesting; a ternary chained in `if_false` nests and scores accordingly).
    fn visit_ternary(&mut self, node: TsNode) {
        let field = |name| node.child_by_field_name(name);
        let test = field("condition").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let then = field("if_true").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let alternate = field("if_false").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    /// One folded [`Node::Logical`] for a run of like operators (`&&` / `||` /
    /// `??`). A different operator nested inside starts a fresh `Logical`.
    fn visit_logical(&mut self, node: TsNode, op: LogicalOp) {
        let mut operands = Vec::new();
        let kids = named_children(node);
        for side in kids {
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

    /// Emit a `Call` (with the callee's simple name for recursion detection),
    /// then recurse into the callee expression and the call suffix (whose
    /// arguments and trailing closure may contain further constructs).
    fn visit_call(&mut self, node: TsNode) {
        let callee = node.named_child(0).and_then(|c| self.callee_name(c));
        self.emit(Node::Call { callee });
        self.visit_named_children(node);
    }

    /// Simple name of a directly-called callee (`foo(..)` or `obj.foo(..)`),
    /// used for recursion detection. Returns the trailing identifier.
    ///
    /// The grammar attaches a `call_suffix` to the *whole* enclosing binary
    /// expression (`a + fib(n)` parses as `call_expression{additive{a, fib},
    /// suffix}`), so for expression wrappers we follow the **rightmost** named
    /// child — the identifier the `(` textually follows is always the rightmost
    /// leaf of the callee expression.
    fn callee_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "simple_identifier" => Some(self.text(node).to_string()),
            "navigation_expression" => node
                .child_by_field_name("suffix")
                .and_then(|s| s.child_by_field_name("suffix"))
                .filter(|c| c.kind() == "simple_identifier")
                .map(|c| self.text(c).to_string()),
            kind if kind.ends_with("_expression") => named_children(node)
                .last()
                .and_then(|c| self.callee_name(*c)),
            _ => None,
        }
    }
}

/// The named children of `node` (skipping `extras`), collected into a `Vec` so
/// the caller can index or slice-match them without holding the cursor's borrow.
/// For the common "just recurse into all of them" case use
/// [`Builder::visit_named_children`], which walks with the cursor directly and
/// allocates nothing.
///
/// Comments (and `#if` directives) are `extras` in this grammar: they are named
/// nodes that can appear *between* any two children. We drop them here so
/// slice-shape checks (`lower_if`'s `else if` test, `unwrap_parens`'
/// single-child unwrap) are not thrown off by an interleaved comment — and
/// because a comment never lowers to IR anyway.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}

/// The normalized logical operator a node represents, if any.
fn logical_op_of(node: TsNode) -> Option<LogicalOp> {
    match node.kind() {
        "conjunction_expression" => Some(LogicalOp::And),
        "disjunction_expression" => Some(LogicalOp::Or),
        "nil_coalescing_expression" => Some(LogicalOp::Coalesce),
        _ => None,
    }
}

/// Follow a single-child parenthesized expression (the grammar parses `( … )`
/// as a one-element `tuple_expression`) to the inner expression so
/// `a && (b && c)` folds into one run. A real tuple has 2+ elements, so a
/// single-child tuple is always just parentheses.
fn unwrap_parens(node: TsNode) -> TsNode {
    if node.kind() == "tuple_expression"
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
        analyze_source(Path::new("test.swift"), src)
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
        to_ir(Path::new("t.swift"), src).1
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        let src = r#"
            func sumOfPrimes(max: Int) -> Int {
                var total = 0
                outer: for i in 2...max {
                    for j in 2..<i {
                        if i % j == 0 {
                            continue outer
                        }
                    }
                    total += i
                }
                return total
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
            func getWords(n: Int) -> String {
                switch n {
                case 1:
                    return "one"
                case 2:
                    return "a couple"
                default:
                    return "lots"
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "getWords"), 1);
        // base 1 + 2 non-default entries = 3
        assert_eq!(cyclomatic_of(src, "getWords"), 3);
    }

    // `case 2, 3:` is still one decision arm.
    #[test]
    fn multi_pattern_case_is_one_arm() {
        let src = r#"
            func f(n: Int) -> String {
                switch n {
                case 1:
                    return "one"
                case 2, 3:
                    return "a couple"
                default:
                    return "lots"
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            func f(a: Bool, b: Bool, c: Bool) {
                if a {
                    if b {
                        if c {
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
            func f(a: Bool, b: Bool) {
                if a {
                } else if b {
                } else {
                }
            }
        "#;
        // if(+1) + else if(+1 flat) + else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if + else if = 3 (else is not a decision point)
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn guard_scores_like_if() {
        let src = r#"
            func f(s: String?) -> Int {
                guard let v = s else {
                    return 0
                }
                return v.count
            }
        "#;
        // guard(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + guard = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    // A construct inside a guard's else block is nested one level deeper.
    #[test]
    fn guard_body_nests() {
        let src = r#"
            func f(s: String?, a: Bool) -> Int {
                guard let v = s else {
                    if a {
                        return -1
                    }
                    return 0
                }
                return v.count
            }
        "#;
        // guard(+1) + nested if(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences_fold() {
        let src = r#"
            func f(a: Bool, b: Bool, c: Bool, d: Bool) {
                if a && b && c || d {
                }
            }
        "#;
        // if(+1) + && seq(+1) + || seq(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    // Parentheses (a one-element tuple in this grammar) do not split a run.
    #[test]
    fn parenthesized_like_operators_fold() {
        let src = r#"
            func f(a: Bool, b: Bool, c: Bool) -> Bool {
                return (a && b) && c
            }
        "#;
        // one folded && run = +1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + (&& 3 operands => +2) = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn nil_coalescing_counts_as_coalesce() {
        let src = r#"
            func f(a: String?, b: String?) -> String {
                return a ?? b ?? "z"
            }
        "#;
        // one folded ?? run = +1 cognitive
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + (?? has 3 operands => +2) = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn ternary_counts_as_conditional() {
        let src = r#"
            func f(a: Bool) -> Int {
                return a ? 1 : 0
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            func f(a: Bool, items: [Int]) {
                while a { }
                for i in items { }
                repeat { } while a
            }
        "#;
        // three loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + three loops = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    // `for … where …` is one loop; the filter clause adds no extra point but
    // logical operators inside it still count.
    #[test]
    fn for_where_clause_is_transparent() {
        let src = r#"
            func f(items: [Int], a: Bool, b: Bool) {
                for i in items where a && b {
                }
            }
        "#;
        // for(+1) + && seq(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
    }

    #[test]
    fn catch_clauses_count() {
        let src = r#"
            func f() {
                do {
                    try risky()
                } catch is CancellationError {
                } catch {
                    cleanup()
                }
            }
        "#;
        // two catch blocks, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + two catches = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn labeled_break_counts() {
        let src = r#"
            func f(a: Bool) {
                outer: while a {
                    break outer
                }
                while a {
                    break
                }
            }
        "#;
        // while(+1) + labelled break(+1) + while(+1) + plain break(0) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
            func fib(_ n: Int) -> Int {
                if n < 2 {
                    return n
                }
                return fib(n - 1) + fib(n - 2)
            }
        "#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn method_recursion_and_kind() {
        let src = r#"
            class C {
                func walk(_ n: Int) -> Int {
                    if n == 0 {
                        return 0
                    }
                    return self.walk(n - 1)
                }
            }
        "#;
        // if(+1) + recursion via self.walk(+1) = 2
        assert_eq!(cognitive_of(src, "walk"), 2);
        assert_eq!(
            find(&analyze(src).functions, "walk").unwrap().kind,
            "method"
        );
    }

    #[test]
    fn struct_and_enum_members_are_methods() {
        let src = r#"
            struct S {
                func f(x: Bool) -> Int {
                    if x { return 1 }
                    return 0
                }
            }
            enum E {
                case a, b
                func g(x: Bool) -> Int {
                    if x { return 1 }
                    return 0
                }
            }
        "#;
        let report = analyze(src);
        assert_eq!(find(&report.functions, "f").unwrap().kind, "method");
        assert_eq!(find(&report.functions, "g").unwrap().kind, "method");
    }

    #[test]
    fn closure_is_its_own_unit() {
        let src = r#"
            func host(items: [Int]) {
                items.forEach { x in
                    if x > 0 && x < 10 {
                    }
                }
            }
        "#;
        // host owns no structural complexity; the closure does.
        assert_eq!(cognitive_of(src, "host"), 0);
        // if(+1) + && seq(+1) = 2
        assert_eq!(cognitive_of(src, "<closure>"), 2);
        assert_eq!(
            find(&analyze(src).functions, "<closure>").unwrap().kind,
            "closure"
        );
    }

    #[test]
    fn init_and_deinit_are_units() {
        let src = r#"
            class C {
                var x: Int
                init(x: Int, big: Bool) {
                    if big {
                        self.x = x * 2
                    } else {
                        self.x = x
                    }
                }
                deinit {
                    if x > 0 {
                        log()
                    }
                }
            }
        "#;
        // if(+1) + else(+1) = 2
        assert_eq!(cognitive_of(src, "<init>"), 2);
        assert_eq!(cognitive_of(src, "<deinit>"), 1);
    }

    #[test]
    fn accessors_are_units() {
        let src = r#"
            class C {
                var name = ""
                var greeting: String {
                    get {
                        if name.isEmpty {
                            return "hello"
                        }
                        return "hello there"
                    }
                    set {
                        if newValue.isEmpty {
                            return
                        }
                        name = newValue
                    }
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "<getter>"), 1);
        assert_eq!(cognitive_of(src, "<setter>"), 1);
    }

    // A getter-only computed property (`var x: T { … }`, no `get` keyword) is
    // still an implicit getter unit.
    #[test]
    fn implicit_getter_is_a_unit() {
        let src = r#"
            class C {
                var name = ""
                var flag: Int {
                    if name.isEmpty {
                        return 0
                    }
                    return 1
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "<getter>"), 1);
        assert_eq!(
            find(&analyze(src).functions, "<getter>").unwrap().kind,
            "getter"
        );
    }

    #[test]
    fn property_observers_are_units() {
        let src = r#"
            struct S {
                var x = 0 {
                    willSet {
                        if newValue > 100 {
                            log()
                        }
                    }
                    didSet {
                        log()
                    }
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "<willSet>"), 1);
        assert_eq!(cognitive_of(src, "<didSet>"), 0);
    }

    #[test]
    fn subscript_is_a_unit() {
        let src = r#"
            struct S {
                let xs: [Int]
                subscript(i: Int) -> Int {
                    if i < 0 {
                        return 0
                    }
                    return xs[i]
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "<subscript>"), 1);
        assert_eq!(cyclomatic_of(src, "<subscript>"), 2);
    }

    // `if let v = x, cond` is one branch; extra comma-separated conditions add
    // nothing, but logical operators inside them still count.
    #[test]
    fn if_let_with_conditions() {
        let src = r#"
            func f(x: Int?, a: Bool, b: Bool) {
                if let v = x, a && b {
                    use(v)
                }
            }
        "#;
        // if(+1) + && seq(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
    }

    // `if case` pattern matching is a plain branch.
    #[test]
    fn if_case_is_a_branch() {
        let src = r#"
            enum E { case a, b }
            func f(e: E) {
                if case .a = e {
                    log()
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    // `#if` compilation directives are transparent; the guarded code parses and
    // scores as ordinary statements.
    #[test]
    fn compiler_directives_are_transparent() {
        let src = r#"
            func f(x: Int?) {
                #if DEBUG
                if x == nil {
                    log()
                }
                #endif
            }
        "#;
        assert!(
            parse_errors(src).is_empty(),
            "errors: {:?}",
            parse_errors(src)
        );
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    // async/await/try? wrappers are complexity-neutral.
    #[test]
    fn async_await_try_are_transparent() {
        let src = r#"
            func f(g: () async throws -> Int, a: Bool) async {
                if a {
                    let r = try? await g()
                    print(r ?? -1)
                }
            }
        "#;
        assert!(
            parse_errors(src).is_empty(),
            "errors: {:?}",
            parse_errors(src)
        );
        // if(+1) + ?? run(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
    }

    // Comments between an `if`'s parts must not change the score.
    #[test]
    fn comment_between_if_parts_does_not_change_score() {
        let plain = "func f(a: Bool) { if a { if a { } } }";
        let commented = "func f(a: Bool) { if /* c */ a /* c */ { if a { } } }";
        // if(+1) + nested if(+2) = 3, with or without the comments
        assert_eq!(cognitive_of(plain, "f"), 3);
        assert_eq!(cognitive_of(commented, "f"), cognitive_of(plain, "f"));
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
            func a(x: Bool) {
                if x {
                }
            }
            func b(y: Bool) {
                if y {
                }
            }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn syntax_errors_are_reported() {
        let errors = parse_errors("func f( {");
        assert!(!errors.is_empty());
    }
}
