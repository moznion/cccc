
sub f {
    my ($a, $b) = @_;
    if ($a) { one(); }
    elsif ($b) { two(); }
    else { three(); }
}
