//! Language-agnostic Cognitive (SonarSource) and Cyclomatic (McCabe) complexity.
//!
//! This crate computes complexity over a normalized intermediate representation
//! ([`ir::Node`]) instead of any particular language's AST. A language adapter
//! (such as `cccc-typescript`) lowers its parse tree into `Vec<ir::Node>`, and
//! [`engine::analyze`] scores it. All scoring rules live here, so adding a
//! language means writing an adapter — not reimplementing the metrics.
//!
//! ```
//! use cccc_core::ir::Node;
//! use cccc_core::engine::analyze;
//!
//! // `if (cond) {}` inside `fn f` — one cognitive point, cyclomatic base 1 + 1.
//! let f = Node::Function {
//!     name: "f".into(),
//!     kind: "function".into(),
//!     line: 1,
//!     body: vec![Node::Branch { test: vec![], then: vec![], alternate: None }],
//! };
//! let report = analyze("example.ts", &[f], vec![]);
//! assert_eq!(report.functions[0].cognitive, 1);
//! assert_eq!(report.functions[0].cyclomatic, 2);
//! ```

pub mod engine;
pub mod guard;
pub mod ir;
pub mod report;
