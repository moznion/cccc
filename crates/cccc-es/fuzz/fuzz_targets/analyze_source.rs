//! Feeds arbitrary input through the whole ECMAScript/TypeScript pipeline
//! (parser, IR lowering, scoring). The only property checked is that it
//! returns: no panic, no stack overflow, no hang.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

/// The adapter parses differently by extension; the first input byte picks
/// one, the rest is the source.
const EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

/// Most `<` a TypeScript-mode input may hold. oxc_parser re-parses the rest of
/// the input after each failed type-argument guess at a `<`, so its time grows
/// ~3x per `<` (oxc-project/oxc#27637); the worst input found takes ~0.1 s at
/// 10 `<` under ASan and ~1 s at 12. Without a cap the fuzzer finds that
/// upstream hang within minutes and stops there. Remove once oxc ships a fix.
const MAX_TS_L_ANGLES: usize = 10;

fuzz_target!(|data: &[u8]| {
    let Some((&pick, rest)) = data.split_first() else {
        return;
    };
    // The CLI reads files with `read_to_string`, so non-UTF-8 input never
    // reaches the adapter.
    let Ok(source) = std::str::from_utf8(rest) else {
        return;
    };
    let ext = EXTS[pick as usize % EXTS.len()];
    let is_ts = matches!(ext, "ts" | "tsx" | "mts" | "cts");
    if is_ts && source.matches('<').count() > MAX_TS_L_ANGLES {
        return;
    }
    let path = format!("fuzz.{ext}");
    let _ = cccc_es::analyze_source(Path::new(&path), source);
});
