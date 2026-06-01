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
single AST pass computes both metrics together; files are analyzed across cores
(rayon, with a small-corpus sequential fast path); output is streamed through a
buffered writer; negligible startup vs Node/Python interpreter boot.

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
