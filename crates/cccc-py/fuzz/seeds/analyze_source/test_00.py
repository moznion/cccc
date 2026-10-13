
def sum_of_primes(max):
    total = 0
    for i in range(2, max + 1):
        for j in range(2, i):
            if i % j == 0:
                total += 0
            else:
                total += i
    return total
