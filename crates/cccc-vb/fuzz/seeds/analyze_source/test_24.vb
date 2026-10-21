
    Dim p As New Person(If(a, 1, 2))
    Dim xs(If(b, 3, 4)) As Integer, ys()() As String
    Dim q = New Foo(Function(x) x) With {.A = 1}
    Using r As New Reader(If(c, "a", "b"))
    End Using
    ReDim Preserve xs(If(a, 5, 6))
    Erase xs, ys
