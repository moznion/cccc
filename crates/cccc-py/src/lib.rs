//! Python adapter: parses source with the official [tree-sitter]
//! `tree-sitter-python` grammar and lowers the concrete syntax tree into the
//! language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the Python grammar (whose C source is compiled by `cc`, so like `cccc-kt`
//! there is no `libclang`/bindgen requirement), with no CLI machinery. The
//! unified `cccc` binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.py`/`.pyi` files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about and emits the matching IR nodes. All complexity rules
//! live in [`cccc_core::engine`].
//!
//! Like `cccc-kt`, lowering is driven by a `kind()`-dispatch whose **default arm
//! recurses into every named child** (tree-sitter has no "walk every child"
//! visitor trait), so an unrecognized construct is transparent and a lambda or
//! operator in an unexpected position (a default parameter value, an index
//! expression) is never silently missed. The IR tree is assembled with a stack
//! of "collectors" ([`Builder::collect`]).
//!
//! ## Python-to-IR mapping notes
//!
//! - `def` (incl. `async def` and decorated definitions) → [`Node::Function`]
//!   (`"method"` when declared directly in a `class` body, else `"function"`);
//!   `lambda` → `"lambda"` (anonymous, named `<lambda>`). Each is its own unit —
//!   nesting resets at the boundary — and decorators are complexity-neutral.
//! - `if` / `elif` / `else` → [`Node::Branch`] (chaining `elif` as a nested
//!   `Branch` so it scores flat). The conditional expression `a if b else c` →
//!   [`Node::Conditional`], so its `else` arm is not a second increment.
//! - `for` / `while` (incl. `async for`) → [`Node::Loop`]. A loop's `else`
//!   clause runs at the surrounding level (it is not a new decision point — it
//!   fires when the loop finishes without `break`).
//! - Comprehensions (`[… for … if …]`, set/dict comprehensions, generator
//!   expressions) are syntactic loops: each `for` clause → [`Node::Loop`] and
//!   each `if` clause → [`Node::Branch`], nested left-to-right with the element
//!   expression innermost — so a comprehension scores exactly like the
//!   written-out loop. The comprehension is not its own function unit.
//! - `match` → [`Node::Switch`]; a bare `case _:` (no guard) is the non-decision
//!   `default` arm. A `case … if guard:` guard adds no branch increment of its
//!   own, but logical operators inside it still count.
//! - `except` / `except*` clauses → [`Node::Catch`]; the `try`, `else`, and
//!   `finally` bodies score at the surrounding level.
//! - `and` / `or` runs → folded [`Node::Logical`] (one node per like-operator
//!   run). Python has no `??`; `not` adds nothing (matching SonarSource).
//! - calls → [`Node::Call`] for recursion detection (`f()`, `obj.f()`,
//!   `self.f()` all yield `Some("f")`).
//!
//! Python has no labelled `break`/`continue`, so [`Node::Jump`] is never emitted
//! (plain jumps score nothing). `with`, `assert`, `raise`, `await`, and the
//! walrus operator are transparent.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given). `.pyi` is
/// a typing stub, parsed by the same grammar.
pub const DEFAULT_EXTS: &[&str] = &["py", "pyi"];

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
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return (
            Vec::new(),
            vec!["failed to load Python grammar".to_string()],
        );
    }
    let Some(tree) = parser.parse(source, None) else {
        return (
            Vec::new(),
            vec!["failed to parse Python source".to_string()],
        );
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
    /// comments). This is the "transparent" step shared by every arm that
    /// carries no score of its own: a fresh cursor walk with no intermediate
    /// `Vec` allocation.
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

    // ---- traversal --------------------------------------------------------

    fn visit(&mut self, node: TsNode) {
        match node.kind() {
            "function_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n).to_string())
                    .unwrap_or_else(|| "<function>".into());
                let kind = if is_class_member(node) {
                    "method"
                } else {
                    "function"
                };
                self.emit_function_node(name, kind, node);
            }
            "lambda" => self.emit_function_node("<lambda>".into(), "lambda", node),

            "if_statement" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "conditional_expression" => self.visit_conditional(node),
            "for_statement" | "while_statement" => self.visit_loop(node),
            "try_statement" => self.visit_try(node),
            "match_statement" => self.visit_match(node),

            "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression" => self.visit_comprehension(node),

            "boolean_operator" => {
                if let Some(op) = logical_op_of(node) {
                    self.visit_logical(node, op);
                } else {
                    self.visit_named_children(node);
                }
            }

            "call" => self.visit_call(node),

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// Build a `Branch` from an `if_statement`. The grammar attaches the
    /// `elif`/`else` tail as a repeated `alternative` field (a run of
    /// `elif_clause`s optionally ending in an `else_clause`); fold it into the
    /// outer branch's `alternate` so each `elif` is a nested `Branch` (scoring
    /// flat) and a plain `else` is a `Group`.
    fn lower_if(&mut self, node: TsNode) -> Node {
        let mut alternatives = Vec::new();
        let mut cursor = node.walk();
        for child in node.children_by_field_name("alternative", &mut cursor) {
            alternatives.push(child);
        }
        let test = node
            .child_by_field_name("condition")
            .map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let then = node
            .child_by_field_name("consequence")
            .map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let alternate = self.lower_alternatives(&alternatives);
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// Fold the remaining `elif_clause`/`else_clause` run: the first `elif`
    /// becomes a nested `Branch` that owns the rest of the run; an
    /// `else_clause` becomes a flat `Group`.
    fn lower_alternatives(&mut self, alternatives: &[TsNode]) -> Option<Box<Node>> {
        let (first, rest) = alternatives.split_first()?;
        if first.kind() == "elif_clause" {
            let test = first
                .child_by_field_name("condition")
                .map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
            let then = first
                .child_by_field_name("consequence")
                .map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
            let alternate = self.lower_alternatives(rest);
            Some(Box::new(Node::Branch {
                test,
                then,
                alternate,
            }))
        } else {
            // An `else_clause` (defensively: anything else) ends the run flat.
            Some(Box::new(Node::Group(
                self.collect(|b| b.visit_named_children(*first)),
            )))
        }
    }

    /// The conditional expression `a if b else c`: a single increment (its
    /// `else` arm is not a second one). The grammar has no fields here; the
    /// named children are `[then, condition, alternate]` in source order.
    fn visit_conditional(&mut self, node: TsNode) {
        let kids = named_children(node);
        if let [then_expr, cond, alt] = kids.as_slice() {
            let test = self.collect(|b| b.visit(*cond));
            let then = self.collect(|b| b.visit(*then_expr));
            let alternate = self.collect(|b| b.visit(*alt));
            self.emit(Node::Conditional {
                test,
                then,
                alternate,
            });
        } else {
            // Defensive: an unexpected shape stays transparent.
            self.visit_named_children(node);
        }
    }

    /// `for` / `while` (incl. `async for`): everything except the `else` clause
    /// scores inside the loop; the `else` clause runs at the surrounding level
    /// (it fires when the loop completes without `break` — no new decision).
    fn visit_loop(&mut self, node: TsNode) {
        let alternative = node.child_by_field_name("alternative");
        let body = self.collect(|b| {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !child.is_extra() && Some(child) != alternative {
                    b.visit(child);
                }
            }
        });
        self.emit(Node::Loop { body });
        if let Some(els) = alternative {
            self.visit_named_children(els);
        }
    }

    /// The `try` body and any `else`/`finally` bodies run at the surrounding
    /// level; each `except`/`except*` clause is a `Node::Catch` decision point.
    fn visit_try(&mut self, node: TsNode) {
        for child in named_children(node) {
            match child.kind() {
                "except_clause" | "except_group_clause" => {
                    let body = self.collect(|b| b.visit_named_children(child));
                    self.emit(Node::Catch { body });
                }
                // The try body (a `block`), `else_clause`, and `finally_clause`
                // are transparent.
                _ => self.visit_named_children(child),
            }
        }
    }

    /// A `match` becomes a `Switch`: one `SwitchCase` per `case_clause`, with a
    /// bare `case _:` (no guard) marked `is_default`. The subject expression(s)
    /// run at the switch's own level first.
    fn visit_match(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        let subjects: Vec<TsNode> = node
            .children_by_field_name("subject", &mut cursor)
            .collect();
        for subject in subjects {
            self.visit(subject);
        }
        let mut cases = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            for clause in named_children(body) {
                if clause.kind() != "case_clause" {
                    continue;
                }
                let is_default = self.is_wildcard_case(clause);
                let body = self.collect(|b| b.visit_named_children(clause));
                cases.push(SwitchCase { is_default, body });
            }
        }
        self.emit(Node::Switch { cases });
    }

    /// True for a catch-all `case _:` — a single `_` pattern with no guard.
    /// (The grammar does not put a field on the patterns; they are the clause's
    /// `case_pattern` children, and a wildcard is the bare token `_`.)
    fn is_wildcard_case(&self, clause: TsNode) -> bool {
        if clause.child_by_field_name("guard").is_some() {
            return false;
        }
        let patterns: Vec<TsNode> = named_children(clause)
            .into_iter()
            .filter(|c| c.kind() == "case_pattern")
            .collect();
        matches!(patterns.as_slice(), [only] if self.text(*only).trim() == "_")
    }

    /// A comprehension scores like the written-out loop: fold its `for`/`if`
    /// clauses left-to-right (each nested in the previous one's body) with the
    /// element expression(s) innermost.
    fn visit_comprehension(&mut self, node: TsNode) {
        let kids = named_children(node);
        let (clauses, elements): (Vec<TsNode>, Vec<TsNode>) = kids
            .into_iter()
            .partition(|c| matches!(c.kind(), "for_in_clause" | "if_clause"));
        self.lower_comp_clauses(&clauses, &elements);
    }

    /// Lower one comprehension clause and nest the rest (then the element
    /// expressions) inside it: `for` → `Loop`, `if` → `Branch` with no `else`.
    fn lower_comp_clauses(&mut self, clauses: &[TsNode], elements: &[TsNode]) {
        match clauses.split_first() {
            None => {
                for e in elements {
                    self.visit(*e);
                }
            }
            Some((first, rest)) if first.kind() == "for_in_clause" => {
                let body = self.collect(|b| {
                    b.visit_named_children(*first);
                    b.lower_comp_clauses(rest, elements);
                });
                self.emit(Node::Loop { body });
            }
            Some((first, rest)) => {
                let test = self.collect(|b| b.visit_named_children(*first));
                let then = self.collect(|b| b.lower_comp_clauses(rest, elements));
                self.emit(Node::Branch {
                    test,
                    then,
                    alternate: None,
                });
            }
        }
    }

    /// One folded [`Node::Logical`] for a run of like operators (`and` / `or`).
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

    /// Emit a `Call` (with the callee's simple name for recursion detection),
    /// then recurse into the callee expression and the arguments (which may
    /// contain lambdas, comprehensions, further calls, …).
    fn visit_call(&mut self, node: TsNode) {
        let callee = node
            .child_by_field_name("function")
            .and_then(|c| self.callee_name(c));
        self.emit(Node::Call { callee });
        self.visit_named_children(node);
    }

    /// Simple name of a directly-called callee (`f(..)`, `obj.f(..)`,
    /// `self.f(..)`), used for recursion detection: the trailing identifier.
    fn callee_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node).to_string()),
            "attribute" => node
                .child_by_field_name("attribute")
                .map(|a| self.text(a).to_string()),
            _ => None,
        }
    }
}

/// True if `node` (a `function_definition`) is declared directly in a `class`
/// body — i.e. it is a method. Decorators wrap the definition in a
/// `decorated_definition`, which is skipped.
fn is_class_member(node: TsNode) -> bool {
    let mut parent = node.parent();
    if parent.map(|p| p.kind()) == Some("decorated_definition") {
        parent = parent.and_then(|p| p.parent());
    }
    match parent {
        Some(block) if block.kind() == "block" => {
            block.parent().map(|p| p.kind()) == Some("class_definition")
        }
        _ => false,
    }
}

/// The named children of `node` (skipping `extras`, i.e. comments), collected
/// into a `Vec` so the caller can index or slice-match them without holding the
/// cursor's borrow. For the common "just recurse into all of them" case use
/// [`Builder::visit_named_children`], which allocates nothing.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}

/// The normalized logical operator a `boolean_operator` node represents, read
/// from its `operator` field (`and` / `or`). Python has no `??`.
fn logical_op_of(node: TsNode) -> Option<LogicalOp> {
    if node.kind() != "boolean_operator" {
        return None;
    }
    match node.child_by_field_name("operator").map(|o| o.kind()) {
        Some("and") => Some(LogicalOp::And),
        Some("or") => Some(LogicalOp::Or),
        _ => None,
    }
}

/// Follow a single-child `parenthesized_expression` to the inner expression so
/// `a and (b and c)` folds into one run.
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
        analyze_source(Path::new("test.py"), src)
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
        // Python has no labelled `continue`, so (as in the Ruby fixture) the
        // flat `else` supplies the 7th cognitive point that the SonarSource
        // original gets from a labelled jump.
        let src = r#"
def sum_of_primes(max):
    total = 0
    for i in range(2, max + 1):
        for j in range(2, i):
            if i % j == 0:
                total += 0
            else:
                total += i
    return total
"#;
        // for(+1) + nested for(+2) + nested if(+3) + else(+1 flat) = 7
        assert_eq!(cognitive_of(src, "sum_of_primes"), 7);
        // base 1 + for + for + if = 4
        assert_eq!(cyclomatic_of(src, "sum_of_primes"), 4);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
def get_words(n):
    match n:
        case 1:
            return "one"
        case 2:
            return "a couple"
        case _:
            return "lots"
"#;
        // match(+1) = 1
        assert_eq!(cognitive_of(src, "get_words"), 1);
        // base 1 + 2 non-default cases = 3
        assert_eq!(cyclomatic_of(src, "get_words"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
def f(a, b, c):
    if a:
        if b:
            if c:
                pass
"#;
        // if(+1) + nested if(+2) + nested if(+3) = 6
        assert_eq!(cognitive_of(src, "f"), 6);
        // base 1 + three ifs = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn elif_else_are_flat() {
        let src = r#"
def f(a, b):
    if a:
        pass
    elif b:
        pass
    else:
        pass
"#;
        // if(+1) + elif(+1 flat) + else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if + elif = 3 (else is not a decision point)
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn nested_construct_inside_elif_gets_the_elif_nesting() {
        let src = r#"
def f(a, b, c):
    if a:
        pass
    elif b:
        if c:
            pass
"#;
        // if(+1) + elif(+1 flat) + if nested in elif(+2) = 4
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn ternary_is_a_conditional() {
        let src = r#"
def f(a):
    return 1 if a else 2
"#;
        // A conditional expression is a single increment.
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
def f(a, items):
    while a:
        pass
    for i in items:
        pass
"#;
        // two loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + two loops = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn loop_else_scores_at_the_surrounding_level() {
        let src = r#"
def f(items, a):
    for i in items:
        pass
    else:
        if a:
            pass
"#;
        // for(+1) + if in the loop-else at nesting 0(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + for + if = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences_fold_by_operator() {
        let src = r#"
def f(a, b, c, d):
    if a and b and c or d:
        pass
"#;
        // if(+1) + and run(+1) + or run(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (and 3 operands => +2) + (or 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn parenthesized_like_operators_fold_into_one_run() {
        let src = r#"
def f(a, b, c):
    return a and (b and c)
"#;
        // one folded `and` run = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + (and 3 operands => +2) = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn match_without_wildcard_has_no_default() {
        let src = r#"
def f(x):
    match x:
        case 1:
            return 1
        case 2:
            return 2
"#;
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + 2 non-default cases = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn guarded_wildcard_is_not_a_default() {
        let src = r#"
def f(x, p):
    match x:
        case _ if p:
            return 1
        case _:
            return 2
"#;
        // match(+1) = 1; the guard itself adds no branch increment
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + the guarded case (the bare `case _` is the default) = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn except_clauses_count() {
        let src = r#"
def f():
    try:
        risky()
    except ValueError:
        pass
    except Exception as e:
        pass
    else:
        ok()
    finally:
        cleanup()
"#;
        // two except clauses, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + two excepts = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn except_group_counts() {
        let src = r#"
def f():
    try:
        risky()
    except* ValueError:
        pass
"#;
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)
"#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
        assert_eq!(
            find(&analyze(src).functions, "fib").unwrap().kind,
            "function"
        );
    }

    #[test]
    fn method_recursion_via_self_is_detected() {
        let src = r#"
class S:
    def walk(self, n):
        if n == 0:
            return 0
        return self.walk(n - 1)
"#;
        // if(+1) + recursion via self.walk(+1) = 2
        assert_eq!(cognitive_of(src, "walk"), 2);
        assert_eq!(
            find(&analyze(src).functions, "walk").unwrap().kind,
            "method"
        );
    }

    #[test]
    fn decorated_method_is_still_a_method() {
        let src = r#"
class S:
    @property
    def value(self):
        if self.raw:
            return self.raw
        return None
"#;
        let report = analyze(src);
        let f = find(&report.functions, "value").expect("value");
        assert_eq!(f.kind, "method");
        // if(+1) = 1 — decorators contribute nothing
        assert_eq!(f.cognitive, 1);
    }

    #[test]
    fn nested_def_is_its_own_unit() {
        let src = r#"
def host(xs):
    def inner(x):
        if x and x:
            return 1
    return inner
"#;
        // host owns no structural complexity; inner does.
        assert_eq!(cognitive_of(src, "host"), 0);
        // if(+1) + and run(+1) = 2, nesting reset inside `inner`
        assert_eq!(cognitive_of(src, "inner"), 2);
        assert_eq!(
            find(&analyze(src).functions, "inner").unwrap().kind,
            "function"
        );
    }

    #[test]
    fn lambda_is_its_own_unit() {
        let src = r#"
def host(xs):
    f = lambda x: x and x
    return f
"#;
        assert_eq!(cognitive_of(src, "host"), 0);
        // and run(+1) = 1
        assert_eq!(cognitive_of(src, "<lambda>"), 1);
        assert_eq!(
            find(&analyze(src).functions, "<lambda>").unwrap().kind,
            "lambda"
        );
    }

    #[test]
    fn lambda_in_default_argument_is_reached() {
        let src = r#"
def host(key=lambda x: x if x else 0):
    return key
"#;
        // the conditional expression lives in the lambda, not in host
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "<lambda>"), 1);
    }

    #[test]
    fn async_def_and_async_for_count() {
        let src = r#"
async def f(items):
    async for i in items:
        if i:
            pass
"#;
        // async for(+1) + nested if(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn comprehension_scores_like_the_written_out_loop() {
        let src = r#"
def f(xs):
    return [x for x in xs if x > 0]
"#;
        // for clause(+1) + nested if clause(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + for + if = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn multi_clause_comprehension_nests_left_to_right() {
        let src = r#"
def f(xss):
    return {x for xs in xss for x in xs}
"#;
        // for(+1) + nested for(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn conditional_inside_comprehension_element_nests() {
        let src = r#"
def f(xs):
    return [x if x else 0 for x in xs]
"#;
        // for(+1) + conditional nested in it(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn plain_break_and_continue_score_nothing() {
        let src = r#"
def f(items):
    for i in items:
        if i:
            continue
        break
"#;
        // for(+1) + nested if(+2) = 3; break/continue add nothing
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn not_operator_scores_nothing() {
        let src = r#"
def f(a, b):
    return not (a and b)
"#;
        // only the folded `and` run counts
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
def a(x):
    if x:
        pass

def b(y):
    if y:
        pass
"#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn module_level_code_counts_toward_the_file() {
        let src = r#"
import os

if os.name == "posix":
    x = 1
"#;
        let report = analyze(src);
        assert_eq!(report.cognitive, 1);
        assert!(report.functions.is_empty());
    }

    #[test]
    fn comment_between_if_parts_does_not_change_score() {
        let plain = "def f(a):\n    if a:\n        if a:\n            pass\n";
        let commented = "def f(a):\n    if a:  # c\n        # c\n        if a:\n            pass\n";
        // if(+1) + nested if(+2) = 3, with or without the comments
        assert_eq!(cognitive_of(plain, "f"), 3);
        assert_eq!(cognitive_of(commented, "f"), cognitive_of(plain, "f"));
    }

    #[test]
    fn parse_error_is_reported() {
        // tree-sitter is fault-tolerant: it still yields a (partial) tree but
        // surfaces the error location for the broken input.
        let (_nodes, errors) = to_ir(Path::new("bad.py"), "def f(:\n");
        assert!(!errors.is_empty());
    }
}
