//! Shell adapter: parses Bash / POSIX sh / [Bats] source with the [tree-sitter]
//! `tree-sitter-bats` grammar and lowers the concrete syntax tree into the
//! language-agnostic [`cccc_core::ir`].
//!
//! The Bats grammar extends the official `tree-sitter-bash` grammar with Bats'
//! `@test` blocks; on plain Bash/sh it yields the same trees as
//! `tree-sitter-bash` (and recovers from a few constructs that one rejects), so
//! one parser serves `.sh`, `.bash` and `.bats` files alike. POSIX sh is a
//! subset of Bash, so it needs nothing extra.
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the grammar (whose C source is compiled by `cc`, so like `cccc-py` there is
//! no `libclang`/bindgen requirement), with no CLI machinery. The unified `cccc`
//! binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.sh`/`.bash`/`.bats`
//! files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about and emits the matching IR nodes. All complexity rules
//! live in [`cccc_core::engine`].
//!
//! Like `cccc-py`, lowering is driven by a `kind()`-dispatch whose **default arm
//! recurses into every named child**, so a construct in an unexpected position
//! (a function defined inside a command substitution, a ternary inside an
//! array subscript) is never silently missed. The IR tree is assembled with a
//! stack of "collectors" ([`Builder::collect`]).
//!
//! ## Shell-to-IR mapping notes
//!
//! - `name() { … }` / `function name { … }` → [`Node::Function`] (`"function"`).
//!   A Bats `@test "…" { … }` block, and a function whose opening line carries
//!   Bats' `# @test` marker, → `"test"`. Each is its own unit — nesting resets
//!   at the boundary.
//! - `if` / `elif` / `else` → [`Node::Branch`] (chaining `elif` as a nested
//!   `Branch` so it scores flat). The arithmetic ternary `a ? b : c` (inside
//!   `(( ))` / `$(( ))`) → [`Node::Conditional`].
//! - `for` / `for (( ; ; ))` / `select` / `while` / `until` → [`Node::Loop`].
//! - `case` → [`Node::Switch`]; one [`SwitchCase`] per `pattern)` item (a
//!   `a | b)` item listing several patterns is one decision), and an item
//!   whose only pattern is the unquoted `*` is the non-decision `default` arm.
//! - `&&` / `||` → folded [`Node::Logical`], both between commands
//!   (`cmd1 && cmd2 || cmd3`) and inside `[[ … ]]` / `(( … ))` expressions.
//!   The guard idiom `cmd || return` is therefore a logical sequence like any
//!   other. `!` adds nothing (matching SonarSource's `not`).
//! - `break N` / `continue N` with a level other than `1` jump out of several
//!   loops at once — the shell's labelled jump — and → `Node::Jump { labeled:
//!   true }`; a plain `break` / `continue` scores nothing.
//! - Every simple command → [`Node::Call`] with its command name, for recursion
//!   detection (a function that runs itself).
//!
//! Subshells, command substitutions, pipelines, redirections, here-documents
//! and parameter expansions are transparent (code inside them still counts).
//! `${var:-default}`-style expansions are not scored as coalescing: in shell
//! they are the routine way to read a possibly-unset variable rather than a
//! decision written by hand. The deprecated `[ a -a b ]` / `[ a -o b ]` test
//! operators are not scored either.
//!
//! [Bats]: https://github.com/bats-core/bats-core
//! [tree-sitter]: https://tree-sitter.github.io/

use std::path::Path;

use cccc_core::guard;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given): shell
/// scripts and Bats test files.
pub const DEFAULT_EXTS: &[&str] = &["sh", "bash", "bats"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    guard::analyze(path, source, to_ir)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any syntax-error messages. tree-sitter always yields a tree (it
/// recovers from errors by inserting `ERROR`/`MISSING` nodes), so we still lower
/// what parsed and report the error locations alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_bats::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load shell grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse shell source".to_string()]);
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
    if !node.has_error() {
        return;
    }
    if node.is_error() || node.is_missing() {
        let msg = format!("syntax error at line {}", node.start_position().row + 1);
        if !out.contains(&msg) {
            out.push(msg);
        }
    }
    let mut cursor = node.walk();
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
    /// comments) — the "transparent" step shared by every arm that carries no
    /// score of its own.
    fn visit_named_children(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !child.is_extra() {
                self.visit(child);
            }
        }
    }

    /// Visit each node of `nodes` into a fresh collector.
    fn collect_all(&mut self, nodes: &[TsNode]) -> Vec<Node> {
        self.collect(|b| {
            for n in nodes {
                b.visit(*n);
            }
        })
    }

    /// A function-like unit: emit a `Function` whose body walks *all* named
    /// children, scored in its own frame.
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
                let kind = if has_test_marker(node) {
                    "test"
                } else {
                    "function"
                };
                self.emit_function_node(name, kind, node);
            }
            "test_block" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.test_name(n))
                    .unwrap_or_else(|| "<test>".into());
                self.emit_function_node(name, "test", node);
            }

            "if_statement" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "ternary_expression" => self.visit_ternary(node),
            "for_statement" | "c_style_for_statement" | "while_statement" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }
            "case_statement" => self.visit_case(node),

            "list" | "binary_expression" => match logical_op_of(node) {
                Some(op) => self.visit_logical(node, op),
                None => self.visit_named_children(node),
            },

            "command" => self.visit_command(node),

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// A Bats test's name, as Bats reads it: the text between `@test` and `{`,
    /// with the quotes of a single quoted string removed.
    fn test_name(&self, node: TsNode) -> String {
        if let [only] = named_children(node).as_slice()
            && matches!(only.kind(), "string" | "raw_string")
        {
            let text = self.text(*only);
            if text.len() >= 2 {
                return text[1..text.len() - 1].to_string();
            }
        }
        self.text(node).trim().to_string()
    }

    /// Build a `Branch` from an `if_statement`. Its children are the
    /// `condition` field (one or more commands), then the `then` body as
    /// field-less statements, then a run of `elif_clause`s optionally ending in
    /// an `else_clause`.
    fn lower_if(&mut self, node: TsNode) -> Node {
        let mut test = Vec::new();
        let mut then = Vec::new();
        let mut alternatives = Vec::new();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() && !child.is_extra() {
                    if cursor.field_name() == Some("condition") {
                        test.push(child);
                    } else if matches!(child.kind(), "elif_clause" | "else_clause") {
                        alternatives.push(child);
                    } else {
                        then.push(child);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        let test = self.collect_all(&test);
        let then = self.collect_all(&then);
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
            // The grammar puts no fields on an `elif_clause`: its condition is
            // everything before the `then` keyword, its body everything after.
            let mut test = Vec::new();
            let mut then = Vec::new();
            let mut seen_then = false;
            let mut cursor = first.walk();
            for child in first.children(&mut cursor) {
                if !child.is_named() {
                    seen_then |= child.kind() == "then";
                } else if !child.is_extra() {
                    if seen_then {
                        then.push(child);
                    } else {
                        test.push(child);
                    }
                }
            }
            let test = self.collect_all(&test);
            let then = self.collect_all(&then);
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

    /// The arithmetic ternary `cond ? a : b`: a single increment.
    fn visit_ternary(&mut self, node: TsNode) {
        let field = |b: &mut Self, name: &str| {
            node.child_by_field_name(name)
                .map_or_else(Vec::new, |c| b.collect(|b| b.visit(c)))
        };
        let test = field(self, "condition");
        let then = field(self, "consequence");
        let alternate = field(self, "alternative");
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    /// A `case` becomes a `Switch`: one `SwitchCase` per `case_item`. The
    /// subject word runs at the switch's own level first.
    fn visit_case(&mut self, node: TsNode) {
        let mut cases = Vec::new();
        for child in named_children(node) {
            if child.kind() == "case_item" {
                let is_default = self.is_catch_all(child);
                let body = self.collect(|b| b.visit_named_children(child));
                cases.push(SwitchCase { is_default, body });
            } else {
                self.visit(child);
            }
        }
        self.emit(Node::Switch { cases });
    }

    /// True for a catch-all `*)` item: its only pattern is an unquoted `*`
    /// (a quoted `"*")` matches a literal asterisk).
    fn is_catch_all(&self, item: TsNode) -> bool {
        let mut cursor = item.walk();
        let patterns: Vec<TsNode> = item.children_by_field_name("value", &mut cursor).collect();
        matches!(patterns.as_slice(), [only]
            if matches!(only.kind(), "extglob_pattern" | "word") && self.text(*only) == "*")
    }

    /// One folded [`Node::Logical`] for a run of like operators (`&&` / `||`).
    /// A different operator nested inside starts a fresh `Logical`.
    fn visit_logical(&mut self, node: TsNode, op: LogicalOp) {
        let mut operands = Vec::new();
        for side in logical_operands(node) {
            self.collect_logical_side(side, op, &mut operands);
        }
        self.emit(Node::Logical { op, operands });
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    fn collect_logical_side(&mut self, side: TsNode, op: LogicalOp, operands: &mut Vec<Node>) {
        let side = unwrap_parens(side);
        match logical_op_of(side) {
            Some(side_op) if side_op == op => {
                for k in logical_operands(side) {
                    self.collect_logical_side(k, op, operands);
                }
            }
            Some(side_op) => {
                let mut sub = Vec::new();
                for k in logical_operands(side) {
                    self.collect_logical_side(k, side_op, &mut sub);
                }
                operands.push(Node::Logical {
                    op: side_op,
                    operands: sub,
                });
            }
            None => operands.push(Node::Group(self.collect(|b| b.visit(side)))),
        }
    }

    /// A simple command. `break N` / `continue N` leaving more than one loop
    /// is a labelled jump; any other command is a call (named for recursion
    /// detection). Its arguments are walked either way — they may hold
    /// command substitutions.
    fn visit_command(&mut self, node: TsNode) {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| named_children(n).into_iter().next())
            .filter(|n| n.kind() == "word")
            .map(|n| self.text(n));
        match name {
            Some("break" | "continue") => {
                let mut cursor = node.walk();
                let level = node
                    .children_by_field_name("argument", &mut cursor)
                    .next()
                    .map(|a| self.text(a));
                if level.is_some_and(|l| l != "1") {
                    self.emit(Node::Jump { labeled: true });
                }
            }
            _ => {
                let callee = name.map(str::to_string);
                self.emit(Node::Call { callee });
            }
        }
        self.visit_named_children(node);
    }
}

/// True if `node` (a `function_definition`) opens with Bats' `# @test` marker
/// comment, which makes Bats run it as a test.
fn has_test_marker(node: TsNode) -> bool {
    node.child_by_field_name("body").is_some_and(|body| {
        let mut cursor = body.walk();
        body.named_children(&mut cursor)
            .any(|c| c.kind() == "test_marker_comment")
    })
}

/// The named children of `node` (skipping `extras`, i.e. comments), collected
/// into a `Vec` so the caller can index or slice-match them without holding the
/// cursor's borrow.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}

/// The normalized logical operator a `list` (`cmd && cmd`) or a
/// `binary_expression` (`[[ a && b ]]`, `(( a || b ))`) represents, if any.
/// A `list`'s operator is an anonymous child; a `binary_expression` has it in
/// its `operator` field.
fn logical_op_of(node: TsNode) -> Option<LogicalOp> {
    let op = match node.kind() {
        "list" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|c| !c.is_named())
                .map(|c| c.kind())
        }
        "binary_expression" => node.child_by_field_name("operator").map(|o| o.kind()),
        _ => None,
    };
    match op {
        Some("&&") => Some(LogicalOp::And),
        Some("||") => Some(LogicalOp::Or),
        _ => None,
    }
}

/// The two sides of a logical `list` / `binary_expression`.
fn logical_operands(node: TsNode) -> Vec<TsNode> {
    named_children(node)
}

/// Follow a single-child `parenthesized_expression` to the inner expression so
/// `[[ a && (b && c) ]]` folds into one run.
fn unwrap_parens(node: TsNode) -> TsNode {
    if node.kind() == "parenthesized_expression"
        && let [inner] = named_children(node).as_slice()
    {
        return unwrap_parens(*inner);
    }
    node
}

#[cfg(test)]
mod tests;
