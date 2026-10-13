
sub f {
    my ($a, $b, $c) = @_;
    if ($a) {
        if ($b) {
            if ($c) { deep(); }
        }
    }
}
