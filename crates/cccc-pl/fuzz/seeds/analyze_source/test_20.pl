
package S;
sub walk {
    my ($self, $n) = @_;
    if ($n == 0) { return 0; }
    return $self->walk($n - 1);
}
