
    Try
        If a Then Foo()
    Catch ex As IOException When ex.HResult = 1
        If b Then Bar()
    Catch
    Finally
        Baz()
    End Try
