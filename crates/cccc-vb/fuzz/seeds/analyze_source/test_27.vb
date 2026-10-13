
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
