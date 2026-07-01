//! End-to-end tests for the unified `cccc` binary: generic CLI behaviour shared
//! by every language (exercised through the ECMAScript fixture), plus the
//! multi-language dispatch, `--lang` filtering, and `cccc.toml` config features.

use assert_cmd::Command;

fn json(args: &[&str]) -> serde_json::Value {
    let out = Command::cargo_bin("cccc")
        .unwrap()
        .args(args)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    serde_json::from_str(&stdout).expect("valid JSON")
}

// ----- generic CLI behaviour (via the ECMAScript fixture) --------------------

#[test]
fn version_reports_binary_name() {
    Command::cargo_bin("cccc")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::starts_with("cccc "));
}

#[test]
fn nonexistent_path_is_an_error() {
    Command::cargo_bin("cccc")
        .unwrap()
        .arg("/no/such/path-xyz")
        .assert()
        .failure()
        .code(2);
}

#[test]
fn existing_dir_with_no_matches_is_ok() {
    let dir = std::env::temp_dir().join("cccc_empty_match_test");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("readme.md"), "# not analyzed").unwrap();
    Command::cargo_bin("cccc")
        .unwrap()
        .arg(&dir)
        .assert()
        .success();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn outputs_json_by_default() {
    let v = json(&["tests/fixtures/sample.ts"]);
    let file = &v["files"][0];
    assert_eq!(file["path"], "tests/fixtures/sample.ts");
    let funcs = file["functions"].as_array().unwrap();
    let sop = funcs.iter().find(|f| f["name"] == "sumOfPrimes").unwrap();
    assert_eq!(sop["cognitive"], 7);
    assert_eq!(sop["cyclomatic"], 4);
}

#[test]
fn jobs_option_produces_same_output() {
    let single = Command::cargo_bin("cccc")
        .unwrap()
        .args(["--jobs", "1", "tests/fixtures/sample.ts"])
        .assert()
        .success();
    let many = Command::cargo_bin("cccc")
        .unwrap()
        .args(["-j", "4", "tests/fixtures/sample.ts"])
        .assert()
        .success();
    assert_eq!(
        single.get_output().stdout,
        many.get_output().stdout,
        "output must not depend on --jobs"
    );
}

#[test]
fn jobs_zero_is_rejected() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--jobs", "0", "tests/fixtures/sample.ts"])
        .assert()
        .failure();
}

#[test]
fn includes_project_summary() {
    let v = json(&["tests/fixtures/sample.ts"]);
    let s = &v["summary"];
    assert_eq!(s["file_count"], 1);
    // sample.ts has sumOfPrimes, getWords, classify.
    assert_eq!(s["function_count"], 3);
    assert_eq!(s["cognitive"]["sum"], 10);
    assert_eq!(s["cognitive"]["max"], 7);
    assert!(s["cognitive"]["median"].is_number());
    assert!(s["cyclomatic"]["p95"].is_number());
}

#[test]
fn top_cognitive_returns_flat_ranking() {
    let v = json(&["--top-cognitive", "2", "tests/fixtures/sample.ts"]);
    assert!(v.get("files").is_none(), "top mode must not emit files");
    assert_eq!(v["metric"], "cognitive");
    let top = v["top"].as_array().unwrap();
    assert_eq!(top.len(), 2);
    assert_eq!(top[0]["name"], "sumOfPrimes");
    assert_eq!(top[0]["cognitive"], 7);
    assert_eq!(top[0]["path"], "tests/fixtures/sample.ts");
    assert_eq!(top[1]["name"], "classify");
    assert_eq!(v["summary"]["function_count"], 3);
}

#[test]
fn top_cyclomatic_orders_by_cyclomatic() {
    let v = json(&["--top-cyclomatic", "1", "tests/fixtures/sample.ts"]);
    assert_eq!(v["metric"], "cyclomatic");
    assert_eq!(v["top"][0]["name"], "sumOfPrimes");
    assert_eq!(v["top"][0]["cyclomatic"], 4);
}

#[test]
fn top_flags_are_mutually_exclusive() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args([
            "--top-cognitive",
            "1",
            "--top-cyclomatic",
            "1",
            "tests/fixtures/sample.ts",
        ])
        .assert()
        .failure();
}

#[test]
fn exclude_glob_drops_matching_files() {
    let dir = std::env::temp_dir().join("cccc_exclude_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/app.ts"), "function a() { return 1; }").unwrap();
    std::fs::write(dir.join("src/app.test.ts"), "function b() { return 2; }").unwrap();

    let out = Command::cargo_bin("cccc")
        .unwrap()
        .args(["--exclude", "*.test.ts"])
        .arg(&dir)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "the *.test.ts file must be excluded");
    assert!(files[0]["path"].as_str().unwrap().ends_with("app.ts"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exclude_glob_prunes_a_directory_subtree() {
    let dir = std::env::temp_dir().join("cccc_exclude_dir_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dist/nested")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/app.ts"), "function a() { return 1; }").unwrap();
    std::fs::write(dir.join("dist/bundle.ts"), "function b() { return 2; }").unwrap();
    std::fs::write(dir.join("dist/nested/x.ts"), "function c() { return 3; }").unwrap();

    let out = Command::cargo_bin("cccc")
        .unwrap()
        .args(["--exclude", "dist/**"])
        .arg(&dir)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "everything under dist/ must be excluded");
    assert!(files[0]["path"].as_str().unwrap().ends_with("app.ts"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exclude_applies_to_explicit_file_argument() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--exclude", "*.ts", "tests/fixtures/sample.ts"])
        .assert()
        .success()
        .stderr(predicates::str::contains("no matching files"));
}

#[test]
fn invalid_exclude_pattern_is_an_error() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--exclude", "a[b", "tests/fixtures/sample.ts"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn max_cognitive_threshold_fails() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--max-cognitive", "5", "tests/fixtures/sample.ts"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn max_cognitive_threshold_passes_when_under() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--max-cognitive", "100", "tests/fixtures/sample.ts"])
        .assert()
        .success();
}

#[test]
fn table_output_renders() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--table", "tests/fixtures/sample.ts"])
        .assert()
        .success()
        .stdout(predicates::str::contains("sumOfPrimes"))
        .stdout(predicates::str::contains("file total"))
        .stdout(predicates::str::contains("summary"));
}

// ----- multi-language dispatch & --lang -------------------------------------

#[test]
fn analyzes_all_languages_in_one_run() {
    // The fixtures dir holds one file per language; a single run dispatches each
    // by extension and reports them all together.
    let v = json(&["tests/fixtures"]);
    assert_eq!(v["summary"]["file_count"], 5);
    let paths: Vec<String> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    for ext in [
        "sample.ts",
        "sample.rs",
        "sample.go",
        "sample.php",
        "sample.rb",
    ] {
        assert!(paths.iter().any(|p| p.ends_with(ext)), "missing {ext}");
    }
}

#[test]
fn lang_filter_restricts_to_selected_languages() {
    let v = json(&["--lang", "go", "tests/fixtures"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "only the Go file should be analyzed");
    assert!(files[0]["path"].as_str().unwrap().ends_with("sample.go"));
}

#[test]
fn lang_filter_accepts_aliases_and_multiple() {
    let v = json(&["--lang", "rs,typescript", "tests/fixtures"]);
    assert_eq!(v["summary"]["file_count"], 2);
}

#[test]
fn unknown_lang_is_an_error() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--lang", "cobol", "tests/fixtures"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("unknown language"));
}

#[test]
fn exclude_lang_drops_a_language() {
    // All languages minus Go, PHP and Ruby leaves the .ts and .rs fixtures.
    let v = json(&["--exclude-lang", "go,php,ruby", "tests/fixtures"]);
    let mut exts: Vec<String> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            f["path"]
                .as_str()
                .unwrap()
                .rsplit('.')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    exts.sort();
    assert_eq!(exts, vec!["rs", "ts"]);
}

#[test]
fn exclude_lang_combines_with_lang() {
    // Start from {es, go, rust}, then drop go → {es, rust}.
    let v = json(&[
        "--lang",
        "es,go,rust",
        "--exclude-lang",
        "go",
        "tests/fixtures",
    ]);
    assert_eq!(v["summary"]["file_count"], 2);
    let has_go = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"].as_str().unwrap().ends_with(".go"));
    assert!(!has_go, "Go must be excluded");
}

#[test]
fn excluding_every_language_is_an_error() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--exclude-lang", "es,rust,go,php,ruby", "tests/fixtures"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("no languages selected"));
}

#[test]
fn unknown_exclude_lang_is_an_error() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--exclude-lang", "cobol", "tests/fixtures"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("unknown language"));
}

// ----- cccc.toml config ------------------------------------------------------

/// Create a temp project dir containing `cccc.toml` and a high-complexity
/// `sample.ts` (sumOfPrimes, cognitive 7).
fn config_project(name: &str, toml: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cccc.toml"), toml).unwrap();
    std::fs::write(
        dir.join("sample.ts"),
        "function sumOfPrimes(max) {\n  for (let i = 2; i <= max; ++i) {\n    for (let j = 2; j < i; ++j) {\n      if (i % j === 0) { return i; }\n    }\n  }\n  return 0;\n}\n",
    )
    .unwrap();
    dir
}

#[test]
fn config_is_discovered_by_walking_up_from_cwd() {
    let dir = config_project("cccc_cfg_discover", "max-cognitive = 5\n");
    // Running from inside the project picks up cccc.toml; the threshold fails.
    Command::cargo_bin("cccc")
        .unwrap()
        .current_dir(&dir)
        .arg("sample.ts")
        .assert()
        .failure()
        .code(1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_flag_overrides_config() {
    let dir = config_project("cccc_cfg_override", "max-cognitive = 5\n");
    // The CLI flag wins over the config's stricter threshold.
    Command::cargo_bin("cccc")
        .unwrap()
        .current_dir(&dir)
        .args(["--max-cognitive", "100", "sample.ts"])
        .assert()
        .success();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_config_ignores_discovered_file() {
    let dir = config_project("cccc_cfg_no_config", "max-cognitive = 5\n");
    Command::cargo_bin("cccc")
        .unwrap()
        .current_dir(&dir)
        .args(["--no-config", "sample.ts"])
        .assert()
        .success();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn explicit_config_path_is_loaded() {
    let dir = config_project("cccc_cfg_explicit", "max-cognitive = 5\n");
    let cfg = dir.join("cccc.toml");
    // Run from elsewhere (the crate dir) but point --config at the file: it
    // applies, so the strict threshold fails.
    Command::cargo_bin("cccc")
        .unwrap()
        .arg("--config")
        .arg(&cfg)
        .arg(dir.join("sample.ts"))
        .assert()
        .failure()
        .code(1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_explicit_config_is_an_error() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--config", "/no/such/cccc.toml", "tests/fixtures/sample.ts"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn invalid_config_is_an_error() {
    let dir = config_project("cccc_cfg_invalid", "totally = not valid =\n");
    Command::cargo_bin("cccc")
        .unwrap()
        .current_dir(&dir)
        .arg("sample.ts")
        .assert()
        .failure()
        .code(2);
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- per-language [ext] config --------------------------------------------

#[test]
fn per_language_ext_override_restricts_extensions() {
    // Restricting ES to `.ts` means a sibling `.js` file is no longer analyzed.
    let dir = std::env::temp_dir().join("cccc_cfg_ext_restrict");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cccc.toml"), "[ext]\nes = [\"ts\"]\n").unwrap();
    std::fs::write(dir.join("a.ts"), "function a() { return 1; }").unwrap();
    std::fs::write(dir.join("b.js"), "function b() { return 2; }").unwrap();

    let out = Command::cargo_bin("cccc")
        .unwrap()
        .current_dir(&dir)
        .arg(".")
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "only the .ts file should be analyzed");
    assert!(files[0]["path"].as_str().unwrap().ends_with("a.ts"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_language_ext_override_routes_custom_extension() {
    // A custom extension assigned to ES is parsed by the ES front-end.
    let dir = std::env::temp_dir().join("cccc_cfg_ext_custom");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cccc.toml"), "[ext]\nes = [\"ts\", \"myts\"]\n").unwrap();
    std::fs::write(dir.join("c.myts"), "const f = (a, b) => (a && b ? 1 : 0);").unwrap();

    let out = Command::cargo_bin("cccc")
        .unwrap()
        .current_dir(&dir)
        .arg(".")
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "the .myts file should be routed to ES");
    assert!(files[0]["path"].as_str().unwrap().ends_with("c.myts"));
    // `a && b ? .. : ..` scores cognitive 2 (ternary + &&) — proof ES parsed it.
    assert_eq!(files[0]["cognitive"], 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_language_in_ext_config_is_an_error() {
    let dir = std::env::temp_dir().join("cccc_cfg_ext_unknown");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cccc.toml"), "[ext]\ncobol = [\"cbl\"]\n").unwrap();
    std::fs::write(dir.join("a.ts"), "function a() { return 1; }").unwrap();

    Command::cargo_bin("cccc")
        .unwrap()
        .current_dir(&dir)
        .arg(".")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("unknown language"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- per-language --ext CLI flag ------------------------------------------

/// A temp dir with `a.ts`, `a.tsx`, and `b.js` (all analyzable as ECMAScript).
fn es_ext_project(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.ts"), "function a() { return 1; }").unwrap();
    std::fs::write(dir.join("a.tsx"), "function b() { return 2; }").unwrap();
    std::fs::write(dir.join("b.js"), "function c() { return 3; }").unwrap();
    dir
}

fn analyzed_names(v: &serde_json::Value) -> Vec<String> {
    v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            std::path::Path::new(f["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn cli_per_language_ext_restricts_extensions() {
    let dir = es_ext_project("cccc_cli_ext_restrict");
    let out = Command::cargo_bin("cccc")
        .unwrap()
        .args(["--ext", "es=ts"])
        .arg(&dir)
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    assert_eq!(analyzed_names(&v), vec!["a.ts"], "only .ts analyzed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_per_language_ext_accepts_alias_and_list() {
    let dir = es_ext_project("cccc_cli_ext_alias");
    let out = Command::cargo_bin("cccc")
        .unwrap()
        .args(["--ext", "typescript=ts,tsx"])
        .arg(&dir)
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    let mut names = analyzed_names(&v);
    names.sort();
    assert_eq!(names, vec!["a.ts", "a.tsx"], ".js excluded, .ts/.tsx kept");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_global_ext_still_filters_across_languages() {
    // The bare (no `=`) form is the original global filter.
    let v = json(&["--ext", "ts", "tests/fixtures"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0]["path"].as_str().unwrap().ends_with("sample.ts"));
}

#[test]
fn cli_ext_overrides_config_ext() {
    // Config says es = [ts, tsx]; the CLI narrows es to ts only — CLI wins.
    let dir = es_ext_project("cccc_cli_ext_over_config");
    std::fs::write(dir.join("cccc.toml"), "[ext]\nes = [\"ts\", \"tsx\"]\n").unwrap();
    let out = Command::cargo_bin("cccc")
        .unwrap()
        .current_dir(&dir)
        .args(["--ext", "es=ts", "."])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    assert_eq!(
        analyzed_names(&v),
        vec!["a.ts"],
        "CLI override wins over config"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_unknown_language_in_ext_is_an_error() {
    Command::cargo_bin("cccc")
        .unwrap()
        .args(["--ext", "cobol=cbl", "tests/fixtures/sample.ts"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("unknown language"));
}
