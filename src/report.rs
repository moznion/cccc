//! Output data structures and renderers (JSON by default, optional table).

use std::io::Write;

use serde::Serialize;

/// Complexity metrics for a single function-like unit (function, method, arrow, accessor).
///
/// Each unit is measured independently: nesting resets to 0 at the function
/// boundary and nested functions are reported as `children` rather than being
/// folded into the parent's own score. See `analyzer` for the exact rules.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionReport {
    pub name: String,
    /// "function" | "method" | "arrow" | "getter" | "setter" | "constructor"
    pub kind: String,
    /// 1-based line where the function starts.
    pub line: u32,
    pub cognitive: u32,
    pub cyclomatic: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<FunctionReport>,
}

/// Aggregated metrics for a single source file.
#[derive(Debug, Clone, Serialize)]
pub struct FileReport {
    pub path: String,
    /// File total = module-level code + every function (all nesting depths).
    pub cognitive: u32,
    pub cyclomatic: u32,
    pub functions: Vec<FunctionReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parse_errors: Vec<String>,
}

/// Distribution of one metric over the population of all functions.
///
/// Complexity is right-skewed, so the percentiles (not a mean/stddev) carry the
/// signal: `median` is the typical function, `p90`/`p95`/`max` describe the tail
/// where refactoring candidates live.
#[derive(Debug, Clone, Serialize)]
pub struct MetricSummary {
    pub sum: u32,
    pub max: u32,
    pub median: u32,
    pub p90: u32,
    pub p95: u32,
}

/// Project-wide rollup across every function in every file.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub file_count: usize,
    pub function_count: usize,
    pub cognitive: MetricSummary,
    pub cyclomatic: MetricSummary,
}

/// Top-level output: per-file reports plus a whole-project summary.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub files: Vec<FileReport>,
    pub summary: Summary,
}

/// The complexity metric a ranking is ordered by.
#[derive(Debug, Clone, Copy)]
pub enum Metric {
    Cognitive,
    Cyclomatic,
}

impl Metric {
    fn as_str(self) -> &'static str {
        match self {
            Metric::Cognitive => "cognitive",
            Metric::Cyclomatic => "cyclomatic",
        }
    }
}

/// One function in a flat cross-file ranking. Carries `path`/`line` so each row
/// is locatable on its own (the per-file nesting is flattened away).
#[derive(Debug, Clone, Serialize)]
pub struct TopEntry {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub cognitive: u32,
    pub cyclomatic: u32,
}

/// Top-level output for `--top-*`: a flat ranking plus the whole-project summary.
#[derive(Debug, Clone, Serialize)]
pub struct TopReport {
    /// The metric the ranking is sorted by ("cognitive" | "cyclomatic").
    pub metric: String,
    pub top: Vec<TopEntry>,
    pub summary: Summary,
}

/// Visit every function in a report tree (parents before children, all depths).
fn for_each_function(fns: &[FunctionReport], f: &mut impl FnMut(&FunctionReport)) {
    for func in fns {
        f(func);
        for_each_function(&func.children, f);
    }
}

/// Build a flat ranking of the `n` most complex functions across all files,
/// ordered by `metric` descending. Ties break by path then line for stable,
/// reproducible output. Counts every function at every nesting depth.
pub fn compute_top(reports: &[FileReport], metric: Metric, n: usize) -> Vec<TopEntry> {
    let mut entries = Vec::new();
    for r in reports {
        for_each_function(&r.functions, &mut |f| {
            entries.push(TopEntry {
                path: r.path.clone(),
                name: f.name.clone(),
                kind: f.kind.clone(),
                line: f.line,
                cognitive: f.cognitive,
                cyclomatic: f.cyclomatic,
            });
        });
    }
    entries.sort_by(|a, b| {
        let (av, bv) = match metric {
            Metric::Cognitive => (a.cognitive, b.cognitive),
            Metric::Cyclomatic => (a.cyclomatic, b.cyclomatic),
        };
        bv.cmp(&av)
            .then_with(|| a.path.cmp(&b.path))
            .then(a.line.cmp(&b.line))
    });
    entries.truncate(n);
    entries
}

/// Assemble a `TopReport` from the per-file reports and a precomputed summary.
pub fn build_top_report(reports: &[FileReport], summary: Summary, metric: Metric, n: usize) -> TopReport {
    TopReport {
        metric: metric.as_str().to_string(),
        top: compute_top(reports, metric, n),
        summary,
    }
}

/// Nearest-rank percentile on an ascending-sorted slice. `p` is in `[0, 100]`.
/// Returns 0 for an empty slice.
fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

fn metric_summary(mut values: Vec<u32>) -> MetricSummary {
    values.sort_unstable();
    MetricSummary {
        sum: values.iter().sum(),
        max: values.last().copied().unwrap_or(0),
        median: percentile(&values, 50.0),
        p90: percentile(&values, 90.0),
        p95: percentile(&values, 95.0),
    }
}

/// Build the whole-project summary. The population is every function at every
/// nesting depth across all files (module-level totals are excluded). Call this
/// before any display-only filtering so the distribution reflects all code.
pub fn compute_summary(reports: &[FileReport]) -> Summary {
    let mut cog = Vec::new();
    let mut cyc = Vec::new();
    for r in reports {
        for_each_function(&r.functions, &mut |f| {
            cog.push(f.cognitive);
            cyc.push(f.cyclomatic);
        });
    }
    Summary {
        file_count: reports.len(),
        function_count: cog.len(),
        cognitive: metric_summary(cog),
        cyclomatic: metric_summary(cyc),
    }
}

/// Print any serializable value as pretty JSON to stdout.
///
/// Serializes straight into a buffered, locked stdout writer rather than
/// building one big `String` first — for large reports (zod's corpus is ~1 MB
/// of JSON) that avoids materializing the whole document in memory.
pub fn print_json<T: Serialize>(value: &T) {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let result = serde_json::to_writer_pretty(&mut out, value)
        .map_err(std::io::Error::from)
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    if let Err(e) = result {
        eprintln!("cccc: failed to write JSON: {e}");
    }
}

/// Print a human-readable table to stdout. Within each level functions are
/// sorted by cognitive complexity (desc); nested functions are indented. A
/// project summary is printed last.
///
/// All rows go through one buffered, locked stdout writer; `writeln!` to a
/// `BufWriter` never fails for stdout in practice, so write errors (e.g. a
/// closed pipe) are reported once at the end rather than per line.
pub fn print_table(report: &Report) {
    with_stdout(|out| {
        for file in &report.files {
            writeln!(out, "{}", file.path)?;
            writeln!(out, "  {:>9}  {:>10}  Function", "Cognitive", "Cyclomatic")?;
            if file.functions.is_empty() {
                writeln!(out, "  (no functions)")?;
            }
            for f in sorted_desc(&file.functions) {
                write_fn(out, f, 1)?;
            }
            writeln!(out, "  {}", "-".repeat(48))?;
            writeln!(
                out,
                "  file total: cognitive={} cyclomatic={}",
                file.cognitive, file.cyclomatic
            )?;
            for e in &file.parse_errors {
                writeln!(out, "  parse warning: {e}")?;
            }
            writeln!(out)?;
        }
        write_summary(out, &report.summary)
    });
}

/// Print a flat ranking as a human-readable table, followed by the summary.
pub fn print_top_table(report: &TopReport) {
    with_stdout(|out| {
        writeln!(out, "top {} by {}", report.top.len(), report.metric)?;
        writeln!(out, "  {:>9}  {:>10}  Function", "Cognitive", "Cyclomatic")?;
        if report.top.is_empty() {
            writeln!(out, "  (no functions)")?;
        }
        for e in &report.top {
            writeln!(
                out,
                "  {:>9}  {:>10}  {} [{}] {}:{}",
                e.cognitive, e.cyclomatic, e.name, e.kind, e.path, e.line
            )?;
        }
        writeln!(out, "  {}", "-".repeat(48))?;
        write_summary(out, &report.summary)
    });
}

/// Run `body` against a buffered, locked stdout writer and report any I/O error
/// once. Centralizes the locking/flushing all table printers share.
fn with_stdout<F>(body: F)
where
    F: FnOnce(&mut dyn Write) -> std::io::Result<()>,
{
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    if let Err(e) = body(&mut out).and_then(|()| out.flush()) {
        eprintln!("cccc: failed to write output: {e}");
    }
}

fn write_summary(out: &mut dyn Write, s: &Summary) -> std::io::Result<()> {
    writeln!(out, "summary ({} files, {} functions)", s.file_count, s.function_count)?;
    writeln!(out, "  {:<11} {:>5} {:>5} {:>7} {:>5} {:>5}", "", "sum", "max", "median", "p90", "p95")?;
    write_metric_row(out, "cognitive", &s.cognitive)?;
    write_metric_row(out, "cyclomatic", &s.cyclomatic)
}

fn write_metric_row(out: &mut dyn Write, label: &str, m: &MetricSummary) -> std::io::Result<()> {
    writeln!(
        out,
        "  {label:<11} {:>5} {:>5} {:>7} {:>5} {:>5}",
        m.sum, m.max, m.median, m.p90, m.p95
    )
}

/// Borrow each function in display order (cognitive desc, then line asc) without
/// cloning the tree.
fn sorted_desc(fns: &[FunctionReport]) -> Vec<&FunctionReport> {
    let mut refs: Vec<&FunctionReport> = fns.iter().collect();
    refs.sort_by(|a, b| b.cognitive.cmp(&a.cognitive).then(a.line.cmp(&b.line)));
    refs
}

fn write_fn(out: &mut dyn Write, f: &FunctionReport, depth: usize) -> std::io::Result<()> {
    let indent = "  ".repeat(depth);
    writeln!(
        out,
        "  {:>9}  {:>10}  {indent}{} [{}] (L{})",
        f.cognitive, f.cyclomatic, f.name, f.kind, f.line
    )?;
    for c in sorted_desc(&f.children) {
        write_fn(out, c, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn func(cognitive: u32, cyclomatic: u32, children: Vec<FunctionReport>) -> FunctionReport {
        FunctionReport {
            name: "f".into(),
            kind: "function".into(),
            line: 1,
            cognitive,
            cyclomatic,
            children,
        }
    }

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<u32> = (1..=10).collect(); // 1..10
        assert_eq!(percentile(&v, 50.0), 5);
        assert_eq!(percentile(&v, 90.0), 9);
        assert_eq!(percentile(&v, 95.0), 10);
        assert_eq!(percentile(&v, 100.0), 10);
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 50.0), 0);
    }

    #[test]
    fn summary_counts_all_depths() {
        let reports = vec![FileReport {
            path: "a.ts".into(),
            cognitive: 0,
            cyclomatic: 0,
            functions: vec![
                func(10, 5, vec![func(2, 2, vec![])]), // parent + 1 nested child
                func(0, 1, vec![]),
            ],
            parse_errors: vec![],
        }];
        let s = compute_summary(&reports);
        assert_eq!(s.file_count, 1);
        assert_eq!(s.function_count, 3); // 2 top-level + 1 nested
        assert_eq!(s.cognitive.sum, 12);
        assert_eq!(s.cognitive.max, 10);
        assert_eq!(s.cyclomatic.sum, 8);
        assert_eq!(s.cyclomatic.max, 5);
    }

    #[test]
    fn summary_empty_is_zeroed() {
        let s = compute_summary(&[]);
        assert_eq!(s.function_count, 0);
        assert_eq!(s.cognitive.sum, 0);
        assert_eq!(s.cognitive.max, 0);
        assert_eq!(s.cognitive.median, 0);
    }
}
