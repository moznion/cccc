
const Service = struct {
    fn run(ok: bool) void {
        if (ok) work();
    }
};

test "service" {
    if (enabled()) work();
}
