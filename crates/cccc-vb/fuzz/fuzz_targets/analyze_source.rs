//! Feeds arbitrary input through the whole VB.NET pipeline (lexer, parser,
//! IR lowering, scoring). The only property checked is that it returns:
//! no panic, no stack overflow, no hang.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The CLI reads files with `read_to_string`, so non-UTF-8 input never
    // reaches the adapter.
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let _ = cccc_vb::analyze_source(Path::new("Fuzz.vb"), source);
});
