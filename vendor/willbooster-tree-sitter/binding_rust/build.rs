use std::{env, fs, path::PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();

    #[cfg(feature = "bindgen")]
    generate_bindings(&out_dir);

    fs::copy(
        "src/wasm-stdlib/imports.txt",
        out_dir.join("stdlib-symbols.txt"),
    )
    .unwrap();

    let mut config = cc::Build::new();
    prefix_symbols(&mut config);
    if env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        config.define("NDEBUG", None);
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_WASM");
    if env::var("CARGO_FEATURE_WASM").is_ok() {
        config
            .define("TREE_SITTER_FEATURE_WASM", "")
            .define("static_assert(...)", "")
            .include(env::var("DEP_WASMTIME_C_API_INCLUDE").unwrap());
    }

    let manifest_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let include_path = manifest_path.join("include");
    println!("cargo:rerun-if-changed={}", include_path.display());
    let src_path = manifest_path.join("src");
    let wasm_path = src_path.join("wasm");

    if target == "wasm32-unknown-unknown" {
        configure_wasm_build(&mut config);
    }

    for entry in fs::read_dir(&src_path).unwrap() {
        let entry = entry.unwrap();
        let path = src_path.join(entry.file_name());
        println!("cargo:rerun-if-changed={}", path.to_str().unwrap());
    }

    config
        .std("c11")
        .flag_if_supported("-fvisibility=hidden")
        .flag_if_supported("-Wshadow")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-incompatible-pointer-types")
        .include(&src_path)
        .include(&wasm_path)
        .include(&include_path)
        .define("_POSIX_C_SOURCE", "200112L")
        .define("_DEFAULT_SOURCE", None)
        .define("_BSD_SOURCE", None)
        .define("_DARWIN_C_SOURCE", None)
        .warnings(false)
        .file(src_path.join("lib.c"))
        .compile("cccc-willbooster-tree-sitter");

    println!("cargo:include={}", include_path.display());
}

fn configure_wasm_build(config: &mut cc::Build) {
    let Ok(wasm_headers) = env::var("DEP_TREE_SITTER_LANGUAGE_WASM_HEADERS") else {
        panic!(
            "Environment variable DEP_TREE_SITTER_LANGUAGE_WASM_HEADERS must be set by the language crate"
        );
    };
    config
        .define("TREE_SITTER_WASM_STDLIB", "")
        .include(&wasm_headers);
}

#[cfg(feature = "bindgen")]
// code-gauge-allow duplicateLines -- mirrors xtask bindgen configuration without cross-crate build-script coupling
fn generate_bindings(out_dir: &std::path::Path) {
    use std::str::FromStr;

    use bindgen::RustTarget;

    const HEADER_PATH: &str = "include/tree_sitter/api.h";

    println!("cargo:rerun-if-changed={HEADER_PATH}");

    let no_copy = ["TSInput", "TSLanguage", "TSLogger", "TSParser", "TSTree"];

    let rust_version = env!("CARGO_PKG_RUST_VERSION");

    let bindings = bindgen::Builder::default()
        .header(HEADER_PATH)
        .layout_tests(false)
        .allowlist_type("^TS.*")
        .allowlist_function("^ts_.*")
        .allowlist_var("^TREE_SITTER.*")
        .no_copy(no_copy.join("|"))
        .prepend_enum_name(false)
        .use_core()
        .clang_arg("-D TREE_SITTER_FEATURE_WASM")
        .rust_target(RustTarget::from_str(rust_version).unwrap())
        .generate()
        .expect("Failed to generate bindings");

    let bindings_rs = out_dir.join("bindings.rs");
    bindings.write_to_file(&bindings_rs).unwrap_or_else(|_| {
        panic!(
            "Failed to write bindings into path: {}",
            bindings_rs.display()
        )
    });
}

/// Renames every global C symbol of the runtime to `cccc_wb_<symbol>` so it
/// can be linked next to upstream `tree-sitter`, which defines the same names.
/// The Rust declarations carry the matching `#[link_name]`.
fn prefix_symbols(config: &mut cc::Build) {
    let manifest_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let symbols_path = manifest_path.join("symbols.txt");
    println!("cargo:rerun-if-changed={}", symbols_path.display());
    let symbols = fs::read_to_string(&symbols_path).unwrap();
    for symbol in symbols
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        config.define(symbol, Some(format!("cccc_wb_{symbol}").as_str()));
    }
}
