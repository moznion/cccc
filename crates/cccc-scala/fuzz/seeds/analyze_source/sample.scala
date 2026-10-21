// Fixture with known complexity values, used by integration tests.

// Cognitive: for(+1) + nested for(+2) + nested if(+3) + else(+1 flat) = 7
// Cyclomatic: base 1 + for + for + if = 4
// (Scala has no labelled `continue`, so the flat `else` supplies the 7th
//  cognitive point that the labelled-jump languages get from `continue`.)
object Sample {
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

  // Cognitive: match(+1) = 1 ; Cyclomatic: base 1 + 2 non-default cases = 3
  def getWords(n: Int): String = n match {
    case 1 => "one"
    case 2 => "a couple"
    case _ => "lots"
  }

  // Cognitive: if(+1) + &&(+1) = 2 ; Cyclomatic: base 1 + if + && = 3
  def classify(a: Boolean, b: Boolean): String = {
    if (a && b) {
      return "both"
    }
    "not"
  }
}
