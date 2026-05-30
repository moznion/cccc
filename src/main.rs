mod analyzer;
mod cli;
mod report;
mod walk;

use clap::Parser;
use rayon::prelude::*;

use std::path::Path;

use cli::Cli;
use report::{FileReport, FunctionReport, Metric, Report};

/// Below this many files, sequential analysis beats paying for a rayon pool.
const PARALLEL_THRESHOLD: usize = 16;

fn main() {
    std::process::exit(run());
}

/// Read and analyze one file, reporting (but not failing on) read errors.
fn analyze(path: &Path) -> Option<FileReport> {
    match std::fs::read_to_string(path) {
        Ok(src) => Some(analyzer::analyze_source(path, &src)),
        Err(e) => {
            eprintln!("cccc: cannot read {}: {e}", path.display());
            None
        }
    }
}

fn run() -> i32 {
    let cli = Cli::parse();

    let exts: Vec<String> = match &cli.ext {
        Some(e) => e.iter().map(|s| s.trim().to_string()).collect(),
        None => walk::DEFAULT_EXTS.iter().map(|s| s.to_string()).collect(),
    };

    let files = walk::collect_files(&cli.paths, &exts, cli.no_ignore);
    if files.is_empty() {
        eprintln!("cccc: no matching files found");
        return 0;
    }

    // `--jobs` caps the worker count; without it we fall back to the number of
    // logical CPUs (1 if that can't be determined).
    let jobs = cli.jobs.map(|j| j as usize).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });

    // For a handful of files, spinning up a rayon pool costs more than it saves,
    // so analyze sequentially. Above the threshold, fan out across `jobs` workers.
    let mut reports: Vec<FileReport> = if jobs <= 1 || files.len() <= PARALLEL_THRESHOLD {
        files.iter().filter_map(|p| analyze(p)).collect()
    } else {
        let pool = match rayon::ThreadPoolBuilder::new().num_threads(jobs).build() {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("cccc: failed to start thread pool: {e}");
                return 2;
            }
        };
        pool.install(|| files.par_iter().filter_map(|p| analyze(p)).collect())
    };

    reports.sort_by(|a, b| a.path.cmp(&b.path));

    // Determine exit status before any `--min` filtering, so display options do
    // not change pass/fail behaviour.
    let fail = (cli.max_cognitive.is_some() || cli.max_cyclomatic.is_some())
        && reports
            .iter()
            .any(|r| exceeds(&r.functions, cli.max_cognitive, cli.max_cyclomatic));

    // Compute the summary over the full population, before `--min`/`--top` change
    // what is displayed, so the distribution always reflects all code.
    let summary = report::compute_summary(&reports);

    // `--top-*` is a distinct, flat ranking view that replaces the per-file
    // output. The two top flags are mutually exclusive (enforced by clap).
    let top_request = match (cli.top_cognitive, cli.top_cyclomatic) {
        (Some(n), _) => Some((Metric::Cognitive, n)),
        (_, Some(n)) => Some((Metric::Cyclomatic, n)),
        (None, None) => None,
    };
    if let Some((metric, n)) = top_request {
        let top = report::build_top_report(&reports, summary, metric, n);
        if cli.table {
            report::print_top_table(&top);
        } else {
            report::print_json(&top);
        }
        return i32::from(fail);
    }

    if let Some(min) = cli.min {
        for r in &mut reports {
            r.functions = filter_min(std::mem::take(&mut r.functions), min);
        }
    }

    let report = Report {
        files: reports,
        summary,
    };

    if cli.table {
        report::print_table(&report);
    } else {
        report::print_json(&report);
    }

    i32::from(fail)
}

/// True if any function (at any depth) exceeds either threshold.
fn exceeds(fns: &[FunctionReport], max_cog: Option<u32>, max_cyc: Option<u32>) -> bool {
    fns.iter().any(|f| {
        max_cog.is_some_and(|m| f.cognitive > m)
            || max_cyc.is_some_and(|m| f.cyclomatic > m)
            || exceeds(&f.children, max_cog, max_cyc)
    })
}

/// Keep functions whose own complexity meets `min`, or that have a kept
/// descendant.
fn filter_min(fns: Vec<FunctionReport>, min: u32) -> Vec<FunctionReport> {
    fns.into_iter()
        .filter_map(|mut f| {
            f.children = filter_min(std::mem::take(&mut f.children), min);
            let keep = f.cognitive >= min || f.cyclomatic >= min || !f.children.is_empty();
            if keep { Some(f) } else { None }
        })
        .collect()
}
