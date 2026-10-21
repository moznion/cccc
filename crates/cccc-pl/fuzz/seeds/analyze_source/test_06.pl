
sub f {
    my ($a, @items) = @_;
    while ($a) { w(); }
    until ($a) { u(); }
    for (my $i = 0; $i < 10; $i++) { c(); }
    foreach my $x (@items) { e($x); }
}
