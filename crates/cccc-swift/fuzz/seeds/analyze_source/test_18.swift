
            class C {
                func walk(_ n: Int) -> Int {
                    if n == 0 {
                        return 0
                    }
                    return self.walk(n - 1)
                }
            }
        