int sumOfPrimes(int max) {
  var total = 0;
  outer:
  for (var i = 2; i <= max; ++i) {
    for (var j = 2; j < i; ++j) {
      if (i % j == 0) {
        continue outer;
      }
    }
    total += i;
  }
  return total;
}
