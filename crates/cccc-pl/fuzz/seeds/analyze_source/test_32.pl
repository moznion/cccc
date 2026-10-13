
=pod

Documentation about f.

=cut

sub f {
    my ($a) = @_;
    # outer check
    if ($a) { # inner
        if ($a) { go(); }
    }
}
