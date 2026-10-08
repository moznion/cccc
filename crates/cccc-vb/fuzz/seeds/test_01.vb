
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
