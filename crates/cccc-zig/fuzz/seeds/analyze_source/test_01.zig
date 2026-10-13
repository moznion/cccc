
fn classify(value: u8) []const u8 {
    return switch (value) {
        1 => "one",
        2 => "two",
        else => "many",
    };
}
