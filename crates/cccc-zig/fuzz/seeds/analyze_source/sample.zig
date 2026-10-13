pub fn sumOfPrimes(max: u32) u32 {
    var total: u32 = 0;
    outer: for (2..max) |i| {
        for (2..i) |j| {
            if (i % j == 0) {
                continue :outer;
            }
        }
        total += i;
    }
    return total;
}
