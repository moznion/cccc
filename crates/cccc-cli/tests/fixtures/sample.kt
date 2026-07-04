// Fixture with known complexity values, used by integration tests.

// Cognitive: for(+1) + nested for(+2) + nested if(+3) + labelled continue(+1) = 7
// Cyclomatic: base 1 + for + for + if = 4
fun sumOfPrimes(max: Int): Int {
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

// Cognitive: when(+1) = 1 ; Cyclomatic: base 1 + 2 non-default entries = 3
fun getWords(n: Int): String {
    return when (n) {
        1 -> "one"
        2 -> "a couple"
        else -> "lots"
    }
}

// Cognitive: if(+1) + &&(+1) = 2 ; Cyclomatic: base 1 + if + && = 3
fun classify(a: Boolean, b: Boolean): String {
    if (a && b) {
        return "both"
    }
    return "not"
}
