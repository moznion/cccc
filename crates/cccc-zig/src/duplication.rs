//! Refuses input whose AST zigsyn would build exponentially large.
//!
//! zigsyn 0.1 stores two subtrees twice:
//!
//! * an anonymous initializer `.{ ... }` keeps its entries both as `entries`
//!   and as the cloned `fields`;
//! * a `const`/`var` declaration keeps its type annotation both as `ty` and
//!   inside the cloned `lhs` prototype.
//!
//! Each copy holds copies of whatever is nested inside, so every level of such
//! nesting doubles the tree: ~20 nested `.{` take over half a gigabyte, and a
//! 4 KiB fuzz input passed libFuzzer's 2 GB limit. The parse cannot be
//! interrupted, so [`exceeds_budget`] estimates the cost before parsing: each
//! significant byte weighs `2^k`, where `k` counts the duplicating constructs
//! enclosing it, and the file is refused once the total outgrows the source
//! by more than [`BUDGET_PER_BYTE`]. Hand-written code nests these a few levels
//! deep at most, so it stays well inside.

/// Average duplication factor allowed over the whole source.
const BUDGET_PER_BYTE: u64 = 16;

/// Flat allowance on top, so short files may nest a little deeper.
const BUDGET_SLACK: u64 = 1 << 16;

/// Whether parsing `source` would make zigsyn duplicate more than the budget.
pub(crate) fn exceeds_budget(source: &str) -> bool {
    let budget = (source.len() as u64)
        .saturating_mul(BUDGET_PER_BYTE)
        .saturating_add(BUDGET_SLACK);
    let bytes = source.as_bytes();
    // Duplication exponent of each open bracket.
    let mut brackets: Vec<u32> = Vec::new();
    // Bracket depths at which a declaration's type annotation is being read.
    let mut types: Vec<usize> = Vec::new();
    let mut decl = Decl::None;
    let mut prev = 0u8;
    let mut weight = 0u64;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let depth = brackets.len();
        let in_type = types.last() == Some(&depth);
        let enclosing = brackets.last().copied().unwrap_or(0);
        let exp = enclosing + u32::from(in_type);
        let start = i;
        i += 1;
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            b'/' if bytes.get(i) == Some(&b'/') => {
                i = line_end(bytes, i);
                continue;
            }
            b'\\' if bytes.get(i) == Some(&b'\\') => i = line_end(bytes, i),
            b'"' | b'\'' => i = quote_end(bytes, i, b),
            b'(' | b'[' | b'{' => {
                let doubles = in_type || (b == b'{' && prev == b'.');
                brackets.push(enclosing + u32::from(doubles));
            }
            b')' | b']' | b'}' => {
                brackets.pop();
                while types.last().is_some_and(|&d| d > brackets.len()) {
                    types.pop();
                }
            }
            b'=' | b';' | b',' if in_type => {
                types.pop();
            }
            b':' if decl == Decl::Named => {
                types.push(depth);
            }
            _ if is_word(b) => {
                while i < bytes.len() && is_word(bytes[i]) {
                    i += 1;
                }
                let word = &bytes[start..i];
                if in_type && matches!(word, b"align" | b"addrspace" | b"linksection") {
                    types.pop();
                }
            }
            _ => {}
        }
        decl = decl.next(b, &bytes[start..i]);
        prev = b;
        let len = (i - start) as u64;
        let unit = 1u64.checked_shl(exp).unwrap_or(u64::MAX);
        weight = weight.saturating_add(unit.saturating_mul(len));
        if weight > budget {
            return true;
        }
    }
    false
}

/// Progress through `const`/`var` NAME `:`, which opens a type annotation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decl {
    None,
    Keyword,
    Named,
}

impl Decl {
    fn next(self, first: u8, token: &[u8]) -> Self {
        match (self, first) {
            _ if token == b"const" || token == b"var" => Self::Keyword,
            // `@"name"` arrives as `@` followed by a string.
            (Self::Keyword, b'@') => Self::Keyword,
            (Self::Keyword, b'"') => Self::Named,
            (Self::Keyword, _) if is_word(first) => Self::Named,
            _ => Self::None,
        }
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |p| from + p)
}

/// End of a string or character literal opened just before `from`; Zig's
/// literals cannot span lines, so an unterminated one ends at the newline.
fn quote_end(bytes: &[u8], mut from: usize, quote: u8) -> usize {
    while from < bytes.len() {
        match bytes[from] {
            b'\\' => from += 2,
            b'\n' => return from,
            b if b == quote => return from + 1,
            _ => from += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_anon_init(depth: usize) -> String {
        format!("const x = {}1{};", ".{".repeat(depth), "}".repeat(depth))
    }

    fn nested_decl_type(depth: usize) -> String {
        format!(
            "fn f() void {{{}break :blk 1;{}}}",
            "var x: struct { a: u8 = blk: {".repeat(depth),
            "}} = .{};".repeat(depth)
        )
    }

    #[test]
    fn ordinary_code_is_accepted() {
        let source = r#"
const std = @import("std");
const Point = struct { x: i32, y: i32 };
const table = [_]Point{ .{ .x = 1, .y = 2 }, .{ .x = 3, .y = 4 } };
pub fn main() !void {
    var list: std.ArrayList(struct { a: [2]u8 }) = .{};
    const opts: struct { nested: struct { deep: u8 } } = .{ .nested = .{ .deep = 1 } };
    std.debug.print("{any} {any}\n", .{ .{ list, opts }, .{ table, "}}}}" } });
}
"#;
        assert!(!exceeds_budget(source));
    }

    #[test]
    fn moderately_nested_initializers_are_accepted() {
        assert!(!exceeds_budget(&nested_anon_init(10)));
        assert!(!exceeds_budget(&nested_decl_type(8)));
    }

    #[test]
    fn deeply_nested_initializers_are_refused() {
        assert!(exceeds_budget(&nested_anon_init(20)));
    }

    #[test]
    fn deeply_nested_declaration_types_are_refused() {
        assert!(exceeds_budget(&nested_decl_type(20)));
    }

    #[test]
    fn brackets_in_comments_and_strings_do_not_count() {
        let source = format!(
            "// {0}\nconst s = \"{0}\";\nconst m =\n    \\\\{0}\n;\n",
            ".{".repeat(40)
        );
        assert!(!exceeds_budget(&source));
    }
}
