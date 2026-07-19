//! Command-line interface definition.

use std::path::PathBuf;

use clap::Parser;

/// Measure Cognitive Complexity and Cyclomatic Complexity of source code.
///
/// One binary analyzes every bundled language; each file is dispatched to the
/// right front-end by its extension. Restrict the set with `--lang`, and bake in
/// recurring options with a `cccc.toml` file.
#[derive(Debug, Parser)]
#[command(about)]
pub struct Cli {
    /// Files or directories to analyze.
    #[arg(required_unless_present = "print_cache_file")]
    pub paths: Vec<PathBuf>,

    /// Restrict analysis to these languages (comma-separated; e.g.
    /// `es,go`). Accepts canonical names and aliases (e.g. `rust`/`rs`,
    /// `typescript`/`ts`). Defaults to every supported language.
    #[arg(long, value_delimiter = ',', value_name = "LIST")]
    pub lang: Option<Vec<String>>,

    /// Exclude these languages from analysis (comma-separated). The inverse of
    /// `--lang`: applied to all languages, or to `--lang`'s set if also given.
    #[arg(long, value_delimiter = ',', value_name = "LIST")]
    pub exclude_lang: Option<Vec<String>>,

    /// Use this config file instead of discovering one. The file must exist.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Do not look for or load a `cccc.toml` config file.
    #[arg(long)]
    pub no_config: bool,

    /// Print a human-readable table instead of JSON.
    #[arg(long)]
    pub table: bool,

    /// File extensions to analyze. Two forms, and repeatable: a global
    /// comma-separated list (`--ext ts,tsx`) restricts which extensions are
    /// scanned across all languages; a per-language override (`--ext
    /// es=ts,tsx`) replaces that language's default extensions and routes those
    /// extensions to it (overriding the config file's `[ext]`).
    #[arg(long, value_name = "EXTS | LANG=EXTS")]
    pub ext: Option<Vec<String>>,

    /// Glob pattern of files to exclude from analysis. May be given multiple
    /// times. Each pattern is matched against a file's path (e.g. `dist/**`)
    /// and against its file name alone (e.g. `*.test.ts`, which then matches at
    /// any depth). Brace alternation is supported: `**/*.{test,spec}.ts`.
    /// `*` does not cross `/`; use `**` to span directories.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Exit non-zero if any function's cognitive complexity exceeds this value.
    #[arg(long, value_name = "N")]
    pub max_cognitive: Option<u32>,

    /// Exit non-zero if any function's cyclomatic complexity exceeds this value.
    #[arg(long, value_name = "N")]
    pub max_cyclomatic: Option<u32>,

    /// Only report functions whose cognitive or cyclomatic complexity is >= N.
    #[arg(long, value_name = "N")]
    pub min: Option<u32>,

    /// Show only the N most cognitively-complex functions across all files, as a
    /// flat ranking (replaces the per-file output; the summary is still shown).
    #[arg(long, value_name = "N", conflicts_with = "top_cyclomatic")]
    pub top_cognitive: Option<usize>,

    /// Show only the N most cyclomatically-complex functions across all files, as
    /// a flat ranking (replaces the per-file output; the summary is still shown).
    #[arg(long, value_name = "N")]
    pub top_cyclomatic: Option<usize>,

    /// Do not respect .gitignore / ignore files when walking directories.
    #[arg(long)]
    pub no_ignore: bool,

    /// Cache results and reuse them for files unchanged since the last run.
    /// The cache lives in `.cccc.cache` next to the config file (or in the
    /// current directory without one) unless `--cache-file` says otherwise.
    #[arg(long)]
    pub cache: bool,

    /// Where to keep the results cache (implies `--cache`).
    #[arg(long, value_name = "PATH")]
    pub cache_file: Option<PathBuf>,

    /// Do not use the results cache, even if the config file enables it.
    #[arg(long, conflicts_with_all = ["cache", "cache_file"])]
    pub no_cache: bool,

    /// Print the resolved results-cache path and exit without analyzing
    /// anything. Prints nothing when the cache is disabled. For tooling
    /// (e.g. CI wrappers) that must know the cache location up front.
    #[arg(long)]
    pub print_cache_file: bool,

    /// Pretty-print the JSON output (the default is compact, one line).
    #[arg(long)]
    pub pretty: bool,

    /// Number of files to analyze in parallel. Defaults to the number of
    /// available logical CPUs.
    #[arg(short = 'j', long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub jobs: Option<u32>,
}
