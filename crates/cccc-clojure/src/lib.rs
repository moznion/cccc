//! Clojure adapter — a thin **façade** over [`cccc_lisp_core::clojure`], where
//! the lowering actually lives.
//!
//! The whole Lisp family shares one lowering core: the collector stack, the
//! [`lispexp::walk_regions`](https://docs.rs/lispexp)-driven code-vs-data
//! traversal, logical folding, and every dialect's special-form table live in
//! [`cccc_lisp_core`]. This crate exists only to give Clojure its own published
//! name and to pull in *just* the Clojure dialect (it depends on
//! `cccc-lisp-core` with `default-features = false, features = ["clojure"]`).
//!
//! Reads `.clj`/`.cljs`/`.cljc`; see [`cccc_lisp_core::clojure`] for the full
//! Clojure-to-IR mapping.

pub use cccc_lisp_core::FileReport;
pub use cccc_lisp_core::clojure::{DEFAULT_EXTS, analyze_source, to_ir};
