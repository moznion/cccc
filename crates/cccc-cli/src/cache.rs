//! On-disk results cache: skip re-analyzing files that haven't changed.
//!
//! Analysis is a pure function of a file's content and the language front-end
//! that handles it, so a cached [`FileReport`] can be reused as long as neither
//! changed. An entry is validated by (size, mtime) — stat-level checks, no file
//! read — plus the canonical name of the language that produced it (extension
//! re-routing via `--ext`/`[ext]` must not resurface another language's scores).
//! The whole cache is stamped with the cccc version, since counting rules can
//! change between releases.
//!
//! ## File layout
//!
//! `bincode(version, index, blobs)`, where `index` maps each path to its
//! validation signature and a byte range into `blobs`, and `blobs` is the
//! concatenation of one independently-encoded blob per file. On load only the
//! index is decoded; each hit decodes just its own blob, in parallel on the
//! analysis thread pool. Warm-run overhead is thus "stat + decode what you
//! use", regardless of cache size.
//!
//! Any unreadable, corrupt, or version-mismatched cache file degrades to a
//! cold run — the cache is an accelerator, never a source of errors.
//!
//! [`FileReport`] is serialized through mirror types: its `skip_serializing_if`
//! attributes would corrupt a non-self-describing format like bincode (a
//! skipped field shifts every later byte of the stream).

use std::collections::HashMap;
use std::path::Path;

use rayon::prelude::*;

use cccc_core::report::{FileReport, FunctionReport};

/// Entries are only valid for the exact version that wrote them.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Validation signature plus blob location for one cached file.
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    /// Canonical name of the language that analyzed the file.
    lang: String,
    size: u64,
    mtime_ns: u128,
    offset: u64,
    len: u64,
}

/// Mirror of [`FunctionReport`] without `skip_serializing_if` (see module doc).
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheFun {
    name: String,
    kind: String,
    line: u32,
    cognitive: u32,
    cyclomatic: u32,
    children: Vec<CacheFun>,
}

/// Mirror of [`FileReport`] without `skip_serializing_if` (see module doc).
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheReport {
    path: String,
    cognitive: u32,
    cyclomatic: u32,
    functions: Vec<CacheFun>,
    parse_errors: Vec<String>,
}

/// A loaded cache: the lookup index plus the raw, still-encoded entry blobs.
pub struct Cache {
    index: HashMap<String, IndexEntry>,
    blobs: Vec<u8>,
}

/// Load the cache at `path`. Returns `None` — a cold run — when the file is
/// missing, unreadable, corrupt, or written by a different cccc version.
pub fn load(path: &Path) -> Option<Cache> {
    let bytes = std::fs::read(path).ok()?;
    let (version, index, blobs): (String, Vec<(String, IndexEntry)>, Vec<u8>) =
        bincode::deserialize(&bytes).ok()?;
    if version != VERSION {
        return None;
    }
    Some(Cache {
        index: index.into_iter().collect(),
        blobs,
    })
}

impl Cache {
    /// Number of files this cache holds entries for.
    pub fn entry_count(&self) -> usize {
        self.index.len()
    }

    /// The cached report for `path`, if the file still matches its recorded
    /// signature and is still analyzed by `lang`.
    pub fn lookup(&self, path: &Path, lang: &str) -> Option<FileReport> {
        let entry = self.index.get(&path.display().to_string())?;
        if entry.lang != lang || file_sig(path)? != (entry.size, entry.mtime_ns) {
            return None;
        }
        let blob = self
            .blobs
            .get(entry.offset as usize..(entry.offset + entry.len) as usize)?;
        let report: CacheReport = bincode::deserialize(blob).ok()?;
        Some(from_cache(report))
    }
}

/// Write a fresh cache for `reports` to `path`, replacing any previous file.
/// `lang_for` names the language that analyzed a path (entries it can't name
/// are simply not cached). Encoding fans out on the current rayon pool; a
/// failed write only costs the next run its warm start, so it warns rather
/// than failing the process.
pub fn store(
    path: &Path,
    reports: &[FileReport],
    lang_for: &(dyn Fn(&Path) -> Option<&'static str> + Sync),
) {
    let encoded: Vec<(String, IndexEntry, Vec<u8>)> = reports
        .par_iter()
        .filter_map(|r| {
            let file = Path::new(&r.path);
            let lang = lang_for(file)?;
            let (size, mtime_ns) = file_sig(file)?;
            let blob = bincode::serialize(&to_cache(r)).ok()?;
            let entry = IndexEntry {
                lang: lang.to_string(),
                size,
                mtime_ns,
                offset: 0, // fixed up below, once concatenation order is known
                len: blob.len() as u64,
            };
            Some((r.path.clone(), entry, blob))
        })
        .collect();

    let mut index = Vec::with_capacity(encoded.len());
    let mut blobs = Vec::new();
    for (key, mut entry, blob) in encoded {
        entry.offset = blobs.len() as u64;
        blobs.extend_from_slice(&blob);
        index.push((key, entry));
    }

    let Ok(bytes) = bincode::serialize(&(VERSION, index, blobs)) else {
        crate::output::warn("cccc: warning: failed to encode results cache");
        return;
    };
    // Write-then-rename so an interrupted run cannot leave a torn cache file.
    let Some(name) = path.file_name() else { return };
    let tmp = path.with_file_name(format!("{}.tmp", name.to_string_lossy()));
    let written = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        crate::output::warn(&format!(
            "cccc: warning: failed to write results cache {}: {e}",
            path.display()
        ));
    }
}

/// Stat `path` into its cache-validation signature.
fn file_sig(path: &Path) -> Option<(u64, u128)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((md.len(), mtime))
}

fn to_cache_funs(fns: &[FunctionReport]) -> Vec<CacheFun> {
    fns.iter()
        .map(|f| CacheFun {
            name: f.name.clone(),
            kind: f.kind.clone(),
            line: f.line,
            cognitive: f.cognitive,
            cyclomatic: f.cyclomatic,
            children: to_cache_funs(&f.children),
        })
        .collect()
}

fn from_cache_funs(fns: Vec<CacheFun>) -> Vec<FunctionReport> {
    fns.into_iter()
        .map(|f| FunctionReport {
            name: f.name,
            kind: f.kind,
            line: f.line,
            cognitive: f.cognitive,
            cyclomatic: f.cyclomatic,
            children: from_cache_funs(f.children),
        })
        .collect()
}

fn to_cache(r: &FileReport) -> CacheReport {
    CacheReport {
        path: r.path.clone(),
        cognitive: r.cognitive,
        cyclomatic: r.cyclomatic,
        functions: to_cache_funs(&r.functions),
        parse_errors: r.parse_errors.clone(),
    }
}

fn from_cache(c: CacheReport) -> FileReport {
    FileReport {
        path: c.path,
        cognitive: c.cognitive,
        cyclomatic: c.cyclomatic,
        functions: from_cache_funs(c.functions),
        parse_errors: c.parse_errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report(path: &str) -> FileReport {
        FileReport {
            path: path.to_string(),
            cognitive: 3,
            cyclomatic: 2,
            functions: vec![FunctionReport {
                name: "f".to_string(),
                kind: "function".to_string(),
                line: 1,
                cognitive: 3,
                cyclomatic: 2,
                children: vec![FunctionReport {
                    name: "g".to_string(),
                    kind: "arrow".to_string(),
                    line: 2,
                    cognitive: 0,
                    cyclomatic: 1,
                    children: Vec::new(),
                }],
            }],
            parse_errors: vec!["boom".to_string()],
        }
    }

    #[test]
    fn roundtrip_hits_for_unchanged_file() {
        let dir = std::env::temp_dir().join("cccc_cache_unit_roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.ts");
        std::fs::write(&src, "function f() {}").unwrap();
        let cache_file = dir.join("cache.bin");

        let report = sample_report(&src.display().to_string());
        store(&cache_file, &[report], &|_| Some("es"));

        let cache = load(&cache_file).expect("cache loads");
        let hit = cache.lookup(&src, "es").expect("unchanged file hits");
        assert_eq!(hit.cognitive, 3);
        assert_eq!(hit.functions[0].children[0].name, "g");
        assert_eq!(hit.parse_errors, vec!["boom".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_file_and_changed_language_miss() {
        let dir = std::env::temp_dir().join("cccc_cache_unit_invalidate");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.ts");
        std::fs::write(&src, "function f() {}").unwrap();
        let cache_file = dir.join("cache.bin");

        store(
            &cache_file,
            &[sample_report(&src.display().to_string())],
            &|_| Some("es"),
        );
        let cache = load(&cache_file).unwrap();
        assert!(
            cache.lookup(&src, "rust").is_none(),
            "other language misses"
        );

        std::fs::write(&src, "function f() { return 1 }").unwrap();
        assert!(cache.lookup(&src, "es").is_none(), "changed file misses");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cache_degrades_to_cold() {
        let dir = std::env::temp_dir().join("cccc_cache_unit_corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let cache_file = dir.join("cache.bin");
        std::fs::write(&cache_file, b"not a cache").unwrap();
        assert!(load(&cache_file).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
