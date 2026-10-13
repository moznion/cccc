# Fixture with known complexity values, used by integration tests.

# Cognitive: for(+1) + nested for(+2) + nested if(+3) + else(+1 flat) = 7
# Cyclomatic: base 1 + for + for + if = 4
# (Ruby has no labelled `next`, so the flat `else` supplies the 7th cognitive
#  point that the other languages get from a labelled `continue`.)
def sum_of_primes(max)
  total = 0
  for i in 2..max
    for j in 2...i
      if i % j == 0
        total += 0
      else
        total += i
      end
    end
  end
  total
end

# Cognitive: case(+1) = 1 ; Cyclomatic: base 1 + 2 non-default whens = 3
def get_words(n)
  case n
  when 1
    "one"
  when 2
    "a couple"
  else
    "lots"
  end
end

# Cognitive: if(+1) + &&(+1) = 2 ; Cyclomatic: base 1 + if + && = 3
def classify(a, b)
  if a && b
    return "both"
  end
  "not"
end
