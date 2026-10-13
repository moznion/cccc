
Module P
#If A Then
    Sub N(a As Integer)
#ElseIf B AndAlso C Then
    Sub N(a As Long)
#Else
    Sub N(a As Short)
#End If
        Foo()
    End Sub
End Module
