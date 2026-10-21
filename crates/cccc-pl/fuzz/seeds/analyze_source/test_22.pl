
sub walk {
    my ($n) = @_;
    if ($n == 0) { return 0; }
    return Tree::Walker::walk($n - 1);
}
