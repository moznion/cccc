
            object S {
                def sumOfPrimes(max: Int): Int = {
                    var total = 0
                    for (i <- 2 to max) {
                        for (j <- 2 until i) {
                            if (i % j == 0) {
                                total += 0
                            } else {
                                total += i
                            }
                        }
                    }
                    total
                }
            }
        