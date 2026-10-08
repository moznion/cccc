
Module P
#If NET48 Then
    Sub N(a As Integer)
#Else
    Sub N(a As Long)
#End If
        If a > 0 Then Log()
    End Sub
#Region "Helpers"
    Sub Q()
    End Sub
#End Region
End Module
