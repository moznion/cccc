
Module M
    Sub Main()
        Task.Run(Sub()
                     If ready Then Go()
                 End Sub)
        Dim pick = Function(x) If(x > 0, x, -x)
        AddHandler b.Click, Async Sub(s, e) Await Save()
    End Sub
End Module
