//! Per-language smoke tests: confirm the unified `cccc` binary dispatches each
//! fixture to the right front-end and reproduces the known scores. The generic
//! CLI behaviour (flags, config, exclude, …) is exercised once in `cli.rs`.

use assert_cmd::Command;

/// Run `cccc` on a single fixture and return the analyzed file's functions.
fn functions(fixture: &str) -> serde_json::Value {
    let out = Command::cargo_bin("cccc")
        .unwrap()
        .arg(format!("tests/fixtures/{fixture}"))
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    v["files"][0]["functions"].clone()
}

/// Every language fixture defines an equivalent `sumOfPrimes`/`sum_of_primes`
/// with cognitive 7 / cyclomatic 4 — the shared cross-language anchor.
fn assert_sum_of_primes(fixture: &str, name: &str) {
    let funcs = functions(fixture);
    let f = funcs
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("{name} not found in {fixture}"));
    assert_eq!(f["cognitive"], 7, "{fixture}: cognitive");
    assert_eq!(f["cyclomatic"], 4, "{fixture}: cyclomatic");
}

#[test]
fn es_fixture_dispatches() {
    assert_sum_of_primes("sample.ts", "sumOfPrimes");
}

#[test]
fn rust_fixture_dispatches() {
    assert_sum_of_primes("sample.rs", "sum_of_primes");
}

#[test]
fn go_fixture_dispatches() {
    assert_sum_of_primes("sample.go", "sumOfPrimes");
}

#[test]
fn php_fixture_dispatches() {
    assert_sum_of_primes("sample.php", "sumOfPrimes");
}

#[test]
fn ruby_fixture_dispatches() {
    assert_sum_of_primes("sample.rb", "sum_of_primes");
}

#[test]
fn scheme_fixture_dispatches() {
    assert_sum_of_primes("sample.scm", "sum-of-primes");
}

#[test]
fn commonlisp_fixture_dispatches() {
    assert_sum_of_primes("sample.lisp", "sum-of-primes");
}

#[test]
fn emacslisp_fixture_dispatches() {
    assert_sum_of_primes("sample.el", "sum-of-primes");
}

#[test]
fn clojure_fixture_dispatches() {
    assert_sum_of_primes("sample.clj", "sum-of-primes");
}

#[test]
fn kotlin_fixture_dispatches() {
    assert_sum_of_primes("sample.kt", "sumOfPrimes");
}
