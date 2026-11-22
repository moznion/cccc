use super::*;
use cccc_core::report::FunctionReport;

fn analyze(src: &str) -> FileReport {
    analyze_source(Path::new("test.sh"), src)
}

fn find<'a>(fns: &'a [FunctionReport], name: &str) -> Option<&'a FunctionReport> {
    for f in fns {
        if f.name == name {
            return Some(f);
        }
        if let Some(found) = find(&f.children, name) {
            return Some(found);
        }
    }
    None
}

fn unit(src: &str, name: &str) -> FunctionReport {
    let r = analyze(src);
    assert!(r.parse_errors.is_empty(), "{:?}", r.parse_errors);
    find(&r.functions, name)
        .unwrap_or_else(|| panic!("function {name} not found"))
        .clone()
}

fn cognitive_of(src: &str, name: &str) -> u32 {
    unit(src, name).cognitive
}

fn cyclomatic_of(src: &str, name: &str) -> u32 {
    unit(src, name).cyclomatic
}

#[test]
fn sonar_sum_of_primes_is_7() {
    let src = r#"
sum_of_primes() {
  local max=$1 total=0 i j
  for ((i = 2; i <= max; i++)); do
    for ((j = 2; j < i; j++)); do
      if ((i % j == 0)); then
        continue 2
      fi
    done
    ((total += i))
  done
  echo "$total"
}
"#;
    assert_eq!(cognitive_of(src, "sum_of_primes"), 7);
    assert_eq!(cyclomatic_of(src, "sum_of_primes"), 4);
}

#[test]
fn sonar_get_words_is_1() {
    let src = r#"
get_words() {
  case $1 in
    1) echo "one" ;;
    2) echo "a couple" ;;
    3) echo "a few" ;;
    *) echo "lots" ;;
  esac
}
"#;
    assert_eq!(cognitive_of(src, "get_words"), 1);
    assert_eq!(cyclomatic_of(src, "get_words"), 4);
}

#[test]
fn both_function_syntaxes_are_units() {
    let src = "a() { :; }\nfunction b { :; }\nfunction c() { :; }\n";
    let r = analyze(src);
    let names: Vec<&str> = r.functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
    assert!(r.functions.iter().all(|f| f.kind == "function"));
}

#[test]
fn subshell_bodied_function_is_a_unit() {
    let src = "f() ( if a; then b; fi )\n";
    assert_eq!(cognitive_of(src, "f"), 1);
}

#[test]
fn nested_if_adds_nesting() {
    let src = "f() {\n  if a; then\n    if b; then\n      c\n    fi\n  fi\n}\n";
    assert_eq!(cognitive_of(src, "f"), 3);
    assert_eq!(cyclomatic_of(src, "f"), 3);
}

#[test]
fn elif_else_are_flat() {
    let src = "f() {\n  if a; then x\n  elif b; then y\n  elif c; then z\n  else w\n  fi\n}\n";
    assert_eq!(cognitive_of(src, "f"), 4);
    assert_eq!(cyclomatic_of(src, "f"), 4);
}

#[test]
fn nested_construct_inside_elif_gets_the_elif_nesting() {
    // if +1, elif +1, nested if inside the elif body +2.
    let src = "f() {\n  if a; then x\n  elif b; then\n    if c; then y; fi\n  fi\n}\n";
    assert_eq!(cognitive_of(src, "f"), 4);
}

#[test]
fn elif_condition_is_scored_outside_its_body() {
    // A logical sequence in the elif condition is +1 and does not nest.
    let src = "f() {\n  if a; then x\n  elif b && c; then y\n  fi\n}\n";
    assert_eq!(cognitive_of(src, "f"), 3);
    assert_eq!(cyclomatic_of(src, "f"), 4);
}

#[test]
fn multi_command_if_condition_is_the_test() {
    let src = "f() {\n  if a; b && c; then\n    x\n  fi\n}\n";
    assert_eq!(cognitive_of(src, "f"), 2);
}

#[test]
fn all_loops_count() {
    let src = r#"
f() {
  for i in a b; do :; done
  for ((i = 0; i < 3; i++)); do :; done
  while true; do :; done
  until false; do :; done
  select x in a b; do :; done
}
"#;
    assert_eq!(cognitive_of(src, "f"), 5);
    assert_eq!(cyclomatic_of(src, "f"), 6);
}

#[test]
fn case_items_with_several_patterns_are_one_decision() {
    let src = "f() {\n  case $1 in\n    a|b|c) x ;;\n    d) y ;;\n  esac\n}\n";
    assert_eq!(cognitive_of(src, "f"), 1);
    assert_eq!(cyclomatic_of(src, "f"), 3);
}

#[test]
fn quoted_star_is_not_the_default_arm() {
    let src = "f() {\n  case $1 in\n    \"*\") x ;;\n    *) y ;;\n  esac\n}\n";
    assert_eq!(cyclomatic_of(src, "f"), 2);
}

#[test]
fn star_inside_a_pattern_list_is_not_the_default_arm() {
    let src = "f() {\n  case $1 in\n    a|*) x ;;\n  esac\n}\n";
    assert_eq!(cyclomatic_of(src, "f"), 2);
}

#[test]
fn command_lists_fold_by_operator() {
    // `a && b && c` is one run; `|| d` starts a second one.
    let src = "f() {\n  a && b && c || d\n}\n";
    assert_eq!(cognitive_of(src, "f"), 2);
    assert_eq!(cyclomatic_of(src, "f"), 4);
}

#[test]
fn guard_idiom_is_a_logical_sequence() {
    let src = "f() {\n  cd \"$1\" || return 1\n}\n";
    assert_eq!(cognitive_of(src, "f"), 1);
    assert_eq!(cyclomatic_of(src, "f"), 2);
}

#[test]
fn double_bracket_operators_fold() {
    let src = "f() {\n  if [[ -n $a && -n $b && -z $c ]]; then x; fi\n}\n";
    assert_eq!(cognitive_of(src, "f"), 2);
    assert_eq!(cyclomatic_of(src, "f"), 4);
}

#[test]
fn mixed_double_bracket_operators_count_each_run() {
    let src = "f() {\n  [[ $a == 1 && $b == 2 || $c == 3 ]]\n}\n";
    assert_eq!(cognitive_of(src, "f"), 2);
}

#[test]
fn parenthesized_like_operators_fold_into_one_run() {
    let src = "f() {\n  [[ $a && ( $b && $c ) ]]\n}\n";
    assert_eq!(cognitive_of(src, "f"), 1);
    assert_eq!(cyclomatic_of(src, "f"), 3);
}

#[test]
fn arithmetic_logical_operators_fold() {
    let src = "f() {\n  (( a > 0 || b > 0 || c > 0 ))\n}\n";
    assert_eq!(cognitive_of(src, "f"), 1);
    assert_eq!(cyclomatic_of(src, "f"), 3);
}

#[test]
fn arithmetic_ternary_is_a_conditional() {
    let src = "f() {\n  y=$(( x > 0 ? 1 : 2 ))\n}\n";
    assert_eq!(cognitive_of(src, "f"), 1);
    assert_eq!(cyclomatic_of(src, "f"), 2);
}

#[test]
fn negation_scores_nothing() {
    let src = "f() {\n  ! a\n  [[ ! -f x ]]\n}\n";
    assert_eq!(cognitive_of(src, "f"), 0);
}

#[test]
fn pipelines_score_nothing() {
    let src = "f() {\n  a | b | c\n}\n";
    assert_eq!(cognitive_of(src, "f"), 0);
    assert_eq!(cyclomatic_of(src, "f"), 1);
}

#[test]
fn plain_break_and_continue_score_nothing() {
    let src = "f() {\n  while a; do\n    break\n    continue 1\n  done\n}\n";
    assert_eq!(cognitive_of(src, "f"), 1);
}

#[test]
fn multi_level_break_is_a_labelled_jump() {
    let src = "f() {\n  while a; do\n    while b; do\n      break 2\n    done\n  done\n}\n";
    assert_eq!(cognitive_of(src, "f"), 4);
}

#[test]
fn recursion_adds_one() {
    let src = "fact() {\n  if (( $1 <= 1 )); then echo 1; else echo $(( $1 * $(fact $(( $1 - 1 ))) )); fi\n}\n";
    // if +1, else +1, recursive call +1.
    assert_eq!(cognitive_of(src, "fact"), 3);
}

#[test]
fn constructs_inside_command_substitution_count() {
    let src = "f() {\n  x=\"$(if a; then b; fi)\"\n}\n";
    assert_eq!(cognitive_of(src, "f"), 1);
}

#[test]
fn nested_function_is_its_own_unit() {
    let src = "outer() {\n  if a; then\n    inner() { if b; then c; fi; }\n  fi\n}\n";
    assert_eq!(cognitive_of(src, "outer"), 1);
    assert_eq!(cognitive_of(src, "inner"), 1);
    let r = analyze(src);
    assert_eq!(r.functions.len(), 1);
    assert_eq!(r.functions[0].children[0].name, "inner");
}

#[test]
fn module_level_code_counts_toward_the_file() {
    let src = "if a; then b; fi\nf() { while c; do :; done; }\n";
    let r = analyze(src);
    assert_eq!(r.functions.len(), 1);
    assert_eq!(r.cognitive, 2);
}

#[test]
fn comment_between_if_parts_does_not_change_score() {
    let src =
        "f() {\n  if a; then # why\n    # note\n    x\n  # else coming\n  else\n    y\n  fi\n}\n";
    assert_eq!(cognitive_of(src, "f"), 2);
}

#[test]
fn bats_test_block_is_a_test_unit() {
    let src = r#"#!/usr/bin/env bats

setup() {
  load helper
}

@test "addition works" {
  run add 1 2
  if [ "$status" -ne 0 ]; then
    false
  fi
}

@test 'single quoted' { :; }

@test unquoted name { :; }
"#;
    let r = analyze_source(Path::new("t.bats"), src);
    assert!(r.parse_errors.is_empty(), "{:?}", r.parse_errors);
    let got: Vec<(&str, &str, u32)> = r
        .functions
        .iter()
        .map(|f| (f.name.as_str(), f.kind.as_str(), f.cognitive))
        .collect();
    assert_eq!(
        got,
        [
            ("setup", "function", 0),
            ("addition works", "test", 1),
            ("single quoted", "test", 0),
            ("unquoted name", "test", 0),
        ]
    );
}

#[test]
fn bats_test_marker_function_is_a_test_unit() {
    let src = "check() { # @test\n  [[ a && b ]]\n}\n";
    let r = analyze_source(Path::new("t.bats"), src);
    let f = find(&r.functions, "check").unwrap();
    assert_eq!(f.kind, "test");
    assert_eq!(f.cognitive, 1);
}

#[test]
fn posix_sh_parses() {
    let src = r#"#!/bin/sh
set -eu
usage() {
  echo "usage: $0 [-v] file" >&2
  exit 2
}
while getopts v opt; do
  case $opt in
    v) verbose=1 ;;
    *) usage ;;
  esac
done
[ -n "${verbose:-}" ] && echo verbose
"#;
    let r = analyze(src);
    assert!(r.parse_errors.is_empty(), "{:?}", r.parse_errors);
    assert_eq!(r.functions.len(), 1);
    // while +1, case +2 (nested), `&&` +1.
    assert_eq!(r.cognitive, 4);
}

#[test]
fn parse_error_is_reported() {
    let r = analyze("f() {\n  if a; then\n}\n");
    assert!(!r.parse_errors.is_empty());
}
