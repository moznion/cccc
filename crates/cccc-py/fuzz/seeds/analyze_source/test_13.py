
def f():
    try:
        risky()
    except* ValueError:
        pass
