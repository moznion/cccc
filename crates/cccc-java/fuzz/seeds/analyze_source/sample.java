class Sample {
    static int sumOfPrimes(int max) {
        int total = 0;
        OUT:
        for (int i = 2; i <= max; ++i) {
            for (int j = 2; j < i; ++j) {
                if (i % j == 0) {
                    continue OUT;
                }
            }
            total += i;
        }
        return total;
    }
}
