//! Discover the source files to analyze from the given paths.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState};

fn has_ext(path: &Path, exts: &[String]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Compile `--exclude` glob patterns into a single matcher.
///
/// Returns `Ok(None)` when no patterns are given (the common case, so callers
/// can skip matching entirely). `literal_separator(true)` makes `*` stop at `/`,
/// so `**` is the way to span directories — matching the intuition from
/// `.gitignore`. An invalid pattern is surfaced as an error rather than silently
/// ignored.
pub fn build_exclude_set(patterns: &[String]) -> Result<Option<GlobSet>, globset::Error> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(GlobBuilder::new(p).literal_separator(true).build()?);
    }
    Ok(Some(builder.build()?))
}

/// True if `path` should be skipped per the exclude set. Patterns are matched
/// against the path **relative to its walk root** (so `dist/**` is anchored at
/// the directory the user passed, regardless of whether that root was absolute),
/// and additionally against the file name alone (so `*.test.ts` matches at any
/// depth without needing a `**/` prefix).
fn is_excluded(path: &Path, base: Option<&Path>, exclude: Option<&GlobSet>) -> bool {
    let Some(set) = exclude else { return false };
    let rel = base
        .and_then(|b| path.strip_prefix(b).ok())
        // No walk root (explicit file argument): just drop a leading `./`.
        .unwrap_or_else(|| path.strip_prefix(".").unwrap_or(path));
    if set.is_match(rel) {
        return true;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| set.is_match(name))
}

/// Collect matching files from `paths`. Explicit file arguments are included
/// regardless of extension; directories are walked (respecting ignore files
/// unless `no_ignore`) and filtered by `exts`. `node_modules` is always skipped.
/// Any file matching `exclude` is dropped, whether named explicitly or found by
/// walking. Directories are walked with up to `threads` workers; the returned
/// order is unspecified (reports are sorted by path before rendering).
pub fn collect_files(
    paths: &[PathBuf],
    exts: &[String],
    no_ignore: bool,
    exclude: Option<&GlobSet>,
    threads: usize,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    // Canonicalizing (a syscall per file) exists only to dedup overlapping
    // roots; a single root cannot yield the same file twice, so skip it then.
    let dedup = paths.len() > 1;

    for root in paths {
        if root.is_file() {
            if !is_excluded(root, None, exclude) {
                push_unique(root, dedup, &mut out, &mut seen);
            }
            continue;
        }

        let mut builder = WalkBuilder::new(root);
        builder
            .git_ignore(!no_ignore)
            .git_global(!no_ignore)
            .git_exclude(!no_ignore)
            .ignore(!no_ignore)
            .hidden(false)
            .threads(threads)
            .filter_entry(|entry| entry.file_name() != "node_modules");

        let collected: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
        builder.build_parallel().run(|| {
            Box::new(|result| {
                let Ok(entry) = result else {
                    return WalkState::Continue;
                };
                // The walker already knows the entry's type; `Path::is_file`
                // would stat every entry a second time.
                if entry.file_type().is_some_and(|ft| ft.is_file()) {
                    let path = entry.path();
                    if has_ext(path, exts) && !is_excluded(path, Some(root), exclude) {
                        collected.lock().unwrap().push(path.to_path_buf());
                    }
                }
                WalkState::Continue
            })
        });
        for path in collected.into_inner().unwrap() {
            push_unique(&path, dedup, &mut out, &mut seen);
        }
    }

    out
}

fn push_unique(path: &Path, dedup: bool, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    if dedup {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !seen.insert(key) {
            return;
        }
    }
    out.push(path.to_path_buf());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a fresh, empty temp tree for one test.
    fn temp_tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ts_exts() -> Vec<String> {
        vec!["ts".to_string()]
    }

    /// File names of the collected paths, sorted for order-independent asserts
    /// (the parallel walk returns files in an unspecified order).
    fn sorted_names(files: &[PathBuf]) -> Vec<String> {
        let mut names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn walks_a_single_root_filtering_by_extension() {
        let dir = temp_tree("cccc_walk_single_root");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.ts"), "").unwrap();
        fs::write(dir.join("sub/b.ts"), "").unwrap();
        fs::write(dir.join("sub/skip.txt"), "").unwrap();

        let files = collect_files(std::slice::from_ref(&dir), &ts_exts(), false, None, 8);
        assert_eq!(sorted_names(&files), ["a.ts", "b.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_roots_are_deduped() {
        let dir = temp_tree("cccc_walk_dup_roots");
        fs::write(dir.join("a.ts"), "").unwrap();

        let files = collect_files(&[dir.clone(), dir.clone()], &ts_exts(), false, None, 8);
        assert_eq!(sorted_names(&files), ["a.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_overlapping_roots_are_deduped() {
        let dir = temp_tree("cccc_walk_nested_roots");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.ts"), "").unwrap();
        fs::write(dir.join("sub/b.ts"), "").unwrap();

        // `sub/b.ts` is reachable from both roots but must be counted once.
        let files = collect_files(&[dir.clone(), dir.join("sub")], &ts_exts(), false, None, 8);
        assert_eq!(sorted_names(&files), ["a.ts", "b.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_file_overlapping_a_walked_root_is_deduped() {
        let dir = temp_tree("cccc_walk_file_and_root");
        fs::write(dir.join("a.ts"), "").unwrap();

        let files = collect_files(&[dir.clone(), dir.join("a.ts")], &ts_exts(), false, None, 8);
        assert_eq!(sorted_names(&files), ["a.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_is_deduped() {
        let dir = temp_tree("cccc_walk_symlink_roots");
        let real = dir.join("real");
        let link = dir.join("link");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("a.ts"), "").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The same file is reachable under two different paths; canonicalization
        // must collapse them to one entry.
        let files = collect_files(&[real, link], &ts_exts(), false, None, 8);
        assert_eq!(sorted_names(&files), ["a.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_thread_finds_the_same_files_as_parallel() {
        let dir = temp_tree("cccc_walk_thread_count");
        for sub in ["x", "y", "z"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
            for f in ["a.ts", "b.ts"] {
                fs::write(dir.join(sub).join(f), "").unwrap();
            }
        }

        let one = collect_files(std::slice::from_ref(&dir), &ts_exts(), false, None, 1);
        let many = collect_files(std::slice::from_ref(&dir), &ts_exts(), false, None, 8);
        let sort = |mut v: Vec<PathBuf>| {
            v.sort();
            v
        };
        assert_eq!(sort(one), sort(many));
        let _ = fs::remove_dir_all(&dir);
    }
}
