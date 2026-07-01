//! The unified `cccc` binary.
//!
//! All behaviour lives in the [`cccc_cli`] library; this entry point just runs
//! it and forwards the process exit code. The set of supported languages is the
//! compiled-in registry in [`cccc_cli::lang`].

fn main() {
    std::process::exit(cccc_cli::run());
}
