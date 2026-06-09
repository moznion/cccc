//! The `cccc-go` binary: Go front-end.
//!
//! All CLI behaviour lives in `cccc_cli`; this binary only wires in the Go
//! adapter (`analyze_source`) and its default file extensions.

fn main() {
    std::process::exit(cccc_cli::run(
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION"),
        cccc_go::analyze_source,
        cccc_go::DEFAULT_EXTS,
    ));
}
