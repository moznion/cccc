//! Command-line interface definition.

use std::path::PathBuf;

use clap::Parser;

/// Measure Cognitive Complexity and Cyclomatic Complexity of TS/JS code.
#[derive(Debug, Parser)]
#[command(name = "cccc", version, about)]
pub struct Cli {
    /// Files or directories to analyze.
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,

    /// Print a human-readable table instead of JSON.
    #[arg(long)]
    pub table: bool,

    /// Comma-separated file extensions to include (overrides the default set).
    #[arg(long, value_delimiter = ',')]
    pub ext: Option<Vec<String>>,

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

    /// Number of files to analyze in parallel. Defaults to the number of
    /// available logical CPUs.
    #[arg(short = 'j', long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub jobs: Option<u32>,
}
