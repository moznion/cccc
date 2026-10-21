
sub f {
    my ($a, $b, $c, $d) = @_;
    if ($a && $b && $c || $d) { go(); }
}
