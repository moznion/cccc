
sub host {
    my %handlers = (
        on_error => sub { retry() if $a; },
    );
    return \%handlers;
}
