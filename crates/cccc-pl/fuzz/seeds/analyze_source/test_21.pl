
package S;
sub init {
    my ($self) = @_;
    if ($self->{ready}) { return; }
    return $self->SUPER::init();
}
