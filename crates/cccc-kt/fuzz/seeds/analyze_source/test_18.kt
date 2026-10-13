
            @file: Suppress("A", "B")

            package foo.bar

            fun f(x: Boolean): Int {
                if (x) return 1
                return 0
            }
        