//! Dart adapter: parses source with
//! [nielsenko/tree-sitter-dart](https://github.com/nielsenko/tree-sitter-dart)
//! and lowers the concrete syntax tree into [`cccc_core::ir`].
//!
//! The adapter recognizes Dart function-like declarations and expressions,
//! branches, loops, switches, exception handlers, labelled jumps, logical
//! operators (including pattern operators, `??`, and `??=`), null-aware
//! operations, collection control-flow elements, and calls. Scoring remains
//! entirely in `cccc-core`.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["dart"];

/// Parse `source` and produce its scored [`FileReport`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the shared complexity IR.
///
/// tree-sitter recovers from syntax errors, so valid parts are still lowered
/// while `ERROR` and `MISSING` locations are returned to the caller.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_dart::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load Dart grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse Dart source".to_string()]);
    };

    let mut errors = Vec::new();
    collect_errors(tree.root_node(), &mut errors);

    let mut builder = Builder::new(source.as_bytes());
    builder.visit(tree.root_node());
    (builder.finish(), errors)
}

fn collect_errors(node: TsNode, out: &mut Vec<String>) {
    if node.is_error() || node.is_missing() {
        let message = format!("syntax error at line {}", node.start_position().row + 1);
        if !out.contains(&message) {
            out.push(message);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_errors(child, out);
    }
}

struct Builder<'a> {
    src: &'a [u8],
    stack: Vec<Vec<Node>>,
}

impl<'a> Builder<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self {
            src,
            stack: vec![Vec::new()],
        }
    }

    fn finish(mut self) -> Vec<Node> {
        self.stack.pop().expect("module collector")
    }

    fn emit(&mut self, node: Node) {
        self.stack.last_mut().expect("collector").push(node);
    }

    fn collect<F: FnOnce(&mut Self)>(&mut self, walk: F) -> Vec<Node> {
        self.stack.push(Vec::new());
        walk(self);
        self.stack.pop().expect("collector")
    }

    fn text(&self, node: TsNode) -> &str {
        node.utf8_text(self.src).unwrap_or("")
    }

    fn visit_named_children(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !child.is_extra() {
                self.visit(child);
            }
        }
    }

    fn visit(&mut self, node: TsNode) {
        match node.kind() {
            "function_declaration" => self.visit_function(node, "function"),
            "local_function_declaration" => self.visit_function(node, "function"),
            "getter_declaration" => self.visit_function(node, "getter"),
            "setter_declaration" => self.visit_function(node, "setter"),
            "method_declaration" => self.visit_method(node),
            "function_expression" => self.visit_anonymous_function(node),

            "if_statement" | "if_element" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }
            "conditional_expression" => self.visit_conditional(node),
            "for_statement" | "for_element" | "while_statement" | "do_statement" => {
                let body = self.collect(|b| b.visit_named_children(node));
                self.emit(Node::Loop { body });
            }
            "switch_statement" | "switch_expression" => self.visit_switch(node),
            "try_statement" => self.visit_try(node),
            "break_statement" | "continue_statement" => self.visit_jump(node),

            "logical_and_expression" => self.visit_logical(node, LogicalOp::And),
            "logical_or_expression" => self.visit_logical(node, LogicalOp::Or),
            "if_null_expression" => self.visit_logical(node, LogicalOp::Coalesce),
            "assignment_expression" if self.is_coalesce_assignment(node) => {
                self.visit_coalesce_assignment(node)
            }

            "null_aware_member_expression"
            | "null_aware_index_expression"
            | "cascade_null_aware_member_expression"
            | "cascade_null_aware_index_expression" => self.visit_optional(node),
            "cascade_section" if self.text(node).trim_start().as_bytes().starts_with(b"?..") => {
                self.visit_optional(node)
            }
            "spread_element" if self.text(node).trim_start().starts_with("...?") => {
                self.visit_optional(node)
            }
            "null_aware_pair" | "null_aware_element" => self.visit_optional(node),

            "call_expression" | "cascade_call_expression" => self.visit_call(node),

            _ => self.visit_named_children(node),
        }
    }

    fn visit_function(&mut self, node: TsNode, kind: &'static str) {
        if is_native_body(node.child_by_field_name("body")) {
            return;
        }
        let signature = node.child_by_field_name("signature").unwrap_or(node);
        let name = descendant_field(signature, "name")
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "<function>".to_string());
        self.emit_function_node(node, name, kind);
    }

    fn visit_method(&mut self, node: TsNode) {
        if is_native_body(node.child_by_field_name("body")) {
            return;
        }
        let signature = node.child_by_field_name("signature").unwrap_or(node);
        let signature_kind = first_signature_kind(signature);
        let (kind, name) = match signature_kind {
            Some("constructor_signature" | "constant_constructor_signature") => {
                ("constructor", self.constructor_name(signature, false))
            }
            Some("factory_constructor_signature") => {
                ("factory", self.constructor_name(signature, true))
            }
            Some("operator_signature") => ("operator", self.field_text(signature, "operator")),
            Some("getter_signature") => ("getter", self.field_text(signature, "name")),
            Some("setter_signature") => ("setter", self.field_text(signature, "name")),
            _ => ("method", self.field_text(signature, "name")),
        };
        let fallback = format!("<{kind}>");
        let name = name.unwrap_or(fallback);
        self.emit_function_node(node, name, kind);
    }

    fn field_text(&self, node: TsNode, field: &str) -> Option<String> {
        descendant_field(node, field).map(|found| self.text(found).to_string())
    }

    fn constructor_name(&self, signature: TsNode, factory: bool) -> Option<String> {
        let signature = named_children(signature)
            .into_iter()
            .find(|child| {
                matches!(
                    child.kind(),
                    "constructor_signature"
                        | "constant_constructor_signature"
                        | "factory_constructor_signature"
                )
            })
            .unwrap_or(signature);
        let before_parameters = self.text(signature).split('(').next()?.trim();
        let name = before_parameters
            .strip_prefix("const ")
            .unwrap_or(before_parameters);
        let name = if factory {
            name.strip_prefix("factory ").unwrap_or(name)
        } else {
            name
        };
        (!name.is_empty()).then(|| name.to_string())
    }

    fn visit_anonymous_function(&mut self, node: TsNode) {
        self.emit_function_node(node, "<anonymous>".to_string(), "anonymous");
    }

    fn emit_function_node(&mut self, node: TsNode, name: String, kind: &'static str) {
        let line = node.start_position().row as u32 + 1;
        let body = self.collect(|b| b.visit_named_children(node));
        self.emit(Node::Function {
            name,
            kind: kind.to_string(),
            line,
            body,
        });
    }

    fn lower_if(&mut self, node: TsNode) -> Node {
        let consequence = node.child_by_field_name("consequence");
        let alternative = node.child_by_field_name("alternative");
        let test = self.collect(|b| {
            b.visit_if_case_pattern_logicals(node, consequence);
            b.visit_children_except(node, &[consequence, alternative]);
        });
        let then = consequence.map_or_else(Vec::new, |n| self.collect(|b| b.visit(n)));
        let alternate = alternative.map(|n| {
            if matches!(n.kind(), "if_statement" | "if_element") {
                Box::new(self.lower_if(n))
            } else {
                Box::new(Node::Group(self.collect(|b| b.visit(n))))
            }
        });
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    fn visit_conditional(&mut self, node: TsNode) {
        let consequence = node.child_by_field_name("consequence");
        let alternative = node.child_by_field_name("alternative");
        let test = self.collect(|b| b.visit_children_except(node, &[consequence, alternative]));
        let then = consequence.map_or_else(Vec::new, |n| self.collect(|b| b.visit(n)));
        let alternate = alternative.map_or_else(Vec::new, |n| self.collect(|b| b.visit(n)));
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    fn visit_children_except(&mut self, node: TsNode, excluded: &[Option<TsNode>]) {
        let excluded_ids: Vec<usize> = excluded.iter().flatten().map(TsNode::id).collect();
        for child in named_children(node) {
            if !excluded_ids.contains(&child.id()) {
                self.visit(child);
            }
        }
    }

    fn visit_switch(&mut self, node: TsNode) {
        if let Some(condition) = node.child_by_field_name("condition") {
            self.visit(condition);
        }
        let mut cases = Vec::new();
        let switch_cases = if node.kind() == "switch_expression" {
            named_children(node)
                .into_iter()
                .filter(|child| child.kind() == "switch_expression_case")
                .collect()
        } else {
            node.child_by_field_name("body")
                .map(named_children)
                .unwrap_or_default()
        };
        for case in switch_cases {
            let is_default = case.kind() == "switch_statement_default"
                || (case.kind() == "switch_expression_case"
                    && self
                        .text(case)
                        .split("=>")
                        .next()
                        .is_some_and(|pattern| pattern.trim() == "_"));
            let case_body = self.collect(|b| {
                b.visit_switch_case_pattern_logicals(case);
                b.visit_named_children(case);
            });
            cases.push(SwitchCase {
                is_default,
                body: case_body,
            });
        }
        self.emit(Node::Switch { cases });
    }

    fn visit_try(&mut self, node: TsNode) {
        let try_body_id = node.child_by_field_name("body").map(|n| n.id());
        let mut handler_pending = false;
        for child in named_children(node) {
            match child.kind() {
                "catch_clause" | "type" => handler_pending = true,
                "block" if Some(child.id()) == try_body_id => self.visit(child),
                "block" if handler_pending => {
                    let body = self.collect(|b| b.visit(child));
                    self.emit(Node::Catch { body });
                    handler_pending = false;
                }
                "finally_clause" => self.visit_named_children(child),
                _ => self.visit(child),
            }
        }
    }

    fn visit_jump(&mut self, node: TsNode) {
        self.emit(Node::Jump {
            labeled: named_children(node)
                .iter()
                .any(|child| child.kind() == "identifier"),
        });
    }

    fn visit_logical(&mut self, node: TsNode, op: LogicalOp) {
        let mut operands = Vec::new();
        for child in named_children(node) {
            self.collect_logical_side(child, op, &mut operands);
        }
        self.emit(Node::Logical { op, operands });
    }

    fn collect_logical_side(&mut self, side: TsNode, op: LogicalOp, operands: &mut Vec<Node>) {
        let side = unwrap_parens(side);
        match logical_op_of(side) {
            Some(side_op) if side_op == op => {
                for child in named_children(side) {
                    self.collect_logical_side(child, op, operands);
                }
            }
            Some(side_op) => {
                let mut nested = Vec::new();
                for child in named_children(side) {
                    self.collect_logical_side(child, side_op, &mut nested);
                }
                operands.push(Node::Logical {
                    op: side_op,
                    operands: nested,
                });
            }
            None => operands.push(Node::Group(self.collect(|b| b.visit(side)))),
        }
    }

    fn is_coalesce_assignment(&self, node: TsNode) -> bool {
        node.child_by_field_name("operator")
            .is_some_and(|operator| self.text(operator) == "??=")
    }

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

    fn visit_optional(&mut self, node: TsNode) {
        let body = self.collect(|b| b.visit_named_children(node));
        self.emit(Node::NullGuard { body });
    }

    /// Dart's logical pattern rules are hidden (`_logical_*_pattern`), so their
    /// `&&`/`||` tokens have no named CST wrapper. Recover the pattern-only
    /// source range after `case`; ordinary logical expressions remain handled
    /// by [`Self::visit_logical`].
    fn visit_if_case_pattern_logicals(&mut self, node: TsNode, consequence: Option<TsNode>) {
        let Some(case) = direct_child_with_kind(node, "case") else {
            return;
        };
        let end = direct_child_with_kind(node, "when")
            .map(|child| child.start_byte())
            .or_else(|| consequence.map(|child| child.start_byte()))
            .unwrap_or_else(|| node.end_byte());
        self.emit_pattern_logicals(node, case.end_byte(), end);
    }

    fn visit_switch_case_pattern_logicals(&mut self, node: TsNode) {
        let start = direct_child_with_kind(node, "case")
            .map(|child| child.end_byte())
            .unwrap_or_else(|| node.start_byte());
        let delimiter =
            direct_child_with_kind(node, "=>").or_else(|| direct_child_with_kind(node, ":"));
        let end = direct_child_with_kind(node, "when")
            .map(|child| child.start_byte())
            .or_else(|| delimiter.map(|child| child.start_byte()))
            .unwrap_or_else(|| node.end_byte());
        self.emit_pattern_logicals(node, start, end);
    }

    fn emit_pattern_logicals(&mut self, node: TsNode, start: usize, end: usize) {
        let mut tokens = Vec::new();
        collect_pattern_tokens(node, start, end, &mut tokens);
        for logical in parse_pattern_logicals(&tokens) {
            self.emit(logical.into_ir());
        }
    }

    fn visit_call(&mut self, node: TsNode) {
        let callee = node
            .child_by_field_name("function")
            .or_else(|| node.child_by_field_name("property"))
            .and_then(|n| self.callee_name(n));
        self.emit(Node::Call { callee });
        self.visit_named_children(node);
    }

    fn callee_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node).to_string()),
            "member_expression"
            | "null_aware_member_expression"
            | "cascade_member_expression"
            | "cascade_null_aware_member_expression" => node
                .child_by_field_name("property")
                .and_then(|property| self.callee_name(property)),
            "instantiation_expression" => node
                .child_by_field_name("function")
                .and_then(|function| self.callee_name(function)),
            "parenthesized_expression" => match named_children(node).as_slice() {
                [inner] => self.callee_name(*inner),
                _ => None,
            },
            _ => None,
        }
    }
}

fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect()
}

fn direct_child_with_kind<'tree>(node: TsNode<'tree>, kind: &str) -> Option<TsNode<'tree>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternToken {
    Open,
    Close,
    Separator,
    Logical(LogicalOp),
}

#[derive(Debug)]
struct PatternLogical {
    op: LogicalOp,
    operators: usize,
    children: Vec<PatternLogical>,
}

impl PatternLogical {
    fn into_ir(self) -> Node {
        let children = self
            .children
            .into_iter()
            .map(PatternLogical::into_ir)
            .collect();
        let mut operands = Vec::with_capacity(self.operators + 1);
        operands.push(Node::Group(children));
        operands.resize_with(self.operators + 1, || Node::Group(Vec::new()));
        Node::Logical {
            op: self.op,
            operands,
        }
    }
}

/// Collect just the punctuation needed to reconstruct hidden logical-pattern
/// precedence. Named logical expressions are skipped because the normal CST
/// traversal lowers those separately.
fn collect_pattern_tokens(node: TsNode, start: usize, end: usize, out: &mut Vec<PatternToken>) {
    if node.end_byte() <= start || node.start_byte() >= end || node.is_extra() {
        return;
    }
    if matches!(
        node.kind(),
        "logical_and_expression" | "logical_or_expression"
    ) {
        return;
    }
    if node.child_count() == 0 {
        let token = match node.kind() {
            "(" => Some(PatternToken::Open),
            ")" => Some(PatternToken::Close),
            "," => Some(PatternToken::Separator),
            "&&" => Some(PatternToken::Logical(LogicalOp::And)),
            "||" => Some(PatternToken::Logical(LogicalOp::Or)),
            _ => None,
        };
        if let Some(token) = token {
            out.push(token);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_pattern_tokens(child, start, end, out);
    }
}

/// Rebuild folded logical-pattern sequences. `||` has lower precedence than
/// `&&`; parentheses recurse, commas split independent nested patterns, and a
/// parenthesized child using the same operator is folded into its parent.
fn parse_pattern_logicals(tokens: &[PatternToken]) -> Vec<PatternLogical> {
    let segments = split_top_level(tokens, PatternToken::Separator);
    if segments.len() > 1 {
        return segments
            .into_iter()
            .flat_map(parse_pattern_logicals)
            .collect();
    }

    for op in [LogicalOp::Or, LogicalOp::And] {
        let separator = PatternToken::Logical(op);
        let parts = split_top_level(tokens, separator);
        if parts.len() > 1 {
            let mut logical = PatternLogical {
                op,
                operators: parts.len() - 1,
                children: Vec::new(),
            };
            for child in parts.into_iter().flat_map(parse_pattern_logicals) {
                if child.op == op {
                    logical.operators += child.operators;
                    logical.children.extend(child.children);
                } else {
                    logical.children.push(child);
                }
            }
            return vec![logical];
        }
    }

    let mut nested = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index] != PatternToken::Open {
            index += 1;
            continue;
        }
        let Some(close) = matching_close(tokens, index) else {
            break;
        };
        nested.extend(parse_pattern_logicals(&tokens[index + 1..close]));
        index = close + 1;
    }
    nested
}

fn split_top_level(tokens: &[PatternToken], separator: PatternToken) -> Vec<&[PatternToken]> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut parts = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            PatternToken::Open => depth += 1,
            PatternToken::Close => depth = depth.saturating_sub(1),
            _ if depth == 0 && *token == separator => {
                parts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&tokens[start..]);
    parts
}

fn matching_close(tokens: &[PatternToken], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token {
            PatternToken::Open => depth += 1,
            PatternToken::Close => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn descendant_field<'tree>(node: TsNode<'tree>, field: &str) -> Option<TsNode<'tree>> {
    if let Some(found) = node.child_by_field_name(field) {
        return Some(found);
    }
    named_children(node)
        .into_iter()
        .find_map(|child| descendant_field(child, field))
}

fn first_signature_kind(node: TsNode<'_>) -> Option<&str> {
    const KINDS: &[&str] = &[
        "constructor_signature",
        "constant_constructor_signature",
        "factory_constructor_signature",
        "operator_signature",
        "getter_signature",
        "setter_signature",
        "function_signature",
    ];
    named_children(node).into_iter().find_map(|child| {
        if KINDS.contains(&child.kind()) {
            Some(child.kind())
        } else {
            first_signature_kind(child)
        }
    })
}

fn is_native_body(body: Option<TsNode>) -> bool {
    body.is_some_and(|node| contains_kind(node, "native"))
}

fn contains_kind(node: TsNode, kind: &str) -> bool {
    node.kind() == kind
        || named_children(node)
            .into_iter()
            .any(|child| contains_kind(child, kind))
}

fn logical_op_of(node: TsNode) -> Option<LogicalOp> {
    match node.kind() {
        "logical_and_expression" => Some(LogicalOp::And),
        "logical_or_expression" => Some(LogicalOp::Or),
        "if_null_expression" => Some(LogicalOp::Coalesce),
        _ => None,
    }
}

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
        analyze_source(Path::new("test.dart"), src)
    }

    fn find<'a>(functions: &'a [FunctionReport], name: &str) -> Option<&'a FunctionReport> {
        functions.iter().find_map(|function| {
            (function.name == name)
                .then_some(function)
                .or_else(|| find(&function.children, name))
        })
    }

    fn function(src: &str, name: &str) -> FunctionReport {
        let report = analyze(src);
        assert!(
            report.parse_errors.is_empty(),
            "unexpected parse errors: {:?}",
            report.parse_errors
        );
        find(&report.functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .clone()
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        let src = r#"
            int sumOfPrimes(int max) {
              var total = 0;
              outer:
              for (var i = 2; i <= max; ++i) {
                for (var j = 2; j < i; ++j) {
                  if (i % j == 0) {
                    continue outer;
                  }
                }
                total += i;
              }
              return total;
            }
        "#;
        let report = function(src, "sumOfPrimes");
        assert_eq!(report.cognitive, 7);
        assert_eq!(report.cyclomatic, 4);
    }

    #[test]
    fn else_if_else_are_flat() {
        let report = function(
            "void f(bool a, bool b) { if (a) {} else if (b) {} else {} }",
            "f",
        );
        assert_eq!(report.cognitive, 3);
        assert_eq!(report.cyclomatic, 3);
    }

    #[test]
    fn logical_and_coalesce_sequences_fold() {
        let report = function(
            "void f(bool a, bool b, bool c, String? x, String y) { if (a && b && c) {} var z = x ?? y ?? 'z'; }",
            "f",
        );
        assert_eq!(report.cognitive, 3);
        assert_eq!(report.cyclomatic, 6);
    }

    #[test]
    fn conditional_loops_switch_and_catches_count() {
        let src = r#"
            int f(int x) {
              while (x > 0) { x--; }
              do { x++; } while (x < 0);
              for (var i = 0; i < x; i++) {}
              var y = x > 0 ? 1 : 2;
              switch (x) { case 1: break; default: break; }
              try {} on FormatException {} catch (e) {} finally {}
              return y;
            }
        "#;
        let report = function(src, "f");
        assert_eq!(report.cognitive, 7);
        assert_eq!(report.cyclomatic, 8);
    }

    #[test]
    fn function_kinds_and_anonymous_functions_are_reported() {
        let src = r#"
            int get top => 1;
            set top(int value) {}
            class C {
              C() {}
              factory C.named() => C();
              int get value => 1;
              set value(int value) {}
              C operator +(C other) => this;
              void method() { var callback = () { if (true) {} }; }
            }
        "#;
        let report = analyze(src);
        for (name, kind) in [
            ("top", "getter"),
            ("C", "constructor"),
            ("C.named", "factory"),
            ("value", "getter"),
            ("+", "operator"),
            ("method", "method"),
            ("<anonymous>", "anonymous"),
        ] {
            let found = find(&report.functions, name)
                .unwrap_or_else(|| panic!("{name} not found in {:#?}", report.functions));
            assert_eq!(found.kind, kind, "{name}");
        }
    }

    #[test]
    fn anonymous_functions_are_independent_units() {
        let report = analyze(
            r#"
                void host() {
                  final callback = (bool a, bool b) {
                    if (a && b) {
                      return;
                    }
                  };
                  callback(true, false);
                }
            "#,
        );
        assert!(report.parse_errors.is_empty(), "{:?}", report.parse_errors);

        let host = find(&report.functions, "host").expect("host function");
        assert_eq!(host.cognitive, 0);
        assert_eq!(host.cyclomatic, 1);
        assert_eq!(host.children.len(), 1);

        let callback = &host.children[0];
        assert_eq!(callback.name, "<anonymous>");
        assert_eq!(callback.kind, "anonymous");
        assert_eq!(callback.cognitive, 2); // if + &&
        assert_eq!(callback.cyclomatic, 3); // base + if + &&

        // File totals include both the enclosing function and its closure.
        assert_eq!(report.cognitive, 2);
        assert_eq!(report.cyclomatic, 4);
    }

    #[test]
    fn recursion_and_coalesce_assignment_count() {
        let src = "int fib(int n) { int? cached; cached ??= fib(n - 1); if (n < 2) return n; return fib(n - 2); }";
        let report = function(src, "fib");
        assert_eq!(report.cognitive, 4);
        assert_eq!(report.cyclomatic, 3);
    }

    #[test]
    fn higher_order_calls_do_not_double_count_recursion() {
        let report = function("void Function() f() { f()(); return () {}; }", "f");
        assert_eq!(report.cognitive, 1); // only the inner `f()` is recursive
        assert_eq!(report.cyclomatic, 1);
    }

    #[test]
    fn bodyless_declarations_are_skipped() {
        let report = analyze("external void f(); class C { external void m(); }");
        assert!(report.functions.is_empty());
    }

    #[test]
    fn abstract_declarations_are_skipped() {
        let report = analyze(
            r#"
                abstract class Repository {
                  Future<void> save();
                  String get name;
                  set name(String value);
                }
            "#,
        );
        assert!(report.parse_errors.is_empty(), "{:?}", report.parse_errors);
        assert!(report.functions.is_empty());
    }

    #[test]
    fn parse_errors_are_reported() {
        assert!(!to_ir(Path::new("test.dart"), "void f( {").1.is_empty());
    }

    #[test]
    fn switch_expression_cases_are_counted() {
        let report = function(
            "String f(int value) => switch (value) { 1 => 'one', 2 => 'two', _ => 'other' };",
            "f",
        );
        assert_eq!(report.cognitive, 1);
        assert_eq!(report.cyclomatic, 3);
    }

    #[test]
    fn generic_calls_precedence_and_labels_parse_cleanly() {
        let src = r#"
            bool f<T>(T value, List<T> values) {
              outer: while (true) {
                if (values.where<T>((item) => item == value).isNotEmpty &&
                    1 + 2 == 3) {
                  break outer;
                }
              }
              return false;
            }
        "#;
        let report = analyze(src);
        assert!(report.parse_errors.is_empty(), "{:?}", report.parse_errors);
        let function = find(&report.functions, "f").expect("f");
        assert_eq!(function.cognitive, 5);
    }

    #[test]
    fn null_aware_accesses_add_only_cyclomatic_paths() {
        let src = r#"
            void f(Object? value, List<int>? values) {
              value?.toString();
              values?[0];
              value?..toString()..hashCode;
            }
        "#;
        let report = function(src, "f");
        assert_eq!(report.cognitive, 0);
        assert_eq!(report.cyclomatic, 4); // base + ?., ?[], ?..
    }

    #[test]
    fn null_aware_spread_adds_only_a_cyclomatic_path() {
        let report = function("List<int> f(List<int>? values) => [...?values];", "f");
        assert_eq!(report.cognitive, 0);
        assert_eq!(report.cyclomatic, 2);
    }

    #[test]
    fn null_aware_collection_elements_add_only_cyclomatic_paths() {
        let src = r#"
            List<int> list(int? value) => [?value];
            Set<String> set(String? value) => {?value};
        "#;
        for name in ["list", "set"] {
            let report = function(src, name);
            assert_eq!(report.cognitive, 0, "{name}");
            assert_eq!(report.cyclomatic, 2, "{name}"); // base + `?value`
        }
    }

    #[test]
    fn null_aware_map_entries_count_keys_and_values() {
        let src = r#"
            Map<String, int> f(String? key, int? value) => {
              ?key: 1,
              'value': ?value,
              ?key: ?value,
            };
        "#;
        let report = function(src, "f");
        assert_eq!(report.cognitive, 0);
        assert_eq!(report.cyclomatic, 5); // base + key + value + key/value
    }

    #[test]
    fn pattern_logical_sequences_fold_by_operator() {
        let src = r#"
            bool f(Object value) {
              if (value case (int a && > 0) || (int b && < -10)) {
                return true;
              }
              return false;
            }
        "#;
        let report = function(src, "f");
        assert_eq!(report.cognitive, 4); // if + outer || + two && sequences
        assert_eq!(report.cyclomatic, 5); // base + if + three pattern operators
    }

    #[test]
    fn parenthesized_like_pattern_operators_fold() {
        let src = r#"
            bool f(Object value) {
              if (value case int a && (> 0 && < 10)) return true;
              return false;
            }
        "#;
        let report = function(src, "f");
        assert_eq!(report.cognitive, 2); // if + one folded && sequence
        assert_eq!(report.cyclomatic, 4); // base + if + two && operators
    }

    #[test]
    fn switch_pattern_logical_and_guard_expression_both_count() {
        let src = r#"
            int f(Object value, bool enabled, bool ready) {
              switch (value) {
                case int parsed && > 0 when enabled && ready:
                  return parsed;
                default:
                  return 0;
              }
            }
        "#;
        let report = function(src, "f");
        assert_eq!(report.cognitive, 3); // switch + pattern && + guard &&
        assert_eq!(report.cyclomatic, 4); // base + case + pattern && + guard &&
    }

    #[test]
    fn pattern_logicals_in_collection_patterns_are_separate_sequences() {
        let src = r#"
            int records(Object value) {
              switch (value) {
                case (int first && > 0, int second && < 10):
                  return first + second;
                default:
                  return 0;
              }
            }

            int map(Object value) {
              switch (value) {
                case {'left': int left && > 0, 'right': int right && < 10}:
                  return left + right;
                default:
                  return 0;
              }
            }
        "#;
        for name in ["records", "map"] {
            let report = function(src, name);
            assert_eq!(report.cognitive, 3, "{name}"); // switch + two && sequences
            assert_eq!(report.cyclomatic, 4, "{name}"); // base + case + two && operators
        }
    }

    #[test]
    fn switch_expression_pattern_logicals_are_counted() {
        let report = function(
            r#"
                String f(Object value) => switch (value) {
                  [int first && > 0, int second && < 10] => 'matched',
                  _ => 'other',
                };
            "#,
            "f",
        );
        assert_eq!(report.cognitive, 3); // switch + two && sequences
        assert_eq!(report.cyclomatic, 4); // base + case + two && operators
    }

    #[test]
    fn if_case_is_one_branch() {
        let report = function(
            "bool f(Object value) { if (value case int parsed) return parsed > 0; return false; }",
            "f",
        );
        assert_eq!(report.cognitive, 1);
        assert_eq!(report.cyclomatic, 2);
    }

    #[test]
    fn collection_if_and_for_nest_like_statements() {
        let src = r#"
            List<int> f(int value, List<int> values) => [
              if (value > 0) value else 0,
              for (final item in values)
                if (item.isEven) item,
            ];
        "#;
        let report = function(src, "f");
        assert_eq!(report.cognitive, 5); // if+else, then for + nested if
        assert_eq!(report.cyclomatic, 4); // base + two ifs + for
    }
}
