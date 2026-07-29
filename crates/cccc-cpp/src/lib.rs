//! C++ adapter: parses source with the official [tree-sitter] `tree-sitter-cpp`
//! grammar and lowers the concrete syntax tree into the language-agnostic
//! [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `tree-sitter`, and
//! the C++ grammar, with no CLI machinery. The unified `cccc` binary (the
//! `cccc-cli` crate) registers this adapter's [`analyze_source`]/[`DEFAULT_EXTS`]
//! and dispatches C++ files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about and emits the matching IR nodes. All complexity rules
//! live in [`cccc_core::engine`].
//!
//! Everything C and C++ share (functions, `if`, the ternary, loops, `switch`,
//! jumps, logical-operator folding, preprocessor conditionals, calls) is
//! lowered by `cccc_clike::SharedBuilder`, tagged with `Language::Cpp`; see
//! `crates/cccc-clike/src/lib.rs`. This crate supplies only the grammar
//! loading and the extension list. The C++-only constructs are lowered there
//! too (gated on the language tag), not here:
//!
//! - a `[...](...){...}` lambda → [`Node::Function`] (`"<lambda>"`, kind
//!   `"lambda"`), its own scored unit.
//! - `catch` clauses → [`Node::Catch`]; the `try` body runs at the
//!   surrounding level.
//! - range-`for` (`for (auto &x : xs)`) → [`Node::Loop`], same as any other
//!   loop.
//! - method/constructor/destructor/operator names, including out-of-line
//!   qualified definitions (`Foo::bar`), are dug out of the declarator chain
//!   the same way a C function name is.

use std::path::Path;

use cccc_clike::{SharedBuilder, collect_errors};
use cccc_core::engine;
use cccc_core::ir::Node;
use cccc_core::report::FileReport;
use tree_sitter::Node as TsNode;

/// File extensions analyzed by default (when `--ext` is not given). `.h` is
/// deliberately excluded: `cccc-c` already claims it, and extension dispatch
/// requires disjoint claims. Users with C++ in `.h` files can override via
/// `--ext`/the `[ext]` config.
pub const DEFAULT_EXTS: &[&str] = &["cpp", "cc", "cxx", "hpp", "hxx", "h++", "tpp", "ipp"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any syntax-error messages. tree-sitter always yields a tree (it
/// recovers from errors by inserting `ERROR`/`MISSING` nodes), so we still lower
/// what parsed and report the error locations alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .is_err()
    {
        return (Vec::new(), vec!["failed to load C++ grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse C++ source".to_string()]);
    };

    let src = source.as_bytes();
    let mut errors = Vec::new();
    collect_errors(tree.root_node(), &mut errors);

    let mut builder = Builder::new(src);
    builder.visit(tree.root_node());
    (builder.finish(), errors)
}

/// Assembles the IR tree while an explicit recursion walks the tree-sitter CST.
struct Builder<'a>(SharedBuilder<'a>);

impl<'a> Builder<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self(SharedBuilder::new(src, cccc_clike::Language::Cpp))
    }

    /// The module-level node list (the single remaining collector).
    fn finish(self) -> Vec<Node> {
        self.0.finish()
    }

    // ---- traversal --------------------------------------------------------

    fn visit(&mut self, node: TsNode) {
        self.0.visit(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.cpp"), src)
    }

    /// Parse once and assert the source came through clean. Almost every test
    /// below wants this, so `cognitive_of`/`cyclomatic_of` route through it —
    /// a stray parse error now fails the test that hit it, not just the
    /// handful of tests that check `parse_errors` explicitly.
    fn analyze_ok(src: &str) -> FileReport {
        let report = analyze(src);
        assert!(
            report.parse_errors.is_empty(),
            "unexpected parse errors: {:?}",
            report.parse_errors
        );
        report
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

    fn cognitive_of(src: &str, name: &str) -> u32 {
        cognitive_of_report(&analyze_ok(src), name)
    }

    fn cognitive_of_report(report: &FileReport, name: &str) -> u32 {
        find(&report.functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cognitive
    }

    fn cyclomatic_of_report(report: &FileReport, name: &str) -> u32 {
        find(&report.functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cyclomatic
    }

    #[test]
    fn parses_without_error() {
        let src = r#"
            int add(int a, int b) {
                return a + b;
            }
        "#;
        analyze_ok(src);
    }

    #[test]
    fn parse_error_is_reported() {
        let errors = to_ir(
            Path::new("t.cpp"),
            "int ok(int a) { return a; }\nint bad( {\n",
        )
        .1;
        assert!(!errors.is_empty());
    }

    #[test]
    fn shared_c_constructs_still_score() {
        // Same shape as cccc-c's `sonar_get_words_is_1`: proof the shared
        // `cccc_clike` lowering is actually wired up, not just parsing.
        let src = r#"
            const char *get_words(int n) {
                switch (n) {
                    case 1:
                        return "one";
                    case 2:
                        return "a couple";
                    default:
                        return "lots";
                }
            }
        "#;
        assert_eq!(cognitive_of(src, "get_words"), 1);
    }

    #[test]
    fn lambda_is_its_own_unit() {
        let src = r#"
            void host() {
                auto f = [](int x) { if (x) { if (x) { } } };
            }
        "#;
        let report = analyze_ok(src);
        // host owns no structural complexity; the lambda does: +1 +2
        assert_eq!(cognitive_of_report(&report, "host"), 0);
        assert_eq!(cognitive_of_report(&report, "<lambda>"), 3);
        assert_eq!(find(&report.functions, "<lambda>").unwrap().kind, "lambda");
    }

    #[test]
    fn lambda_in_default_argument_is_reached() {
        let src = r#"
            void host(int key = [](int x) { return x ? 1 : 0; }()) {
            }
        "#;
        assert_eq!(cognitive_of(src, "<lambda>"), 1);
    }

    #[test]
    fn range_for_counts_as_loop() {
        let src = r#"
            void f(int a) {
                for (auto &x : items) {
                    if (a) { }
                }
            }
        "#;
        // range-for(+1) + nested if(+2) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn catch_clause_counts_try_body_is_flat() {
        let src = r#"
            void f() {
                try {
                    if (true) { }
                } catch (const std::exception &e) {
                    if (true) { }
                }
            }
        "#;
        // try body runs at the surrounding level: if(+1).
        // catch clause(+1) + nested if inside catch(+2) = 3.
        // total = 1 + 3 = 4
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn out_of_line_qualified_method_is_named() {
        let src = r#"
            class Foo {
                void bar(int retry);
            };
            void Foo::bar(int retry) {
                if (retry) {
                    Foo::bar(0);
                }
            }
        "#;
        // an out-of-line definition is registered under its trailing simple
        // name ("bar", not "Foo::bar") so it lines up with a qualified *call*
        // to the same method, which resolves the same way: if(+1) +
        // recursion(+1) = 2
        assert_eq!(cognitive_of(src, "bar"), 2);
    }

    #[test]
    fn out_of_line_method_unqualified_self_call_is_recursion() {
        let src = r#"
            class Foo {
                void bar(int retry);
            };
            void Foo::bar(int retry) {
                if (retry) {
                    bar(0);
                }
            }
        "#;
        // the far more common case: calling the method unqualified from
        // inside itself must still resolve to the same registered name.
        assert_eq!(cognitive_of(src, "bar"), 2);
    }

    #[test]
    fn destructor_is_named() {
        let src = r#"
            class Foo {
                ~Foo() {
                    if (true) { }
                }
            };
        "#;
        assert_eq!(cognitive_of(src, "~Foo"), 1);
    }

    #[test]
    fn operator_overload_is_named() {
        let src = r#"
            class Foo {
                Foo operator+(const Foo &other) {
                    if (true) { }
                    return *this;
                }
            };
        "#;
        assert_eq!(cognitive_of(src, "operator+"), 1);
    }

    #[test]
    fn preproc_around_functions_still_finds_them() {
        let src = r#"
            #if defined(A) && defined(B)
            int f(int x) {
                if (x) { return 1; }
                return 0;
            }
            #endif
        "#;
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn header_only_class_methods_score_independently() {
        // A class defined entirely inline (as it would be in a header):
        // constructor/destructor with member-initializer lists, a default
        // member initializer, and one method calling another by a different
        // name (not recursion). Every method is its own unit; the class body
        // itself adds no complexity. Cross-checked against `lizard`, an
        // independent C++ CCN tool: Stack::Stack=1, Stack::~Stack=1,
        // Stack::empty=1, Stack::push=2, Stack::pop=2, Stack::grow=1.
        let src = r#"
            class Stack {
            public:
                Stack() : top_(0) {}
                ~Stack() {}

                bool empty() const {
                    return top_ == 0;
                }

                void push(int x) {
                    if (top_ >= capacity_) {
                        grow();
                    }
                    data_[top_++] = x;
                }

                int pop() {
                    if (empty()) {
                        return -1;
                    }
                    return data_[--top_];
                }

            private:
                void grow() {
                    capacity_ *= 2;
                }

                int data_[1024];
                int top_;
                int capacity_ = 1024;
            };
        "#;
        let report = analyze_ok(src);
        assert_eq!(cognitive_of_report(&report, "Stack"), 0);
        assert_eq!(cyclomatic_of_report(&report, "Stack"), 1);
        assert_eq!(cognitive_of_report(&report, "~Stack"), 0);
        assert_eq!(cyclomatic_of_report(&report, "~Stack"), 1);
        assert_eq!(cognitive_of_report(&report, "empty"), 0);
        assert_eq!(cyclomatic_of_report(&report, "empty"), 1);
        assert_eq!(cognitive_of_report(&report, "push"), 1);
        assert_eq!(cyclomatic_of_report(&report, "push"), 2);
        assert_eq!(cognitive_of_report(&report, "pop"), 1);
        assert_eq!(cyclomatic_of_report(&report, "pop"), 2);
        assert_eq!(cognitive_of_report(&report, "grow"), 0);
        assert_eq!(cyclomatic_of_report(&report, "grow"), 1);
        // the class body/access specifiers add nothing of their own
        assert_eq!(report.cognitive, 2);
    }

    #[test]
    fn higher_complexity_combination() {
        // A kitchen sink: switch, range-for containing a lambda (its own
        // scored unit) with a folded `&&`, recursion, a labelled `goto`, and
        // a preprocessor conditional — all in one function, at varying
        // nesting depths. Cross-checked against `lizard`: it can't split the
        // lambda into its own unit (a known limitation, not a contradiction),
        // but its merged cyclomatic count for the whole snippet (7, or 6
        // with the `#ifdef` block removed) equals ours once accounted for:
        // `lizard` counts one base for the merged blob where we count one
        // base per function (process=6 + lambda=2, minus the one duplicate
        // base = 7; and 5 + 2 - 1 = 6 without the preprocessor block).
        let src = r#"
            int process(int n, std::vector<int> &items) {
                switch (n % 3) {
                    case 0:
                        for (auto &x : items) {
                            auto scored = [](int v) {
                                return v > 0 && v < 10;
                            };
                            if (scored(x)) {
                                break;
                            }
                        }
                        break;
                    case 1:
                        return process(n - 1, items);
                    default:
                        goto done;
                }
            #ifdef DEBUG
                log_debug(n);
            #endif
            done:
                return n;
            }
        "#;
        let report = analyze_ok(src);
        // switch(+1) + [case0: for(+1+1=2) + if(+1+2=3)] + [case1: recursion
        // (+1 flat)] + [default: goto (+1 flat)] + preproc(+1) = 9
        assert_eq!(cognitive_of_report(&report, "process"), 9);
        // base(1) + case0(1) + for-range(1) + if(1) + case1(1) + preproc(1)
        // = 6 (default doesn't count; recursion/goto aren't cyclomatic)
        assert_eq!(cyclomatic_of_report(&report, "process"), 6);
        // the lambda scores fully independently: base 1 + && (+1) = 2,
        // cognitive: && fold is flat = 1
        assert_eq!(cognitive_of_report(&report, "<lambda>"), 1);
        assert_eq!(cyclomatic_of_report(&report, "<lambda>"), 2);
    }

    #[test]
    fn test_templated_header() {
        let src = r#"
            template <typename T>
            Queue<T>::~Queue() {
              if (this->data != nullptr) delete[] this->data;
            }
            "#;

        let report = analyze_ok(src);
        assert_eq!(cognitive_of_report(&report, "~Queue"), 1);
    }

    #[test]
    fn test_treesitter_grammar_ambiguity() {
        // This grammar is ambiguous: the parser has problems resolving
        // the paranthesized expresion after the delete.
        let src = r#"
            template <typename T>
            Queue<T>::~Queue() {
              if (this->data != nullptr) delete[] (this->data);
            }
            "#;

        let file_report = analyze(src);
        assert_eq!(file_report.parse_errors.len(), 1);
        assert_eq!(cognitive_of_report(&file_report, "~Queue"), 1);
    }

    #[test]
    fn printf_format_macro_reports_error() {
        // `"..." PRIu32 "..."` (the <cinttypes> printf-width-macro idiom) is
        // another tree-sitter-cpp grammar gap: the grammar's
        // concatenated_string rule expects only string-literal pieces, and
        // an identifier that only becomes a string literal after macro
        // expansion — expansion the grammar never performs — comes through
        // as an ERROR nested inside it. The error stays local to that
        // expression — the surrounding function still lowers and scores
        // fine.
        let src = r#"
            void log_uptime(uint32_t up_time) {
                if (up_time) {
                    log_info("Uptime: %" PRIu32 "\r\n", up_time);
                }
            }
            "#;

        let report = analyze(src);
        assert_eq!(report.parse_errors.len(), 1);
        assert_eq!(cognitive_of_report(&report, "log_uptime"), 1);
    }

    #[test]
    fn explicit_template_instantiation_reports_error() {
        // `template class Foo<T>;` (explicit instantiation, no declarator)
        // this is another tree-sitter-cpp grammar gap: the grammar parses `class Foo<T>` as a
        // class_specifier and then expects a trailing declarator before the `;` (the same
        // shape as `struct Foo x;`), which this syntax never has. The error
        // stays local to that line — the templated class above it still
        // lowers and scores fine.
        let src = r#"
            template <typename T>
            class PeriodicTaskBase {
            public:
                void run(T ticks) {
                    if (ticks) { }
                }
            };

            template class PeriodicTaskBase<uint16_t>;
            "#;

        let report = analyze(src);
        assert_eq!(report.parse_errors.len(), 1);
        assert_eq!(cognitive_of_report(&report, "run"), 1);
    }
}
