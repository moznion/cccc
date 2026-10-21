
            class C {
                def walk(n: Int): Int =
                    if (n == 0) 0 else this.walk(n - 1)
            }
        