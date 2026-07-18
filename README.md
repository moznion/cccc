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
    Its child dialect **Racket** (`--lang racket`, `.rkt`/`.rktl`/`.rktd`) rides
    the same tolerant reader, with `match` and the `for` comprehension family
    scored on top of R7RS.
  - **Common Lisp** (`--lang commonlisp`), via the
    [lispexp](https://docs.rs/lispexp) S-expression reader. `.lisp`, `.lsp`, `.cl`.
  - **Emacs Lisp** (`--lang emacslisp`), via the
    [lispexp](https://docs.rs/lispexp) S-expression reader. `.el`.
  - **Clojure** (`--lang clojure`), via the
    [lispexp](https://docs.rs/lispexp) S-expression reader. `.clj`, `.cljs`, `.cljc`.
  - **Kotlin** (`--lang kotlin`), via the
    [exoego/tree-sitter-kotlin](https://github.com/exoego/tree-sitter-kotlin)
    grammar (a fork of the fwcd tree-sitter Kotlin grammar with fixes for
    modern-Kotlin constructs). Analyzes `.kt`, `.kts`.
  - **Python** (`--lang python`), via the official
    [tree-sitter-python](https://github.com/tree-sitter/tree-sitter-python)
    grammar. Analyzes `.py`, `.pyi`.
  - **Zig** (`--lang zig`), via the pure-Rust
    [zigsyn](https://docs.rs/zigsyn) parser. Analyzes `.zig`.
  - **C** (`--lang c`), via the official
    [tree-sitter-c](https://github.com/tree-sitter/tree-sitter-c) grammar.
    Analyzes `.c`, `.h`.
  - **Perl** (`--lang perl`), via the community-maintained
    [tree-sitter-perl](https://github.com/tree-sitter-perl/tree-sitter-perl)
    grammar. Analyzes `.pl`, `.pm`, `.t`.
  - **Swift** (`--lang swift`), via the
    [alex-pinkus/tree-sitter-swift](https://github.com/alex-pinkus/tree-sitter-swift)
    grammar. Analyzes `.swift`.
  - **Java** (`--lang java`), via the official
    [tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java)
    grammar. Analyzes `.java`.
  - **Dart** (`--lang dart`), via the
    [nielsenko/tree-sitter-dart](https://github.com/nielsenko/tree-sitter-dart)
    grammar. Analyzes `.dart`.
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
| [`cccc-scheme`](crates/cccc-scheme) | Scheme (R7RS-small) + Racket adapter **library**: lowers the [lispexp](https://docs.rs/lispexp) S-expression tree into `cccc-core`'s IR. Depends only on `cccc-core` + lispexp (pure Rust) — **no CLI dependencies**. |
| [`cccc-lisp-kit`](crates/cccc-lisp-kit) | Shared **lowering kit** for the Lisp-family adapters: the collector stack, the `walk_regions` code-vs-data traversal, and logical folding. A dialect adapter supplies just a reader preset + a head-symbol dispatch table. Re-exports `cccc-core`'s IR and the pure-Rust lispexp reader. |
| [`cccc-lisp`](crates/cccc-lisp) | Lisp-family adapter **library** (Common Lisp, Emacs Lisp, …) built on `cccc-lisp-kit`. Its `Dialect` API also analyzes Scheme/Clojure by delegating to `cccc-scheme`/`cccc-clojure` (no duplicated lowering). **No CLI dependencies.** |
| [`cccc-clojure`](crates/cccc-clojure) | Clojure adapter **library**: lowers the [lispexp](https://docs.rs/lispexp) S-expression tree into `cccc-core`'s IR. Depends only on `cccc-core` + lispexp (pure Rust) — **no CLI dependencies**. |
| [`cccc-scheme`](crates/cccc-scheme) | Scheme (R7RS-small) adapter **library**: lowers the [lispexp](https://docs.rs/lispexp) S-expression tree into `cccc-core`'s IR. Depends only on `cccc-core` + lispexp (pure Rust) — **no CLI dependencies**. |
| [`cccc-kt`](crates/cccc-kt) | Kotlin adapter **library**: lowers the [exoego/tree-sitter-kotlin](https://github.com/exoego/tree-sitter-kotlin) CST into `cccc-core`'s IR. Depends only on `cccc-core` + tree-sitter + the Kotlin grammar — **no CLI dependencies**. Note: the grammar ships C source compiled by `cc`, so building this crate needs a C compiler (but not libclang, unlike `cccc-rb`). |
| [`cccc-py`](crates/cccc-py) | Python adapter **library**: lowers the official [tree-sitter-python](https://github.com/tree-sitter/tree-sitter-python) CST into `cccc-core`'s IR. Depends only on `cccc-core` + tree-sitter + the Python grammar — **no CLI dependencies**. Like `cccc-kt`, the grammar's C source is compiled by `cc`, so building needs a C compiler (no libclang). |
| [`cccc-zig`](crates/cccc-zig) | Zig adapter **library**: lowers the pure-Rust [zigsyn](https://docs.rs/zigsyn) AST into `cccc-core`'s IR. Depends only on `cccc-core` + zigsyn — **no CLI dependencies or C toolchain**. |
| [`cccc-c`](crates/cccc-c) | C adapter **library**: lowers the official [tree-sitter-c](https://github.com/tree-sitter/tree-sitter-c) CST into `cccc-core`'s IR. Depends only on `cccc-core` + tree-sitter + the C grammar — **no CLI dependencies**. Like `cccc-kt`/`cccc-py`, the grammar's C source is compiled by `cc`, so building needs a C compiler (no libclang). |
| [`cccc-pl`](crates/cccc-pl) | Perl adapter **library**: lowers the [tree-sitter-perl](https://github.com/tree-sitter-perl/tree-sitter-perl) CST into `cccc-core`'s IR. Depends only on `cccc-core` + tree-sitter + the Perl grammar — **no CLI dependencies**. Like `cccc-kt`/`cccc-py`, the grammar's C source is compiled by `cc`, so building needs a C compiler (no libclang). |
| [`cccc-swift`](crates/cccc-swift) | Swift adapter **library**: lowers the [alex-pinkus/tree-sitter-swift](https://github.com/alex-pinkus/tree-sitter-swift) CST into `cccc-core`'s IR. Depends only on `cccc-core` + tree-sitter + the Swift grammar — **no CLI dependencies**. Like `cccc-kt`/`cccc-py`, the grammar's C source is compiled by `cc`, so building needs a C compiler (no libclang). |
| [`cccc-java`](crates/cccc-java) | Java adapter **library**: lowers the official [tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java) CST into `cccc-core`'s IR. Depends only on `cccc-core` + tree-sitter + the Java grammar — **no CLI dependencies**. Like `cccc-kt`/`cccc-py`, the grammar's C source is compiled by `cc`, so building needs a C compiler (no libclang). |
| [`cccc-dart`](crates/cccc-dart) | Dart adapter **library**: lowers the [nielsenko/tree-sitter-dart](https://github.com/nielsenko/tree-sitter-dart) CST into `cccc-core`'s IR. Depends only on `cccc-core` + tree-sitter + the Dart grammar — **no CLI dependencies**. The grammar's C source is compiled by `cc`, so building needs a C compiler (no libclang). |

Each adapter is a standalone library so that a consumer who only wants the
metrics pulls in just that adapter (+ `cccc-core` + its parser), never clap /
ignore / rayon. The `cccc` binary depends on all of them and dispatches by
extension.

To support another language: (1) add an adapter crate that lowers its AST into
`cccc_core::ir::Node` and calls `cccc_core::engine::analyze`, then (2) register
it with one entry in `cccc-cli`'s `lang::LANGUAGES` (and add the dependency) —
no new binary, and no reimplementing the metrics or the CLI. `cccc-es` (oxc),
`cccc-rs` (syn), `cccc-go` (gosyn), `cccc-php` (php-rs-parser), `cccc-rb`
(ruby-prism), `cccc-kt` / `cccc-py` / `cccc-pl` (tree-sitter), `cccc-swift` (tree-sitter), `cccc-c` (tree-sitter),
`cccc-java` (tree-sitter), `cccc-dart` (tree-sitter), `cccc-scheme` (lispexp), `cccc-clojure` (lispexp), `cccc-lisp` (lispexp, Common Lisp / Emacs Lisp / …),
and `cccc-zig` (zigsyn) are the reference adapters: same shape, different parser.
The Lisp-family adapters share their lowering skeleton via `cccc-lisp-kit`.

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

Output is **JSON by default** — compact, on one line, ready to pipe into `jq`
or an artifact store; `--pretty` prints the same document indented.

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
| `--pretty` | Pretty-print the JSON output (default is compact, one line) |
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
pretty        = false               # indented JSON instead of the compact default

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

An example GitHub Actions workflow for continuously measuring complexity with [k1LoW/octocov](https://github.com/k1LoW/octocov) is available at [.github/workflows/complexity.yml](./.github/workflows/complexity.yml).

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
    "parse_error_count": 0,
    "parse_error_file_count": 0,
    "cognitive":  { "sum": 10, "max": 7, "median": 2, "p90": 7, "p95": 7 },
    "cyclomatic": { "sum": 10, "max": 4, "median": 3, "p90": 4, "p95": 4 }
  }
}
```

### Parse errors

A file that fails to parse cleanly is still measured from whatever the parser
recovered, and its `parse_errors` (an array of messages, omitted when empty)
appears on that file's entry. The `summary` aggregates them — `parse_error_count`
(total errors), `parse_error_file_count` (affected files), and
`parse_error_files` (the affected paths, omitted when empty) — so a partial
parse can't go unnoticed without inspecting every file entry, even in `--top-*`
mode where per-file entries aren't emitted at all:

```json
{
  "files": [
    {
      "path": "src/broken.py",
      "parse_errors": ["syntax error at line 6"],
      ...
    }
  ],
  "summary": {
    "parse_error_count": 1,
    "parse_error_file_count": 1,
    "parse_error_files": ["src/broken.py"],
    ...
  }
}
```

In `--table` mode the aggregate count is printed in the summary block, and a
warning listing each affected file (with its error count) goes to stderr so it
isn't lost in a long table:

```console
$ cccc --table src/ >/dev/null
cccc: warning: 1 parse error(s) in 1 file(s); results for those files may be incomplete:
cccc:   src/broken.py (1)
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
`default`), `catch`, each `&&`/`||`/`??`, and each explicit null guard such as
an optional-chain segment. Null guards do not add cognitive complexity or
nesting.

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
Each null-safe property or method access (`?->`) adds one cyclomatic path.

For **Ruby** (`--lang ruby`): methods, blocks, and lambdas are function-like
units; branches, loops, `case`/`when` and `case`/`in`, rescue clauses, ternary
expressions, and logical operators map to the corresponding nodes. Each safe
navigation operator (`&.`) adds one cyclomatic path.

For **Kotlin** (`--lang kotlin`): `fun` declarations / methods / local
functions / `fun` anonymous functions / lambdas / property `get`/`set`
accessors are the function-like units; the `if` expression (`else if` — an `if`
nested in the `else` body — chains flat), the `when` expression with or without
a subject (its `else` entry is the non-decision `default` arm), `for`/`while`/
`do`-`while`, `catch` clauses, labelled `break@`/`continue@`, and `&&`/`||` map
to the corresponding nodes. The elvis operator `?:` folds as a coalescing run
(like PHP's `??`). Kotlin has no C-style ternary — `if` is already an
expression. Each safe-navigation operator (`?.`) adds one cyclomatic path.

For **Python** (`--lang python`): `def` (incl. `async def` and decorated
definitions) / methods / `lambda` are the function-like units;
`if`/`elif`/`else` (`elif` chains flat), the conditional expression
`a if b else c` (a ternary — its `else` arm is not a second increment),
`for`/`while` (incl. `async for`; a loop's `else` clause runs at the
surrounding level), `match` (a bare `case _:` is the non-decision `default`
arm), `except`/`except*` clauses, and `and`/`or` map to the corresponding
nodes. Comprehensions and generator expressions score like the written-out
loop: each `for` clause is a loop and each `if` clause a branch, nested
left-to-right. Python has no labelled `break`/`continue` and no `??`; `not`
adds nothing.

For **Zig** (`--lang zig`): named `fn` declarations and `test` blocks are the
function-like units; `if`/`else if`/`else`, `while`/`for` (a loop's `else`
branch runs at the surrounding level), `switch` (an `else` prong is the
non-decision `default` arm), `catch` handlers, labelled `break`/`continue`, and
`and`/`or` map to the corresponding nodes. `orelse` folds as a coalescing run.
Zig has no ternary expression; `if` is already an expression.

For **C** (`--lang c`): function definitions (including K&R-style definitions
and GNU nested functions) are the function-like units; `if`/`else if`/`else`,
the ternary `?:` (GNU's elided-middle `a ?: b` included), `for`/`while`/
`do`-`while`, `switch` (the `default:` label is the non-decision arm; each
fall-through `case` label is its own cyclomatic point), `goto` (one flat
cognitive point, like a labelled jump), and `&&`/`||` map to the corresponding
nodes. Preprocessor conditionals (`#if`/`#ifdef`/`#ifndef`, chained via
`#elif`/`#else`) score as branches, mirroring the SonarSource C/C++ analyzers.
C has no exceptions and no `??`; `#define` bodies are opaque to the grammar, so
code inside a macro body is not scored. One known wart of preprocessor-unaware
parsing: the standard `extern "C" {` guard splits its braces across two
`#ifdef __cplusplus` blocks, which surfaces as a parse warning — the rest of
the header still parses and scores.

For **Perl** (`--lang perl`): named `sub`s / `method` declarations (feature
`class`, Perl 5.38+) / anonymous `sub`s are the function-like units, and a
block callback passed to `grep`/`map`/`sort` is its own anonymous unit (like a
Ruby block); `if`/`elsif`/`else` (`elsif` chains flat) and `unless`, the
statement modifiers `EXPR if/unless COND` (a branch) and
`EXPR while/until/for COND` (a loop, incl. `do { } while`), the ternary `?:`,
`while`/`until`/C-style `for`/`foreach`, `try`/`catch` (feature `try`, Perl
5.34+ — `finally` runs at the surrounding level), labelled `next`/`last`/
`redo`, and `&&`/`and`/`||`/`or`/`//` map to the corresponding nodes. `&&` and
`and` (likewise `||` and `or`) are the same normalized operator; `//` folds as
a coalescing run. A classic `eval { }` is transparent (the `if ($@)` after it
is the decision point), `xor`/`not`/`!` add nothing, and `given`/`when` (long
deprecated) is not scored.

For **Swift** (`--lang swift`): `func` declarations / methods / local
functions / closures / `init` / `deinit` / `subscript` / computed-property
`get`/`set` accessors (including the implicit getter-only form) / `willSet`/
`didSet` observers are the function-like units; `if`/`else if`/`else` (with
`if let` / `if case` variants), `guard` … `else` (scored exactly like an `if`),
`switch` (its `default` entry is the non-decision arm; `case a, b:` is one
arm), `for`-`in` (its `where` clause adds nothing by itself), `while`/
`repeat`-`while`, `catch` blocks, labelled `break`/`continue`, the ternary
`a ? b : c`, and `&&`/`||` map to the corresponding nodes. Nil-coalescing `??`
folds as a coalescing run (like PHP's `??`). `#if` compilation directives are
transparent — every branch's code scores where it stands; `try`/`try?`/`await`
add nothing. Each optional-chaining guard on a member access, subscript, or call
adds one cyclomatic path.

For **Java** (`--lang java`): methods (incl. ones in anonymous classes and
interface `default` methods) / constructors / record compact constructors /
lambdas are the function-like units (static and instance initializer blocks
run at the surrounding level); `if`/`else if`/`else` (`else if` chains flat),
the ternary `?:`, `switch` statements and expressions alike — both colon-style
`case:` groups and arrow-style `case ->` rules, with pattern matching and
guards supported (a `default` or `case null, default` arm is the non-decision
case), `for`/enhanced `for`/`while`/`do`-`while`, `catch` clauses (a
multi-catch `catch (A | B e)` is one clause; `try`-with-resources bodies are
transparent), labelled `break L`/`continue L`, and `&&`/`||` map to the
corresponding nodes. Java has no `??`-style coalescing operator.

For **Dart** (`--lang dart`): top-level and local functions, methods, getters,
setters, constructors, factory constructors, operators, and anonymous function
expressions are function-like units; `if`/`else if`/`else`, the ternary `?:`,
`for` (including `await for`)/`while`/`do`-`while`, switch statements and switch
expressions (a `default` or wildcard arm is the non-decision case), `catch` and
`on` handlers, labelled `break`/`continue`, and `&&`/`||` map to the
corresponding nodes. Pattern `&&`/`||` use the same logical-sequence rules, and
collection `if`/`for` lower to nested branches/loops. `??` and `??=` map to
coalescing logical nodes. Null-aware access (`?.`, `?[]`, `?..`), null-aware
spread (`...?`), collection elements (`?value`), and map keys/values each add
one cyclomatic path without adding cognitive complexity. External, native, and
otherwise bodyless declarations are not reported as functions.
