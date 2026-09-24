//! Shared lowering code for the C-family adapters (`cccc-c` and `cccc-cpp`).
//!
//! C and C++ share a lot of syntax. The `tree-sitter-cpp` grammar is a
//! superset of `tree-sitter-c`'s, so constructs like `if`, loops, `switch`,
//! `break`/`continue`/`goto`, `&&`/`||`, preprocessor conditionals, and
//! function calls parse to the same node kinds in both languages. This crate
//! lowers all of that once, instead of duplicating it in both adapter crates.
//!
//! The main type is [`SharedBuilder`]. Each adapter creates one and tells it
//! which language it's lowering (see [`Language`]). Most of
//! [`SharedBuilder::visit`] handles the constructs C and C++ share. A few
//! match arms handle constructs that only exist in C++ (lambdas, `catch`,
//! range-`for`, and extra ways of naming a function such as destructors,
//! operators, and qualified names like `Foo::bar`) — those arms just check
//! the language field before running.
//!
//! This crate has no scoring logic; that lives in `cccc_core::engine`. It
//! depends only on `cccc-core` and `tree-sitter`, not on a specific grammar
//! crate. Each adapter crate brings its own grammar dependency
//! (`tree-sitter-c` or `tree-sitter-cpp`) and does its own parsing.

use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use tree_sitter::Node as TsNode;

/// Collect the 1-based lines of every `ERROR`/`MISSING` node so a partially
/// parsed file surfaces its syntax problems (deduplicated, order preserved).
pub fn collect_errors(node: TsNode, out: &mut Vec<String>) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cpp,
}

#[derive(Debug)]
pub struct SharedBuilder<'a> {
    src: &'a [u8],
    stack: Vec<Vec<Node>>,
    lang: Language,
}

impl SharedBuilder<'_> {
    pub fn new(src: &[u8], lang: Language) -> SharedBuilder<'_> {
        SharedBuilder {
            src,
            stack: vec![Vec::new()],
            lang,
        }
    }

    pub fn lang(&self) -> &Language {
        &self.lang
    }

    /// The module-level node list (the single remaining collector).
    pub fn finish(mut self) -> Vec<Node> {
        self.stack.pop().expect("module collector")
    }

    /// Run `f` against a fresh collector and return the nodes it gathered.
    pub fn collect<F: FnOnce(&mut Self)>(&mut self, f: F) -> Vec<Node> {
        self.stack.push(Vec::new());
        f(self);
        self.stack.pop().expect("collector")
    }

    /// Append a node to the current collector.
    pub fn emit(&mut self, node: Node) {
        self.stack.last_mut().expect("collector").push(node);
    }

    /// The UTF-8 text of `node`, or `""` if it is not valid UTF-8.
    pub fn text(&self, node: TsNode) -> &str {
        node.utf8_text(self.src).unwrap_or("")
    }

    /// Recurse into every named child of `node` (skipping `extras`, i.e.
    /// comments — see [`named_children`]). This is the "transparent" step shared
    /// by every arm that carries no score of its own: a fresh cursor walk with
    /// no intermediate `Vec` allocation.
    pub fn visit_named_children(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !child.is_extra() {
                self.visit(child);
            }
        }
    }

    // ---- traversal --------------------------------------------------------

    pub fn visit(&mut self, node: TsNode) {
        match node.kind() {
            "function_definition" => {
                let name = node
                    .child_by_field_name("declarator")
                    .and_then(|d| declarator_name(self, d))
                    .unwrap_or_else(|| "<function>".into());
                self.emit_function_node(name, "function", node);
            }
            // C++ only: a `[...](...){...}` closure is its own scored unit,
            // same as a named function.
            "lambda_expression" if self.lang == Language::Cpp => {
                self.emit_function_node("<lambda>".into(), "lambda", node)
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
            "for_statement" | "while_statement" | "do_statement" => self.emit_loop(node),
            // C++ only: `for (auto &x : xs)`.
            "for_range_loop" if self.lang == Language::Cpp => self.emit_loop(node),
            "switch_statement" => self.visit_switch(node),

            "break_statement" | "continue_statement" => self.emit(Node::Jump { labeled: false }),
            "goto_statement" => self.emit(Node::Jump { labeled: true }),

            "binary_expression" => match logical_op_of(node) {
                Some(op) => self.visit_logical(node, op),
                None => self.visit_named_children(node),
            },

            "call_expression" => self.visit_call(node),

            // C++ only: the `try` body runs at the surrounding level (it's
            // visited transparently by the fallback arm below); each `catch`
            // clause is its own `Node::Catch` decision point.
            "catch_clause" if self.lang == Language::Cpp => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Catch { body });
            }

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

    /// Any loop (`for` / `while` / `do`-`while`, or C++'s range-`for`): +1 at
    /// its own nesting level, body walked transparently.
    fn emit_loop(&mut self, node: TsNode) {
        let body = self.collect(|b| b.visit_named_children(node));
        self.emit(Node::Loop { body });
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
                    // A label or statement outside any case (legal in both
                    // C and C++) runs at the switch's level.
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
    /// (`(*fp)(..)`) — plus, in C++, a qualified call (`Foo::bar(..)`).
    /// Returns the trailing identifier.
    fn callee_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node).to_string()),
            "field_expression" => node
                .child_by_field_name("field")
                .map(|f| self.text(f).to_string()),
            "qualified_identifier" if self.lang == Language::Cpp => node
                .child_by_field_name("name")
                .and_then(|n| self.callee_name(n)),
            "parenthesized_expression" | "pointer_expression" => named_children(node)
                .into_iter()
                .find_map(|c| self.callee_name(c)),
            _ => None,
        }
    }
}

/// Find the name being defined in a declarator chain.
///
/// A `function_definition`'s `declarator` is rarely just a plain identifier.
/// In C, it can be wrapped in a `pointer_declarator` (a function returning a
/// pointer), a `function_declarator`, or a `parenthesized_declarator`
/// (`int *(*f(void))(int)` still names `f`). In C++, the name itself can also
/// be a `field_identifier` (a method defined inline in a class body), a
/// `destructor_name` (`~Foo`), an `operator_name` (`operator+`), or a
/// `qualified_identifier` (`Foo::bar`, an out-of-line definition) — those four
/// only happen in C++, so they're gated on `b.lang`.
///
/// For a `qualified_identifier` we keep only the trailing name (`Foo::bar`
/// becomes `bar`), because that's what [`SharedBuilder::callee_name`] returns
/// for a qualified *call* too. Without that, an out-of-line method calling
/// itself wouldn't be recognized as recursion.
fn declarator_name(b: &SharedBuilder, node: TsNode) -> Option<String> {
    match node.kind() {
        "identifier" => Some(b.text(node).to_string()),
        "field_identifier" | "destructor_name" | "operator_name" if b.lang == Language::Cpp => {
            Some(b.text(node).to_string())
        }
        "qualified_identifier" if b.lang == Language::Cpp => node
            .child_by_field_name("name")
            .and_then(|n| declarator_name(b, n)),
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
