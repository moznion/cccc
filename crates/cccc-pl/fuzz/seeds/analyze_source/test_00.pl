
sub sum_of_primes {
    my ($max) = @_;
    my $total = 0;
    OUT: for my $i (2 .. $max) {
        for my $j (2 .. $i - 1) {
            if ($i % $j == 0) {
                next OUT;
            }
        }
        $total += $i;
    }
    return $total;
}
