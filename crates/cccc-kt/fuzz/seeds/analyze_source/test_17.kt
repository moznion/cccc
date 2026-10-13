
            fun sumOfPrimes(max: Int): Int {
                val note = "reach me at a@b.com" // @not-an-annotation
                var total = 0
                outer@ for (i in 2..max) {
                    for (j in 2 until i) {
                        if (i % j == 0) {
                            continue@outer
                        }
                    }
                    total += i
                }
                return total
            }
        