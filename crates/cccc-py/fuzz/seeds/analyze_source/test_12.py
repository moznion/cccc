
def f():
    try:
        risky()
    except ValueError:
        pass
    except Exception as e:
        pass
    else:
        ok()
    finally:
        cleanup()
