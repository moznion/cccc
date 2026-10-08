
Module M
    Sub Retry(n As Integer)
Again:  If n > 0 Then Call Retry(n - 1)
        Dim f = Function(x As Integer) As Integer
                    Return If(x > 0, x, 0)
                End Function
        ReDim buf(0 To n)
    End Sub
End Module
