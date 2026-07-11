//! Result types and language-agnostic aggregation (summary statistics and
//! cross-file rankings). Output *rendering* lives in the CLI, not here.

use serde::Serialize;

/// Complexity metrics for a single function-like unit (function, method, arrow, accessor).
///
/// Each unit is measured independently: nesting resets to 0 at the function
/// boundary and nested functions are reported as `children` rather than being
/// folded into the parent's own score. See [`crate::engine`] for the exact rules.
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
    /// Total parse errors across all files. Parsers are fault-tolerant where
    /// possible, so a non-zero count means some code was measured from a
    /// partial parse (or, for non-recovering parsers, skipped entirely).
    pub parse_error_count: usize,
    /// Number of files that had at least one parse error.
    pub parse_error_file_count: usize,
    /// Paths of the files that had parse errors, in report order. Lets a
    /// consumer locate the affected files without scanning every entry in
    /// `files` (which the `--top-*` output doesn't even include).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parse_error_files: Vec<String>,
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
pub fn for_each_function(fns: &[FunctionReport], f: &mut impl FnMut(&FunctionReport)) {
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
pub fn build_top_report(
    reports: &[FileReport],
    summary: Summary,
    metric: Metric,
    n: usize,
) -> TopReport {
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
    let mut parse_error_count = 0;
    let mut parse_error_files = Vec::new();
    for r in reports {
        for_each_function(&r.functions, &mut |f| {
            cog.push(f.cognitive);
            cyc.push(f.cyclomatic);
        });
        if !r.parse_errors.is_empty() {
            parse_error_count += r.parse_errors.len();
            parse_error_files.push(r.path.clone());
        }
    }
    Summary {
        file_count: reports.len(),
        function_count: cog.len(),
        parse_error_count,
        parse_error_file_count: parse_error_files.len(),
        parse_error_files,
        cognitive: metric_summary(cog),
        cyclomatic: metric_summary(cyc),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<u32> = (1..=10).collect();
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
    fn summary_aggregates_parse_errors() {
        let file = |path: &str, errors: Vec<String>| FileReport {
            path: path.to_string(),
            cognitive: 0,
            cyclomatic: 0,
            functions: Vec::new(),
            parse_errors: errors,
        };
        let reports = vec![
            file("clean", Vec::new()),
            file("one", vec!["syntax error at line 1".to_string()]),
            file(
                "two",
                vec![
                    "syntax error at line 3".to_string(),
                    "syntax error at line 9".to_string(),
                ],
            ),
        ];
        let s = compute_summary(&reports);
        assert_eq!(s.parse_error_count, 3);
        assert_eq!(s.parse_error_file_count, 2);
        assert_eq!(s.parse_error_files, vec!["one", "two"]);
    }

    #[test]
    fn summary_without_parse_errors_is_zero() {
        let s = compute_summary(&[]);
        assert_eq!(s.parse_error_count, 0);
        assert_eq!(s.parse_error_file_count, 0);
        assert!(s.parse_error_files.is_empty());
    }
}
