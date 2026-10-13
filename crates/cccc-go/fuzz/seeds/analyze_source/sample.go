// Fixture with known complexity values, used by integration tests.

package sample

// Cognitive: for(+1) + nested for(+2) + nested if(+3) + labelled continue(+1) = 7
// Cyclomatic: base 1 + for + for + if = 4
func sumOfPrimes(max int) int {
	total := 0
OUT:
	for i := 2; i <= max; i++ {
		for j := 2; j < i; j++ {
			if i%j == 0 {
				continue OUT
			}
		}
		total += i
	}
	return total
}

// Cognitive: switch(+1) = 1 ; Cyclomatic: base 1 + 2 non-default cases = 3
func getWords(n int) string {
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
func classify(a, b bool) string {
	if a && b {
		return "both"
	}
	return "not"
}
