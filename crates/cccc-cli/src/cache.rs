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
//! files once. In a git worktree even that read can be skipped: a churned file
//! that git's own index calls clean is validated against the index's blob SHA
//! (see [`GitIndex`]). Each entry also records the canonical name of the language that
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
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use cccc_core::report::{FileReport, FunctionReport};

/// Entries are only valid for the exact version that wrote them.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Validation signature for one file: stat facts for the cheap check, a
/// content hash as ground truth when the mtime has moved, and the content's
/// git blob name for the index fast path (see [`git_index`]).
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sig {
    size: u64,
    mtime_ns: u128,
    hash: u128,
    git_sha: [u8; 20],
}

/// The signature of content `bytes` observed at `mtime_ns`.
pub fn sig_for(bytes: &[u8], mtime_ns: u128) -> Sig {
    Sig {
        size: bytes.len() as u64,
        mtime_ns,
        hash: xxhash_rust::xxh3::xxh3_128(bytes),
        git_sha: git_blob_sha1(bytes),
    }
}

/// The SHA-1 git would name a blob holding `bytes` — computed here from the
/// bytes themselves, so a stored signature never depends on git having been
/// right about a file at store time.
fn git_blob_sha1(bytes: &[u8]) -> [u8; 20] {
    let mut h = sha1_smol::Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    h.digest().bytes()
}

/// Blob SHAs of the clean tracked files under one git worktree: what each
/// file's content is, answered from git's own index without reading the file.
///
/// A fresh CI checkout resets every mtime, which would push the whole tree
/// onto the content-hash fallback. But that checkout also wrote git's index,
/// which already names each file's blob — and `git status` confirms, by stat,
/// which files still match it. A lookup whose stat check failed on the mtime
/// alone and whose stored blob SHA matches the index's is a hit without
/// reading the file.
///
/// Trust runs content-to-content: stored SHAs are computed by cccc from the
/// analyzed bytes (see [`sig_for`]), and the index vouches for the checked-out
/// bytes, so a match never assumes the cache and the checkout describe the
/// same commit. Files git reports dirty are dropped from the map, and any git
/// failure — no repository, no git binary, a SHA-256 repository — yields
/// `None`; both just fall back to stat/hash validation.
pub struct GitIndex {
    shas: HashMap<PathBuf, [u8; 20]>,
}

impl GitIndex {
    /// The index's blob SHA for `path`, if it is a clean tracked file.
    fn sha_of(&self, path: &Path) -> Option<[u8; 20]> {
        self.shas.get(&std::path::absolute(path).ok()?).copied()
    }
}

/// A [`GitIndex`] materialized only if a lookup actually needs it.
/// Stat-validated hits never consult git, so a fully warm local run pays
/// nothing — not even the subprocesses, whose `git status` is a stat pass
/// over the whole tree that would contend with our own.
///
/// Under `CI=…` (every major CI service sets it) the subprocesses instead
/// start eagerly on a background thread: there the checkout has reset every
/// mtime, git is needed with near-certainty, and starting it before file
/// discovery hides its latency entirely. Either way, the first lookup that
/// sees a moved mtime waits for the answer — its alternative was reading and
/// hashing the file, strictly more work than the wait once amortized over
/// every churned file.
pub struct LazyGitIndex {
    index: std::sync::OnceLock<Option<GitIndex>>,
    source: std::sync::Mutex<Option<GitIndexSource>>,
}

enum GitIndexSource {
    /// Not started; the worktree root to read if anyone asks.
    Pending(PathBuf),
    /// Already reading on a background thread (the CI path).
    Running(std::thread::JoinHandle<Option<GitIndex>>),
}

impl LazyGitIndex {
    /// Prepare to read the git index of the worktree containing `root`.
    pub fn new(root: PathBuf) -> Self {
        let source = if std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "false") {
            GitIndexSource::Running(std::thread::spawn(move || git_index(&root)))
        } else {
            GitIndexSource::Pending(root)
        };
        Self {
            index: std::sync::OnceLock::new(),
            source: std::sync::Mutex::new(Some(source)),
        }
    }

    fn get(&self) -> Option<&GitIndex> {
        self.index
            .get_or_init(
                || match self.source.lock().ok().and_then(|mut s| s.take())? {
                    GitIndexSource::Pending(root) => git_index(&root),
                    GitIndexSource::Running(handle) => handle.join().ok().flatten(),
                },
            )
            .as_ref()
    }
}

/// Read the git index of the worktree containing `root`.
pub fn git_index(root: &Path) -> Option<GitIndex> {
    let toplevel = PathBuf::from(git(root, &["rev-parse", "--show-toplevel"])?.trim_end());
    // `ls-files -s`: "<mode> <sha> <stage>\t<path>", NUL-terminated. Keep
    // regular blobs at stage 0: gitlinks and symlinks aren't analyzable files,
    // a nonzero stage is an unmerged path, and a SHA that isn't 40 hex digits
    // means a SHA-256 repository, whose blob names can never match ours.
    let mut shas = HashMap::new();
    for entry in git(root, &["ls-files", "-s", "-z"])?
        .split('\0')
        .filter(|e| !e.is_empty())
    {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split(' ');
        let (Some(mode), Some(sha), Some("0")) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !mode.starts_with("100") {
            continue;
        }
        let Some(sha) = decode_sha1(sha) else {
            continue;
        };
        shas.insert(toplevel.join(path), sha);
    }
    // `status --porcelain -z`: "XY <path>", NUL-terminated. Anything listed is
    // not clean; whatever the reason, its index SHA can't be trusted.
    for entry in git(
        root,
        &["status", "--porcelain", "-z", "-uno", "--no-renames"],
    )?
    .split('\0')
    .filter(|e| !e.is_empty())
    {
        let Some(path) = entry.get(3..) else { continue };
        shas.remove(&toplevel.join(path));
    }
    Some(GitIndex { shas })
}

/// Run git in `dir` and return its stdout, `None` on any failure.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn decode_sha1(hex: &str) -> Option<[u8; 20]> {
    let hex = hex.as_bytes();
    if hex.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, b) in out.iter_mut().enumerate() {
        let hi = (hex[2 * i] as char).to_digit(16)?;
        let lo = (hex[2 * i + 1] as char).to_digit(16)?;
        *b = (hi * 16 + lo) as u8;
    }
    Some(out)
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
    /// signature — by stat, by git's index, or by content hash when only the
    /// mtime moved — and is still analyzed by `lang`.
    pub fn lookup(&self, path: &Path, lang: &str, git: Option<&LazyGitIndex>) -> Option<Hit> {
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
        } else if git.and_then(|g| g.get()).and_then(|g| g.sha_of(path)) == Some(entry.sig.git_sha)
        {
            // The mtime moved but git's index vouches that the file still
            // holds the very blob we analyzed — no read needed. (Only now is
            // the git subprocess awaited: an all-stat-hit run never blocks
            // on it.)
            Sig {
                mtime_ns: mtime,
                ..entry.sig
            }
        } else {
            // The mtime moved (checkout, touch, rewrite-in-place) and git has
            // nothing to say; the hash decides. Size is re-checked from the
            // bytes actually read, in case the file changed again between the
            // stat and the read.
            let sig = sig_for(&std::fs::read(path).ok()?, mtime);
            if sig.size != entry.sig.size || sig.hash != entry.sig.hash {
                return None;
            }
            sig
        };
        Some(Hit {
            report: self.decode(entry)?,
            refreshed: sig.mtime_ns != entry.sig.mtime_ns,
            sig,
        })
    }

    /// Decode `entry`'s blob into its report.
    fn decode(&self, entry: &IndexEntry) -> Option<FileReport> {
        let blob = self
            .blobs
            .get(entry.offset as usize..(entry.offset + entry.len) as usize)?;
        let report: CacheReport = bincode::deserialize(blob).ok()?;
        Some(from_cache(report))
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
        let hit = cache.lookup(&src, "es", None).expect("unchanged file hits");
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
            cache.lookup(&src, "rust", None).is_none(),
            "other language misses"
        );

        // Same size, different content: the stat check passes on size, the
        // hash check must still reject it.
        std::fs::write(&src, "function f() { return 2 }").unwrap();
        assert!(
            cache.lookup(&src, "es", None).is_none(),
            "same-size edit misses via hash"
        );

        std::fs::write(&src, "function f() {}").unwrap();
        assert!(
            cache.lookup(&src, "es", None).is_none(),
            "resized file misses"
        );

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
        let hit = cache.lookup(&src, "es", None).expect("content match hits");
        assert!(hit.refreshed, "a hash hit asks for a rewrite");
        assert_eq!(hit.report.cognitive, 3);

        // Re-storing with the refreshed signature turns it back into a stat hit.
        store(&cache_file, &[(hit.report, Some(hit.sig))], &|_| Some("es"));
        let hit = load(&cache_file).unwrap().lookup(&src, "es", None).unwrap();
        assert!(!hit.refreshed, "refreshed mtime now stat-hits");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_blob_sha1_matches_git() {
        // `git hash-object` of an empty file and of "hello\n".
        let hex = |sha: [u8; 20]| sha.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex(git_blob_sha1(b"")),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            hex(git_blob_sha1(b"hello\n")),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn git_index_vouches_for_clean_files_only() {
        let dir = std::env::temp_dir().join("cccc_cache_unit_gitindex");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // `git rev-parse --show-toplevel` resolves symlinks (macOS /tmp);
        // canonicalize so our paths agree with git's.
        let dir = dir.canonicalize().unwrap();
        let src = dir.join("a.ts");
        std::fs::write(&src, "function f() {}").unwrap();
        let run_git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        run_git(&["init", "-q"]);
        run_git(&["add", "."]);
        run_git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "x",
        ]);

        let cache_file = dir.join("cache.bin");
        store_one(&cache_file, &src, "es");
        let cache = load(&cache_file).unwrap();

        // An mtime-only change: the git index vouches without reading the
        // file. Dropping read permission proves the hit really came from git —
        // the content-hash fallback would need to open the file. The mtime
        // moves into the past: a future mtime would trip git's racy-clean
        // check, which re-reads the file on every status.
        let bumped = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(bumped)
            .unwrap();
        // Re-sync the index's stat cache (as a checkout would have), so `git
        // status` can call the file clean without re-reading it.
        run_git(&["update-index", "--refresh"]);
        #[cfg(unix)]
        let perms = {
            use std::os::unix::fs::PermissionsExt;
            let orig = std::fs::metadata(&src).unwrap().permissions();
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
            orig
        };
        let git = LazyGitIndex::new(dir.clone());
        let hit = cache
            .lookup(&src, "es", Some(&git))
            .expect("clean file hits via git");
        assert!(hit.refreshed, "a git hit persists the fresh mtime");
        assert_eq!(hit.report.cognitive, 3);
        #[cfg(unix)]
        std::fs::set_permissions(&src, perms).unwrap();

        // A dirty file drops out of the map and must fail validation (the
        // content changed, so the stat/hash fallback rejects it too).
        std::fs::write(&src, "function f() { return 9 }").unwrap();
        let git = LazyGitIndex::new(dir.clone());
        assert!(
            cache.lookup(&src, "es", Some(&git)).is_none(),
            "dirty file must not hit via the stale index SHA"
        );

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
