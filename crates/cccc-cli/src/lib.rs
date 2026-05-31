//! Shared CLI machinery for `cccc` front-ends.
//!
//! Everything common to every language front-end lives here: argument parsing,
//! file discovery, parallel analysis, the threshold/`--min`/`--top` logic, and
//! output rendering. A language-specific binary (e.g. `cccc-es`) supplies only
//! two things via [`run`]: how to analyze one file into a [`FileReport`], and the
//! default set of file extensions. This keeps the per-language binaries tiny and
//! the behaviour identical across languages.
//!
//! ## Exit codes
//!
//! [`run`] returns a process exit code with a consistent meaning:
//! - `0` — success (including an existing input path that simply contains no
//!   matching files: "nothing to analyze" is not an error).
//! - `1` — a `--max-cognitive`/`--max-cyclomatic` threshold was exceeded
//!   (the CI gate).
//! - `2` — unable to proceed: a given path does not exist, or the worker pool
//!   could not be created. (clap's own usage errors also exit `2`.)

mod cli;
mod output;
mod walk;

use std::path::Path;

use clap::{CommandFactory, FromArgMatches};
use rayon::prelude::*;

use cccc_core::report::{self, FileReport, FunctionReport, Metric, Report};
use cli::Cli;

/// Below this many files, sequential analysis beats paying for a rayon pool.
const PARALLEL_THRESHOLD: usize = 16;

/// Analyze one file's source into a [`FileReport`]. Implemented per language by
/// the relevant adapter (e.g. `cccc_typescript::analyze_source`).
pub type AnalyzeFn = fn(&Path, &str) -> FileReport;

/// Run the CLI end to end and return a process exit code.
///
/// `bin_name` is the front-end binary's name (e.g. `"cccc-es"`); it sets the
/// program name shown in `--help`/`--version` and the version string, so the
/// shared `Cli` definition doesn't bake in any one front-end's identity.
/// `analyze` lowers+scores a single file; `default_exts` is the extension set
/// used when `--ext` is not given (e.g. `&["ts", "tsx", "js", ...]`).
pub fn run(
    bin_name: &'static str,
    version: &'static str,
    analyze: AnalyzeFn,
    default_exts: &[&str],
) -> i32 {
    let command = Cli::command()
        .name(bin_name)
        .bin_name(bin_name)
        .version(version);
    let cli = match Cli::from_arg_matches(&command.get_matches()) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };

    // A path that doesn't exist is almost always a typo, so fail loudly rather
    // than silently reporting "no files". (A path that exists but contains no
    // matching files is still treated as an empty, successful run below.)
    let mut any_missing = false;
    for path in cli.paths.iter().filter(|p| !p.exists()) {
        eprintln!("cccc: path does not exist: {}", path.display());
        any_missing = true;
    }
    if any_missing {
        return 2;
    }

    let exts: Vec<String> = match &cli.ext {
        Some(e) => e.iter().map(|s| s.trim().to_string()).collect(),
        None => default_exts.iter().map(|s| s.to_string()).collect(),
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
        files.iter().filter_map(|p| read_and_analyze(analyze, p)).collect()
    } else {
        let pool = match rayon::ThreadPoolBuilder::new().num_threads(jobs).build() {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("cccc: failed to start thread pool: {e}");
                return 2;
            }
        };
        pool.install(|| {
            files
                .par_iter()
                .filter_map(|p| read_and_analyze(analyze, p))
                .collect()
        })
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
            output::print_top_table(&top);
        } else {
            output::print_json(&top);
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
        output::print_table(&report);
    } else {
        output::print_json(&report);
    }

    i32::from(fail)
}

/// Read a file and analyze it, reporting (but not failing on) read errors.
fn read_and_analyze(analyze: AnalyzeFn, path: &Path) -> Option<FileReport> {
    match std::fs::read_to_string(path) {
        Ok(src) => Some(analyze(path, &src)),
        Err(e) => {
            eprintln!("cccc: cannot read {}: {e}", path.display());
            None
        }
    }
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
