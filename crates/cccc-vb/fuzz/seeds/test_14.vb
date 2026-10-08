
Module P
    Sub M()
#If DEBUG Then
        Log("a")
#ElseIf TRACE AndAlso Not RELEASE Then
        Log("b")
#Else
        If x Then Log("c")
#End If
    End Sub
End Module
