
            class C {
                fun walk(n: Int): Int {
                    if (n == 0) {
                        return 0
                    }
                    return this.walk(n - 1)
                }
            }
        