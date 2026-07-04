//! Lisp-family adapter — a **façade** over [`cccc_lisp_core`] that exposes the
//! "traditional and derived" Lisps whose names don't warrant a standalone
//! crate ([`commonlisp`] and [`emacslisp`]), plus a unified [`Dialect`] entry
//! point that can analyze **any** bundled dialect, Scheme and Clojure included.
//!
//! # Structure
//!
//! Every Lisp dialect shares the same 80% — a collector stack, a
//! [`lispexp::walk_regions`](https://docs.rs/lispexp)-driven code-vs-data
//! traversal, and like-operator
//! logical folding — and, since the reader can be shared, each dialect's
//! special-form table is just data. All of it lives once in [`cccc_lisp_core`].
//!
//! This crate re-exports the [`commonlisp`] and [`emacslisp`] modules and adds
//! [`Dialect`] / [`analyze_as`], which dispatch straight into `cccc-lisp-core`'s
//! dialect modules — Scheme and Clojure are analyzed here directly (their
//! published [`cccc-scheme`](https://docs.rs/cccc-scheme) /
//! [`cccc-clojure`](https://docs.rs/cccc-clojure) crates are façades over the
//! very same modules), so no lowering is duplicated.
//!
//! ```
//! use cccc_lisp::{Dialect, analyze_as};
//! use std::path::Path;
//!
//! let report = analyze_as(Dialect::CommonLisp, Path::new("x.lisp"), "(defun f (x) (if x 1 2))");
//! assert_eq!(report.functions[0].cognitive, 1);
//! ```

use std::path::Path;

pub use cccc_lisp_core::FileReport;
pub use cccc_lisp_core::{commonlisp, emacslisp};

/// A Lisp dialect this crate can analyze. Every variant is lowered by the
/// matching [`cccc_lisp_core`] module; the standalone `cccc-scheme` /
/// `cccc-clojure` crates are façades over those same modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Dialect {
    /// ANSI Common Lisp (`.lisp`, `.lsp`, `.cl`).
    CommonLisp,
    /// Emacs Lisp (`.el`).
    EmacsLisp,
    /// Scheme (R7RS-small, tolerant superset) + Racket.
    Scheme,
    /// Clojure.
    Clojure,
}

impl Dialect {
    /// The dialect a file extension maps to by default (`None` if unknown).
    /// Ambiguous "traditional Lisp" extensions (`.lisp`/`.lsp`/`.cl`) default to
    /// Common Lisp; a consumer that knows better selects the dialect explicitly.
    #[must_use]
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "lisp" | "lsp" | "cl" => Some(Dialect::CommonLisp),
            "el" => Some(Dialect::EmacsLisp),
            "scm" | "ss" | "sld" | "rkt" | "rktl" | "rktd" => Some(Dialect::Scheme),
            "clj" | "cljs" | "cljc" => Some(Dialect::Clojure),
            _ => None,
        }
    }
}

/// Analyze `source` (labelled `path`) as the given [`Dialect`], returning its
/// scored [`FileReport`]. Each dialect dispatches to its [`cccc_lisp_core`]
/// module; no lowering is duplicated.
pub fn analyze_as(dialect: Dialect, path: &Path, source: &str) -> FileReport {
    match dialect {
        Dialect::CommonLisp => cccc_lisp_core::commonlisp::analyze_source(path, source),
        Dialect::EmacsLisp => cccc_lisp_core::emacslisp::analyze_source(path, source),
        Dialect::Scheme => cccc_lisp_core::scheme::analyze_source(path, source),
        Dialect::Clojure => cccc_lisp_core::clojure::analyze_source(path, source),
    }
}
