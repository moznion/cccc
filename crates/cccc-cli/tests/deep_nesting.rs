//! Regression tests for stack overflows on deeply nested input. An overflow
//! aborts the process, so before `cccc_core::guard` one such file killed the
//! whole multi-file run — Rust overflowed at ~1000 nested `if`s.
//!
//! The library tests run on the test harness's 2 MiB threads, the same budget
//! a rayon worker gets by default, so they also catch an adapter that bypasses
//! the guard's large stack.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use cccc_cli::lang::LANGUAGES;
use cccc_core::guard::MAX_NESTING_DEPTH;

/// `n` nested `if`s inside a function `f`, in the language named `lang`, plus
/// a file name with that language's extension. Every template opens at most
/// two brackets before the `if`s, and the innermost condition's `(x)` one
/// more, so `n = MAX_NESTING_DEPTH - 3` stays within the limit everywhere.
fn nested_ifs(lang: &str, n: usize) -> (&'static str, String) {
    // The innermost body is `x();` where statements need a terminator.
    let wrap = |head: &str, open: &str, close: &str, tail: &str| {
        let body = if close == "}" && !matches!(lang, "go" | "kotlin" | "swift" | "scala") {
            "x();"
        } else {
            "x"
        };
        format!("{head}{}{body}{}{tail}", open.repeat(n), close.repeat(n))
    };
    match lang {
        "es" => ("a.js", wrap("function f() {", "if (x) {", "}", "}")),
        "rust" => ("a.rs", wrap("fn f() {", "if x {", "}", "}")),
        "go" => (
            "a.go",
            wrap("package m\nfunc f() {\n", "if x {\n", "}\n", "}\n"),
        ),
        "php" => ("a.php", wrap("<?php function f() {", "if ($x) {", "}", "}")),
        "ruby" => ("a.rb", wrap("def f\n", "if x\n", "\nend", "\nend\n")),
        "scheme" => ("a.scm", wrap("(define (f) ", "(if x ", ")", ")")),
        "commonlisp" => ("a.lisp", wrap("(defun f () ", "(if x ", ")", ")")),
        "emacslisp" => ("a.el", wrap("(defun f () ", "(if x ", ")", ")")),
        "clojure" => ("a.clj", wrap("(defn f [] ", "(if x ", ")", ")")),
        "kotlin" => ("a.kt", wrap("fun f() {", "if (x) {", "}", "}")),
        "python" => {
            let mut src = String::from("def f():\n");
            for depth in 1..=n {
                src += &" ".repeat(depth);
                src += "if x:\n";
            }
            src += &" ".repeat(n + 1);
            src += "x\n";
            ("a.py", src)
        }
        "zig" => ("a.zig", wrap("fn f() void {", "if (x) {", "}", "}")),
        "c" => ("a.c", wrap("void f() {", "if (x) {", "}", "}")),
        "cpp" => ("a.cpp", wrap("void f() {", "if (x) {", "}", "}")),
        "perl" => ("a.pl", wrap("sub f {", "if ($x) {", "}", "}")),
        "swift" => ("a.swift", wrap("func f() {", "if x {", "}", "}")),
        "java" => (
            "A.java",
            wrap("class A { void f() {", "if (x) {", "}", "}}"),
        ),
        "dart" => ("a.dart", wrap("void f() {", "if (x) {", "}", "}")),
        "scala" => (
            "a.scala",
            wrap("object A { def f() = {", "if (x) {", "}", "}}"),
        ),
        "csharp" => ("A.cs", wrap("class A { void F() {", "if (x) {", "}", "}}")),
        "vbnet" => (
            "A.vb",
            wrap(
                "Module M\nSub F()\n",
                "If x Then\n",
                "\nEnd If",
                "\nEnd Sub\nEnd Module\n",
            ),
        ),
        other => panic!("no nested-if template for language {other:?}; add one"),
    }
}

/// Parsers that give up on deep nesting by themselves, below
/// `MAX_NESTING_DEPTH`: `gosyn` and `zigsyn` error out at 64 levels, `lispexp`
/// at 200, and tree-sitter-python's scanner keeps at most ~511 indentation
/// levels.
fn parser_depth_limit(lang: &str) -> Option<usize> {
    match lang {
        "go" | "zig" => Some(60),
        "python" => Some(500),
        "scheme" | "commonlisp" | "emacslisp" | "clojure" => Some(190),
        _ => None,
    }
}

/// Nesting right up to the limit must still be analyzed — fully, not just
/// without crashing: `n` nested `if`s score `1 + 2 + … + n` cognitive.
#[test]
fn nesting_at_the_limit_is_analyzed_in_every_language() {
    for lang in LANGUAGES {
        let n = MAX_NESTING_DEPTH - 3;
        if parser_depth_limit(lang.name).is_some() {
            // Past its own limit the parser reports an error instead.
            let (file, src) = nested_ifs(lang.name, n);
            let report = (lang.analyze)(Path::new(file), &src);
            assert!(!report.parse_errors.is_empty(), "{}", lang.name);
        }
        let n = parser_depth_limit(lang.name).unwrap_or(n);
        let (file, src) = nested_ifs(lang.name, n);
        let report = (lang.analyze)(Path::new(file), &src);
        assert!(
            report.parse_errors.is_empty(),
            "{}: {:?}",
            lang.name,
            report.parse_errors
        );
        let f = report
            .functions
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case("f"))
            .unwrap_or_else(|| panic!("{}: function f not found", lang.name));
        assert_eq!(
            f.cognitive as usize,
            n * (n + 1) / 2,
            "{}: cognitive",
            lang.name
        );
    }
}

/// Past the limit, every language refuses the file with a parse error instead
/// of recursing into it.
#[test]
fn bracket_nesting_past_the_limit_is_refused_in_every_language() {
    let bomb = "(".repeat(100_000);
    for lang in LANGUAGES {
        let file = format!("bomb.{}", lang.exts[0]);
        let report = (lang.analyze)(Path::new(&file), &bomb);
        assert!(report.functions.is_empty(), "{}", lang.name);
        assert_eq!(report.parse_errors.len(), 1, "{}", lang.name);
        assert!(
            report.parse_errors[0].starts_with("line 1: nested deeper than"),
            "{}: {:?}",
            lang.name,
            report.parse_errors
        );
    }
}

/// Function nesting needs no brackets in some languages; it is capped too, so
/// no report deeper than the limit ever leaves the adapter.
#[test]
fn keyword_nested_functions_past_the_limit_are_refused() {
    let python = LANGUAGES.iter().find(|l| l.name == "python").unwrap();
    let lambdas = |n: usize| format!("f = {}1\n", "lambda: ".repeat(n));

    let ok = (python.analyze)(Path::new("ok.py"), &lambdas(MAX_NESTING_DEPTH));
    assert!(ok.parse_errors.is_empty(), "{:?}", ok.parse_errors);

    let deep = (python.analyze)(Path::new("deep.py"), &lambdas(MAX_NESTING_DEPTH + 1));
    assert!(deep.functions.is_empty());
    assert_eq!(deep.parse_errors.len(), 1);
    assert!(deep.parse_errors[0].starts_with("functions nested deeper than"));
}

/// A tree mixing an over-deep file, a file nested to the limit, and an
/// ordinary one, on disk under a fresh temp directory.
fn deep_tree(name: &str, ordinary_files: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bomb.rs"), "(".repeat(100_000)).unwrap();
    // A report nested to the limit, which the CLI then caches and renders.
    let fns = MAX_NESTING_DEPTH - 1;
    let deep = "function f() {".repeat(fns) + &"}".repeat(fns);
    std::fs::write(dir.join("deep.js"), deep).unwrap();
    for i in 0..ordinary_files {
        std::fs::write(dir.join(format!("ok{i}.js")), "function g(a) { if (a) {} }").unwrap();
    }
    dir
}

/// End to end, the over-deep file is reported and the rest of the run carries
/// on — in parallel and sequential mode, rendered as JSON and as a table, and
/// through a cold and then a warm cache (decoding the deep report back).
#[test]
fn cli_survives_deep_files_on_every_path() {
    // 20 ordinary files take the rayon path; 1 stays sequential.
    for (name, ordinary) in [("cccc_deep_parallel", 20), ("cccc_deep_sequential", 1)] {
        let dir = deep_tree(name, ordinary);
        let cache = dir.join("cache.bin");
        let cache = cache.to_str().unwrap();
        for args in [
            vec![],
            vec!["--table"],
            vec!["-j", "1"],
            vec!["--cache", "--cache-file", cache],
            vec!["--cache", "--cache-file", cache, "--table"],
        ] {
            let out = Command::cargo_bin("cccc")
                .unwrap()
                .args(&args)
                .arg(&dir)
                .assert()
                .success();
            let output = out.get_output();
            let all = String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr);
            assert!(all.contains("nested deeper than"), "{name} {args:?}: {all}");
            assert!(
                all.contains("ok0.js"),
                "{name} {args:?}: ordinary file missing"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
