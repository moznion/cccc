//! Keeps pathologically nested input from overflowing the stack.
//!
//! Every adapter's parser, lowering and the scoring [`engine`] recurse once per
//! nesting level, and a stack overflow aborts the whole process — one hostile
//! or generated file would take a multi-file run down with it. [`analyze`]
//! closes that off:
//!
//! * Source whose bracket nesting exceeds [`MAX_NESTING_DEPTH`] is refused
//!   before parsing and reported as a parse error. Brackets let input nest one
//!   level per byte, which no fixed stack can keep up with.
//! * The same limit caps how deeply the resulting [`FunctionReport`]s nest.
//!   Keyword-delimited functions (`lambda:`, `Sub()`, `def`) can nest past it
//!   without a single bracket, and the report outlives the large stack: the
//!   CLI serializes, caches and drops it on ordinary threads.
//! * Everything else runs with at least [`STACK_SIZE`] of stack, sized so the
//!   hungriest adapter (Rust, ~8 KiB per level) clears that depth with a wide
//!   margin, and long `else if` / operator chains — which nest the AST without
//!   nesting brackets — get the same headroom. A thread with less left gets a
//!   fresh stack for the call; one spawned with enough (as the CLI's are) runs
//!   it in place, sparing a map/unmap per file. Either way it is reserved, not
//!   committed: only the pages a file actually touches are backed by memory.

use std::path::Path;

use crate::engine;
use crate::ir::Node;
use crate::report::{FileReport, FunctionReport};

/// Deepest `()`/`[]`/`{}` nesting [`analyze`] accepts. Hand-written code stays
/// far below this; only generated or adversarial input gets near it.
pub const MAX_NESTING_DEPTH: usize = 1000;

/// Stack [`analyze`] guarantees the adapter. Threads that call it for many
/// files should be spawned with this much plus their own needs, so it never
/// has to switch stacks.
pub const STACK_SIZE: usize = 256 << 20;

/// Lower `source` with `to_ir` and score it, as every adapter's
/// `analyze_source` does — but refusing over-deep input and running on a stack
/// big enough for anything that passes. Callers that use `to_ir` directly get
/// neither protection.
pub fn analyze(
    path: &Path,
    source: &str,
    to_ir: impl FnOnce(&Path, &str) -> (Vec<Node>, Vec<String>),
) -> FileReport {
    let label = path.display().to_string();
    if let Some(line) = too_deep_at(source) {
        return refused(&label, &format!("line {line}: "));
    }
    stacker::maybe_grow(STACK_SIZE, STACK_SIZE, || {
        let (nodes, parse_errors) = to_ir(path, source);
        let report = engine::analyze(&label, &nodes, parse_errors);
        if function_depth(&report.functions) > MAX_NESTING_DEPTH {
            // Dropped here, on the large stack, like the IR above.
            return refused(&label, "functions ");
        }
        report
    })
}

/// An empty report whose only content is the parse error explaining why the
/// file was skipped; `site` says where the nesting got too deep.
fn refused(label: &str, site: &str) -> FileReport {
    let msg = format!("{site}nested deeper than {MAX_NESTING_DEPTH} levels; file skipped");
    engine::analyze(label, &[], vec![msg])
}

/// How many levels deep `functions` nest (0 for none), walked with an explicit
/// stack so the depth check cannot itself overflow.
fn function_depth(functions: &[FunctionReport]) -> usize {
    let mut max = 0;
    let mut pending: Vec<(&FunctionReport, usize)> = functions.iter().map(|f| (f, 1)).collect();
    while let Some((f, depth)) = pending.pop() {
        max = max.max(depth);
        pending.extend(f.children.iter().map(|c| (c, depth + 1)));
    }
    max
}

/// The 1-based line where bracket nesting first exceeds [`MAX_NESTING_DEPTH`],
/// if it does. The count is lexically blind — brackets inside strings and
/// comments count too — which can only over-estimate drift-free code by the
/// unbalanced brackets its literals carry, far short of the limit in practice.
/// A stray closer saturates at zero rather than going negative.
fn too_deep_at(source: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut line = 1;
    for b in source.bytes() {
        match b {
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Some(line);
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'\n' => line += 1,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(depth: usize) -> String {
        "(".repeat(depth) + &")".repeat(depth)
    }

    #[test]
    fn depth_at_the_limit_is_accepted() {
        assert_eq!(too_deep_at(&nested(MAX_NESTING_DEPTH)), None);
    }

    #[test]
    fn depth_past_the_limit_is_reported_with_its_line() {
        let src = format!("a\n\n{}", nested(MAX_NESTING_DEPTH + 1));
        assert_eq!(too_deep_at(&src), Some(3));
    }

    #[test]
    fn mixed_bracket_kinds_share_one_depth() {
        let src = "([{".repeat(MAX_NESTING_DEPTH / 3 + 1);
        assert!(too_deep_at(&src).is_some());
    }

    #[test]
    fn stray_closers_do_not_bank_depth() {
        let src = ")".repeat(MAX_NESTING_DEPTH) + &nested(MAX_NESTING_DEPTH);
        assert_eq!(too_deep_at(&src), None);
    }

    #[test]
    fn too_deep_input_never_reaches_the_adapter() {
        let report = analyze(
            Path::new("deep.x"),
            &nested(MAX_NESTING_DEPTH + 1),
            |_, _| unreachable!("over-deep input must be refused before lowering"),
        );
        assert_eq!(report.path, "deep.x");
        assert!(report.functions.is_empty());
        assert_eq!(report.parse_errors.len(), 1);
        assert!(report.parse_errors[0].starts_with("line 1: nested deeper than"));
    }

    fn chain(depth: usize) -> FunctionReport {
        let mut f = FunctionReport {
            name: "f".into(),
            kind: "function".into(),
            line: 1,
            cognitive: 0,
            cyclomatic: 1,
            children: Vec::new(),
        };
        for _ in 1..depth {
            f = FunctionReport {
                children: vec![f.clone()],
                ..f
            };
        }
        f
    }

    #[test]
    fn function_depth_counts_the_deepest_branch() {
        assert_eq!(function_depth(&[]), 0);
        assert_eq!(function_depth(&[chain(1), chain(3), chain(2)]), 3);
    }

    #[test]
    fn functions_nested_past_the_limit_are_refused() {
        // Keyword-delimited nesting: no brackets for the source check to see.
        let nest = |depth: usize| {
            move |_: &Path, _: &str| {
                let mut node = Node::Function {
                    name: "f".into(),
                    kind: "function".into(),
                    line: 1,
                    body: Vec::new(),
                };
                for _ in 1..depth {
                    node = Node::Function {
                        name: "f".into(),
                        kind: "function".into(),
                        line: 1,
                        body: vec![node],
                    };
                }
                (vec![node], Vec::new())
            }
        };
        let ok = analyze(Path::new("ok.x"), "", nest(MAX_NESTING_DEPTH));
        assert!(ok.parse_errors.is_empty());
        assert_eq!(function_depth(&ok.functions), MAX_NESTING_DEPTH);

        let deep = analyze(Path::new("deep.x"), "", nest(MAX_NESTING_DEPTH + 1));
        assert!(deep.functions.is_empty());
        assert_eq!(deep.parse_errors.len(), 1);
        assert!(deep.parse_errors[0].starts_with("functions nested deeper than"));
    }

    #[test]
    fn adapter_runs_on_the_large_stack() {
        // Recursion far past what a default 2 MiB thread stack holds.
        fn burn(n: usize) -> usize {
            let pad = std::hint::black_box([0u8; 1024]);
            if n == 0 {
                pad[0] as usize
            } else {
                burn(n - 1) + pad[n % 1024] as usize
            }
        }
        let report = analyze(Path::new("ok.x"), "", |_, _| {
            std::hint::black_box(burn(64 * 1024));
            (Vec::new(), Vec::new())
        });
        assert!(report.parse_errors.is_empty());
    }
}
