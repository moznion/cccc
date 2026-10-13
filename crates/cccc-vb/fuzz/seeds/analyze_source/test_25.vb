
    RaiseEvent Changed(If(a, 1, 2))
    AddHandler btn.Click, Sub(s, e) Foo()
    RemoveHandler btn.Click, AddressOf Handler
