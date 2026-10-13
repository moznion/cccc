
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
