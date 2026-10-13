unsigned int sum_of_primes(unsigned int max) {
    unsigned int total = 0;
    for (unsigned int i = 2; i <= max; i++) {
        for (unsigned int j = 2; j < i; j++) {
            if (i % j == 0) {
                goto next;
            }
        }
        total += i;
next:;
    }
    return total;
}
