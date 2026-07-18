//! On-disk results cache: skip re-analyzing files that haven't changed.
//!
//! Analysis is a pure function of a file's content and the language front-end
//! that handles it, so a cached [`FileReport`] can be reused as long as neither
//! changed. Validation is hybrid, the way git's index does it: an entry stores
//! the file's size, mtime, and an xxh3-128 hash of its content. When size and
//! mtime both match, the entry is trusted on the stat alone — no file read.
//! When the mtime moved but the size didn't, the file is read and its hash
//! compared: a match is still a hit (the content is what was analyzed), and the
//! entry's mtime is refreshed so the next run stat-hits again. This keeps warm
//! local runs at stat speed while surviving mtime churn — a fresh `git clone`
//! in CI, a branch switch, a `touch` — at the cost of hashing only the churned
//! files once. Each entry also records the canonical name of the language that
//! produced it (extension re-routing via `--ext`/`[ext]` must not resurface
//! another language's scores), and the whole cache is stamped with the cccc
//! version, since counting rules can change between releases.
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
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use cccc_core::report::{FileReport, FunctionReport};

/// Entries are only valid for the exact version that wrote them.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Validation signature for one file: stat facts for the cheap check, a
/// content hash as ground truth when the mtime has moved.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sig {
    size: u64,
    mtime_ns: u128,
    hash: u128,
}

/// The signature of content `bytes` observed at `mtime_ns`.
pub fn sig_for(bytes: &[u8], mtime_ns: u128) -> Sig {
    Sig {
        size: bytes.len() as u64,
        mtime_ns,
        hash: xxhash_rust::xxh3::xxh3_128(bytes),
    }
}

/// The mtime of `path` in nanoseconds since the epoch. Callers that go on to
/// read the file should stat first: if the file changes in between, the stored
/// mtime is then older than the analyzed content, and the staleness is caught
/// by the next run's hash check instead of masking the newer content.
pub fn mtime_ns(path: &Path) -> Option<u128> {
    mtime_of(&std::fs::metadata(path).ok()?)
}

fn mtime_of(md: &std::fs::Metadata) -> Option<u128> {
    Some(
        md.modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    )
}

/// Validation signature plus blob location for one cached file.
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    /// Canonical name of the language that analyzed the file.
    lang: String,
    sig: Sig,
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

/// A cache hit: the reusable report and the file's current signature.
/// `refreshed` marks a hit that went through the content check — the stored
/// mtime is stale, so the cache file should be rewritten with `sig` to make
/// the next run stat-hit instead of hashing again.
pub struct Hit {
    pub report: FileReport,
    pub sig: Sig,
    pub refreshed: bool,
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
    /// signature — by stat, or by content hash when only the mtime moved —
    /// and is still analyzed by `lang`.
    pub fn lookup(&self, path: &Path, lang: &str) -> Option<Hit> {
        let entry = self.index.get(&path.display().to_string())?;
        if entry.lang != lang {
            return None;
        }
        let md = std::fs::metadata(path).ok()?;
        if md.len() != entry.sig.size {
            return None;
        }
        let mtime = mtime_of(&md)?;
        let sig = if mtime == entry.sig.mtime_ns {
            entry.sig
        } else {
            // The mtime moved (checkout, touch, rewrite-in-place); the hash
            // decides. Size is re-checked from the bytes actually read, in
            // case the file changed again between the stat and the read.
            let sig = sig_for(&std::fs::read(path).ok()?, mtime);
            if sig.size != entry.sig.size || sig.hash != entry.sig.hash {
                return None;
            }
            sig
        };
        let blob = self
            .blobs
            .get(entry.offset as usize..(entry.offset + entry.len) as usize)?;
        let report: CacheReport = bincode::deserialize(blob).ok()?;
        Some(Hit {
            report: from_cache(report),
            refreshed: sig.mtime_ns != entry.sig.mtime_ns,
            sig,
        })
    }
}

/// Write a fresh cache for `entries` to `path`, replacing any previous file.
/// Signatures come from the caller — hashed from bytes it already had in
/// memory, never by re-reading here — and an entry without one (its read-time
/// stat failed), or whose language `lang_for` can't name, is simply not
/// cached. Encoding fans out on the current rayon pool; a failed write only
/// costs the next run its warm start, so it warns rather than failing the
/// process.
pub fn store(
    path: &Path,
    entries: &[(FileReport, Option<Sig>)],
    lang_for: &(dyn Fn(&Path) -> Option<&'static str> + Sync),
) {
    let encoded: Vec<(String, IndexEntry, Vec<u8>)> = entries
        .par_iter()
        .filter_map(|(r, sig)| {
            let sig = (*sig)?;
            let lang = lang_for(Path::new(&r.path))?;
            let blob = bincode::serialize(&to_cache(r)).ok()?;
            let entry = IndexEntry {
                lang: lang.to_string(),
                sig,
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

    /// Store a one-entry cache for `src`, signed off its current content.
    fn store_one(cache_file: &Path, src: &Path, lang: &'static str) {
        let sig = sig_for(&std::fs::read(src).unwrap(), mtime_ns(src).unwrap());
        store(
            cache_file,
            &[(sample_report(&src.display().to_string()), Some(sig))],
            &move |_| Some(lang),
        );
    }

    #[test]
    fn roundtrip_hits_for_unchanged_file() {
        let dir = std::env::temp_dir().join("cccc_cache_unit_roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.ts");
        std::fs::write(&src, "function f() {}").unwrap();
        let cache_file = dir.join("cache.bin");

        store_one(&cache_file, &src, "es");

        let cache = load(&cache_file).expect("cache loads");
        let hit = cache.lookup(&src, "es").expect("unchanged file hits");
        assert!(!hit.refreshed, "a stat hit needs no rewrite");
        assert_eq!(hit.report.cognitive, 3);
        assert_eq!(hit.report.functions[0].children[0].name, "g");
        assert_eq!(hit.report.parse_errors, vec!["boom".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_file_and_changed_language_miss() {
        let dir = std::env::temp_dir().join("cccc_cache_unit_invalidate");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.ts");
        std::fs::write(&src, "function f() { return 1 }").unwrap();
        let cache_file = dir.join("cache.bin");

        store_one(&cache_file, &src, "es");
        let cache = load(&cache_file).unwrap();
        assert!(
            cache.lookup(&src, "rust").is_none(),
            "other language misses"
        );

        // Same size, different content: the stat check passes on size, the
        // hash check must still reject it.
        std::fs::write(&src, "function f() { return 2 }").unwrap();
        assert!(
            cache.lookup(&src, "es").is_none(),
            "same-size edit misses via hash"
        );

        std::fs::write(&src, "function f() {}").unwrap();
        assert!(cache.lookup(&src, "es").is_none(), "resized file misses");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtime_only_change_hits_via_hash_and_refreshes() {
        let dir = std::env::temp_dir().join("cccc_cache_unit_touch");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.ts");
        std::fs::write(&src, "function f() {}").unwrap();
        let cache_file = dir.join("cache.bin");

        store_one(&cache_file, &src, "es");

        // Same bytes, new mtime — what a fresh git checkout does to every file.
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(bumped)
            .unwrap();

        let cache = load(&cache_file).unwrap();
        let hit = cache.lookup(&src, "es").expect("content match hits");
        assert!(hit.refreshed, "a hash hit asks for a rewrite");
        assert_eq!(hit.report.cognitive, 3);

        // Re-storing with the refreshed signature turns it back into a stat hit.
        store(&cache_file, &[(hit.report, Some(hit.sig))], &|_| Some("es"));
        let hit = load(&cache_file).unwrap().lookup(&src, "es").unwrap();
        assert!(!hit.refreshed, "refreshed mtime now stat-hits");

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
