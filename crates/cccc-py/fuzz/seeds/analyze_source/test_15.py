
class S:
    def walk(self, n):
        if n == 0:
            return 0
        return self.walk(n - 1)
