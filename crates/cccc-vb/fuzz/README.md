# cccc-vb fuzzing

Fuzz targets for the hand-written VB.NET lexer/parser, via [cargo-fuzz]
(libFuzzer). This crate is outside the main workspace and needs nightly.

```sh
cargo install cargo-fuzz
cd crates/cccc-vb
mkdir -p fuzz/corpus/analyze_source
cargo +nightly fuzz run analyze_source fuzz/corpus/analyze_source fuzz/seeds -- \
  -max_total_time=300 -timeout=5 -max_len=4096
```

- `analyze_source`: runs arbitrary UTF-8 input through the full pipeline
  (lex, parse, lower, score). It checks that every input finishes without a
  panic, stack overflow, or hang. `-timeout` reports a slow input as a hang.
- `seeds/`: the starting corpus. It holds the CLI fixture and the snippets
  from the crate's unit tests. Discoveries go to `corpus/` and failing inputs
  to `artifacts/` (both git-ignored).

## CI and the shared corpus

`.github/workflows/fuzz.yml` runs every target daily for 10 minutes. You can
also start it by hand with a different duration. It resumes from the
corpus on the `fuzz-corpus` branch (`<crate>/<target>/`), minimizes it with
`cargo fuzz cmin`, and pushes it back, so the corpus keeps growing across
runs. Pull requests touching this crate get a 2-minute smoke run against
that corpus without writing back. Failing inputs are uploaded as a
workflow artifact.

To fuzz locally from the shared corpus:

```sh
git fetch origin fuzz-corpus
git worktree add ../cccc-fuzz-corpus origin/fuzz-corpus
cargo +nightly fuzz run analyze_source ../../../cccc-fuzz-corpus/cccc-vb/analyze_source fuzz/seeds
```

To reproduce or minimize a failure:

```sh
cargo +nightly fuzz run  analyze_source fuzz/artifacts/analyze_source/<file>
cargo +nightly fuzz tmin analyze_source fuzz/artifacts/analyze_source/<file>
```

[cargo-fuzz]: https://github.com/rust-fuzz/cargo-fuzz
