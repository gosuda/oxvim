//! Byte-oriented lexer for legacy Vimscript expressions.
//!
//! The lexer never decodes source text as UTF-8.  Every [`Span`] is expressed
//! in byte offsets into the original input and string tokens retain arbitrary
//! bytes.

use crate::error::EvalError;

/// A half-open byte range in the expression source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Span {
    /// First byte belonging to the syntax item.
    pub start: usize,
    /// First byte after the syntax item.
    pub end: usize,
}

impl Span {
    /// Construct a half-open source span.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Return a span covering both input spans.
    #[must_use]
    pub const fn through(self, other: Self) -> Self {
        Self {
            start: self.start,
            end: other.end,
        }
    }
}

/// A comparison operator's explicit case-selection suffix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaseSensitivity {
    /// Respect the current `'ignorecase'` setting.
    Default,
    /// The `#` suffix: compare case-sensitively.
    MatchCase,
    /// The `?` suffix: compare case-insensitively.
    IgnoreCase,
}

/// One literal or expression segment in an interpolated string token.
#[derive(Clone, Debug, PartialEq)]
pub enum InterpolationPart {
    /// Decoded literal bytes.
    Literal(Vec<u8>),
    /// Raw source bytes of one embedded expression.
    Expression(Vec<u8>),
}

/// Tokens accepted by the legacy expression parser.
#[allow(missing_docs)]
#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    /// End of input.
    Eof,
    /// A signed 64-bit integer literal (the sign is a separate token).
    Integer(i64),
    /// An IEEE-754 floating-point literal.
    Float(f64),
    /// A decoded single- or double-quoted byte string.
    String(Vec<u8>),
    /// A `$"..."` or `$'...'` string split into literal and expression parts.
    Interpolated(Vec<InterpolationPart>),
    /// A decoded `0z` hexadecimal blob.
    Blob(Vec<u8>),
    /// An internal variable or function name.
    Identifier(Vec<u8>),
    /// `$NAME` without the leading dollar sign.
    Environment(Vec<u8>),
    /// `&name`, `&g:name`, or `&l:name`.
    Option {
        scope: Option<u8>,
        name: Vec<u8>,
    },
    /// `@r`; the payload is the register-name byte.
    Register(u8),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    DotDot,
    DotDotDot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    AndAnd,
    OrOr,
    Question,
    Coalesce,
    Arrow,
    HashLBrace,
    Equal(CaseSensitivity),
    NotEqual(CaseSensitivity),
    Greater(CaseSensitivity),
    GreaterEqual(CaseSensitivity),
    Less(CaseSensitivity),
    LessEqual(CaseSensitivity),
    Match(CaseSensitivity),
    NoMatch(CaseSensitivity),
    Is(CaseSensitivity),
    IsNot(CaseSensitivity),
}

/// One token and its exact source range.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// Token payload.
    pub kind: TokenKind,
    /// Half-open byte range in the original source.
    pub span: Span,
}

/// Byte-oriented Vimscript expression lexer.
pub struct Lexer<'a> {
    source: &'a [u8],
    offset: usize,
    at_line_start: bool,
}

impl<'a> Lexer<'a> {
    /// Create a lexer over an expression byte slice.
    #[must_use]
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            offset: 0,
            at_line_start: true,
        }
    }

    /// Tokenize the complete expression, including one trailing [`TokenKind::Eof`].
    ///
    /// # Errors
    ///
    /// Returns `E15: Invalid expression` when the source contains a byte
    /// that cannot start any token.
    pub fn tokenize(self) -> Result<Vec<Token>, EvalError> {
        match self.tokenize_tolerant() {
            (_, Some(error)) => Err(error),
            (tokens, None) => Ok(tokens),
        }
    }

    /// Tokenize as far as the bytes allow, reporting the first refusal separately.
    ///
    /// Upstream never lexes past the expression it is parsing: `eval0`
    /// (`eval.c:1234-1252`) parses one expression, stops, and reports
    /// `E488: Trailing characters: %s` for whatever is left. An eager lexer
    /// cannot answer that, because a trailing `\r` fails lexing before the
    /// parser has finished the expression in front of it, turning upstream's
    /// E488 into E15. So stop at the first byte that cannot start a token,
    /// keep the error, and hand back the tokens so far with an
    /// [`TokenKind::Eof`] sitting on the refused offset. The parser then
    /// decides which answer is upstream's: the expression completed before the
    /// refused byte, so the remainder is trailing garbage (E488), or the
    /// expression needed that byte, so the lexer's own error stands.
    #[must_use]
    pub fn tokenize_tolerant(mut self) -> (Vec<Token>, Option<EvalError>) {
        let mut tokens = Vec::new();
        loop {
            self.skip_layout();
            let start = self.offset;
            let kind = match self.next_kind() {
                Ok(kind) => kind,
                Err(error) => {
                    tokens.push(Token {
                        kind: TokenKind::Eof,
                        span: Span::new(start, start),
                    });
                    return (tokens, Some(error));
                }
            };
            let eof = matches!(kind, TokenKind::Eof);
            tokens.push(Token {
                kind,
                span: Span::new(start, self.offset),
            });
            if eof {
                return (tokens, None);
            }
            self.at_line_start = false;
        }
    }

    /// Lex one token at the current offset, which layout skipping already reached.
    fn next_kind(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        Ok(match self.peek(0) {
            None | Some(b'\n') => TokenKind::Eof,
            Some(b'0') if matches!(self.peek(1), Some(b'z' | b'Z')) => self.lex_blob()?,
            Some(b'0'..=b'9') => self.lex_number()?,
            Some(b'\'') => self.lex_single_string()?,
            Some(b'"') => self.lex_double_string()?,
            Some(b'$') if matches!(self.peek(1), Some(b'\'' | b'"')) => self.lex_interpolated()?,
            Some(b'$') => self.lex_environment()?,
            Some(b'&') if self.peek(1) == Some(b'&') => {
                self.offset += 2;
                TokenKind::AndAnd
            }
            Some(b'&') => self.lex_option()?,
            Some(b'@') => self.lex_register()?,
            Some(b'#') if self.peek(1) == Some(b'{') => {
                self.offset += 2;
                TokenKind::HashLBrace
            }
            Some(b'(') => self.one(TokenKind::LParen),
            Some(b')') => self.one(TokenKind::RParen),
            Some(b'[') => self.one(TokenKind::LBracket),
            Some(b']') => self.one(TokenKind::RBracket),
            Some(b'{') => self.one(TokenKind::LBrace),
            Some(b'}') => self.one(TokenKind::RBrace),
            Some(b',') => self.one(TokenKind::Comma),
            Some(b':') => self.one(TokenKind::Colon),
            Some(b'+') => self.one(TokenKind::Plus),
            Some(b'*') => self.one(TokenKind::Star),
            Some(b'/') => self.one(TokenKind::Slash),
            Some(b'%') => self.one(TokenKind::Percent),
            Some(b'-') if self.peek(1) == Some(b'>') => {
                self.offset += 2;
                TokenKind::Arrow
            }
            Some(b'-') => self.one(TokenKind::Minus),
            Some(b'.') if self.peek(1) == Some(b'.') && self.peek(2) == Some(b'.') => {
                self.offset += 3;
                TokenKind::DotDotDot
            }
            Some(b'.') if self.peek(1) == Some(b'.') => {
                self.offset += 2;
                TokenKind::DotDot
            }
            Some(b'.') => self.one(TokenKind::Dot),
            Some(b'?') if self.peek(1) == Some(b'?') => {
                self.offset += 2;
                TokenKind::Coalesce
            }
            Some(b'?') => self.one(TokenKind::Question),
            Some(b'|') if self.peek(1) == Some(b'|') => {
                self.offset += 2;
                TokenKind::OrOr
            }
            Some(b'=') if self.peek(1) == Some(b'=') => {
                self.offset += 2;
                TokenKind::Equal(self.lex_case_suffix())
            }
            Some(b'=') if self.peek(1) == Some(b'~') => {
                self.offset += 2;
                TokenKind::Match(self.lex_case_suffix())
            }
            Some(b'!') if self.peek(1) == Some(b'=') => {
                self.offset += 2;
                TokenKind::NotEqual(self.lex_case_suffix())
            }
            Some(b'!') if self.peek(1) == Some(b'~') => {
                self.offset += 2;
                TokenKind::NoMatch(self.lex_case_suffix())
            }
            Some(b'!') => self.one(TokenKind::Bang),
            Some(b'>') if self.peek(1) == Some(b'=') => {
                self.offset += 2;
                TokenKind::GreaterEqual(self.lex_case_suffix())
            }
            Some(b'>') => {
                self.offset += 1;
                TokenKind::Greater(self.lex_case_suffix())
            }
            Some(b'<') if self.peek(1) == Some(b'=') => {
                self.offset += 2;
                TokenKind::LessEqual(self.lex_case_suffix())
            }
            Some(b'<') => {
                self.offset += 1;
                TokenKind::Less(self.lex_case_suffix())
            }
            Some(byte) if is_name_start(byte) => self.lex_identifier(),
            Some(byte) => {
                return Err(EvalError::new(
                    "E15",
                    start,
                    format!("invalid character 0x{byte:02x} in expression"),
                ));
            }
        })
    }

    fn peek(&self, ahead: usize) -> Option<u8> {
        self.source.get(self.offset + ahead).copied()
    }

    fn one(&mut self, kind: TokenKind) -> TokenKind {
        self.offset += 1;
        kind
    }

    fn skip_layout(&mut self) {
        loop {
            while matches!(self.peek(0), Some(b' ' | b'\t')) {
                self.offset += 1;
            }
            if self.peek(0) == Some(b'\n') {
                let mut ahead = 1;
                while matches!(self.peek(ahead), Some(b' ' | b'\t')) {
                    ahead += 1;
                }
                if self.peek(ahead) == Some(b'"')
                    && self.peek(ahead + 1) == Some(b'\\')
                    && self.peek(ahead + 2) == Some(b' ')
                {
                    self.offset += ahead + 3;
                    while !matches!(self.peek(0), None | Some(b'\n')) {
                        self.offset += 1;
                    }
                    self.at_line_start = true;
                    continue;
                }
                if self.peek(ahead) != Some(b'\\') {
                    break;
                }
                self.offset += ahead + 1;
                while matches!(self.peek(0), Some(b' ' | b'\t')) {
                    self.offset += 1;
                }
                self.at_line_start = false;
                continue;
            }
            if self.at_line_start && self.peek(0) == Some(b'\\') {
                self.offset += 1;
                while matches!(self.peek(0), Some(b' ' | b'\t')) {
                    self.offset += 1;
                }
                self.at_line_start = false;
                continue;
            }
            // In a continued expression Vim permits a standalone comment line
            // beginning with `"\ `; it contributes no expression tokens.
            if self.at_line_start
                && self.peek(0) == Some(b'"')
                && self.peek(1) == Some(b'\\')
                && self.peek(2) == Some(b' ')
            {
                while !matches!(self.peek(0), None | Some(b'\n')) {
                    self.offset += 1;
                }
                continue;
            }
            break;
        }
    }

    fn lex_case_suffix(&mut self) -> CaseSensitivity {
        match self.peek(0) {
            Some(b'#') => {
                self.offset += 1;
                CaseSensitivity::MatchCase
            }
            Some(b'?') => {
                self.offset += 1;
                CaseSensitivity::IgnoreCase
            }
            _ => CaseSensitivity::Default,
        }
    }

    fn lex_identifier(&mut self) -> TokenKind {
        let start = self.offset;
        self.offset += 1;
        if self.offset == start + 1
            && matches!(
                self.source[start],
                b'g' | b'b' | b'w' | b't' | b's' | b'l' | b'a' | b'v'
            )
            && self.peek(0) == Some(b':')
            && self.peek(1).is_some_and(is_name_start)
        {
            self.offset += 1;
        }
        while matches!(self.peek(0), Some(byte) if is_name_continue(byte)) {
            self.offset += 1;
        }
        let bytes = self.source[start..self.offset].to_vec();
        if bytes.as_slice() == b"is#" {
            return TokenKind::Is(CaseSensitivity::MatchCase);
        }
        if bytes.as_slice() == b"isnot#" {
            return TokenKind::IsNot(CaseSensitivity::MatchCase);
        }
        let case = if self.peek(0) == Some(b'#') || self.peek(0) == Some(b'?') {
            self.lex_case_suffix()
        } else {
            CaseSensitivity::Default
        };
        match bytes.as_slice() {
            b"is" => TokenKind::Is(case),
            b"isnot" => TokenKind::IsNot(case),
            _ => TokenKind::Identifier(bytes),
        }
    }

    fn lex_number(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        if self.peek(0) == Some(b'0') {
            match self.peek(1) {
                Some(b'x' | b'X') => return self.lex_based_integer(start, 16, 2),
                Some(b'o' | b'O') => return self.lex_based_integer(start, 8, 2),
                Some(b'b' | b'B') => return self.lex_based_integer(start, 2, 2),
                _ => {}
            }
        }
        while matches!(self.peek(0), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
        let integer_end = self.offset;
        if self.peek(0) == Some(b'.') && matches!(self.peek(1), Some(b'0'..=b'9')) {
            self.offset += 1;
            while matches!(self.peek(0), Some(b'0'..=b'9')) {
                self.offset += 1;
            }
            if matches!(self.peek(0), Some(b'e' | b'E')) {
                self.offset += 1;
                if matches!(self.peek(0), Some(b'+' | b'-')) {
                    self.offset += 1;
                }
                let digits = self.offset;
                while matches!(self.peek(0), Some(b'0'..=b'9')) {
                    self.offset += 1;
                }
                if self.offset == digits {
                    // `eval_number` (eval.c:3473-3479): a letter-less
                    // exponent turns the candidate back into a plain
                    // number, it is not an error.
                    self.offset = integer_end;
                    return self.finish_integer(start, integer_end);
                }
            }
            // `eval_number` (eval.c:3484-3486): a `.` or letter after the
            // float candidate keeps it a plain number — ":let vers =
            // 1.2.3" parses 1, then 2, then 3 across two `.` operators.
            if self
                .peek(0)
                .is_some_and(|byte| byte == b'.' || byte.is_ascii_alphabetic())
            {
                self.offset = integer_end;
                return self.finish_integer(start, integer_end);
            }
            let text = std::str::from_utf8(&self.source[start..self.offset])
                .map_err(|_| EvalError::new("E15", start, "invalid float literal"))?;
            return text
                .parse::<f64>()
                .map(TokenKind::Float)
                .map_err(|_| EvalError::new("E15", start, "invalid float literal"));
        }
        self.finish_integer(start, integer_end)
    }

    /// Parse the digits `[start..end]` with upstream's legacy-octal rule.
    fn finish_integer(&self, start: usize, end: usize) -> Result<TokenKind, EvalError> {
        let old_octal = self.source[start] == b'0'
            && end > start + 1
            && self.source[start + 1..end]
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'7'));
        let (base, digits_start) = if old_octal {
            (8, start + 1)
        } else {
            (10, start)
        };
        let digits = &self.source[digits_start..end];
        let digits = if digits.is_empty() {
            &self.source[start..end]
        } else {
            digits
        };
        parse_integer(digits, base, start).map(TokenKind::Integer)
    }

    fn lex_based_integer(
        &mut self,
        start: usize,
        base: u32,
        prefix_len: usize,
    ) -> Result<TokenKind, EvalError> {
        self.offset += prefix_len;
        let digits_start = self.offset;
        while matches!(self.peek(0), Some(byte) if byte.is_ascii_hexdigit()) {
            self.offset += 1;
        }
        if self.offset == digits_start {
            return Err(EvalError::new(
                "E15",
                start,
                "missing digits after numeric prefix",
            ));
        }
        let digits = &self.source[digits_start..self.offset];
        if digits.iter().any(|byte| match hex_value(*byte) {
            Some(digit) => u32::from(digit) >= base,
            None => true,
        }) {
            return Err(EvalError::new(
                "E15",
                start,
                "digit is invalid for numeric base",
            ));
        }
        parse_integer(digits, base, start).map(TokenKind::Integer)
    }

    fn lex_blob(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 2;
        let mut digits = Vec::new();
        while let Some(byte) = self.peek(0) {
            if byte.is_ascii_hexdigit() {
                digits.push(byte);
                self.offset += 1;
            } else if byte == b'.' && self.peek(1).is_some_and(|next| next.is_ascii_hexdigit()) {
                self.offset += 1;
            } else {
                break;
            }
        }
        if matches!(self.peek(0), Some(byte) if is_name_continue(byte)) {
            return Err(EvalError::new(
                "E973",
                start,
                "invalid character in blob literal",
            ));
        }
        if digits.len() % 2 != 0 {
            return Err(EvalError::new(
                "E973",
                start,
                "blob literal should have an even number of hex characters",
            ));
        }
        let mut bytes = Vec::with_capacity(digits.len() / 2);
        for pair in digits.as_chunks::<2>().0 {
            let high = hex_value(pair[0])
                .ok_or_else(|| EvalError::new("E973", start, "invalid blob digit"))?;
            let low = hex_value(pair[1])
                .ok_or_else(|| EvalError::new("E973", start, "invalid blob digit"))?;
            bytes.push((high << 4) | low);
        }
        Ok(TokenKind::Blob(bytes))
    }

    fn lex_interpolated(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        let Some(quote) = self.peek(1) else {
            return Err(EvalError::new(
                "E115",
                start,
                "missing quote in interpolated string",
            ));
        };
        self.offset += 2;
        let mut parts = Vec::new();
        let mut literal = Vec::new();
        loop {
            match self.peek(0) {
                None => {
                    return Err(EvalError::new(
                        if quote == b'"' { "E114" } else { "E115" },
                        start,
                        "missing quote in interpolated string",
                    ));
                }
                Some(byte) if byte == quote => {
                    if quote == b'\'' && self.peek(1) == Some(b'\'') {
                        literal.push(b'\'');
                        self.offset += 2;
                        continue;
                    }
                    self.offset += 1;
                    if !literal.is_empty() {
                        parts.push(InterpolationPart::Literal(literal));
                    }
                    return Ok(TokenKind::Interpolated(parts));
                }
                Some(byte) if quote == b'"' && byte == 0x5c => {
                    self.offset += 1;
                    literal.extend(self.lex_escape(start)?);
                }
                Some(b'{') if self.peek(1) == Some(b'{') => {
                    literal.push(b'{');
                    self.offset += 2;
                }
                Some(b'}') if self.peek(1) == Some(b'}') => {
                    literal.push(b'}');
                    self.offset += 2;
                }
                Some(b'}') => {
                    return Err(EvalError::new(
                        "E1278",
                        self.offset,
                        "stray closing brace in interpolated string",
                    ));
                }
                Some(b'{') => {
                    if !literal.is_empty() {
                        parts.push(InterpolationPart::Literal(std::mem::take(&mut literal)));
                    }
                    self.offset += 1;
                    let expression_start = self.offset;
                    let expression_end = self.scan_interpolation_expression(start)?;
                    let expression = self.source[expression_start..expression_end].to_vec();
                    if expression.iter().all(u8::is_ascii_whitespace) {
                        return Err(EvalError::new(
                            "E15",
                            expression_start,
                            "empty interpolated expression",
                        ));
                    }
                    parts.push(InterpolationPart::Expression(expression));
                }
                Some(byte) => {
                    literal.push(byte);
                    self.offset += 1;
                }
            }
        }
    }

    fn scan_interpolation_expression(&mut self, string_start: usize) -> Result<usize, EvalError> {
        let mut braces = 0usize;
        let mut quote = None;
        while let Some(byte) = self.peek(0) {
            if let Some(active) = quote {
                if active == b'"' && byte == b'\\' {
                    self.offset += 1;
                    if self.peek(0).is_some() {
                        self.offset += 1;
                    }
                    continue;
                }
                if byte == active {
                    if active == b'\'' && self.peek(1) == Some(b'\'') {
                        self.offset += 2;
                        continue;
                    }
                    quote = None;
                }
                self.offset += 1;
                continue;
            }
            match byte {
                b'\'' | b'"' => {
                    quote = Some(byte);
                    self.offset += 1;
                }
                b'{' => {
                    braces += 1;
                    self.offset += 1;
                }
                b'}' if braces == 0 => {
                    let end = self.offset;
                    self.offset += 1;
                    return Ok(end);
                }
                b'}' => {
                    braces -= 1;
                    self.offset += 1;
                }
                _ => self.offset += 1,
            }
        }
        Err(EvalError::new(
            "E1279",
            string_start,
            "missing closing brace in interpolated string",
        ))
    }

    fn lex_single_string(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let mut bytes = Vec::new();
        loop {
            match self.peek(0) {
                None => return Err(EvalError::new("E115", start, "missing single quote")),
                Some(b'\'') if self.peek(1) == Some(b'\'') => {
                    bytes.push(b'\'');
                    self.offset += 2;
                }
                Some(b'\'') => {
                    self.offset += 1;
                    return Ok(TokenKind::String(bytes));
                }
                Some(byte) => {
                    bytes.push(byte);
                    self.offset += 1;
                }
            }
        }
    }

    fn lex_double_string(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let mut bytes = Vec::new();
        let mut nul_seen = false;
        loop {
            match self.peek(0) {
                None => return Err(EvalError::new("E114", start, "missing double quote")),
                Some(b'"') => {
                    self.offset += 1;
                    return Ok(TokenKind::String(bytes));
                }
                Some(b'\\') => {
                    self.offset += 1;
                    let escaped = self.lex_escape(start)?;
                    if !nul_seen {
                        if escaped.contains(&0) {
                            let before_nul = match escaped.iter().position(|byte| *byte == 0) {
                                Some(position) => position,
                                None => escaped.len(),
                            };
                            bytes.extend_from_slice(&escaped[..before_nul]);
                            nul_seen = true;
                        } else {
                            bytes.extend_from_slice(&escaped);
                        }
                    }
                }
                Some(byte) => {
                    if !nul_seen {
                        bytes.push(byte);
                    }
                    self.offset += 1;
                }
            }
        }
    }

    fn lex_escape(&mut self, string_start: usize) -> Result<Vec<u8>, EvalError> {
        let escape_offset = self.offset.saturating_sub(1);
        let Some(byte) = self.peek(0) else {
            return Err(EvalError::new(
                "E114",
                string_start,
                "unfinished string escape",
            ));
        };
        self.offset += 1;
        let simple = match byte {
            b'b' => Some(0x08),
            b'e' => Some(0x1b),
            b'f' => Some(0x0c),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'\\' => Some(b'\\'),
            b'"' => Some(b'"'),
            _ => None,
        };
        if let Some(value) = simple {
            return Ok(vec![value]);
        }
        if matches!(byte, b'0'..=b'7') {
            let mut value = u32::from(byte - b'0');
            for _ in 1..3 {
                let Some(next @ b'0'..=b'7') = self.peek(0) else {
                    break;
                };
                value = value * 8 + u32::from(next - b'0');
                self.offset += 1;
            }
            return Ok(vec![u8::try_from(value & 0xff).unwrap_or(0)]);
        }
        if matches!(byte, b'x' | b'X') {
            let value = self.read_hex_escape(2, escape_offset)?;
            return Ok(vec![u8::try_from(value).map_err(|_| {
                EvalError::new("E114", escape_offset, "hex escape out of range")
            })?]);
        }
        if matches!(byte, b'u' | b'U') {
            let limit = if byte == b'u' { 4 } else { 8 };
            let value = self.read_hex_escape(limit, escape_offset)?;
            let Some(character) = char::from_u32(value) else {
                return Err(EvalError::new(
                    "E114",
                    escape_offset,
                    "invalid Unicode escape",
                ));
            };
            let mut encoded = [0; 4];
            return Ok(character.encode_utf8(&mut encoded).as_bytes().to_vec());
        }
        if byte == b'<' {
            let tail = &self.source[self.offset..];
            match find_special_key(tail, escape_offset)? {
                Some((bytes, consumed)) => {
                    self.offset += consumed;
                    return Ok(bytes);
                }
                // Unresolved `\<`: Vim emits the `<` literal and scans the
                // rest as ordinary string text (`"\<C-\\"` is the string
                // `<C-\`).
                None => return Ok(vec![b'<']),
            }
        }
        // As in Vim, an unrecognized escape keeps the escaped byte and drops
        // only the backslash.
        Ok(vec![byte])
    }

    fn read_hex_escape(&mut self, limit: usize, offset: usize) -> Result<u32, EvalError> {
        let mut value = 0_u32;
        let mut count = 0;
        while count < limit {
            let Some(byte) = self.peek(0) else { break };
            let Some(digit) = hex_value(byte) else { break };
            value = value * 16 + u32::from(digit);
            count += 1;
            self.offset += 1;
        }
        if count == 0 {
            Err(EvalError::new(
                "E114",
                offset,
                "hex escape requires at least one digit",
            ))
        } else {
            Ok(value)
        }
    }

    fn lex_environment(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let name_start = self.offset;
        while matches!(self.peek(0), Some(byte) if is_name_continue(byte)) {
            self.offset += 1;
        }
        if self.offset == name_start {
            Err(EvalError::new(
                "E15",
                start,
                "environment variable name is missing",
            ))
        } else {
            Ok(TokenKind::Environment(
                self.source[name_start..self.offset].to_vec(),
            ))
        }
    }

    fn lex_option(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let scope = if matches!(
            (self.peek(0), self.peek(1)),
            (Some(b'g' | b'l'), Some(b':'))
        ) {
            let scope = self.peek(0);
            self.offset += 2;
            scope
        } else {
            None
        };
        let name_start = self.offset;
        while matches!(self.peek(0), Some(byte) if is_name_continue(byte)) {
            self.offset += 1;
        }
        if self.offset == name_start {
            Err(EvalError::new("E112", start, "option name is missing"))
        } else {
            Ok(TokenKind::Option {
                scope,
                name: self.source[name_start..self.offset].to_vec(),
            })
        }
    }

    fn lex_register(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let Some(name) = self.peek(0) else {
            return Err(EvalError::new("E15", start, "register name is missing"));
        };
        self.offset += 1;
        Ok(TokenKind::Register(name))
    }
}

fn parse_integer(digits: &[u8], base: u32, offset: usize) -> Result<i64, EvalError> {
    let text = std::str::from_utf8(digits)
        .map_err(|_| EvalError::new("E15", offset, "invalid integer literal"))?;
    i64::from_str_radix(text, base)
        .map_err(|_| EvalError::new("E15", offset, "integer literal is out of range"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'#')
}

/// One resolved key from a `\<name>` escape.
#[derive(Clone, Copy)]
enum DecodedKey {
    /// A plain Unicode scalar (or byte), emitted with `utf_char2bytes`.
    Char(u32),
    /// An internal two-byte special key emitted as `K_SPECIAL t0 t1`.
    Special(u8, u8),
}

/// `simplify_key`'s `modifier_keys_table` (`keycodes.c:51-137`):
/// `(modifier mask, with-modifier pair, without-modifier pair)`.
static SIMPLIFY_TABLE: &[(u8, u8, u8, u8, u8)] = &[
    (0x02, b'&', b'9', b'@', b'1'),
    (0x02, b'&', b'0', b'@', b'2'),
    (0x02, b'*', b'1', b'@', b'4'),
    (0x02, b'*', b'2', b'@', b'5'),
    (0x02, b'*', b'3', b'@', b'6'),
    (0x02, b'*', b'4', b'k', b'D'),
    (0x02, b'*', b'5', b'k', b'L'),
    (0x02, b'*', b'7', b'@', b'7'),
    (0x02, b'*', b'9', b'@', b'9'),
    (0x02, b'*', b'0', b'@', b'0'),
    (0x02, b'#', b'1', b'%', b'1'),
    (0x02, b'#', b'2', b'k', b'h'),
    (0x02, b'#', b'3', b'k', b'I'),
    (0x02, b'#', b'4', b'k', b'l'),
    (0x02, b'%', b'a', b'%', b'3'),
    (0x02, b'%', b'b', b'%', b'4'),
    (0x02, b'%', b'c', b'%', b'5'),
    (0x02, b'%', b'd', b'%', b'7'),
    (0x02, b'%', b'e', b'%', b'8'),
    (0x02, b'%', b'f', b'%', b'9'),
    (0x02, b'%', b'g', b'%', b'0'),
    (0x02, b'%', b'h', b'&', b'3'),
    (0x02, b'%', b'i', b'k', b'r'),
    (0x02, b'%', b'j', b'&', b'5'),
    (0x02, b'!', b'1', b'&', b'6'),
    (0x02, b'!', b'2', b'&', b'7'),
    (0x02, b'!', b'3', b'&', b'8'),
    (0x04, 0xfd, 88, b'@', b'7'),
    (0x04, 0xfd, 87, b'k', b'h'),
    (0x04, 0xfd, 85, b'k', b'l'),
    (0x04, 0xfd, 86, b'k', b'r'),
    (0x02, 0xfd, 4, b'k', b'u'),
    (0x02, 0xfd, 5, b'k', b'd'),
    (0x02, 0xfd, 71, 0xfd, 57),
    (0x02, 0xfd, 72, 0xfd, 58),
    (0x02, 0xfd, 73, 0xfd, 59),
    (0x02, 0xfd, 74, 0xfd, 60),
    (0x02, 0xfd, 6, b'k', b'1'),
    (0x02, 0xfd, 7, b'k', b'2'),
    (0x02, 0xfd, 8, b'k', b'3'),
    (0x02, 0xfd, 9, b'k', b'4'),
    (0x02, 0xfd, 10, b'k', b'5'),
    (0x02, 0xfd, 11, b'k', b'6'),
    (0x02, 0xfd, 12, b'k', b'7'),
    (0x02, 0xfd, 13, b'k', b'8'),
    (0x02, 0xfd, 14, b'k', b'9'),
    (0x02, 0xfd, 15, b'k', b';'),
    (0x02, 0xfd, 16, b'F', b'1'),
    (0x02, 0xfd, 17, b'F', b'2'),
    (0x02, 0xfd, 18, b'F', b'3'),
    (0x02, 0xfd, 19, b'F', b'4'),
    (0x02, 0xfd, 20, b'F', b'5'),
    (0x02, 0xfd, 21, b'F', b'6'),
    (0x02, 0xfd, 22, b'F', b'7'),
    (0x02, 0xfd, 23, b'F', b'8'),
    (0x02, 0xfd, 24, b'F', b'9'),
    (0x02, 0xfd, 25, b'F', b'A'),
    (0x02, 0xfd, 26, b'F', b'B'),
    (0x02, 0xfd, 27, b'F', b'C'),
    (0x02, 0xfd, 28, b'F', b'D'),
    (0x02, 0xfd, 29, b'F', b'E'),
    (0x02, 0xfd, 30, b'F', b'F'),
    (0x02, 0xfd, 31, b'F', b'G'),
    (0x02, 0xfd, 32, b'F', b'H'),
    (0x02, 0xfd, 33, b'F', b'I'),
    (0x02, 0xfd, 34, b'F', b'J'),
    (0x02, 0xfd, 35, b'F', b'K'),
    (0x02, 0xfd, 36, b'F', b'L'),
    (0x02, 0xfd, 37, b'F', b'M'),
    (0x02, 0xfd, 38, b'F', b'N'),
    (0x02, 0xfd, 39, b'F', b'O'),
    (0x02, 0xfd, 40, b'F', b'P'),
    (0x02, 0xfd, 41, b'F', b'Q'),
    (0x02, 0xfd, 42, b'F', b'R'),
    (0x02, b'k', b'B', 0xfd, 54),
];

/// Identifier byte for the `find_special_key` name scan (`ascii_isident`).
fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `name_to_mod_mask` (`keycodes.c:171-184`): modifier letters recognized in a
/// `\<X-key>` prefix. Case-insensitive; 'A' is an 'M' alias.
fn mod_mask(byte: u8) -> Option<u8> {
    Some(match byte.to_ascii_uppercase() {
        b'M' | b'A' => 0x08,
        b'T' => 0x10,
        b'C' => 0x04,
        b'S' => 0x02,
        b'2' => 0x20,
        b'3' => 0x40,
        b'4' => 0x60,
        b'D' => 0x80,
        _ => return None,
    })
}

/// Byte length of the first UTF-8 scalar (`utfc_ptr2len`); 1 on invalid UTF-8.
fn utf_len(bytes: &[u8]) -> usize {
    let Some(&first) = bytes.first() else { return 0 };
    let len = if first < 0x80 {
        1
    } else if first < 0xe0 {
        2
    } else if first < 0xf0 {
        3
    } else {
        4
    };
    if bytes.len() >= len && bytes[1..len].iter().all(|byte| byte & 0xc0 == 0x80) {
        len
    } else {
        1
    }
}

/// `vim_str2nr` with `STR2NR_ALL` and `strict` (`charset.c`): optional sign,
/// `0x`/`0o`/`0b` radix prefixes (a bare leading `0` stays decimal in Neovim),
/// at least one digit, and no trailing identifier junk. Returns the value and
/// the consumed byte count.
fn vim_str2nr(text: &[u8]) -> Option<(u32, usize)> {
    let mut rest = text;
    let mut negative = false;
    if matches!(rest.first(), Some(&b'-' | &b'+')) {
        negative = rest[0] == b'-';
        rest = &rest[1..];
    }
    let (digits, radix) = if rest.len() > 2 && rest[..2].eq_ignore_ascii_case(b"0x") {
        (&rest[2..], 16)
    } else if rest.len() > 2 && rest[..2].eq_ignore_ascii_case(b"0o") {
        (&rest[2..], 8)
    } else if rest.len() > 2 && rest[..2].eq_ignore_ascii_case(b"0b") {
        (&rest[2..], 2)
    } else {
        (rest, 10)
    };
    let digit_len = digits
        .iter()
        .take_while(|byte| (**byte as char).is_digit(radix))
        .count();
    if digit_len == 0 {
        return None;
    }
    // The number ends at the first non-digit; strict mode rejects a trailing
    // identifier character (`char-66x` is E474).
    if digits
        .get(digit_len)
        .is_some_and(|byte| is_ident(*byte))
    {
        return None;
    }
    let value = u32::from_str_radix(
        std::str::from_utf8(&digits[..digit_len]).ok()?,
        radix,
    )
    .ok()?;
    let signed = if negative {
        value.wrapping_neg()
    } else {
        value
    };
    Some((signed, text.len() - digits.len() + digit_len))
}

/// `handle_x_keys` (`keycodes.c:236-268`): maps the extra xterm keys to the
/// codes they alias.
fn handle_x_keys(second: u8, third: u8) -> DecodedKey {
    const EXTRA: u8 = 0xfd;
    match (second, third) {
        (EXTRA, 57..=60) => DecodedKey::Special(b'k', third - 8),
        (EXTRA, 71..=74) => DecodedKey::Special(EXTRA, third - 65),
        (EXTRA, 65) => DecodedKey::Special(b'k', b'u'),
        (EXTRA, 66) => DecodedKey::Special(b'k', b'd'),
        (EXTRA, 67) => DecodedKey::Special(b'k', b'l'),
        (EXTRA, 68) => DecodedKey::Special(b'k', b'r'),
        (EXTRA, 63 | 64) => DecodedKey::Special(b'k', b'h'),
        (EXTRA, 61 | 62) => DecodedKey::Special(b'@', b'7'),
        _ => DecodedKey::Special(second, third),
    }
}

/// `get_special_key_code` (`keycodes.c:664-680`): the named-key table,
/// case-insensitive. `t_xx` resolves to the raw termcap pair — the second `x`
/// may be the `>` terminator itself, so `next` supplies the byte right after
/// `name` (always `>` at this call site).
#[allow(clippy::too_many_lines)] // one flat table mirrors `keycode_names.generated.h`
fn named_key_code(name: &[u8], next: u8) -> Option<DecodedKey> {
    const EXTRA: u8 = 0xfd;
    if name.len() >= 3 && name[0] == b't' && name[1] == b'_' {
        return Some(DecodedKey::Special(name[2], name.get(3).copied().unwrap_or(next)));
    }
    let lower: Vec<u8> = name.iter().map(u8::to_ascii_lowercase).collect();
    if let [b'f', rest @ ..] = lower.as_slice()
        && !rest.is_empty()
        && rest.iter().all(u8::is_ascii_digit)
    {
        let number: u8 = std::str::from_utf8(rest).ok()?.parse().ok()?;
        return f_key(number);
    }
    let plain = match lower.as_slice() {
        b"esc" | b"escape" => 0x1b,
        b"cr" | b"enter" | b"return" => b'\r',
        b"nl" | b"lf" | b"newline" | b"linefeed" => b'\n',
        b"tab" => b'\t',
        b"space" => b' ',
        b"lt" => b'<',
        b"bar" => b'|',
        b"bslash" => b'\\',
        b"csi" => 0x9b,
        _ => 0,
    };
    if plain != 0 {
        return Some(DecodedKey::Char(u32::from(plain)));
    }
    let special = match lower.as_slice() {
        b"bs" | b"backspace" => (b'k', b'b'),
        b"del" | b"delete" => (b'k', b'D'),
        b"up" => (b'k', b'u'),
        b"down" => (b'k', b'd'),
        b"left" => (b'k', b'l'),
        b"right" => (b'k', b'r'),
        b"home" => (b'k', b'h'),
        b"end" => (b'@', b'7'),
        b"pageup" => (b'k', b'P'),
        b"pagedown" => (b'k', b'N'),
        b"ins" | b"insert" => (b'k', b'I'),
        b"help" => (b'%', b'1'),
        b"undo" => (b'&', b'8'),
        b"find" => (b'@', b'0'),
        b"select" => (b'*', b'6'),
        b"nul" => (0xff, b'X'),
        b"k0" => (b'K', b'C'),
        b"k1" => (b'K', b'D'),
        b"k2" => (b'K', b'E'),
        b"k3" => (b'K', b'F'),
        b"k4" => (b'K', b'G'),
        b"k5" => (b'K', b'H'),
        b"k6" => (b'K', b'I'),
        b"k7" => (b'K', b'J'),
        b"k8" => (b'K', b'K'),
        b"k9" => (b'K', b'L'),
        b"kup" | b"kp8" => (b'K', b'u'),
        b"kdown" | b"kp2" => (b'K', b'd'),
        b"kleft" | b"kp4" => (b'K', b'l'),
        b"kright" | b"kp6" => (b'K', b'r'),
        b"khome" | b"kp7" => (b'K', b'1'),
        b"kend" | b"kp1" => (b'K', b'4'),
        b"korigin" | b"kp5" => (b'K', b'2'),
        b"kpageup" | b"kp9" => (b'K', b'3'),
        b"kpagedown" | b"kp3" => (b'K', b'5'),
        b"kplus" | b"kpplus" => (b'K', b'6'),
        b"kminus" | b"kpminus" => (b'K', b'7'),
        b"kdivide" | b"kpdiv" => (b'K', b'8'),
        b"kmultiply" | b"kpmult" => (b'K', b'9'),
        b"kenter" | b"kpenter" => (b'K', b'A'),
        b"kpoint" => (b'K', b'B'),
        b"kcomma" | b"kpcomma" => (b'K', b'M'),
        b"kequal" | b"kpequals" => (b'K', b'N'),
        b"kp0" | b"kins" | b"kinsert" => (EXTRA, 79),
        b"kdel" | b"kpperiod" => (EXTRA, 80),
        b"xf1" => (EXTRA, 57),
        b"xf2" => (EXTRA, 58),
        b"xf3" => (EXTRA, 59),
        b"xf4" => (EXTRA, 60),
        b"xend" => (EXTRA, 61),
        b"zend" => (EXTRA, 62),
        b"xhome" => (EXTRA, 63),
        b"zhome" => (EXTRA, 64),
        b"xup" => (EXTRA, 65),
        b"xdown" => (EXTRA, 66),
        b"xleft" => (EXTRA, 67),
        b"xright" => (EXTRA, 68),
        b"leftmousenm" => (EXTRA, 69),
        b"leftreleasenm" => (EXTRA, 70),
        b"ignore" => (EXTRA, 53),
        b"snr" => (EXTRA, 82),
        b"plug" => (EXTRA, 83),
        b"drop" => (EXTRA, 95),
        b"cmd" => (EXTRA, 104),
        b"leftmouse" => (EXTRA, 44),
        b"leftdrag" => (EXTRA, 45),
        b"leftrelease" => (EXTRA, 46),
        b"middlemouse" => (EXTRA, 47),
        b"middledrag" => (EXTRA, 48),
        b"middlerelease" => (EXTRA, 49),
        b"rightmouse" => (EXTRA, 50),
        b"rightdrag" => (EXTRA, 51),
        b"rightrelease" => (EXTRA, 52),
        b"x1mouse" => (EXTRA, 89),
        b"x1drag" => (EXTRA, 90),
        b"x1release" => (EXTRA, 91),
        b"x2mouse" => (EXTRA, 92),
        b"x2drag" => (EXTRA, 93),
        b"x2release" => (EXTRA, 94),
        b"mousemove" => (EXTRA, 100),
        // `ScrollWheelLeft`/`Right` carry the pseudo-codes in upstream's
        // order — K_MOUSERIGHT for left and K_MOUSELEFT for right.
        b"scrollwheelup" | b"mousedown" => (EXTRA, 75),
        b"scrollwheeldown" | b"mouseup" => (EXTRA, 76),
        b"scrollwheelleft" => (EXTRA, 78),
        b"scrollwheelright" => (EXTRA, 77),
        b"mouse" => (0xfb, b'X'),
        _ => return None,
    };
    Some(DecodedKey::Special(special.0, special.1))
}

/// `K_F1`..`K_F63` (`keycodes.h:270-339`): F1-F10 live in the `k` row,
/// F11-F63 in the `F` row.
fn f_key(number: u8) -> Option<DecodedKey> {
    let pair = match number {
        1..=9 => (b'k', b'0' + number),
        10 => (b'k', b';'),
        11..=63 => (
            b'F',
            *b"123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqr"
                .get(usize::from(number) - 11)?,
        ),
        _ => return None,
    };
    Some(DecodedKey::Special(pair.0, pair.1))
}

/// `find_special_key` + `special_to_buf` (`keycodes.c:470-604`), as called by
/// `eval_string` with `FSK_KEYCODE | FSK_IN_STRING` plus `FSK_SIMPLIFY` when
/// the name does not start with `*`.
///
/// `tail` is the source immediately after `\<`. On success returns the bytes
/// the string stores and the number of `tail` bytes the whole escape consumed
/// (name plus the `>` terminator). `Ok(None)` means the escape did not
/// resolve: the caller emits a literal `<` and resumes at the next byte,
/// matching upstream's `mb_copy_char` fallback.
#[allow(clippy::too_many_lines)] // mirrors `find_special_key` + `special_to_buf` end to end
fn find_special_key(tail: &[u8], offset: usize) -> Result<Option<(Vec<u8>, usize)>, EvalError> {
    const SPECIAL: u8 = 0x80;
    const MODIFIER: u8 = 0xfc;
    const MOD_MASK_SHIFT: u8 = 0x02;
    const MOD_MASK_CTRL: u8 = 0x04;

    let mut src = 0usize;
    let mut simplify = true;
    if tail.first() == Some(&b'*') {
        simplify = false;
        src = 1;
    }

    // Find end of modifier list (`keycodes.c:494-522`): walk `-` and
    // identifier bytes, remembering the last `-` and skipping `t_xx` termcap
    // names and `<char-N>` numbers whose dashes are not modifier separators.
    let mut last_dash: Option<usize> = None;
    let mut bp = src;
    while bp < tail.len() && (tail[bp] == b'-' || is_ident(tail[bp])) {
        if tail[bp] == b'-' {
            last_dash = Some(bp);
            if bp + 1 < tail.len() {
                let len = utf_len(&tail[bp + 1..]);
                // `<C-">`/`\<M-">` are not special inside a double-quoted
                // string: `"` is the delimiter. `\">` escapes it.
                if tail.len() - bp > len + 1
                    && tail[bp + 1] != b'"'
                    && tail.get(bp + 1 + len) == Some(&b'>')
                {
                    bp += len;
                } else if tail.len() - bp > 3
                    && tail[bp + 1] == b'\\'
                    && tail[bp + 2] == b'"'
                    && tail[bp + 3] == b'>'
                {
                    bp += 2;
                }
            }
        }
        if tail.len() - bp > 4 && tail[bp] == b't' && tail[bp + 1] == b'_' {
            bp += 3;
        } else if tail.len() - bp > 5 && tail[bp..bp + 5].eq_ignore_ascii_case(b"char-") {
            let Some((_, len)) = vim_str2nr(&tail[bp + 5..]) else {
                return Err(EvalError::new("E474", offset, "Invalid argument"));
            };
            bp += len + 5;
            break;
        }
        bp += 1;
    }
    if tail.get(bp) != Some(&b'>') {
        return Ok(None);
    }
    let consumed = bp + 1;
    let name = &tail[src..bp];
    let (pre, after) = match last_dash {
        Some(dash) => (&name[..dash - src], &name[dash - src + 1..]),
        None => (&name[..0], name),
    };

    // Which modifiers are given? (`keycodes.c:531-540`)
    let mut modifiers = 0u8;
    for &byte in pre {
        if byte == b'-' {
            continue;
        }
        let Some(bit) = mod_mask(byte) else {
            return Ok(None);
        };
        modifiers |= bit;
    }

    let resolved = if after.len() > 5
        && after[..5].eq_ignore_ascii_case(b"char-")
        && after[5].is_ascii_digit()
    {
        // `<Char-123>`, `<Char-033>`, `<Char-0x33>` (`keycodes.c:544-552`).
        vim_str2nr(&after[5..])
            .map(|(value, _)| DecodedKey::Char(value))
    } else {
        let single = if modifiers != 0 {
            if after == b"\\\"" {
                // `<C-\">` inside a double-quoted string (`keycodes.c:557-559`).
                Some(DecodedKey::Char(u32::from(b'"')))
            } else {
                match after {
                    &[first] => Some(DecodedKey::Char(u32::from(first))),
                    _ if utf_len(after) == after.len() => std::str::from_utf8(after)
                        .ok()
                        .and_then(|text| text.chars().next())
                        .map(|ch| DecodedKey::Char(u32::from(ch))),
                    _ => None,
                }
            }
        } else {
            None
        };
        match single {
            Some(key) => Some(key),
            None => named_key_code(after, b'>').map(|key| match key {
                DecodedKey::Char(value) => DecodedKey::Char(value),
                DecodedKey::Special(second, third) => handle_x_keys(second, third),
            }),
        }
    };
    let Some(mut resolved) = resolved else {
        return Ok(None);
    };
    if matches!(resolved, DecodedKey::Char(0)) {
        // `key != NUL` (`keycodes.c:575`): a zero keycode is no match.
        return Ok(None);
    }

    // `simplify_key` (`keycodes.c:190-213`): fold Shift/Ctrl into a dedicated
    // shifted keycode when one exists.
    if modifiers & (MOD_MASK_SHIFT | MOD_MASK_CTRL) != 0 {
        if let DecodedKey::Char(value) = resolved
            && value == u32::from(b'\t')
            && modifiers & MOD_MASK_SHIFT != 0
        {
            resolved = DecodedKey::Special(b'k', b'B');
            modifiers &= !MOD_MASK_SHIFT;
        } else if let DecodedKey::Special(first, second) = resolved
            && let Some(entry) = SIMPLIFY_TABLE
                .iter()
                .find(|row| modifiers & row.0 != 0 && row.3 == first && row.4 == second)
        {
            modifiers &= !entry.0;
            resolved = DecodedKey::Special(entry.1, entry.2);
        }
    }

    // `extract_modifiers` (`keycodes.c:609-640`): fold Shift+letter into the
    // uppercase key and simplify Ctrl+key into the control byte.
    if let DecodedKey::Char(mut value) = resolved {
        if modifiers & MOD_MASK_SHIFT != 0
            && value < 0x80
            && u8::try_from(value).is_ok_and(|byte| byte.is_ascii_alphabetic())
        {
            value = u32::from(u8::try_from(value).unwrap_or_default().to_ascii_uppercase());
            if modifiers & MOD_MASK_CTRL == 0 {
                modifiers &= !MOD_MASK_SHIFT;
            }
        }
        if modifiers & MOD_MASK_CTRL != 0
            && value < 0x80
            && u8::try_from(value).is_ok_and(|byte| byte.is_ascii_alphabetic())
        {
            value = u32::from(u8::try_from(value).unwrap_or_default().to_ascii_uppercase());
        }
        if simplify
            && modifiers & MOD_MASK_CTRL != 0
            && ((0x3f..=0x5f).contains(&value)
                || (value < 0x80
                    && u8::try_from(value).is_ok_and(|byte| byte.is_ascii_alphabetic())))
        {
            value = match value {
                0x3f => 0x7f,
                value => value & 0x1f,
            };
            modifiers &= !MOD_MASK_CTRL;
            resolved = if value == 0 {
                // `<C-@>` is `<Nul>`.
                DecodedKey::Special(0xff, b'X')
            } else {
                DecodedKey::Char(value)
            };
        } else {
            resolved = DecodedKey::Char(value);
        }
    }

    let mut output = Vec::new();
    if modifiers != 0 {
        output.extend_from_slice(&[SPECIAL, MODIFIER, modifiers]);
    }
    match resolved {
        DecodedKey::Char(value) => {
            let Some(ch) = char::from_u32(value) else {
                return Ok(None);
            };
            output.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
        }
        DecodedKey::Special(second, third) => {
            output.extend_from_slice(&[SPECIAL, second, third]);
        }
    }
    Ok(Some((output, consumed)))
}
