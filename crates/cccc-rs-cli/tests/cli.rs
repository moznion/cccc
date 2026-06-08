use assert_cmd::Command;

#[test]
fn version_reports_binary_name() {
    Command::cargo_bin("cccc-rs")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::starts_with("cccc-rs "));
}

#[test]
fn nonexistent_path_is_an_error() {
    Command::cargo_bin("cccc-rs")
        .unwrap()
        .arg("/no/such/path-xyz")
        .assert()
        .failure()
        .code(2);
}

#[test]
fn existing_dir_with_no_matches_is_ok() {
    // A real directory containing nothing analyzable is an empty, successful run
    // — distinct from a path that doesn't exist.
    let dir = std::env::temp_dir().join("cccc_rs_empty_match_test");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("readme.md"), "# not analyzed").unwrap();
    Command::cargo_bin("cccc-rs")
        .unwrap()
        .arg(&dir)
        .assert()
        .success();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn outputs_json_by_default() {
    let out = Command::cargo_bin("cccc-rs")
        .unwrap()
        .arg("tests/fixtures/sample.rs")
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let file = &v["files"][0];
    assert_eq!(file["path"], "tests/fixtures/sample.rs");
    let funcs = file["functions"].as_array().unwrap();
    let sop = funcs.iter().find(|f| f["name"] == "sum_of_primes").unwrap();
    assert_eq!(sop["cognitive"], 7);
    assert_eq!(sop["cyclomatic"], 4);
}

#[test]
fn jobs_option_produces_same_output() {
    // The result must be independent of the worker count.
    let single = Command::cargo_bin("cccc-rs")
        .unwrap()
        .args(["--jobs", "1", "tests/fixtures/sample.rs"])
        .assert()
        .success();
    let many = Command::cargo_bin("cccc-rs")
        .unwrap()
        .args(["-j", "4", "tests/fixtures/sample.rs"])
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
    Command::cargo_bin("cccc-rs")
        .unwrap()
        .args(["--jobs", "0", "tests/fixtures/sample.rs"])
        .assert()
        .failure();
}

#[test]
fn includes_project_summary() {
    let out = Command::cargo_bin("cccc-rs")
        .unwrap()
        .arg("tests/fixtures/sample.rs")
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let s = &v["summary"];
    assert_eq!(s["file_count"], 1);
    // sample.rs has sum_of_primes, get_words, classify.
    assert_eq!(s["function_count"], 3);
    assert_eq!(s["cognitive"]["sum"], 10);
    assert_eq!(s["cognitive"]["max"], 7);
    assert!(s["cognitive"]["median"].is_number());
    assert!(s["cyclomatic"]["p95"].is_number());
}

#[test]
fn top_cognitive_returns_flat_ranking() {
    let out = Command::cargo_bin("cccc-rs")
        .unwrap()
        .args(["--top-cognitive", "2", "tests/fixtures/sample.rs"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert!(v.get("files").is_none(), "top mode must not emit files");
    assert_eq!(v["metric"], "cognitive");
    let top = v["top"].as_array().unwrap();
    assert_eq!(top.len(), 2);
    // sample.rs: sum_of_primes(7) > classify(2) > get_words(1).
    assert_eq!(top[0]["name"], "sum_of_primes");
    assert_eq!(top[0]["cognitive"], 7);
    assert_eq!(top[0]["path"], "tests/fixtures/sample.rs");
    assert_eq!(top[1]["name"], "classify");
    // summary still reflects the full population (3 functions).
    assert_eq!(v["summary"]["function_count"], 3);
}

#[test]
fn top_cyclomatic_orders_by_cyclomatic() {
    let out = Command::cargo_bin("cccc-rs")
        .unwrap()
        .args(["--top-cyclomatic", "1", "tests/fixtures/sample.rs"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(v["metric"], "cyclomatic");
    // sum_of_primes has the highest cyclomatic (4).
    assert_eq!(v["top"][0]["name"], "sum_of_primes");
    assert_eq!(v["top"][0]["cyclomatic"], 4);
}

#[test]
fn top_flags_are_mutually_exclusive() {
    Command::cargo_bin("cccc-rs")
        .unwrap()
        .args([
            "--top-cognitive",
            "1",
            "--top-cyclomatic",
            "1",
            "tests/fixtures/sample.rs",
        ])
        .assert()
        .failure();
}

#[test]
fn max_cognitive_threshold_fails() {
    // sum_of_primes has cognitive 7, so a max of 5 must fail (exit 1).
    Command::cargo_bin("cccc-rs")
        .unwrap()
        .args(["--max-cognitive", "5", "tests/fixtures/sample.rs"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn max_cognitive_threshold_passes_when_under() {
    Command::cargo_bin("cccc-rs")
        .unwrap()
        .args(["--max-cognitive", "100", "tests/fixtures/sample.rs"])
        .assert()
        .success();
}

#[test]
fn table_output_renders() {
    Command::cargo_bin("cccc-rs")
        .unwrap()
        .args(["--table", "tests/fixtures/sample.rs"])
        .assert()
        .success()
        .stdout(predicates::str::contains("sum_of_primes"))
        .stdout(predicates::str::contains("file total"))
        .stdout(predicates::str::contains("summary"));
}
