
sub host {
    my ($a) = @_;
    my $cb = sub { return $a && $a; };
    return $cb;
}
