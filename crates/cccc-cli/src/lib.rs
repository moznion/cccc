//! The unified `cccc` CLI.
//!
//! One binary measures Cognitive Complexity and Cyclomatic Complexity across
//! every bundled language. A file is routed to the right front-end by its
//! extension (see [`lang`]), so a single run can analyze a mixed-language tree.
//! Recurring options can be baked into a `cccc.toml` config file (see
//! [`config`]); CLI flags always take precedence over it.
//!
//! Everything common to the languages lives here: argument parsing, config
//! resolution, file discovery, parallel analysis, the threshold/`--min`/`--top`
//! logic, and output rendering. Each language supplies only how to analyze one
//! file and its default extensions, via the [`lang::LANGUAGES`] registry.
//!
//! ## Exit codes
//!
//! [`run`] returns a process exit code with a consistent meaning:
//! - `0` — success (including an existing input path that simply contains no
//!   matching files: "nothing to analyze" is not an error).
//! - `1` — a `--max-cognitive`/`--max-cyclomatic` threshold was exceeded
//!   (the CI gate).
//! - `2` — unable to proceed: a given path does not exist, a config/`--lang`
//!   value was invalid, or the worker pool could not be created. (clap's own
//!   usage errors also exit `2`.)

mod cli;
pub mod config;
pub mod lang;
mod output;
mod walk;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches};
use rayon::prelude::*;

use cccc_core::report::{self, FileReport, FunctionReport, Metric, Report};
use cli::Cli;
use config::Config;

/// Below this many files, sequential analysis beats paying for a rayon pool.
const PARALLEL_THRESHOLD: usize = 16;

/// Analyze one file's source into a [`FileReport`]. Implemented per language by
/// the relevant adapter (e.g. `cccc_es::analyze_source`).
pub type AnalyzeFn = fn(&Path, &str) -> FileReport;

/// Run the CLI end to end and return a process exit code.
pub fn run() -> i32 {
    let command = Cli::command()
        .name("cccc")
        .bin_name("cccc")
        .version(env!("CARGO_PKG_VERSION"));
    let matches = command.get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };

    // Load the config file (honoring --config/--no-config) before resolving any
    // option, so CLI flags can be layered on top of it.
    let config = match Config::resolve(cli.config.as_deref(), cli.no_config) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("cccc: {e}");
            return 2;
        }
    };

    // Resolve each option as: explicit CLI value > config file > built-in
    // default. `was_set` distinguishes a flag the user actually passed from one
    // sitting at its clap default (needed for the boolean flags).
    let was_set = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
    let table = if was_set("table") {
        cli.table
    } else {
        config.table.unwrap_or(false)
    };
    let no_ignore = if was_set("no_ignore") {
        cli.no_ignore
    } else {
        config.no_ignore.unwrap_or(false)
    };
    let exclude_patterns = if was_set("exclude") {
        cli.exclude.clone()
    } else {
        config.exclude.clone().unwrap_or_default()
    };
    let max_cognitive = cli.max_cognitive.or(config.max_cognitive);
    let max_cyclomatic = cli.max_cyclomatic.or(config.max_cyclomatic);
    let min = cli.min.or(config.min);
    let jobs_opt = cli.jobs.or(config.jobs);
    let lang_filter = cli.lang.clone().or_else(|| config.languages.clone());
    let exclude_lang_filter = cli
        .exclude_lang
        .clone()
        .or_else(|| config.exclude_languages.clone());
    // Resolve extensions from both the config `[ext]` table and the `--ext`
    // flag. `per_lang_ext` (keyed by canonical language name) replaces a
    // language's default extensions and routes them to it; `global_ext` is a
    // cross-language scan filter. CLI per-language entries override the config's.
    let (per_lang_ext, global_ext) = match resolve_ext(config.ext.clone(), cli.ext.as_deref()) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("cccc: {e}");
            return 2;
        }
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

    // Determine which languages are in play and build the extension→analyzer
    // dispatch used to route each file.
    let languages =
        match lang::resolve_languages(lang_filter.as_deref(), exclude_lang_filter.as_deref()) {
            Ok(langs) => langs,
            Err(e) => {
                eprintln!("cccc: {e}");
                return 2;
            }
        };
    let dispatch = lang::build_dispatch(&languages, &per_lang_ext);

    // Which extensions to collect: a global `--ext` filter if one was given,
    // otherwise the union of the active languages' (possibly overridden)
    // extensions. Either way each file is dispatched by its own extension.
    let exts: Vec<String> = if global_ext.is_empty() {
        dispatch.keys().cloned().collect()
    } else {
        for ext in &global_ext {
            if !dispatch.contains_key(ext) {
                output::warn(&format!(
                    "cccc: warning: no active language analyzes .{ext} files; they will be skipped"
                ));
            }
        }
        global_ext
    };

    // Compile the exclude globs up front so a bad pattern fails loudly (exit 2)
    // rather than silently skipping nothing.
    let exclude = match walk::build_exclude_set(&exclude_patterns) {
        Ok(set) => set,
        Err(e) => {
            eprintln!("cccc: invalid --exclude pattern: {e}");
            return 2;
        }
    };

    // `--jobs` caps the worker count; without it we fall back to the number of
    // logical CPUs (1 if that can't be determined). Resolved before discovery
    // because the directory walk fans out across the same worker count.
    let jobs = jobs_opt.map(|j| j as usize).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });

    let files = walk::collect_files(&cli.paths, &exts, no_ignore, exclude.as_ref(), jobs);
    if files.is_empty() {
        eprintln!("cccc: no matching files found");
        return 0;
    }

    // For a handful of files, spinning up a rayon pool costs more than it saves,
    // so analyze sequentially. Above the threshold, fan out across `jobs` workers.
    let mut reports: Vec<FileReport> = if jobs <= 1 || files.len() <= PARALLEL_THRESHOLD {
        files
            .iter()
            .filter_map(|p| read_and_analyze(&dispatch, p))
            .collect()
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
                .filter_map(|p| read_and_analyze(&dispatch, p))
                .collect()
        })
    };

    reports.sort_by(|a, b| a.path.cmp(&b.path));

    // Determine exit status before any `--min` filtering, so display options do
    // not change pass/fail behaviour.
    let fail = (max_cognitive.is_some() || max_cyclomatic.is_some())
        && reports
            .iter()
            .any(|r| exceeds(&r.functions, max_cognitive, max_cyclomatic));

    // Compute the summary over the full population, before `--min`/`--top` change
    // what is displayed, so the distribution always reflects all code.
    let summary = report::compute_summary(&reports);

    // Parse errors mean the scores may be computed from a partial parse. JSON
    // consumers see this in `summary`; for the human-readable views, warn on
    // stderr — with the affected paths — so it isn't lost in a long table.
    if table && summary.parse_error_count > 0 {
        output::warn(&format!(
            "cccc: warning: {} parse error(s) in {} file(s); results for those files may be incomplete:",
            summary.parse_error_count, summary.parse_error_file_count
        ));
        for r in reports.iter().filter(|r| !r.parse_errors.is_empty()) {
            output::warn(&format!("cccc:   {} ({})", r.path, r.parse_errors.len()));
        }
    }

    // `--top-*` is a distinct, flat ranking view that replaces the per-file
    // output. The two top flags are mutually exclusive (enforced by clap).
    let top_request = match (cli.top_cognitive, cli.top_cyclomatic) {
        (Some(n), _) => Some((Metric::Cognitive, n)),
        (_, Some(n)) => Some((Metric::Cyclomatic, n)),
        (None, None) => None,
    };
    if let Some((metric, n)) = top_request {
        let top = report::build_top_report(&reports, summary, metric, n);
        if table {
            output::print_top_table(&top);
        } else {
            output::print_json(&top);
        }
        return i32::from(fail);
    }

    if let Some(min) = min {
        for r in &mut reports {
            r.functions = filter_min(std::mem::take(&mut r.functions), min);
        }
    }

    let report = Report {
        files: reports,
        summary,
    };

    if table {
        output::print_table(&report);
    } else {
        output::print_json(&report);
    }

    i32::from(fail)
}

/// Resolved extension settings: per-language overrides keyed by canonical
/// language name, plus the global cross-language scan filter.
type ExtSettings = (BTreeMap<String, Vec<String>>, Vec<String>);

/// Merge the config `[ext]` table and the `--ext` flag into [`ExtSettings`].
///
/// Config entries are applied first, then CLI entries, so a `--ext lang=…`
/// overrides the config's entry for the same language. Each `--ext` value is
/// either `LANG=ext,ext` (a per-language override) or `ext,ext` (added to the
/// global filter). An unknown language name (in either source) is an error.
fn resolve_ext(
    config_ext: Option<BTreeMap<String, Vec<String>>>,
    cli_ext: Option<&[String]>,
) -> Result<ExtSettings, String> {
    let mut per_lang: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(cfg) = config_ext {
        for (key, list) in cfg {
            let canon = lang::require_canonical(key.trim(), "[ext] config")?;
            per_lang.insert(canon.to_string(), list);
        }
    }
    let mut global: Vec<String> = Vec::new();
    for raw in cli_ext.unwrap_or(&[]) {
        match raw.split_once('=') {
            Some((name, exts)) => {
                let canon = lang::require_canonical(name.trim(), "--ext")?;
                per_lang.insert(canon.to_string(), split_exts(exts));
            }
            None => global.extend(split_exts(raw).into_iter().map(|e| e.to_ascii_lowercase())),
        }
    }
    Ok((per_lang, global))
}

/// Split a comma-separated extension list, trimming blanks and empties.
fn split_exts(s: &str) -> Vec<String> {
    s.split(',')
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Read a file and analyze it with the language matching its extension,
/// reporting (but not failing on) read errors. A file whose extension no active
/// language claims is skipped silently (it was only collected via `--ext`).
fn read_and_analyze(dispatch: &HashMap<String, AnalyzeFn>, path: &Path) -> Option<FileReport> {
    let analyze = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| dispatch.get(&e.to_ascii_lowercase()))?;
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
