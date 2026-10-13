
    Dim doc = <?xml version="1.0"?>
              <!-- c -->
              <?pi x?>
              <root><![CDATA[ If x Then ]]><!-- If --><a/></root>
    Dim d2 = <?xml version="1.0"?><%= el %>
    Dim c = <!-- only a comment -->
    Dim cd = <![CDATA[ If ]]>
    Dim e = <<%= name %>>v</>
    If a Then Foo()
