//! Zig adapter: parses source with [zigsyn](https://docs.rs/zigsyn) and
//! lowers the AST into the language-agnostic [`cccc_core::ir`].
//!
//! This crate is a pure library with no CLI dependencies. The unified `cccc`
//! binary registers [`analyze_source`] and [`DEFAULT_EXTS`] and dispatches
//! `.zig` files here.
//!
//! This crate contains **no scoring logic** — it recognizes named functions and
//! test blocks, `if`/`else`, `while`/`for`, `switch`, `catch`, labelled jumps,
//! `and`/`or`/`orelse`, and calls, then emits the corresponding IR nodes. All
//! complexity rules remain in [`cccc_core::engine`].
//!
//! ## Why a hand-written traversal
//!
//! zigsyn does not expose a full-traversal visitor, so lowering follows the same
//! approach as `cccc-go`: the node-bearing AST enums are matched exhaustively.
//! Adding a node variant therefore produces a compile error until the adapter
//! decides how to traverse it. A collector stack assembles nested IR bodies.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use zigsyn::ast::*;
use zigsyn::token::{Keyword, Token};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["zig"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR.
///
/// zigsyn parses a whole file at once and does not recover from syntax errors,
/// so a parse failure yields an empty node list and one parser error string.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    match zigsyn::parse_source(source) {
        Ok(file) => {
            let mut builder = Builder::new(file.line_info.clone());
            builder.visit_file(&file);
            (builder.finish(), Vec::new())
        }
        Err(error) => (Vec::new(), vec![error.to_string()]),
    }
}

/// Assembles the IR while explicitly traversing zigsyn's node-bearing AST enums.
struct Builder {
    stack: Vec<Vec<Node>>,
    line_starts: Vec<usize>,
}

impl Builder {
    fn new(line_starts: Vec<usize>) -> Self {
        Self {
            stack: vec![Vec::new()],
            line_starts,
        }
    }

    fn finish(mut self) -> Vec<Node> {
        self.stack.pop().expect("module collector")
    }

    fn line_of(&self, pos: usize) -> u32 {
        self.line_starts.partition_point(|&start| start <= pos) as u32
    }

    fn emit(&mut self, node: Node) {
        self.stack.last_mut().expect("collector").push(node);
    }

    fn collect(&mut self, walk: impl FnOnce(&mut Self)) -> Vec<Node> {
        self.stack.push(Vec::new());
        walk(self);
        self.stack.pop().expect("collector")
    }

    fn emit_function(
        &mut self,
        name: String,
        kind: &'static str,
        line: u32,
        walk: impl FnOnce(&mut Self),
    ) {
        let body = self.collect(walk);
        self.emit(Node::Function {
            name,
            kind: kind.to_string(),
            line,
            body,
        });
    }

    // ---- declarations -----------------------------------------------------

    fn visit_file(&mut self, file: &File) {
        for member in &file.members {
            self.visit_container_member(member);
        }
    }

    fn visit_container_member(&mut self, member: &ContainerMember) {
        match member {
            ContainerMember::Field(field) => self.visit_container_field(field),
            ContainerMember::Fn(function) => self.visit_fn_decl(function),
            ContainerMember::Var(var) => self.visit_var_decl(var),
            ContainerMember::Test(test) => self.visit_test_decl(test),
            ContainerMember::Comptime(comptime) => self.visit_block(&comptime.block),
        }
    }

    fn visit_container_field(&mut self, field: &ContainerField) {
        self.visit_optional_expression(field.ty.as_ref());
        self.visit_optional_expression(field.align.as_ref());
        self.visit_optional_expression(field.default.as_ref());
    }

    fn visit_fn_decl(&mut self, function: &FnDecl) {
        let Some(body) = &function.body else {
            self.visit_fn_signature(function);
            return;
        };
        let name = function
            .name
            .as_ref()
            .map(|name| name.name.clone())
            .unwrap_or_else(|| "<function>".to_string());
        let line = self.line_of(function.pos);
        self.emit_function(name, "function", line, |builder| {
            builder.visit_fn_signature(function);
            builder.visit_block(body);
        });
    }

    fn visit_fn_signature(&mut self, function: &FnDecl) {
        for param in &function.params {
            match &param.ty {
                ParamType::AnyType(_) | ParamType::VarArgs(_) => {}
                ParamType::Type(ty) => self.visit_expression(ty),
            }
        }
        self.visit_optional_expression(function.align.as_ref());
        self.visit_optional_expression(function.addrspace.as_ref());
        self.visit_optional_expression(function.linksection.as_ref());
        self.visit_optional_expression(function.callconv.as_ref());
        self.visit_optional_expression(function.ret.as_deref());
    }

    fn visit_var_decl(&mut self, var: &VarDecl) {
        for lhs in &var.lhs {
            match lhs {
                VarProtoOrExpr::VarProto { ty, .. } => self.visit_optional_expression(ty.as_ref()),
                VarProtoOrExpr::Expr(expr) => self.visit_expression(expr),
            }
        }
        self.visit_optional_expression(var.ty.as_ref());
        self.visit_optional_expression(var.align.as_ref());
        self.visit_optional_expression(var.addrspace.as_ref());
        self.visit_optional_expression(var.linksection.as_ref());
        self.visit_optional_expression(var.init.as_ref());
    }

    fn visit_test_decl(&mut self, test: &TestDecl) {
        let name = test.name.as_ref().map_or_else(
            || "<test>".to_string(),
            |expr| format!("test {}", expression_label(expr)),
        );
        let line = self.line_of(test.pos);
        self.emit_function(name, "test", line, |builder| {
            builder.visit_optional_expression(test.name.as_ref());
            builder.visit_block(&test.body);
        });
    }

    // ---- statements -------------------------------------------------------

    fn visit_block(&mut self, block: &Block) {
        for statement in &block.statements {
            self.visit_statement(statement);
        }
    }

    fn visit_statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Var(var) => self.visit_var_decl(var),
            Statement::Defer(defer) => self.visit_statement(&defer.body),
            Statement::If(if_) => {
                let node = self.lower_if_statement(if_);
                self.emit(node);
            }
            Statement::Loop(loop_) => self.visit_loop_statement(loop_),
            Statement::Switch(switch) => self.visit_switch_expression(&switch.expr),
            Statement::Suspend(suspend) => self.visit_statement(&suspend.body),
            Statement::Nosuspend(nosuspend) => self.visit_statement(&nosuspend.body),
            Statement::Comptime(comptime) => self.visit_statement(&comptime.body),
            Statement::Assign(assign) => {
                for lhs in &assign.lhs {
                    self.visit_expression(lhs);
                }
                self.visit_expression(&assign.rhs);
            }
            Statement::Expr(expr) => self.visit_expression(&expr.expr),
            Statement::Block(block) => self.visit_block(block),
        }
    }

    fn lower_if_statement(&mut self, if_: &IfStmt) -> Node {
        let test = self.collect(|builder| builder.visit_expression(&if_.condition));
        let then = self.collect(|builder| builder.visit_statement(&if_.then_branch));
        let alternate = if_.else_branch.as_deref().map(|branch| {
            Box::new(match branch {
                Statement::If(else_if) => self.lower_if_statement(else_if),
                other => Node::Group(self.collect(|builder| builder.visit_statement(other))),
            })
        });
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    fn visit_loop_statement(&mut self, loop_: &LoopStmt) {
        let body = self.collect(|builder| {
            match &loop_.kind {
                LoopKind::While {
                    condition,
                    continue_expr,
                    ..
                } => {
                    builder.visit_expression(condition);
                    builder.visit_optional_expression(continue_expr.as_deref());
                }
                LoopKind::For { items, .. } => builder.visit_for_items(items),
            }
            builder.visit_statement(&loop_.body);
        });
        self.emit(Node::Loop { body });
        if let Some(else_branch) = &loop_.else_branch {
            self.visit_statement(else_branch);
        }
    }

    // ---- expressions ------------------------------------------------------

    fn visit_expression(&mut self, expression: &Expression) {
        match expression {
            Expression::Ident(_)
            | Expression::BasicLit(_)
            | Expression::MultilineStr(_)
            | Expression::EnumLiteral(_)
            | Expression::ErrorValue(_)
            | Expression::ErrorSetDecl(_)
            | Expression::Unreachable(_) => {}
            Expression::BuiltinCall(call) => self.visit_expressions(&call.args),
            Expression::AnonInit(init) => {
                self.visit_expressions(&init.fields);
                for entry in &init.entries {
                    self.visit_init_entry(entry);
                }
            }
            Expression::Grouped(inner)
            | Expression::Comptime(inner)
            | Expression::Nosuspend(inner)
            | Expression::Resume(inner) => self.visit_expression(inner),
            Expression::AnyframeType(anyframe) => {
                self.visit_optional_expression(anyframe.result.as_deref())
            }
            Expression::Unary(unary) => self.visit_expression(&unary.expr),
            Expression::Binary(binary) => self.visit_binary(binary),
            Expression::Catch(catch) => self.visit_catch(catch),
            Expression::Assign(assign) => {
                self.visit_expressions(&assign.lhs);
                self.visit_expression(&assign.rhs);
            }
            Expression::If(if_) => {
                let node = self.lower_if_expression(if_);
                self.emit(node);
            }
            Expression::While(while_) => self.visit_while_expression(while_),
            Expression::For(for_) => self.visit_for_expression(for_),
            Expression::Switch(switch) => self.visit_switch_expression(switch),
            Expression::Block(block) => self.visit_block(block),
            Expression::Break(break_) => {
                self.emit(Node::Jump {
                    labeled: break_.label.is_some(),
                });
                self.visit_optional_expression(break_.value.as_deref());
            }
            Expression::Continue(continue_) => {
                self.emit(Node::Jump {
                    labeled: continue_.label.is_some(),
                });
                self.visit_optional_expression(continue_.value.as_deref());
            }
            Expression::Return(return_) => self.visit_optional_expression(return_.value.as_deref()),
            Expression::Asm(asm) => self.visit_asm(asm),
            Expression::Call(call) => {
                self.emit(Node::Call {
                    callee: callee_name(&call.callee),
                });
                self.visit_expression(&call.callee);
                self.visit_expressions(&call.args);
            }
            Expression::Index(index) => {
                self.visit_expression(&index.expr);
                self.visit_expression(&index.index);
            }
            Expression::Slice(slice) => {
                self.visit_expression(&slice.expr);
                self.visit_optional_expression(slice.start.as_deref());
                self.visit_optional_expression(slice.end.as_deref());
                self.visit_optional_expression(slice.sentinel.as_deref());
            }
            Expression::FieldAccess(field) => self.visit_expression(&field.expr),
            Expression::Deref(deref) => self.visit_expression(&deref.expr),
            Expression::Unwrap(unwrap) => self.visit_expression(&unwrap.expr),
            Expression::InitList(init) => {
                self.visit_expression(&init.ty);
                for entry in &init.entries {
                    self.visit_init_entry(entry);
                }
            }
            Expression::Optional(optional) => self.visit_expression(&optional.child),
            Expression::ErrorUnion(union) => {
                self.visit_expression(&union.error);
                self.visit_expression(&union.payload);
            }
            Expression::Pointer(pointer) => {
                match &pointer.kind {
                    PointerKind::One | PointerKind::Many | PointerKind::C => {}
                    PointerKind::Sentinel(sentinel) => self.visit_expression(sentinel),
                }
                self.visit_pointer_modifiers(&pointer.modifiers);
                self.visit_expression(&pointer.child);
            }
            Expression::SliceType(slice) => {
                self.visit_optional_expression(slice.sentinel.as_deref());
                self.visit_pointer_modifiers(&slice.modifiers);
                self.visit_expression(&slice.child);
            }
            Expression::ArrayType(array) => {
                self.visit_expression(&array.len);
                self.visit_optional_expression(array.sentinel.as_deref());
                self.visit_expression(&array.child);
            }
            Expression::Container(container) => {
                if let Some(arg) = &container.arg {
                    match arg {
                        ContainerArg::Expr(expr) => self.visit_expression(expr),
                        ContainerArg::Enum { tag_type, .. } => {
                            self.visit_optional_expression(tag_type.as_deref())
                        }
                    }
                }
                for member in &container.members {
                    self.visit_container_member(member);
                }
            }
            Expression::FnProto(function) => self.visit_fn_signature(function),
        }
    }

    // ---- control flow -----------------------------------------------------

    fn lower_if_expression(&mut self, if_: &IfExpr) -> Node {
        let test =
            self.collect(|builder| builder.visit_optional_expression(if_.condition.as_deref()));
        let then =
            self.collect(|builder| builder.visit_optional_expression(if_.then_branch.as_deref()));
        let alternate = if_.else_branch.as_deref().map(|branch| {
            Box::new(match branch {
                Expression::If(else_if) => self.lower_if_expression(else_if),
                other => Node::Group(self.collect(|builder| builder.visit_expression(other))),
            })
        });
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    fn visit_while_expression(&mut self, while_: &WhileExpr) {
        let body = self.collect(|builder| {
            builder.visit_optional_expression(while_.condition.as_deref());
            builder.visit_optional_expression(while_.continue_expr.as_deref());
            builder.visit_optional_expression(while_.body.as_deref());
        });
        self.emit(Node::Loop { body });
        self.visit_optional_expression(while_.else_branch.as_deref());
    }

    fn visit_for_expression(&mut self, for_: &ForExpr) {
        let body = self.collect(|builder| {
            builder.visit_for_items(&for_.items);
            builder.visit_optional_expression(for_.body.as_deref());
        });
        self.emit(Node::Loop { body });
        self.visit_optional_expression(for_.else_branch.as_deref());
    }

    fn visit_switch_expression(&mut self, switch: &SwitchExpr) {
        self.visit_optional_expression(switch.target.as_deref());
        let mut cases = Vec::with_capacity(switch.prongs.len());
        for prong in &switch.prongs {
            let body = self.collect(|builder| builder.visit_expression(&prong.body));
            cases.push(SwitchCase {
                is_default: prong.cases.iter().any(switch_item_is_default),
                body,
            });
        }
        self.emit(Node::Switch { cases });
    }

    fn visit_binary(&mut self, binary: &BinaryOp) {
        let Some(op) = logical_op(&binary.op) else {
            self.visit_expression(&binary.lhs);
            self.visit_expression(&binary.rhs);
            return;
        };
        let mut operands = Vec::new();
        self.collect_logical_side(&binary.lhs, op, &mut operands);
        self.collect_logical_side(&binary.rhs, op, &mut operands);
        self.emit(Node::Logical { op, operands });
    }

    fn collect_logical_side(
        &mut self,
        expression: &Expression,
        op: LogicalOp,
        out: &mut Vec<Node>,
    ) {
        if let Expression::Grouped(inner) = expression {
            self.collect_logical_side(inner, op, out);
            return;
        }
        if let Expression::Binary(binary) = expression
            && let Some(inner_op) = logical_op(&binary.op)
        {
            if inner_op == op {
                self.collect_logical_side(&binary.lhs, op, out);
                self.collect_logical_side(&binary.rhs, op, out);
            } else {
                let mut operands = Vec::new();
                self.collect_logical_side(&binary.lhs, inner_op, &mut operands);
                self.collect_logical_side(&binary.rhs, inner_op, &mut operands);
                out.push(Node::Logical {
                    op: inner_op,
                    operands,
                });
            }
            return;
        }
        out.push(Node::Group(
            self.collect(|builder| builder.visit_expression(expression)),
        ));
    }

    fn visit_catch(&mut self, catch: &CatchExpr) {
        self.visit_expression(&catch.lhs);
        let body = self.collect(|builder| builder.visit_expression(&catch.rhs));
        self.emit(Node::Catch { body });
    }

    // ---- transparent child traversal -------------------------------------

    fn visit_asm(&mut self, asm: &AsmExpr) {
        self.visit_optional_expression(asm.expr.as_deref());
        for output in &asm.outputs {
            self.visit_expression(&output.constraint);
            match &output.target {
                AsmOutputTarget::Type(ty) => self.visit_expression(ty),
                AsmOutputTarget::Ident(_) => {}
            }
        }
        for input in &asm.inputs {
            self.visit_expression(&input.constraint);
            self.visit_expression(&input.expr);
        }
        self.visit_optional_expression(asm.clobbers.as_deref());
    }

    fn visit_init_entry(&mut self, entry: &InitEntry) {
        match entry {
            InitEntry::Field { value, .. } | InitEntry::Expr(value) => self.visit_expression(value),
        }
    }

    fn visit_pointer_modifiers(&mut self, modifiers: &[PointerModifier]) {
        for modifier in modifiers {
            match modifier {
                PointerModifier::Align(expressions) => self.visit_expressions(expressions),
                PointerModifier::AddrSpace(expression) => self.visit_expression(expression),
                PointerModifier::Const | PointerModifier::Volatile | PointerModifier::AllowZero => {
                }
            }
        }
    }

    fn visit_for_items(&mut self, items: &[ForItem]) {
        for item in items {
            self.visit_expression(&item.expr);
            if let Some(Some(end)) = &item.range {
                self.visit_expression(end);
            }
        }
    }

    fn visit_expressions(&mut self, expressions: &[Expression]) {
        for expression in expressions {
            self.visit_expression(expression);
        }
    }

    fn visit_optional_expression(&mut self, expression: Option<&Expression>) {
        if let Some(expression) = expression {
            self.visit_expression(expression);
        }
    }
}

fn logical_op(token: &Token) -> Option<LogicalOp> {
    match token {
        Token::Keyword(Keyword::And) => Some(LogicalOp::And),
        Token::Keyword(Keyword::Or) => Some(LogicalOp::Or),
        Token::Keyword(Keyword::Orelse) => Some(LogicalOp::Coalesce),
        _ => None,
    }
}

fn callee_name(expression: &Expression) -> Option<String> {
    match expression {
        Expression::Ident(ident) => Some(ident.name.clone()),
        Expression::FieldAccess(field) => Some(field.field.name.clone()),
        Expression::Grouped(inner) => callee_name(inner),
        _ => None,
    }
}

fn expression_label(expression: &Expression) -> String {
    match expression {
        Expression::Ident(ident) => ident.name.clone(),
        Expression::BasicLit(literal) => literal.text.trim_matches('"').to_string(),
        _ => "<anonymous>".to_string(),
    }
}

fn switch_item_is_default(item: &SwitchItem) -> bool {
    match item {
        SwitchItem::Expr(_) | SwitchItem::Range(_, _) => false,
        SwitchItem::Else(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(source: &str) -> FileReport {
        analyze_source(Path::new("test.zig"), source)
    }

    fn find<'a>(functions: &'a [FunctionReport], name: &str) -> Option<&'a FunctionReport> {
        for function in functions {
            if function.name == name {
                return Some(function);
            }
            if let Some(found) = find(&function.children, name) {
                return Some(found);
            }
        }
        None
    }

    fn cognitive_of(source: &str, name: &str) -> u32 {
        find(&analyze(source).functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cognitive
    }

    fn cyclomatic_of(source: &str, name: &str) -> u32 {
        find(&analyze(source).functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cyclomatic
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        let source = r#"
pub fn sumOfPrimes(max: u32) u32 {
    var total: u32 = 0;
    outer: for (2..max) |i| {
        for (2..i) |j| {
            if (i % j == 0) {
                continue :outer;
            }
        }
        total += i;
    }
    return total;
}
"#;
        assert_eq!(cognitive_of(source, "sumOfPrimes"), 7);
        assert_eq!(cyclomatic_of(source, "sumOfPrimes"), 4);
    }

    #[test]
    fn switch_else_is_the_default_arm() {
        let source = r#"
fn classify(value: u8) []const u8 {
    return switch (value) {
        1 => "one",
        2 => "two",
        else => "many",
    };
}
"#;
        assert_eq!(cognitive_of(source, "classify"), 1);
        assert_eq!(cyclomatic_of(source, "classify"), 3);
    }

    #[test]
    fn else_if_and_else_are_flat() {
        let source = r#"
fn choose(a: bool, b: bool) void {
    if (a) {
        useA();
    } else if (b) {
        useB();
    } else {
        useDefault();
    }
}
"#;
        assert_eq!(cognitive_of(source, "choose"), 3);
        assert_eq!(cyclomatic_of(source, "choose"), 3);
    }

    #[test]
    fn logical_and_coalescing_runs_fold_by_operator() {
        let source = r#"
fn choose(a: bool, b: bool, c: bool, fallback: ?bool) bool {
    return (a and b and c) or (fallback orelse false);
}
"#;
        assert_eq!(cognitive_of(source, "choose"), 3);
        assert_eq!(cyclomatic_of(source, "choose"), 5);
    }

    #[test]
    fn catch_handler_is_nested() {
        let source = r#"
fn recover() void {
    risky() catch |err| {
        if (isFatal(err)) {
            return;
        }
    };
}
"#;
        assert_eq!(cognitive_of(source, "recover"), 3);
        assert_eq!(cyclomatic_of(source, "recover"), 3);
    }

    #[test]
    fn loop_else_runs_at_the_surrounding_level() {
        let source = r#"
fn wait(ready: bool, fallback: bool) void {
    while (ready) {
        tick();
    } else {
        if (fallback) useFallback();
    }
}
"#;
        assert_eq!(cognitive_of(source, "wait"), 2);
        assert_eq!(cyclomatic_of(source, "wait"), 3);
    }

    #[test]
    fn recursion_and_labelled_jumps_are_detected() {
        let source = r#"
fn walk(n: u32) u32 {
    if (n == 0) return 0;
    return walk(n - 1);
}

fn scan(items: []const u8) void {
    outer: for (items) |_| {
        break :outer;
    }
}
"#;
        assert_eq!(cognitive_of(source, "walk"), 2);
        assert_eq!(cognitive_of(source, "scan"), 2);
    }

    #[test]
    fn functions_in_containers_and_tests_are_units() {
        let source = r#"
const Service = struct {
    fn run(ok: bool) void {
        if (ok) work();
    }
};

test "service" {
    if (enabled()) work();
}
"#;
        let report = analyze(source);
        let run = find(&report.functions, "run").expect("run");
        let test = find(&report.functions, "test service").expect("test service");
        assert_eq!((run.kind.as_str(), run.cognitive), ("function", 1));
        assert_eq!((test.kind.as_str(), test.cognitive), ("test", 1));
    }

    #[test]
    fn module_level_comptime_is_scored() {
        let source = r#"
comptime {
    if (enabled()) generate();
}
"#;
        let report = analyze(source);
        assert_eq!(report.cognitive, 1);
        assert!(report.functions.is_empty());
    }

    #[test]
    fn function_line_comes_from_source_position() {
        let source = "\n\nfn third() void {}\n";
        let report = analyze(source);
        assert_eq!(find(&report.functions, "third").expect("third").line, 3);
    }

    #[test]
    fn parse_error_is_reported() {
        let (nodes, errors) = to_ir(Path::new("bad.zig"), "fn broken( {");
        assert!(nodes.is_empty());
        assert!(!errors.is_empty());
    }
}
