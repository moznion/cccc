
sub f {
    my ($a) = @_;
    $a //= 1;
    $a ||= 2;
    $a &&= 3;
    return $a;
}
