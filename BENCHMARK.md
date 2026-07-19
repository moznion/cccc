# Benchmark: cccc vs other complexity tools

cccc (Rust + oxc) compared against other tools that measure cyclomatic and/or
cognitive complexity of TS/JS code. All four tools were run in a single harness
(`timeit.py`) that **verifies each tool actually processed the corpus** (a
proof-of-work check on its output) before timing it — so every number below
comes from one internally consistent run.

## Tools compared

| Tool | Lang/runtime | Metrics | Granularity | Parser |
|------|--------------|---------|-------------|--------|
| **cccc** | Rust (native) | cognitive + cyclomatic | per-function | oxc (full TS/JS AST) |
| ESLint + @typescript-eslint + eslint-plugin-sonarjs | Node | cognitive + cyclomatic | per-function | full TS AST |
| lizard | Python | cyclomatic (CCN) | per-function | heuristic multi-language |
| scc | Go (native) | coarse "complexity" | per-file | keyword count, no AST |

Only **cccc** and **ESLint+SonarJS** compute *both* cognitive and cyclomatic
complexity. lizard does cyclomatic only. scc reports a coarse file-level
keyword-count "complexity" (no AST, no per-function data, no cognitive) —
included as a "fastest-possible native baseline", not a like-for-like tool.

**ts-complex was evaluated but excluded**: it returned only 27 functions across
all 286 files (its TS-compiler-API path did not parse this codebase correctly),
so its numbers are not trustworthy and are not reported.

## Corpus

[zod](https://github.com/colinhacks/zod) `packages/zod/src`, excluding `*.d.ts`:
**286 `.ts` files, 68,357 lines.** Function counts as a sanity check:

| Tool | functions | note |
|------|----------:|------|
| cccc | 6,197 | incl. module-level units |
| ESLint (`complexity`) | 6,174 | within 0.4% of cccc |
| lizard | 5,790 | heuristic parser |
| scc | n/a | file-level only |

## Environment

Apple M4 Pro (12 cores), macOS. rustc 1.93.0 (release, `lto=true`),
Node v24.16.0, scc 3.x (Homebrew), Python 3.9.6 / lizard 1.22.2,
ESLint 8.57.0, @typescript-eslint/parser 8.x, eslint-plugin-sonarjs 4.0.3.
5 measured runs after 1 warmup; timed in-process via `perf_counter`, peak RSS
via `wait4`.

## Results — wall-clock, full corpus (286 files / 68,357 LOC)

Per-run (ms): cccc `15.3 16.5 15.6 15.1 15.5`; scc `8.5 8.0 8.3 8.4 8.0`;
lizard `1405.8 1416.2 1413.0 1414.7 1397.8`;
eslint `1806.7 1786.0 1788.2 1807.4 1838.0`.

| Tool | median | vs cccc |
|------|-------:|--------:|
| scc | **8.3 ms** | 1.9× faster |
| **cccc** | **15.5 ms** | baseline |
| lizard | **1,413.0 ms** | 91× slower |
| ESLint+SonarJS | **1,806.7 ms** | 117× slower |

## Results — peak memory, full corpus

| Tool | peak RSS | vs cccc |
|------|---------:|--------:|
| **cccc** | 12.5 MB | 1.0× |
| scc | 13.9 MB | 1.1× |
| lizard | 45.7 MB | 3.7× |
| ESLint+SonarJS | 604.0 MB | 48.5× |

## Reading the results

- **Among tools that do the same job** (both metrics, per-function, real AST),
  cccc is by far the fastest — **~117× faster than ESLint+SonarJS**, the only
  other tool computing cognitive complexity — and uses ~48× less memory.
- **scc is ~2× faster than cccc** but does far less: no AST, no per-function
  data, no cognitive complexity — just a per-file keyword count. Right for a
  rough whole-repo sweep, not for function-level cognitive/cyclomatic numbers.
  Its memory is comparable to cccc (both native).
- **lizard** (cyclomatic only, heuristic parser) is ~91× slower than cccc while
  computing only one of the two metrics.

## Why cccc is fast

Native Rust binary over the oxc parser (one of the fastest JS/TS parsers); a
single AST pass computes both metrics together; file discovery fans out across
cores (the same parallel walker ripgrep uses) and so does analysis (rayon, with
a small-corpus sequential fast path); output is streamed through a buffered
writer; negligible startup vs Node/Python interpreter boot.

## Why cccc is (a bit) slower than scc

scc is ~5× faster because it does a categorically smaller job: a per-file
keyword count, no tokenizer and no AST. cccc tokenizes and builds oxc's full
typed AST for every file, then walks it — which is the only way to compute
Cognitive Complexity at all. Per file that is ~0.08 ms, fast for a real parser;
scc simply never parses. The gap is a difference in work, not in efficiency, and
closing it fully is impossible without dropping the AST (and the metrics).

Profiling the remaining cost found two things worth noting:

- **Startup (~1.5 ms) is almost entirely the OS exec + dyld + Rust-runtime floor
  shared by every native binary** — only ~0.1 ms is cccc-specific (clap's command
  tree). There is no meaningful win to chase there. Earlier "~2.6 ms startup"
  figures were inflated by a Python `subprocess` measurement harness; measured
  with a low-overhead spawner it is ~1.5 ms.
- **Output rendering** was the one avoidable cost: the table view now streams
  through a single buffered, locked writer instead of a `println!` per row
  (~6,000 rows), cutting `--table` on the zod corpus from ~17 ms to ~14 ms. JSON
  is serialized straight into the buffered writer (no intermediate `String`).

## Update (2026-07-18): parallel file discovery

File discovery used to walk the tree on one thread and pay two avoidable
syscalls per file: a `canonicalize` (only needed to dedup overlapping roots)
and a second `stat` to confirm the entry is a file. The walk now fans out
across the `--jobs` worker count using `ignore`'s parallel walker (the one
ripgrep uses), the file-type check reuses what the walker already knows, and
canonicalization runs only when multiple roots could actually overlap. Output
is byte-identical: reports are sorted by path before rendering, so the
nondeterministic walk order never reaches the user.

Measured on the same machine (Apple M4 Pro, 12 cores), median of 5 runs after
1 warmup, default flags:

| corpus | files | before | after | speedup |
|--------|------:|-------:|------:|--------:|
| zod `packages/zod/src` (TS) | 286 | 15.5 ms | **12.6 ms** | 1.2× |
| VS Code (TS/JS, ~0.7M LOC) | 2,976 | 216 ms | **108 ms** | 2.0× |
| Kubernetes (Go, ~5.2M LOC) | 17,518 | 1,711 ms | **914 ms** | 1.9× |
| PostgreSQL (C, ~1.8M LOC) | 2,953 | 777 ms | **547 ms** | 1.4× |

The gain grows with tree size because discovery is a fixed cost paid before
any parsing starts: on Kubernetes the walk alone dropped from ~453 ms to
~109 ms. The four-tool comparison tables above still show the original
15.5 ms figure — those numbers come from one internally consistent run of the
comparison harness, and only cccc has been re-measured here.

## Caveats

- ESLint is a general-purpose linter; this measures the cost of getting these two
  numbers, not overall tool quality. scc and lizard likewise have different
  scopes.
- Tools differ in counting-rule edge cases, so scores are not byte-identical;
  AST-based function counts agree within ~0.4%, lizard differs more (heuristic
  definition of "a function").
- Single machine (Apple M4 Pro); absolute values vary by hardware, but the
  relative order is stable.

## Reproduce

Harness in `/tmp/cccc-bench`: zod clone, `.eslintrc.json`, `timeit.py` (the
verify-then-time script). Build cccc with `cargo build --release`; install scc
(`brew install scc`), lizard (`pip install lizard`), and the Node tools
(`npm install eslint@8 @typescript-eslint/parser eslint-plugin-sonarjs`). Run
`python3 timeit.py`.

---

# Benchmark: the results cache (`--cache`)

The results cache reuses the previous run's per-file scores for unchanged
files (still validated per entry against the analyzing language and the cccc
version), re-analyzing only what changed. As first shipped, "unchanged" meant
size/mtime alone; the update at the end of this file reworks validation to
survive mtime churn (a fresh CI checkout, a branch switch). These numbers
size what the cache buys on real monorepos, per language front-end.

## Corpora

Shallow clones, analyzed at the repository root:

| Corpus | Front-end | Files | Functions |
|--------|-----------|------:|----------:|
| [microsoft/vscode](https://github.com/microsoft/vscode) (TS/JS) | oxc | 2,976 | 49,087 |
| [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (Go) | gosyn | 17,518 | 214,354 |
| [postgres/postgres](https://github.com/postgres/postgres) (C) | tree-sitter | 2,953 | 27,299 |

## Method

Same machine as above (Apple M4 Pro, 12 cores; release build). Each cell is the
median of 5 runs after 1 warmup, stdout to `/dev/null`. Scenarios:

- **cold** — no cache (`cccc <repo>`).
- **warm, all hits** — a populated cache, nothing changed.
- **warm + `--pretty`** — same, with pretty-printed instead of the default
  single-line JSON.
- **1% changed** — before every run, 1% of the files (26 / 175 / 25) are
  appended to, so each run re-analyzes those and reuses the rest.

Cached output was verified byte-for-byte identical to a cold run on all three
corpora before timing.

## Results — wall clock, median ms

| Corpus | cold | warm (all hits) | warm + `--pretty` | 1% changed | warm vs cold |
|--------|-----:|----------------:|------------------:|-----------:|-------------:|
| vscode (TS/JS) | 101.0 | 37.2 | 43.1 | 44.7 | **2.7×** |
| kubernetes (Go) | 868.2 | 168.6 | 187.3 | 212.0 | **5.1×** |
| postgres (C) | 549.3 | 31.1 | 33.3 | 67.5 | **17.7×** |

Cache file sizes: vscode 3.9 MB, kubernetes 20 MB, postgres 3.1 MB.

## Reading the results

- **The win scales with per-file parse cost.** tree-sitter front-ends (C here;
  also Java, Kotlin, Python, Swift, Dart, Perl) parse ~5× slower per line than
  oxc, so they gain the most (~18×). oxc is fast enough that TS/JS gains only
  ~2.7× — the remaining time isn't analysis at all.
- **Warm runs are floored by discovery + output, not by the cache.** Phase
  timing on the kubernetes warm run put the gitignore-aware directory walk at
  ~100 ms and JSON rendering at ~28 ms (~50 ms with `--pretty`), while all
  cache work combined — loading the index, 17.5k stats, decoding hit entries —
  is ~26 ms. The cache's own format matters little at this point: only the
  index is decoded on load, and each hit decodes just its own entry, in
  parallel.
- **The realistic steady state is "a few files changed".** That scenario stays
  well below the cold cost on every corpus (1.2–2.2× the all-hit floor):
  invalidation is stat-only, and re-analysis is proportional to what changed
  (plus one cache rewrite).
- **When not to bother:** small trees (a cold zod-sized run is already ~15 ms)
  and one-shot CI runs where restoring/saving a 4–20 MB cache artifact costs
  more than the 0.1–0.7 s it saves. The cache is off by default for a reason;
  it pays off for repeated local runs — watch loops, pre-commit hooks, editor
  integrations — on large trees.

## Reproduce

```sh
cargo build --release
git clone --depth 1 https://github.com/kubernetes/kubernetes /tmp/k8s
# cold
target/release/cccc --no-config /tmp/k8s > /dev/null
# populate, then warm
target/release/cccc --no-config --cache-file /tmp/k8s.cache /tmp/k8s > /dev/null
target/release/cccc --no-config --cache-file /tmp/k8s.cache /tmp/k8s > /dev/null
```

## Update (2026-07-19): validation reworked — stat, then git's index, then the blob SHA

The size/mtime check above has a blind spot: anything that rewrites mtimes
without changing content. The big one is CI — `git clone`/`actions/checkout`
stamp every file with the checkout time, so a cache restored by
`actions/cache` would never hit. Measured with every mtime bumped but no
content changed, the mtime-only cache was **worse than no cache at all**: it
re-analyzed everything *and* rewrote the whole cache file — vscode 127.5 ms
vs 107.8 ms cold, postgres 576.6 ms vs 544.5 ms cold.

Validation is now a cheapest-first chain (details in the README and
`cache.rs`). Each entry stores size, mtime, and the SHA-1 git would name a
blob of the content — computed by cccc from the bytes it analyzed, never
taken on faith from git, so every check is content-to-content and it never
matters which commit (or how stale a run) a restored cache came from:

1. **stat** — size+mtime match: trusted with no read (the local fast path).
2. **git's index** — mtime moved but git calls the file clean and the
   index's blob SHA matches the stored one: still no read. One
   `git ls-files -s`/`git status` pair answers for the whole tree. git is
   consulted lazily, only after some stat check failed — an all-stat-hit
   run doesn't even spawn it (an eager-spawn draft cost 15–30 ms of `git
   status` contention on big trees) — except under `CI=…`, where it starts
   ahead of file discovery to hide its latency.
3. **blob SHA re-derived from the bytes** — the final authority, for
   whatever git can't vouch: dirty/untracked files, non-git trees, filtered
   (e.g. CRLF-normalized) files, any git failure at all.

A hit via 2 or 3 persists the refreshed mtime, so churn converges to stat
hits after one run. On the choice of hash: candidates were benchmarked at
~48 GB/s (xxh3-128), ~12 GB/s (hw CRC32), and ~1.3 GB/s (SHA-1), but the
re-derive path is I/O-bound — ~0.6–1.5 GB/s effective including opens — so
even the slowest never reaches the wall clock, and the SHA-1 git already
requires is the only candidate that also serves the index check. One
content identity, one hash dependency (`sha1_smol`).

### Results

Corpora as above, except vscode: fixing this machine's partial LFS checkout
grew that tree to its true size, **12,301 analyzed files** (earlier sections
measured the 2,976-file subset; their numbers remain internally consistent).
Same machine and method (median of 5 after 1 warmup, stdout to `/dev/null`).
"CI churn" bumps every mtime and re-syncs git's stat cache before each run —
the state `actions/checkout` leaves — with `CI=true`; "hash only" is the
same churn with git made unavailable; "local churn" is a branch-switch-like
state without `CI` (git started on demand):

| Corpus | cold | warm (stat) | CI churn, git | CI churn, hash only | local churn, git |
|--------|-----:|------------:|--------------:|--------------------:|-----------------:|
| vscode (TS/JS, 12,301) | 345.0 | 149.1 | **225.3** | 399.9 | 259.6 |
| kubernetes (Go, 17,518) | 992.7 | 181.7 | **307.2** | 651.6 | 364.2 |
| postgres (C, 2,954) | 525.3 | 33.6 | **56.2** | 92.9 | 75.0 |

Reading it:

- **CI goes from net-negative to a real win**: 1.5× (vscode — oxc parses
  nearly as fast as re-validating), 3.2× (kubernetes), 9.3× (postgres)
  faster than cold, and 1.7–2.1× faster than re-hashing the tree.
- **A fully warm local run pays exactly nothing for any of it**: stat hits
  never read a file and never consult (or spawn) git — warm medians measure
  equal with git present or absent.
- **Local mtime churn benefits too**: a branch switch validates off git on
  demand and still beats re-hashing the tree.
- Outputs stay byte-for-byte identical to cold runs in every scenario
  (verified on all three corpora before timing), and churn converges: the
  run after the rewrite is back to pure stat hits with no cache write.

### Reproduce

```sh
cargo build --release
git clone --depth 1 https://github.com/kubernetes/kubernetes /tmp/k8s
target/release/cccc --no-config --cache-file /tmp/k8s.cache /tmp/k8s > /dev/null  # populate
# Emulate a fresh CI checkout: bump every mtime, re-sync git's stat cache.
find /tmp/k8s -type f -not -path '*/.git/*' -print0 | xargs -0 touch
git -C /tmp/k8s update-index --refresh -q
CI=true target/release/cccc --no-config --cache-file /tmp/k8s.cache /tmp/k8s > /dev/null
```
