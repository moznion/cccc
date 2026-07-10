//! Perl adapter: parses source with the community-maintained [tree-sitter]
//! grammar for Perl (the `tree-sitter-perl` org, published as `ts-parser-perl`)
//! and lowers the concrete syntax tree into the language-agnostic
//! [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the Perl grammar (whose C source is compiled by `cc`, so like `cccc-py`
//! there is no `libclang`/bindgen requirement), with no CLI machinery. The
//! unified `cccc` binary (the `cccc-cli` crate) registers this adapter's
//! [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.pl`/`.pm`/`.t` files to
//! it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about and emits the matching IR nodes. All complexity rules
//! live in [`cccc_core::engine`].
//!
//! Perl is famously not fully statically parseable ("only perl can parse
//! Perl" — prototypes, source filters, and `BEGIN` blocks can change how later
//! code parses), but the grammar is fault-tolerant and covers everything the
//! complexity metrics care about; the pathological cases surface as syntax
//! errors on the report rather than silent misparses.
//!
//! Like `cccc-py`, lowering is driven by a `kind()`-dispatch whose **default
//! arm recurses into every named child** (tree-sitter has no "walk every child"
//! visitor trait), so an unrecognized construct is transparent and an anonymous
//! sub or operator in an unexpected position (a hash value, a signature default)
//! is never silently missed. The IR tree is assembled with a stack of
//! "collectors" ([`Builder::collect`]).
//!
//! ## Perl-to-IR mapping notes
//!
//! - `sub name { }` → [`Node::Function`] (`"function"` — Perl cannot statically
//!   distinguish a package sub from a method); a `method` declaration inside
//!   `class` (feature `class`, Perl 5.38+) → `"method"`; an anonymous
//!   `sub { }` → `"sub"` (named `<sub>`). A block callback passed to
//!   `grep`/`map`/`sort` → `"block"` (named `<block>`), mirroring how `cccc-rb`
//!   models Ruby blocks: a unit of its own, not a `Loop`. Each is its own
//!   unit — nesting resets at the boundary.
//! - `if` / `elsif` / `else` and `unless` → [`Node::Branch`] (chaining `elsif`
//!   as a nested `Branch` so it scores flat). The statement modifiers
//!   `EXPR if COND` / `EXPR unless COND` are a `Branch` with no `else`. The
//!   ternary `a ? b : c` → [`Node::Conditional`].
//! - `while` / `until` / C-style `for` / `foreach` (and the `EXPR while COND` /
//!   `EXPR for LIST` modifiers, incl. `do { } while`) → [`Node::Loop`].
//! - `try` / `catch` (feature `try`, Perl 5.34+) → the `catch` block is a
//!   [`Node::Catch`]; the `try` and `finally` bodies score at the surrounding
//!   level. A classic `eval { }` is transparent (it is the *try*, not the
//!   handler — the `if ($@)` after it already scores as a branch).
//! - `&&` / `and`, `||` / `or`, `//` → folded [`Node::Logical`] (one node per
//!   like-operator run; high- and low-precedence spellings of the same operator
//!   fold together). `xor`, `not`, and `!` add nothing, and the assignment
//!   forms (`||=`, `&&=`, `//=`) are transparent, matching the other adapters.
//! - labelled `next` / `last` / `redo` → [`Node::Jump`] `{ labeled: true }`
//!   (unlabelled ones score nothing).
//! - calls → [`Node::Call`] for recursion detection: `f()`, `&f()`, and
//!   `Pkg::f()` yield `Some("f")` (trailing `::` segment), `$obj->f()` yields
//!   `Some("f")`. A dynamic callee (`$code->()`, `$obj->$m()`) and a
//!   `SUPER::`-qualified method (calling the *parent's* implementation — the
//!   standard override pattern, not recursion) yield `None`.
//!
//! `given`/`when` is not lowered (the grammar does not parse it; the construct
//! was deprecated for removal), so [`Node::Switch`] is never emitted. POD,
//! comments, `__END__`/`__DATA__` sections, phasers (`BEGIN` etc.), string
//! interpolation, regexes, and heredocs are transparent.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given): scripts,
/// modules, and test files (which are plain Perl).
pub const DEFAULT_EXTS: &[&str] = &["pl", "pm", "t"];

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
        .set_language(&ts_parser_perl::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load Perl grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse Perl source".to_string()]);
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
    /// comments and POD). This is the "transparent" step shared by every arm
    /// that carries no score of its own: a fresh cursor walk with no
    /// intermediate `Vec` allocation.
    fn visit_named_children(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !child.is_extra() {
                self.visit(child);
            }
        }
    }

    /// A function-like unit: emit a `Function` whose body walks *all* named
    /// children (so an anonymous sub hiding in a signature default is still
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
            "subroutine_declaration_statement" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n).to_string())
                    .unwrap_or_else(|| "<sub>".into());
                self.emit_function_node(name, "function", node);
            }
            "method_declaration_statement" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n).to_string())
                    .unwrap_or_else(|| "<method>".into());
                self.emit_function_node(name, "method", node);
            }
            "anonymous_subroutine_expression" => {
                self.emit_function_node("<sub>".into(), "sub", node);
            }

            "conditional_statement" => {
                let branch = self.lower_conditional(node);
                self.emit(branch);
            }
            "postfix_conditional_expression" => self.visit_postfix_conditional(node),
            "conditional_expression" => self.visit_ternary(node),

            "loop_statement"
            | "cstyle_for_statement"
            | "for_statement"
            | "postfix_loop_expression"
            | "postfix_for_expression" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }

            "try_statement" => self.visit_try(node),

            "map_grep_expression" | "sort_expression" => self.visit_block_callback(node),

            "binary_expression" | "lowprec_logical_expression" => {
                if let Some(op) = logical_op_of(node) {
                    self.visit_logical(node, op);
                } else {
                    self.visit_named_children(node);
                }
            }

            "loopex_expression" => {
                // `next` / `last` / `redo`: only the labelled form is a
                // cognitive point.
                let labeled = named_children(node).iter().any(|c| c.kind() == "label");
                self.emit(Node::Jump { labeled });
            }

            "function_call_expression" | "ambiguous_function_call_expression" => {
                let callee = node
                    .child_by_field_name("function")
                    .and_then(|f| self.function_name(f));
                self.emit(Node::Call { callee });
                self.visit_named_children(node);
            }
            "method_call_expression" => {
                let callee = node
                    .child_by_field_name("method")
                    .and_then(|m| self.method_name(m));
                self.emit(Node::Call { callee });
                self.visit_named_children(node);
            }

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// Build a `Branch` from a `conditional_statement` (`if` / `unless`) or an
    /// `elsif` node (same field shape). The grammar already nests the chain:
    /// the `elsif`/`else` tail is a single named child of the previous link, so
    /// each `elsif` lowers to a nested `Branch` (scoring flat) and a plain
    /// `else` to a `Group`.
    fn lower_conditional(&mut self, node: TsNode) -> Node {
        let test = self.collect(|b| b.visit_field(node, "condition"));
        let then = self.collect(|b| b.visit_field(node, "block"));
        let alternate = named_children(node)
            .into_iter()
            .find(|c| matches!(c.kind(), "elsif" | "else"))
            .and_then(|c| self.lower_alternative(c));
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    fn lower_alternative(&mut self, node: TsNode) -> Option<Box<Node>> {
        match node.kind() {
            "elsif" => Some(Box::new(self.lower_conditional(node))),
            "else" => Some(Box::new(Node::Group(
                self.collect(|b| b.visit_field(node, "block")),
            ))),
            _ => None,
        }
    }

    /// Visit every *named* child of `node` under the field `field`. The grammar
    /// tags a parenthesized condition's `(`/`)` tokens with the same field as
    /// the expression, so a plain `child_by_field_name` could land on a token.
    fn visit_field(&mut self, node: TsNode, field: &str) {
        let mut cursor = node.walk();
        let children: Vec<TsNode> = node
            .children_by_field_name(field, &mut cursor)
            .filter(|c| c.is_named() && !c.is_extra())
            .collect();
        for child in children {
            self.visit(child);
        }
    }

    /// The statement modifiers `EXPR if COND` / `EXPR unless COND`: a `Branch`
    /// with no `else` whose `then` is the modified expression.
    fn visit_postfix_conditional(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        let condition_ids: Vec<usize> = node
            .children_by_field_name("condition", &mut cursor)
            .map(|c| c.id())
            .collect();
        let test = self.collect(|b| b.visit_field(node, "condition"));
        let then = self.collect(|b| {
            for child in named_children(node) {
                if !condition_ids.contains(&child.id()) {
                    b.visit(child);
                }
            }
        });
        self.emit(Node::Branch {
            test,
            then,
            alternate: None,
        });
    }

    /// The ternary `a ? b : c`: a single increment (its `:` arm is not a
    /// second one).
    fn visit_ternary(&mut self, node: TsNode) {
        let test = self.collect(|b| b.visit_field(node, "condition"));
        let then = self.collect(|b| b.visit_field(node, "consequent"));
        let alternate = self.collect(|b| b.visit_field(node, "alternative"));
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    /// `try`/`catch`/`finally` (feature `try`): the `try` and `finally` bodies
    /// run at the surrounding level; the `catch` block is a `Node::Catch`
    /// decision point (its error variable carries no complexity).
    fn visit_try(&mut self, node: TsNode) {
        self.visit_field(node, "try_block");
        let body = self.collect(|b| b.visit_field(node, "catch_block"));
        self.emit(Node::Catch { body });
        self.visit_field(node, "finally_block");
    }

    /// `grep` / `map` / `sort` with a block callback: the block is its own
    /// anonymous `"block"` unit (mirroring Ruby blocks in `cccc-rb`); the list
    /// arguments score at the surrounding level. The expression forms
    /// (`grep EXPR, LIST`) stay transparent.
    fn visit_block_callback(&mut self, node: TsNode) {
        let callback = node
            .child_by_field_name("callback")
            .filter(|c| c.kind() == "block");
        let Some(cb) = callback else {
            self.visit_named_children(node);
            return;
        };
        let line = cb.start_position().row as u32 + 1;
        let body = self.collect(|b| b.visit_named_children(cb));
        self.emit(Node::Function {
            name: "<block>".to_string(),
            kind: "block".to_string(),
            line,
            body,
        });
        for child in named_children(node) {
            if child.id() != cb.id() {
                self.visit(child);
            }
        }
    }

    /// One folded [`Node::Logical`] for a run of like operators — `&&` and
    /// `and` are the same run, as are `||`/`or`; `//` is `Coalesce`. A
    /// different operator nested inside starts a fresh `Logical`.
    fn visit_logical(&mut self, node: TsNode, op: LogicalOp) {
        let mut operands = Vec::new();
        for side in named_children(node) {
            self.collect_logical_side(side, op, &mut operands);
        }
        self.emit(Node::Logical { op, operands });
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    /// (Parentheses need no unwrapping: the grammar attaches `(`/`)` as tokens,
    /// so `$a && ($b && $c)` is already one nested `binary_expression`.)
    fn collect_logical_side(&mut self, side: TsNode, op: LogicalOp, operands: &mut Vec<Node>) {
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

    /// Simple name of a statically-called function (`f()`, `&f()`,
    /// `Pkg::f()` → `"f"`), used for recursion detection. A dynamic callee
    /// (`&$code()`, `$obj->()`) yields `None`.
    fn function_name(&self, node: TsNode) -> Option<String> {
        let name = self.text(node).trim().trim_start_matches('&');
        if name.is_empty() || name.contains(['$', '@', '{']) {
            return None;
        }
        Some(name.rsplit("::").next().unwrap_or(name).to_string())
    }

    /// Simple name of a method callee (`$obj->f()` → `"f"`). A dynamic method
    /// (`$obj->$m()`) yields `None`, and so does a package-qualified one
    /// (`$self->SUPER::f()` dispatches to the *parent's* `f` — the standard
    /// override pattern, which must not read as recursion).
    fn method_name(&self, node: TsNode) -> Option<String> {
        let name = self.text(node).trim();
        if name.is_empty() || name.contains(['$', '@', ':']) {
            return None;
        }
        Some(name.to_string())
    }
}

/// The named children of `node` (skipping `extras`, i.e. comments and POD),
/// collected into a `Vec` so the caller can filter or re-walk them without
/// holding the cursor's borrow. For the common "just recurse into all of them"
/// case use [`Builder::visit_named_children`], which allocates nothing.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}

/// The normalized logical operator a `binary_expression` /
/// `lowprec_logical_expression` node represents, read from its `operator`
/// token. High- and low-precedence spellings normalize to the same
/// [`LogicalOp`] so `$a && $b and $c` folds into one run; `xor` (and every
/// arithmetic/comparison operator) yields `None` and stays transparent.
fn logical_op_of(node: TsNode) -> Option<LogicalOp> {
    if !matches!(
        node.kind(),
        "binary_expression" | "lowprec_logical_expression"
    ) {
        return None;
    }
    match node.child_by_field_name("operator").map(|o| o.kind()) {
        Some("&&") | Some("and") => Some(LogicalOp::And),
        Some("||") | Some("or") => Some(LogicalOp::Or),
        Some("//") => Some(LogicalOp::Coalesce),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.pl"), src)
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
        // Perl has labelled `next`, so this is the SonarSource white-paper
        // original, labelled jump and all.
        let src = r#"
sub sum_of_primes {
    my ($max) = @_;
    my $total = 0;
    OUT: for my $i (2 .. $max) {
        for my $j (2 .. $i - 1) {
            if ($i % $j == 0) {
                next OUT;
            }
        }
        $total += $i;
    }
    return $total;
}
"#;
        // for(+1) + nested for(+2) + nested if(+3) + labelled next(+1) = 7
        assert_eq!(cognitive_of(src, "sum_of_primes"), 7);
        // base 1 + for + for + if = 4
        assert_eq!(cyclomatic_of(src, "sum_of_primes"), 4);
    }

    #[test]
    fn elsif_else_are_flat() {
        let src = r#"
sub f {
    my ($a, $b) = @_;
    if ($a) { one(); }
    elsif ($b) { two(); }
    else { three(); }
}
"#;
        // if(+1) + elsif(+1 flat) + else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if + elsif = 3 (else is not a decision point)
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn nested_construct_inside_elsif_gets_the_elsif_nesting() {
        let src = r#"
sub f {
    my ($a, $b, $c) = @_;
    if ($a) { one(); }
    elsif ($b) {
        if ($c) { two(); }
    }
}
"#;
        // if(+1) + elsif(+1 flat) + if nested in elsif(+2) = 4
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
sub f {
    my ($a, $b, $c) = @_;
    if ($a) {
        if ($b) {
            if ($c) { deep(); }
        }
    }
}
"#;
        // if(+1) + nested if(+2) + nested if(+3) = 6
        assert_eq!(cognitive_of(src, "f"), 6);
        // base 1 + three ifs = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn unless_is_a_branch() {
        let src = r#"
sub f {
    my ($a) = @_;
    unless ($a) { bail(); }
    else { go(); }
}
"#;
        // unless(+1) + else(+1 flat) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn ternary_is_a_conditional() {
        let src = r#"
sub f {
    my ($a) = @_;
    return $a ? 1 : 2;
}
"#;
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
sub f {
    my ($a, @items) = @_;
    while ($a) { w(); }
    until ($a) { u(); }
    for (my $i = 0; $i < 10; $i++) { c(); }
    foreach my $x (@items) { e($x); }
}
"#;
        // four loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 4);
        // base 1 + four loops = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn statement_modifiers_count() {
        let src = r#"
sub f {
    my ($a, $b, $c, @xs) = @_;
    go() if $a;
    stop() unless $b;
    spin() while $c;
    each_one($_) for @xs;
}
"#;
        // postfix if(+1) + unless(+1) + while(+1) + for(+1) = 4
        assert_eq!(cognitive_of(src, "f"), 4);
        // base 1 + 2 branches + 2 loops = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn do_while_is_a_loop() {
        let src = r#"
sub f {
    my ($a) = @_;
    do { step(); } while ($a);
}
"#;
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn nested_construct_inside_postfix_if_nests() {
        let src = r#"
sub f {
    my ($a, @xs) = @_;
    do_all(map { $_ ? 1 : 0 } @xs) if $a;
}
"#;
        // postfix if(+1); the map block is its own unit with the ternary(+1)
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cognitive_of(src, "<block>"), 1);
    }

    #[test]
    fn logical_sequences_fold_by_operator() {
        let src = r#"
sub f {
    my ($a, $b, $c, $d) = @_;
    if ($a && $b && $c || $d) { go(); }
}
"#;
        // if(+1) + && run(+1) + || run(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn parenthesized_like_operators_fold_into_one_run() {
        let src = r#"
sub f {
    my ($a, $b, $c) = @_;
    return $a && ($b && $c);
}
"#;
        // one folded && run = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + (&& 3 operands => +2) = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn high_and_low_precedence_spellings_fold_together() {
        let src = r#"
sub f {
    my ($a, $b, $c) = @_;
    if ($a && $b and $c) { go(); }
}
"#;
        // if(+1) + one folded And run (&& + and)(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + if + (And 3 operands => +2) = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn defined_or_counts_as_coalesce() {
        let src = r#"
sub f {
    my ($a, $b, $c) = @_;
    return $a // $b // $c;
}
"#;
        // one folded // run = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn xor_and_not_score_nothing() {
        let src = r#"
sub f {
    my ($a, $b) = @_;
    return not($a xor $b);
}
"#;
        assert_eq!(cognitive_of(src, "f"), 0);
        assert_eq!(cyclomatic_of(src, "f"), 1);
    }

    #[test]
    fn logical_assignment_operators_are_transparent() {
        let src = r#"
sub f {
    my ($a) = @_;
    $a //= 1;
    $a ||= 2;
    $a &&= 3;
    return $a;
}
"#;
        assert_eq!(cognitive_of(src, "f"), 0);
    }

    #[test]
    fn try_catch_counts_and_finally_is_transparent() {
        let src = r#"
use feature 'try';
sub f {
    try { risky(); }
    catch ($e) { handle($e); }
    finally { cleanup(); }
}
"#;
        // catch(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + catch = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn eval_block_is_transparent() {
        let src = r#"
sub f {
    eval { risky(); };
    if ($@) { handle(); }
}
"#;
        // only the if(+1) counts — eval is the try, not the handler
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn labelled_jump_counts_and_plain_does_not() {
        let src = r#"
sub f {
    my (@xs) = @_;
    OUTER: for my $x (@xs) {
        for my $y (@xs) {
            next OUTER;
        }
        last;
        next;
    }
}
"#;
        // for(+1) + nested for(+2) + labelled next(+1); plain last/next add 0
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
sub fib {
    my ($n) = @_;
    if ($n < 2) { return $n; }
    return fib($n - 1) + fib($n - 2);
}
"#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
        assert_eq!(
            find(&analyze(src).functions, "fib").unwrap().kind,
            "function"
        );
    }

    #[test]
    fn method_recursion_via_arrow_is_detected() {
        let src = r#"
package S;
sub walk {
    my ($self, $n) = @_;
    if ($n == 0) { return 0; }
    return $self->walk($n - 1);
}
"#;
        // if(+1) + recursion via $self->walk(+1) = 2
        assert_eq!(cognitive_of(src, "walk"), 2);
    }

    #[test]
    fn super_qualified_call_is_not_recursion() {
        let src = r#"
package S;
sub init {
    my ($self) = @_;
    if ($self->{ready}) { return; }
    return $self->SUPER::init();
}
"#;
        // if(+1) only — SUPER::init dispatches to the parent, not to us
        assert_eq!(cognitive_of(src, "init"), 1);
    }

    #[test]
    fn package_qualified_function_recursion_is_detected() {
        let src = r#"
sub walk {
    my ($n) = @_;
    if ($n == 0) { return 0; }
    return Tree::Walker::walk($n - 1);
}
"#;
        // if(+1) + recursion via the trailing `::` segment(+1) = 2
        assert_eq!(cognitive_of(src, "walk"), 2);
    }

    #[test]
    fn class_method_is_a_method_unit() {
        let src = r#"
use feature 'class';
class Counter {
    field $count = 0;
    method increment {
        if ($count > 10) { return $count; }
        $count++;
        return $count;
    }
}
"#;
        let report = analyze(src);
        let f = find(&report.functions, "increment").expect("increment");
        assert_eq!(f.kind, "method");
        // if(+1) = 1
        assert_eq!(f.cognitive, 1);
    }

    #[test]
    fn anonymous_sub_is_its_own_unit() {
        let src = r#"
sub host {
    my ($a) = @_;
    my $cb = sub { return $a && $a; };
    return $cb;
}
"#;
        assert_eq!(cognitive_of(src, "host"), 0);
        // && run(+1) = 1, nesting reset inside the anonymous sub
        assert_eq!(cognitive_of(src, "<sub>"), 1);
        assert_eq!(find(&analyze(src).functions, "<sub>").unwrap().kind, "sub");
    }

    #[test]
    fn anonymous_sub_in_a_hash_value_is_reached() {
        let src = r#"
sub host {
    my %handlers = (
        on_error => sub { retry() if $a; },
    );
    return \%handlers;
}
"#;
        assert_eq!(cognitive_of(src, "host"), 0);
        // postfix if(+1) inside the anonymous sub
        assert_eq!(cognitive_of(src, "<sub>"), 1);
    }

    #[test]
    fn nested_named_sub_is_its_own_unit() {
        let src = r#"
sub host {
    sub inner {
        my ($x) = @_;
        if ($x) { return 1; }
    }
    return 1;
}
"#;
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "inner"), 1);
    }

    #[test]
    fn grep_block_is_its_own_anonymous_unit() {
        let src = r#"
sub host {
    my (@xs) = @_;
    my @pos = grep { $_ > 0 && $_ < 10 } @xs;
    return @pos;
}
"#;
        // host owns no structural complexity; the block does.
        assert_eq!(cognitive_of(src, "host"), 0);
        // && run(+1) = 1
        assert_eq!(cognitive_of(src, "<block>"), 1);
        assert_eq!(
            find(&analyze(src).functions, "<block>").unwrap().kind,
            "block"
        );
    }

    #[test]
    fn grep_expression_form_scores_in_the_host() {
        let src = r#"
sub host {
    my (@xs) = @_;
    my @pos = grep $_ > 0 && $_ < 10, @xs;
    return @pos;
}
"#;
        // no block unit: the && run(+1) scores in host itself
        assert_eq!(cognitive_of(src, "host"), 1);
        assert!(find(&analyze(src).functions, "<block>").is_none());
    }

    #[test]
    fn sort_comparator_block_is_its_own_unit() {
        let src = r#"
sub host {
    my (@xs) = @_;
    return sort { $a <=> $b ? -1 : 1 } @xs;
}
"#;
        assert_eq!(cognitive_of(src, "host"), 0);
        // ternary(+1) inside the comparator block
        assert_eq!(cognitive_of(src, "<block>"), 1);
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
sub a { my ($x) = @_; if ($x) { one(); } }
sub b { my ($y) = @_; if ($y) { two(); } }
"#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn module_level_code_counts_toward_the_file() {
        let src = r#"
use strict;
if ($ENV{DEBUG}) {
    warn "debugging";
}
"#;
        let report = analyze(src);
        assert_eq!(report.cognitive, 1);
        assert!(report.functions.is_empty());
    }

    #[test]
    fn pod_and_comments_do_not_change_the_score() {
        let plain = "sub f { my ($a) = @_; if ($a) { if ($a) { go(); } } }\n";
        let commented = r#"
=pod

Documentation about f.

=cut

sub f {
    my ($a) = @_;
    # outer check
    if ($a) { # inner
        if ($a) { go(); }
    }
}
"#;
        // if(+1) + nested if(+2) = 3, with or without POD/comments
        assert_eq!(cognitive_of(plain, "f"), 3);
        assert_eq!(cognitive_of(commented, "f"), cognitive_of(plain, "f"));
    }

    #[test]
    fn parse_error_is_reported() {
        // tree-sitter is fault-tolerant: it still yields a (partial) tree but
        // surfaces the error location for the broken input.
        let (_nodes, errors) = to_ir(Path::new("bad.pl"), "sub f {\n    if (\n");
        assert!(!errors.is_empty());
    }
}
