
sub f {
    my ($a, @xs) = @_;
    do_all(map { $_ ? 1 : 0 } @xs) if $a;
}
