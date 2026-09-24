//! Scala adapter: parses source with the official [tree-sitter]
//! `tree-sitter/tree-sitter-scala` grammar and lowers the CST into the
//! language-agnostic [`cccc_core::ir`]. Pure library, no CLI machinery; the
//! grammar's C source is compiled by `cc`, so no libclang/bindgen. The `cccc`
//! binary registers this adapter's [`analyze_source`]/[`DEFAULT_EXTS`] and
//! dispatches `.scala`/`.sc` to it.
//!
//! No scoring logic lives here (that is [`cccc_core::engine`]). [`Builder::visit`]
//! matches only the node kinds that produce IR; its default arm recurses into
//! every named child, so an unrecognized construct is transparent and nothing —
//! a logical operator or lambda in any position — is silently dropped. The IR is
//! assembled with a stack of collectors ([`Builder::collect`]).
//!
//! ## Scala-to-IR mapping
//!
//! - `def` (and bodyless abstract `def`) and `x => …` → [`Node::Function`]. A
//!   secondary constructor `def this(..)` is reported as a `<constructor>` unit
//!   (as in the Kotlin/Swift adapters), so no unit is named `this` and its
//!   mandatory `this(..)` self-delegation is not mistaken for recursion.
//! - `if` → [`Node::Branch`] (`else if` chains as a nested `Branch`, scored flat).
//! - `match` → [`Node::Switch`]; a `case _ =>` or lowercase variable pattern
//!   (`case other =>`) is the non-decision `default` arm, an uppercase stable-id
//!   (`case None =>`) is not. A guard (`case x if a && b =>`) is transparent: its
//!   operators count, the guard itself is not a decision.
//! - a partial-function literal (`xs.collect { case … }`) → an anonymous
//!   [`Node::Function`] wrapping a [`Node::Switch`], like a lambda. `match` and
//!   `catch` reuse the same `case_block` node via their own paths, so only a
//!   genuine partial function lands here.
//! - `for` / `while` / `do`-`while` → [`Node::Loop`]. Scala has no `break` /
//!   `continue` (nor labelled loops), so no [`Node::Jump`]; the library escapes
//!   (`scala.util.control.Breaks`, Scala 3's `boundary`) are plain calls and stay
//!   transparent.
//! - `try` / `catch` / `finally`: one [`Node::Catch`] per `catch` clause (its
//!   `case` handlers score inside it; the `try` body and `finally` run outside).
//! - `&&` / `||` runs → folded [`Node::Logical`] (Scala has no `??`).
//! - calls (`f(..)`, `obj.m(..)`) → [`Node::Call`] for recursion detection. An
//!   infix-style call (`a foo b`) is not detected: treating every non-logical
//!   infix as a call would misflag operator methods (`this.value < o` in `def <`).

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["scala", "sc"];

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
        .set_language(&tree_sitter_scala::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load Scala grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse Scala source".to_string()]);
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

    /// Recurse into every named child (skipping `extras`, i.e. comments). The
    /// "transparent" step for arms that carry no score of their own.
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
            "function_definition" | "function_declaration" => match self.name_of(node) {
                // `def this(..)` is a secondary constructor (see module docs).
                Some(name) if name == "this" => {
                    self.emit_function_node("<constructor>".into(), "constructor", node);
                }
                Some(name) => self.emit_function_node(name, "method", node),
                None => self.emit_function_node("<def>".into(), "method", node),
            },
            "lambda_expression" => self.emit_function_node("<lambda>".into(), "lambda", node),

            "if_expression" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "match_expression" => self.visit_match(node),
            // A `case_block`/`indented_cases` is a `match` body, a `catch`'s
            // handlers, or a partial-function literal. A `match` body never
            // reaches here (`visit_match` consumes it); a `catch`'s handlers score
            // inside the one `Catch`, so recurse transparently; anything else is a
            // partial function.
            "case_block" | "indented_cases" => {
                if node.parent().is_some_and(|p| p.kind() == "catch_clause") {
                    self.visit_named_children(node);
                } else {
                    self.visit_partial_function(node);
                }
            }
            "for_expression" | "while_expression" | "do_while_expression" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }
            "catch_clause" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Catch { body });
            }

            "infix_expression" => match self.logical_op_of(node) {
                Some(op) => self.visit_logical(node, op),
                None => self.visit_named_children(node),
            },

            "call_expression" => self.visit_call(node),

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// Build a `Branch` from an `if_expression`; recursion makes an `else if` (an
    /// `if_expression` in the `alternative` field) a nested `Branch` that scores
    /// flat. Parts are addressed by field, not position (a comment between them
    /// would shift a positional index).
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

    /// Lower the expression after an `else`. The grammar puts an `else if`'s
    /// `if_expression` directly in the `alternative` field, so it chains as a
    /// nested `Branch`; anything else is a plain `else` → `Group`.
    fn lower_alternate(&mut self, alt: TsNode) -> Node {
        if alt.kind() == "if_expression" {
            return self.lower_if(alt);
        }
        Node::Group(self.collect(|b| b.visit(alt)))
    }

    /// A `match` → [`Node::Switch`]. The scrutinee runs at the switch's own level
    /// first, then one `SwitchCase` per arm (see [`Builder::lower_cases`]).
    fn visit_match(&mut self, node: TsNode) {
        if let Some(value) = node.child_by_field_name("value") {
            self.visit(value);
        }
        let cases = node
            .child_by_field_name("body")
            .map_or_else(Vec::new, |body| self.lower_cases(body));
        self.emit(Node::Switch { cases });
    }

    /// A partial-function literal (`{ case … }` as an expression) → an anonymous
    /// [`Node::Function`] (its own frame, like a lambda) wrapping a `Switch`, so
    /// its branching is scored on the function rather than the enclosing unit.
    fn visit_partial_function(&mut self, node: TsNode) {
        let line = node.start_position().row as u32 + 1;
        let cases = self.lower_cases(node);
        self.emit(Node::Function {
            name: "<partial>".to_string(),
            kind: "lambda".to_string(),
            line,
            body: vec![Node::Switch { cases }],
        });
    }

    /// Lower a `case_block`/`indented_cases` into one [`SwitchCase`] per arm
    /// (shared by `match` and partial functions). The pattern and any guard are
    /// walked inside the arm body, so a guard's operators still contribute.
    fn lower_cases(&mut self, body: TsNode) -> Vec<SwitchCase> {
        let mut cases = Vec::new();
        let mut cursor = body.walk();
        for arm in body.named_children(&mut cursor) {
            if arm.kind() != "case_clause" {
                continue;
            }
            let is_default = self.is_default_case(arm);
            let arm_body = self.collect(|b| b.visit_named_children(arm));
            cases.push(SwitchCase {
                is_default,
                body: arm_body,
            });
        }
        cases
    }

    /// True if an arm is the non-decision `default`: no guard, and an irrefutable
    /// pattern. A guarded arm (`case _ if c =>`) always tests something.
    fn is_default_case(&self, arm: TsNode) -> bool {
        let mut cursor = arm.walk();
        if arm.children(&mut cursor).any(|c| c.kind() == "guard") {
            return false;
        }
        arm.child_by_field_name("pattern")
            .is_some_and(|p| self.is_irrefutable_pattern(p))
    }

    /// Whether a pattern always matches: `_`, or a variable pattern — a
    /// lowercase-initial `identifier` binding the whole value. An uppercase
    /// `identifier` (`case None`) is a stable-id comparison; all else is refutable.
    fn is_irrefutable_pattern(&self, pattern: TsNode) -> bool {
        match pattern.kind() {
            "wildcard" => true,
            "identifier" => self
                .text(pattern)
                .chars()
                .next()
                .is_some_and(|c| c == '_' || c.is_lowercase()),
            _ => false,
        }
    }

    /// One folded [`Node::Logical`] for a run of like operators (`&&` / `||`).
    /// A different operator nested inside starts a fresh `Logical`.
    fn visit_logical(&mut self, node: TsNode, op: LogicalOp) {
        let mut operands = Vec::new();
        for side in infix_sides(node).into_iter().flatten() {
            self.collect_logical_side(side, op, &mut operands);
        }
        self.emit(Node::Logical { op, operands });
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    fn collect_logical_side(&mut self, side: TsNode, op: LogicalOp, operands: &mut Vec<Node>) {
        let side = unwrap_parens(side);
        match self.logical_op_of(side) {
            Some(side_op) => {
                let sides = infix_sides(side);
                if side_op == op {
                    for k in sides.into_iter().flatten() {
                        self.collect_logical_side(k, op, operands);
                    }
                } else {
                    let mut sub = Vec::new();
                    for k in sides.into_iter().flatten() {
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
    /// then recurse into the callee and arguments.
    fn visit_call(&mut self, node: TsNode) {
        let callee = node
            .child_by_field_name("function")
            .and_then(|f| self.callee_name(f));
        self.emit(Node::Call { callee });
        self.visit_named_children(node);
    }

    /// The simple name of a call target: a bare `identifier`, a `field_expression`
    /// (`obj.m` → `m`), or a `generic_function` (`f[T]` → `f`). `None` otherwise,
    /// so recursion is not detected there.
    fn callee_name(&self, func: TsNode) -> Option<String> {
        match func.kind() {
            "identifier" | "operator_identifier" => Some(self.text(func).to_string()),
            "field_expression" => func
                .child_by_field_name("field")
                .map(|c| self.text(c).to_string()),
            "generic_function" => func
                .child_by_field_name("function")
                .and_then(|f| self.callee_name(f)),
            _ => None,
        }
    }

    /// The logical operator an `infix_expression` represents, if any — only `&&`
    /// and `||` (the `operator` field's text; Scala has no coalescing operator).
    fn logical_op_of(&self, node: TsNode) -> Option<LogicalOp> {
        if node.kind() != "infix_expression" {
            return None;
        }
        let op = node.child_by_field_name("operator")?;
        match self.text(op) {
            "&&" => Some(LogicalOp::And),
            "||" => Some(LogicalOp::Or),
            _ => None,
        }
    }
}

/// The named children of `node` as a `Vec` (for indexing / slice-matching),
/// skipping `extras` — comments, which can appear between any two children and
/// would throw off shape checks like [`unwrap_parens`]'s single-child unwrap.
/// To just recurse into all children, use [`Builder::visit_named_children`]
/// (cursor-driven, no allocation).
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}

/// The `left`/`right` operands of an `infix_expression`, by field so the (named)
/// `operator` node is never taken for an operand. `Copy` array (no allocation in
/// the folding recursion); callers `.flatten()` any absent side.
fn infix_sides(node: TsNode) -> [Option<TsNode>; 2] {
    [
        node.child_by_field_name("left"),
        node.child_by_field_name("right"),
    ]
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
        analyze_source(Path::new("Test.scala"), src)
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
        to_ir(Path::new("T.scala"), src).1
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        // Scala has no labelled `continue`, so a flat `else` supplies the 7th
        // cognitive point the labelled-jump languages get.
        let src = r#"
            object S {
                def sumOfPrimes(max: Int): Int = {
                    var total = 0
                    for (i <- 2 to max) {
                        for (j <- 2 until i) {
                            if (i % j == 0) {
                                total += 0
                            } else {
                                total += i
                            }
                        }
                    }
                    total
                }
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        // for(+1) + nested for(+2) + nested if(+3) + else(+1 flat) = 7
        assert_eq!(cognitive_of(src, "sumOfPrimes"), 7);
        // base 1 + for + for + if = 4
        assert_eq!(cyclomatic_of(src, "sumOfPrimes"), 4);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
            object S {
                def getWords(n: Int): String = n match {
                    case 1 => "one"
                    case 2 => "a couple"
                    case _ => "lots"
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "getWords"), 1);
        // base 1 + 2 non-default cases = 3
        assert_eq!(cyclomatic_of(src, "getWords"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            object S {
                def f(a: Boolean, b: Boolean, c: Boolean): Unit = {
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
            object S {
                def f(a: Boolean, b: Boolean): Int = {
                    if (a) {
                        1
                    } else if (b) {
                        2
                    } else {
                        3
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
            object S {
                def f(a: Boolean, b: Boolean, c: Boolean, d: Boolean): Unit = {
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
            object S {
                def f(a: Boolean, b: Boolean, c: Boolean): Unit = {
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
    fn if_expression_counts_and_nests() {
        let src = r#"
            object S {
                def f(a: Boolean, b: Boolean): Int =
                    if (a) (if (b) 1 else 2) else 3
            }
        "#;
        // outer if(+1) + else(+1 flat) + nested if(+2) + nested else(+1 flat) = 5
        assert_eq!(cognitive_of(src, "f"), 5);
        // base 1 + two ifs = 3 (elses are not decision points)
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            object S {
                def f(a: Boolean, items: List[Int]): Unit = {
                    while (a) { work() };
                    for (x <- items) { work() };
                    do { work() } while (a);
                }
            }
        "#;
        // three loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + three loops = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn catch_clause_counts() {
        let src = r#"
            object S {
                def f(): Unit = {
                    try {
                        risky()
                    } catch {
                        case e: IllegalStateException =>
                        case e: RuntimeException =>
                    } finally {
                        cleanup()
                    }
                }
            }
        "#;
        // one catch clause (a block of case handlers) +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + one catch = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
        // the handler block is NOT lowered as a partial-function `Switch`: the
        // arms score inside the single `Catch`, so no `<partial>` unit appears.
        assert!(find(&analyze(src).functions, "<partial>").is_none());
    }

    // A partial-function literal is its own unit; the enclosing method owns none
    // of its branching.
    #[test]
    fn partial_function_literal_is_its_own_switch_unit() {
        let src = r#"
            object M {
                def f(xs: List[Int]): List[Int] = xs.collect {
                    case x if x > 0 && x < 9 => x
                    case _ => 0
                }
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        // f owns no structural complexity; the partial function is its own unit.
        assert_eq!(cognitive_of(src, "f"), 0);
        // switch(+1) + guard && run(+1) = 2
        assert_eq!(cognitive_of(src, "<partial>"), 2);
        // base 1 + one non-default arm (`case x if …`; bare `case _` is default)
        //   + (&& 2 operands => +1) = 3
        assert_eq!(cyclomatic_of(src, "<partial>"), 3);
        assert_eq!(
            find(&analyze(src).functions, "<partial>").unwrap().kind,
            "lambda"
        );
    }

    // `scala.util.control.Breaks` / `boundary` are ordinary calls, so
    // `breakable`/`break` add nothing — the score comes only from the loop and `if`.
    #[test]
    fn library_break_is_transparent() {
        let src = r#"
            import scala.util.control.Breaks._

            object S {
                def firstEven(xs: List[Int]): Int = {
                    var result = -1
                    breakable {
                        for (x <- xs) {
                            if (x % 2 == 0) {
                                result = x
                                break()
                            }
                        }
                    }
                    result
                }
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        // for(+1) + nested if(+2) = 3; breakable/break contribute nothing.
        assert_eq!(cognitive_of(src, "firstEven"), 3);
        // base 1 + for + if = 3
        assert_eq!(cyclomatic_of(src, "firstEven"), 3);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
            object S {
                def fib(n: Int): Int =
                    if (n < 2) n else fib(n - 1) + fib(n - 2)
            }
        "#;
        // if(+1) + else(+1 flat) + two recursive calls(+2) = 4
        assert_eq!(cognitive_of(src, "fib"), 4);
    }

    #[test]
    fn method_recursion_through_this() {
        let src = r#"
            class C {
                def walk(n: Int): Int =
                    if (n == 0) 0 else this.walk(n - 1)
            }
        "#;
        // if(+1) + else(+1 flat) + recursion via this.walk(+1) = 3
        assert_eq!(cognitive_of(src, "walk"), 3);
        assert_eq!(
            find(&analyze(src).functions, "walk").unwrap().kind,
            "method"
        );
    }

    #[test]
    fn lambda_is_its_own_unit() {
        let src = r#"
            object S {
                def host(items: List[Int]): Unit = {
                    items.foreach(x => {
                        if (x > 0 && x < 10) {
                        }
                    })
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
    fn guarded_case_is_a_decision_but_wildcard_default_is_not() {
        let src = r#"
            object S {
                def f(o: Any, p: Boolean): String = o match {
                    case i: Int if i > 0 && p => "positive int"
                    case s: String => "string"
                    case _ => "other"
                }
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        // match(+1) + guard && run(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + 2 non-default arms + (&& 2 operands => +1) = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    // A wildcard *with* a guard (`case _ if p =>`) tests something, so it is a
    // real decision — unlike the bare `case _ =>`, which is the default arm.
    #[test]
    fn guarded_wildcard_is_a_decision() {
        let src = r#"
            object S {
                def f(n: Int, p: Boolean): String = n match {
                    case 1 => "one"
                    case _ if p => "guarded"
                    case _ => "other"
                }
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        assert_eq!(cognitive_of(src, "f"), 1); // match(+1)
        // base 1 + `case 1` + guarded `case _ if p` (both decisions); bare `case _` is default = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    // A lowercase variable pattern (`case other =>`) binds the whole value and
    // always matches, so it is the catch-all — like `case _ =>`, not a decision.
    #[test]
    fn variable_pattern_catch_all_is_the_default_arm() {
        let named = r#"
            object S {
                def f(n: Int): String = n match {
                    case 0 => "zero"
                    case other => "nonzero"
                }
            }
        "#;
        assert!(parse_errors(named).is_empty(), "{:?}", parse_errors(named));
        assert_eq!(cognitive_of(named, "f"), 1); // match(+1)
        // base 1 + only `case 0` is a decision; `case other` is the catch-all = 2
        assert_eq!(cyclomatic_of(named, "f"), 2);

        // An uppercase identifier is a stable-id constant match, so it stays a
        // decision (both arms count).
        let stable = r#"
            object S {
                def f(o: Option[Int]): Int = o match {
                    case Some(x) => x
                    case None => 0
                }
            }
        "#;
        assert!(
            parse_errors(stable).is_empty(),
            "{:?}",
            parse_errors(stable)
        );
        // base 1 + `Some(x)` + `None` (both refutable) = 3
        assert_eq!(cyclomatic_of(stable, "f"), 3);
    }

    // A secondary constructor is a `<constructor>` unit, and its `this(..)`
    // self-delegation is not scored as recursion.
    #[test]
    fn secondary_constructor_is_a_constructor_unit_not_recursion() {
        let src = r#"
            class C(x: Int) {
                def this() = this(0)
            }
        "#;
        assert!(parse_errors(src).is_empty(), "{:?}", parse_errors(src));
        let report = analyze(src);
        let ctor = find(&report.functions, "<constructor>").expect("constructor unit");
        assert_eq!(ctor.kind, "constructor");
        assert_eq!(ctor.cognitive, 0);
        // no unit is named `this`
        assert!(find(&report.functions, "this").is_none());
    }

    #[test]
    fn file_total_sums_all_defs() {
        let src = r#"
            object S {
                def a(x: Boolean): Unit = {
                    if (x) {
                    }
                }
                def b(y: Boolean): Unit = {
                    if (y) {
                    }
                }
            }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn syntax_error_is_reported() {
        let errors = parse_errors("object S { def f( = ");
        assert!(!errors.is_empty());
    }
}
