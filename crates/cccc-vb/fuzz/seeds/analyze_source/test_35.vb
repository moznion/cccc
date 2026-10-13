
    Foo(a,
#If DEBUG Then
        If(b, 1, 2)
#Else
        0
#End If
        )
