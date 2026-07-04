//! Scheme (R7RS-small) + Racket adapter — a thin **façade** over
//! [`cccc_lisp_core::scheme`], where the lowering actually lives.
//!
//! The whole Lisp family shares one lowering core: the collector stack, the
//! [`lispexp::walk_regions`](https://docs.rs/lispexp)-driven code-vs-data
//! traversal, logical folding, and every dialect's special-form table live in
//! [`cccc_lisp_core`]. This crate exists only to give Scheme its own published
//! name and to pull in *just* the Scheme dialect (it depends on
//! `cccc-lisp-core` with `default-features = false, features = ["scheme"]`).
//!
//! Reads `.scm`/`.ss`/`.sld` and Racket's `.rkt`/`.rktl`/`.rktd` via the
//! tolerant `Options::scheme_superset` reader; see [`cccc_lisp_core::scheme`]
//! for the full Scheme-to-IR mapping.

pub use cccc_lisp_core::FileReport;
pub use cccc_lisp_core::scheme::{DEFAULT_EXTS, analyze_source, to_ir};
