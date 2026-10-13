
sub f {
    my ($a) = @_;
    unless ($a) { bail(); }
    else { go(); }
}
