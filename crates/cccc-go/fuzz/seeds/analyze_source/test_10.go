
            package p
            func free() {}
            type C struct{}
            func (c C) method() {}
            func host() {
                lambda := func(x int) int { return x + 1 }
                _ = lambda
            }
        