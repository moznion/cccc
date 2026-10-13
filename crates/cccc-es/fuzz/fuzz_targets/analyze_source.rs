//! Feeds arbitrary input through the whole ECMAScript/TypeScript pipeline
//! (parser, IR lowering, scoring). The only property checked is that it
//! returns: no panic, no stack overflow, no hang.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

/// The adapter parses differently by extension; the first input byte picks
/// one, the rest is the source.
const EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

fuzz_target!(|data: &[u8]| {
    let Some((&pick, rest)) = data.split_first() else {
        return;
    };
    // The CLI reads files with `read_to_string`, so non-UTF-8 input never
    // reaches the adapter.
    let Ok(source) = std::str::from_utf8(rest) else {
        return;
    };
    let path = format!("fuzz.{}", EXTS[pick as usize % EXTS.len()]);
    let _ = cccc_es::analyze_source(Path::new(&path), source);
});
