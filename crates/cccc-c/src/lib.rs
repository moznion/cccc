//! C adapter: parses source with the official [tree-sitter] `tree-sitter-c`
//! grammar and lowers the concrete syntax tree into the language-agnostic
//! [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the C grammar (whose C source is compiled by `cc`, so like `cccc-kt` there
//! is no `libclang`/bindgen requirement), with no CLI machinery. The unified
//! `cccc` binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.c`/`.h` files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about and emits the matching IR nodes. All complexity rules
//! live in [`cccc_core::engine`].
//!
//! Like `cccc-kt`, lowering is an explicit `kind()`-dispatch whose **default arm
//! recurses into every named child** (see the warning in
//! `docs/ADDING_A_LANGUAGE.md`), so an unrecognized construct is transparent and
//! nothing nested inside it is silently dropped. The IR tree is assembled with a
//! stack of "collectors": [`Builder::collect`] pushes a fresh child vector, runs
//! a sub-traversal, and pops the nodes it gathered.
//!
//! ## C-to-IR mapping notes
//!
//! - `function_definition` (including K&R-style definitions and GNU nested
//!   functions) → [`Node::Function`]. The name is dug out of the declarator
//!   chain (`int *(*f(void))(int)` still names `f`).
//! - `if` / `else if` / `else` → [`Node::Branch`] (an `if_statement` directly
//!   under an `else_clause` chains as a nested `Branch` so it scores flat).
//! - the ternary `?:` → [`Node::Conditional`] (GNU's elided-middle `a ?: b`
//!   included).
//! - `for` / `while` / `do`-`while` → [`Node::Loop`].
//! - `switch` → [`Node::Switch`]; the `default:` label is the non-decision
//!   `default` arm. Fall-through label runs (`case 1: case 2:`) parse as
//!   sibling `case_statement`s, so each label is its own cyclomatic point.
//! - `goto` → [`Node::Jump`] (`labeled: true`, one flat cognitive point); plain
//!   `break` / `continue` score flat (`labeled: false`).
//! - `&&` / `||` runs → folded [`Node::Logical`]. C has no null-coalescing
//!   operator, so [`LogicalOp::Coalesce`] is never emitted.
//! - preprocessor conditionals (`#if` / `#ifdef` / `#ifndef`, chained via
//!   `#elif` / `#elifdef` / `#else`) → [`Node::Branch`], mirroring how the
//!   SonarSource C/C++ analyzers treat macro conditionals as decision points.
//! - calls (`f(..)`, `s.f(..)`, `(*fp)(..)`) → [`Node::Call`] for recursion
//!   detection.
//! - `#define` bodies are opaque token blobs in the grammar (`preproc_arg`), so
//!   code inside a macro body is not scored — same trade-off as every
//!   preprocessor-unaware C tool.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given). `.h`
/// headers are claimed as C — this project bundles no C++ front-end, so there
/// is no dispatch ambiguity.
pub const DEFAULT_EXTS: &[&str] = &["c", "h"];

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
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load C grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse C source".to_string()]);
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

    // ---- traversal --------------------------------------------------------

    fn visit(&mut self, node: TsNode) {
        match node.kind() {
            "function_definition" => {
                let name = node
                    .child_by_field_name("declarator")
                    .and_then(|d| declarator_name(self, d))
                    .unwrap_or_else(|| "<function>".into());
                let line = node.start_position().row as u32 + 1;
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Function {
                    name,
                    kind: "function".to_string(),
                    line,
                    body,
                });
            }

            "if_statement" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "conditional_expression" => {
                let field = |name| node.child_by_field_name(name);
                let test =
                    field("condition").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
                let then =
                    field("consequence").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
                let alternate =
                    field("alternative").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
                self.emit(Node::Conditional {
                    test,
                    then,
                    alternate,
                });
            }
            "for_statement" | "while_statement" | "do_statement" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }
            "switch_statement" => self.visit_switch(node),

            "break_statement" | "continue_statement" => self.emit(Node::Jump { labeled: false }),
            "goto_statement" => self.emit(Node::Jump { labeled: true }),

            "binary_expression" => match logical_op_of(node) {
                Some(op) => self.visit_logical(node, op),
                None => self.visit_named_children(node),
            },

            "call_expression" => self.visit_call(node),

            // The grammar aliases preprocessor conditionals inside declarations
            // and inside blocks to the same kinds, so one set of arms covers
            // both placements.
            "preproc_if" | "preproc_ifdef" => {
                let branch = self.lower_preproc(node);
                self.emit(branch);
            }

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// Build a `Branch` from an `if_statement` (recursively, so an `else if`
    /// becomes a nested `Branch` and thus scores flat). The grammar tags the
    /// parts with fields (`condition`, `consequence`, `alternative`), so we
    /// address them by field rather than by position.
    fn lower_if(&mut self, node: TsNode) -> Node {
        let field = |name| node.child_by_field_name(name);
        let test = field("condition").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let then = field("consequence").map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let alternate = field("alternative").map(|ec| Box::new(self.lower_else(ec)));
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// Lower an `else_clause`. If it wraps a single `if_statement` it is an
    /// `else if` → nested `Branch`; otherwise it is a plain `else` → `Group`.
    fn lower_else(&mut self, else_clause: TsNode) -> Node {
        let inner = named_children(else_clause);
        if let [only] = inner.as_slice()
            && only.kind() == "if_statement"
        {
            return self.lower_if(*only);
        }
        Node::Group(self.collect(|b| b.visit_named_children(else_clause)))
    }

    /// Build a `Branch` from a preprocessor conditional (`#if` / `#ifdef` /
    /// `#ifndef` / `#elif` / `#elifdef` / `#elifndef`): the directive's own
    /// body is the `then`, and the `alternative` field chains — an `#elif`
    /// nests as another `Branch` (scoring flat, like `else if`), an `#else`
    /// closes the chain as a `Group`.
    fn lower_preproc(&mut self, node: TsNode) -> Node {
        // `#if`/`#elif` carry a `condition` expression; `#ifdef`/`#elifdef`
        // carry a `name` identifier. Either way it is the branch's test.
        let cond = node
            .child_by_field_name("condition")
            .or_else(|| node.child_by_field_name("name"));
        let alt = node.child_by_field_name("alternative");
        let test = cond.map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let then = self.collect(|b| {
            for child in named_children(node) {
                let is_cond = cond.is_some_and(|c| c.id() == child.id());
                let is_alt = alt.is_some_and(|a| a.id() == child.id());
                if !is_cond && !is_alt {
                    b.visit(child);
                }
            }
        });
        let alternate = alt.map(|a| {
            Box::new(match a.kind() {
                "preproc_elif" | "preproc_elifdef" | "preproc_elifndef" => self.lower_preproc(a),
                // preproc_else
                _ => Node::Group(self.collect(|b| b.visit_named_children(a))),
            })
        });
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// A `switch` becomes a `Switch`: one `SwitchCase` per `case_statement`,
    /// with the `default:` label marked `is_default` (the grammar gives it no
    /// `value` field). The subject expression runs at the switch's own level.
    fn visit_switch(&mut self, node: TsNode) {
        if let Some(cond) = node.child_by_field_name("condition") {
            self.visit(cond);
        }
        let mut cases = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            for child in named_children(body) {
                if child.kind() == "case_statement" {
                    let is_default = child.child_by_field_name("value").is_none();
                    let case_body = self.collect(|b| b.visit_named_children(child));
                    cases.push(SwitchCase {
                        is_default,
                        body: case_body,
                    });
                } else {
                    // A label or statement outside any case (legal C) runs at
                    // the switch's level.
                    self.visit(child);
                }
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

    /// Emit a `Call` (with the callee's simple name for recursion detection),
    /// then recurse into the callee expression and the argument list (which may
    /// contain further constructs).
    fn visit_call(&mut self, node: TsNode) {
        let callee = node
            .child_by_field_name("function")
            .and_then(|f| self.callee_name(f));
        self.emit(Node::Call { callee });
        self.visit_named_children(node);
    }

    /// Simple name of a directly-called callee: `foo(..)`, `s.foo(..)` /
    /// `p->foo(..)`, or a parenthesized/dereferenced function pointer
    /// (`(*fp)(..)`). Returns the trailing identifier.
    fn callee_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node).to_string()),
            "field_expression" => node
                .child_by_field_name("field")
                .map(|f| self.text(f).to_string()),
            "parenthesized_expression" | "pointer_expression" => named_children(node)
                .into_iter()
                .find_map(|c| self.callee_name(c)),
            _ => None,
        }
    }
}

/// Dig the defined name out of a declarator chain: a `function_definition`'s
/// `declarator` may wrap the identifier in `pointer_declarator` (functions
/// returning pointers), `function_declarator`, and `parenthesized_declarator`
/// layers (`int *(*f(void))(int)`). Follows `declarator` fields, falling back
/// to the named children for the field-less `parenthesized_declarator`.
fn declarator_name(b: &Builder, node: TsNode) -> Option<String> {
    match node.kind() {
        "identifier" => Some(b.text(node).to_string()),
        "parenthesized_declarator" => named_children(node)
            .into_iter()
            .find_map(|c| declarator_name(b, c)),
        _ => node
            .child_by_field_name("declarator")
            .and_then(|d| declarator_name(b, d)),
    }
}

/// The named children of `node` (skipping `extras`), collected into a `Vec` so
/// the caller can index or slice-match them without holding the cursor's
/// borrow. Comments are `extras` in this grammar: they can appear *between*
/// any two children, so dropping them keeps slice-shape checks
/// (`lower_else`'s single-child `else if` test, `unwrap_parens`' single-child
/// unwrap) honest.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}

/// The normalized logical operator a node represents, if any. The grammar
/// aliases the preprocessor's binary expressions to `binary_expression` too,
/// so `#if defined(A) && defined(B)` folds the same way.
fn logical_op_of(node: TsNode) -> Option<LogicalOp> {
    if node.kind() != "binary_expression" {
        return None;
    }
    match node.child_by_field_name("operator")?.kind() {
        "&&" => Some(LogicalOp::And),
        "||" => Some(LogicalOp::Or),
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
        analyze_source(Path::new("test.c"), src)
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
        to_ir(Path::new("t.c"), src).1
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        // C has no labelled continue; the canonical Sonar example uses goto.
        let src = r#"
            unsigned int sum_of_primes(unsigned int max) {
                unsigned int total = 0;
                for (unsigned int i = 2; i <= max; i++) {
                    for (unsigned int j = 2; j < i; j++) {
                        if (i % j == 0) {
                            goto next;
                        }
                    }
                    total += i;
            next:;
                }
                return total;
            }
        "#;
        // for(+1) + nested for(+2) + nested if(+3) + goto(+1) = 7
        assert_eq!(cognitive_of(src, "sum_of_primes"), 7);
        // base 1 + for + for + if = 4
        assert_eq!(cyclomatic_of(src, "sum_of_primes"), 4);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
            const char *get_words(int n) {
                switch (n) {
                    case 1:
                        return "one";
                    case 2:
                        return "a couple";
                    default:
                        return "lots";
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "get_words"), 1);
        // base 1 + 2 non-default cases = 3
        assert_eq!(cyclomatic_of(src, "get_words"), 3);
    }

    #[test]
    fn fall_through_case_labels_each_count() {
        let src = r#"
            int classify(int n) {
                switch (n) {
                    case 1:
                    case 2:
                        return 1;
                    default:
                        return 0;
                }
            }
        "#;
        // the switch itself: +1 cognitive
        assert_eq!(cognitive_of(src, "classify"), 1);
        // base 1 + two case labels = 3 (default is not a decision point)
        assert_eq!(cyclomatic_of(src, "classify"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            void f(int a, int b, int c) {
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
            void f(int a, int b) {
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
            void f(int a, int b, int c, int d) {
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
    fn parenthesized_like_operators_fold_into_one_run() {
        let src = r#"
            void f(int a, int b, int c) {
                if (a && (b && c)) {
                }
            }
        "#;
        // if(+1) + one folded && run(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + if 1 + (&& 3 operands => +2) = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn ternary_counts_as_conditional() {
        let src = r#"
            int f(int a, int b) {
                return a ? a : b;
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + ternary = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn gnu_elided_ternary_counts_too() {
        let src = r#"
            int f(int a, int b) {
                return a ?: b;
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            void f(int a) {
                while (a) { }
                for (int i = 0; i < a; i++) { }
                do { } while (a);
            }
        "#;
        // three loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + three loops = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn goto_counts_plain_break_does_not() {
        let src = r#"
            void f(int a) {
                for (;;) {
                    if (a) {
                        break;
                    }
                    goto out;
                }
            out:;
            }
        "#;
        // for(+1) + if(+2) + goto(+1); plain break is free = 4
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
            int fib(int n) {
                if (n < 2) {
                    return n;
                }
                return fib(n - 1) + fib(n - 2);
            }
        "#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn pointer_returning_function_is_named() {
        let src = r#"
            char *dup(const char *s, int retry) {
                if (retry) {
                    return dup(s, 0);
                }
                return 0;
            }
        "#;
        // the name is dug out of the pointer_declarator chain, so the
        // recursive call is detected: if(+1) + recursion(+1) = 2
        assert_eq!(cognitive_of(src, "dup"), 2);
    }

    #[test]
    fn function_pointer_call_is_not_misdetected_as_recursion() {
        let src = r#"
            void f(void (*g)(void)) {
                if (g) {
                    (*g)();
                }
            }
        "#;
        // if(+1); calling through the pointer named g is not recursion into f
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn preproc_conditional_counts_as_branch() {
        let src = r#"
            void f(int a) {
            #ifdef FAST_PATH
                if (a) { }
            #elif defined(SLOW_PATH)
                while (a) { }
            #else
                (void)a;
            #endif
            }
        "#;
        // #ifdef(+1) + nested if(+2) + #elif(+1 flat) + nested while(+2)
        // + #else(+1 flat) = 7
        assert_eq!(cognitive_of(src, "f"), 7);
    }

    #[test]
    fn preproc_around_functions_still_finds_them() {
        let src = r#"
            #if defined(A) && defined(B)
            int f(int x) {
                if (x) { return 1; }
                return 0;
            }
            #endif
        "#;
        assert!(parse_errors(src).is_empty());
        // the function's own score is unaffected by the surrounding #if
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn knr_style_definition_is_analyzed() {
        let src = r#"
            int f(x)
                int x;
            {
                if (x) { return 1; }
                return 0;
            }
        "#;
        assert!(parse_errors(src).is_empty());
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn comment_between_if_parts_does_not_change_score() {
        let plain = "void f(int a) { if (a) { if (a) { } } }";
        let commented = "void f(int a) { if (/* c */ a) /* c */ { if (a) { } } }";
        // if(+1) + nested if(+2) = 3, with or without the comments
        assert_eq!(cognitive_of(plain, "f"), 3);
        assert_eq!(cognitive_of(commented, "f"), cognitive_of(plain, "f"));
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
            void a(int x) {
                if (x) {
                }
            }
            void b(int y) {
                if (y) {
                }
            }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn extern_c_guard_reports_error_but_still_scores() {
        // The standard `extern "C" {` guard splits its braces across two
        // `#ifdef __cplusplus` blocks. A preprocessor-unaware grammar cannot
        // balance them, so the guard surfaces as a parse warning — but the
        // declarations between the guards still lower and score.
        let src = r#"
            #ifdef __cplusplus
            extern "C" {
            #endif

            int f(int x) {
                if (x) { return 1; }
                return 0;
            }

            #ifdef __cplusplus
            }
            #endif
        "#;
        assert!(!parse_errors(src).is_empty());
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn parse_error_is_reported() {
        // tree-sitter is fault-tolerant: it still yields a (partial) tree but
        // surfaces the error location for the broken input.
        let errors = parse_errors("int ok(int a) { return a; }\nint bad( {\n");
        assert!(!errors.is_empty());
    }
}
