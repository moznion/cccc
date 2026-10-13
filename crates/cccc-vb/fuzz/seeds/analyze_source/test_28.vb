
    Dim q = From c In customers, o As Order In orders
            Join p In products On o.Pid Equals p.Id
            Where c.Ok AndAlso o.Ok
            Group By c.City Into Count()
            Distinct
    Dim n = Aggregate x In xs Into Sum(x)
    If a Then Foo()
