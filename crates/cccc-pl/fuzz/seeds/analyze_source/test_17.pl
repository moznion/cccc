
sub f {
    eval { risky(); };
    if ($@) { handle(); }
}
