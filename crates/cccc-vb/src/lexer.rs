//! Tokenizer for Visual Basic .NET.
//!
//! The lexer is deliberately *line-aware but not line-deciding*: every token
//! records whether a physical line break (one not cancelled by an explicit
//! ` _` continuation) precedes it, and the parser decides from context whether
//! that break ends the statement (VB's implicit line continuation depends on
//! syntax — open parentheses, a trailing operator, a query clause — which only
//! the parser knows).
//!
//! Things resolved here so the parser never sees them as text:
//! - comments (`'`, the typographic `‘`/`’`, and `REM`) and ` _` continuations;
//! - string / char / number / date literals (strings may span lines and use
//!   the typographic `“`/`”` quotes);
//! - interpolated strings, whose `{…}` holes are lexed into nested token lists
//!   (they may hold scored code such as `If(…)` or a lambda);
//! - XML literals (opaque);
//! - preprocessor lines (`#If`, `#ElseIf`, `#Else`, `#End If`, and the
//!   directives that carry no logic), as one token each;
//! - escaped identifiers (`[Next]`) and member names after `.` / `?.` / `!`,
//!   which are never keywords.

/// One lexical token. `start..end` is its byte range in the source.
pub(crate) struct Token {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    /// 1-based source line of `start`.
    pub line: u32,
    /// A physical line break (not cancelled by ` _`) precedes this token.
    pub nl_before: bool,
    /// The lowercase keyword an unescaped identifier spells (one of
    /// [`KEYWORDS`]), classified once here so the parser compares interned
    /// strings instead of re-folding case.
    pub kw: Option<&'static str>,
}

pub(crate) enum Kind {
    /// An identifier or keyword. `escaped` identifiers (`[Next]`, or a member
    /// name right after `.` / `?.` / `!`) are never treated as keywords.
    Ident {
        escaped: bool,
    },
    Punct,
    /// String, character, number, or date literal.
    Literal,
    /// An XML literal, skipped as a whole.
    Xml,
    /// An interpolated string: one token list per `{…}` hole expression.
    Interp(Vec<Vec<Token>>),
    /// A preprocessor line.
    Directive(Directive),
    Eof,
}

pub(crate) struct Directive {
    pub kind: DirKind,
    /// The condition of `#If` / `#ElseIf` (without the trailing `Then`).
    pub cond: Vec<Token>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirKind {
    If,
    ElseIf,
    Else,
    EndIf,
    /// `#Region`, `#Const`, `#ExternalSource`, `#Disable Warning`, …
    Other,
}

/// The terminating token handed out when a token list is exhausted.
pub(crate) static EOF: Token = Token {
    kind: Kind::Eof,
    start: 0,
    end: 0,
    line: 0,
    nl_before: true,
    kw: None,
};

/// Every word the parser treats as a keyword (sorted, lowercase).
pub(crate) const KEYWORDS: &[&str] = &[
    "addhandler",
    "addressof",
    "aggregate",
    "and",
    "andalso",
    "as",
    "ascending",
    "async",
    "await",
    "by",
    "byref",
    "byval",
    "call",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "custom",
    "declare",
    "default",
    "delegate",
    "descending",
    "dim",
    "distinct",
    "do",
    "each",
    "else",
    "elseif",
    "end",
    "endif",
    "enum",
    "equals",
    "erase",
    "error",
    "event",
    "exit",
    "finally",
    "for",
    "friend",
    "from",
    "function",
    "get",
    "goto",
    "group",
    "handles",
    "if",
    "implements",
    "imports",
    "in",
    "inherits",
    "interface",
    "into",
    "is",
    "isnot",
    "iterator",
    "join",
    "key",
    "let",
    "like",
    "loop",
    "mod",
    "module",
    "mustinherit",
    "mustoverride",
    "mybase",
    "myclass",
    "namespace",
    "narrowing",
    "new",
    "next",
    "not",
    "notinheritable",
    "notoverridable",
    "of",
    "on",
    "operator",
    "option",
    "optional",
    "or",
    "order",
    "orelse",
    "overloads",
    "overridable",
    "overrides",
    "paramarray",
    "partial",
    "preserve",
    "private",
    "property",
    "protected",
    "public",
    "raiseevent",
    "readonly",
    "redim",
    "removehandler",
    "resume",
    "return",
    "select",
    "set",
    "shadows",
    "shared",
    "skip",
    "static",
    "step",
    "stop",
    "structure",
    "sub",
    "synclock",
    "take",
    "then",
    "throw",
    "to",
    "try",
    "typeof",
    "until",
    "using",
    "wend",
    "when",
    "where",
    "while",
    "widening",
    "with",
    "withevents",
    "writeonly",
    "xor",
    "yield",
];

/// The keyword `text` spells, case-insensitively.
fn keyword(text: &str) -> Option<&'static str> {
    const MAX: usize = 16;
    if text.len() > MAX {
        return None;
    }
    let mut buf = [0u8; MAX];
    let lower = &mut buf[..text.len()];
    lower.copy_from_slice(text.as_bytes());
    lower.make_ascii_lowercase();
    let lower = std::str::from_utf8(lower).ok()?;
    KEYWORDS.binary_search(&lower).ok().map(|i| KEYWORDS[i])
}

/// Multi-character operators, longest first.
const OPERATORS: &[&str] = &[
    "<<=", ">>=", "?.", ":=", "<>", "<=", ">=", "<<", ">>", "+=", "-=", "*=", "/=", "\\=", "^=",
    "&=",
];

/// Tokens after which a `<` starts an expression (and may thus open an XML
/// literal) rather than an attribute or a comparison.
const XML_PREV: &[&str] = &[
    "=", "(", ",", "return", "&", ":=", "in", "select", "yield", "then", "else", "{", "+", "from",
    "let",
];

/// Tokenize `src`, ending with an [`Kind::Eof`] token.
pub(crate) fn tokenize(src: &str) -> Vec<Token> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let last_close_tag = src.rfind("</");
    let mut lexer = Lexer::new(src, &line_starts, last_close_tag, 0, src.len());
    lexer.run();
    let line = line_starts.len() as u32;
    lexer.toks.push(Token {
        kind: Kind::Eof,
        start: src.len(),
        end: src.len(),
        line,
        nl_before: true,
        kw: None,
    });
    lexer.toks
}

struct Lexer<'a> {
    src: &'a str,
    b: &'a [u8],
    line_starts: &'a [usize],
    /// Index into `line_starts` of the line holding the last token (tokens
    /// are pushed in source order, so line lookup only moves forward).
    line_idx: usize,
    /// Offset of the last `</` in the source: a `<` after it cannot open a
    /// non-self-closing XML element.
    last_close_tag: Option<usize>,
    pos: usize,
    end: usize,
    toks: Vec<Token>,
    /// A line break precedes the next token.
    nl: bool,
    /// No token yet on the current physical line.
    bol: bool,
}

impl<'a> Lexer<'a> {
    fn new(
        src: &'a str,
        line_starts: &'a [usize],
        last_close_tag: Option<usize>,
        start: usize,
        end: usize,
    ) -> Self {
        Self {
            src,
            b: src.as_bytes(),
            line_starts,
            line_idx: 0,
            last_close_tag,
            pos: start,
            end,
            toks: Vec::new(),
            nl: true,
            bol: true,
        }
    }

    /// Lex `start..end` of the same source into a standalone token list (for
    /// interpolation holes and directive conditions).
    fn sub(&self, start: usize, end: usize) -> Vec<Token> {
        let mut lexer = Lexer::new(self.src, self.line_starts, self.last_close_tag, start, end);
        lexer.nl = false;
        lexer.bol = false;
        lexer.run();
        lexer.toks
    }

    fn line_of(&mut self, offset: usize) -> u32 {
        while self.line_idx + 1 < self.line_starts.len()
            && self.line_starts[self.line_idx + 1] <= offset
        {
            self.line_idx += 1;
        }
        self.line_idx as u32 + 1
    }

    fn byte(&self, i: usize) -> u8 {
        if i < self.end { self.b[i] } else { 0 }
    }

    fn push(&mut self, kind: Kind, start: usize, end: usize) {
        let kind = match kind {
            Kind::Ident { escaped: false } if self.after_member_access() => {
                Kind::Ident { escaped: true }
            }
            k => k,
        };
        let kw = match kind {
            Kind::Ident { escaped: false } => keyword(&self.src[start..end]),
            _ => None,
        };
        let line = self.line_of(start);
        self.toks.push(Token {
            kind,
            start,
            end,
            line,
            nl_before: self.nl,
            kw,
        });
        self.nl = false;
        self.bol = false;
    }

    /// The previous token is `.`, `?.`, or `!`, so the next identifier is a
    /// member name (`Kind.Like`, `x.End`), never a keyword.
    fn after_member_access(&self) -> bool {
        self.toks.last().is_some_and(|t| {
            matches!(t.kind, Kind::Punct) && matches!(&self.src[t.start..t.end], "." | "?." | "!")
        })
    }

    fn run(&mut self) {
        while self.pos < self.end {
            let c = self.b[self.pos];
            match c {
                b'\n' => {
                    self.pos += 1;
                    self.nl = true;
                    self.bol = true;
                }
                b' ' | b'\t' | b'\r' | 0x0c | 0x0b => self.pos += 1,
                b'_' if self.is_line_continuation() => {}
                b'\'' => self.skip_comment(),
                b'#' if self.bol && self.directive_follows() => self.lex_directive(),
                b'#' => self.lex_hash(),
                b'$' if self.quote_len(self.pos + 1) > 0 => self.lex_interpolated(),
                b'"' => self.lex_string(),
                b'[' => self.lex_bracketed(),
                b'0'..=b'9' => self.lex_number(),
                b'.' if self.byte(self.pos + 1).is_ascii_digit() => self.lex_number(),
                b'&' if matches!(
                    self.byte(self.pos + 1),
                    b'h' | b'H' | b'o' | b'O' | b'b' | b'B'
                ) && (self.byte(self.pos + 2).is_ascii_hexdigit()
                    || self.byte(self.pos + 2) == b'_') =>
                {
                    self.lex_number()
                }
                b'<' if self.xml_may_start() => {
                    if let Some(end) = self.try_xml(self.pos) {
                        let start = self.pos;
                        self.pos = end;
                        self.push(Kind::Xml, start, end);
                    } else {
                        self.lex_punct();
                    }
                }
                _ if c >= 0x80 => self.lex_non_ascii(),
                _ if c.is_ascii_alphabetic() || c == b'_' => self.lex_ident(),
                _ => self.lex_punct(),
            }
        }
    }

    // ---- character classes ---------------------------------------------------

    /// The character starting at byte `i` (`None` past the end or inside a
    /// multi-byte character).
    fn char_at(&self, i: usize) -> Option<char> {
        self.src.get(i..self.end).and_then(|s| s.chars().next())
    }

    /// The offset of the first `pat` at or after `i`.
    fn find(&self, i: usize, pat: &[u8]) -> Option<usize> {
        self.b[i.min(self.end)..self.end]
            .windows(pat.len())
            .position(|w| w == pat)
            .map(|j| i + j)
    }

    fn starts_with(&self, i: usize, pat: &[u8]) -> bool {
        self.b[i.min(self.end)..self.end].starts_with(pat)
    }

    /// Length of a `"` / `“` / `”` quote at `i`, or 0.
    fn quote_len(&self, i: usize) -> usize {
        match self.char_at(i) {
            Some('"') => 1,
            Some('\u{201c}' | '\u{201d}') => 3,
            _ => 0,
        }
    }

    /// Length of an identifier character at `i`, or 0.
    fn ident_char_len(&self, i: usize) -> usize {
        match self.char_at(i) {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' => 1,
            Some(c) if !c.is_ascii() && (c.is_alphanumeric() || is_combining(c)) => c.len_utf8(),
            _ => 0,
        }
    }

    // ---- trivia --------------------------------------------------------------

    /// ` _` followed by optional blanks / a comment and a line break: join the
    /// next physical line onto this one.
    fn is_line_continuation(&mut self) -> bool {
        let prev = if self.pos == 0 {
            b' '
        } else {
            self.b[self.pos - 1]
        };
        if !matches!(prev, b' ' | b'\t' | b'\n') || self.ident_char_len(self.pos + 1) > 0 {
            return false;
        }
        let mut j = self.pos + 1;
        while matches!(self.byte(j), b' ' | b'\t' | b'\r') {
            j += 1;
        }
        if self.byte(j) == b'\'' || matches!(self.char_at(j), Some('\u{2018}' | '\u{2019}')) {
            while j < self.end && self.b[j] != b'\n' {
                j += 1;
            }
        }
        if j < self.end && self.b[j] != b'\n' {
            return false;
        }
        self.pos = (j + 1).min(self.end);
        self.bol = false;
        true
    }

    fn skip_comment(&mut self) {
        while self.pos < self.end && self.b[self.pos] != b'\n' {
            self.pos += 1;
        }
    }

    fn lex_non_ascii(&mut self) {
        let c = self.char_at(self.pos).unwrap_or(' ');
        match c {
            '\u{feff}' | '\u{a0}' | '\u{3000}' | '\u{2028}' | '\u{2029}' => {
                self.pos += c.len_utf8()
            }
            '\u{2018}' | '\u{2019}' => self.skip_comment(),
            '\u{201c}' | '\u{201d}' => self.lex_string(),
            _ if c.is_whitespace() => self.pos += c.len_utf8(),
            _ if c.is_alphabetic() => self.lex_ident(),
            _ => {
                let start = self.pos;
                self.pos += c.len_utf8();
                self.push(Kind::Punct, start, self.pos);
            }
        }
    }

    // ---- literals ------------------------------------------------------------

    /// The offset just past the string literal whose opening quote is at `i`
    /// (a doubled quote is an escaped `"`).
    fn string_end(&self, mut i: usize) -> usize {
        i += self.quote_len(i);
        while i < self.end {
            let q = self.quote_len(i);
            if q == 0 {
                i += 1;
                continue;
            }
            i += q;
            let q2 = self.quote_len(i);
            if q2 == 0 {
                break;
            }
            i += q2;
        }
        i.min(self.end)
    }

    fn lex_string(&mut self) {
        let start = self.pos;
        self.pos = self.string_end(self.pos);
        // Character literal suffix: "a"c
        if matches!(self.byte(self.pos), b'c' | b'C') && self.ident_char_len(self.pos + 1) == 0 {
            self.pos += 1;
        }
        self.push(Kind::Literal, start, self.pos);
    }

    /// `$"…{expr,align:format}…"`: each hole's expression part is lexed into
    /// its own token list.
    fn lex_interpolated(&mut self) {
        let start = self.pos;
        self.pos += 1 + self.quote_len(self.pos + 1);
        let mut holes = Vec::new();
        while self.pos < self.end {
            let q = self.quote_len(self.pos);
            if q > 0 {
                self.pos += q;
                let q2 = self.quote_len(self.pos);
                if q2 > 0 {
                    self.pos += q2;
                    continue;
                }
                break;
            }
            match self.b[self.pos] {
                b'{' if self.byte(self.pos + 1) == b'{' => self.pos += 2,
                b'{' => {
                    let expr_start = self.pos + 1;
                    let (expr_end, close) = self.scan_hole(expr_start);
                    holes.push(self.sub(expr_start, expr_end));
                    self.pos = close;
                }
                _ => self.pos += 1,
            }
        }
        self.push(Kind::Interp(holes), start, self.pos);
    }

    /// Scan an interpolation hole starting at `i`: returns the end of its
    /// expression part (before a top-level `,` alignment or `:` format) and the
    /// offset just past the closing `}`.
    fn scan_hole(&self, mut i: usize) -> (usize, usize) {
        let mut depth = 0usize;
        let mut expr_end = None;
        while i < self.end {
            if self.quote_len(i) > 0 {
                // A nested (possibly interpolated) string literal.
                i = self.string_end(i);
                continue;
            }
            match self.b[i] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' => depth = depth.saturating_sub(1),
                b'}' if depth == 0 => return (expr_end.unwrap_or(i), i + 1),
                b'}' => depth -= 1,
                b',' | b':' if depth == 0 && expr_end.is_none() && self.byte(i + 1) != b'=' => {
                    expr_end = Some(i);
                }
                _ => {}
            }
            i += 1;
        }
        (expr_end.unwrap_or(self.end), self.end)
    }

    fn lex_number(&mut self) {
        let start = self.pos;
        if self.b[self.pos] == b'&' {
            self.pos += 2;
            while self.byte(self.pos).is_ascii_hexdigit() || self.byte(self.pos) == b'_' {
                self.pos += 1;
            }
        } else {
            loop {
                let c = self.byte(self.pos);
                if c.is_ascii_digit()
                    || c == b'_'
                    || (c == b'.' && self.byte(self.pos + 1).is_ascii_digit())
                {
                    self.pos += 1;
                } else if matches!(c, b'e' | b'E')
                    && (self.byte(self.pos + 1).is_ascii_digit()
                        || (matches!(self.byte(self.pos + 1), b'+' | b'-')
                            && self.byte(self.pos + 2).is_ascii_digit()))
                {
                    self.pos += 2;
                } else {
                    break;
                }
            }
        }
        // Type suffixes: letters (D, F, R, US, UL, …) and type characters.
        while self.byte(self.pos).is_ascii_alphabetic() {
            self.pos += 1;
        }
        if matches!(self.byte(self.pos), b'!' | b'#' | b'@' | b'%' | b'&')
            && self.ident_char_len(self.pos + 1) == 0
        {
            self.pos += 1;
        }
        self.push(Kind::Literal, start, self.pos);
    }

    /// `#` outside a directive: a date literal `#1/2/2000 10:00 AM#`, else a
    /// lone `#`.
    fn lex_hash(&mut self) {
        let start = self.pos;
        let mut j = self.pos + 1;
        while j < self.end
            && (self.b[j].is_ascii_alphanumeric()
                || matches!(self.b[j], b' ' | b'/' | b':' | b'-' | b'.' | b'\t'))
        {
            j += 1;
        }
        if j < self.end && self.b[j] == b'#' && j > self.pos + 1 {
            self.pos = j + 1;
            self.push(Kind::Literal, start, self.pos);
        } else {
            self.pos += 1;
            self.push(Kind::Punct, start, self.pos);
        }
    }

    // ---- identifiers -----------------------------------------------------------

    fn lex_bracketed(&mut self) {
        let start = self.pos;
        let mut j = self.pos + 1;
        while j < self.end && !matches!(self.b[j], b']' | b'\n') {
            j += 1;
        }
        if j < self.end && self.b[j] == b']' {
            self.pos = j + 1;
            self.push(Kind::Ident { escaped: true }, start, self.pos);
        } else {
            self.lex_punct();
        }
    }

    fn lex_ident(&mut self) {
        let start = self.pos;
        loop {
            let n = self.ident_char_len(self.pos);
            if n == 0 {
                break;
            }
            self.pos += n;
        }
        if self.pos == start {
            // A non-ASCII letter that `ident_char_len` rejects; keep moving.
            self.pos += self.char_at(start).map_or(1, char::len_utf8);
        }
        if self.src[start..self.pos].eq_ignore_ascii_case("rem")
            && matches!(self.byte(self.pos), b' ' | b'\t' | b'\r' | b'\n' | 0)
            && !self.after_member_access()
        {
            self.skip_comment();
            return;
        }
        // Type characters: x%, x&, x@, x!, x#, x$.
        let c = self.byte(self.pos);
        let next = self.byte(self.pos + 1);
        let next_ident = self.ident_char_len(self.pos + 1) > 0;
        let type_char = match c {
            b'%' | b'@' | b'#' | b'$' => !next_ident,
            b'!' => !next_ident && next != b'!',
            b'&' => matches!(next, b' ' | b'\t' | b'\r' | b'\n' | b')' | b',' | 0),
            _ => false,
        };
        if type_char {
            self.pos += 1;
        }
        self.push(Kind::Ident { escaped: false }, start, self.pos);
    }

    fn lex_punct(&mut self) {
        let start = self.pos;
        let rest = &self.b[self.pos..self.end];
        let len = OPERATORS
            .iter()
            .find(|op| rest.starts_with(op.as_bytes()))
            .map_or(1, |op| op.len());
        self.pos += len;
        self.push(Kind::Punct, start, self.pos);
    }

    // ---- preprocessor ----------------------------------------------------------

    fn directive_follows(&self) -> bool {
        let mut j = self.pos + 1;
        while matches!(self.byte(j), b' ' | b'\t') {
            j += 1;
        }
        self.byte(j).is_ascii_alphabetic()
    }

    fn lex_directive(&mut self) {
        let start = self.pos;
        let mut line_end = self.pos;
        while line_end < self.end && self.b[line_end] != b'\n' {
            line_end += 1;
        }
        let mut words = self.src[self.pos + 1..line_end]
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty());
        let first = words.next().map(keyword).unwrap_or_default();
        let second = words.next().map(keyword).unwrap_or_default();
        let kind = match (first, second) {
            (Some("if"), _) => DirKind::If,
            (Some("elseif"), _) | (Some("else"), Some("if")) => DirKind::ElseIf,
            (Some("else"), _) => DirKind::Else,
            (Some("end"), Some("if")) | (Some("endif"), _) => DirKind::EndIf,
            _ => DirKind::Other,
        };
        let cond = if matches!(kind, DirKind::If | DirKind::ElseIf) {
            // The condition follows the first `if` (in `#If`, `#ElseIf`, and
            // `#Else If` alike).
            let text = &self.src[start..line_end];
            let kw_at = text
                .as_bytes()
                .windows(2)
                .position(|w| w.eq_ignore_ascii_case(b"if"))
                .map_or(text.len(), |i| i + 2);
            let mut cond = self.sub(start + kw_at, line_end);
            if cond.last().is_some_and(|t| t.kw == Some("then")) {
                cond.pop();
            }
            cond
        } else {
            Vec::new()
        };
        self.pos = line_end;
        self.push(Kind::Directive(Directive { kind, cond }), start, line_end);
        self.nl = true;
    }

    // ---- XML literals ----------------------------------------------------------

    /// A `<` may open an XML literal where an expression starts and the next
    /// character can begin markup.
    fn xml_may_start(&self) -> bool {
        let next = self.byte(self.pos + 1);
        let computed_name = self.starts_with(self.pos + 1, b"<%=");
        if !(next.is_ascii_alphabetic() || matches!(next, b'?' | b'!' | b'_') || computed_name) {
            return false;
        }
        match self.toks.last() {
            None => true,
            Some(t) => match t.kind {
                Kind::Punct => XML_PREV.contains(&&self.src[t.start..t.end]),
                Kind::Ident { .. } => t.kw.is_some_and(|kw| XML_PREV.contains(&kw)),
                _ => false,
            },
        }
    }

    /// The end offset of an XML literal starting at `i`, or `None` if the
    /// markup is not one (e.g. a parameter attribute `<Out>` after `(`).
    fn try_xml(&self, mut i: usize) -> Option<usize> {
        if self.starts_with(i, b"<?xml") {
            // An XML document: the declaration, then processing instructions
            // / comments, then the root element (possibly embedded).
            i = self.find(i, b"?>")? + 2;
            loop {
                while i < self.end && matches!(self.b[i], b' ' | b'\t' | b'\r' | b'\n') {
                    i += 1;
                }
                if self.starts_with(i, b"<?") {
                    i = self.find(i, b"?>")? + 2;
                } else if self.starts_with(i, b"<!--") {
                    i = self.find(i, b"-->")? + 3;
                } else if self.starts_with(i, b"<%=") {
                    return Some(self.skip_embedded(i));
                } else if self.byte(i) != b'<' {
                    return Some(i);
                } else {
                    break;
                }
            }
        } else if self.starts_with(i, b"<!--") {
            return self.find(i, b"-->").map(|j| j + 3);
        } else if self.starts_with(i, b"<![CDATA[") {
            return self.find(i, b"]]>").map(|j| j + 3);
        }
        if self.starts_with(i + 1, b"<%=") {
            // `<<%= name %>>…</>`: an element with a computed name.
            return self.scan_xml_elements(i);
        }
        let name_end = (i + 1..self.end)
            .find(|&j| {
                !(self.b[j].is_ascii_alphanumeric()
                    || matches!(self.b[j], b'_' | b'.' | b':' | b'-'))
            })
            .unwrap_or(self.end);
        if name_end == i + 1 {
            return None;
        }
        let gt = self.tag_end(i);
        if gt >= self.end {
            return None;
        }
        if self.b[gt - 1] != b'/' {
            // Not self-closing: a matching close tag must follow, otherwise
            // this is an attribute such as `<Out>`.
            if self.last_close_tag.is_none_or(|last| last < gt) {
                return None;
            }
            let rest = &self.src[gt..self.end];
            let name = &self.src[i + 1..name_end];
            let closes = rest
                .match_indices("</")
                .any(|(j, _)| rest[j + 2..].starts_with(name) || rest[j + 2..].starts_with('>'));
            if !closes {
                return None;
            }
        }
        self.scan_xml_elements(i)
    }

    /// The end of a run of XML markup starting at `i` (one root element).
    fn scan_xml_elements(&self, mut i: usize) -> Option<usize> {
        let skip_past =
            |from: usize, pat: &[u8]| self.find(from, pat).map_or(self.end, |j| j + pat.len());
        let mut depth = 0i32;
        while i < self.end {
            if self.starts_with(i, b"<%=") {
                i = self.skip_embedded(i);
            } else if self.starts_with(i, b"<!--") {
                i = skip_past(i, b"-->");
            } else if self.starts_with(i, b"<![CDATA[") {
                i = skip_past(i, b"]]>");
            } else if self.starts_with(i, b"<?") {
                i = skip_past(i, b"?>");
            } else if self.starts_with(i, b"</") {
                i = skip_past(i, b">");
                depth -= 1;
                if depth <= 0 {
                    return Some(i);
                }
            } else if self.b[i] == b'<' {
                let j = self.tag_end(i);
                let self_close = j < self.end && self.b[j - 1] == b'/';
                i = (j + 1).min(self.end);
                if !self_close {
                    depth += 1;
                } else if depth == 0 {
                    return Some(i);
                }
            } else {
                i += 1;
            }
        }
        None
    }

    /// The offset of the `>` closing the tag that starts at `i` (skipping
    /// quoted attribute values and embedded expressions), or the end.
    fn tag_end(&self, i: usize) -> usize {
        let mut j = i + 1;
        while j < self.end && self.b[j] != b'>' {
            if self.starts_with(j, b"<%=") {
                j = self.skip_embedded(j);
                continue;
            }
            if matches!(self.b[j], b'"' | b'\'') {
                let q = self.b[j];
                j += 1;
                while j < self.end && self.b[j] != q {
                    j += 1;
                }
            }
            j += 1;
        }
        j
    }

    /// Skip an embedded expression `<%= … %>` starting at `i`, which may
    /// itself contain XML literals with embedded expressions.
    fn skip_embedded(&self, mut i: usize) -> usize {
        let mut depth = 0usize;
        while i < self.end {
            if self.starts_with(i, b"<%=") {
                depth += 1;
                i += 3;
            } else if self.starts_with(i, b"%>") {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return i;
                }
            } else {
                i += 1;
            }
        }
        self.end
    }
}

/// Unicode combining marks / connectors that may continue an identifier.
fn is_combining(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036f}' | '\u{200c}' | '\u{200d}' | '\u{203f}' | '\u{2040}'
    )
}
