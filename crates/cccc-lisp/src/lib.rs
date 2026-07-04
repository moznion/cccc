//! Lisp-family adapter: lowers **Common Lisp**, **Emacs Lisp**, and (via
//! delegation) **Scheme** / **Clojure** source into the language-agnostic
//! [`cccc_core::ir`](https://docs.rs/cccc-core), reusing the shared
//! [`cccc_lisp_kit`] lowering kit.
//!
//! # Structure
//!
//! Every Lisp adapter shares the same 80% — a collector stack, a
//! [`lispexp::walk_regions`]-driven code-vs-data traversal, and like-operator
//! logical folding — which lives once in [`cccc_lisp_kit`]. A *dialect* is then
//! just a [reader preset](lispexp::Options) plus a head-symbol dispatch table.
//!
//! This crate bundles the dialect tables for the "traditional and derived"
//! Lisps whose names don't warrant a standalone crate — currently
//! [`commonlisp`] and [`emacslisp`]. **Scheme** and **Clojure** each have their
//! own standalone crate ([`cccc_scheme`] / [`cccc_clojure`]) because they carry
//! distinct identities and demand; this crate does not duplicate their lowering
//! but **delegates** to them through the unified [`Dialect`] entry point, so an
//! embedder using `cccc-lisp` can analyze any of these dialects from one API
//! without the code living in two places.
//!
//! ```
//! use cccc_lisp::{Dialect, analyze_as};
//! use std::path::Path;
//!
//! let report = analyze_as(Dialect::CommonLisp, Path::new("x.lisp"), "(defun f (x) (if x 1 2))");
//! assert_eq!(report.functions[0].cognitive, 1);
//! ```

pub mod commonlisp;
pub mod emacslisp;

use std::path::Path;

pub use cccc_lisp_kit::FileReport;

/// A Lisp dialect this crate can analyze. Common Lisp and Emacs Lisp are lowered
/// here; Scheme and Clojure are delegated to their standalone crates (their
/// lowering is not duplicated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Dialect {
    /// ANSI Common Lisp (`.lisp`, `.lsp`, `.cl`).
    CommonLisp,
    /// Emacs Lisp (`.el`).
    EmacsLisp,
    /// Scheme (R7RS-small, tolerant superset) — delegated to `cccc-scheme`.
    Scheme,
    /// Clojure — delegated to `cccc-clojure`.
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
            "scm" | "ss" | "sld" => Some(Dialect::Scheme),
            "clj" | "cljs" | "cljc" => Some(Dialect::Clojure),
            _ => None,
        }
    }
}

/// Analyze `source` (labelled `path`) as the given [`Dialect`], returning its
/// scored [`FileReport`]. Common Lisp / Emacs Lisp are lowered here; Scheme /
/// Clojure delegate to their standalone crates.
pub fn analyze_as(dialect: Dialect, path: &Path, source: &str) -> FileReport {
    match dialect {
        Dialect::CommonLisp => commonlisp::analyze_source(path, source),
        Dialect::EmacsLisp => emacslisp::analyze_source(path, source),
        Dialect::Scheme => cccc_scheme::analyze_source(path, source),
        Dialect::Clojure => cccc_clojure::analyze_source(path, source),
    }
}
