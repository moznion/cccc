
    Dim doc = <root kind="a">
                  <item><%= If(a, 1, 2) %></item>
              </root>
    Dim name = doc.<item>.@kind
    If a Then Foo()
