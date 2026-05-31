//! Discover the source files to analyze from the given paths.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

fn has_ext(path: &Path, exts: &[String]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Collect matching files from `paths`. Explicit file arguments are included
/// regardless of extension; directories are walked (respecting ignore files
/// unless `no_ignore`) and filtered by `exts`. `node_modules` is always skipped.
pub fn collect_files(paths: &[PathBuf], exts: &[String], no_ignore: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for root in paths {
        if root.is_file() {
            push_unique(root, &mut out, &mut seen);
            continue;
        }

        let mut builder = WalkBuilder::new(root);
        builder
            .git_ignore(!no_ignore)
            .git_global(!no_ignore)
            .git_exclude(!no_ignore)
            .ignore(!no_ignore)
            .hidden(false)
            .filter_entry(|entry| entry.file_name() != "node_modules");

        for result in builder.build() {
            let Ok(entry) = result else { continue };
            let path = entry.path();
            if path.is_file() && has_ext(path, exts) {
                push_unique(path, &mut out, &mut seen);
            }
        }
    }

    out
}

fn push_unique(path: &Path, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if seen.insert(key) {
        out.push(path.to_path_buf());
    }
}
