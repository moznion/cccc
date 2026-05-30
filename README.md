# cccc

A fast CLI that measures **Cognitive Complexity** (SonarSource / G. Ann Campbell)
and **Cyclomatic Complexity** (McCabe) of TypeScript and JavaScript code.

Written in Rust, using the [oxc](https://oxc.rs) parser. Supports `.ts`, `.tsx`,
`.js`, `.jsx`, `.mts`, `.cts`, `.mjs`, `.cjs`.

## Install / build

```sh
cargo build --release
# binary at ./target/release/cccc
```

## Usage

```sh
cccc <paths...> [options]
```

Output is **JSON by default**. Pass one or more files or directories;
directories are walked recursively (respecting `.gitignore`, always skipping
`node_modules`).

### Options

| Flag | Description |
|------|-------------|
| `--table` | Human-readable table instead of JSON |
| `--ext ts,tsx,...` | Override the set of analyzed extensions |
| `--max-cognitive N` | Exit non-zero if any function's cognitive complexity exceeds N |
| `--max-cyclomatic N` | Exit non-zero if any function's cyclomatic complexity exceeds N |
| `--min N` | Only report functions with complexity >= N |
| `--top-cognitive N` | Show only the N most cognitively-complex functions, as a flat cross-file ranking |
| `--top-cyclomatic N` | Show only the N most cyclomatically-complex functions, as a flat cross-file ranking |
| `--no-ignore` | Do not respect `.gitignore` when walking directories |
| `-j, --jobs N` | Number of files to analyze in parallel (default: logical CPU count) |

`--top-cognitive` and `--top-cyclomatic` are mutually exclusive. In top mode the
output is a ranking (`{ "metric", "top": [...], "summary" }`) instead of the
per-file `files` array; each entry carries its own `path` and `line`. The
`summary` still reflects the full population.

### Examples

```sh
# JSON for one file
cccc src/app.ts

# Pretty table for a directory
cccc --table src/

# CI gate: fail if any function exceeds cognitive complexity 15
cccc --max-cognitive 15 src/

# The 10 most cognitively-complex functions across the project
cccc --top-cognitive 10 src/

# Limit parallelism to 4 workers (default is the logical CPU count)
cccc -j 4 src/
```

Files are analyzed in parallel. The worker count defaults to the number of
logical CPUs and can be capped with `-j/--jobs`; the output is identical
regardless of the worker count.

## Output shape (JSON)

An object with `files` (per-file reports) and `summary` (a whole-project
rollup). Each function is measured independently and nested functions appear
under `children`. A file's totals sum every function at every depth plus
module-level code.

The `summary` is computed over every function in every file (all nesting
depths). Because complexity is right-skewed, it reports the distribution
(`sum`/`max`/`median`/`p90`/`p95`) rather than a mean — the percentiles describe
the tail where refactoring candidates live. It is unaffected by `--min`.

```json
{
  "files": [
    {
      "path": "src/app.ts",
      "cognitive": 10,
      "cyclomatic": 10,
      "functions": [
        {
          "name": "handleRequest",
          "kind": "function",
          "line": 10,
          "cognitive": 7,
          "cyclomatic": 4,
          "children": []
        }
      ]
    }
  ],
  "summary": {
    "file_count": 1,
    "function_count": 3,
    "cognitive":  { "sum": 10, "max": 7, "median": 2, "p90": 7, "p95": 7 },
    "cyclomatic": { "sum": 10, "max": 4, "median": 3, "p90": 4, "p95": 4 }
  }
}
```

> Note: the top level is an object (`{ files, summary }`), so to post-process
> the per-file array with `jq`, start from `.files` — e.g.
> `cccc src/ | jq '.files | sort_by(-.cognitive)'`.

## Metric rules

**Cyclomatic (McCabe):** base 1 per function; +1 for each `if`/`else if`,
ternary, `for`/`for-in`/`for-of`/`while`/`do-while`, `case` (excluding
`default`), `catch`, and each `&&`/`||`/`??`.

**Cognitive (SonarSource):**
- +1 plus a nesting bonus for `if`, ternary, `switch`, loops, `catch`.
- +1 flat (no bonus) for `else`/`else if`, labelled `break`/`continue`, each
  run of like logical operators, and recursion (call to the enclosing
  function's own name).
- Nesting increases inside control-flow bodies and nested function bodies.

Each function-like unit is scored independently (nesting resets to 0 at the
function boundary); nested functions are reported as children rather than
inflating the parent's own score.
