//! C# adapter: parses source with the [tree-sitter]
//! `WillBooster/tree-sitter-c-sharp` grammar (a fork of the official
//! `tree-sitter/tree-sitter-c-sharp` that fixes many real-world parse failures
//! — method-call and pointer assignment targets, null-conditional assignment,
//! C# 14 extension members, file-based app directives, …) and lowers the
//! concrete syntax tree into the language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the C# grammar (whose C source is compiled by `cc`, so there is no
//! `libclang`/bindgen requirement), with no CLI machinery. The unified `cccc`
//! binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.cs`/`.csx` files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about and emits the matching IR nodes. All complexity rules
//! live in [`cccc_core::engine`].
//!
//! ## Why a `kind()`-dispatch with a full-recursion default
//!
//! tree-sitter does not offer a "walk every child" visitor trait, so lowering is
//! driven by an explicit recursion. [`Builder::visit`] matches only the node
//! kinds that produce IR and its **default arm recurses into every named
//! child**, so an unrecognized construct is transparent rather than dropped.
//! The IR tree is assembled with a stack of "collectors": [`Builder::collect`]
//! pushes a fresh child vector, runs a sub-traversal, and pops the nodes it
//! gathered.
//!
//! ## C#-to-IR mapping notes
//!
//! - method, constructor, destructor, operator, conversion operator, local
//!   function, lambda, anonymous method (`delegate { … }`), and property /
//!   indexer / event accessors (`get`/`set`/`init`/`add`/`remove`, plus the
//!   expression-bodied `int P => …` getter) → [`Node::Function`]. Bodyless
//!   declarations (abstract/interface members, `extern`/`partial` signatures,
//!   auto-property `get;`/`set;`) have no code and are not reported.
//!   Top-level statements (and `.csx` scripts) score at the module level.
//! - `if` statement → [`Node::Branch`] (`else if` chains as a nested `Branch`).
//! - `#if`/`#elif`/`#else` → a [`Node::Branch`] chain, like the C/C++
//!   adapters: a reader has to hold every compilation variant in mind.
//!   `#region` and other directives are transparent.
//! - ternary `?:` → [`Node::Conditional`].
//! - `switch` statement and `switch` expression → [`Node::Switch`]; the
//!   `default:` section, and a switch-expression arm whose pattern is the
//!   discard `_` or a `var` pattern without a `when` guard, are the
//!   non-decision default arm.
//! - `for` / `foreach` / `while` / `do`-`while` → [`Node::Loop`].
//! - one [`Node::Catch`] per `catch` clause (an exception filter `when (…)`
//!   scores inside it; `try` and `finally` bodies run at the surrounding level).
//! - `goto` (to a label, `case`, or `default`) → [`Node::Jump`] with
//!   `labeled: true`; C# `break` / `continue` never take a label.
//! - `&&` / `||` / `??` runs and `??=` → folded [`Node::Logical`]; the pattern
//!   combinators `and` / `or` (`x is 'a' or 'b'`) fold the same way, since they
//!   replace `&&` / `||` in conditions. `not` patterns are transparent, like
//!   `!`.
//! - each null-conditional access `?.` / `?[]` → one [`Node::NullGuard`].
//! - invocations → [`Node::Call`] for recursion detection (`base.M()` names the
//!   overridden member, so it is never counted as recursion).
//!
//! `.csx` scripts may start with `#r` / `#load` directives, which the grammar
//! does not know; they are blanked out (offsets and line numbers preserved)
//! before parsing.

use std::borrow::Cow;
use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["cs", "csx"];

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
pub fn to_ir(path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load C# grammar".to_string()]);
    }
    let source = if path.extension().is_some_and(|e| e == "csx") {
        blank_script_directives(source)
    } else {
        Cow::Borrowed(source)
    };
    let Some(tree) = parser.parse(source.as_ref(), None) else {
        return (Vec::new(), vec!["failed to parse C# source".to_string()]);
    };

    let src = source.as_bytes();
    let mut errors = Vec::new();
    collect_errors(tree.root_node(), &mut errors);

    let mut builder = Builder::new(src);
    builder.visit(tree.root_node());
    (builder.finish(), errors)
}

/// Replace the leading `#r "…"` / `#load "…"` script directives of a `.csx`
/// file with spaces. They may only precede the first token of a script (after
/// an optional shebang, blank lines, and comments), so the scan stops at the
/// first other line. Byte offsets — and thus line numbers — are unchanged.
fn blank_script_directives(source: &str) -> Cow<'_, str> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim_start_matches('\u{feff}').trim_start();
        let is_directive = ["#r", "#load"].iter().any(|d| {
            trimmed
                .strip_prefix(d)
                .is_some_and(|rest| rest.starts_with([' ', '\t', '"']))
        });
        if is_directive {
            let start = offset + (content.len() - trimmed.len());
            ranges.push(start..offset + content.len());
        } else if !(trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("#!")) {
            break;
        }
        offset += line.len();
    }
    if ranges.is_empty() {
        return Cow::Borrowed(source);
    }
    let mut out = source.to_string();
    for range in ranges {
        // Directive text is ASCII-led; replace byte-for-byte with spaces (any
        // multibyte char inside the string literal becomes several spaces, which
        // keeps every later byte offset intact).
        let spaces = " ".repeat(range.len());
        out.replace_range(range, &spaces);
    }
    Cow::Owned(out)
}

/// Collect the 1-based lines of every `ERROR`/`MISSING` node so a partially
/// parsed file surfaces its syntax problems (deduplicated, order preserved).
fn collect_errors(node: TsNode, out: &mut Vec<String>) {
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
    /// comments). This is the "transparent" step shared by every arm that
    /// carries no score of its own.
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

    /// Like [`Self::emit_function_node`], but a declaration without a `body`
    /// (abstract/interface member, `extern`/`partial` signature, auto-property
    /// accessor) has no code to score and is not a unit. Its parameters still
    /// run at the surrounding level (a default value may hold an expression).
    fn emit_declaration(&mut self, name: String, kind: &'static str, node: TsNode) {
        if node.child_by_field_name("body").is_some() {
            self.emit_function_node(name, kind, node);
        } else {
            self.visit_named_children(node);
        }
    }

    /// The nodes an optional sub-node lowers to (empty when it is absent).
    fn collect_opt(&mut self, node: Option<TsNode>) -> Vec<Node> {
        node.map_or_else(Vec::new, |n| self.collect(|b| b.visit(n)))
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
                self.emit_declaration(name, "method", node);
            }
            "conditional_method_declaration" => self.visit_conditional_method(node),
            "constructor_declaration" => {
                let name = self.name_of(node).unwrap_or_else(|| "<constructor>".into());
                self.emit_declaration(name, "constructor", node);
            }
            "destructor_declaration" => {
                let name = self.name_of(node).unwrap_or_default();
                self.emit_declaration(format!("~{name}"), "destructor", node);
            }
            "operator_declaration" => {
                let op = node
                    .child_by_field_name("operator")
                    .map_or("", |o| self.text(o));
                self.emit_declaration(format!("operator {op}"), "operator", node);
            }
            "conversion_operator_declaration" => {
                let ty = node
                    .child_by_field_name("type")
                    .map_or("", |t| self.text(t));
                self.emit_declaration(format!("operator {ty}"), "operator", node);
            }
            "local_function_statement" => {
                let name = self.name_of(node).unwrap_or_else(|| "<function>".into());
                self.emit_declaration(name, "function", node);
            }
            "lambda_expression" => self.emit_function_node("<lambda>".into(), "lambda", node),
            "anonymous_method_expression" => {
                self.emit_function_node("<delegate>".into(), "delegate", node);
            }
            "property_declaration" | "event_declaration" => {
                let name = self.name_of(node).unwrap_or_else(|| "<property>".into());
                self.visit_property(node, name);
            }
            "indexer_declaration" => self.visit_property(node, "this[]".into()),

            "if_statement" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "preproc_if" => {
                let branch = self.lower_preproc(node);
                self.emit(branch);
            }
            "conditional_expression" => self.visit_ternary(node),
            "switch_statement" => self.visit_switch_statement(node),
            "switch_expression" => self.visit_switch_expression(node),
            "for_statement" | "foreach_statement" | "while_statement" | "do_statement" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }
            "catch_clause" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Catch { body });
            }
            "goto_statement" => {
                self.emit(Node::Jump { labeled: true });
                self.visit_named_children(node);
            }
            "break_statement" | "continue_statement" => self.emit(Node::Jump { labeled: false }),

            "binary_expression" | "and_pattern" | "or_pattern" => match logical_op_of(node) {
                Some(op) => {
                    let logical = self.lower_logical(node, op);
                    self.emit(logical);
                }
                None => self.visit_named_children(node),
            },
            "assignment_expression" if is_coalesce_assignment(node) => {
                self.visit_coalesce_assignment(node);
            }
            "conditional_access_expression" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::NullGuard { body });
            }

            "invocation_expression" => self.visit_call(node),

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// A property / indexer / event: each accessor with a body (`get`, `set`,
    /// `init`, `add`, `remove`) is a unit named after the member, and an
    /// expression-bodied member (`int P => …;`) is an implicit getter. An
    /// auto-property initializer (`int P { get; } = …;`) runs at the
    /// surrounding level.
    fn visit_property(&mut self, node: TsNode, name: String) {
        for child in named_children(node) {
            match child.kind() {
                "accessor_list" => {
                    for accessor in named_children(child) {
                        if accessor.kind() == "accessor_declaration" {
                            self.emit_declaration(name.clone(), accessor_kind(accessor), accessor);
                        } else {
                            self.visit(accessor);
                        }
                    }
                }
                "arrow_expression_clause" => self.emit_function_node(name.clone(), "getter", child),
                _ => self.visit(child),
            }
        }
    }

    /// A method whose signature varies by `#if` but shares one body
    /// (`#if A void M(int x) #else void M(long x) #endif { … }`): one unit named
    /// after the first signature, whose `#if` chain scores inside it.
    fn visit_conditional_method(&mut self, node: TsNode) {
        let name = first_descendant(node, "method_signature")
            .and_then(|sig| self.name_of(sig))
            .unwrap_or_else(|| "<method>".into());
        self.emit_function_node(name, "method", node);
    }

    /// Build a `Branch` from an `if_statement` (recursively, so an `else if`
    /// becomes a nested `Branch` and thus scores flat). Parts are addressed by
    /// field, not position, so interleaved comments cannot shift them.
    fn lower_if(&mut self, node: TsNode) -> Node {
        let field = |name| node.child_by_field_name(name);
        let test = self.collect_opt(field("condition"));
        let then = self.collect_opt(field("consequence"));
        let alternate = field("alternative").map(|alt| {
            Box::new(if alt.kind() == "if_statement" {
                self.lower_if(alt)
            } else {
                Node::Group(self.collect(|b| b.visit(alt)))
            })
        });
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// Build a `Branch` from `#if` / `#elif`: the directive's own body is the
    /// `then`, and the `alternative` field chains — an `#elif` nests as another
    /// `Branch` (scoring flat, like `else if`), an `#else` closes the chain as a
    /// `Group`.
    fn lower_preproc(&mut self, node: TsNode) -> Node {
        let test = self.collect_opt(node.child_by_field_name("condition"));
        let then = self.collect(|b| {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    let is_part = matches!(cursor.field_name(), Some("condition" | "alternative"));
                    if child.is_named() && !child.is_extra() && !is_part {
                        b.visit(child);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        });
        let alternate = node.child_by_field_name("alternative").map(|a| {
            Box::new(match a.kind() {
                "preproc_elif" => self.lower_preproc(a),
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

    /// A ternary `cond ? a : b` becomes a `Conditional`.
    fn visit_ternary(&mut self, node: TsNode) {
        let field = |name| node.child_by_field_name(name);
        let test = self.collect_opt(field("condition"));
        let then = self.collect_opt(field("consequence"));
        let alternate = self.collect_opt(field("alternative"));
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    /// A `switch` statement: the governing value runs at the switch's own
    /// level, then one `SwitchCase` per `switch_section`. The grammar gives each
    /// label its own section (`case 1: case 2:` is two sections, the first with
    /// no statements), so stacked labels score one decision each — like C's
    /// fall-through cases. The `default:` label is an anonymous keyword token.
    fn visit_switch_statement(&mut self, node: TsNode) {
        if let Some(value) = node.child_by_field_name("value") {
            self.visit(value);
        }
        let mut cases = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            for section in named_children(body) {
                if section.kind() == "switch_section" {
                    let is_default = has_direct_child(section, "default");
                    let body = self.collect(|b| b.visit_named_children(section));
                    cases.push(SwitchCase { is_default, body });
                } else {
                    self.visit(section);
                }
            }
        }
        self.emit(Node::Switch { cases });
    }

    /// A `switch` expression: the governing value (the first named child) runs
    /// at the switch's own level, then one `SwitchCase` per arm. An unguarded
    /// discard `_` or `var` pattern arm matches everything and is the default.
    fn visit_switch_expression(&mut self, node: TsNode) {
        let mut cases = Vec::new();
        for child in named_children(node) {
            if child.kind() == "switch_expression_arm" {
                let is_default = child.named_child(0).is_some_and(is_catch_all_pattern)
                    && !has_direct_child(child, "when_clause");
                let body = self.collect(|b| b.visit_named_children(child));
                cases.push(SwitchCase { is_default, body });
            } else {
                self.visit(child);
            }
        }
        self.emit(Node::Switch { cases });
    }

    /// One folded [`Node::Logical`] for a run of like operators (`&&`, `||`,
    /// `??`, or the `and` / `or` pattern combinators). A different operator
    /// nested inside starts a fresh `Logical`.
    fn lower_logical(&mut self, node: TsNode, op: LogicalOp) -> Node {
        let mut operands = Vec::new();
        for side in operand_children(node) {
            self.collect_logical_side(side, op, &mut operands);
        }
        Node::Logical { op, operands }
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    fn collect_logical_side(&mut self, side: TsNode, op: LogicalOp, operands: &mut Vec<Node>) {
        let side = unwrap_parens(side);
        match logical_op_of(side) {
            Some(side_op) if side_op == op => {
                for k in operand_children(side) {
                    self.collect_logical_side(k, op, operands);
                }
            }
            Some(side_op) => {
                let nested = self.lower_logical(side, side_op);
                operands.push(nested);
            }
            None => operands.push(Node::Group(self.collect(|b| b.visit(side)))),
        }
    }

    /// `a ??= b` is a coalescing run of its two sides (like Dart's `??=`).
    fn visit_coalesce_assignment(&mut self, node: TsNode) {
        let mut operands = Vec::new();
        for field in ["left", "right"] {
            if let Some(child) = node.child_by_field_name(field) {
                operands.push(Node::Group(self.collect(|b| b.visit(child))));
            }
        }
        self.emit(Node::Logical {
            op: LogicalOp::Coalesce,
            operands,
        });
    }

    /// Emit a `Call` (with the invoked member's simple name for recursion
    /// detection), then recurse into the target and arguments (which may
    /// contain further constructs, e.g. a lambda argument).
    fn visit_call(&mut self, node: TsNode) {
        let callee = node
            .child_by_field_name("function")
            .and_then(|f| self.callee_name(f));
        self.emit(Node::Call { callee });
        self.visit_named_children(node);
    }

    /// The simple name an invocation target resolves to: `M`, `M<T>`,
    /// `this.M`, `obj.M`, `obj?.M`. `base.M` calls the overridden member, not
    /// the enclosing one, so it yields `None` (never recursion).
    fn callee_name(&self, target: TsNode) -> Option<String> {
        match target.kind() {
            "identifier" => Some(self.text(target).to_string()),
            "generic_name" => target.named_child(0).map(|c| self.text(c).to_string()),
            "member_access_expression" => {
                let receiver = target.child_by_field_name("expression");
                if receiver.is_some_and(|r| self.text(r) == "base") {
                    return None;
                }
                target
                    .child_by_field_name("name")
                    .and_then(|n| self.callee_name(n))
            }
            // `obj?.M(..)`: the target is the whole conditional access, whose
            // trailing member binding names the method.
            "conditional_access_expression" => named_children(target)
                .into_iter()
                .rev()
                .find(|c| c.kind() == "member_binding_expression")
                .and_then(|m| self.callee_name(m)),
            "member_binding_expression" => target
                .child_by_field_name("name")
                .and_then(|n| self.callee_name(n)),
            _ => None,
        }
    }
}

/// The named children of `node` (skipping `extras` such as comments), collected
/// into a `Vec` so the caller can index or slice-match them.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}

/// The unit kind of an `accessor_declaration`, from its `get` / `set` / `init` /
/// `add` / `remove` keyword, found among the direct children: it is usually the
/// `name` field, but the grammar leaves it unnamed in an explicit interface
/// implementation (`int I.P { get … }`).
fn accessor_kind(accessor: TsNode) -> &'static str {
    let mut cursor = accessor.walk();
    let keyword = accessor.children(&mut cursor).find_map(|c| match c.kind() {
        "get" => Some("getter"),
        "set" => Some("setter"),
        "init" => Some("init"),
        "add" => Some("add"),
        "remove" => Some("remove"),
        _ => None,
    });
    keyword.unwrap_or("accessor")
}

/// True for a pattern that matches every value: the discard `_`, `var x`
/// (a `declaration_pattern` typed `var`), or `var (a, b)`.
fn is_catch_all_pattern(pattern: TsNode) -> bool {
    match pattern.kind() {
        "discard" | "var_pattern" => true,
        "declaration_pattern" => pattern
            .child_by_field_name("type")
            .is_some_and(|t| t.kind() == "implicit_type"),
        _ => false,
    }
}

/// The normalized logical operator a node represents, if any: `&&` / `||` /
/// `??` on a `binary_expression`, or an `and` / `or` pattern combinator.
fn logical_op_of(node: TsNode) -> Option<LogicalOp> {
    match node.kind() {
        "and_pattern" => Some(LogicalOp::And),
        "or_pattern" => Some(LogicalOp::Or),
        "binary_expression" => match node.child_by_field_name("operator").map(|o| o.kind()) {
            Some("&&") => Some(LogicalOp::And),
            Some("||") => Some(LogicalOp::Or),
            Some("??") => Some(LogicalOp::Coalesce),
            _ => None,
        },
        _ => None,
    }
}

fn is_coalesce_assignment(node: TsNode) -> bool {
    node.child_by_field_name("operator")
        .is_some_and(|op| op.kind() == "??=")
}

/// The `left` / `right` operands of a binary expression or pattern combinator.
fn operand_children(node: TsNode) -> Vec<TsNode> {
    ["left", "right"]
        .iter()
        .filter_map(|f| node.child_by_field_name(f))
        .collect()
}

/// True if `node` has a direct (possibly anonymous) child of `kind`.
fn has_direct_child(node: TsNode, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| c.kind() == kind)
}

/// The first descendant (pre-order) of `node` with the given kind.
fn first_descendant<'t>(node: TsNode<'t>, kind: &str) -> Option<TsNode<'t>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(found) = first_descendant(child, kind) {
            return Some(found);
        }
    }
    None
}

/// Follow a single-child `parenthesized_expression` / `parenthesized_pattern`
/// to its inner node so `a && (b && c)` folds into one run.
fn unwrap_parens(node: TsNode) -> TsNode {
    if matches!(
        node.kind(),
        "parenthesized_expression" | "parenthesized_pattern"
    ) && let [inner] = named_children(node).as_slice()
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
        analyze_source(Path::new("Test.cs"), src)
    }

    /// Every unit of the report, depth-first in source order.
    fn flatten(fns: &[FunctionReport]) -> Vec<&FunctionReport> {
        fns.iter()
            .flat_map(|f| std::iter::once(f).chain(flatten(&f.children)))
            .collect()
    }

    fn function<'a>(report: &'a FileReport, name: &str) -> &'a FunctionReport {
        flatten(&report.functions)
            .into_iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("function {name} not found"))
    }

    /// `(cognitive, cyclomatic)` of the named unit, asserting a clean parse.
    fn scores(src: &str, name: &str) -> (u32, u32) {
        assert_clean(src);
        let report = analyze(src);
        let f = function(&report, name);
        (f.cognitive, f.cyclomatic)
    }

    fn parse_errors(src: &str) -> Vec<String> {
        to_ir(Path::new("T.cs"), src).1
    }

    fn assert_clean(src: &str) {
        let errors = parse_errors(src);
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// `name:kind` of every unit, depth-first in source order.
    fn units(src: &str) -> Vec<String> {
        flatten(&analyze(src).functions)
            .iter()
            .map(|f| format!("{}:{}", f.name, f.kind))
            .collect()
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        let src = r#"
            class C {
                static int SumOfPrimes(int max) {
                    int total = 0;
                    for (int i = 2; i <= max; ++i) {
                        for (int j = 2; j < i; ++j) {
                            if (i % j == 0) {
                                goto Next;
                            }
                        }
                        total += i;
                    Next:;
                    }
                    return total;
                }
            }
        "#;
        // for(+1) + nested for(+2) + nested if(+3) + goto(+1) = 7;
        // base 1 + for + for + if = 4
        assert_eq!(scores(src, "SumOfPrimes"), (7, 4));
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
            class C {
                string GetWords(int number) {
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
        // base 1 + 2 non-default cases = 3
        assert_eq!(scores(src, "GetWords"), (1, 3));
    }

    #[test]
    fn stacked_case_labels_each_count_like_c_fallthrough() {
        let src = r#"
            class C {
                int F(int x) {
                    switch (x) {
                        case 1:
                        case 2:
                            return 1;
                        default:
                            return 0;
                    }
                }
            }
        "#;
        assert_eq!(scores(src, "F"), (1, 3));
    }

    #[test]
    fn switch_expression_with_discard_default() {
        let src = r#"
            class C {
                string F(object o) => o switch {
                    int n when n > 0 => "positive",
                    null => "null",
                    _ => "other",
                };
            }
        "#;
        // switch +1; base 1 + two non-default arms = 3
        assert_eq!(scores(src, "F"), (1, 3));
    }

    #[test]
    fn guarded_or_non_catch_all_arms_are_decisions() {
        let src = r#"
            class C {
                int F(object o) => o switch {
                    var x when x is null => 0,
                    var y => 1,
                };
                int G((int, int) t) => t switch {
                    (0, 0) => 0,
                    var (a, b) => a + b,
                };
            }
        "#;
        // `var x when …` is a decision, the unguarded `var y` is the default.
        assert_eq!(scores(src, "F"), (1, 2));
        assert_eq!(scores(src, "G"), (1, 2));
    }

    #[test]
    fn if_else_if_else_chains_flat() {
        let src = r#"
            class C {
                void F(int a) {
                    if (a == 1) { } else if (a == 2) { } else { }
                }
            }
        "#;
        // if +1, else if +1, else +1; base 1 + if + else if = 3
        assert_eq!(scores(src, "F"), (3, 3));
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
            class C {
                void F(bool a, bool b, bool c) {
                    if (a) { if (b) { if (c) { } } }
                }
            }
        "#;
        assert_eq!(scores(src, "F"), (6, 4));
    }

    #[test]
    fn ternary_is_conditional() {
        let src = "class C { int F(bool a) => a ? 1 : 2; }";
        assert_eq!(scores(src, "F"), (1, 2));
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            class C {
                void F(bool a, int[] xs) {
                    for (;;) { }
                    foreach (var x in xs) { }
                    while (a) { }
                    do { } while (a);
                }
            }
        "#;
        assert_eq!(scores(src, "F"), (4, 5));
    }

    #[test]
    fn catch_clauses_count_and_filters_score_inside() {
        let src = r#"
            class C {
                void F() {
                    try { }
                    catch (IOException e) when (e.HResult == 1 && Log(e)) { }
                    catch (Exception) { }
                    finally { }
                }
            }
        "#;
        // two catches +2, the filter's `&&` +1; base 1 + 2 catches + 1 extra && operand
        assert_eq!(scores(src, "F"), (3, 4));
    }

    #[test]
    fn plain_break_and_continue_are_free() {
        let src = r#"
            class C {
                void F(int[] xs) {
                    foreach (var x in xs) { if (x > 0) continue; break; }
                }
            }
        "#;
        // foreach +1, nested if +2
        assert_eq!(scores(src, "F"), (3, 3));
    }

    #[test]
    fn logical_operators_fold_by_kind() {
        let src = r#"
            class C {
                bool F(bool a, bool b, bool c, bool d) => a && b && (c || d);
            }
        "#;
        // one && run + one || run; base 1 + 2 extra && operands + 1 extra || operand
        assert_eq!(scores(src, "F"), (2, 4));
    }

    #[test]
    fn coalescing_operators_fold() {
        let src = r#"
            class C {
                string F(string a, string b, string c) {
                    var x = a ?? b ?? c;
                    x ??= "d";
                    return x;
                }
            }
        "#;
        // `??` run +1 (2 extra operands), `??=` run +1 (1 extra operand)
        assert_eq!(scores(src, "F"), (2, 4));
    }

    #[test]
    fn pattern_combinators_fold_like_logical_operators() {
        let src = r#"
            class C {
                bool IsVowel(char c) => c is 'a' or 'e' or 'i' or 'o' or 'u';
                bool Mixed(object o) => o is int and > 0 or string;
                bool Negated(object o) => o is not null;
                bool Equivalent(char c) => c == 'a' || c == 'e' || c == 'i' || c == 'o' || c == 'u';
            }
        "#;
        // one `or` run of five patterns, same as the `||` spelling
        assert_eq!(scores(src, "IsVowel"), (1, 5));
        assert_eq!(scores(src, "Equivalent"), (1, 5));
        // `or` run containing an `and` run
        assert_eq!(scores(src, "Mixed"), (2, 3));
        // `not` is free, like `!`
        assert_eq!(scores(src, "Negated"), (0, 1));
    }

    #[test]
    fn pattern_combinators_in_switch_arms() {
        let src = r#"
            class C {
                int F(int x) => x switch {
                    1 or 2 or 3 => 1,
                    > 10 and < 20 => 2,
                    _ => 0,
                };
            }
        "#;
        // switch +1, `or` run +1, `and` run +1;
        // base 1 + 2 non-default arms + 2 extra `or` operands + 1 extra `and` operand
        assert_eq!(scores(src, "F"), (3, 6));
    }

    #[test]
    fn null_conditional_access_adds_only_cyclomatic_paths() {
        let src = r#"
            class C {
                void F(Node n) {
                    var a = n?.Next?.Value;
                    var b = n?.Items?[0];
                    n?.Run();
                }
            }
        "#;
        assert_eq!(scores(src, "F"), (0, 6)); // base + five explicit guards
    }

    #[test]
    fn preprocessor_conditionals_branch_like_c() {
        let src = r#"
            class C {
                void F() {
            #if DEBUG
                    Log();
            #elif TRACE && VERBOSE
                    Trace();
            #else
                    Nop();
            #endif
                }
            }
        "#;
        // #if +1, #elif +1, #else +1, `&&` in the #elif condition +1;
        // base 1 + #if + #elif + 1 extra && operand
        assert_eq!(scores(src, "F"), (4, 4));
    }

    #[test]
    fn preprocessor_around_case_labels_or_accessors_is_a_known_wart() {
        // Every tree-sitter C# grammar (official included) only accepts `#if`
        // where a statement or member may stand, so one wrapping a `case`
        // label or an accessor surfaces as a parse warning. The rest of the
        // file still lowers: both members remain units.
        let src = r#"
            class C {
                int F(int x) {
                    switch (x) {
                        case 1: return 1;
            #if NET
                        case 2: return 2;
            #endif
                        default: return 0;
                    }
                }
                int P {
                    get { return 1; }
            #if NET
                    set { }
            #endif
                }
            }
        "#;
        assert!(!parse_errors(src).is_empty());
        let names = units(src);
        assert!(names.contains(&"F:method".to_string()), "{names:?}");
        assert!(names.contains(&"P:getter".to_string()), "{names:?}");
    }

    #[test]
    fn conditional_signature_with_shared_body_is_one_method() {
        let src = r#"
            class C {
            #if NET
                public static int Pick(int a, int b)
            #else
                public static int Pick(int a, int b, int c)
            #endif
                {
                    if (a > b) { return a; }
                    return b;
                }
            }
        "#;
        // #if/#else around the signatures +2, the body's `if` +1
        assert_eq!(scores(src, "Pick"), (3, 3));
        assert_eq!(units(src), ["Pick:method"]);
    }

    #[test]
    fn region_directives_are_transparent() {
        let src = r#"
            class C {
                #region Helpers
                void F() { }
                #endregion
            }
        "#;
        assert_eq!(scores(src, "F"), (0, 1));
    }

    #[test]
    fn preprocessor_around_members_scores_at_module_level() {
        let src = r#"
            class C {
            #if NET
                void F() { if (true) { } }
            #endif
            }
        "#;
        assert_eq!(scores(src, "F"), (1, 2));
        assert_eq!(analyze(src).functions.len(), 1);
    }

    #[test]
    fn recursion_counts_but_base_call_does_not() {
        let src = r#"
            class C : B {
                int Fib(int n) => n < 2 ? n : Fib(n - 1) + this.Fib(n - 2);
                public override void Run() { base.Run(); }
                T Generic<T>(int n) => n == 0 ? default : Generic<T>(n - 1);
                void Maybe(C c) { c?.Maybe(null); }
            }
        "#;
        // ternary +1, two recursive calls +1 each (one cognitive point per call)
        let report = analyze(src);
        assert_eq!(function(&report, "Fib").cognitive, 3);
        assert_eq!(function(&report, "Run").cognitive, 0);
        assert_eq!(function(&report, "Generic").cognitive, 2);
        assert_eq!(function(&report, "Maybe").cognitive, 1);
    }

    #[test]
    fn function_like_units_are_reported() {
        let src = r#"
            class C {
                C() { }
                ~C() { }
                void M() {
                    int Local() => 1;
                    Func<int, int> f = x => x;
                    Action a = delegate { };
                }
                public static C operator +(C a, C b) => a;
                public static implicit operator int(C c) => 0;
                int P { get { return 1; } set { } }
                int Q => 2;
                int R { get; init; }
                string this[int i] { get => ""; }
                event EventHandler E { add { } remove { } }
                int I.X { get { return 0; } set { } }
            }
        "#;
        assert_clean(src);
        assert_eq!(
            units(src),
            [
                "C:constructor",
                "~C:destructor",
                "M:method",
                "Local:function",
                "<lambda>:lambda",
                "<delegate>:delegate",
                "operator +:operator",
                "operator int:operator",
                "P:getter",
                "P:setter",
                "Q:getter",
                "this[]:getter",
                "E:add",
                "E:remove",
                "X:getter",
                "X:setter",
            ]
        );
    }

    #[test]
    fn bodyless_declarations_are_not_units() {
        let src = r#"
            interface I { void M(); int P { get; set; } }
            abstract class A { public abstract void M(); }
            partial class P { partial void OnChanged(); }
            static class N { [DllImport("x")] static extern int Ext(int a); }
            record R(int X);
        "#;
        assert_clean(src);
        assert_eq!(units(src), Vec::<String>::new());
    }

    #[test]
    fn nested_units_score_independently() {
        let src = r#"
            class C {
                void Outer(int[] xs) {
                    if (xs != null) {
                        xs.Where(x => x > 0 ? true : false);
                    }
                }
            }
        "#;
        let report = analyze(src);
        assert_eq!(function(&report, "Outer").cognitive, 1);
        // the lambda's ternary starts at nesting 0 in its own frame
        assert_eq!(function(&report, "<lambda>").cognitive, 1);
    }

    #[test]
    fn top_level_statements_score_at_module_level() {
        let src = r#"
            using System;
            if (args.Length > 0) { Console.WriteLine(args[0]); }
            static int Twice(int x) => x * 2;
        "#;
        // the module-level `if` is not attributed to any function
        assert_eq!(scores(src, "Twice"), (0, 1));
        assert_eq!(units(src), ["Twice:function"]);
    }

    #[test]
    fn csx_script_directives_are_blanked() {
        let src = "\u{feff}#!/usr/bin/env dotnet-script\n#r \"nuget: Newtonsoft.Json, 13.0.3\"\n// comment\n#load \"helpers.csx\"\n\nint F(int x) {\n    if (x > 0) { return 1; }\n    return 0;\n}\n";
        let script = Path::new("T.csx");
        let (_, errors) = to_ir(script, src);
        assert!(errors.is_empty(), "{errors:?}");
        let report = analyze_source(script, src);
        let f = function(&report, "F");
        assert_eq!((f.cognitive, f.line), (1, 6));
        // The same directives are not C#: a `.cs` file reports them.
        assert!(!parse_errors(src).is_empty());
    }

    #[test]
    fn hash_r_after_code_is_left_alone() {
        // Directives are only legal before the first token; later `#r` is kept
        // (and reported) rather than silently hidden.
        let src = "int x = 1;\n#r \"late.dll\"\n";
        assert!(!to_ir(Path::new("T.csx"), src).1.is_empty());
        assert_eq!(blank_script_directives("int x;\n").as_ref(), "int x;\n");
    }

    #[test]
    fn modern_syntax_parses_cleanly() {
        let src = r#"
            namespace N;
            public record Point(int X, int Y);
            public class Svc(ILogger log) {
                public int[] Xs { get; } = [1, 2, 3];
                public string Raw = """
                    raw { text }
                    """;
                public void M(Span<byte> s, Node? n) {
                    Unsafe.Add(ref s[0], 1) = 2;
                    n?.Value = 3;
                    if (Xs is [1, .., var last]) { log.Log($"{last}"); }
                }
            }
            public static class Ext {
                extension(string s) {
                    public bool IsEmpty => s.Length == 0;
                }
            }
        "#;
        assert_clean(src);
        assert_eq!(scores(src, "M"), (1, 3)); // if +1; base + if + `?.`
    }

    #[test]
    fn syntax_errors_are_reported_with_lines() {
        let src = "class C {\n void F() { if ( }\n}\n";
        let errors = parse_errors(src);
        assert!(!errors.is_empty());
        assert!(
            errors
                .iter()
                .all(|e| e.starts_with("syntax error at line "))
        );
    }
}
