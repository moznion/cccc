
sub f {
    my ($a, $b, $c) = @_;
    if ($a) { one(); }
    elsif ($b) {
        if ($c) { two(); }
    }
}
