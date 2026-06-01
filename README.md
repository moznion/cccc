# cccc - A tool/library for measurement of **C**ognitive **C**omplexity and **C**yclomatic **C**omplexity

- A fast CLI that measures **Cognitive Complexity** (SonarSource / G. Ann Campbell)
  and **Cyclomatic Complexity** (McCabe) of TypeScript and JavaScript code.
  - Written in Rust, using the [oxc](https://oxc.rs) parser. Supports `.ts`, `.tsx`,
    `.js`, `.jsx`, `.mts`, `.cts`, `.mjs`, `.cjs`.
- A Rust library for calculating cognitive and cyclomatic complexity in a language-agnostic way

## Workspace layout

The complexity engine is split from the language parser so it can be reused as a
library and extended to other languages:

| Crate | Role |
|-------|------|
| [`cccc-core`](crates/cccc-core) | Language-agnostic engine: a normalized IR (`ir::Node`), the scoring rules (`engine::analyze`), and the result/aggregation types. Depends only on `serde`. |
| [`cccc-cli`](crates/cccc-cli) | Shared CLI machinery (argument parsing, file walking, parallelism, output rendering) as a library. A front-end calls `cccc_cli::run(bin_name, version, analyze_fn, default_exts)`. |
| [`cccc-es`](crates/cccc-es) | ECMAScript/TypeScript adapter **library**: lowers the oxc AST into `cccc-core`'s IR. Depends only on `cccc-core` + oxc — **no CLI dependencies**, so embedding it stays lightweight. |
| [`cccc-es-cli`](crates/cccc-es-cli) | The **`cccc-es`** binary: a thin shell that wires the `cccc-es` adapter into the shared `cccc-cli` runner. |

The adapter and the binary are separate crates so that a library consumer who
only wants the metrics pulls in just `cccc-es` (+ `cccc-core` + oxc), never clap
/ ignore / rayon.

To support another language: (1) add an adapter crate that lowers its AST into
`cccc_core::ir::Node` and calls `cccc_core::engine::analyze`, then (2) add a tiny
binary crate whose `main` calls
`cccc_cli::run(env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"), analyze_source, DEFAULT_EXTS)`
— no need to reimplement either the metrics or the CLI.

**See [docs/ADDING_A_LANGUAGE.md](docs/ADDING_A_LANGUAGE.md) for the full
step-by-step guide**, including the IR-node reference table, the
logical-operator folding rule, and how to test the adapter.

```rust
use cccc_core::{engine::analyze, ir::Node};

let f = Node::Function {
    name: "f".into(), kind: "function".into(), line: 1,
    body: vec![Node::Branch { test: vec![], then: vec![], alternate: None }],
};
let report = analyze("example", &[f], vec![]);
assert_eq!(report.functions[0].cognitive, 1);  // one `if`
```

## Install / build

```sh
cargo build --release
# binary at ./target/release/cccc-es
```

## Usage

```sh
cccc-es <paths...> [options]
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
cccc-es src/app.ts

# Pretty table for a directory
cccc-es --table src/

# CI gate: fail if any function exceeds cognitive complexity 15
cccc-es --max-cognitive 15 src/

# The 10 most cognitively-complex functions across the project
cccc-es --top-cognitive 10 src/

# Limit parallelism to 4 workers (default is the logical CPU count)
cccc-es -j 4 src/
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
> `cccc-es src/ | jq '.files | sort_by(-.cognitive)'`.

## Benchmark

On [zod](https://github.com/colinhacks/zod)'s `packages/zod/src` (286 `.ts`
files, 68,357 LOC), median wall-clock and peak memory over 5 runs on an Apple
M4 Pro:

| Tool | Metrics | Time | Peak RSS |
|------|---------|-----:|---------:|
| **cccc** (`cccc-es`) | cognitive + cyclomatic, per-function, full AST | **15.5 ms** | **12.5 MB** |
| ESLint + SonarJS | cognitive + cyclomatic, per-function, full AST | 1,807 ms (**117× slower**) | 604 MB (48× more) |
| lizard | cyclomatic only, heuristic parser | 1,413 ms (91× slower) | 45.7 MB |
| scc | coarse per-file keyword count, no AST | 8.3 ms (1.9× faster) | 13.9 MB |

Among tools that do the same job — both metrics, per-function, over a real AST —
cccc is **~117× faster than ESLint+SonarJS** (the only other tool that computes
cognitive complexity) and uses ~48× less memory. `scc` is faster only because it
never parses: it counts keywords per file, with no AST, no per-function data, and
no cognitive complexity.

See **[BENCHMARK.md](BENCHMARK.md)** for the full methodology, the verify-then-time
harness, per-run numbers, function-count sanity checks, and caveats.

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
