#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};
use core::{fmt::Write, iter, mem, str};

#[must_use]
pub fn format_sexp(sexp: &str, initial_indent_level: usize) -> String {
    let sexp = sexp.trim();
    let mut formatter = SexpFormatter {
        formatted: String::with_capacity(sexp.len()),
        indent_level: initial_indent_level,
        initial_indent_level,
        has_field: false,
    };
    let mut scratch = String::with_capacity(sexp.len());
    let mut scanner = SexpScanner::new(sexp);

    while let Some(has_payload) =
        scanner.next_token(&mut scratch, '\0', formatter.open_node_depth())
    {
        if scratch.is_empty() && formatter.indent_level > initial_indent_level {
            formatter.indent_level -= 1;
            write!(formatter.formatted, ")").unwrap();
        } else if scratch.starts_with('(') {
            formatter.emit_open_node(&mut scratch, &mut scanner, has_payload);
        } else if scratch.ends_with(':') {
            formatter.write_newline_indent();
            write!(formatter.formatted, "{scratch} ").unwrap();
            formatter.has_field = true;
            formatter.indent_level += 1;
        }
    }

    formatter.formatted
}

struct SexpFormatter {
    formatted: String,
    indent_level: usize,
    initial_indent_level: usize,
    has_field: bool,
}

impl SexpFormatter {
    const fn open_node_depth(&self) -> usize {
        self.indent_level.saturating_sub(self.initial_indent_level)
    }

    fn emit_open_node(
        &mut self,
        scratch: &mut String,
        scanner: &mut SexpScanner<'_>,
        has_payload: bool,
    ) {
        if self.has_field {
            self.has_field = false;
        } else {
            if self.indent_level > 0 {
                self.write_newline_indent();
            }
            self.indent_level += 1;
        }

        write!(self.formatted, "{scratch}").unwrap();
        if !has_payload {
            return;
        }

        let payload_quote = if scratch == "(UNEXPECTED" { '\'' } else { '"' };
        scanner
            .next_token(scratch, payload_quote, self.open_node_depth())
            .unwrap();
        if scratch.is_empty() {
            while self.indent_level > self.initial_indent_level {
                self.indent_level -= 1;
                write!(self.formatted, ")").unwrap();
            }
        } else {
            write!(self.formatted, " {scratch}").unwrap();
        }
    }

    fn write_newline_indent(&mut self) {
        writeln!(self.formatted).unwrap();
        for _ in 0..self.indent_level {
            write!(self.formatted, "  ").unwrap();
        }
    }
}

struct SexpScanner<'a> {
    sexp: &'a str,
    c_iter: iter::Peekable<str::Chars<'a>>,
    quote: char,
    saw_paren: bool,
    did_last: bool,
    first_token: bool,
    quote_suffixes: [Vec<(usize, usize)>; 2],
    byte_offset: usize,
}

impl<'a> SexpScanner<'a> {
    fn new(sexp: &'a str) -> Self {
        Self {
            sexp,
            c_iter: sexp.chars().peekable(),
            quote: '\0',
            saw_paren: false,
            did_last: false,
            first_token: true,
            quote_suffixes: quote_suffix_minimum_depths(sexp),
            byte_offset: 0,
        }
    }

    fn next_token(
        &mut self,
        next: &mut String,
        payload_quote: char,
        open_nodes: usize,
    ) -> Option<bool> {
        let root_token = mem::replace(&mut self.first_token, false);
        let open_nodes = if root_token { 1 } else { open_nodes };
        next.clear();
        let separated = self.read_token_chars(next, payload_quote, open_nodes, root_token);

        if self.c_iter.peek().is_none() && next.is_empty() {
            if self.saw_paren {
                self.saw_paren = false;
                return Some(false);
            }
            if !self.did_last {
                self.did_last = true;
                return Some(false);
            }
            return None;
        }

        let has_payload = separated
            && (next == "(MISSING" || next == "(UNEXPECTED")
            && self.c_iter.peek().is_some_and(|&c| c != '(' && c != ')')
            && !sexp_field_follows(self.c_iter.clone());
        Some(has_payload)
    }

    fn read_token_chars(
        &mut self,
        next: &mut String,
        payload_quote: char,
        open_nodes: usize,
        root_token: bool,
    ) -> bool {
        while let Some(c) = self.c_iter.next() {
            self.byte_offset += c.len_utf8();
            if self.quote != '\0' {
                // `to_sexp` does not escape quotes in anonymous MISSING names or
                // quoted UNEXPECTED characters, so their node boundary disambiguates
                // the delimiter. Named MISSING payloads are emitted without delimiters.
                if c == self.quote
                    && quote_closing_depth(self.c_iter.clone())
                        .is_some_and(|depth| depth <= open_nodes)
                {
                    self.quote = '\0';
                }
            } else if self.try_open_quote(c, next, payload_quote, open_nodes, root_token) {
            } else if c == ' ' {
                return true;
            } else if c == ')' {
                self.saw_paren = true;
                return false;
            }
            next.push(c);
        }
        false
    }

    fn try_open_quote(
        &mut self,
        c: char,
        next: &str,
        payload_quote: char,
        open_nodes: usize,
        root_token: bool,
    ) -> bool {
        if next.is_empty() && c == payload_quote {
            let suffixes = &mut self.quote_suffixes[usize::from(c == '"')];
            while suffixes
                .last()
                .is_some_and(|&(offset, _)| offset < self.byte_offset)
            {
                suffixes.pop();
            }
            if suffixes
                .last()
                .is_some_and(|&(_, depth)| depth <= open_nodes)
            {
                self.quote = c;
            }
            return true;
        }

        if root_token
            && next == "("
            && c == '"'
            && self.sexp.len() > 3
            && self.sexp.ends_with("\")")
        {
            self.quote = c;
            return true;
        }

        false
    }
}

fn sexp_field_follows(mut chars: impl Iterator<Item = char>) -> bool {
    let mut last = None;
    for c in chars.by_ref() {
        match c {
            '(' | ')' | '\'' | '"' => return false,
            ' ' => break,
            _ => last = Some(c),
        }
    }
    last == Some(':') && chars.next() == Some('(')
}

fn quote_suffix_minimum_depths(sexp: &str) -> [Vec<(usize, usize)>; 2] {
    let mut suffixes: [Vec<(usize, usize)>; 2] = [Vec::new(), Vec::new()];
    for (offset, c) in sexp.char_indices().rev() {
        if c != '\'' && c != '"' {
            continue;
        }
        if let Some(depth) = quote_closing_depth(sexp[offset + 1..].chars()) {
            let quotes = &mut suffixes[usize::from(c == '"')];
            // A later, shallower candidate answers the same existence query for longer.
            if quotes.last().is_none_or(|&(_, minimum)| depth < minimum) {
                quotes.push((offset, depth));
            }
        }
    }
    suffixes
}

fn quote_closing_depth(mut chars: impl Iterator<Item = char>) -> Option<usize> {
    match chars.next() {
        None => Some(0),
        Some(')') => {
            let mut depth = 1;
            for c in chars {
                match c {
                    ')' => depth += 1,
                    c if c.is_whitespace() => return Some(depth),
                    _ => return None,
                }
            }
            Some(depth)
        }
        _ => None,
    }
}
