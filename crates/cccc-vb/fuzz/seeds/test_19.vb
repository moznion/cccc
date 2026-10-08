
    Dim p = New Person With {.Name = If(a, "x", "y"), .Age = 3}
    Dim xs = New List(Of Integer) From {1, 2}
    With p
        .Name = "z"
        If .Age > 1 Then .Grow()
    End With
