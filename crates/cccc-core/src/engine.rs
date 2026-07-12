//! The scoring engine: walks the normalized [`crate::ir`] and computes Cognitive
//! Complexity (SonarSource / G. Ann Campbell) and Cyclomatic Complexity (McCabe).
//!
//! ## Measurement model
//!
//! Every [`Node::Function`] is measured **independently**: its nesting level
//! starts at 0 at its own boundary, and a structural increment is attributed
//! only to the nearest enclosing function. Nested functions therefore do *not*
//! inflate the enclosing function's own score; they are reported as `children`.
//! A file's total is module-level code plus every function at every depth (each
//! structural increment counted exactly once).
//!
//! ## Cyclomatic Complexity (McCabe)
//!
//! Base 1 per function; +1 for each branch (`if`/`else if`), ternary, loop,
//! non-default `case`, `catch`, explicit null guard, and each logical
//! operator (one per extra operand in a [`Node::Logical`]).
//!
//! ## Cognitive Complexity (SonarSource)
//!
//! - +1 and +nesting bonus for: branch, ternary, switch, loop, catch.
//! - +1 flat (no bonus) for: `else` / `else if`, labelled jumps, each logical
//!   sequence, and recursion (a call to the nearest enclosing function's name).
//! - Nesting increases inside branch/ternary/switch/loop/catch bodies and nested
//!   function bodies.

use crate::ir::Node;
use crate::report::{FileReport, FunctionReport};

/// An in-progress accumulator for one function-like unit (or the module root).
///
/// `kind` is an opaque, adapter-chosen label; the engine only ever compares it
/// against the sentinel `"module"` (the implicit root frame) when deciding
/// whether a call counts as recursion.
struct Frame {
    name: String,
    kind: String,
    line: u32,
    cognitive: u32,
    cyclomatic: u32,
    nesting: u32,
    children: Vec<FunctionReport>,
}

struct Engine {
    /// `stack[0]` is always the module frame; deeper entries are functions.
    stack: Vec<Frame>,
}

impl Engine {
    fn new() -> Self {
        let module = Frame {
            name: "<module>".to_string(),
            kind: "module".to_string(),
            line: 1,
            cognitive: 0,
            cyclomatic: 0,
            nesting: 0,
            children: Vec::new(),
        };
        Self {
            stack: vec![module],
        }
    }

    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("stack never empty")
    }

    fn top_nesting(&self) -> u32 {
        self.stack.last().expect("stack never empty").nesting
    }

    fn add_cognitive(&mut self, amount: u32) {
        self.top().cognitive += amount;
    }

    fn add_cyclomatic(&mut self) {
        self.top().cyclomatic += 1;
    }

    fn enter_nesting(&mut self) {
        self.top().nesting += 1;
    }

    fn leave_nesting(&mut self) {
        self.top().nesting -= 1;
    }

    /// Walk a slice of sibling nodes at the current nesting/frame.
    fn walk(&mut self, nodes: &[Node]) {
        for node in nodes {
            self.visit(node);
        }
    }

    /// The structural increment shared by loops, switch, and catch: +1 plus the
    /// nesting bonus to cognitive, optionally a cyclomatic point, then the body
    /// with nesting raised by one. (`switch` passes `add_cyclomatic = false` —
    /// the switch itself is not a McCabe decision point; its cases are.)
    fn nested(&mut self, add_cyclomatic: bool, body: &[Node]) {
        let n = self.top_nesting();
        self.add_cognitive(1 + n);
        if add_cyclomatic {
            self.add_cyclomatic();
        }
        self.enter_nesting();
        self.walk(body);
        self.leave_nesting();
    }

    fn visit(&mut self, node: &Node) {
        match node {
            Node::Function {
                name,
                kind,
                line,
                body,
            } => self.visit_function(name, kind, *line, body),
            Node::Branch {
                test,
                then,
                alternate,
            } => self.visit_branch(test, then, alternate),
            Node::Conditional {
                test,
                then,
                alternate,
            } => {
                let n = self.top_nesting();
                self.add_cognitive(1 + n);
                self.add_cyclomatic();
                self.walk(test);
                self.enter_nesting();
                self.walk(then);
                self.walk(alternate);
                self.leave_nesting();
            }
            Node::Loop { body } => self.nested(true, body),
            Node::Catch { body } => self.nested(true, body),
            Node::Switch { cases } => {
                let n = self.top_nesting();
                self.add_cognitive(1 + n);
                self.enter_nesting();
                for case in cases {
                    if !case.is_default {
                        self.add_cyclomatic(); // a non-default `case` is a decision point
                    }
                    self.walk(&case.body);
                }
                self.leave_nesting();
            }
            Node::Jump { labeled } => {
                if *labeled {
                    self.add_cognitive(1);
                }
            }
            Node::Logical { operands, .. } => self.visit_logical(operands),
            Node::NullGuard { body } => {
                self.add_cyclomatic();
                self.walk(body);
            }
            Node::Call { callee } => self.visit_call(callee.as_deref()),
            Node::Group(children) => self.walk(children),
        }
    }

    fn visit_function(&mut self, name: &str, kind: &str, line: u32, body: &[Node]) {
        self.stack.push(Frame {
            name: name.to_string(),
            kind: kind.to_string(),
            line,
            cognitive: 0,
            cyclomatic: 1, // McCabe base
            nesting: 0,
            children: Vec::new(),
        });
        self.walk(body);
        let frame = self.stack.pop().expect("function frame");
        let report = FunctionReport {
            name: frame.name,
            kind: frame.kind,
            line: frame.line,
            cognitive: frame.cognitive,
            cyclomatic: frame.cyclomatic,
            children: frame.children,
        };
        self.top().children.push(report);
    }

    /// `if` consequent gets a nesting bonus; the alternate (`else` / `else if`)
    /// is scored flat — one cognitive point, no bonus.
    fn visit_branch(&mut self, test: &[Node], then: &[Node], alternate: &Option<Box<Node>>) {
        let n = self.top_nesting();
        self.add_cognitive(1 + n);
        self.add_cyclomatic();
        self.walk(test);
        self.enter_nesting();
        self.walk(then);
        self.leave_nesting();
        self.visit_alternate(alternate);
    }

    fn visit_alternate(&mut self, alternate: &Option<Box<Node>>) {
        let Some(node) = alternate else { return };
        match node.as_ref() {
            // `else if`: its own decision (cyclomatic +1), flat cognitive +1.
            Node::Branch {
                test,
                then,
                alternate,
            } => {
                self.add_cognitive(1);
                self.add_cyclomatic();
                self.walk(test);
                self.enter_nesting();
                self.walk(then);
                self.leave_nesting();
                self.visit_alternate(alternate);
            }
            // plain `else`: flat cognitive +1, body nested.
            other => {
                self.add_cognitive(1);
                self.enter_nesting();
                self.visit(other);
                self.leave_nesting();
            }
        }
    }

    /// One cognitive point for the sequence; one cyclomatic point per operator
    /// (i.e. per extra operand). Operands are walked for nested constructs.
    fn visit_logical(&mut self, operands: &[Node]) {
        self.add_cognitive(1);
        for _ in 1..operands.len() {
            self.add_cyclomatic();
        }
        self.walk(operands);
    }

    fn visit_call(&mut self, callee: Option<&str>) {
        if let Some(name) = callee
            && let Some(top) = self.stack.last()
            && top.kind != "module"
            && top.name == name
        {
            self.add_cognitive(1); // recursion
        }
    }
}

/// Sum every function (all depths) into the running totals.
fn sum_tree(fns: &[FunctionReport], cog: &mut u32, cyc: &mut u32) {
    for f in fns {
        *cog += f.cognitive;
        *cyc += f.cyclomatic;
        sum_tree(&f.children, cog, cyc);
    }
}

/// Score a module's worth of IR (`nodes` is module-level code) into a
/// [`FileReport`] labelled `path`. `parse_errors` is carried through verbatim
/// for the adapter's convenience.
pub fn analyze(path: &str, nodes: &[Node], parse_errors: Vec<String>) -> FileReport {
    let mut engine = Engine::new();
    engine.walk(nodes);
    let module = engine.stack.pop().expect("module frame");

    let functions = module.children;
    let mut cognitive = module.cognitive;
    let mut cyclomatic = module.cyclomatic;
    sum_tree(&functions, &mut cognitive, &mut cyclomatic);

    FileReport {
        path: path.to_string(),
        cognitive,
        cyclomatic,
        functions,
        parse_errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{LogicalOp, Node, SwitchCase};

    fn func(name: &str, kind: &str, body: Vec<Node>) -> Node {
        Node::Function {
            name: name.into(),
            kind: kind.into(),
            line: 1,
            body,
        }
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
    fn cog(report: &FileReport, name: &str) -> u32 {
        find(&report.functions, name).expect("function").cognitive
    }
    fn cyc(report: &FileReport, name: &str) -> u32 {
        find(&report.functions, name).expect("function").cyclomatic
    }

    // sumOfPrimes: OUT: for { for { if { continue OUT } } } -> cognitive 7.
    #[test]
    fn sonar_sum_of_primes_is_7() {
        let inner_if = Node::Branch {
            test: vec![],
            then: vec![Node::Jump { labeled: true }],
            alternate: None,
        };
        let inner_for = Node::Loop {
            body: vec![inner_if],
        };
        let outer_for = Node::Loop {
            body: vec![inner_for],
        };
        let f = func("sumOfPrimes", "function", vec![outer_for]);
        let r = analyze("t", &[f], vec![]);
        // for(+1) + nested for(+2) + nested if(+3) + continue OUT(+1) = 7
        assert_eq!(cog(&r, "sumOfPrimes"), 7);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let sw = Node::Switch {
            cases: vec![
                SwitchCase {
                    is_default: false,
                    body: vec![],
                },
                SwitchCase {
                    is_default: false,
                    body: vec![],
                },
                SwitchCase {
                    is_default: true,
                    body: vec![],
                },
            ],
        };
        let r = analyze("t", &[func("getWords", "function", vec![sw])], vec![]);
        assert_eq!(cog(&r, "getWords"), 1);
        // base 1 + 2 non-default cases = 3
        assert_eq!(cyc(&r, "getWords"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let deep = Node::Branch {
            test: vec![],
            then: vec![Node::Branch {
                test: vec![],
                then: vec![Node::Branch {
                    test: vec![],
                    then: vec![],
                    alternate: None,
                }],
                alternate: None,
            }],
            alternate: None,
        };
        let r = analyze("t", &[func("f", "function", vec![deep])], vec![]);
        assert_eq!(cog(&r, "f"), 6); // +1 +2 +3
    }

    #[test]
    fn else_if_else_are_flat() {
        // if {} else if {} else {}
        let chain = Node::Branch {
            test: vec![],
            then: vec![],
            alternate: Some(Box::new(Node::Branch {
                test: vec![],
                then: vec![],
                alternate: Some(Box::new(Node::Group(vec![]))),
            })),
        };
        let r = analyze("t", &[func("f", "function", vec![chain])], vec![]);
        assert_eq!(cog(&r, "f"), 3); // if +1, else-if +1, else +1
    }

    #[test]
    fn logical_sequences() {
        // if (a && b && c || d): if(+1) + && seq(+1) + || seq(+1) = 3
        let inner_and = Node::Logical {
            op: LogicalOp::And,
            operands: vec![
                Node::Group(vec![]),
                Node::Group(vec![]),
                Node::Group(vec![]),
            ],
        };
        let outer_or = Node::Logical {
            op: LogicalOp::Or,
            operands: vec![inner_and, Node::Group(vec![])],
        };
        let branch = Node::Branch {
            test: vec![outer_or],
            then: vec![],
            alternate: None,
        };
        let r = analyze("t", &[func("f", "function", vec![branch])], vec![]);
        assert_eq!(cog(&r, "f"), 3);
        // cyc: base1 + if1 + (&& has 3 operands => +2) + (|| has 2 => +1) = 5
        assert_eq!(cyc(&r, "f"), 5);
    }

    #[test]
    fn null_guard_adds_only_a_cyclomatic_path() {
        let null_guard = Node::NullGuard {
            body: vec![Node::Call { callee: None }],
        };
        let r = analyze("t", &[func("f", "function", vec![null_guard])], vec![]);
        assert_eq!(cog(&r, "f"), 0);
        assert_eq!(cyc(&r, "f"), 2); // base 1 + optional path 1
    }

    #[test]
    fn recursion_adds_one() {
        // fib: if(+1) + call fib (+1 recursion)
        let body = vec![
            Node::Branch {
                test: vec![],
                then: vec![],
                alternate: None,
            },
            Node::Call {
                callee: Some("fib".into()),
            },
        ];
        let r = analyze("t", &[func("fib", "function", body)], vec![]);
        assert_eq!(cog(&r, "fib"), 2);
    }

    #[test]
    fn nested_function_is_independent_unit() {
        let inner = func(
            "inner",
            "function",
            vec![Node::Branch {
                test: vec![],
                then: vec![],
                alternate: None,
            }],
        );
        let outer = func("outer", "function", vec![inner]);
        let r = analyze("t", &[outer], vec![]);
        assert_eq!(cog(&r, "outer"), 0); // child excluded from parent's own score
        assert_eq!(cog(&r, "inner"), 1);
    }

    #[test]
    fn file_total_sums_all_functions() {
        let a = func(
            "a",
            "function",
            vec![Node::Branch {
                test: vec![],
                then: vec![],
                alternate: None,
            }],
        );
        let b = func(
            "b",
            "function",
            vec![Node::Branch {
                test: vec![],
                then: vec![],
                alternate: None,
            }],
        );
        let r = analyze("t", &[a, b], vec![]);
        assert_eq!(r.cognitive, 2);
    }
}
