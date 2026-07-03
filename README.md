# cccc - A tool/library for measurement of **C**ognitive **C**omplexity and **C**yclomatic **C**omplexity

- A fast CLI — a **single `cccc` binary** — that measures **Cognitive Complexity**
  (SonarSource / G. Ann Campbell) and **Cyclomatic Complexity** (McCabe). Written
  in Rust. It routes each file to the right front-end by its extension, so one run
  can analyze a mixed-language tree. Various languages ship today, all sharing the
  same engine, flags, and output format:
  - **TypeScript / JavaScript** (`--lang es`), via the [oxc](https://oxc.rs)
    parser. Analyzes `.ts`, `.tsx`, `.js`, `.jsx`, `.mts`, `.cts`, `.mjs`, `.cjs`.
  - **Rust** (`--lang rust`), via the [syn](https://docs.rs/syn) parser. `.rs`.
  - **Go** (`--lang go`), via the [gosyn](https://docs.rs/gosyn) parser. `.go`.
  - **PHP** (`--lang php`), via the [php-rs-parser](https://docs.rs/php-rs-parser)
    parser. `.php`.
  - **Ruby** (`--lang ruby`), via the [ruby-prism](https://docs.rs/ruby-prism)
    parser (Ruby's official Prism parser). `.rb`.
  - **Scheme** (`--lang scheme`), R7RS-small, via the
    [lispexp](https://docs.rs/lispexp) S-expression reader. `.scm`, `.ss`, `.sld`.
  - **Common Lisp** (`--lang commonlisp`), via the
    [lispexp](https://docs.rs/lispexp) S-expression reader. `.lisp`, `.lsp`, `.cl`.
  - **Emacs Lisp** (`--lang emacslisp`), via the
    [lispexp](https://docs.rs/lispexp) S-expression reader. `.el`.
  - **Clojure** (`--lang clojure`), via the
    [lispexp](https://docs.rs/lispexp) S-expression reader. `.clj`, `.cljs`, `.cljc`.
- A Rust library for calculating cognitive and cyclomatic complexity in a language-agnostic way

## Workspace layout

The complexity engine is split from the language parser so it can be reused as a
library and extended to other languages:

| Crate | Role |
|-------|------|
| [`cccc-core`](crates/cccc-core) | Language-agnostic engine: a normalized IR (`ir::Node`), the scoring rules (`engine::analyze`), and the result/aggregation types. Depends only on `serde`. |
| [`cccc-cli`](crates/cccc-cli) | The unified **`cccc` binary**. Owns argument parsing, config-file handling, file walking, parallelism, and output rendering, and holds the registry of bundled languages (`lang::LANGUAGES`) that routes each file to its adapter. |
| [`cccc-es`](crates/cccc-es) | ECMAScript/TypeScript adapter **library**: lowers the oxc AST into `cccc-core`'s IR. Depends only on `cccc-core` + oxc — **no CLI dependencies**, so embedding it stays lightweight. |
| [`cccc-rs`](crates/cccc-rs) | Rust adapter **library**: lowers the [syn](https://docs.rs/syn) AST into `cccc-core`'s IR. Depends only on `cccc-core` + syn — **no CLI dependencies**. |
| [`cccc-go`](crates/cccc-go) | Go adapter **library**: lowers the [gosyn](https://docs.rs/gosyn) AST into `cccc-core`'s IR. Depends only on `cccc-core` + gosyn — **no CLI dependencies**. |
| [`cccc-php`](crates/cccc-php) | PHP adapter **library**: lowers the [php-rs-parser](https://docs.rs/php-rs-parser) AST into `cccc-core`'s IR. Depends only on `cccc-core` + php-rs-parser / php-ast — **no CLI dependencies**. |
| [`cccc-rb`](crates/cccc-rb) | Ruby adapter **library**: lowers the [ruby-prism](https://docs.rs/ruby-prism) AST into `cccc-core`'s IR. Depends only on `cccc-core` + ruby-prism — **no CLI dependencies**. Note: ruby-prism is an FFI binding to the vendored Prism C source, so building this crate (unlike the others) needs a C99 compiler and libclang. |
| [`cccc-scheme`](crates/cccc-scheme) | Scheme (R7RS-small) adapter **library**: lowers the [lispexp](https://docs.rs/lispexp) S-expression tree into `cccc-core`'s IR. Depends only on `cccc-core` + lispexp (pure Rust) — **no CLI dependencies**. |
| [`cccc-lisp-kit`](crates/cccc-lisp-kit) | Shared **lowering kit** for the Lisp-family adapters: the collector stack, the `walk_regions` code-vs-data traversal, and logical folding. A dialect adapter supplies just a reader preset + a head-symbol dispatch table. Re-exports `cccc-core`'s IR and the pure-Rust lispexp reader. |
| [`cccc-lisp`](crates/cccc-lisp) | Lisp-family adapter **library** (Common Lisp, Emacs Lisp, …) built on `cccc-lisp-kit`. Its `Dialect` API also analyzes Scheme/Clojure by delegating to `cccc-scheme`/`cccc-clojure` (no duplicated lowering). **No CLI dependencies.** |
| [`cccc-clojure`](crates/cccc-clojure) | Clojure adapter **library**: lowers the [lispexp](https://docs.rs/lispexp) S-expression tree into `cccc-core`'s IR. Depends only on `cccc-core` + lispexp (pure Rust) — **no CLI dependencies**. |

Each adapter is a standalone library so that a consumer who only wants the
metrics pulls in just that adapter (+ `cccc-core` + its parser), never clap /
ignore / rayon. The `cccc` binary depends on all of them and dispatches by
extension.

To support another language: (1) add an adapter crate that lowers its AST into
`cccc_core::ir::Node` and calls `cccc_core::engine::analyze`, then (2) register
it with one entry in `cccc-cli`'s `lang::LANGUAGES` (and add the dependency) —
no new binary, and no reimplementing the metrics or the CLI. `cccc-es` (oxc),
`cccc-rs` (syn), `cccc-go` (gosyn), `cccc-php` (php-rs-parser), `cccc-rb`
(ruby-prism), `cccc-scheme` (lispexp), `cccc-clojure` (lispexp), and `cccc-lisp`
(lispexp, Common Lisp / Emacs Lisp / …) are the reference adapters: same shape,
different parser. The Lisp-family adapters share their lowering skeleton via
`cccc-lisp-kit`.

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
# single binary at ./target/release/cccc
```

## Usage

```sh
cccc <paths...> [options]
```

One binary handles every language. Pass one or more files or directories;
directories are walked recursively (respecting `.gitignore`, always skipping
`node_modules`). Each file is dispatched to the right front-end by its
extension, so a directory mixing `.ts`, `.rs`, `.go`, and `.php` is analyzed in
a single run. Restrict the languages with `--lang` (e.g. `--lang go,rust`).

Output is **JSON by default**.

### Options

| Flag | Description |
|------|-------------|
| `--lang LIST` | Restrict analysis to these languages (comma-separated; canonical names or aliases, e.g. `es`,`rust`/`rs`,`go`,`php`). Default: all |
| `--exclude-lang LIST` | Exclude these languages (comma-separated). The inverse of `--lang`; applied to all languages, or to `--lang`'s set when both are given |
| `--config PATH` | Use this config file instead of discovering one (must exist) |
| `--no-config` | Do not look for or load a `cccc.toml` config file |
| `--table` | Human-readable table instead of JSON |
| `--ext EXTS \| LANG=EXTS` | Extensions to analyze. Global form `--ext ts,tsx` filters across all languages; per-language form `--ext es=ts,tsx` overrides that language's extensions and routes them to it. Repeatable |
| `--exclude GLOB` | Exclude files matching a glob (repeatable) |
| `--max-cognitive N` | Exit non-zero if any function's cognitive complexity exceeds N |
| `--max-cyclomatic N` | Exit non-zero if any function's cyclomatic complexity exceeds N |
| `--min N` | Only report functions with complexity >= N |
| `--top-cognitive N` | Show only the N most cognitively-complex functions, as a flat cross-file ranking |
| `--top-cyclomatic N` | Show only the N most cyclomatically-complex functions, as a flat cross-file ranking |
| `--no-ignore` | Do not respect `.gitignore` when walking directories |
| `-j, --jobs N` | Number of files to analyze in parallel (default: logical CPU count) |

### Configuration file

Recurring options can be stored in a `cccc.toml` file so they don't have to be
repeated on every run. By default `cccc` discovers one by walking up from the
current directory, looking for `cccc.toml` (then `.cccc.toml`) in each ancestor;
`--config PATH` selects an explicit file and `--no-config` disables discovery.

Resolution precedence is **CLI flag > config file > built-in default**: anything
passed on the command line always wins. Supported keys (all optional):

```toml
# cccc.toml
languages         = ["es", "go"]        # same as --lang
exclude-languages = ["php"]             # same as --exclude-lang
exclude           = ["dist/**", "**/*.test.ts"]
table         = false
max-cognitive = 15
max-cyclomatic = 10
min           = 1
no-ignore     = false
jobs          = 8

# Per-language extension overrides. Each entry replaces that language's default
# extensions (and routes those extensions to it). Keyed by a language's name or
# alias; languages without an entry keep their defaults.
[ext]
es = ["ts", "tsx"]      # analyze only .ts/.tsx as ECMAScript (not .js, .mjs, …)
go = ["go", "tmpl"]     # also route a custom .tmpl extension to the Go front-end
```

The config-file `ext` is a **per-language table**: it both narrows/extends which
extensions a language claims and determines how a custom extension is routed.
The same per-language form is available on the command line as
`--ext LANG=ext,ext` (which overrides the config's entry for that language),
alongside the global filter form `--ext ext,ext`.

(`--top-cognitive`/`--top-cyclomatic` and the input paths are command-line only.)

`--top-cognitive` and `--top-cyclomatic` are mutually exclusive. In top mode the
output is a ranking (`{ "metric", "top": [...], "summary" }`) instead of the
per-file `files` array; each entry carries its own `path` and `line`. The
`summary` still reflects the full population.

`--exclude` takes a glob pattern and may be given multiple times. Each pattern is
matched both against a file's path relative to the directory you passed (so
`dist/**` is anchored at that root) and against its file name alone (so
`*.test.ts` matches at any depth without a `**/` prefix). `*` does not cross `/`;
use `**` to span directories. Brace alternation is supported, e.g.
`**/*.{test,spec}.ts`. Excluded files are dropped whether found by walking a
directory or named explicitly on the command line. An invalid pattern is an error
(exit code 2). This is independent of `--no-ignore` and `.gitignore` handling.

### Examples

```sh
# JSON for one file
cccc src/app.ts

# Pretty table for a directory (any mix of supported languages)
cccc --table src/

# Only Go and Rust files under a mixed tree
cccc --lang go,rust .

# Everything except PHP
cccc --exclude-lang php .

# Analyze only .ts/.tsx as ECMAScript (not .js, .mjs, …)
cccc --ext es=ts,tsx src/

# CI gate: fail if any function exceeds cognitive complexity 15
cccc --max-cognitive 15 src/

# The 10 most cognitively-complex functions across the project
cccc --top-cognitive 10 src/

# Skip build output and test files
cccc --exclude 'dist/**' --exclude '**/*.{test,spec}.ts' src/

# Limit parallelism to 4 workers (default is the logical CPU count)
cccc -j 4 src/
```

Files are analyzed in parallel. The worker count defaults to the number of
logical CPUs and can be capped with `-j/--jobs`; the output is identical
regardless of the worker count.

## GitHub Action

A composite action to install and run `cccc`  in CI lives in its own repository:
[moznion/cccc-action](https://github.com/moznion/cccc-action).

```yaml
- uses: moznion/cccc-action@v1
  with:
    path: src/           # analyze this; thresholds come from cccc.toml
```

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

## Benchmark

On [zod](https://github.com/colinhacks/zod)'s `packages/zod/src` (286 `.ts`
files, 68,357 LOC), median wall-clock and peak memory over 5 runs on an Apple
M4 Pro:

| Tool | Metrics | Time | Peak RSS |
|------|---------|-----:|---------:|
| **cccc** (ECMAScript) | cognitive + cyclomatic, per-function, full AST | **15.5 ms** | **12.5 MB** |
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

The rules above are stated in TypeScript/JavaScript terms; each adapter maps its
language onto the same IR. For **Rust** (`--lang rust`): `fn` / `impl` methods /
trait default methods / closures are the function-like units; `if`/`else if`/
`else`, `match` (a `_` or bare-binding arm is the non-decision `default`),
`for`/`while`/`loop`, labelled `break`/`continue`, and `&&`/`||` map to the
corresponding nodes. Rust has no ternary (`if` is an expression) and no
`try`/`catch` (errors flow through `?`), so those simply don't occur.

For **Go** (`--lang go`): top-level functions / methods / function literals
(closures) are the function-like units; `if`/`else if`/`else`, `for` (including
`for`-`range`), `switch`/type-`switch`/`select` (a `default` clause is the
non-decision arm), labelled `break`/`continue`/`goto`, and `&&`/`||` map to the
corresponding nodes. Go has no ternary and no `try`/`catch` (errors are returned
values), so those simply don't occur.

For **PHP** (`--lang php`): functions / methods / closures / `fn` arrow functions /
property hooks are the function-like units; `if`/`elseif`/`else`, `while`/
`do`-`while`/`for`/`foreach`, `switch` and the `match` expression (a `default`
arm is the non-decision case), `catch` clauses, multi-level `break N`/
`continue N` and `goto`, the ternary `?:`, and `&&`/`and`/`||`/`or`/`??` map to
the corresponding nodes. `&&` and `and` (likewise `||` and `or`) are the same
normalized operator; `??` folds as a coalescing run.
