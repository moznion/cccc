//! Visual Basic .NET adapter: parses source with a purpose-built pure-Rust
//! lexer and recursive-descent parser and lowers it into the
//! language-agnostic [`cccc_core::ir`].
//!
//! There is no usable off-the-shelf VB.NET parser for Rust: the available
//! tree-sitter grammars fail on most real-world files (implicit line
//! continuation, `TypeOf … Is`, `?.`, generic member access, …), so this
//! crate ships its own. VB suits a hand-written parser: statements are
//! line-oriented and every block ends with its own terminator (`End If`,
//! `Next`, `Loop`, `End Sub`, …), which keeps error recovery local.
//!
//! This is a pure library — it depends only on `cccc-core`, with no CLI
//! machinery and no C toolchain. The unified `cccc` binary (the `cccc-cli`
//! crate) registers this adapter's [`analyze_source`]/[`DEFAULT_EXTS`] and
//! dispatches `.vb` files to it.
//!
//! This crate contains **no scoring logic** — it only recognizes the constructs
//! the engine cares about and emits the matching IR nodes. All complexity rules
//! live in [`cccc_core::engine`].
//!
//! ## VB-to-IR mapping notes
//!
//! - `Sub`, `Function`, `Operator`, property accessors (`Get` / `Set`),
//!   custom-event accessors (`AddHandler` / `RemoveHandler` / `RaiseEvent`),
//!   and lambdas (single- and multi-line `Sub(…)` / `Function(…)`) →
//!   [`Node::Function`]. Bodyless declarations (`MustOverride`, interface
//!   members, `Declare`, `Delegate`, auto-properties, an empty `Partial`
//!   method declaration) are not reported.
//! - `If` (block and single-line forms) → [`Node::Branch`] (`ElseIf` chains as
//!   a nested `Branch`).
//! - `#If` / `#ElseIf` / `#Else` → a [`Node::Branch`] chain, like the C/C++/C#
//!   adapters. When the arms do not nest as complete statements (e.g. two
//!   alternative method signatures sharing one body), only the first arm is
//!   parsed — as a compiler with its symbols defined would — and the chain is
//!   scored without bodies. `#Region` and other directives are transparent.
//! - `If(c, a, b)` → [`Node::Conditional`]; `If(a, b)` → a coalescing
//!   [`Node::Logical`].
//! - `Select Case` → [`Node::Switch`] (`Case Else` is the non-decision arm;
//!   one decision per `Case` clause, however many values it lists).
//! - `For` / `For Each` / `Do … Loop` / `While` → [`Node::Loop`] (`Next i, j`
//!   closes several loops).
//! - one [`Node::Catch`] per `Catch` clause (a `When` filter scores inside it;
//!   `Try` and `Finally` bodies run at the surrounding level).
//! - `GoTo`, `On Error GoTo label`, `On Error Resume Next`, and `Resume` →
//!   [`Node::Jump`] with `labeled: true` (`On Error GoTo 0` / `-1` only reset
//!   the handler and are not jumps); `Exit …` / `Continue …` → unlabeled.
//! - `AndAlso` / `And` and `OrElse` / `Or` runs → folded [`Node::Logical`].
//!   The eager `And` / `Or` count like their short-circuit forms (and share
//!   their run): they are VB's original logical operators and are routinely
//!   used in conditions. `Not` and `Xor` are transparent.
//! - each null-conditional access `?.` / `?(…)` / `?!` → one
//!   [`Node::NullGuard`].
//! - invocations (and a statement that is a bare name, VB's parenthesis-free
//!   `Sub` call) → [`Node::Call`] for recursion detection, matched
//!   case-insensitively; `MyBase.M()` calls the overridden member, so it is
//!   never recursion.
//!
//! [`Node::Function`]: cccc_core::ir::Node::Function
//! [`Node::Branch`]: cccc_core::ir::Node::Branch
//! [`Node::Conditional`]: cccc_core::ir::Node::Conditional
//! [`Node::Logical`]: cccc_core::ir::Node::Logical
//! [`Node::Switch`]: cccc_core::ir::Node::Switch
//! [`Node::Loop`]: cccc_core::ir::Node::Loop
//! [`Node::Catch`]: cccc_core::ir::Node::Catch
//! [`Node::Jump`]: cccc_core::ir::Node::Jump
//! [`Node::NullGuard`]: cccc_core::ir::Node::NullGuard
//! [`Node::Call`]: cccc_core::ir::Node::Call

mod lexer;
mod parser;

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::Node;
use cccc_core::report::FileReport;

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["vb"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` and lower it to the complexity IR, returning the module-level
/// nodes plus any syntax-error messages. The parser recovers from errors at
/// statement granularity, so it still lowers everything it understood and
/// reports the error locations alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    let tokens = lexer::tokenize(source);
    let mut parser = parser::Parser::new(source, &tokens);
    let nodes = parser.parse_file();
    let mut seen = std::collections::HashSet::new();
    let errors = parser
        .errors
        .into_iter()
        .filter(|line| seen.insert(*line))
        .map(|line| format!("syntax error at line {line}"))
        .collect();
    (nodes, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("Test.vb"), src)
    }

    /// Every unit of the report, depth-first in source order.
    fn flatten(fns: &[FunctionReport]) -> Vec<&FunctionReport> {
        fns.iter()
            .flat_map(|f| std::iter::once(f).chain(flatten(&f.children)))
            .collect()
    }

    fn function<'a>(report: &'a FileReport, name: &str) -> &'a FunctionReport {
        flatten(&report.functions)
            .into_iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("function {name} not found"))
    }

    /// `(cognitive, cyclomatic)` of the named unit, asserting a clean parse.
    fn scores(src: &str, name: &str) -> (u32, u32) {
        assert_clean(src);
        let report = analyze(src);
        let f = function(&report, name);
        (f.cognitive, f.cyclomatic)
    }

    fn parse_errors(src: &str) -> Vec<String> {
        to_ir(Path::new("T.vb"), src).1
    }

    fn assert_clean(src: &str) {
        let errors = parse_errors(src);
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// `name:kind` of every unit, depth-first in source order.
    fn units(src: &str) -> Vec<String> {
        assert_clean(src);
        flatten(&analyze(src).functions)
            .iter()
            .map(|f| format!("{}:{}", f.name, f.kind))
            .collect()
    }

    /// Wrap statements in `Module M` / `Function F` so a test can focus on
    /// the body.
    fn body_scores(body: &str) -> (u32, u32) {
        let src =
            format!("Module M\nFunction F(a, b, c) As Object\n{body}\nEnd Function\nEnd Module\n");
        scores(&src, "F")
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        let src = r#"
Module M
    Function SumOfPrimes(max As Integer) As Integer
        Dim total As Integer = 0
        For i As Integer = 2 To max
            For j As Integer = 2 To i - 1
                If i Mod j = 0 Then
                    GoTo NextI
                End If
            Next
            total += i
NextI:
        Next
        Return total
    End Function
End Module
"#;
        // For(+1) + nested For(+2) + nested If(+3) + GoTo(+1) = 7;
        // base 1 + For + For + If = 4
        assert_eq!(scores(src, "SumOfPrimes"), (7, 4));
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
Module M
    Function GetWords(number As Integer) As String
        Select Case number
            Case 1
                Return "one"
            Case 2
                Return "a couple"
            Case Else
                Return "lots"
        End Select
    End Function
End Module
"#;
        // base 1 + 2 non-default cases = 3
        assert_eq!(scores(src, "GetWords"), (1, 3));
    }

    #[test]
    fn if_elseif_else_and_single_line_if() {
        let body = r#"
    If a > 0 Then
        Foo()
    ElseIf a < 0 Then
        Bar()
    Else
        Baz()
    End If
    If a = 5 Then Foo() Else Bar()
"#;
        // If(+1) ElseIf(+1) Else(+1); single-line If(+1) Else(+1)
        assert_eq!(body_scores(body), (5, 4));
    }

    #[test]
    fn else_if_spelled_as_two_words_and_then_less_block_if() {
        let body = r#"
    If a > 0
        Foo()
    Else If a < 0 Then
        Bar()
    EndIf
"#;
        assert_eq!(body_scores(body), (2, 3));
    }

    #[test]
    fn nested_single_line_ifs_take_nesting() {
        let body = "    If a Then If b Then Foo() : Bar() Else Baz()";
        // outer If(+1); inner If(+2) with its Else(+1)
        assert_eq!(body_scores(body), (4, 3));
    }

    #[test]
    fn like_logical_operators_fold() {
        assert_eq!(body_scores("Return a AndAlso b AndAlso c"), (1, 3));
        assert_eq!(body_scores("Return a AndAlso b OrElse c"), (2, 3));
        assert_eq!(body_scores("Return (a AndAlso b) AndAlso c"), (1, 3));
    }

    #[test]
    fn eager_and_or_count_like_short_circuit_forms() {
        assert_eq!(body_scores("Return a And b"), (1, 2));
        assert_eq!(body_scores("Return a Or b Or c"), (1, 3));
        // `And` and `AndAlso` share a precedence level: one run.
        assert_eq!(body_scores("Return a And b AndAlso c"), (1, 3));
        assert_eq!(body_scores("Return a Or b AndAlso c"), (2, 3));
    }

    #[test]
    fn not_and_xor_are_transparent() {
        assert_eq!(body_scores("Return a Xor b"), (0, 1));
        // `Not (…)` stops the outer run from absorbing the inner one.
        assert_eq!(body_scores("Return Not (a OrElse b) AndAlso c"), (2, 3));
        assert_eq!(body_scores("a = Not b"), (0, 1));
    }

    #[test]
    fn if_operator_is_ternary_or_coalesce() {
        let body = r#"
    If c Then
        Return If(c, a, b)
    End If
    Return If(a, b)
"#;
        // If(+1), nested ternary(+2), coalesce(+1)
        assert_eq!(body_scores(body), (4, 4));
    }

    #[test]
    fn null_conditional_access_adds_paths_only() {
        assert_eq!(body_scores("Return a?.b?.c"), (0, 3));
        assert_eq!(body_scores("Return a?(0)"), (0, 2));
    }

    #[test]
    fn loops() {
        let body = r#"
    For i = 0 To 10 Step 2
        For Each x In a
        Next x
    Next i
    Do While a
    Loop
    Do
    Loop Until b
    While c
    End While
    For i = 0 To 1
        For j = 0 To 1
    Next j, i
    Dim y = 1
"#;
        // For(+1) ForEach(+2) DoWhile(+1) Do(+1) While(+1) For(+1) For(+2)
        assert_eq!(body_scores(body), (9, 8));
    }

    #[test]
    fn catch_clauses_score_try_and_finally_do_not() {
        let body = r#"
    Try
        If a Then Foo()
    Catch ex As IOException When ex.HResult = 1
        If b Then Bar()
    Catch
    Finally
        Baz()
    End Try
"#;
        // If(+1); Catch(+1) + nested If(+2); Catch(+1)
        assert_eq!(body_scores(body), (5, 5));
    }

    #[test]
    fn select_case_clause_lists_are_one_decision() {
        let body = r#"
    Select Case a
        Case 1, 2, 3
            Foo()
        Case 4 To 9, Is > 100
            Bar()
    End Select
"#;
        assert_eq!(body_scores(body), (1, 3));
    }

    #[test]
    fn goto_and_legacy_error_handling_are_jumps() {
        let src = r#"
Module M
    Sub OldStyle()
        On Error GoTo Handler
        DoWork()
        Exit Sub
Handler:
        Resume Next
    End Sub
    Sub Quiet()
        On Error Resume Next
        DoWork()
        On Error GoTo 0
    End Sub
End Module
"#;
        // `On Error GoTo Handler`(+1), `Resume Next`(+1); `Exit Sub` is free
        assert_eq!(scores(src, "OldStyle"), (2, 1));
        // `On Error GoTo 0` only resets the handler
        assert_eq!(scores(src, "Quiet"), (1, 1));
    }

    #[test]
    fn exit_and_continue_are_free() {
        let body = r#"
    For Each x In a
        If x Then Continue For
        If b Then Exit For
    Next
"#;
        assert_eq!(body_scores(body), (5, 4));
    }

    #[test]
    fn units_and_their_kinds() {
        let src = r#"
Public MustInherit Class Shape
    Implements IDisposable
    Private _w As Double
    Public Sub New()
    End Sub
    Public MustOverride Function Area() As Double
    Public Property Name As String
    Public ReadOnly Property Size As Integer = 3
    Public Property Width As Double
        Get
            Return _w
        End Get
        Private Set(value As Double)
            _w = value
        End Set
    End Property
    Public Shared Operator +(a As Shape, b As Shape) As Shape
        Return a
    End Operator
    Public Custom Event Changed As EventHandler
        AddHandler(value As EventHandler)
        End AddHandler
        RemoveHandler(value As EventHandler)
        End RemoveHandler
        RaiseEvent(sender As Object, e As EventArgs)
        End RaiseEvent
    End Event
    Partial Private Sub OnCreated()
    End Sub
    Private Declare Function GetTickCount Lib "kernel32" () As Integer
    Delegate Sub Notify(message As String)
    Function Run() As Integer
        Dim f = Function(x As Integer) x * 2
        Dim g = Sub()
                    Console.WriteLine()
                End Sub
        Return f(1)
    End Function
End Class

Interface IShape
    Sub Draw()
    Function Area() As Double
    Property Color As Integer
End Interface
"#;
        assert_eq!(
            units(src),
            vec![
                "New:constructor",
                "Width:getter",
                "Width:setter",
                "operator +:operator",
                "Changed:add",
                "Changed:remove",
                "Changed:raise",
                "Run:function",
                "<lambda>:lambda",
                "<lambda>:lambda",
            ]
        );
    }

    #[test]
    fn lambdas_score_in_their_own_frame() {
        let src = r#"
Module M
    Sub Main()
        Task.Run(Sub()
                     If ready Then Go()
                 End Sub)
        Dim pick = Function(x) If(x > 0, x, -x)
        AddHandler b.Click, Async Sub(s, e) Await Save()
    End Sub
End Module
"#;
        let report = analyze(src);
        assert_clean(src);
        let main = function(&report, "Main");
        assert_eq!((main.cognitive, main.cyclomatic), (0, 1));
        let lambdas: Vec<(u32, u32)> = main
            .children
            .iter()
            .map(|f| (f.cognitive, f.cyclomatic))
            .collect();
        assert_eq!(lambdas, vec![(1, 2), (1, 2), (0, 1)]);
    }

    #[test]
    fn recursion_is_case_insensitive_and_includes_bare_calls() {
        let src = r#"
Module R
    Function Fact(n As Integer) As Integer
        If n <= 1 Then Return 1
        Fact = n * fact(n - 1)
    End Function
    Sub Walk(node As Node)
        If node Is Nothing Then Exit Sub
        walk
    End Sub
End Module
Class Derived : Inherits Base
    Overrides Sub Save()
        MyBase.Save()
        Me.Flush()
    End Sub
End Class
"#;
        // `Fact = …` assigns the return variable; `fact(n - 1)` recurses
        assert_eq!(scores(src, "Fact"), (2, 2));
        // a bare statement name calls the parameterless Sub
        assert_eq!(scores(src, "Walk"), (2, 2));
        assert_eq!(scores(src, "Save"), (0, 1));
    }

    #[test]
    fn implicit_and_explicit_line_continuation() {
        let src = r#"
Module C
    Sub Handle(sender As Object,
               e As EventArgs) Handles Button1.Click,
                                       Button2.Click
        If sender IsNot Nothing AndAlso
           e IsNot Nothing Then
            Dim q = From x In items
                    Where x.Enabled AndAlso x.Visible
                    Order By x.Name Descending
                    Select x
            Process(q,
                    If(x, y))
        End If
        If a _
           OrElse b Then Foo()
    End Sub
End Module
"#;
        // If(+1) AndAlso(+1); Where's AndAlso(+1); coalesce(+1);
        // single-line If(+1) OrElse(+1)
        assert_eq!(scores(src, "Handle"), (6, 7));
    }

    #[test]
    fn preprocessor_conditionals_are_branches() {
        let src = r#"
Module P
    Sub M()
#If DEBUG Then
        Log("a")
#ElseIf TRACE AndAlso Not RELEASE Then
        Log("b")
#Else
        If x Then Log("c")
#End If
    End Sub
End Module
"#;
        // #If(+1) #ElseIf(+1) AndAlso(+1) #Else(+1) + If nested in #Else(+2)
        assert_eq!(scores(src, "M"), (6, 5));
    }

    #[test]
    fn preprocessor_split_signature_parses_the_first_arm() {
        let src = r#"
Module P
#If NET48 Then
    Sub N(a As Integer)
#Else
    Sub N(a As Long)
#End If
        If a > 0 Then Log()
    End Sub
#Region "Helpers"
    Sub Q()
    End Sub
#End Region
End Module
"#;
        assert_eq!(units(src), vec!["N:sub", "Q:sub"]);
        assert_eq!(scores(src, "N"), (1, 2));
        // The chain itself scores at the module level.
        let report = analyze(src);
        assert_eq!(report.cognitive, 3);
    }

    #[test]
    fn comments_and_strings_are_inert() {
        let body = r#"
    ' If a Then
    REM If b Then
    Dim s = "If c Then AndAlso ""quoted"""
    Dim ch = "x"c
    Dim t = $"{If(c, 1, 2)} and {{If}} {a?.b,5:N2}"
    Dim d = #1/2/2024#
"#;
        // the ternary and the `?.` inside the interpolation holes count
        assert_eq!(body_scores(body), (1, 3));
    }

    #[test]
    fn keywords_are_case_insensitive_and_escapable() {
        let body = r#"
    IF a THEN
        Dim [Next] = 1
        x.End = [Next]
    END IF
    Return a ANDALSO b
"#;
        assert_eq!(body_scores(body), (2, 3));
    }

    #[test]
    fn xml_literals_are_opaque() {
        let body = r#"
    Dim doc = <root kind="a">
                  <item><%= If(a, 1, 2) %></item>
              </root>
    Dim name = doc.<item>.@kind
    If a Then Foo()
"#;
        assert_eq!(body_scores(body), (1, 2));
    }

    #[test]
    fn with_blocks_and_initializers() {
        let body = r#"
    Dim p = New Person With {.Name = If(a, "x", "y"), .Age = 3}
    Dim xs = New List(Of Integer) From {1, 2}
    With p
        .Name = "z"
        If .Age > 1 Then .Grow()
    End With
"#;
        assert_eq!(body_scores(body), (2, 3));
    }

    #[test]
    fn syntax_errors_are_reported_and_the_rest_still_scores() {
        let src = r#"
Module M
    Sub Broken()
        Dim x = )
    End Sub
    Sub Fine()
        If a Then Foo()
    End Sub
End Module
"#;
        assert_eq!(parse_errors(src), vec!["syntax error at line 4"]);
        let report = analyze(src);
        assert_eq!(function(&report, "Fine").cognitive, 1);
    }

    #[test]
    fn flat_preprocessor_chain_scores_elseif_conditions() {
        let src = r#"
Module P
#If A Then
    Sub N(a As Integer)
#ElseIf B AndAlso C Then
    Sub N(a As Long)
#Else
    Sub N(a As Short)
#End If
        Foo()
    End Sub
End Module
"#;
        assert_eq!(units(src), vec!["N:sub"]);
        // #If(+1) #ElseIf(+1) AndAlso(+1) #Else(+1), at the module level
        assert_eq!(analyze(src).cognitive, 4);
    }

    #[test]
    fn two_word_else_if_directive() {
        let body = r#"
#If A Then
    Foo()
#Else If B Then
    Bar()
#End If
"#;
        assert_eq!(body_scores(body), (2, 3));
    }

    #[test]
    fn unterminated_preprocessor_group_is_reported() {
        let src = r#"
Module M
    Sub A()
#If X Then
        If a Then Foo()
    End Sub
End Module
"#;
        assert!(!parse_errors(src).is_empty());
        // the (flat) #If chain(+1) and the If(+1) both sit inside A
        assert_eq!(function(&analyze(src), "A").cognitive, 2);
    }

    #[test]
    fn code_in_constructor_arguments_and_declarators_scores() {
        let body = r#"
    Dim p As New Person(If(a, 1, 2))
    Dim xs(If(b, 3, 4)) As Integer, ys()() As String
    Dim q = New Foo(Function(x) x) With {.A = 1}
    Using r As New Reader(If(c, "a", "b"))
    End Using
    ReDim Preserve xs(If(a, 5, 6))
    Erase xs, ys
"#;
        // four ternaries; the lambda is its own unit
        assert_eq!(body_scores(body), (4, 5));
    }

    #[test]
    fn event_statements_score_their_arguments() {
        let body = r#"
    RaiseEvent Changed(If(a, 1, 2))
    AddHandler btn.Click, Sub(s, e) Foo()
    RemoveHandler btn.Click, AddressOf Handler
"#;
        assert_eq!(body_scores(body), (1, 2));
    }

    #[test]
    fn tuples_and_generic_calls() {
        let src = r#"
Module M
    Function Pick(Of T)(x As T, n As Integer) As T
        Dim t = (a:=If(n > 0, 1, 2), b:=3)
        Dim u = (1, If(n > 1, 2, 3))
        Return Pick(Of T)(x, n - 1)
    End Function
End Module
"#;
        // two ternaries + recursion through the generic call
        assert_eq!(scores(src, "Pick"), (3, 3));
    }

    #[test]
    fn async_and_iterator_members() {
        let src = r#"
Class C
    Public Async Function LoadAsync() As Task
        Dim f = Async Function(x) Await G(x)
        If a Then Await H()
    End Function
    Public Iterator Function Items() As IEnumerable(Of Integer)
        Yield If(a, 1, 2)
    End Function
    Public ReadOnly Iterator Property Values As IEnumerable(Of Integer)
        Get
            Yield 1
        End Get
    End Property
End Class
"#;
        assert_eq!(
            units(src),
            vec![
                "LoadAsync:function",
                "<lambda>:lambda",
                "Items:function",
                "Values:getter"
            ]
        );
        assert_eq!(scores(src, "LoadAsync"), (1, 2));
        assert_eq!(scores(src, "Items"), (1, 2));
    }

    #[test]
    fn query_clauses_across_lines() {
        let body = r#"
    Dim q = From c In customers, o As Order In orders
            Join p In products On o.Pid Equals p.Id
            Where c.Ok AndAlso o.Ok
            Group By c.City Into Count()
            Distinct
    Dim n = Aggregate x In xs Into Sum(x)
    If a Then Foo()
"#;
        // Where's AndAlso(+1), If(+1)
        assert_eq!(body_scores(body), (2, 3));
    }

    #[test]
    fn dictionary_null_guard_and_xml_axes() {
        let body = r#"
    Dim v = x.@id
    Dim w = x...<item>.<name>
    Dim z = x.@<ns:attr>
    Return d?!key
"#;
        assert_eq!(body_scores(body), (0, 2));
    }

    #[test]
    fn literal_and_identifier_forms() {
        let body = "
    Dim h = &HFF + &O17 + &B1010 + 1.5E+3 + 10UL + 2.5! + 7&
    Dim s$ = \"x\" : Dim n% = 3
    Dim u = \u{201c}smart \u{201c}\u{201c}quoted\u{201d}\u{201d} If\u{201d} \u{2018} comment If x Then
    Dim \u{540d}\u{524d} = 1
    If \u{540d}\u{524d} > 0 Then Foo()
    Dim t = a _ ' comment after the continuation
        + b
";
        assert_eq!(body_scores(body), (1, 2));
    }

    #[test]
    fn xml_document_forms_are_opaque() {
        let body = r#"
    Dim doc = <?xml version="1.0"?>
              <!-- c -->
              <?pi x?>
              <root><![CDATA[ If x Then ]]><!-- If --><a/></root>
    Dim d2 = <?xml version="1.0"?><%= el %>
    Dim c = <!-- only a comment -->
    Dim cd = <![CDATA[ If ]]>
    Dim e = <<%= name %>>v</>
    If a Then Foo()
"#;
        assert_eq!(body_scores(body), (1, 2));
    }

    #[test]
    fn interface_structure_enum_and_indexed_properties() {
        let src = r#"
Interface I
    Event Changed As EventHandler
    Property Item(i As Integer) As String
End Interface
Structure S
    Implements I
    Public Event Changed As EventHandler Implements I.Changed
    Default Public Property Item(Optional i As Integer = 0) As String Implements I.Item
        Get
            If i > 0 Then Return ""
            Return Nothing
        End Get
        Set(value As String)
        End Set
    End Property
    Public Property Items As New List(Of Integer) From {If(y, 1, 2)}
End Structure
Enum E
    A = 1
    B
End Enum
"#;
        assert_eq!(units(src), vec!["Item:getter", "Item:setter"]);
        assert_eq!(scores(src, "Item"), (1, 2));
    }

    #[test]
    fn stray_terminators_and_unclosed_brackets_recover() {
        let src = r#"
Module M
    Sub A()
        End If
        Foo(1, 2
    End Sub
    Sub B(ByVal a As Integer, 5)
        If b Then Bar()
    End Sub
End Module
Next
"#;
        let errors = parse_errors(src);
        for line in [4, 7, 11] {
            let msg = format!("syntax error at line {line}");
            assert!(errors.contains(&msg), "{msg} missing from {errors:?}");
        }
        let report = analyze(src);
        assert_eq!(function(&report, "A").cognitive, 0);
        assert_eq!(function(&report, "B").cognitive, 1);
    }

    #[test]
    fn attributes_and_type_forms() {
        let src = r#"
<Assembly: CLSCompliant(True)>
<Serializable(), Obsolete("x")>
Public Class C
    <DllImport("user32.dll")>
    Public Shared Function Run(<Out> ByRef a As Integer(), ParamArray rest() As Object) As (Integer, String)
        Dim b As Integer? = Nothing, c As Dictionary(Of String, List(Of Integer))()
        Dim d As New System.Text.StringBuilder()
        Dim e = New Integer() {1, If(x, 2, 3)}
        Return (1, "")
    End Function
End Class
"#;
        assert_eq!(scores(src, "Run"), (1, 2));
    }

    #[test]
    fn call_statement_labels_and_typed_lambdas() {
        let src = r#"
Module M
    Sub Retry(n As Integer)
Again:  If n > 0 Then Call Retry(n - 1)
        Dim f = Function(x As Integer) As Integer
                    Return If(x > 0, x, 0)
                End Function
        ReDim buf(0 To n)
    End Sub
End Module
"#;
        // If(+1) + recursion through `Call`(+1)
        assert_eq!(scores(src, "Retry"), (2, 2));
        let report = analyze(src);
        assert_eq!(function(&report, "Retry").children[0].cognitive, 1);
    }

    #[test]
    fn string_and_identifier_edge_forms() {
        let body = "\u{feff}
    Dim s = $\"say \"\"hi\"\" {If(a, \"x\", \"y\")} {New Integer() {1}(0)}\"
    Dim f! = 1.0 : Dim l& = 2
    Dim aVeryLongIdentifierNameIndeed = 1\u{3000}
    Dim arrow = 1 \u{2192} 2
    Return <a/>
";
        // the ternary inside the interpolation hole; `→` is a stray symbol
        let src =
            format!("Module M\nFunction F(a, b, c) As Object\n{body}\nEnd Function\nEnd Module\n");
        let report = analyze(&src);
        assert_eq!(function(&report, "F").cognitive, 1);
    }

    #[test]
    fn preprocessor_group_inside_an_argument_list_parses_its_first_arm() {
        let body = r#"
    Foo(a,
#If DEBUG Then
        If(b, 1, 2)
#Else
        0
#End If
        )
"#;
        // #If(+1) #Else(+1), plus the ternary in the parsed first arm(+1)
        assert_eq!(body_scores(body), (3, 3));
    }

    #[test]
    fn keyword_table_is_sorted_for_binary_search() {
        assert!(lexer::KEYWORDS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn single_line_lambda_statement_ends_at_the_list_separator() {
        let src = r#"
Module M
    Sub A()
        Foo(Sub() Exit Sub, 1)
    End Sub
    Sub B()
        If b Then Bar()
    End Sub
End Module
"#;
        assert_eq!(scores(src, "B"), (1, 2));
    }

    #[test]
    fn unterminated_method_recovers_at_the_next_member() {
        let src = r#"
Class C
    Sub A()
        If a Then
            Foo()
    End Sub
    Sub B()
        If b Then Bar()
    End Sub
End Class
"#;
        assert!(!parse_errors(src).is_empty());
        let report = analyze(src);
        assert_eq!(function(&report, "B").cognitive, 1);
    }

    #[test]
    fn nested_flat_preproc_groups_parse_in_polynomial_time() {
        // Each group opens a `Sub` its `#End If` does not close, so every
        // structured attempt fails. Retrying the inner groups on each
        // enclosing group's flat re-parse used to cost 2^depth (found by
        // fuzzing).
        let depth = 64;
        let src = "#If A Then\nSub F()\n".repeat(depth) + &"#End If\n".repeat(depth);
        assert!(!parse_errors(&src).is_empty());
    }
}
