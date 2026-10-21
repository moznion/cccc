Module Sample
    Function sumOfPrimes(max As Integer) As Integer
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
