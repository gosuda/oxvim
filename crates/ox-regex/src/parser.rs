use crate::{CompileError, Magic};

#[derive(Clone, Debug)]
pub(crate) enum Expr {
    Empty,
    Literal(char),
    Any {
        newline: bool,
    },
    Class(CharClass),
    Concat(Vec<Expr>),
    Alt(Vec<Expr>),
    And(Vec<Expr>),
    Repeat {
        expr: Box<Expr>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    Group {
        index: Option<usize>,
        expr: Box<Expr>,
    },
    OptionalSeq(Vec<Expr>),
    Anchor(Anchor),
    Look {
        expr: Box<Expr>,
        kind: LookKind,
        limit: Option<usize>,
    },
    Backref(usize),
    SetStart,
    SetEnd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LookKind {
    Ahead,
    NotAhead,
    Behind,
    NotBehind,
    Atomic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Compare {
    Equal,
    Less,
    Greater,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Anchor {
    LineStart,
    LineEnd,
    FileStart,
    FileEnd,
    WordStart,
    WordEnd,
    Line(Compare, usize),
    Column(Compare, usize),
    VirtualColumn(Compare, usize),
    Visual,
    Cursor,
    Mark(char),
}

#[derive(Clone, Debug)]
pub(crate) struct CharClass {
    pub(crate) negated: bool,
    pub(crate) include_newline: bool,
    pub(crate) items: Vec<ClassItem>,
}

#[derive(Clone, Debug)]
pub(crate) enum ClassItem {
    Char(char),
    Range(char, char),
    Kind(ClassKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClassKind {
    Alnum,
    Alpha,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
    Word,
    Head,
    Octal,
    Hex,
    Ident,
    IdentNoDigit,
    Keyword,
    KeywordNoDigit,
    File,
    FileNoDigit,
    PrintNoDigit,
}

#[derive(Clone, Copy, Debug, Default)]
// Independent feature detectors, not a state machine: each records a
// different upstream engine-capability observed while parsing, and callers
// combine them with `||` at engine selection.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Features {
    pub(crate) backref: bool,
    pub(crate) lookbehind: bool,
    pub(crate) lookaround: bool,
    pub(crate) complex_repeat: bool,
}

pub(crate) struct Parsed {
    pub(crate) expr: Expr,
    pub(crate) captures: usize,
    pub(crate) ignore_case: bool,
    pub(crate) features: Features,
}

#[derive(Clone, Copy)]
enum Terminator {
    Group,
}

pub(crate) struct Parser<'a> {
    pattern: &'a str,
    pos: usize,
    mode: Magic,
    /// Mirror of upstream `RE_STRICT`: an unclosed `[` collection is an
    /// error instead of a literal `[`. Only strict consumers (`vim.regex()`,
    /// spell files, `'indentexpr'`) set this; the editor compiles
    /// non-strict and treats `[` without a matching `]` as plain text.
    strict: bool,
    captures: usize,
    closed_captures: usize,
    ignore_case: bool,
    features: Features,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(pattern: &'a str, mode: Magic) -> Self {
        Self {
            pattern,
            pos: 0,
            mode,
            strict: false,
            captures: 0,
            closed_captures: 0,
            ignore_case: false,
            features: Features::default(),
        }
    }

    /// Non-strict [`Parser::new`] plus upstream `RE_STRICT` handling: an
    /// unclosed `[` collection fails to compile with `E769` instead of
    /// becoming a literal `[`.
    pub(crate) fn new_strict(pattern: &'a str, mode: Magic) -> Self {
        let mut parser = Self::new(pattern, mode);
        parser.strict = true;
        parser
    }

    pub(crate) fn parse(mut self) -> Result<Parsed, CompileError> {
        let expr = self.parse_pattern(None)?;
        if self.pos != self.pattern.len() {
            return Err(self.error("unexpected pattern terminator"));
        }
        Ok(Parsed {
            expr,
            captures: self.captures,
            ignore_case: self.ignore_case,
            features: self.features,
        })
    }

    fn parse_pattern(&mut self, terminator: Option<Terminator>) -> Result<Expr, CompileError> {
        let mut branches = vec![self.parse_branch(terminator)?];
        while self.consume_operator('|') {
            branches.push(self.parse_branch(terminator)?);
        }
        Ok(if branches.len() == 1 {
            match branches.pop() {
                Some(branch) => branch,
                None => Expr::Empty,
            }
        } else {
            Expr::Alt(branches)
        })
    }

    fn parse_branch(&mut self, terminator: Option<Terminator>) -> Result<Expr, CompileError> {
        let mut conjunctions = vec![self.parse_concat(terminator)?];
        while self.consume_escaped('&') {
            conjunctions.push(self.parse_concat(terminator)?);
        }
        Ok(if conjunctions.len() == 1 {
            match conjunctions.pop() {
                Some(concat) => concat,
                None => Expr::Empty,
            }
        } else {
            Expr::And(conjunctions)
        })
    }

    fn parse_concat(&mut self, terminator: Option<Terminator>) -> Result<Expr, CompileError> {
        let mut pieces = Vec::new();
        while !self.at_end()
            && !self.at_operator('|')
            && !self.at_escaped('&')
            && !terminator.is_some_and(|end| self.at_terminator(end))
        {
            pieces.push(self.parse_piece()?);
        }
        Ok(match pieces.len() {
            0 => Expr::Empty,
            1 => match pieces.pop() {
                Some(piece) => piece,
                None => Expr::Empty,
            },
            _ => Expr::Concat(pieces),
        })
    }

    fn parse_piece(&mut self) -> Result<Expr, CompileError> {
        let mut atom = self.parse_atom()?;
        if let Some((min, max, greedy)) = self.parse_multiplier()? {
            if matches!(atom, Expr::SetStart | Expr::SetEnd) {
                return Err(self.error("\\zs and \\ze cannot be followed by a multiplier"));
            }
            atom = Expr::Repeat {
                expr: Box::new(atom),
                min,
                max,
                greedy,
            };
        }
        if let Some((kind, limit)) = self.parse_look_suffix()? {
            self.features.lookaround = true;
            if matches!(kind, LookKind::Behind | LookKind::NotBehind) {
                self.features.lookbehind = true;
            }
            atom = Expr::Look {
                expr: Box::new(atom),
                kind,
                limit,
            };
        }
        Ok(atom)
    }

    // One linear per-escape dispatch mirroring regatom.c's atom switch; the
    // arm order is upstream's, so splitting it would fragment the mirror.
    #[allow(clippy::too_many_lines)]
    fn parse_atom(&mut self) -> Result<Expr, CompileError> {
        let start = self.pos;
        if self.starts_with("\\@") {
            return Err(self.error("lookaround suffix follows nothing"));
        }
        if self.consume_mode_switch() {
            return Ok(Expr::Empty);
        }
        if self.consume_escaped('c') {
            self.ignore_case = true;
            return Ok(Expr::Empty);
        }
        if self.consume_escaped('C') {
            self.ignore_case = false;
            return Ok(Expr::Empty);
        }
        if self.starts_with("\\zs") {
            self.pos += 3;
            return Ok(Expr::SetStart);
        }
        if self.starts_with("\\ze") {
            self.pos += 3;
            return Ok(Expr::SetEnd);
        }
        if self.at_percent("[") {
            self.consume_percent("[");
            let mut atoms = Vec::new();
            while !self.at_end() && !self.starts_with("]") {
                atoms.push(self.parse_piece()?);
            }
            if !self.consume_str("]") {
                return Err(self.error("unclosed optional atom sequence"));
            }
            return Ok(Expr::OptionalSeq(atoms));
        }
        if self.at_percent("(") {
            self.consume_percent("(");
            let expr = self.parse_pattern(Some(Terminator::Group))?;
            self.expect_group_close("unclosed non-capturing group")?;
            return Ok(Expr::Group {
                index: None,
                expr: Box::new(expr),
            });
        }
        if self.at_group_open() {
            self.consume_group_open();
            self.captures += 1;
            if self.captures > 9 {
                return Err(self.error("at most nine capture groups are supported"));
            }
            let index = self.captures;
            let expr = self.parse_pattern(Some(Terminator::Group))?;
            // regexp.c `e_unmatched_str_open` interpolates the opener: `\(` in
            // magic modes, bare `(` in very magic mode.
            let unmatched = if self.mode == Magic::VeryMagic {
                "Unmatched ("
            } else {
                "Unmatched \\("
            };
            self.expect_group_close(unmatched)?;
            self.closed_captures = self.closed_captures.max(index);
            return Ok(Expr::Group {
                index: Some(index),
                expr: Box::new(expr),
            });
        }
        if self.at_percent("^") {
            self.consume_percent("^");
            return Ok(Expr::Anchor(Anchor::FileStart));
        }
        if self.at_percent("$") {
            self.consume_percent("$");
            return Ok(Expr::Anchor(Anchor::FileEnd));
        }
        if self.at_percent("V") {
            self.consume_percent("V");
            return Ok(Expr::Anchor(Anchor::Visual));
        }
        if self.at_percent("#") {
            self.consume_percent("#");
            return Ok(Expr::Anchor(Anchor::Cursor));
        }
        if self.at_percent("'") {
            self.consume_percent("'");
            let mark = self
                .next_char()
                .ok_or_else(|| self.error("missing mark name"))?;
            return Ok(Expr::Anchor(Anchor::Mark(mark)));
        }
        if let Some(literal) = self.parse_numeric_character()? {
            return Ok(Expr::Literal(literal));
        }
        if self.at_percent("")
            && let Some(anchor) = self.parse_position_anchor()?
        {
            return Ok(Expr::Anchor(anchor));
        }
        if self.consume_escaped('<') {
            return Ok(Expr::Anchor(Anchor::WordStart));
        }
        if self.consume_escaped('>') {
            return Ok(Expr::Anchor(Anchor::WordEnd));
        }
        if self.starts_with("\\_^") {
            self.pos += 3;
            return Ok(Expr::Anchor(Anchor::LineStart));
        }
        if self.starts_with("\\_$") {
            self.pos += 3;
            return Ok(Expr::Anchor(Anchor::LineEnd));
        }
        if self.starts_with("\\_.") {
            self.pos += 3;
            return Ok(Expr::Any { newline: true });
        }
        if self.starts_with("\\_[") {
            self.pos += 3;
            return self.parse_multiline_collection();
        }
        if self.starts_with("\\_") {
            self.pos += 2;
            let code = self
                .next_char()
                .ok_or_else(|| self.error("missing newline class"))?;
            let (kind, negated) = class_escape(code).ok_or(CompileError::InvalidEscape {
                offset: start,
                escape: code,
            })?;
            return Ok(Expr::Class(CharClass {
                negated,
                include_newline: true,
                items: vec![ClassItem::Kind(kind)],
            }));
        }
        if self.at_collection_open() {
            self.consume_collection_open();
            return self.parse_collection();
        }
        if self.at_dot() {
            self.consume_dot();
            return Ok(Expr::Any { newline: false });
        }
        if self.at_anchor_start() {
            self.pos += if self.mode == Magic::VeryNoMagic {
                2
            } else {
                1
            };
            return Ok(Expr::Anchor(Anchor::LineStart));
        }
        if self.at_anchor_end() {
            self.pos += if self.mode == Magic::VeryNoMagic {
                2
            } else {
                1
            };
            return Ok(Expr::Anchor(Anchor::LineEnd));
        }
        if self.starts_with("\\") {
            self.pos += 1;
            let escaped = self
                .next_char()
                .ok_or_else(|| self.error("trailing backslash"))?;
            if ('1'..='9').contains(&escaped) {
                let index = usize::from(escaped as u8 - b'0');
                if index > self.closed_captures {
                    return Err(self.error("backreference refers to a group not yet closed"));
                }
                self.features.backref = true;
                return Ok(Expr::Backref(index));
            }
            if let Some((kind, negated)) = class_escape(escaped) {
                return Ok(Expr::Class(CharClass {
                    negated,
                    include_newline: false,
                    items: vec![ClassItem::Kind(kind)],
                }));
            }
            let literal = match escaped {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'e' => '\u{1b}',
                'b' => '\u{8}',
                other => other,
            };
            return Ok(Expr::Literal(literal));
        }
        self.next_char()
            .map(Expr::Literal)
            .ok_or_else(|| self.error("expected atom"))
    }

    fn parse_position_anchor(&mut self) -> Result<Option<Anchor>, CompileError> {
        let saved = self.pos;
        self.pos += self.percent_prefix_len();
        let compare = if self.consume_str("<") {
            Compare::Less
        } else if self.consume_str(">") {
            Compare::Greater
        } else {
            Compare::Equal
        };
        let number_start = self.pos;
        while self.peek_char().is_some_and(|c| c.is_ascii_digit()) {
            self.next_char();
        }
        if number_start == self.pos {
            self.pos = saved;
            return Ok(None);
        }
        let number = self.pattern[number_start..self.pos]
            .parse::<usize>()
            .map_err(|_| self.error("position number is too large"))?;
        let kind = self.next_char();
        let anchor = match kind {
            Some('l') => Anchor::Line(compare, number),
            Some('c') => Anchor::Column(compare, number),
            Some('v') => Anchor::VirtualColumn(compare, number),
            _ => {
                self.pos = saved;
                return Ok(None);
            }
        };
        Ok(Some(anchor))
    }

    fn parse_numeric_character(&mut self) -> Result<Option<char>, CompileError> {
        if !self.at_percent("") {
            return Ok(None);
        }
        let prefix_len = self.percent_prefix_len();
        let Some(code) = self.pattern[self.pos + prefix_len..].chars().next() else {
            return Ok(None);
        };
        let radix = match code {
            'd' => 10,
            'x' | 'u' | 'U' => 16,
            'o' => 8,
            _ => return Ok(None),
        };
        let saved = self.pos;
        self.pos += prefix_len + code.len_utf8();
        let digits_start = self.pos;
        while self.peek_char().is_some_and(|ch| ch.is_digit(radix)) {
            self.next_char();
        }
        if digits_start == self.pos {
            self.pos = saved;
            return Err(self.error("numeric character atom requires digits"));
        }
        let value = u32::from_str_radix(&self.pattern[digits_start..self.pos], radix)
            .map_err(|_| self.error("numeric character atom is too large"))?;
        char::from_u32(value)
            .map(Some)
            .ok_or_else(|| self.error("numeric character atom is not a Unicode scalar"))
    }

    /// Parses a `[...]` collection (regexp.c `nfa_regatom` `collection:`).
    fn parse_collection(&mut self) -> Result<Expr, CompileError> {
        self.parse_collection_impl(false)
    }

    /// Parses a `\_[...]` collection, which additionally matches a line break.
    /// An unclosed `\_[` stays a literal `[` with no newline attached.
    fn parse_multiline_collection(&mut self) -> Result<Expr, CompileError> {
        self.parse_collection_impl(true)
    }

    fn parse_collection_impl(&mut self, include_newline: bool) -> Result<Expr, CompileError> {
        let Some(end) = self.skip_collection_end(self.pos) else {
            if self.strict {
                return Err(CompileError::MissingBracket);
            }
            return Ok(Expr::Literal('['));
        };
        let mut items = Vec::new();
        let negated = self.consume_str("^");
        if self.consume_str("]") {
            items.push(ClassItem::Char(']'));
        }
        while self.pos < end {
            if let Some(kind) = self.take_posix_kind(end)? {
                items.push(ClassItem::Kind(kind));
                continue;
            }
            let first = self.parse_collection_char()?;
            if self.consume_str("-") {
                if self.starts_with("]") {
                    items.push(ClassItem::Char(first));
                    items.push(ClassItem::Char('-'));
                } else {
                    let last = self.parse_collection_char()?;
                    if first > last {
                        return Err(CompileError::ReversedRange);
                    }
                    items.push(ClassItem::Range(first, last));
                }
            } else {
                items.push(ClassItem::Char(first));
            }
        }
        self.pos = end + 1;
        Ok(Expr::Class(CharClass {
            negated,
            include_newline,
            items,
        }))
    }

    /// Parses a `[:name:]` item when the body starts with one before `end`.
    fn take_posix_kind(&mut self, end: usize) -> Result<Option<ClassKind>, CompileError> {
        if !self.starts_with("[:") {
            return Ok(None);
        }
        let class_start = self.pos + 2;
        if class_start > end {
            return Ok(None);
        }
        let search_end = (end + 1).min(self.pattern.len());
        let Some(relative_end) = self.pattern[class_start..search_end].find(":]") else {
            return Ok(None);
        };
        let name = &self.pattern[class_start..class_start + relative_end];
        let kind =
            posix_class(name).ok_or_else(|| self.error("unknown POSIX character class"))?;
        self.pos = class_start + relative_end + 2;
        Ok(Some(kind))
    }

    fn parse_collection_char(&mut self) -> Result<char, CompileError> {
        if self.consume_str("\\") {
            let escaped = self
                .next_char()
                .ok_or_else(|| self.error("trailing escape in collection"))?;
            return Ok(match escaped {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'e' => '\u{1b}',
                'b' => '\u{8}',
                other => other,
            });
        }
        self.next_char()
            .ok_or_else(|| self.error("missing collection character"))
    }

    /// Ports `skip_anyof()` (regexp.c:710).
    fn skip_collection_end(&self, mut pos: usize) -> Option<usize> {
        let bytes = self.pattern.as_bytes();
        if bytes.get(pos) == Some(&b'^') {
            pos += 1;
        }
        if matches!(bytes.get(pos), Some(b']' | b'-')) {
            pos += 1;
        }
        while pos < bytes.len() && bytes[pos] != b']' {
            let Some(ch) = self.pattern.get(pos..).and_then(|rest| rest.chars().next()) else {
                break;
            };
            if ch.len_utf8() > 1 {
                pos += ch.len_utf8();
                continue;
            }
            match ch {
                '-' => {
                    pos += 1;
                    if pos < bytes.len() && bytes[pos] != b']' {
                        pos += self
                            .pattern
                            .get(pos..)
                            .and_then(|rest| rest.chars().next())
                            .map_or(1, char::len_utf8);
                    }
                }
                '\\' => {
                    let skips_two = matches!(
                        bytes.get(pos + 1),
                        Some(b']' | b'^' | b'-' | b'n' | b'\\' | b'r' | b't' | b'e' | b'b' | b'd'
                        | b'o' | b'x' | b'u' | b'U')
                    );
                    pos += if skips_two { 2 } else { 1 };
                }
                '[' => {
                    let next = self
                        .class_name_end(pos)
                        .or_else(|| self.bracketed_class_end(pos, b'='))
                        .or_else(|| self.bracketed_class_end(pos, b'.'));
                    if let Some(next) = next {
                        pos = next;
                    } else if pos + 1 < bytes.len() {
                        pos += 1;
                    } else {
                        break;
                    }
                }
                _ => {
                    pos += 1;
                }
            }
        }
        (pos < bytes.len() && bytes[pos] == b']').then_some(pos)
    }

    /// End offset of the `[:name:]` class itself (regexp.c:459 scan rule).
    fn class_name_end(&self, pos: usize) -> Option<usize> {
        let body = self.pattern.get(pos..)?.strip_prefix("[:")?;
        POSIX_CLASS_NAMES
            .iter()
            .find(|name| {
                body.strip_prefix(**name)
                    .is_some_and(|tail| tail.starts_with(":]"))
            })
            .map(|name| pos + "[:".len() + name.len() + ":]".len())
    }

    /// Ports `get_equi_class()` / `get_coll_element()` (regexp.c:662,683):
    /// recognizes `[=c=]` / `[.c.]` at `pos`, returning the single character
    /// and the index just past the closing `]`.
    fn take_bracketed_class(&self, pos: usize, delimiter: u8) -> Option<(char, usize)> {
        let rest = self.pattern.get(pos..)?;
        let prefix = if delimiter == b'=' { "[=" } else { "[." };
        let rest = rest.strip_prefix(prefix)?;
        let ch = rest.chars().next()?;
        let after = pos + prefix.len() + ch.len_utf8();
        let tail = self.pattern.get(after..)?;
        let closing = if delimiter == b'=' { "=]" } else { ".]" };
        tail.starts_with(closing)
            .then(|| (ch, after + closing.len()))
    }

    /// End offset of a `[[=c=]]` / `[[.c.]]` opener for the scan, mirroring
    /// the shared `posix` advance rule in `skip_anyof`.
    fn bracketed_class_end(&self, pos: usize, delimiter: u8) -> Option<usize> {
        self.take_bracketed_class(pos, delimiter)
            .map(|(_, next)| next)
    }

    fn parse_multiplier(&mut self) -> Result<Option<(usize, Option<usize>, bool)>, CompileError> {
        if self.consume_operator('*') {
            return Ok(Some((0, None, true)));
        }
        if self.consume_operator('+') {
            return Ok(Some((1, None, true)));
        }
        if self.consume_operator('?') || self.consume_operator('=') {
            return Ok(Some((0, Some(1), true)));
        }
        let brace = match self.mode {
            Magic::VeryMagic => self.consume_str("{"),
            _ => self.consume_str("\\{"),
        };
        if !brace {
            return Ok(None);
        }
        let greedy = !self.consume_str("-");
        let min_start = self.pos;
        while self.peek_char().is_some_and(|c| c.is_ascii_digit()) {
            self.next_char();
        }
        let min_text = &self.pattern[min_start..self.pos];
        let comma = self.consume_str(",");
        let max_start = self.pos;
        while self.peek_char().is_some_and(|c| c.is_ascii_digit()) {
            self.next_char();
        }
        let max_text = &self.pattern[max_start..self.pos];
        if !self.consume_str("}") {
            return Err(self.error("unclosed multiplier"));
        }
        let min = if min_text.is_empty() {
            0
        } else {
            min_text
                .parse::<usize>()
                .map_err(|_| self.error("multiplier is too large"))?
        };
        let max = if comma {
            if max_text.is_empty() {
                None
            } else {
                Some(
                    max_text
                        .parse::<usize>()
                        .map_err(|_| self.error("multiplier is too large"))?,
                )
            }
        } else if min_text.is_empty() {
            None
        } else {
            Some(min)
        };
        if max.is_some_and(|value| value < min) {
            return Err(self.error("multiplier maximum is less than minimum"));
        }
        if max.is_some_and(|value| (value > 500 || value.saturating_sub(min) > 200) && min < 200) {
            self.features.complex_repeat = true;
        }
        Ok(Some((min, max, greedy)))
    }

    fn parse_look_suffix(&mut self) -> Result<Option<(LookKind, Option<usize>)>, CompileError> {
        let marker = if self.mode == Magic::VeryMagic {
            "@"
        } else {
            "\\@"
        };
        if !self.consume_str(marker) {
            return Ok(None);
        }
        let number_start = self.pos;
        while self.peek_char().is_some_and(|c| c.is_ascii_digit()) {
            self.next_char();
        }
        let limit = if number_start == self.pos {
            None
        } else {
            Some(
                self.pattern[number_start..self.pos]
                    .parse::<usize>()
                    .map_err(|_| self.error("lookbehind limit is too large"))?,
            )
        };
        let kind = if self.consume_str("<=") {
            LookKind::Behind
        } else if self.consume_str("<!") {
            LookKind::NotBehind
        } else if limit.is_none() && self.consume_str("=") {
            LookKind::Ahead
        } else if limit.is_none() && self.consume_str("!") {
            LookKind::NotAhead
        } else if limit.is_none() && self.consume_str(">") {
            LookKind::Atomic
        } else {
            return Err(self.error("invalid lookaround suffix"));
        };
        Ok(Some((kind, limit.filter(|value| *value != 0))))
    }

    fn consume_mode_switch(&mut self) -> bool {
        let mode = if self.starts_with("\\v") {
            Some(Magic::VeryMagic)
        } else if self.starts_with("\\m") {
            Some(Magic::Magic)
        } else if self.starts_with("\\M") {
            Some(Magic::NoMagic)
        } else if self.starts_with("\\V") {
            Some(Magic::VeryNoMagic)
        } else {
            None
        };
        if let Some(mode) = mode {
            self.pos += 2;
            self.mode = mode;
            true
        } else {
            false
        }
    }

    fn at_group_open(&self) -> bool {
        if self.mode == Magic::VeryMagic {
            self.starts_with("(")
        } else {
            self.starts_with("\\(")
        }
    }

    fn percent_prefix_len(&self) -> usize {
        if self.mode == Magic::VeryMagic { 1 } else { 2 }
    }

    fn at_percent(&self, suffix: &str) -> bool {
        let rest = &self.pattern[self.pos..];
        if self.mode == Magic::VeryMagic {
            rest.strip_prefix('%')
                .is_some_and(|tail| tail.starts_with(suffix))
        } else {
            rest.strip_prefix("\\%")
                .is_some_and(|tail| tail.starts_with(suffix))
        }
    }

    fn consume_percent(&mut self, suffix: &str) {
        self.pos += self.percent_prefix_len() + suffix.len();
    }

    fn at_terminator(&self, terminator: Terminator) -> bool {
        match terminator {
            Terminator::Group if self.mode == Magic::VeryMagic => self.starts_with(")"),
            Terminator::Group => self.starts_with("\\)"),
        }
    }

    fn expect_group_close(&mut self, message: &'static str) -> Result<(), CompileError> {
        let close = if self.mode == Magic::VeryMagic {
            ""
        } else {
            "\\"
        };
        if !close.is_empty() && !self.consume_str(close) {
            return Err(self.error(message));
        }
        if self.consume_str(")") {
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    fn consume_group_open(&mut self) {
        self.pos += if self.mode == Magic::VeryMagic { 1 } else { 2 };
    }

    fn at_collection_open(&self) -> bool {
        match self.mode {
            Magic::Magic | Magic::VeryMagic => self.starts_with("["),
            Magic::NoMagic | Magic::VeryNoMagic => self.starts_with("\\["),
        }
    }

    fn consume_collection_open(&mut self) {
        self.pos += if matches!(self.mode, Magic::Magic | Magic::VeryMagic) {
            1
        } else {
            2
        };
    }

    fn at_dot(&self) -> bool {
        match self.mode {
            Magic::Magic | Magic::VeryMagic => self.starts_with("."),
            Magic::NoMagic | Magic::VeryNoMagic => self.starts_with("\\."),
        }
    }

    fn consume_dot(&mut self) {
        self.pos += if matches!(self.mode, Magic::Magic | Magic::VeryMagic) {
            1
        } else {
            2
        };
    }

    fn at_anchor_start(&self) -> bool {
        if self.mode == Magic::VeryNoMagic {
            return self.starts_with("\\^");
        }
        if !self.starts_with("^") {
            return false;
        }
        let before = &self.pattern[..self.pos];
        self.pos == 0
            || before.ends_with("\\|")
            || before.ends_with('|')
            || before.ends_with("\\(")
            || before.ends_with('(')
            || before.ends_with("\\%(")
            || before.ends_with('\n')
    }

    fn at_anchor_end(&self) -> bool {
        if self.mode == Magic::VeryNoMagic {
            return self.starts_with("\\$");
        }
        if !self.starts_with("$") {
            return false;
        }
        let after = &self.pattern[self.pos + 1..];
        after.is_empty()
            || after.starts_with("\\|")
            || after.starts_with('|')
            || after.starts_with("\\)")
            || after.starts_with(')')
            || after.starts_with('\n')
    }

    fn at_operator(&self, op: char) -> bool {
        let escaped = format!("\\{op}");
        match (self.mode, op) {
            (Magic::VeryMagic, _) | (Magic::Magic, '*') => self.starts_with_char(op),
            (_, _) => self.starts_with(&escaped),
        }
    }

    fn consume_operator(&mut self, op: char) -> bool {
        if !self.at_operator(op) {
            return false;
        }
        self.pos += if self.mode == Magic::VeryMagic || (self.mode == Magic::Magic && op == '*') {
            1
        } else {
            2
        };
        true
    }

    fn at_escaped(&self, ch: char) -> bool {
        let escaped = format!("\\{ch}");
        self.starts_with(&escaped)
    }

    fn consume_escaped(&mut self, ch: char) -> bool {
        if self.at_escaped(ch) {
            self.pos += 2;
            true
        } else {
            false
        }
    }

    fn starts_with_char(&self, ch: char) -> bool {
        self.peek_char() == Some(ch)
    }

    fn starts_with(&self, text: &str) -> bool {
        self.pattern[self.pos..].starts_with(text)
    }

    fn consume_str(&mut self, text: &str) -> bool {
        if self.starts_with(text) {
            self.pos += text.len();
            true
        } else {
            false
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.pattern[self.pos..].chars().next()
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn at_end(&self) -> bool {
        self.pos == self.pattern.len()
    }

    fn error(&self, message: &'static str) -> CompileError {
        CompileError::Syntax {
            offset: self.pos,
            message,
        }
    }
}

fn class_escape(code: char) -> Option<(ClassKind, bool)> {
    let (kind, negated) = match code {
        'i' => (ClassKind::Ident, false),
        'I' => (ClassKind::IdentNoDigit, false),
        'k' => (ClassKind::Keyword, false),
        'K' => (ClassKind::KeywordNoDigit, false),
        'f' => (ClassKind::File, false),
        'F' => (ClassKind::FileNoDigit, false),
        'p' => (ClassKind::Print, false),
        'P' => (ClassKind::PrintNoDigit, false),
        's' => (ClassKind::Space, false),
        'S' => (ClassKind::Space, true),
        'd' => (ClassKind::Digit, false),
        'D' => (ClassKind::Digit, true),
        'x' => (ClassKind::Hex, false),
        'X' => (ClassKind::Hex, true),
        'o' => (ClassKind::Octal, false),
        'O' => (ClassKind::Octal, true),
        'w' => (ClassKind::Word, false),
        'W' => (ClassKind::Word, true),
        'h' => (ClassKind::Head, false),
        'H' => (ClassKind::Head, true),
        'a' => (ClassKind::Alpha, false),
        'A' => (ClassKind::Alpha, true),
        'l' => (ClassKind::Lower, false),
        'L' => (ClassKind::Lower, true),
        'u' => (ClassKind::Upper, false),
        'U' => (ClassKind::Upper, true),
        _ => return None,
    };
    Some((kind, negated))
}

const POSIX_CLASS_NAMES: &[&str] = &[
    "alnum", "alpha", "backspace", "blank", "cntrl", "digit", "escape", "fname", "graph", "ident",
    "keyword", "lower", "print", "punct", "return", "space", "tab", "upper", "xdigit",
];

fn posix_class(name: &str) -> Option<ClassKind> {
    Some(match name {
        "alnum" => ClassKind::Alnum,
        "alpha" => ClassKind::Alpha,
        "blank" => ClassKind::Blank,
        "cntrl" => ClassKind::Cntrl,
        "digit" => ClassKind::Digit,
        "graph" => ClassKind::Graph,
        "lower" => ClassKind::Lower,
        "print" => ClassKind::Print,
        "punct" => ClassKind::Punct,
        "space" => ClassKind::Space,
        "upper" => ClassKind::Upper,
        "xdigit" => ClassKind::Xdigit,
        "keyword" => ClassKind::Keyword,
        "ident" => ClassKind::Ident,
        "fname" => ClassKind::File,
        _ => return None,
    })
}
