
Public MustInherit Class Shape
    Implements IDisposable
    Private _w As Double
    Public Sub New()
    End Sub
    Public MustOverride Function Area() As Double
    Public Property Name As String
    Public ReadOnly Property Size As Integer = 3
    Public Property Width As Double
        Get
            Return _w
        End Get
        Private Set(value As Double)
            _w = value
        End Set
    End Property
    Public Shared Operator +(a As Shape, b As Shape) As Shape
        Return a
    End Operator
    Public Custom Event Changed As EventHandler
        AddHandler(value As EventHandler)
        End AddHandler
        RemoveHandler(value As EventHandler)
        End RemoveHandler
        RaiseEvent(sender As Object, e As EventArgs)
        End RaiseEvent
    End Event
    Partial Private Sub OnCreated()
    End Sub
    Private Declare Function GetTickCount Lib "kernel32" () As Integer
    Delegate Sub Notify(message As String)
    Function Run() As Integer
        Dim f = Function(x As Integer) x * 2
        Dim g = Sub()
                    Console.WriteLine()
                End Sub
        Return f(1)
    End Function
End Class

Interface IShape
    Sub Draw()
    Function Area() As Double
    Property Color As Integer
End Interface
