
sub host {
    my (@xs) = @_;
    my @pos = grep $_ > 0 && $_ < 10, @xs;
    return @pos;
}
