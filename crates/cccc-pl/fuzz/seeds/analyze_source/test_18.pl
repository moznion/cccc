
sub f {
    my (@xs) = @_;
    OUTER: for my $x (@xs) {
        for my $y (@xs) {
            next OUTER;
        }
        last;
        next;
    }
}
