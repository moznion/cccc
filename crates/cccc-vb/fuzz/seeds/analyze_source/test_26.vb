
Module M
    Function Pick(Of T)(x As T, n As Integer) As T
        Dim t = (a:=If(n > 0, 1, 2), b:=3)
        Dim u = (1, If(n > 1, 2, 3))
        Return Pick(Of T)(x, n - 1)
    End Function
End Module
