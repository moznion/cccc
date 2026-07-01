//! Go adapter: parses source with [gosyn](https://docs.rs/gosyn) and lowers the
//! AST into the language-agnostic [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core` and `gosyn` (itself a
//! pure-Rust Go parser, so there is no C toolchain and cross-compilation stays
//! clean), with no CLI machinery. The unified `cccc` binary (the `cccc-cli`
//! crate) registers this adapter's [`analyze_source`]/[`DEFAULT_EXTS`] in its
//! language registry and dispatches `.go` files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about (functions/methods/closures, `if`/`else`, `switch`/
//! type-switch/`select`, loops, labelled jumps, `&&`/`||` sequences, calls) and
//! emits the matching IR nodes. All complexity rules live in
//! [`cccc_core::engine`].
//!
//! ## Why a hand-written traversal
//!
//! Unlike `syn` (used by `cccc-rs`) or oxc (used by `cccc-es`), gosyn does not
//! ship a "walk every child" visitor trait. We therefore drive lowering with an
//! explicit recursion over the AST. To keep the completeness guarantee a visitor
//! would give us, every [`Statement`] and [`Expression`] variant is matched
//! **exhaustively** (no `_ => {}` wildcard over node kinds): the compiler forces
//! us to consider each construct, so a closure or logical operator appearing in
//! any position is still reached. The IR tree is assembled with a stack of
//! "collectors": [`Builder::collect`] pushes a fresh child vector, runs a
//! sub-traversal, and pops the nodes it gathered.
//!
//! ## Go-to-IR mapping notes
//!
//! - top-level `func` / method (`func (recv) ..`) / function literal →
//!   [`Node::Function`] (`"function"` / `"method"` / `"closure"`). Go has no
//!   nested function *declarations*; only literals nest.
//! - `if` / `else if` / `else` → [`Node::Branch`] (chaining `else if` as a nested
//!   `Branch` so it scores flat).
//! - `for` (all three header forms) and `for … range …` → [`Node::Loop`].
//! - `switch`, type-`switch`, and `select` → [`Node::Switch`]; a `default` arm
//!   (an empty case-expression list, or the `select` `default`) is not a
//!   cyclomatic decision point.
//! - labelled `break`/`continue` and `goto` → [`Node::Jump`] (`labeled: true`);
//!   plain `break`/`continue`/`fallthrough` score flat.
//! - `&&` / `||` runs → folded [`Node::Logical`] (one node per like-operator run).
//! - calls (`f(..)`, `obj.m(..)`) → [`Node::Call`] for recursion detection.
//!
//! Go has no ternary (`if` is a statement, not an expression) and no
//! `try`/`catch` (errors are ordinary values), so no `Conditional`/`Catch` nodes
//! are emitted, and it has no nullish-coalescing operator.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use cccc_core::report::FileReport;
use gosyn::ast::{
    AssignStmt, BlockStmt, Call, CaseBlock, DeclStmt, Declaration, Element, Expression, FuncDecl,
    Ident, IfStmt, LiteralValue, Operation, Statement,
};
use gosyn::token::{Keyword, Operator};

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["go"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any parser error messages. gosyn parses a whole file at once and
/// does not recover from syntax errors, so a parse failure yields an empty node
/// list and a single error string.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    match gosyn::parse_source(source) {
        Ok(file) => {
            let mut builder = Builder::new(source);
            for decl in &file.decl {
                builder.visit_decl(decl);
            }
            (builder.finish(), Vec::new())
        }
        Err(e) => (Vec::new(), vec![e.to_string()]),
    }
}

/// Assembles the IR tree while an explicit recursion walks the gosyn AST.
struct Builder {
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
    /// Byte offset of the start of each 1-based line, for `pos` → line lookup.
    line_starts: Vec<usize>,
    /// Name captured from an assignment/`var` to label the next function literal.
    pending_name: Option<String>,
}

impl Builder {
    fn new(source: &str) -> Self {
        // line_starts[0] = 0 (line 1); each '\n' begins the next line.
        let mut line_starts = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self {
            stack: vec![Vec::new()], // module-level collector
            line_starts,
            pending_name: None,
        }
    }

    /// 1-based line containing byte offset `pos` (gosyn positions are byte
    /// offsets into the source we parsed).
    fn line_of(&self, pos: usize) -> u32 {
        self.line_starts.partition_point(|&start| start <= pos) as u32
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

    // ---- declarations -----------------------------------------------------

    fn visit_decl(&mut self, decl: &Declaration) {
        match decl {
            Declaration::Function(f) => self.visit_func_decl(f),
            // A top-level `var`/`const` value can be a function literal.
            Declaration::Variable(d) => {
                for spec in &d.specs {
                    self.visit_value_spec(&spec.name, &spec.values);
                }
            }
            Declaration::Const(d) => {
                for spec in &d.specs {
                    self.visit_value_spec(&spec.name, &spec.values);
                }
            }
            // Type declarations carry no runnable complexity.
            Declaration::Type(_) => {}
        }
    }

    fn visit_func_decl(&mut self, f: &FuncDecl) {
        // A bodyless declaration (e.g. an assembly/external stub) is not a
        // measurable unit, so don't report it as a function.
        let Some(body) = &f.body else { return };
        let kind = if f.recv.is_some() {
            "method"
        } else {
            "function"
        };
        let line = self.line_of(f.name.pos);
        let name = f.name.name.clone();
        self.emit_function(name, kind, line, |s| s.visit_block(body));
    }

    // ---- statements -------------------------------------------------------

    fn visit_block(&mut self, block: &BlockStmt) {
        for stmt in &block.list {
            self.visit_stmt(stmt);
        }
    }

    fn visit_stmt(&mut self, stmt: &Statement) {
        match stmt {
            Statement::If(it) => {
                let node = self.lower_if(it);
                self.emit(node);
            }
            Statement::For(f) => {
                let body = self.collect(|s| {
                    if let Some(init) = &f.init {
                        s.visit_stmt(init);
                    }
                    if let Some(cond) = &f.cond {
                        s.visit_stmt(cond);
                    }
                    if let Some(post) = &f.post {
                        s.visit_stmt(post);
                    }
                    s.visit_block(&f.body);
                });
                self.emit(Node::Loop { body });
            }
            Statement::Range(r) => {
                let body = self.collect(|s| {
                    if let Some(key) = &r.key {
                        s.visit_expr(key);
                    }
                    if let Some(value) = &r.value {
                        s.visit_expr(value);
                    }
                    s.visit_expr(&r.expr);
                    s.visit_block(&r.body);
                });
                self.emit(Node::Loop { body });
            }
            Statement::Switch(sw) => {
                // The init/tag run at the switch's own level, before its cases.
                let head = self.collect(|s| {
                    if let Some(init) = &sw.init {
                        s.visit_stmt(init);
                    }
                    if let Some(tag) = &sw.tag {
                        s.visit_expr(tag);
                    }
                });
                for node in head {
                    self.emit(node);
                }
                let cases = self.lower_case_block(&sw.block);
                self.emit(Node::Switch { cases });
            }
            Statement::TypeSwitch(ts) => {
                let head = self.collect(|s| {
                    if let Some(init) = &ts.init {
                        s.visit_stmt(init);
                    }
                    if let Some(tag) = &ts.tag {
                        s.visit_stmt(tag);
                    }
                });
                for node in head {
                    self.emit(node);
                }
                let cases = self.lower_case_block(&ts.block);
                self.emit(Node::Switch { cases });
            }
            Statement::Select(sel) => {
                let mut cases = Vec::new();
                for clause in &sel.body.body {
                    let body = self.collect(|s| {
                        if let Some(comm) = &clause.comm {
                            s.visit_stmt(comm);
                        }
                        for st in clause.body.iter() {
                            s.visit_stmt(st);
                        }
                    });
                    cases.push(SwitchCase {
                        is_default: matches!(clause.tok, Keyword::Default),
                        body,
                    });
                }
                self.emit(Node::Switch { cases });
            }
            Statement::Branch(b) => {
                // Labelled break/continue and goto (which always names a label)
                // score one flat point; plain break/continue/fallthrough do not.
                self.emit(Node::Jump {
                    labeled: b.ident.is_some(),
                });
            }
            Statement::Go(g) => self.visit_call(&g.call),
            Statement::Defer(d) => self.visit_call(&d.call),
            Statement::Send(s) => {
                self.visit_expr(&s.chan);
                self.visit_expr(&s.value);
            }
            Statement::Expr(e) => self.visit_expr(&e.expr),
            Statement::IncDec(i) => self.visit_expr(&i.expr),
            Statement::Return(r) => {
                for e in &r.ret {
                    self.visit_expr(e);
                }
            }
            Statement::Assign(a) => self.visit_assign(a),
            Statement::Label(l) => self.visit_stmt(&l.stmt),
            Statement::Block(b) => self.visit_block(b),
            Statement::Declaration(d) => self.visit_decl_stmt(d),
            Statement::Empty(_) => {}
        }
    }

    fn visit_decl_stmt(&mut self, decl: &DeclStmt) {
        match decl {
            DeclStmt::Variable(d) => {
                for spec in &d.specs {
                    self.visit_value_spec(&spec.name, &spec.values);
                }
            }
            DeclStmt::Const(d) => {
                for spec in &d.specs {
                    self.visit_value_spec(&spec.name, &spec.values);
                }
            }
            DeclStmt::Type(_) => {}
        }
    }

    /// Walk a `var`/`const` spec's values; capture `name = func(){…}` so the
    /// resulting closure is labelled with the binding name.
    fn visit_value_spec(&mut self, names: &[Ident], values: &[Expression]) {
        if names.len() == 1 && values.len() == 1 && matches!(values[0], Expression::FuncLit(_)) {
            self.pending_name = Some(names[0].name.clone());
        }
        for value in values {
            self.visit_expr(value);
        }
    }

    fn visit_assign(&mut self, a: &AssignStmt) {
        // `name := func(){…}` / `name = func(){…}` labels the next closure.
        if a.left.len() == 1
            && a.right.len() == 1
            && let (Expression::Ident(id), Expression::FuncLit(_)) = (&a.left[0], &a.right[0])
        {
            self.pending_name = Some(id.name.clone());
        }
        for e in &a.left {
            self.visit_expr(e);
        }
        for e in &a.right {
            self.visit_expr(e);
        }
    }

    /// Build an `if` (recursively, so `else if` becomes a nested `Branch`).
    fn lower_if(&mut self, it: &IfStmt) -> Node {
        let test = self.collect(|s| {
            if let Some(init) = &it.init {
                s.visit_stmt(init);
            }
            s.visit_expr(&it.cond);
        });
        let then = self.collect(|s| s.visit_block(&it.body));
        let alternate = it
            .else_
            .as_ref()
            .map(|alt| Box::new(self.lower_alternate(alt)));
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// `else if` → nested `Branch`; plain `else { … }` → `Group`.
    fn lower_alternate(&mut self, stmt: &Statement) -> Node {
        match stmt {
            Statement::If(elif) => self.lower_if(elif),
            other => Node::Group(self.collect(|s| s.visit_stmt(other))),
        }
    }

    /// One `SwitchCase` per case clause; an empty expression list is `default`.
    fn lower_case_block(&mut self, block: &CaseBlock) -> Vec<SwitchCase> {
        let mut cases = Vec::new();
        for clause in &block.body {
            let body = self.collect(|s| {
                for expr in &clause.list {
                    s.visit_expr(expr);
                }
                for st in clause.body.iter() {
                    s.visit_stmt(st);
                }
            });
            cases.push(SwitchCase {
                is_default: clause.list.is_empty(),
                body,
            });
        }
        cases
    }

    // ---- expressions ------------------------------------------------------

    fn visit_call(&mut self, call: &Call) {
        self.emit(Node::Call {
            callee: callee_name(&call.func),
        });
        self.visit_expr(&call.func);
        for arg in &call.args {
            self.visit_expr(arg);
        }
    }

    fn visit_operation(&mut self, op: &Operation) {
        match (logical_op(&op.op), &op.y) {
            (Some(lop), Some(y)) => {
                let mut operands = Vec::new();
                self.collect_logical_side(&op.x, lop, &mut operands);
                self.collect_logical_side(y, lop, &mut operands);
                self.emit(Node::Logical { op: lop, operands });
            }
            _ => {
                self.visit_expr(&op.x);
                if let Some(y) = &op.y {
                    self.visit_expr(y);
                }
            }
        }
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    fn collect_logical_side(&mut self, side: &Expression, op: LogicalOp, operands: &mut Vec<Node>) {
        match side {
            Expression::Operation(inner) => match (logical_op(&inner.op), &inner.y) {
                (Some(inner_op), Some(iy)) if inner_op == op => {
                    self.collect_logical_side(&inner.x, op, operands);
                    self.collect_logical_side(iy, op, operands);
                }
                (Some(inner_op), Some(iy)) => {
                    let mut sub = Vec::new();
                    self.collect_logical_side(&inner.x, inner_op, &mut sub);
                    self.collect_logical_side(iy, inner_op, &mut sub);
                    operands.push(Node::Logical {
                        op: inner_op,
                        operands: sub,
                    });
                }
                _ => operands.push(Node::Group(self.collect(|s| s.visit_expr(side)))),
            },
            Expression::Paren(p) => self.collect_logical_side(&p.expr, op, operands),
            _ => operands.push(Node::Group(self.collect(|s| s.visit_expr(side)))),
        }
    }

    fn visit_literal_value(&mut self, lv: &LiteralValue) {
        for ke in &lv.values {
            if let Some(key) = &ke.key {
                self.visit_element(key);
            }
            self.visit_element(&ke.val);
        }
    }

    fn visit_element(&mut self, e: &Element) {
        match e {
            Element::Expr(x) => self.visit_expr(x),
            Element::LitValue(lv) => self.visit_literal_value(lv),
        }
    }

    fn visit_expr(&mut self, expr: &Expression) {
        match expr {
            Expression::Call(c) => self.visit_call(c),
            Expression::FuncLit(fl) => {
                let name = self
                    .pending_name
                    .take()
                    .unwrap_or_else(|| "<closure>".to_string());
                let line = self.line_of(fl.typ.pos);
                self.emit_function(name, "closure", line, |s| s.visit_block(&fl.body));
            }
            Expression::Operation(op) => self.visit_operation(op),
            Expression::Paren(p) => self.visit_expr(&p.expr),
            Expression::Index(i) => {
                self.visit_expr(&i.left);
                self.visit_expr(&i.index);
            }
            Expression::IndexList(i) => {
                self.visit_expr(&i.left);
                for e in &i.indices {
                    self.visit_expr(e);
                }
            }
            Expression::Slice(s) => {
                self.visit_expr(&s.left);
                for e in s.index.iter().flatten() {
                    self.visit_expr(e);
                }
            }
            Expression::Selector(s) => self.visit_expr(&s.x),
            Expression::Star(s) => self.visit_expr(&s.right),
            Expression::TypeAssert(t) => self.visit_expr(&t.left),
            Expression::Ellipsis(e) => {
                if let Some(elt) = &e.elt {
                    self.visit_expr(elt);
                }
            }
            Expression::Range(r) => self.visit_expr(&r.right),
            Expression::CompositeLit(c) => self.visit_literal_value(&c.val),
            Expression::List(list) => {
                for e in list {
                    self.visit_expr(e);
                }
            }
            Expression::Ident(_) | Expression::BasicLit(_) => {}
            // Pure type expressions hold no runnable code.
            Expression::TypeMap(_)
            | Expression::TypeArray(_)
            | Expression::TypeSlice(_)
            | Expression::TypeFunction(_)
            | Expression::TypeStruct(_)
            | Expression::TypeChannel(_)
            | Expression::TypePointer(_)
            | Expression::TypeInterface(_) => {}
        }
    }
}

/// `&&` / `||` map to the normalized logical ops; everything else is not a
/// logical sequence. (Go has no nullish-coalescing operator.)
fn logical_op(op: &Operator) -> Option<LogicalOp> {
    match op {
        Operator::AndAnd => Some(LogicalOp::And),
        Operator::OrOr => Some(LogicalOp::Or),
        _ => None,
    }
}

/// Simple name of a directly-called callee (`foo(..)` or `obj.foo(..)`), used
/// for recursion detection. Returns the trailing identifier.
fn callee_name(func: &Expression) -> Option<String> {
    match func {
        Expression::Ident(id) => Some(id.name.clone()),
        Expression::Selector(s) => Some(s.sel.name.clone()),
        Expression::Paren(p) => callee_name(&p.expr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.go"), src)
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
            package p
            func sumOfPrimes(max int) int {
                total := 0
            OUT:
                for i := 2; i <= max; i++ {
                    for j := 2; j < i; j++ {
                        if i%j == 0 {
                            continue OUT
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
            package p
            func getWords(number int) string {
                switch number {
                case 1:
                    return "one"
                case 2:
                    return "a couple"
                default:
                    return "lots"
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
            package p
            func f(a, b, c bool) {
                if a {
                    if b {
                        if c {
                        }
                    }
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 6);
    }

    #[test]
    fn else_if_else_are_flat() {
        let src = r#"
            package p
            func f(a, b bool) {
                if a {
                } else if b {
                } else {
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences() {
        let src = r#"
            package p
            func f(a, b, c, d bool) {
                if a && b && c || d {
                }
            }
        "#;
        // if(+1) + && seq(+1) + || seq(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
            package p
            func fib(n int) int {
                if n < 2 {
                    return n
                }
                return fib(n-1) + fib(n-2)
            }
        "#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn method_recursion_is_detected() {
        let src = r#"
            package p
            type S struct{}
            func (s S) walk(n int) int {
                if n == 0 {
                    return 0
                } else {
                    return s.walk(n - 1)
                }
            }
        "#;
        // if(+1) + else(+1) + recursion(+1) = 3
        assert_eq!(cognitive_of(src, "walk"), 3);
        assert_eq!(
            find(&analyze(src).functions, "walk").unwrap().kind,
            "method"
        );
    }

    #[test]
    fn closure_is_its_own_unit() {
        let src = r#"
            package p
            func host() {
                pick := func(a, b bool) int {
                    if a && b {
                        return 1
                    } else {
                        return 0
                    }
                }
                _ = pick
            }
        "#;
        // host owns no structural complexity; the closure does.
        assert_eq!(cognitive_of(src, "host"), 0);
        // if(+1) + && seq(+1) + else(+1) = 3
        assert_eq!(cognitive_of(src, "pick"), 3);
        assert_eq!(
            find(&analyze(src).functions, "pick").unwrap().kind,
            "closure"
        );
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
            package p
            func f(a bool, items []int) {
                for a {
                }
                for i := 0; i < 3; i++ {
                }
                for i := range items {
                    _ = i
                }
            }
        "#;
        // three loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn cyclomatic_basic() {
        let src = r#"
            package p
            func f(a, b bool) {
                if a && b {
                    for i := 0; i < 1; i++ {
                    }
                } else if b {
                }
            }
        "#;
        // base 1 + if 1 + (&& => +1) + for 1 + else if 1 = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn names_methods_and_closures() {
        let src = r#"
            package p
            func free() {}
            type C struct{}
            func (c C) method() {}
            func host() {
                lambda := func(x int) int { return x + 1 }
                _ = lambda
            }
        "#;
        let r = analyze(src);
        assert_eq!(find(&r.functions, "free").unwrap().kind, "function");
        assert_eq!(find(&r.functions, "method").unwrap().kind, "method");
        assert_eq!(find(&r.functions, "lambda").unwrap().kind, "closure");
    }

    #[test]
    fn select_default_is_not_a_decision() {
        let src = r#"
            package p
            func f(ch chan int) {
                select {
                case v := <-ch:
                    _ = v
                default:
                }
            }
        "#;
        // select scores like a switch: +1 cognitive.
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + one non-default comm case = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
            package p
            func a(x bool) {
                if x {
                }
            }
            func b(y bool) {
                if y {
                }
            }
        "#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn parse_error_is_reported() {
        let (nodes, errors) = to_ir(Path::new("bad.go"), "package p\nfunc f( {");
        assert!(nodes.is_empty());
        assert_eq!(errors.len(), 1);
    }
}
