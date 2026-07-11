//! Kotlin adapter: parses source with the [tree-sitter] `exoego/tree-sitter-kotlin`
//! grammar (a fork of the fwcd tree-sitter Kotlin grammar with fixes for
//! modern-Kotlin constructs; pinned by git rev in `Cargo.toml`) and lowers the
//! concrete syntax tree into the language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the Kotlin grammar (whose C source is compiled by `cc`, so unlike `cccc-rb`
//! there is no `libclang`/bindgen requirement), with no CLI machinery. The
//! unified `cccc` binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.kt`/`.kts` files to it.
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
//! ## Kotlin-to-IR mapping notes
//!
//! - `fun` declaration / method / local `fun`, `anonymous_function`,
//!   `lambda_literal`, and property `get`/`set` accessors → [`Node::Function`].
//! - `if` expression → [`Node::Branch`] (`else if` — an `if_expression` nested in
//!   the `else` body — chains as a nested `Branch` so it scores flat).
//! - `when` expression (with or without a subject) → [`Node::Switch`]; the `else`
//!   entry is the non-decision `default` arm.
//! - `for` / `while` / `do`-`while` → [`Node::Loop`].
//! - `try` / `catch` / `finally` → one [`Node::Catch`] per `catch` clause (the
//!   `try` and `finally` bodies run at the surrounding level).
//! - labelled `break@l` / `continue@l` → [`Node::Jump`] (`labeled: true`); plain
//!   `break` / `continue` score flat. `return` / `throw` are transparent.
//! - `&&` / `||` runs → folded [`Node::Logical`]; the elvis operator `?:` folds
//!   as a `Coalesce` run (mirroring how `cccc-php` treats `??`).
//! - calls (`f(..)`, `obj.m(..)`) → [`Node::Call`] for recursion detection.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given). `.kts` is
/// Kotlin script (e.g. Gradle build scripts), parsed by the same grammar.
pub const DEFAULT_EXTS: &[&str] = &["kt", "kts"];

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
        .set_language(&tree_sitter_kotlin::LANGUAGE.into())
        .is_err()
    {
        return (
            Vec::new(),
            vec!["failed to load Kotlin grammar".to_string()],
        );
    }
    let Some(tree) = parser.parse(source, None) else {
        return (
            Vec::new(),
            vec!["failed to parse Kotlin source".to_string()],
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
        self.emit_function(name, kind, line, |b| b.visit_named_children(node));
    }

    // ---- traversal --------------------------------------------------------

    fn visit(&mut self, node: TsNode) {
        match node.kind() {
            "function_declaration" => {
                // A `fun` declared directly in a class / enum-class / interface /
                // object body is a method; anything else (top-level, or local to
                // a function body) is a plain function.
                let kind = if matches!(
                    node.parent().map(|p| p.kind()),
                    Some("class_body" | "enum_class_body")
                ) {
                    "method"
                } else {
                    "function"
                };
                let name = self
                    .simple_name(node)
                    .unwrap_or_else(|| "<function>".into());
                self.emit_function_node(name, kind, node);
            }
            "anonymous_function" => {
                self.emit_function_node("<anonymous>".into(), "anonymous", node)
            }
            "lambda_literal" => self.emit_function_node("<lambda>".into(), "lambda", node),
            "getter" => self.emit_function_node("<getter>".into(), "getter", node),
            "setter" => self.emit_function_node("<setter>".into(), "setter", node),
            "secondary_constructor" => {
                self.emit_function_node("<constructor>".into(), "constructor", node)
            }

            "if_expression" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "when_expression" => self.visit_when(node),
            "for_statement" | "while_statement" | "do_while_statement" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }
            "try_expression" => self.visit_try(node),
            "jump_expression" => self.visit_jump(node),

            "conjunction_expression" => self.visit_logical(node, LogicalOp::And),
            "disjunction_expression" => self.visit_logical(node, LogicalOp::Or),
            "elvis_expression" => self.visit_logical(node, LogicalOp::Coalesce),

            "call_expression" => self.visit_call(node),

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// The first direct `simple_identifier` child's text (a declaration's name).
    fn simple_name(&self, node: TsNode) -> Option<String> {
        named_children(node)
            .into_iter()
            .find(|c| c.kind() == "simple_identifier")
            .map(|c| self.text(c).to_string())
    }

    /// Build a `Branch` from an `if_expression` (recursively, so an `else if`
    /// becomes a nested `Branch` and thus scores flat).
    ///
    /// The grammar tags the three parts with fields (`condition`, `consequence`,
    /// `alternative`), so we address them by field rather than by position: the
    /// `consequence` is *optional* in the else-bearing production (`if (x) else
    /// y`), and comments can sit between the parts — either would shift a
    /// positional index and mis-assign the `then`/`else` bodies.
    fn lower_if(&mut self, node: TsNode) -> Node {
        let field = |name| node.child_by_field_name(name);
        let test = field("condition").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let then = field("consequence").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let alternate = field("alternative").map(|csb| Box::new(self.lower_alternate(csb)));
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// Lower the `control_structure_body` after an `else`. If it wraps a single
    /// `if_expression` it is an `else if` → nested `Branch`; otherwise it is a
    /// plain `else` → `Group`.
    fn lower_alternate(&mut self, csb: TsNode) -> Node {
        let inner = named_children(csb);
        if let [only] = inner.as_slice()
            && only.kind() == "if_expression"
        {
            return self.lower_if(*only);
        }
        Node::Group(self.collect(|b| b.visit(csb)))
    }

    /// A `when` becomes a `Switch`: one `SwitchCase` per `when_entry`, with the
    /// `else` entry marked `is_default`. Any subject expression runs at the
    /// switch's own level first.
    fn visit_when(&mut self, node: TsNode) {
        let mut cases = Vec::new();
        for child in named_children(node) {
            match child.kind() {
                "when_subject" => self.visit_named_children(child),
                "when_entry" => {
                    let is_default = !named_children(child)
                        .iter()
                        .any(|c| c.kind() == "when_condition");
                    let body = self.collect(|b| b.visit_named_children(child));
                    cases.push(SwitchCase { is_default, body });
                }
                _ => {}
            }
        }
        self.emit(Node::Switch { cases });
    }

    /// The `try` body and any `finally` body run at the surrounding level; each
    /// `catch` clause is a `Node::Catch` decision point.
    fn visit_try(&mut self, node: TsNode) {
        for child in named_children(node) {
            match child.kind() {
                "catch_block" => {
                    let body = self.collect(|b| b.visit_named_children(child));
                    self.emit(Node::Catch { body });
                }
                // The try body (`statements`) and `finally_block` are transparent.
                _ => self.visit_named_children(child),
            }
        }
    }

    /// A labelled `break@l` / `continue@l` scores one flat cognitive point; plain
    /// `break` / `continue` do not. `return` / `throw` carry no jump point but may
    /// hold a sub-expression, so recurse into their children.
    fn visit_jump(&mut self, node: TsNode) {
        let keyword = node.child(0).map(|c| c.kind()).unwrap_or("");
        match keyword {
            "break" | "break@" | "continue" | "continue@" => {
                self.emit(Node::Jump {
                    labeled: keyword.contains('@'),
                });
            }
            _ => self.visit_named_children(node),
        }
    }

    /// One folded [`Node::Logical`] for a run of like operators (`&&` / `||` /
    /// elvis `?:`). A different operator nested inside starts a fresh `Logical`.
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
    /// arguments and trailing lambda may contain further constructs).
    fn visit_call(&mut self, node: TsNode) {
        let callee = node.named_child(0).and_then(|c| self.callee_name(c));
        self.emit(Node::Call { callee });
        self.visit_named_children(node);
    }

    /// Simple name of a directly-called callee (`foo(..)` or `obj.foo(..)`),
    /// used for recursion detection. Returns the trailing identifier.
    fn callee_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "simple_identifier" => Some(self.text(node).to_string()),
            "navigation_expression" => named_children(node).into_iter().rev().find_map(|c| match c
                .kind()
            {
                "navigation_suffix" => self.simple_name(c),
                "simple_identifier" => Some(self.text(c).to_string()),
                _ => None,
            }),
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
/// Comments are `extras` in this grammar: they are named nodes that can appear
/// *between* any two children (e.g. `if (x) /* c */ { … }`). We drop them here
/// so slice-shape checks (`lower_alternate`'s single-child `else if` test,
/// `unwrap_parens`' single-child unwrap) are not thrown off by an interleaved
/// comment — and because a comment never lowers to IR anyway.
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
        "elvis_expression" => Some(LogicalOp::Coalesce),
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
        analyze_source(Path::new("test.kt"), src)
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
        let src = r#"
            fun sumOfPrimes(max: Int): Int {
                var total = 0
                outer@ for (i in 2..max) {
                    for (j in 2 until i) {
                        if (i % j == 0) {
                            continue@outer
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
            fun getWords(n: Int): String {
                return when (n) {
                    1 -> "one"
                    2 -> "a couple"
                    else -> "lots"
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "getWords"), 1);
        // base 1 + 2 non-default entries = 3
        assert_eq!(cyclomatic_of(src, "getWords"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            fun f(a: Boolean, b: Boolean, c: Boolean) {
                if (a) {
                    if (b) {
                        if (c) {
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
            fun f(a: Boolean, b: Boolean) {
                if (a) {
                } else if (b) {
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
    fn logical_sequences_fold() {
        let src = r#"
            fun f(a: Boolean, b: Boolean, c: Boolean, d: Boolean) {
                if (a && b && c || d) {
                }
            }
        "#;
        // if(+1) + && seq(+1) + || seq(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn elvis_counts_as_coalesce() {
        let src = r#"
            fun f(a: String?, b: String?): String = a ?: b ?: "z"
        "#;
        // one folded elvis run = +1 cognitive
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + (elvis has 3 operands => +2) = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            fun f(a: Boolean, items: List<Int>) {
                while (a) { }
                for (i in items) { }
                do { } while (a)
            }
        "#;
        // three loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + three loops = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn catch_clauses_count() {
        let src = r#"
            fun f() {
                try {
                    risky()
                } catch (e: IllegalStateException) {
                } catch (e: Exception) {
                } finally {
                    cleanup()
                }
            }
        "#;
        // two catch clauses, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + two catches = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
            fun fib(n: Int): Int {
                if (n < 2) {
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
                fun walk(n: Int): Int {
                    if (n == 0) {
                        return 0
                    }
                    return this.walk(n - 1)
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
    fn lambda_is_its_own_unit() {
        let src = r#"
            fun host(items: List<Int>) {
                items.forEach { x ->
                    if (x > 0 && x < 10) {
                    }
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
    fn file_total_sums_all_functions() {
        let src = r#"
            fun a(x: Boolean) {
                if (x) {
                }
            }
            fun b(y: Boolean) {
                if (y) {
                }
            }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    fn parse_errors(src: &str) -> Vec<String> {
        to_ir(Path::new("t.kt"), src).1
    }

    // `@Composable`-annotated function types (pervasive in Jetpack Compose)
    // Real pattern from coil/koin.
    #[test]
    fn compose_annotated_function_type_is_analyzed() {
        let src = r#"
            @Composable
            fun Loader(
                loading: @Composable (Scope.(State) -> Unit)? = null,
                content: @Composable (String) -> Unit,
            ) {
                if (loading != null) {
                    content("x")
                }
            }
        "#;
        assert!(
            parse_errors(src).is_empty(),
            "annotated function types should parse: {:?}",
            parse_errors(src)
        );
        // the function is found and scored (if +1)
        assert_eq!(cognitive_of(src, "Loader"), 1);
    }

    #[test]
    fn annotation_with_arguments_does_not_break_parsing() {
        let src = r#"
            @JvmName("legacyName")
            @Suppress("UNCHECKED_CAST", "DEPRECATION")
            fun f(x: Boolean): Int {
                if (x) { return 1 }
                return 0
            }
        "#;
        assert!(parse_errors(src).is_empty());
        // if(+1) = 1 — annotations contribute nothing
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    // Annotations are complexity-neutral: the score is identical with or without.
    #[test]
    fn annotations_do_not_change_score() {
        let annotated = r#"
            @Test
            @Composable
            fun f(a: Boolean, b: Boolean): Int {
                if (a && b) return 1
                return 0
            }
        "#;
        let plain = r#"
            fun f(a: Boolean, b: Boolean): Int {
                if (a && b) return 1
                return 0
            }
        "#;
        // if(+1) + &&(+1) = 2 in both
        assert_eq!(cognitive_of(annotated, "f"), 2);
        assert_eq!(cognitive_of(annotated, "f"), cognitive_of(plain, "f"));
    }

    // Labels, `this@`, `return@`, and `@` inside strings/comments must not be
    // mistaken for annotations — the labelled `continue@outer` must still score.
    #[test]
    fn labels_and_string_at_are_untouched() {
        let src = r#"
            fun sumOfPrimes(max: Int): Int {
                val note = "reach me at a@b.com" // @not-an-annotation
                var total = 0
                outer@ for (i in 2..max) {
                    for (j in 2 until i) {
                        if (i % j == 0) {
                            continue@outer
                        }
                    }
                    total += i
                }
                return total
            }
        "#;
        assert!(parse_errors(src).is_empty());
        assert_eq!(cognitive_of(src, "sumOfPrimes"), 7);
    }

    // A file-level annotation whose use-site target is separated from its name
    // by a space (`@file: Suppress(...)`) parses cleanly before `package`.
    #[test]
    fn file_annotation_with_space_after_target() {
        let src = r#"
            @file: Suppress("A", "B")

            package foo.bar

            fun f(x: Boolean): Int {
                if (x) return 1
                return 0
            }
        "#;
        assert!(
            parse_errors(src).is_empty(),
            "errors: {:?}",
            parse_errors(src)
        );
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    // A use-site target with a bracketed annotation array (`@get:[JvmStatic
    // JvmName("x")]`, as in Timber) parses cleanly.
    #[test]
    fn use_site_target_annotation_array() {
        let src = r#"
            class C {
                @get:[JvmStatic JvmName("treeCount")]
                val treeCount: Int get() = 1

                fun f(x: Boolean): Int {
                    if (x) return 1
                    return 0
                }
            }
        "#;
        assert!(
            parse_errors(src).is_empty(),
            "errors: {:?}",
            parse_errors(src)
        );
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    // Comments are `extras`: they appear as named nodes wherever they are
    // written, including between an `if`'s condition and its body. Lowering must
    // address the parts by field (not by named-child position) so an interleaved
    // comment does not shift the `then`/`else` bodies and mis-score the branch.
    #[test]
    fn comment_between_if_parts_does_not_change_score() {
        // baseline: plain `if` = +1
        let plain = "fun f(a: Boolean) { if (a) { if (a) { } } }";
        // a block comment before the body, and inside the condition parens
        let commented = "fun f(a: Boolean) { if (/* c */ a) /* c */ { if (a) { } } }";
        // if(+1) + nested if(+2) = 3, with or without the comments
        assert_eq!(cognitive_of(plain, "f"), 3);
        assert_eq!(cognitive_of(commented, "f"), cognitive_of(plain, "f"));
    }

    // A `fun` declared directly in an `enum class` body is a method, like one in
    // a plain `class` body — the parent node is `enum_class_body`, not
    // `class_body`, so both must map to the `"method"` kind.
    #[test]
    fn enum_class_member_is_a_method() {
        let src = r#"
            enum class E {
                A, B;
                fun describe(x: Boolean): Int {
                    if (x) return 1
                    return 0
                }
            }
        "#;
        let report = analyze(src);
        let f = find(&report.functions, "describe").expect("describe");
        assert_eq!(f.kind, "method");
        assert_eq!(f.cognitive, 1);
    }

    #[test]
    fn parse_error_is_reported() {
        // tree-sitter is fault-tolerant: it still yields a (partial) tree but
        // surfaces the error location for the broken input.
        let errors = parse_errors("fun ok(a: Int): Int { return a }\nfun bad( {\n");
        assert!(!errors.is_empty());
    }
}
