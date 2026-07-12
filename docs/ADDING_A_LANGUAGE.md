# Adding support for a new language

`cccc` keeps the **scoring engine** (`cccc-core`) and the **CLI** (`cccc-cli`)
completely language-agnostic. Supporting a new language means writing a thin
**adapter** that lowers that language's AST into the shared IR
([`cccc_core::ir::Node`]) — you never touch the metrics or the CLI.

This guide walks through it end to end, using the Python adapter (`cccc-py`)
as the running example — it now ships in-tree, so you can read the real thing
at [`crates/cccc-py`](../crates/cccc-py). The ECMAScript adapter (`cccc-es`)
is the original reference implementation — read it alongside this guide.

- [The big picture](#the-big-picture)
- [Step 1 — create the adapter crate](#step-1--create-the-adapter-crate)
- [Step 2 — lower the AST to IR](#step-2--lower-the-ast-to-ir)
- [Step 3 — register the language in the `cccc` binary](#step-3--register-the-language-in-the-cccc-binary)
- [Step 4 — register the crate in the workspace](#step-4--register-the-crate-in-the-workspace)
- [Step 5 — test it](#step-5--test-it)
- [The IR contract (reference)](#the-ir-contract-reference)
- [Checklist](#checklist)

## The big picture

```
your-parser ──▶ cccc-<lang>  ──(Vec<ir::Node>)──▶ cccc-core::engine ──▶ FileReport
 (AST)          (adapter lib)                      (scoring, shared)      (JSON/table)
                     ▲                                                        ▲
                     └──── registered in cccc-cli's lang::LANGUAGES ──────────┘
                           (the single `cccc` binary dispatches by extension)
```

One new crate per language, plus a one-line registry entry:

| Piece | Kind | Depends on | Responsibility |
|-------|------|-----------|----------------|
| `cccc-<lang>` | library crate | `cccc-core` + your parser | Parse source, lower AST → `Vec<ir::Node>`. **No scoring, no CLI deps.** |
| `lang::LANGUAGES` entry | one line in `cccc-cli` | — | Maps the adapter's `analyze_source`/`DEFAULT_EXTS` to a `--lang` name. |

Keeping the adapter as a **standalone library** is deliberate: a consumer who
only wants the metrics depends on `cccc-<lang>` (+ `cccc-core` + your parser) and
never pulls in `clap` / `ignore` / `rayon`. The unified `cccc` binary depends on
every adapter and routes each file to the right one by its extension.

## Step 1 — create the adapter crate

```
crates/cccc-py/
├── Cargo.toml
└── src/
    └── lib.rs
```

`crates/cccc-py/Cargo.toml`:

```toml
[package]
name = "cccc-py"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Python adapter that lowers source into the cccc-core complexity IR"

[dependencies]
cccc-core = { workspace = true }
# your parser crate(s) here — the real cccc-py uses the official grammar:
tree-sitter = "0.26"
tree-sitter-python = "0.25"
```

The adapter must expose exactly two public items, because that is the contract
the binary (Step 3) and `cccc_cli::run` rely on:

```rust
use std::path::Path;
use cccc_core::report::FileReport;

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["py", "pyi"];

/// Parse `source` (named by `path`) and produce its scored [`FileReport`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    cccc_core::engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}
```

> `analyze_source`'s signature **must** be `fn(&Path, &str) -> FileReport` — that
> is `cccc_cli::AnalyzeFn`. `analyze` itself is generic over languages; you only
> supply the `Vec<ir::Node>`.

Optionally also expose `to_ir(path, source) -> (Vec<Node>, Vec<String>)` (the IR
plus parser-error strings) for embedders who want the raw IR — `cccc-es` does.

Populate the parse-error strings whenever the parse was not clean — one entry
per error, e.g. `"syntax error at line 42"`. If your parser is fault-tolerant
(tree-sitter grammars, oxc, Prism, …), still lower whatever parsed and report
the error locations alongside; the engine attaches them to the file's
`parse_errors` and the CLI aggregates them into the summary
(`parse_error_count` / `parse_error_file_count` / `parse_error_files`) and
warns on stderr — listing the affected files — in `--table` mode, so partial
parses are surfaced without any adapter-side work.

## Step 2 — lower the AST to IR

This is the only real work. You walk your parser's AST and emit
[`cccc_core::ir::Node`] values for the constructs that affect complexity;
everything else becomes a [`Node::Group`] (a transparent container the engine
just recurses into) or is dropped.

See [The IR contract](#the-ir-contract-reference) below for what each node means
and which scoring it triggers. The mapping for a typical language:

| Source construct | IR node |
|------------------|---------|
| function / method / lambda / accessor | `Node::Function { name, kind, line, body }` |
| `if` / `elif` / `else` | `Node::Branch { test, then, alternate }` (chain `elif` as a nested `Branch` in `alternate`) |
| ternary / conditional expression | `Node::Conditional { test, then, alternate }` |
| `for` / `while` / comprehension-with-condition | `Node::Loop { body }` |
| `switch` / `match` | `Node::Switch { cases }` (one `SwitchCase` per arm; set `is_default` for the catch-all) |
| `catch` / `except` | `Node::Catch { body }` |
| labelled `break` / `continue` | `Node::Jump { labeled: true }` (unlabelled → `false`, or just omit) |
| `&&` / `||` / `??` chains | `Node::Logical { op, operands }` — **fold like operators** (see below) |
| null-guarded execution / null-aware access | `Node::NullGuard { body }` |
| function call | `Node::Call { callee: Some(name) }` (for recursion detection) |
| anything else with children | `Node::Group(children)` |

### Three subtleties to get right

**1. Logical-operator folding (the only lowering logic the adapter owns).**
A run of *like* operators is **one** `Logical` node: `a && b && c` →
`Logical { op: And, operands: [a, b, c] }` (one cognitive point). A *different*
operator nested inside starts a fresh `Logical`: `a && (b || c)` → an outer
`And` whose second operand is an inner `Logical { op: Or, .. }` (two points).
The engine counts one cognitive point per `Logical` node and one cyclomatic
point per *extra* operand, so the folding is what makes the SonarSource
like-operator rule come out right.

**2. Emit one `NullGuard` per explicit source-level guard.** Optional chaining
such as `a?.b?.c` has two guards, even if the parser wraps the whole chain in a
single container. Conversely, do not emit `NullGuard` for nullable types,
unwraps, ordinary null comparisons, or coalescing operators (`??`, elvis,
`orelse`); those either carry no score or remain `Logical::Coalesce`.

**3. Use your parser's full-traversal visitor, not a hand-written recursion.**
If your parser offers a visitor with default "walk every child" methods (oxc's
`Visit`, `syn::visit`, tree-sitter cursors, …), **use it** and override only the
nodes that produce IR. Hand-rolling a recursive `match` that enumerates node
kinds *will* miss constructs in positions you didn't think of (a lambda inside a
default-argument, an operator inside an index expression) and silently
undercount. `cccc-es` learned this the hard way; it now drives lowering from
oxc's `Visit` and assembles the IR with a stack of "collector" vectors:

```rust
// Sketch of the collector pattern (see crates/cccc-es/src/lib.rs for the real thing):
struct Builder { stack: Vec<Vec<Node>>, /* line table, pending name/kind … */ }

impl Builder {
    fn emit(&mut self, node: Node) { self.stack.last_mut().unwrap().push(node); }

    /// Run `f` against a fresh child collector and return what it gathered.
    fn collect(&mut self, f: impl FnOnce(&mut Self)) -> Vec<Node> {
        self.stack.push(Vec::new());
        f(self);
        self.stack.pop().unwrap()
    }
}

// In the visitor: a loop emits a Loop whose body is whatever the sub-walk gathered.
fn visit_while(&mut self, node: &WhileStmt) {
    let body = self.collect(|b| walk_while(b, node)); // default walk reaches everything
    self.emit(Node::Loop { body });
}
```

## Step 3 — register the language in the `cccc` binary

There is no per-language binary anymore. Instead, add one entry to the registry
in [`crates/cccc-cli/src/lang.rs`](../crates/cccc-cli/src/lang.rs) and add the
adapter as a dependency of `cccc-cli`.

In `lang.rs`, append to the `LANGUAGES` array:

```rust
Language {
    name: "python",                       // canonical --lang name
    aliases: &["py"],                     // extra accepted spellings
    exts: cccc_py::DEFAULT_EXTS,          // from Step 1
    analyze: cccc_py::analyze_source,     // the AnalyzeFn from Step 1
},
```

In `crates/cccc-cli/Cargo.toml`, add the dependency:

```toml
[dependencies]
cccc-py = { workspace = true }
```

That's it — the `cccc` binary now discovers `.py`/`.pyi` files, routes them to
your adapter, and accepts `--lang python`. The shared CLI already provides, for
free and identically across languages: argument parsing (`--lang`, `--config`,
`--table`, `--ext`, `--max-*`, `--min`, `--top-*`, `--no-ignore`, `-j/--jobs`),
config-file handling, `.gitignore`-aware file discovery, parallel analysis,
summary / ranking, JSON & table rendering, and the exit-code convention
(`0` ok / `1` threshold exceeded / `2` cannot proceed).

> Extensions must be **disjoint** across languages, since dispatch is by
> extension. If two languages would claim the same extension, the one registered
> first in `LANGUAGES` wins.

## Step 4 — register the crate in the workspace

Add the adapter crate to the root `Cargo.toml`:

```toml
[workspace]
members = [
    "crates/cccc-core",
    "crates/cccc-cli",
    "crates/cccc-es",
    "crates/cccc-py",       # new
]

[workspace.dependencies]
cccc-core = { path = "crates/cccc-core" }
cccc-es   = { path = "crates/cccc-es" }
cccc-py   = { path = "crates/cccc-py" }   # new — lets cccc-cli use `workspace = true`
```

## Step 5 — test it

Test the two concerns **separately** — this is the payoff of the IR seam:

1. **Engine rules** are already covered by `cccc-core`'s own tests (built from IR
   directly, no parser). You don't re-test the metrics.

2. **Your adapter's lowering** — write end-to-end tests that feed real source to
   `analyze_source` and assert the scores, mirroring
   `crates/cccc-es/src/lib.rs`'s `#[cfg(test)]` module. Anchor them on the
   SonarSource white-paper examples so cross-language results stay comparable:

   ```rust
   #[test]
   fn sonar_sum_of_primes_is_7() {
       let src = "def sum_of_primes(max):\n  ...";   // your language
       let r = analyze_source(Path::new("t.py"), src);
       assert_eq!(find(&r.functions, "sum_of_primes").unwrap().cognitive, 7);
   }
   ```

3. **CLI behaviour** is already covered generically in
   `crates/cccc-cli/tests/cli.rs`. To smoke-test that your language is wired into
   the unified binary, drop a fixture in `crates/cccc-cli/tests/fixtures/` and
   add a case to `crates/cccc-cli/tests/lang_smoke.rs` (it dispatches each
   fixture by extension through `cargo_bin("cccc")`).

Then:

```sh
cargo build
cargo clippy --all-targets   # expect zero warnings
cargo test                   # core + your adapter + cli tests
cargo run -p cccc-cli -- --lang python --table path/to/some/code
```

## The IR contract (reference)

Defined in [`crates/cccc-core/src/ir.rs`](../crates/cccc-core/src/ir.rs). The
top level you return is a `Vec<Node>` representing **module-level code**; the
engine scores it under an implicit "module" frame (so top-level statements count
toward the file totals but are not reported as a function).

| `Node` variant | Meaning | Cognitive | Cyclomatic |
|----------------|---------|-----------|------------|
| `Function { name, kind, line, body }` | A function-like unit. Scored independently — **nesting resets to 0** inside it — and reported as a child of the enclosing unit. `name`/`kind` are opaque labels you choose (`"function"`, `"method"`, `"arrow"`, `"getter"`, …); the engine only compares `name` for recursion. | — (body scored in its own frame) | base **1** |
| `Branch { test, then, alternate }` | `if`. `then` scored with nesting bonus; `alternate` (nested `Branch` = `else if`, else any node = `else`) scored **flat**. | `then`: +1 +nesting · `alternate`: +1 flat | +1 per branch + per `else if` |
| `Conditional { test, then, alternate }` | Ternary `?:`. | +1 +nesting | +1 |
| `Loop { body }` | Any loop. | +1 +nesting | +1 |
| `Switch { cases }` | `switch`/`match`. | +1 +nesting (the switch) | +1 per non-`default` case |
| `Catch { body }` | Exception handler. | +1 +nesting | +1 |
| `Jump { labeled }` | `break`/`continue`. | +1 if `labeled`, else 0 | — |
| `Logical { op, operands }` | One folded run of like operators. | +1 per node | +1 per *extra* operand (`operands.len() - 1`) |
| `NullGuard { body }` | A continuation or collection contribution skipped when its input is null. Emit one node per explicit null guard; coalescing remains `Logical`. | 0 | +1 |
| `Call { callee }` | A call. | +1 if `callee` == nearest enclosing function's `name` (recursion) | — |
| `Group(children)` | Transparent container; recurse only. | 0 | 0 |

Rules to respect when lowering:

- **Nesting** increases inside the bodies of `Branch`/`Conditional`/`Loop`/
  `Switch`/`Catch` and inside nested `Function` bodies. You express this purely
  by **structure** — put nested constructs inside the parent's `body`/`then`/
  `operands`; the engine tracks the depth. You never compute nesting yourself.
- **`else if` is a nested `Branch`** in the outer branch's `alternate`; a plain
  `else` is any other node (commonly a `Group`). This is what makes `else if`
  score flat (no nesting bonus) while a nested `if` inside `then` does get the
  bonus.
- **Recursion** is detected by name equality against the nearest enclosing
  `Function`. Populate `Call.callee` with the called function's simple name
  (e.g. both `foo()` and `obj.foo()` → `Some("foo")` in `cccc-es`). If you can't
  resolve a name, use `None`.
- **Module code is not a `Function`.** Don't wrap the file in a synthetic
  `Function`; just return its statements as the top-level `Vec<Node>`.

## Checklist

- [ ] `crates/cccc-<lang>/` adapter lib: `analyze_source` (`fn(&Path,&str)->FileReport`) + `DEFAULT_EXTS`; depends only on `cccc-core` + your parser.
- [ ] AST → IR lowering driven by a full-traversal visitor; like-logical-operators folded into single `Logical` nodes.
- [ ] New entry in `cccc-cli`'s `lang::LANGUAGES` (name + aliases + `DEFAULT_EXTS` + `analyze_source`); adapter added as a `cccc-cli` dependency. Extensions disjoint from existing languages.
- [ ] Adapter crate added to root `[workspace] members` and `[workspace.dependencies]`.
- [ ] Adapter tests assert scores on real source (anchor on SonarSource examples); a fixture + `lang_smoke.rs` case for the unified binary.
- [ ] `cargo build` / `cargo clippy --all-targets` / `cargo test` all green.

[`cccc_core::ir::Node`]: ../crates/cccc-core/src/ir.rs
[`Node::Group`]: ../crates/cccc-core/src/ir.rs
