//! Shared lowering kit for the cccc **Lisp-family** adapters.
//!
//! Every Lisp adapter — [`cccc-scheme`](https://docs.rs/cccc-scheme),
//! [`cccc-clojure`](https://docs.rs/cccc-clojure), and the dialects bundled in
//! [`cccc-lisp`](https://docs.rs/cccc-lisp) (Common Lisp, Emacs Lisp, …) —
//! shares the same shape: read the source with [`lispexp`] into a datum tree,
//! then walk it emitting [`cccc_core::ir`] nodes, using
//! [`lispexp::walk_regions`] (ADR-0026) to skip quoted *data* while descending
//! into the *code* carried by `unquote`. Only the **reader preset** and the
//! **special-form dispatch** differ per dialect.
//!
//! This crate factors out that shared 80%: the [`Builder`] (a stack of
//! "collector" vectors and the building-block emit/collect/lower methods), the
//! `walk_regions`-driven [`Builder::lower_datum`], the like-operator logical
//! folding ([`Builder::lower_logical`]), and the [`analyze`]/[`lower`] drivers.
//! A dialect supplies just two function pointers — a [`LowerListFn`] (its
//! head-symbol dispatch table) and a [`LogicalOpFn`] (which heads are
//! `and`/`or`) — plus its [`Options`] reader preset.
//!
//! ```ignore
//! pub fn analyze_source(path: &Path, source: &str) -> FileReport {
//!     cccc_lisp_kit::analyze(&Options::scheme(), lower_list, logical_op, path, source)
//! }
//! ```

use std::path::Path;

use cccc_core::engine;
pub use cccc_core::ir::{LogicalOp, Node, SwitchCase};
pub use cccc_core::report::{FileReport, FunctionReport};
pub use lispexp::{Datum, DatumKind, Delim, Options, Region, Walk, parse, walk_regions};

/// A dialect's special-form dispatch: given a code-position `()`/`[]`/`{}` list
/// (its `delim`, `items`, and dotted `tail`), emit the matching IR into `b`.
/// This is the one function each dialect writes in full; everything else is
/// provided by [`Builder`].
pub type LowerListFn = fn(&mut Builder, &Datum, Delim, &[Datum], Option<&Datum>);

/// The normalized logical operator named by a list head, if any — the dialect's
/// answer to "is this `and`/`or`?" (Common Lisp lower-cases first; case-
/// sensitive dialects match exactly).
pub type LogicalOpFn = fn(Option<&str>) -> Option<LogicalOp>;

/// Assembles the IR tree while recursing the datum tree, parameterized by the
/// active dialect's [`LowerListFn`]/[`LogicalOpFn`].
pub struct Builder {
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
    lower_list: LowerListFn,
    logical_op: LogicalOpFn,
}

impl Builder {
    fn new(lower_list: LowerListFn, logical_op: LogicalOpFn) -> Self {
        Self {
            stack: vec![Vec::new()], // module-level collector
            lower_list,
            logical_op,
        }
    }

    fn finish(mut self) -> Vec<Node> {
        self.stack.pop().expect("module collector")
    }

    /// Append a node to the current collector.
    pub fn emit(&mut self, node: Node) {
        self.stack.last_mut().expect("collector").push(node);
    }

    /// Run `f` against a fresh collector and return the nodes it gathered.
    pub fn collect<F: FnOnce(&mut Self)>(&mut self, f: F) -> Vec<Node> {
        self.stack.push(Vec::new());
        f(self);
        self.stack.pop().expect("collector")
    }

    /// Emit a `Function` whose body is whatever `walk` gathers in a sub-traversal.
    pub fn emit_function<F: FnOnce(&mut Self)>(
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

    /// Lower each datum in `items` at the current level.
    pub fn lower_seq(&mut self, items: &[Datum]) {
        for d in items {
            self.lower_datum(d);
        }
    }

    /// Lower an optional datum (a missing element contributes nothing).
    pub fn lower_opt(&mut self, d: Option<&Datum>) {
        if let Some(d) = d {
            self.lower_datum(d);
        }
    }

    /// Lower `d` if it sits in code position. Delegates the code-vs-data
    /// judgment to [`lispexp::walk_regions`] (ADR-0026): each `Region::Code`
    /// list is handed to the dialect's [`LowerListFn`] (returning `Walk::Skip`,
    /// since that function does its own targeted recursion), `Region::SealedData`
    /// is pruned, and everything else — including a `Region::PorousData`
    /// quasiquote template that may still carry a nested `unquote` — is
    /// descended into.
    pub fn lower_datum(&mut self, d: &Datum) {
        let lower_list = self.lower_list;
        walk_regions(std::slice::from_ref(d), |dd, region| {
            if region == Region::Code
                && let DatumKind::List {
                    delim, items, tail, ..
                } = &dd.kind
            {
                lower_list(self, dd, *delim, items, tail.as_deref());
                return Walk::Skip;
            }
            if region.is_prunable() {
                return Walk::Skip;
            }
            Walk::Descend
        });
    }

    /// Fold a run of like `and`/`or` operators into one [`Node::Logical`]. A
    /// 0- or 1-operand run is not a decision point: its contents are spliced in
    /// rather than emitting a degenerate `Logical` (which would also underflow
    /// the engine's `operands.len() - 1`).
    pub fn lower_logical(&mut self, op: LogicalOp, args: &[Datum]) {
        let mut operands = Vec::new();
        for a in args {
            self.collect_logical(op, a, &mut operands);
        }
        if operands.len() >= 2 {
            self.emit(Node::Logical { op, operands });
        } else {
            for n in operands {
                self.emit(n);
            }
        }
    }

    /// Flatten a run of like operators; a different operator nests as its own
    /// `Logical`; anything else becomes a `Group` of its sub-nodes. Only a
    /// `()` list can be an operator application (a `[]`/`{}` literal is data).
    fn collect_logical(&mut self, op: LogicalOp, arg: &Datum, operands: &mut Vec<Node>) {
        if let DatumKind::List {
            delim: Delim::Round,
            items,
            ..
        } = &arg.kind
            && let Some(arg_op) = (self.logical_op)(head_symbol(items))
        {
            if arg_op == op {
                for a in &items[1..] {
                    self.collect_logical(op, a, operands);
                }
            } else {
                let mut sub = Vec::new();
                for a in &items[1..] {
                    self.collect_logical(arg_op, a, &mut sub);
                }
                if sub.len() >= 2 {
                    operands.push(Node::Logical {
                        op: arg_op,
                        operands: sub,
                    });
                } else {
                    operands.extend(sub);
                }
            }
            return;
        }
        let nodes = self.collect(|b| b.lower_datum(arg));
        operands.push(Node::Group(nodes));
    }
}

/// Parse `source` with `options` and lower it to the module-level IR nodes plus
/// any reader diagnostics, dispatching each code list through `lower_list`.
/// `lispexp` is fault-tolerant: it always yields a (possibly partial) tree, so
/// whatever it recovered is lowered and the diagnostics surfaced alongside.
pub fn lower(
    options: &Options,
    lower_list: LowerListFn,
    logical_op: LogicalOpFn,
    source: &str,
) -> (Vec<Node>, Vec<String>) {
    let parsed = parse(source, options);
    let mut builder = Builder::new(lower_list, logical_op);
    builder.lower_seq(&parsed.data);
    let errors = parsed.errors.iter().map(ToString::to_string).collect();
    (builder.finish(), errors)
}

/// Parse and lower `source` (labelled `path`) into a scored [`FileReport`] via
/// the core engine — the convenience entry point each dialect's
/// `analyze_source` forwards to.
pub fn analyze(
    options: &Options,
    lower_list: LowerListFn,
    logical_op: LogicalOpFn,
    path: &Path,
    source: &str,
) -> FileReport {
    let (nodes, parse_errors) = lower(options, lower_list, logical_op, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// The symbol text of a datum, if it is a symbol.
pub fn as_symbol<'a>(d: &Datum<'a>) -> Option<&'a str> {
    match d.kind {
        DatumKind::Symbol(s) => Some(s),
        _ => None,
    }
}

/// The head (operator) symbol of a list's elements.
pub fn head_symbol<'a>(items: &[Datum<'a>]) -> Option<&'a str> {
    items.first().and_then(as_symbol)
}
