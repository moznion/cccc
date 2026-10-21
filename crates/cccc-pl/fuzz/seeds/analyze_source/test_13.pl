
sub f {
    my ($a, $b, $c) = @_;
    return $a // $b // $c;
}
