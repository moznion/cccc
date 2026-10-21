
fn choose(a: bool, b: bool, c: bool, fallback: ?bool) bool {
    return (a and b and c) or (fallback orelse false);
}
