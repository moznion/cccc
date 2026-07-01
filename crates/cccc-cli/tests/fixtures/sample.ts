// Fixture with known complexity values, used by integration tests.

// Cognitive: for(+1) + nested for(+2) + nested if(+3) + continue OUT(+1) = 7
// Cyclomatic: base 1 + for + for + if = 4
function sumOfPrimes(max: number): number {
  let total = 0;
  OUT: for (let i = 2; i <= max; ++i) {
    for (let j = 2; j < i; ++j) {
      if (i % j === 0) {
        continue OUT;
      }
    }
    total += i;
  }
  return total;
}

// Cognitive: switch(+1) = 1 ; Cyclomatic: base 1 + 2 cases = 3
function getWords(n: number): string {
  switch (n) {
    case 1:
      return "one";
    case 2:
      return "a couple";
    default:
      return "lots";
  }
}

// arrow with a logical sequence: Cognitive ternary(+1) + &&(+1) = 2
const classify = (a: boolean, b: boolean) => (a && b ? "both" : "not");
