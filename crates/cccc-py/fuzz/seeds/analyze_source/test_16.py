
class S:
    @property
    def value(self):
        if self.raw:
            return self.raw
        return None
