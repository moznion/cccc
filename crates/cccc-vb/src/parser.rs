//! Recursive-descent parser for Visual Basic .NET that lowers straight into the
//! complexity IR (there is no intermediate AST: each parse function returns the
//! IR nodes of the construct it consumed).
//!
//! VB is statement- and line-oriented, and every block ends with its own
//! terminator (`End If`, `Next`, `Loop`, `End Sub`, …), so error recovery is
//! local: a block loop stops at any terminator that some *open* block owns
//! (reporting the blocks it skips as unterminated), and a terminator nobody
//! owns is reported and skipped. A malformed line never derails the rest of
//! the file.
//!
//! Line breaks: a token's `nl_before` ends the statement only at nesting depth
//! 0 (`nested`), and only where the grammar could stop — after a complete
//! operand, never right after an operator, `(`, `,`, `=`, etc. That is VB's
//! implicit line continuation. A multi-line lambda resets the depth for its
//! body, so statements inside it end at line breaks again.

use cccc_core::ir::{LogicalOp, Node, SwitchCase};

use crate::lexer::{DirKind, EOF, KEYWORDS, Kind, Token};

/// A block whose terminator the parser is waiting for.
#[derive(Clone, PartialEq, Eq)]
enum Block {
    If,
    Select,
    For,
    Do,
    While,
    Try,
    With,
    Using,
    SyncLock,
    Sub,
    Function,
    Operator,
    Property(String),
    Get,
    Set,
    Event(String),
    AddHandler,
    RemoveHandler,
    RaiseEvent,
    Class,
    Structure,
    Module,
    Interface,
    Enum,
    Namespace,
    /// A multi-line `Sub(…)` / `Function(…)` lambda (`true` = `Function`).
    Lambda(bool),
    /// A preprocessor `#If` group parsed as structured branches.
    Preproc(usize),
}

impl Block {
    /// The word after `End` that closes this block.
    fn end_word(&self) -> Option<&'static str> {
        Some(match self {
            Block::If => "if",
            Block::Select => "select",
            Block::While => "while",
            Block::Try => "try",
            Block::With => "with",
            Block::Using => "using",
            Block::SyncLock => "synclock",
            Block::Sub | Block::Lambda(false) => "sub",
            Block::Function | Block::Lambda(true) => "function",
            Block::Operator => "operator",
            Block::Property(_) => "property",
            Block::Get => "get",
            Block::Set => "set",
            Block::Event(_) => "event",
            Block::AddHandler => "addhandler",
            Block::RemoveHandler => "removehandler",
            Block::RaiseEvent => "raiseevent",
            Block::Class => "class",
            Block::Structure => "structure",
            Block::Module => "module",
            Block::Interface => "interface",
            Block::Enum => "enum",
            Block::Namespace => "namespace",
            Block::For | Block::Do | Block::Preproc(_) => return None,
        })
    }

    /// An executable member body (where statements, labels, and calls live).
    fn is_member_body(&self) -> bool {
        matches!(
            self,
            Block::Sub
                | Block::Function
                | Block::Operator
                | Block::Get
                | Block::Set
                | Block::AddHandler
                | Block::RemoveHandler
                | Block::RaiseEvent
        )
    }

    fn is_type(&self) -> bool {
        matches!(
            self,
            Block::Class | Block::Structure | Block::Module | Block::Interface | Block::Namespace
        )
    }
}

/// Words that may follow `End` as a block terminator.
const END_WORDS: &[&str] = &[
    "if",
    "select",
    "while",
    "try",
    "with",
    "using",
    "synclock",
    "sub",
    "function",
    "operator",
    "property",
    "get",
    "set",
    "event",
    "addhandler",
    "removehandler",
    "raiseevent",
    "class",
    "structure",
    "module",
    "interface",
    "enum",
    "namespace",
];

/// Modifiers of member and local declarations alike.
const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "friend",
    "protected",
    "shared",
    "overrides",
    "overridable",
    "notoverridable",
    "mustoverride",
    "overloads",
    "shadows",
    "readonly",
    "writeonly",
    "partial",
    "default",
    "widening",
    "narrowing",
    "mustinherit",
    "notinheritable",
];

/// Keywords that start a variable declarator list (`Dim x`, `Const y`, …).
const LOCAL_MODIFIERS: &[&str] = &["withevents", "dim", "const", "static"];

/// Contextual modifiers, only modifiers right before a member declaration.
const CONTEXTUAL_MODIFIERS: &[&str] = &["async", "iterator", "custom"];

/// Keywords that start a member or type declaration.
const DECLARATION_WORDS: &[&str] = &[
    "sub",
    "function",
    "property",
    "operator",
    "event",
    "delegate",
    "declare",
    "class",
    "structure",
    "module",
    "interface",
    "enum",
    "namespace",
];

/// Query clause keywords that continue a query expression onto a new line.
const QUERY_CLAUSES: &[&str] = &[
    "from",
    "aggregate",
    "where",
    "select",
    "order",
    "group",
    "join",
    "let",
    "distinct",
    "skip",
    "take",
    "into",
    "on",
];

/// Query keywords that separate the expressions inside a query.
const QUERY_WORDS: &[&str] = &[
    "from",
    "aggregate",
    "where",
    "select",
    "order",
    "by",
    "group",
    "join",
    "on",
    "equals",
    "into",
    "let",
    "distinct",
    "skip",
    "take",
    "while",
    "ascending",
    "descending",
    "in",
];

/// Statement keywords that can never be a label name.
const RESERVED_STATEMENTS: &[&str] = &[
    "try",
    "else",
    "do",
    "loop",
    "finally",
    "catch",
    "next",
    "end",
    "return",
    "exit",
    "continue",
    "stop",
    "resume",
    "endif",
    "wend",
    "case",
    "with",
    "using",
    "synclock",
    "while",
    "then",
    "get",
    "set",
    "addhandler",
    "removehandler",
    "raiseevent",
    "elseif",
];

/// Non-logical binary operators (all one precedence level here: their relative
/// precedence never changes a score).
const BINARY_OPS: &[&str] = &[
    "+", "-", "*", "/", "\\", "^", "&", "=", "<>", "<", ">", "<=", ">=", "<<", ">>", "+=", "-=",
    "*=", "/=", "\\=", "^=", "&=", "<<=", ">>=", "mod", "is", "isnot", "like",
];

/// An expression's lowering. A logical run is kept open so an enclosing run
/// of the same operator can absorb it (`(a AndAlso b) AndAlso c` is one run).
enum Expr {
    Logical(LogicalOp, Vec<Node>),
    Nodes(Vec<Node>),
}

impl Expr {
    fn empty() -> Self {
        Expr::Nodes(Vec::new())
    }

    fn into_nodes(self) -> Vec<Node> {
        match self {
            Expr::Logical(op, operands) => vec![Node::Logical { op, operands }],
            Expr::Nodes(nodes) => nodes,
        }
    }

    /// Append this expression as an operand of a `op` run.
    fn fold_into(self, op: LogicalOp, operands: &mut Vec<Node>) {
        match self {
            Expr::Logical(o, inner) if o == op => operands.extend(inner),
            Expr::Logical(o, inner) => operands.push(Node::Logical {
                op: o,
                operands: inner,
            }),
            Expr::Nodes(nodes) => operands.push(Node::Group(nodes)),
        }
    }
}

/// A preprocessor `#If` … `#End If` group: token indices of its directives.
struct Group {
    /// `#ElseIf` / `#Else` directives.
    arms: Vec<(usize, DirKind)>,
    end: Option<usize>,
}

/// Parser state to rewind to when a speculative `#If` parse fails.
struct Checkpoint {
    pos: usize,
    errors: usize,
    pending_next: usize,
    /// Groups from the speculated one on (only those can change).
    group: usize,
    flat: Vec<bool>,
}

pub(crate) struct Parser<'a> {
    src: &'a str,
    toks: &'a [Token],
    pos: usize,
    /// Depth of enclosing `(…)` / `{…}`; line breaks are insignificant inside.
    nested: u32,
    /// Index of the token that starts the current statement (its own
    /// preceding line break does not end the statement).
    stmt_start: usize,
    /// 1-based lines of syntax errors.
    pub errors: Vec<u32>,
    open: Vec<Block>,
    /// Loops still to be closed by a `Next a, b` that closed an inner loop.
    pending_next: usize,
    /// Names of the enclosing units (for recursion detection; VB is
    /// case-insensitive, so calls are normalized to the declared spelling).
    units: Vec<String>,
    groups: Vec<Group>,
    /// For each token, the preprocessor group its directive belongs to.
    group_of: Vec<Option<usize>>,
    /// Groups whose arms do not nest as blocks: only the first arm is parsed
    /// (the others are skipped, like a compiler's inactive regions).
    flat: Vec<bool>,
    /// Groups whose structured parse already failed. Not rewound with a
    /// [`Checkpoint`]: re-parsing an enclosing group after its own attempt
    /// failed must not retry these, or nested groups cost 2^depth.
    structured_failed: Vec<bool>,
}

impl<'a> Parser<'a> {
    /// A parser over a token sub-list (interpolation hole, directive
    /// condition), which never holds directives.
    fn bare(src: &'a str, toks: &'a [Token]) -> Self {
        Self {
            src,
            toks,
            pos: 0,
            nested: 0,
            stmt_start: 0,
            errors: Vec::new(),
            open: Vec::new(),
            pending_next: 0,
            units: Vec::new(),
            groups: Vec::new(),
            group_of: Vec::new(),
            flat: Vec::new(),
            structured_failed: Vec::new(),
        }
    }

    pub(crate) fn new(src: &'a str, toks: &'a [Token]) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        let mut group_of = vec![None; toks.len()];
        let mut stack = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            if let Kind::Directive(d) = &t.kind {
                match d.kind {
                    DirKind::If => {
                        stack.push(groups.len());
                        group_of[i] = Some(groups.len());
                        groups.push(Group {
                            arms: Vec::new(),
                            end: None,
                        });
                    }
                    DirKind::ElseIf | DirKind::Else => {
                        if let Some(&g) = stack.last() {
                            groups[g].arms.push((i, d.kind));
                            group_of[i] = Some(g);
                        }
                    }
                    DirKind::EndIf => {
                        if let Some(g) = stack.pop() {
                            groups[g].end = Some(i);
                            group_of[i] = Some(g);
                        }
                    }
                    DirKind::Other => {}
                }
            }
        }
        let flat = vec![false; groups.len()];
        let structured_failed = vec![false; groups.len()];
        Self {
            groups,
            group_of,
            flat,
            structured_failed,
            ..Self::bare(src, toks)
        }
    }

    /// Lower an expression token sub-list (interpolation hole, directive
    /// condition) with the enclosing unit names lent for recursion
    /// detection. Returns the nodes and the sub-list's syntax errors.
    fn sub_expr(&mut self, toks: &'a [Token]) -> (Vec<Node>, Vec<u32>) {
        let mut sub = Parser::bare(self.src, toks);
        sub.units = std::mem::take(&mut self.units);
        sub.nested = 1;
        let nodes = sub.parse_expr();
        self.units = sub.units;
        (nodes, sub.errors)
    }

    /// Parse a whole file into its module-level nodes.
    pub(crate) fn parse_file(&mut self) -> Vec<Node> {
        let mut out = Vec::new();
        loop {
            out.extend(self.parse_block());
            if self.at_eof() {
                break;
            }
            self.error_here();
            if self.pending_next > 0 {
                // `Next a, b` naming more loops than are open.
                self.pending_next = 0;
            } else {
                // A terminator no open block owns at top level.
                self.skip_line();
            }
        }
        out
    }

    // ---- token access ----------------------------------------------------------

    fn tok(&self, i: usize) -> &'a Token {
        self.toks.get(i).unwrap_or(&EOF)
    }

    fn cur(&self) -> &'a Token {
        self.tok(self.pos)
    }

    fn peek(&self, n: usize) -> &'a Token {
        self.tok(self.pos + n)
    }

    fn bump(&mut self) {
        if self.pos < self.toks.len() {
            self.pos += 1;
        }
    }

    fn text(&self, t: &Token) -> &'a str {
        &self.src[t.start..t.end]
    }

    fn at_eof(&self) -> bool {
        matches!(self.cur().kind, Kind::Eof)
    }

    /// `t` is the (unescaped) keyword `kw`.
    fn is_kw(&self, t: &Token, kw: &str) -> bool {
        debug_assert!(
            KEYWORDS.binary_search(&kw).is_ok(),
            "{kw} is not in KEYWORDS"
        );
        t.kw == Some(kw)
    }

    fn kw_in(&self, t: &Token, set: &[&str]) -> bool {
        t.kw.is_some_and(|kw| set.contains(&kw))
    }

    fn at_kw(&self, kw: &str) -> bool {
        self.is_kw(self.cur(), kw)
    }

    fn is_punct(&self, t: &Token, p: &str) -> bool {
        matches!(t.kind, Kind::Punct) && self.text(t) == p
    }

    fn at_punct(&self, p: &str) -> bool {
        self.is_punct(self.cur(), p)
    }

    /// The keyword at the cursor, if it is one.
    fn cur_word(&self) -> Option<&'static str> {
        self.cur().kw
    }

    /// A line break before the cursor ends the current statement here.
    fn line_breaks_here(&self) -> bool {
        self.nested == 0 && self.cur().nl_before && self.pos != self.stmt_start
    }

    /// The current line ends here (or the input / a directive line does).
    fn at_line_end(&self) -> bool {
        is_hard_end(self.cur()) || self.line_breaks_here()
    }

    /// Inside brackets (a single-line lambda's statement), the enclosing list
    /// continues here.
    fn at_list_closer(&self) -> bool {
        self.nested > 0 && (self.at_punct(")") || self.at_punct(",") || self.at_punct("}"))
    }

    /// The cursor is past the end of the current statement.
    fn at_stmt_end(&self) -> bool {
        self.at_line_end() || self.at_list_closer() || (self.nested == 0 && self.at_punct(":"))
    }

    /// The end of a single-line `If` arm: the line ends, its `Else` follows,
    /// or the enclosing list continues (a colon separates statements within
    /// the arm).
    fn at_inline_end(&self) -> bool {
        self.at_line_end() || self.at_list_closer() || self.at_kw("else")
    }

    /// Run `f` at bracket depth `depth`.
    fn with_nested<T>(&mut self, depth: u32, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.nested, depth);
        let r = f(self);
        self.nested = saved;
        r
    }

    /// Run `f` one bracket level deeper (line breaks are insignificant).
    fn bracketed<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.with_nested(self.nested + 1, f)
    }

    /// Skip a named-argument prefix `name:=`.
    fn skip_named_arg(&mut self) {
        if matches!(self.cur().kind, Kind::Ident { .. }) && self.is_punct(self.peek(1), ":=") {
            self.bump();
            self.bump();
        }
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        let hit = self.at_kw(kw);
        if hit {
            self.bump();
        }
        hit
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        let hit = self.at_punct(p);
        if hit {
            self.bump();
        }
        hit
    }

    fn error_here(&mut self) {
        let line = self.cur().line.max(1);
        if self.errors.last() != Some(&line) {
            self.errors.push(line);
        }
    }

    /// Skip to the start of the next line (or a directive / end of input).
    fn skip_line(&mut self) {
        self.bump();
        while !is_hard_end(self.cur()) && !self.cur().nl_before {
            self.bump();
        }
    }

    /// Skip the rest of the current statement.
    fn skip_rest(&mut self) {
        while !self.at_stmt_end() {
            self.bump();
        }
    }

    /// Skip the rest of a declaration-like statement (`Declare …`, `Handles
    /// a, b`, `Imports <xmlns:p="…">`) that may continue implicitly across
    /// lines: inside brackets, or after a `,` / `.` / `(` / operator.
    fn skip_clause(&mut self) {
        let mut depth = 0i32;
        let mut first = true;
        loop {
            let t = self.cur();
            if is_hard_end(t) {
                return;
            }
            if !first && depth <= 0 && t.nl_before {
                let prev = self.tok(self.pos - 1);
                let continues = matches!(prev.kind, Kind::Punct)
                    && matches!(self.text(prev), "," | "." | "(" | "=" | "&" | "+" | "<");
                if !continues {
                    return;
                }
            }
            if depth <= 0 && self.is_punct(t, ":") {
                return;
            }
            match self.text(t) {
                "(" | "{" | "<" if matches!(t.kind, Kind::Punct) => depth += 1,
                ")" | "}" | ">" if matches!(t.kind, Kind::Punct) => depth -= 1,
                _ => {}
            }
            first = false;
            self.bump();
        }
    }

    /// The display name of an identifier token (brackets and type character
    /// stripped).
    fn ident_name(&self, t: &Token) -> String {
        let text = self.text(t);
        let text = text
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(text);
        text.trim_end_matches(['%', '&', '@', '!', '#', '$'])
            .to_string()
    }

    /// A call to the name token `name`: spelled like the enclosing unit when
    /// it matches it case-insensitively, so recursion is detected.
    fn call(&self, name: Option<&Token>) -> Node {
        let callee = name.map(|t| {
            let n = self.ident_name(t);
            match self.units.last() {
                Some(unit) if unit.eq_ignore_ascii_case(&n) => unit.clone(),
                _ => n,
            }
        });
        Node::Call { callee }
    }

    // ---- blocks ----------------------------------------------------------------

    /// Parse statements until end of input, a pending `Next`, or a terminator
    /// that an open block owns (left for its owner to consume).
    fn parse_block(&mut self) -> Vec<Node> {
        let mut out = Vec::new();
        loop {
            while self.at_punct(":") {
                self.bump();
            }
            if self.at_eof() || self.pending_next > 0 {
                return out;
            }
            if let Kind::Directive(d) = &self.cur().kind {
                match d.kind {
                    DirKind::If => self.parse_preproc(&mut out),
                    DirKind::ElseIf | DirKind::Else | DirKind::EndIf => {
                        match self.group_of[self.pos] {
                            Some(g) if self.flat[g] => {
                                if d.kind == DirKind::EndIf {
                                    self.bump();
                                } else {
                                    self.skip_inactive_arms(g);
                                }
                            }
                            Some(g) if self.open.contains(&Block::Preproc(g)) => return out,
                            _ => {
                                self.error_here();
                                self.bump();
                            }
                        }
                    }
                    DirKind::Other => self.bump(),
                }
                continue;
            }
            match self.terminator() {
                Some(true) => return out,
                Some(false) => {
                    self.error_here();
                    self.skip_line();
                    continue;
                }
                None => {}
            }
            let before = self.pos;
            self.parse_statement(&mut out);
            if self.pos == before {
                self.error_here();
                self.bump();
            }
            if !self.at_stmt_end() {
                self.error_here();
                self.skip_rest();
            }
        }
    }

    /// Whether the cursor starts a block terminator: `Some(true)` if an open
    /// block owns it, `Some(false)` if none does, `None` if it is not one.
    fn terminator(&self) -> Option<bool> {
        let word = self.cur_word()?;
        let owned = |pred: &dyn Fn(&Block) -> bool| Some(self.open.iter().any(pred));
        match word {
            "end" => {
                let next = self.peek(1);
                if next.nl_before || !self.kw_in(next, END_WORDS) {
                    return None; // the `End` statement
                }
                owned(&|b| b.end_word() == next.kw)
            }
            "endif" | "else" | "elseif" => owned(&|b| *b == Block::If),
            "next" => owned(&|b| *b == Block::For),
            "loop" => owned(&|b| *b == Block::Do),
            "wend" => owned(&|b| *b == Block::While),
            "case" => owned(&|b| *b == Block::Select),
            "catch" | "finally" => owned(&|b| *b == Block::Try),
            _ => {
                // A member / type declaration inside a member body means the
                // body was never closed: let the enclosing type take over.
                let in_body = self.open.last().is_some_and(Block::is_member_body);
                (in_body && self.at_declaration() && self.open.iter().any(Block::is_type))
                    .then_some(true)
            }
        }
    }

    /// The cursor starts a member or type declaration (after modifiers).
    fn at_declaration(&self) -> bool {
        let mut i = self.pos;
        while self.is_member_modifier(self.tok(i)) {
            i += 1;
        }
        let t = self.tok(i);
        if self.is_kw(t, "sub") || self.is_kw(t, "function") {
            // `Sub(…)` would be a lambda.
            return !self.is_punct(self.tok(i + 1), "(");
        }
        self.kw_in(t, DECLARATION_WORDS)
    }

    fn is_member_modifier(&self, t: &Token) -> bool {
        self.kw_in(t, MODIFIERS) || self.kw_in(t, CONTEXTUAL_MODIFIERS)
    }

    /// Expect `End <word>` closing the current block.
    fn expect_end(&mut self, word: &str) {
        if self.at_kw("end") && self.is_kw(self.peek(1), word) && !self.peek(1).nl_before {
            self.bump();
            self.bump();
        } else {
            self.error_here();
        }
    }

    /// Run `f` with `block` open.
    fn within<T>(&mut self, block: Block, f: impl FnOnce(&mut Self) -> T) -> T {
        self.open.push(block);
        let r = f(self);
        self.open.pop();
        r
    }

    /// Run `f` as the body of the unit `name`.
    fn in_unit<T>(&mut self, name: &str, f: impl FnOnce(&mut Self) -> T) -> T {
        self.units.push(name.to_string());
        let r = f(self);
        self.units.pop();
        r
    }

    /// The statements of `block` up to its `End …` terminator.
    fn close_block(&mut self, block: Block) -> Vec<Node> {
        let word = block.end_word();
        self.within(block, |p| {
            let body = p.parse_block();
            if let Some(word) = word {
                p.expect_end(word);
            }
            body
        })
    }

    /// The body of the unit `name`, a `block` up to its terminator.
    fn unit_body(&mut self, block: Block, name: &str) -> Vec<Node> {
        self.in_unit(name, |p| p.close_block(block))
    }

    // ---- preprocessor ----------------------------------------------------------

    /// `#If` … `#End If`. When every arm nests as complete statements it
    /// becomes a structured [`Node::Branch`] chain; otherwise (an arm opens a
    /// block another arm or the code after `#End If` closes) only the first
    /// arm is parsed in place and the chain is scored without bodies.
    fn parse_preproc(&mut self, out: &mut Vec<Node>) {
        let start = self.pos;
        let Some(g) = self.group_of[start] else {
            self.bump();
            return;
        };
        if !self.structured_failed[g] {
            let checkpoint = self.checkpoint(g);
            if let Some(branch) = self.try_structured_preproc(g)
                && self.errors.len() == checkpoint.errors
                && self.pending_next == checkpoint.pending_next
            {
                out.push(branch);
                return;
            }
            self.restore(checkpoint);
            self.structured_failed[g] = true;
        }
        out.push(self.flat_preproc(g));
    }

    /// Parse group `g` flat from its `#If` at the cursor: only the first arm
    /// is parsed in place (later arms are skipped when reached) and the chain
    /// is scored without bodies.
    fn flat_preproc(&mut self, g: usize) -> Node {
        let start = self.pos;
        self.flat[g] = true;
        let mut arms = vec![self.directive_cond(start)];
        let mut has_else = false;
        for k in 0..self.groups[g].arms.len() {
            match self.groups[g].arms[k] {
                (_, DirKind::Else) => has_else = true,
                (arm, _) => {
                    let cond = self.directive_cond(arm);
                    arms.push(cond);
                }
            }
        }
        if self.groups[g].end.is_none() {
            self.error_here();
        }
        self.bump();
        branch_chain(
            arms.into_iter().map(|test| (test, Vec::new())).collect(),
            has_else.then(Vec::new),
        )
    }

    /// Skip the inactive arms of flat group `g` from its `#ElseIf` / `#Else`
    /// at the cursor, past its `#End If`.
    fn skip_inactive_arms(&mut self, g: usize) {
        self.pos = self.groups[g].end.unwrap_or(self.toks.len());
        self.bump();
    }

    /// Directives inside an expression list (arguments, initializers): a
    /// `#If` group there is parsed flat.
    fn skip_inline_directives(&mut self, out: &mut Vec<Node>) {
        while let Kind::Directive(d) = &self.cur().kind {
            match (d.kind, self.group_of.get(self.pos).copied().flatten()) {
                (DirKind::If, Some(g)) => out.push(self.flat_preproc(g)),
                (DirKind::ElseIf | DirKind::Else, Some(g)) if self.flat[g] => {
                    self.skip_inactive_arms(g);
                }
                _ => self.bump(),
            }
        }
    }

    fn checkpoint(&self, group: usize) -> Checkpoint {
        Checkpoint {
            pos: self.pos,
            errors: self.errors.len(),
            pending_next: self.pending_next,
            group,
            flat: self.flat[group..].to_vec(),
        }
    }

    fn restore(&mut self, c: Checkpoint) {
        self.pos = c.pos;
        self.errors.truncate(c.errors);
        self.pending_next = c.pending_next;
        self.flat.truncate(c.group);
        self.flat.extend(c.flat);
    }

    fn try_structured_preproc(&mut self, g: usize) -> Option<Node> {
        let mut test = self.directive_cond(self.pos);
        self.bump();
        self.within(Block::Preproc(g), move |p| {
            let mut arms = Vec::new();
            loop {
                let body = p.parse_block();
                match p.group_directive(g)? {
                    DirKind::ElseIf => {
                        arms.push((std::mem::take(&mut test), body));
                        test = p.directive_cond(p.pos);
                        p.bump();
                    }
                    DirKind::Else => {
                        arms.push((std::mem::take(&mut test), body));
                        p.bump();
                        let else_body = p.parse_block();
                        if p.group_directive(g)? != DirKind::EndIf {
                            return None;
                        }
                        p.bump();
                        return Some(branch_chain(arms, Some(else_body)));
                    }
                    DirKind::EndIf => {
                        arms.push((test, body));
                        p.bump();
                        return Some(branch_chain(arms, None));
                    }
                    _ => return None,
                }
            }
        })
    }

    /// The kind of the directive at the cursor if it belongs to group `g`.
    fn group_directive(&self, g: usize) -> Option<DirKind> {
        match &self.cur().kind {
            Kind::Directive(d) if self.group_of[self.pos] == Some(g) => Some(d.kind),
            _ => None,
        }
    }

    /// The lowered condition of the `#If` / `#ElseIf` directive at token `i`.
    fn directive_cond(&mut self, i: usize) -> Vec<Node> {
        match &self.tok(i).kind {
            Kind::Directive(d) if !d.cond.is_empty() => self.sub_expr(&d.cond).0,
            _ => Vec::new(),
        }
    }

    // ---- statements --------------------------------------------------------------

    fn parse_statement(&mut self, out: &mut Vec<Node>) {
        self.stmt_start = self.pos;
        self.skip_attributes();
        // Attributes may stand on their own line before the declaration.
        self.stmt_start = self.pos;
        if self.at_stmt_end() {
            return;
        }
        // `label:` (only inside a body; in an `Enum`, `A : B` lists members)
        if matches!(self.cur().kind, Kind::Ident { .. } | Kind::Literal)
            && self.is_punct(self.peek(1), ":")
            && !self.peek(1).nl_before
            && !self.kw_in(self.cur(), RESERVED_STATEMENTS)
            && self
                .open
                .iter()
                .any(|b| b.is_member_body() || matches!(b, Block::Lambda(_)))
        {
            self.bump();
            self.bump();
            if !self.at_stmt_end() {
                self.parse_statement(out);
            }
            return;
        }
        let Some(word) = self.cur_word() else {
            self.parse_expression_statement(out);
            return;
        };
        match word {
            "if" => self.parse_if(out),
            "select" if !self.peek(1).nl_before => self.parse_select(out),
            "for" => self.parse_for(out),
            "do" => self.parse_do(out),
            "while" => self.parse_while(out),
            "try" => self.parse_try(out),
            "with" => self.parse_simple_block(Block::With, out),
            "using" => self.parse_simple_block(Block::Using, out),
            "synclock" => self.parse_simple_block(Block::SyncLock, out),
            "goto" => {
                self.bump();
                self.skip_rest();
                out.push(Node::Jump { labeled: true });
            }
            "on" if self.is_kw(self.peek(1), "error") => self.parse_on_error(out),
            "resume" => {
                // `Resume`, `Resume Next`, `Resume label`: a jump back into
                // (or past) the failing code.
                self.bump();
                self.skip_rest();
                out.push(Node::Jump { labeled: true });
            }
            "exit" | "continue" => {
                self.bump();
                self.skip_rest();
                out.push(Node::Jump { labeled: false });
            }
            "return" | "throw" | "yield" | "error" => {
                self.bump();
                if !self.at_stmt_end() {
                    out.extend(self.parse_expr());
                }
            }
            "call" => {
                self.bump();
                self.parse_expression_statement(out);
            }
            "addhandler" | "removehandler" | "raiseevent" if self.in_custom_event() => {
                self.parse_accessor(word, out);
            }
            "raiseevent" => {
                self.bump();
                self.bump(); // event name
                if self.at_punct("(") {
                    out.extend(self.parse_args());
                }
            }
            "addhandler" | "removehandler" => {
                self.bump();
                out.extend(self.parse_expr());
                if self.eat_punct(",") {
                    out.extend(self.parse_expr());
                }
            }
            "redim" | "erase" => {
                self.bump();
                self.eat_kw("preserve");
                out.extend(self.parse_expr_list());
            }
            "stop" | "end" => {
                self.bump();
            }
            "imports" | "option" | "inherits" | "implements" => self.skip_clause(),
            "namespace" => self.parse_type_block(Block::Namespace, out),
            _ => self.parse_declaration_or_expression(out),
        }
    }

    fn in_custom_event(&self) -> bool {
        matches!(self.open.last(), Some(Block::Event(_))) && self.is_punct(self.peek(1), "(")
    }

    fn in_property(&self) -> bool {
        matches!(self.open.last(), Some(Block::Property(_)))
    }

    /// Declarations here are interface members (which have no bodies).
    fn in_interface(&self) -> bool {
        self.open
            .iter()
            .rev()
            .find(|b| b.is_type() || **b == Block::Enum)
            == Some(&Block::Interface)
    }

    fn parse_declaration_or_expression(&mut self, out: &mut Vec<Node>) {
        let start = self.pos;
        let mut mods: Vec<&str> = Vec::new();
        while let Some(w) = self.cur_word() {
            let next = self.peek(1);
            let is_mod = match w {
                "async" | "iterator" => {
                    self.kw_in(next, &["sub", "function", "property"])
                        || self.is_member_modifier(next)
                }
                "custom" => self.is_kw(next, "event"),
                _ => MODIFIERS.contains(&w) || LOCAL_MODIFIERS.contains(&w),
            };
            if !is_mod {
                break;
            }
            mods.push(w);
            self.bump();
        }
        let has = |m: &str| mods.contains(&m);
        match self.cur_word() {
            Some(w @ ("get" | "set")) if self.in_property() => return self.parse_accessor(w, out),
            Some("class") => return self.parse_type_block(Block::Class, out),
            Some("structure") => return self.parse_type_block(Block::Structure, out),
            Some("module") => return self.parse_type_block(Block::Module, out),
            Some("interface") => return self.parse_type_block(Block::Interface, out),
            Some("enum") => return self.parse_type_block(Block::Enum, out),
            Some("sub") if !self.is_punct(self.peek(1), "(") || !mods.is_empty() => {
                return self.parse_method(Block::Sub, has("mustoverride"), has("partial"), out);
            }
            Some("function") if !self.is_punct(self.peek(1), "(") || !mods.is_empty() => {
                return self.parse_method(
                    Block::Function,
                    has("mustoverride"),
                    has("partial"),
                    out,
                );
            }
            Some("operator") => {
                return self.parse_method(Block::Operator, has("mustoverride"), false, out);
            }
            Some("property") => return self.parse_property(has("mustoverride"), out),
            Some("event") => return self.parse_event(has("custom"), out),
            Some("delegate" | "declare") => return self.skip_clause(),
            _ => {}
        }
        if self.pos > start {
            self.parse_declarators(out);
        } else {
            self.parse_expression_statement(out);
        }
    }

    /// An expression or assignment statement. A statement that is just a
    /// (member-access) name is a call to a parameterless `Sub` (`DoWork`,
    /// `Me.Refresh`).
    fn parse_expression_statement(&mut self, out: &mut Vec<Node>) {
        let start = self.pos;
        let nodes = self.parse_expr();
        let consumed = &self.toks[start..self.pos];
        let name_chain = consumed.len() % 2 == 1
            && consumed.iter().enumerate().all(|(k, t)| {
                if k % 2 == 0 {
                    matches!(t.kind, Kind::Ident { .. })
                } else {
                    self.is_punct(t, ".")
                }
            });
        if name_chain && (self.at_stmt_end() || self.at_kw("else")) {
            let mybase = self.kw_in(&consumed[0], &["mybase", "myclass"]);
            if !(mybase && consumed.len() == 1) {
                out.push(self.call(consumed.last().filter(|_| !mybase)));
            }
        }
        out.extend(nodes);
    }

    /// `If` — block form (`If c [Then]` … `ElseIf` … `Else` … `End If`) or
    /// single-line form (`If c Then a : b Else c`).
    fn parse_if(&mut self, out: &mut Vec<Node>) {
        self.bump();
        let test = self.parse_expr();
        let has_then = self.eat_kw("then");
        if !has_then && !self.at_stmt_end() {
            self.error_here();
            self.skip_rest();
        }
        if has_then && !self.at_line_end() {
            let then = self.parse_inline_statements();
            let alternate = if self.eat_kw("else") {
                Some(self.parse_inline_statements())
            } else {
                None
            };
            out.push(Node::Branch {
                test,
                then,
                alternate: alternate.map(|a| Box::new(Node::Group(a))),
            });
            return;
        }
        let mut arms = Vec::new();
        let mut else_body = None;
        self.within(Block::If, |p| {
            let mut test = test;
            loop {
                let body = p.parse_block();
                arms.push((test, body));
                let else_if = p.at_kw("elseif")
                    || (p.at_kw("else") && p.is_kw(p.peek(1), "if") && !p.peek(1).nl_before);
                if else_if {
                    if p.at_kw("else") {
                        p.bump();
                    }
                    p.bump();
                    test = p.parse_expr();
                    p.eat_kw("then");
                    continue;
                }
                if p.eat_kw("else") {
                    else_body = Some(p.parse_block());
                }
                break;
            }
            if !p.eat_kw("endif") {
                p.expect_end("if");
            }
        });
        out.push(branch_chain(arms, else_body));
    }

    /// The `:`-separated statements of a single-line `If` arm, up to `Else` or
    /// the end of the line.
    fn parse_inline_statements(&mut self) -> Vec<Node> {
        let mut out = Vec::new();
        loop {
            while self.eat_punct(":") {}
            if self.at_inline_end() {
                break;
            }
            let before = self.pos;
            self.parse_statement(&mut out);
            if self.pos == before {
                self.error_here();
                self.skip_rest();
                break;
            }
            if !self.at_inline_end() && !self.at_punct(":") {
                self.error_here();
                self.skip_rest();
            }
        }
        out
    }

    /// `Select [Case] x` … `Case …` … `Case Else` … `End Select`.
    fn parse_select(&mut self, out: &mut Vec<Node>) {
        self.bump();
        self.eat_kw("case");
        out.extend(self.parse_expr());
        let mut cases = Vec::new();
        self.within(Block::Select, |p| {
            out.extend(p.parse_block());
            while p.eat_kw("case") {
                let mut body = Vec::new();
                let is_default = p.eat_kw("else");
                if !is_default {
                    body.extend(p.parse_case_clauses());
                }
                body.extend(p.parse_block());
                cases.push(SwitchCase { is_default, body });
            }
            p.expect_end("select");
        });
        out.push(Node::Switch { cases });
    }

    /// `Case 1, 2 To 5, Is > x`.
    fn parse_case_clauses(&mut self) -> Vec<Node> {
        let mut out = Vec::new();
        loop {
            if self.eat_kw("is") && matches!(self.cur().kind, Kind::Punct) {
                self.bump();
            }
            out.extend(self.parse_expr());
            if self.eat_kw("to") {
                out.extend(self.parse_expr());
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        out
    }

    /// `For i = a To b [Step s]` / `For Each x In xs` … `Next [i[, j…]]`.
    fn parse_for(&mut self, out: &mut Vec<Node>) {
        self.bump();
        self.eat_kw("each");
        let mut body = Vec::new();
        if matches!(self.cur().kind, Kind::Ident { .. }) && self.is_kw(self.peek(1), "as") {
            self.bump();
            self.bump();
            self.skip_type();
        } else {
            body.extend(self.parse_expr());
        }
        // `In xs` / `= a To b Step s`
        for (kw, punct) in [("in", false), ("=", true), ("to", false), ("step", false)] {
            if if punct {
                self.eat_punct(kw)
            } else {
                self.eat_kw(kw)
            } {
                body.extend(self.parse_expr());
            }
        }
        self.within(Block::For, |p| {
            body.extend(p.parse_block());
            if p.pending_next > 0 {
                p.pending_next -= 1;
            } else if p.eat_kw("next") {
                let mut closes = 1usize;
                if !p.at_stmt_end() {
                    p.parse_expr();
                    while p.eat_punct(",") {
                        p.parse_expr();
                        closes += 1;
                    }
                }
                let open_loops = p.open.iter().filter(|b| **b == Block::For).count();
                p.pending_next = closes.min(open_loops) - 1;
            } else {
                p.error_here();
            }
        });
        out.push(Node::Loop { body });
    }

    /// `Do [While|Until c]` … `Loop [While|Until c]`.
    fn parse_do(&mut self, out: &mut Vec<Node>) {
        self.bump();
        let mut body = Vec::new();
        if self.eat_kw("while") || self.eat_kw("until") {
            body.extend(self.parse_expr());
        }
        self.within(Block::Do, |p| {
            body.extend(p.parse_block());
            if p.eat_kw("loop") {
                if p.eat_kw("while") || p.eat_kw("until") {
                    body.extend(p.parse_expr());
                }
            } else {
                p.error_here();
            }
        });
        out.push(Node::Loop { body });
    }

    /// `While c` … `End While` (or the legacy `Wend`).
    fn parse_while(&mut self, out: &mut Vec<Node>) {
        self.bump();
        let mut body = self.parse_expr();
        self.within(Block::While, |p| {
            body.extend(p.parse_block());
            if !p.eat_kw("wend") {
                p.expect_end("while");
            }
        });
        out.push(Node::Loop { body });
    }

    /// `Try` … `Catch [e As T] [When f]` … `Finally` … `End Try`: the `Try`
    /// and `Finally` bodies run at the surrounding level; each `Catch` is a
    /// [`Node::Catch`] (its `When` filter scores inside it).
    fn parse_try(&mut self, out: &mut Vec<Node>) {
        self.bump();
        self.within(Block::Try, |p| {
            out.extend(p.parse_block());
            loop {
                if p.eat_kw("catch") {
                    let mut body = Vec::new();
                    if matches!(p.cur().kind, Kind::Ident { .. })
                        && !p.at_stmt_end()
                        && !p.at_kw("when")
                    {
                        p.bump();
                        if p.eat_kw("as") {
                            p.skip_type();
                        }
                    }
                    if p.eat_kw("when") {
                        body.extend(p.parse_expr());
                    }
                    body.extend(p.parse_block());
                    out.push(Node::Catch { body });
                } else if p.eat_kw("finally") {
                    out.extend(p.parse_block());
                } else {
                    break;
                }
            }
            p.expect_end("try");
        });
    }

    /// `With` / `Using` / `SyncLock` blocks are transparent.
    fn parse_simple_block(&mut self, block: Block, out: &mut Vec<Node>) {
        self.bump();
        if block == Block::Using
            && matches!(self.cur().kind, Kind::Ident { .. })
            && (self.is_kw(self.peek(1), "as") || self.is_punct(self.peek(1), "="))
        {
            self.parse_declarators(out);
        } else {
            out.extend(self.parse_expr());
        }
        out.extend(self.close_block(block));
    }

    /// `On Error GoTo label` / `On Error Resume Next` jump to a handler (or
    /// past the failing statement). `On Error GoTo 0` / `-1` only reset the
    /// handler and jump nowhere.
    fn parse_on_error(&mut self, out: &mut Vec<Node>) {
        self.bump();
        self.bump();
        let resets = self.at_kw("goto") && {
            let target = self.text(self.peek(1));
            target == "0" || (target == "-" && self.text(self.peek(2)) == "1")
        };
        self.skip_rest();
        if !resets {
            out.push(Node::Jump { labeled: true });
        }
    }

    // ---- declarations --------------------------------------------------------------

    /// `Class` / `Structure` / `Module` / `Interface` / `Enum` / `Namespace`:
    /// a transparent container.
    fn parse_type_block(&mut self, block: Block, out: &mut Vec<Node>) {
        self.bump();
        self.skip_clause();
        out.extend(self.close_block(block));
    }

    /// `Sub` / `Function` / `Operator`. Bodyless declarations
    /// (`MustOverride`, interface members, and an empty `Partial` method
    /// declaration) are not units; their parameter defaults still run at the
    /// surrounding level.
    fn parse_method(&mut self, block: Block, abstract_: bool, partial: bool, out: &mut Vec<Node>) {
        let line = self.cur().line;
        self.bump();
        let name_tok = self.cur();
        let (name, kind) = match block {
            Block::Operator => (format!("operator {}", self.text(name_tok)), "operator"),
            Block::Sub if self.is_kw(name_tok, "new") => ("New".to_string(), "constructor"),
            Block::Sub => (self.ident_name(name_tok), "sub"),
            _ => (self.ident_name(name_tok), "function"),
        };
        self.bump();
        let mut params = Vec::new();
        self.skip_type_args();
        if self.at_punct("(") {
            params = self.parse_params();
        }
        if self.eat_kw("as") {
            self.skip_attributes();
            self.skip_type();
        }
        if self.at_kw("handles") || self.at_kw("implements") {
            self.skip_clause();
        }
        if abstract_ || self.in_interface() {
            out.extend(params);
            return;
        }
        let body = self.unit_body(block, &name);
        if partial && body.is_empty() {
            out.extend(params);
            return;
        }
        params.extend(body);
        out.push(Node::Function {
            name,
            kind: kind.to_string(),
            line,
            body: params,
        });
    }

    /// `Property P[(…)] [As T] [= init]`: a block (with `Get` / `Set`
    /// accessor units named after the property) only when an accessor
    /// follows; otherwise an auto-property whose initializer runs at the
    /// surrounding level.
    fn parse_property(&mut self, abstract_: bool, out: &mut Vec<Node>) {
        self.bump();
        let name = self.ident_name(self.cur());
        self.bump();
        if self.at_punct("(") {
            out.extend(self.parse_params());
        }
        if self.eat_kw("as") {
            self.skip_attributes();
            if self.at_kw("new") {
                out.extend(self.parse_unary().into_nodes());
            } else {
                self.skip_type();
            }
        }
        if self.eat_punct("=") {
            out.extend(self.parse_expr());
        }
        if self.at_kw("implements") {
            self.skip_clause();
        }
        if abstract_ || self.in_interface() || !self.accessor_follows() {
            return;
        }
        out.extend(self.close_block(Block::Property(name)));
    }

    /// The next statement is a `Get` / `Set` accessor.
    fn accessor_follows(&self) -> bool {
        let mut i = self.pos;
        while matches!(self.tok(i).kind, Kind::Directive(_)) {
            i += 1;
        }
        i = self.attributes_end(i);
        while self.is_member_modifier(self.tok(i)) {
            i += 1;
        }
        self.kw_in(self.tok(i), &["get", "set"])
    }

    /// A property accessor (`Get` / `Set`) or custom-event accessor
    /// (`AddHandler` / `RemoveHandler` / `RaiseEvent`): a unit named after
    /// its property / event.
    fn parse_accessor(&mut self, word: &str, out: &mut Vec<Node>) {
        let name = match self.open.last() {
            Some(Block::Property(name) | Block::Event(name)) => name.clone(),
            _ => return,
        };
        let (block, kind) = match word {
            "get" => (Block::Get, "getter"),
            "set" => (Block::Set, "setter"),
            "addhandler" => (Block::AddHandler, "add"),
            "removehandler" => (Block::RemoveHandler, "remove"),
            _ => (Block::RaiseEvent, "raise"),
        };
        let line = self.cur().line;
        self.bump();
        let mut body = self.parse_params();
        body.extend(self.unit_body(block, &name));
        out.push(Node::Function {
            name,
            kind: kind.to_string(),
            line,
            body,
        });
    }

    /// `[Custom] Event E …`: only a `Custom Event` has a body (its
    /// `AddHandler` / `RemoveHandler` / `RaiseEvent` accessors are units).
    fn parse_event(&mut self, custom: bool, out: &mut Vec<Node>) {
        self.bump();
        let name = self.ident_name(self.cur());
        self.skip_clause();
        if !custom || self.in_interface() {
            return;
        }
        out.extend(self.close_block(Block::Event(name)));
    }

    /// Variable declarators after `Dim` / `Const` / modifiers:
    /// `a, b As T, c(10) As T = x, d As New T(…) With {…}`.
    fn parse_declarators(&mut self, out: &mut Vec<Node>) {
        loop {
            if !matches!(self.cur().kind, Kind::Ident { .. }) {
                if !self.at_stmt_end() {
                    out.extend(self.parse_expr());
                }
                return;
            }
            self.bump();
            self.eat_punct("?");
            while self.at_punct("(") {
                out.extend(self.parse_args());
            }
            self.eat_punct("?");
            if self.eat_kw("as") {
                self.skip_attributes();
                if self.at_kw("new") {
                    out.extend(self.parse_unary().into_nodes());
                } else {
                    self.skip_type();
                }
            }
            if self.eat_punct("=") {
                out.extend(self.parse_expr());
            }
            if !self.eat_punct(",") {
                return;
            }
        }
    }

    /// A parameter list `(ByVal a As T, Optional b As T = x, …)`: returns the
    /// default-value expressions.
    fn parse_params(&mut self) -> Vec<Node> {
        let mut out = Vec::new();
        if !self.eat_punct("(") {
            return out;
        }
        self.bracketed(|p| p.parse_param_list(&mut out));
        out
    }

    fn parse_param_list(&mut self, out: &mut Vec<Node>) {
        while !self.at_punct(")") && !self.at_eof() {
            let before = self.pos;
            self.skip_attributes();
            while self.kw_in(self.cur(), &["byval", "byref", "optional", "paramarray"]) {
                self.bump();
            }
            if matches!(self.cur().kind, Kind::Ident { .. }) {
                self.bump();
            }
            if self.at_punct("(") {
                self.skip_balanced();
            }
            self.eat_punct("?");
            if self.eat_kw("as") {
                self.skip_type();
            }
            if self.eat_punct("=") {
                out.extend(self.parse_expr());
            }
            if !self.eat_punct(",") && !self.at_punct(")") {
                self.error_here();
                if self.pos == before {
                    self.bump();
                }
                while !self.at_punct(")") && !self.at_punct(",") && !self.at_eof() {
                    self.bump();
                }
                self.eat_punct(",");
            }
        }
        self.eat_punct(")");
    }

    // ---- skipping ----------------------------------------------------------------

    /// Skip attribute blocks `<A(…), B> <C>`.
    fn skip_attributes(&mut self) {
        self.pos = self.attributes_end(self.pos);
    }

    /// The index after the attribute blocks starting at token `i`.
    fn attributes_end(&self, mut i: usize) -> usize {
        while self.is_punct(self.tok(i), "<") {
            let mut depth = 0i32;
            loop {
                let t = self.tok(i);
                if matches!(t.kind, Kind::Eof) {
                    return i;
                }
                if self.is_punct(t, "(") {
                    i = self.balanced_end(i);
                    continue;
                }
                if self.is_punct(t, "<") {
                    depth += 1;
                } else if self.is_punct(t, ">") {
                    depth -= 1;
                }
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            if self.is_punct(self.tok(i), ",") {
                i += 1;
            }
        }
        i
    }

    /// Skip a balanced `( … )` / `{ … }` group starting at the cursor.
    fn skip_balanced(&mut self) {
        self.pos = self.balanced_end(self.pos);
    }

    /// The index after the balanced `( … )` / `{ … }` group at token `i`.
    fn balanced_end(&self, mut i: usize) -> usize {
        let mut depth = 0i32;
        loop {
            let t = self.tok(i);
            if matches!(t.kind, Kind::Eof) {
                return i;
            }
            if self.is_punct(t, "(") || self.is_punct(t, "{") {
                depth += 1;
            } else if self.is_punct(t, ")") || self.is_punct(t, "}") {
                depth -= 1;
            }
            i += 1;
            if depth <= 0 {
                return i;
            }
        }
    }

    /// Skip `(Of T, …)` type arguments / parameters.
    fn skip_type_args(&mut self) {
        if self.at_punct("(") && self.is_kw(self.peek(1), "of") {
            self.skip_balanced();
        }
    }

    /// Skip a type: `Integer`, `A.B(Of C)()`, `T?`, `(Integer, String)`.
    fn skip_type(&mut self) {
        if self.at_punct("(") {
            self.skip_balanced();
        } else {
            loop {
                if matches!(self.cur().kind, Kind::Ident { .. }) {
                    self.bump();
                }
                self.skip_type_args();
                if self.at_punct(".") {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        loop {
            if self.at_punct("(") && !self.line_breaks_here() {
                self.skip_balanced();
            } else if self.at_punct("?") && !self.line_breaks_here() {
                self.bump();
            } else {
                break;
            }
        }
    }

    // ---- expressions ---------------------------------------------------------------

    pub(crate) fn parse_expr(&mut self) -> Vec<Node> {
        self.parse_xor().into_nodes()
    }

    fn parse_expr_list(&mut self) -> Vec<Node> {
        let mut out = self.parse_expr();
        while self.eat_punct(",") {
            out.extend(self.parse_expr());
        }
        out
    }

    /// A binary operator keyword / punctuation is here and continues the
    /// expression (an operator on the next line does not).
    fn op_here(&self, ops: &[&str]) -> bool {
        if self.line_breaks_here() {
            return false;
        }
        let t = self.cur();
        match t.kind {
            Kind::Punct => ops.contains(&self.text(t)),
            _ => self.kw_in(t, ops),
        }
    }

    /// A run of transparent binary operators `ops` over `next` operands.
    fn parse_transparent_run(&mut self, ops: &[&str], next: fn(&mut Self) -> Expr) -> Expr {
        let first = next(self);
        if !self.op_here(ops) {
            return first;
        }
        let mut nodes = first.into_nodes();
        while self.op_here(ops) {
            self.bump();
            nodes.extend(next(self).into_nodes());
        }
        Expr::Nodes(nodes)
    }

    fn parse_xor(&mut self) -> Expr {
        self.parse_transparent_run(&["xor"], |p| p.parse_logical(LogicalOp::Or))
    }

    /// A run of `Or`/`OrElse` (or `And`/`AndAlso`). The short-circuit and the
    /// eager forms share a precedence level and fold into one run.
    fn parse_logical(&mut self, op: LogicalOp) -> Expr {
        let (ops, next): (&[&str], fn(&mut Self) -> Expr) = match op {
            LogicalOp::Or => (&["or", "orelse"], |p| p.parse_logical(LogicalOp::And)),
            _ => (&["and", "andalso"], Self::parse_not),
        };
        let first = next(self);
        if !self.op_here(ops) {
            return first;
        }
        let mut operands = Vec::new();
        first.fold_into(op, &mut operands);
        while self.op_here(ops) {
            self.bump();
            next(self).fold_into(op, &mut operands);
        }
        Expr::Logical(op, operands)
    }

    /// `Not` is transparent, like `!`.
    fn parse_not(&mut self) -> Expr {
        if self.at_kw("not") {
            self.bump();
            return Expr::Nodes(self.parse_not().into_nodes());
        }
        self.parse_binary()
    }

    fn parse_binary(&mut self) -> Expr {
        self.parse_transparent_run(BINARY_OPS, Self::parse_unary)
    }

    fn parse_unary(&mut self) -> Expr {
        if self.at_punct("-")
            || self.at_punct("+")
            || self.kw_in(self.cur(), &["addressof", "await", "typeof", "not"])
        {
            self.bump();
            return Expr::Nodes(self.parse_unary().into_nodes());
        }
        if self.at_kw("new") {
            return Expr::Nodes(self.parse_new());
        }
        self.parse_postfix()
    }

    /// `New T(args) [With {…} | From {…} | {…}]`, `New With {…}`.
    fn parse_new(&mut self) -> Vec<Node> {
        self.bump();
        let mut out = Vec::new();
        if !self.at_kw("with") {
            self.skip_attributes();
            self.skip_type_name();
            while self.at_punct("(") && !self.line_breaks_here() {
                out.extend(self.parse_args());
            }
            if self.at_punct("{") && !self.line_breaks_here() {
                out.extend(self.parse_braced());
            }
        }
        if !self.line_breaks_here() && (self.eat_kw("with") || self.eat_kw("from")) {
            out.extend(self.parse_braced());
        }
        let mut chain = Expr::Nodes(out);
        self.parse_postfix_ops(&mut chain, None, false);
        chain.into_nodes()
    }

    /// A qualified type name with type arguments (no array suffix: `(…)`
    /// after `New T` is the constructor argument list).
    fn skip_type_name(&mut self) {
        loop {
            if matches!(self.cur().kind, Kind::Ident { .. }) {
                self.bump();
            }
            self.skip_type_args();
            if self.at_punct(".") && !self.line_breaks_here() {
                self.bump();
            } else {
                break;
            }
        }
        self.eat_punct("?");
    }

    /// A primary expression followed by its member accesses, invocations, and
    /// null-conditional accesses.
    fn parse_postfix(&mut self) -> Expr {
        let t = self.cur();
        let mut name: Option<&'a Token> = None;
        let mut mybase = false;
        let mut base = match &t.kind {
            Kind::Literal | Kind::Xml => {
                self.bump();
                Expr::empty()
            }
            Kind::Interp(holes) => {
                self.bump();
                let mut out = Vec::new();
                for hole in holes {
                    let (nodes, errors) = self.sub_expr(hole);
                    out.extend(nodes);
                    self.errors.extend(errors);
                }
                Expr::Nodes(out)
            }
            Kind::Ident { .. } => match t.kw {
                Some("if") if self.is_punct(self.peek(1), "(") => {
                    Expr::Nodes(vec![self.parse_if_operator()])
                }
                Some("sub" | "function") => Expr::Nodes(self.parse_lambda()),
                Some("async" | "iterator")
                    if self.kw_in(self.peek(1), &["sub", "function", "async", "iterator"]) =>
                {
                    Expr::Nodes(self.parse_lambda())
                }
                Some("from" | "aggregate") if self.query_follows() => {
                    Expr::Nodes(self.parse_query())
                }
                Some("mybase") => {
                    mybase = true;
                    self.bump();
                    Expr::empty()
                }
                _ => {
                    name = Some(t);
                    self.bump();
                    Expr::empty()
                }
            },
            Kind::Punct => match self.text(t) {
                "(" => self.parse_parenthesized(),
                "{" => Expr::Nodes(self.parse_braced()),
                // With-block member access `.Name`, dictionary access `!key`.
                "." | "!" | "?." => {
                    let mut chain = Expr::empty();
                    self.member_access(&mut chain, &mut name);
                    chain
                }
                _ => return Expr::empty(),
            },
            _ => return Expr::empty(),
        };
        self.parse_postfix_ops(&mut base, name, mybase);
        base
    }

    /// Apply postfix operators to `chain`. `name` is the simple name the next
    /// invocation would call; `mybase` marks a `MyBase.M()` call (never
    /// recursion).
    fn parse_postfix_ops(&mut self, chain: &mut Expr, mut name: Option<&'a Token>, mybase: bool) {
        loop {
            if self.op_here(&[".", "!", "?."]) {
                self.member_access(chain, &mut name);
                if mybase {
                    name = None;
                }
            } else if self.op_here(&["?"]) {
                self.bump();
                // `a?(i)` / `a?!key`; a lone `?` is a nullable-type marker.
                if self.at_punct("(") || self.at_punct("!") {
                    push_nodes(chain, vec![Node::NullGuard { body: Vec::new() }]);
                }
            } else if self.op_here(&["("]) {
                if self.is_kw(self.peek(1), "of") {
                    self.skip_balanced();
                    continue;
                }
                let args = self.parse_args();
                let mut nodes = vec![self.call(name.take())];
                nodes.extend(args);
                push_nodes(chain, nodes);
            } else {
                return;
            }
        }
    }

    /// `.name`, `?.name`, `!key`, and the XML axes `.@attr`, `.<elem>`,
    /// `...<elem>`.
    fn member_access(&mut self, chain: &mut Expr, name: &mut Option<&'a Token>) {
        if self.at_punct("?.") {
            push_nodes(chain, vec![Node::NullGuard { body: Vec::new() }]);
        }
        self.bump();
        while self.at_punct(".") {
            self.bump();
        }
        if self.eat_punct("@") {
            if self.at_punct("<") {
                self.skip_angle();
            } else {
                self.bump();
                self.xml_qualified_name();
            }
            *name = None;
        } else if self.at_punct("<") {
            self.skip_angle();
            *name = None;
        } else if matches!(self.cur().kind, Kind::Ident { .. }) {
            *name = Some(self.cur());
            self.bump();
        } else {
            *name = None;
        }
        // A logical run stops being foldable once a member is applied.
        if let Expr::Logical(..) = chain {
            push_nodes(chain, Vec::new());
        }
    }

    /// The `:local` part of an XML name `ns:local` written without spaces.
    fn xml_qualified_name(&mut self) {
        let prev_end = self.tok(self.pos.saturating_sub(1)).end;
        if self.at_punct(":") && self.cur().start == prev_end {
            self.bump();
            self.bump();
        }
    }

    fn skip_angle(&mut self) {
        while !self.at_eof() && !self.at_punct(">") && !self.cur().nl_before {
            self.bump();
        }
        self.eat_punct(">");
    }

    /// `( expr )` — kept foldable when it is a lone logical run — or a tuple.
    fn parse_parenthesized(&mut self) -> Expr {
        self.bump();
        self.bracketed(|p| {
            p.skip_named_arg();
            let first = p.parse_xor();
            let result = if p.at_punct(",") {
                let mut nodes = first.into_nodes();
                while p.eat_punct(",") {
                    if p.at_punct(")") {
                        break;
                    }
                    p.skip_named_arg();
                    nodes.extend(p.parse_expr());
                }
                Expr::Nodes(nodes)
            } else {
                first
            };
            p.close(")");
            result
        })
    }

    /// Expect a closing token; on garbage, skip to it (within reason).
    fn close(&mut self, closer: &str) {
        if self.eat_punct(closer) {
            return;
        }
        self.error_here();
        let opener = if closer == ")" { "(" } else { "{" };
        let mut depth = 0;
        while !self.at_eof() {
            if self.at_punct(opener) {
                depth += 1;
            } else if self.at_punct(closer) {
                if depth == 0 {
                    self.bump();
                    return;
                }
                depth -= 1;
            } else if self.cur().nl_before && self.nested <= 1 && self.terminator().is_some() {
                return;
            }
            self.bump();
        }
    }

    /// An argument list `(a, , name:=b, 0 To n)`.
    fn parse_args(&mut self) -> Vec<Node> {
        self.bump();
        self.parse_list(")", |p| {
            p.skip_named_arg();
            let mut out = p.parse_expr();
            if p.eat_kw("to") {
                out.extend(p.parse_expr());
            }
            out
        })
    }

    /// A braced list `{a, {b}, .X = c, Key .Y = d}` (array literal, collection
    /// or object initializer).
    fn parse_braced(&mut self) -> Vec<Node> {
        if !self.eat_punct("{") {
            return Vec::new();
        }
        self.parse_list("}", |p| {
            p.eat_kw("key");
            p.parse_expr()
        })
    }

    /// The comma-separated items of a bracketed list up to `closer` (the
    /// opener already consumed); empty items are allowed.
    fn parse_list(
        &mut self,
        closer: &str,
        mut item: impl FnMut(&mut Self) -> Vec<Node>,
    ) -> Vec<Node> {
        self.bracketed(|p| {
            let mut out = Vec::new();
            loop {
                if p.at_punct(closer) || p.at_eof() {
                    break;
                }
                if p.eat_punct(",") {
                    continue;
                }
                if matches!(p.cur().kind, Kind::Directive(_)) {
                    p.skip_inline_directives(&mut out);
                    continue;
                }
                let before = p.pos;
                out.extend(item(p));
                p.skip_inline_directives(&mut out);
                if p.pos == before || !(p.at_punct(",") || p.at_punct(closer)) {
                    break;
                }
            }
            p.close(closer);
            out
        })
    }

    /// `If(c, a, b)` is a ternary; `If(a, b)` is null coalescing.
    fn parse_if_operator(&mut self) -> Node {
        self.bump();
        self.bump();
        let (first, mut rest) = self.bracketed(|p| {
            let first = p.parse_expr();
            let mut rest = Vec::new();
            while p.eat_punct(",") {
                rest.push(p.parse_expr());
            }
            p.close(")");
            (first, rest)
        });
        match rest.len() {
            2 => {
                let alternate = rest.pop().unwrap_or_default();
                let then = rest.pop().unwrap_or_default();
                Node::Conditional {
                    test: first,
                    then,
                    alternate,
                }
            }
            _ => {
                let mut operands = vec![Node::Group(first)];
                operands.extend(rest.into_iter().map(Node::Group));
                Node::Logical {
                    op: LogicalOp::Coalesce,
                    operands,
                }
            }
        }
    }

    /// `[Async] Sub(…) stmt`, `Function(…) expr`, or the multi-line forms
    /// ending in `End Sub` / `End Function`. Each is a unit.
    fn parse_lambda(&mut self) -> Vec<Node> {
        let line = self.cur().line;
        while self.at_kw("async") || self.at_kw("iterator") {
            self.bump();
        }
        let is_function = self.at_kw("function");
        self.bump();
        let mut body = Vec::new();
        if self.at_punct("(") {
            body.extend(self.parse_params());
        }
        if self.eat_kw("as") {
            self.skip_type();
        }
        let multi_line = self.cur().nl_before || is_hard_end(self.cur());
        body.extend(self.in_unit("<lambda>", |p| {
            if multi_line {
                p.with_nested(0, |p| p.close_block(Block::Lambda(is_function)))
            } else if is_function {
                p.parse_expr()
            } else {
                let mut out = Vec::new();
                p.parse_statement(&mut out);
                out
            }
        }));
        vec![Node::Function {
            name: "<lambda>".to_string(),
            kind: "lambda".to_string(),
            line,
            body,
        }]
    }

    /// `From x In xs` / `Aggregate x In xs` starts a query (rather than an
    /// identifier named `From`).
    fn query_follows(&self) -> bool {
        matches!(self.peek(1).kind, Kind::Ident { .. }) && (self.kw_in(self.peek(2), &["in", "as"]))
    }

    /// A LINQ query: clause keywords separating expressions. A clause keyword
    /// at the start of the next line continues the query.
    fn parse_query(&mut self) -> Vec<Node> {
        let mut out = Vec::new();
        // The previous token was a query keyword or `,`: a line break here is
        // an implicit continuation.
        let mut continues = true;
        let mut last_kw: Option<&'static str> = None;
        loop {
            if self.at_eof() {
                break;
            }
            let t = self.cur();
            let breaks = !continues && self.line_breaks_here();
            // `Order` / `Group` are clause keywords only as `Order By`,
            // `Group … By`, `Group Join`; elsewhere they are names (`Select
            // order`, `Into Group`).
            let name_like = (self.is_kw(t, "order") && !self.is_kw(self.peek(1), "by"))
                || (self.is_kw(t, "group") && self.peek(1).nl_before);
            if self.kw_in(t, QUERY_WORDS) && !breaks && !name_like {
                // `Into Group` names the group (a value), and `Distinct` /
                // `Ascending` / `Descending` take no operand: a line break
                // after them ends the query.
                let value = self.is_kw(t, "group") && last_kw == Some("into");
                continues = !value && !self.kw_in(t, &["distinct", "ascending", "descending"]);
                last_kw = t.kw;
                self.bump();
                continue;
            }
            if self.kw_in(t, QUERY_CLAUSES)
                && t.nl_before
                && !(self.is_kw(t, "select") && self.is_kw(self.peek(1), "case"))
                && !(self.is_kw(t, "on") && self.is_kw(self.peek(1), "error"))
            {
                self.bump();
                continue;
            }
            if breaks || matches!(t.kind, Kind::Directive(_)) {
                break;
            }
            if self.eat_punct(",") {
                continues = true;
                continue;
            }
            continues = false;
            last_kw = None;
            if matches!(t.kind, Kind::Ident { .. }) && self.is_kw(self.peek(1), "as") {
                self.bump();
                self.bump();
                self.skip_type();
                continue;
            }
            let before = self.pos;
            let stmt_start = std::mem::replace(&mut self.stmt_start, self.pos);
            out.extend(self.parse_expr());
            self.stmt_start = stmt_start;
            if self.pos == before {
                break;
            }
        }
        out
    }
}

/// The input or a directive line ends any statement.
fn is_hard_end(t: &Token) -> bool {
    matches!(t.kind, Kind::Eof | Kind::Directive(_))
}

/// Append lowered nodes to an expression chain (which stops being a foldable
/// logical run once anything is applied to it).
fn push_nodes(chain: &mut Expr, nodes: Vec<Node>) {
    let taken = std::mem::replace(chain, Expr::empty());
    let mut all = taken.into_nodes();
    all.extend(nodes);
    *chain = Expr::Nodes(all);
}

/// Build an `If` / `ElseIf` … / `Else` chain: each `ElseIf` is a nested
/// [`Node::Branch`] in the previous arm's `alternate` (so it scores flat).
fn branch_chain(arms: Vec<(Vec<Node>, Vec<Node>)>, else_body: Option<Vec<Node>>) -> Node {
    let mut acc = else_body.map(Node::Group);
    for (test, then) in arms.into_iter().rev() {
        acc = Some(Node::Branch {
            test,
            then,
            alternate: acc.map(Box::new),
        });
    }
    acc.unwrap_or(Node::Group(Vec::new()))
}
