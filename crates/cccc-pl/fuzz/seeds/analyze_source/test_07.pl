
sub f {
    my ($a, $b, $c, @xs) = @_;
    go() if $a;
    stop() unless $b;
    spin() while $c;
    each_one($_) for @xs;
}
