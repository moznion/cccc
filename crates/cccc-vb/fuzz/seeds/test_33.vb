
<Assembly: CLSCompliant(True)>
<Serializable(), Obsolete("x")>
Public Class C
    <DllImport("user32.dll")>
    Public Shared Function Run(<Out> ByRef a As Integer(), ParamArray rest() As Object) As (Integer, String)
        Dim b As Integer? = Nothing, c As Dictionary(Of String, List(Of Integer))()
        Dim d As New System.Text.StringBuilder()
        Dim e = New Integer() {1, If(x, 2, 3)}
        Return (1, "")
    End Function
End Class
