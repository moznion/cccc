# Benchmark harness

Scripts to reproduce the numbers in [`../BENCHMARK.md`](../BENCHMARK.md):
cccc vs other TS/JS complexity tools (ESLint+SonarJS, lizard, scc).

## Quick start

```sh
cargo build --release          # build the cccc binary first
benchmark/setup.sh             # clone corpus + install comparison tools
python3 benchmark/run.py       # verify, then time + measure memory
```

## Files

| File | Purpose |
|------|---------|
| `setup.sh` | Clones the corpus (zod, shallow) and installs the comparison tools into `benchmark/.work/`. Idempotent. |
| `run.py` | Verify-then-time harness. Proves each tool actually processed the corpus before timing it; skips any tool that is missing or fails the proof check. |
| `eslintrc.json` | ESLint config enabling only `complexity` (cyclomatic) and `sonarjs/cognitive-complexity`. |

Everything the scripts download or build goes under `benchmark/.work/`, which is
git-ignored. Nothing is hard-coded to a machine path; `run.py` discovers the
`cccc` binary under `target/release` (or `target/debug`, or `$PATH`) and the
comparison tools via `$PATH` / the local `node_modules`.

## Options

```sh
python3 benchmark/run.py --runs 10                 # more samples (default 5)
python3 benchmark/run.py --corpus path/to/src      # benchmark a different tree
```

## Why the proof-of-work check

Several tools fail *silently* in ways that produce a fast-but-meaningless run: a
missing npm module, a wrong working directory, a parser that bails out. `run.py`
therefore requires a signature string in each tool's output (e.g. ESLint must
emit `"messages"`, cccc must emit `"function_count"`) before it will time that
tool. A tool that can't prove it did the work is reported as skipped, never
timed. This is why the published numbers can be trusted as a single, internally
consistent run.

## Notes

- `scc` measures a coarse, file-level keyword-count "complexity" (no AST, no
  cognitive). It's included as a fastest-possible native baseline, not a
  like-for-like tool.
- `lizard` computes cyclomatic only.
- Only cccc and ESLint+SonarJS compute *both* cognitive and cyclomatic
  complexity per function.
- `ts-complex` is intentionally not wired in: in testing it failed to parse the
  corpus correctly (27 functions across 286 files), so its numbers are not
  trustworthy. See `../BENCHMARK.md`.
- Peak RSS units differ by OS: bytes on macOS, kilobytes on Linux (`run.py`
  labels which).
```
