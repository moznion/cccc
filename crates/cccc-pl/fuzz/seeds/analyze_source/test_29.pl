
sub host {
    my (@xs) = @_;
    return sort { $a <=> $b ? -1 : 1 } @xs;
}
