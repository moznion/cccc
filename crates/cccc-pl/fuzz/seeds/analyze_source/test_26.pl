
sub host {
    sub inner {
        my ($x) = @_;
        if ($x) { return 1; }
    }
    return 1;
}
