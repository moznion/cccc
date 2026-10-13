# cccc-java fuzzing

Fuzz targets for the Java adapter, via [cargo-fuzz] (libFuzzer). This
crate is outside the main workspace and needs nightly.

```sh
cargo install cargo-fuzz
cd crates/cccc-java
mkdir -p fuzz/corpus/analyze_source
cargo +nightly fuzz run analyze_source fuzz/corpus/analyze_source fuzz/seeds/analyze_source -- \
  -max_total_time=300 -timeout=10 -max_len=4096
```

- `analyze_source`: runs arbitrary UTF-8 input through the full pipeline (parse,
  lower, score) and checks that every input finishes without a panic, stack
  overflow, or hang. `-timeout` reports a slow input as a hang.
- `seeds/<target>/`: the starting corpus — the CLI fixture plus the snippets
  from the crate's unit tests. Discoveries go to `corpus/` and failing inputs
  to `artifacts/` (both git-ignored).

CI (`.github/workflows/fuzz.yml`), the shared corpus on the `fuzz-corpus`
branch, and reproducing a failure work as described in
[`crates/cccc-vb/fuzz/README.md`](../../cccc-vb/fuzz/README.md).

[cargo-fuzz]: https://github.com/rust-fuzz/cargo-fuzz
