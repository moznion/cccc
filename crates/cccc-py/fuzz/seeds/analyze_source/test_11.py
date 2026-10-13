
def f(x, p):
    match x:
        case _ if p:
            return 1
        case _:
            return 2
