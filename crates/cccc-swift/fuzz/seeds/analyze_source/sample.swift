// Fixture with known complexity values, used by integration tests.

// Cognitive: for(+1) + nested for(+2) + nested if(+3) + labelled continue(+1) = 7
// Cyclomatic: base 1 + for + for + if = 4
func sumOfPrimes(max: Int) -> Int {
    var total = 0
    outer: for i in 2...max {
        for j in 2..<i {
            if i % j == 0 {
                continue outer
            }
        }
        total += i
    }
    return total
}

// Cognitive: switch(+1) = 1 ; Cyclomatic: base 1 + 2 non-default entries = 3
func getWords(n: Int) -> String {
    switch n {
    case 1:
        return "one"
    case 2:
        return "a couple"
    default:
        return "lots"
    }
}

// Cognitive: if(+1) + &&(+1) = 2 ; Cyclomatic: base 1 + if + && = 3
func classify(a: Bool, b: Bool) -> String {
    if a && b {
        return "both"
    }
    return "not"
}
