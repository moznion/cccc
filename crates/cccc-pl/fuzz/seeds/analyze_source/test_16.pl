
use feature 'try';
sub f {
    try { risky(); }
    catch ($e) { handle($e); }
    finally { cleanup(); }
}
