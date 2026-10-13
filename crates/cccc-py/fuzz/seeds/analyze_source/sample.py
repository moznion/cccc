# Fixture with known complexity values, used by integration tests.

# Cognitive: for(+1) + nested for(+2) + nested if(+3) + else(+1 flat) = 7
# Cyclomatic: base 1 + for + for + if = 4
# (Python has no labelled `continue`, so the flat `else` supplies the 7th
#  cognitive point that the other languages get from a labelled `continue`.)
def sum_of_primes(max):
    total = 0
    for i in range(2, max + 1):
        for j in range(2, i):
            if i % j == 0:
                total += 0
            else:
                total += i
    return total


# Cognitive: match(+1) = 1 ; Cyclomatic: base 1 + 2 non-default cases = 3
def get_words(n):
    match n:
        case 1:
            return "one"
        case 2:
            return "a couple"
        case _:
            return "lots"


# Cognitive: if(+1) + and(+1) = 2 ; Cyclomatic: base 1 + if + and = 3
def classify(a, b):
    if a and b:
        return "both"
    return "not"
