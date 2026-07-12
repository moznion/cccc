//! The language-agnostic intermediate representation (IR) that the complexity
//! engine scores.
//!
//! A language adapter (e.g. `cccc-typescript`) lowers its native AST into a
//! `Vec<Node>` describing only the constructs that affect Cognitive / Cyclomatic
//! Complexity — branches, loops, switches, exception handlers, logical-operator
//! sequences, function boundaries, and calls. Everything else collapses into
//! [`Node::Group`], which is a transparent container the engine simply recurses
//! into. All scoring rules live in [`crate::engine`]; the IR carries no scores.
//!
//! The top level handed to the engine is itself a `&[Node]`, representing
//! module-level code; the engine scores it under an implicit module frame.

/// A logical operator, normalized across languages.
///
/// The adapter is responsible for folding a run of like operators into a single
/// [`Node::Logical`] (e.g. `a && b && c` is one `Logical` with three operands);
/// the engine counts one cognitive point per `Logical` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
    /// Nullish coalescing (`??`).
    Coalesce,
}

/// One arm of a [`Node::Switch`].
#[derive(Debug, Clone)]
pub struct SwitchCase {
    /// `true` for the `default` arm, which is not a cyclomatic decision point.
    pub is_default: bool,
    pub body: Vec<Node>,
}

/// A node of the normalized complexity IR.
///
/// Fields that hold sub-expressions or sub-statements are `Vec<Node>` so the
/// adapter can drop irrelevant detail and the engine can recurse uniformly.
#[derive(Debug, Clone)]
pub enum Node {
    /// A function-like unit (function, method, arrow, accessor, …). The engine
    /// scores each one independently — nesting resets to 0 at this boundary —
    /// and reports it as a child of the enclosing unit. `kind`/`name` are
    /// opaque, adapter-chosen labels (the engine never interprets them).
    Function {
        name: String,
        kind: String,
        /// 1-based line where the unit starts.
        line: u32,
        body: Vec<Node>,
    },

    /// An `if` (with optional `else` / `else if`). `then` is scored with a
    /// nesting bonus; `alternate` (a nested `Branch` for `else if`, or any other
    /// node for a plain `else`) is scored flat — one cognitive point, no bonus.
    Branch {
        test: Vec<Node>,
        then: Vec<Node>,
        alternate: Option<Box<Node>>,
    },

    /// A ternary `?:`. Scored like a branch: +1 plus nesting bonus.
    Conditional {
        test: Vec<Node>,
        then: Vec<Node>,
        alternate: Vec<Node>,
    },

    /// Any loop (`for` / `for-in` / `for-of` / `while` / `do-while`), normalized
    /// to one shape. +1 plus nesting bonus, and a cyclomatic point.
    Loop { body: Vec<Node> },

    /// A `switch`. +1 plus nesting bonus (but the switch itself is not a McCabe
    /// decision point — each non-default `case` is, scored via [`SwitchCase`]).
    Switch { cases: Vec<SwitchCase> },

    /// A `catch` clause. +1 plus nesting bonus, and a cyclomatic point.
    Catch { body: Vec<Node> },

    /// A `break` / `continue`. Only a *labelled* jump adds one cognitive point.
    Jump { labeled: bool },

    /// A run of like logical operators. One cognitive point per node; the
    /// adapter folds `a && b && c` into a single node with three operands.
    Logical { op: LogicalOp, operands: Vec<Node> },

    /// An implicit null-dependent control-flow split.
    ///
    /// The guarded continuation or collection contribution proceeds only when
    /// the source-language value is present (non-null, non-nil, or non-nullish).
    /// Each explicit source-level guard adds one cyclomatic path without adding
    /// cognitive complexity or nesting. Null coalescing remains a
    /// [`Node::Logical`] with [`LogicalOp::Coalesce`].
    NullGuard { body: Vec<Node> },

    /// A function call. If `callee` names the nearest enclosing function, the
    /// engine counts it as recursion (one cognitive point).
    Call { callee: Option<String> },

    /// A transparent container for any construct that holds children but carries
    /// no score of its own (statements, expressions, blocks). The engine simply
    /// recurses into it.
    Group(Vec<Node>),
}
