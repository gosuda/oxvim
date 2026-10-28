//! Vim-compatible register storage.

use thiserror::Error;

/// The shape of text stored in a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterKind {
    /// Text is inserted at a byte-column position.
    CharacterWise,
    /// Complete logical lines are inserted after the target line.
    LineWise,
    /// A rectangle whose rows are padded before insertion.
    BlockWise {
        /// Rectangle width in bytes.
        width: usize,
    },
}

/// Validated text held by a register.
///
/// Lines never contain line separators and always contain valid UTF-8. A
/// characterwise value may have several lines; their separators are restored
/// when the value is rendered as bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterContent {
    kind: RegisterKind,
    lines: Vec<Vec<u8>>,
}

impl RegisterContent {
    /// Creates validated register content from newline-free logical lines.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::NewlineInLine`] if any line contains a newline,
    /// [`RegisterError::InvalidUtf8`] if any line is not valid UTF-8, or
    /// [`RegisterError::InvalidBlockWidth`] for a zero or too-narrow blockwise
    /// width.
    pub fn new(kind: RegisterKind, lines: Vec<Vec<u8>>) -> Result<Self, RegisterError> {
        let lines = if lines.is_empty() {
            vec![Vec::new()]
        } else {
            lines
        };
        for line in &lines {
            validate_register_line(line)?;
        }
        if let RegisterKind::BlockWise { width } = kind
            && (width == 0 || lines.iter().any(|line| line.len() > width))
        {
            return Err(RegisterError::InvalidBlockWidth { width });
        }
        Ok(Self { kind, lines })
    }

    /// Creates a register value from logical rows without requiring valid
    /// UTF-8 or newline-free rows.
    ///
    /// Macro registers hold encoded key sequences, which are binary, and
    /// `setreg()` stores its argument verbatim the way upstream's
    /// `str_to_reg` does: invalid UTF-8 stays invalid (it renders as
    /// `<xx>`), and a list item may carry an embedded line feed (it renders
    /// as `^@`). Callers that need validated text use [`Self::new`].
    #[must_use]
    pub fn from_binary_lines(kind: RegisterKind, lines: Vec<Vec<u8>>) -> Self {
        let lines = if lines.is_empty() {
            vec![Vec::new()]
        } else {
            lines
        };
        Self { kind, lines }
    }

    /// Counts display cells in a register row the way upstream's
    /// `mb_string2cells_len` does for `str_to_reg` block widths: printable
    /// ASCII and tabs are one cell, control bytes two, each invalid byte
    /// four (`<xx>`), and other characters their `unicode-width` cells.
    /// An embedded line feed counts two (`^@`).
    #[must_use]
    pub(crate) fn display_cells(line: &[u8]) -> usize {
        use unicode_width::UnicodeWidthChar;

        let mut cells = 0;
        let mut index = 0;
        while index < line.len() {
            let first = line[index];
            if first < 0x80 {
                cells += match first {
                    0x00..=0x08 | 0x0a..=0x1f | 0x7f => 2,
                    _ => 1,
                };
                index += 1;
                continue;
            }
            let width = match first {
                0xc2..=0xdf => {
                    if line.get(index + 1).is_some_and(|byte| (0x80..=0xbf).contains(byte)) {
                        2
                    } else {
                        0
                    }
                }
                0xe0..=0xef => {
                    if line.get(index + 1).is_some_and(|byte| (0x80..=0xbf).contains(byte))
                        && line.get(index + 2).is_some_and(|byte| (0x80..=0xbf).contains(byte))
                    {
                        3
                    } else {
                        0
                    }
                }
                0xf0..=0xf4 => {
                    if line.get(index + 1).is_some_and(|byte| (0x80..=0xbf).contains(byte))
                        && line.get(index + 2).is_some_and(|byte| (0x80..=0xbf).contains(byte))
                        && line.get(index + 3).is_some_and(|byte| (0x80..=0xbf).contains(byte))
                    {
                        4
                    } else {
                        0
                    }
                }
                _ => 0,
            };
            if width == 0 {
                // An invalid byte renders as `<xx>`.
                cells += 4;
                index += 1;
                continue;
            }
            let text = std::str::from_utf8(&line[index..index + width]);
            let character = text.ok().and_then(|text| text.chars().next());
            cells += character.map_or(1, |character| {
                UnicodeWidthChar::width(character).unwrap_or(1).max(1)
            });
            index += width;
        }
        cells
    }
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidUtf8`] if `bytes` is not valid UTF-8.
    pub fn characterwise(bytes: &[u8]) -> Result<Self, RegisterError> {
        let text = std::str::from_utf8(bytes).map_err(|_| RegisterError::InvalidUtf8)?;
        let lines = text
            .split('\n')
            .map(|line| line.as_bytes().to_vec())
            .collect();
        Self::new(RegisterKind::CharacterWise, lines)
    }

    /// Infers Vim's register shape from serialized text.
    ///
    /// A final line feed is structural and is removed before line storage. A
    /// final carriage return also selects linewise storage, but remains text.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidUtf8`] if `bytes` is not valid UTF-8.
    pub fn from_text(bytes: &[u8]) -> Result<Self, RegisterError> {
        let linewise = bytes
            .last()
            .is_some_and(|byte| matches!(byte, b'\n' | b'\r'));
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let text = std::str::from_utf8(bytes).map_err(|_| RegisterError::InvalidUtf8)?;
        let lines = text
            .split('\n')
            .map(|line| line.as_bytes().to_vec())
            .collect();
        let kind = if linewise {
            RegisterKind::LineWise
        } else {
            RegisterKind::CharacterWise
        };
        Self::new(kind, lines)
    }

    /// Creates a linewise value from newline-free logical lines.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::NewlineInLine`] if any line contains a newline
    /// or [`RegisterError::InvalidUtf8`] if any line is not valid UTF-8.
    pub fn linewise(lines: Vec<Vec<u8>>) -> Result<Self, RegisterError> {
        Self::new(RegisterKind::LineWise, lines)
    }

    /// Creates a rectangular value with an exact byte width.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::NewlineInLine`] if any line contains a newline,
    /// [`RegisterError::InvalidUtf8`] if any line is not valid UTF-8, or
    /// [`RegisterError::InvalidBlockWidth`] if `width` is zero or narrower
    /// than a row.
    pub fn blockwise(lines: Vec<Vec<u8>>, width: usize) -> Result<Self, RegisterError> {
        Self::new(RegisterKind::BlockWise { width }, lines)
    }

    /// Returns the register's text shape.
    #[must_use]
    pub const fn kind(&self) -> RegisterKind {
        self.kind
    }

    /// Returns the newline-free logical rows.
    #[must_use]
    pub fn lines(&self) -> &[Vec<u8>] {
        &self.lines
    }

    /// Serializes the rows with line-feed separators.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let separators = self.lines.len().saturating_sub(1);
        let capacity = self
            .lines
            .iter()
            .fold(separators, |total, line| total.saturating_add(line.len()));
        let mut bytes = Vec::with_capacity(capacity);
        for (index, line) in self.lines.iter().enumerate() {
            if index != 0 {
                bytes.push(b'\n');
            }
            bytes.extend_from_slice(line);
        }
        bytes
    }

    /// Returns the serialized bytes, with a trailing newline for linewise
    /// content (`getreg()` in `eval/funcs.c` uses `get_reg_contents`).
    #[must_use]
    pub fn getreg_bytes(&self) -> Vec<u8> {
        let mut bytes = self.to_bytes();
        if matches!(self.kind, RegisterKind::LineWise) {
            bytes.push(b'\n');
        }
        bytes
    }

    /// Returns the logical lines as owned vectors (`getreg()` with `{list}`).
    #[must_use]
    pub fn getreg_lines(&self) -> Vec<Vec<u8>> {
        self.lines.clone()
    }

    fn append(&mut self, other: &Self) {
        if matches!(other.kind, RegisterKind::LineWise) {
            if matches!(self.kind, RegisterKind::CharacterWise) {
                // LineWise-incoming into CharacterWise-existing: concatenate
                // the first incoming line onto the last existing line, then
                // extend with the remaining incoming lines.
                if let Some(first) = other.lines.first() {
                    if let Some(last) = self.lines.last_mut() {
                        last.extend_from_slice(first);
                        self.lines.extend(other.lines.iter().skip(1).cloned());
                    } else {
                        self.lines.clone_from(&other.lines);
                    }
                }
            } else {
                self.lines.extend(other.lines.iter().cloned());
            }
            self.kind = RegisterKind::LineWise;
            return;
        }

        if matches!(self.kind, RegisterKind::CharacterWise) {
            let Some(first) = other.lines.first() else {
                return;
            };
            let Some(last) = self.lines.last_mut() else {
                self.lines.clone_from(&other.lines);
                return;
            };
            last.extend_from_slice(first);
            self.lines.extend(other.lines.iter().skip(1).cloned());
            return;
        }

        if let RegisterKind::BlockWise { width } = self.kind {
            let incoming_width = match other.kind {
                RegisterKind::BlockWise { width } => width,
                RegisterKind::CharacterWise | RegisterKind::LineWise => other
                    .lines
                    .iter()
                    .fold(0, |maximum, line| maximum.max(line.len())),
            };
            self.kind = RegisterKind::BlockWise {
                width: width.max(incoming_width),
            };
        }
        self.lines.extend(other.lines.iter().cloned());
    }

    fn append_from_setreg(&mut self, other: &Self) {
        let incoming_kind = other.kind;
        self.append(other);
        self.kind = incoming_kind;
    }
}

/// One of the two selection registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The primary selection (`*`).
    Primary,
    /// The clipboard selection (`+`).
    Clipboard,
}

/// What the unnamed register `"` reads through.
///
/// The unnamed register is a slot alias, not storage: it names the register a
/// previous write landed in (`y_previous`, `register.c:57`), and every read
/// resolves through that target, so overwriting the named slot later is
/// visible through `"` without another pointer update.
#[derive(Clone, Debug, PartialEq, Eq)]
enum UnnamedTarget {
    /// A locally stored slot addressed by its canonical register name.
    Slot(char),
    /// A `*`/`+` write. The value's owner is the clipboard provider, so the
    /// bank keeps the target name plus a snapshot of what this write
    /// published; the provider is not re-read on unnamed reads.
    ProviderSnapshot {
        /// Canonical selection register name (`*` or `+`).
        name: char,
        /// The value the write sent to the provider.
        content: RegisterContent,
    },
}

impl UnnamedTarget {
    /// The canonical single-character name of the target register.
    #[must_use]
    const fn name(&self) -> char {
        match self {
            Self::Slot(name) | Self::ProviderSnapshot { name, .. } => *name,
        }
    }
}

/// Host integration for the `*` and `+` registers.
///
/// The defaults deliberately expose no clipboard. Hosts may implement only the
/// operations they support.
pub trait ClipboardProvider {
    /// Reads a selection, or returns `None` when no provider/data is available.
    ///
    /// # Errors
    ///
    /// Implementations may return a [`RegisterError`] when the clipboard host
    /// fails; the default returns `Ok(None)`.
    fn get(&mut self, _selection: Selection) -> Result<Option<RegisterContent>, RegisterError> {
        Ok(None)
    }

    /// Writes a selection. The default is an unavailable-provider no-op.
    ///
    /// # Errors
    ///
    /// Implementations may return a [`RegisterError`] when the clipboard host
    /// fails; the default returns `Ok(())`.
    fn set(
        &mut self,
        _selection: Selection,
        _content: &RegisterContent,
    ) -> Result<(), RegisterError> {
        Ok(())
    }
}

/// Failures from register parsing, integration, or buffer insertion.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RegisterError {
    /// A register name is not part of the supported Vim register set.
    #[error("invalid register name {0:?}")]
    InvalidName(char),
    /// Register text was not valid UTF-8.
    #[error("register text must be valid UTF-8")]
    InvalidUtf8,
    /// A logical register line contained a line separator.
    #[error("a register line must not contain a newline")]
    NewlineInLine,
    /// The declared rectangle cannot contain all rows.
    #[error("invalid blockwise register width {width}")]
    InvalidBlockWidth {
        /// Declared rectangle width in bytes.
        width: usize,
    },
    /// The requested byte column lies beyond a line where padding is invalid.
    #[error("byte column {col} is outside line {lnum}, whose length is {line_len}")]
    ColumnOutOfBounds {
        /// One-based target line.
        lnum: usize,
        /// Zero-based target byte column.
        col: usize,
        /// Target line length in bytes.
        line_len: usize,
    },
    /// The requested byte column splits a UTF-8 code point.
    #[error("byte column {col} on line {lnum} is not a UTF-8 boundary")]
    NotCharBoundary {
        /// One-based target line.
        lnum: usize,
        /// Zero-based target byte column.
        col: usize,
    },
    /// Computing a blockwise target line overflowed.
    #[error("target line number overflow")]
    PositionOverflow,
    /// A read-only or externally-owned register was written without its host seam.
    #[error("register {0:?} requires a host provider")]
    ProviderRequired(char),
    /// The clipboard provider rejected an operation.
    #[error("clipboard provider failed: {0}")]
    Clipboard(String),
    /// The expression evaluator rejected an operation.
    #[error("expression evaluator failed: {0}")]
    Expression(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisterName {
    Named { index: usize, append: bool },
    Numbered(usize),
    Unnamed,
    SmallDelete,
    BlackHole,
    Expression,
    Selection(Selection),
    SearchPattern,
    CommandLine,
    InsertContent,
    AlternateFile,
    CurrentFile,
}

impl TryFrom<char> for RegisterName {
    type Error = RegisterError;

    fn try_from(name: char) -> Result<Self, Self::Error> {
        match name {
            'a'..='z' => Ok(Self::Named {
                index: usize::from(name as u8 - b'a'),
                append: false,
            }),
            'A'..='Z' => Ok(Self::Named {
                index: usize::from(name as u8 - b'A'),
                append: true,
            }),
            '0'..='9' => Ok(Self::Numbered(usize::from(name as u8 - b'0'))),
            '"' => Ok(Self::Unnamed),
            '-' => Ok(Self::SmallDelete),
            '_' => Ok(Self::BlackHole),
            '=' => Ok(Self::Expression),
            '*' => Ok(Self::Selection(Selection::Primary)),
            '+' => Ok(Self::Selection(Selection::Clipboard)),
            '/' => Ok(Self::SearchPattern),
            ':' => Ok(Self::CommandLine),
            '.' => Ok(Self::InsertContent),
            '#' => Ok(Self::AlternateFile),
            '%' => Ok(Self::CurrentFile),
            _ => Err(RegisterError::InvalidName(name)),
        }
    }
}

/// The canonical pointer name for a parsed register (`op_reg_index` plus
/// `get_register_name`, `register.h:31-74`): an uppercase write canonicalizes
/// to its lowercase slot, a `"` write selects register `0`, and `-`, `*`, `+`
/// name themselves. `None` marks registers the unnamed pointer cannot name —
/// the black hole and the read-only `/`, `=`, `.`, `:`, `%`, `#` all leave
/// the current target unchanged.
fn unnamed_pointer_name(name: RegisterName) -> Option<char> {
    match name {
        RegisterName::Named { index, .. } => Some((b'a' + u8::try_from(index).ok()?) as char),
        RegisterName::Numbered(index) => Some((b'0' + u8::try_from(index).ok()?) as char),
        RegisterName::SmallDelete => Some('-'),
        RegisterName::Unnamed => Some('0'),
        RegisterName::Selection(Selection::Primary) => Some('*'),
        RegisterName::Selection(Selection::Clipboard) => Some('+'),
        RegisterName::BlackHole
        | RegisterName::Expression
        | RegisterName::SearchPattern
        | RegisterName::CommandLine
        | RegisterName::InsertContent
        | RegisterName::AlternateFile
        | RegisterName::CurrentFile => None,
    }
}

/// The editor's local register bank.
///
/// Clipboard and expression values are resolved through host traits rather
/// than retained here, and the unnamed register is an alias over the slot a
/// previous write selected, never an independent copy. All mutation is
/// single-writer `&mut self` state.
pub struct Registers {
    named: [Option<RegisterContent>; 26],
    numbered: [Option<RegisterContent>; 10],
    unnamed_target: Option<UnnamedTarget>,
    small_delete: Option<RegisterContent>,
    expression_source: Option<Vec<u8>>,
    search_pattern: Option<RegisterContent>,
    command_line: Option<RegisterContent>,
    insert_content: Option<RegisterContent>,
    /// Raw text staged by a `#` write (`:let @# = ...`), resolved against
    /// live buffers at read time. `Some` empty text means cleared; `None`
    /// means no staged write, so reads fall through to the window's
    /// alternate buffer.
    alternate_override: Option<Vec<u8>>,
}

impl Default for Registers {
    fn default() -> Self {
        Self::new()
    }
}

impl Registers {
    /// Creates an empty register bank.
    #[must_use]
    pub fn new() -> Self {
        Self {
            named: std::array::from_fn(|_| None),
            numbered: std::array::from_fn(|_| None),
            unnamed_target: None,
            small_delete: None,
            expression_source: None,
            search_pattern: None,
            command_line: None,
            insert_content: None,
            alternate_override: None,
        }
    }

    /// Stages raw `#` text for later buffer resolution, or clears the
    /// staged write with `None`.
    pub fn set_alternate_override(&mut self, staged: Option<Vec<u8>>) {
        self.alternate_override = staged;
    }

    /// Returns raw `#` text staged by a write, if any.
    #[must_use]
    pub fn alternate_override(&self) -> Option<&[u8]> {
        self.alternate_override.as_deref()
    }

    /// Returns stored content. Provider-backed registers return `None`.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidName`] if `name` is not a supported
    /// register.
    pub fn get(&self, name: char) -> Result<Option<&RegisterContent>, RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Named { index, .. } => Ok(self.named.get(index).and_then(Option::as_ref)),
            RegisterName::Numbered(index) => Ok(self.numbered.get(index).and_then(Option::as_ref)),
            RegisterName::Unnamed => Ok(self.unnamed_slot()),
            RegisterName::SmallDelete => Ok(self.small_delete.as_ref()),
            RegisterName::SearchPattern => Ok(self.search_pattern.as_ref()),
            RegisterName::CommandLine => Ok(self.command_line.as_ref()),
            RegisterName::InsertContent => Ok(self.insert_content.as_ref()),
            RegisterName::BlackHole
            | RegisterName::Expression
            | RegisterName::Selection(_)
            | RegisterName::AlternateFile
            | RegisterName::CurrentFile => Ok(None),
        }
    }
    /// Stores content in a writable register.
    ///
    /// Uppercase names append to their lowercase register. Selection registers
    /// require [`Self::set_with_clipboard`]. The black-hole register discards
    /// the value successfully.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidName`] if `name` is not a supported
    /// register, or [`RegisterError::ProviderRequired`] for selection registers
    /// without a clipboard host.
    pub fn set(&mut self, name: char, content: RegisterContent) -> Result<(), RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Named { index, append } => {
                let Some(slot) = self.named.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                write_slot(slot, content, append);
                Ok(())
            }
            RegisterName::Numbered(index) => {
                let Some(slot) = self.numbered.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                *slot = Some(content);
                Ok(())
            }
            RegisterName::Unnamed => {
                // `':let @" = "val"' writes physical register 0 and points
                // the unnamed register there: `get_yank_register` selects
                // `y_regs[0]` for `"` and `finish_write_reg` keeps that
                // pointer (`register.c:362-373, 2685-2694`).
                if let Some(slot) = self.numbered.get_mut(0) {
                    *slot = Some(content);
                }
                self.unnamed_target = Some(UnnamedTarget::Slot('0'));
                Ok(())
            }
            RegisterName::SmallDelete => {
                self.small_delete = Some(content);
                Ok(())
            }
            RegisterName::BlackHole => Ok(()),
            RegisterName::Expression => {
                self.expression_source = Some(content.to_bytes());
                Ok(())
            }
            // An empty pattern clears the search register: upstream stores
            // no text (`set_last_search_pat("")`), so reads answer empty
            // instead of a lone line feed.
            RegisterName::SearchPattern => {
                if content.to_bytes().is_empty() {
                    self.search_pattern = None;
                } else {
                    self.search_pattern = Some(content);
                }
                Ok(())
            }
            // `.`, `:`, and `%` are readable but never writable: writing
            // them fails yank-register validation
            // (`valid_yank_reg(name, true)`, `register.c:153-165`), which
            // surfaces as E354.
            RegisterName::CommandLine
            | RegisterName::InsertContent
            | RegisterName::CurrentFile => Err(RegisterError::InvalidName(name)),
            // `#` resolves against live buffers at read time, so a write
            // only stages the raw text; the read side resolves it against
            // the buffer list or treats empty text as cleared.
            RegisterName::AlternateFile => {
                self.alternate_override = Some(content.to_bytes());
                Ok(())
            }
            RegisterName::Selection(_) => Err(RegisterError::ProviderRequired(name)),
        }
    }
    /// Stores content, forwarding selection registers to the clipboard host.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidName`] if `name` is not a supported
    /// register, or propagates a clipboard-provider failure.
    pub fn set_with_clipboard(
        &mut self,
        name: char,
        content: RegisterContent,
        clipboard: &mut dyn ClipboardProvider,
    ) -> Result<(), RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Selection(selection) => clipboard.set(selection, &content),
            _ => self.set(name, content),
        }
    }

    /// Records an ordinary yank in register `0` and points the unnamed
    /// register at it (`get_yank_register`'s `YREG_YANK` pointer update).
    pub fn yank(&mut self, content: RegisterContent) {
        if let Some(slot) = self.numbered.get_mut(0) {
            *slot = Some(content);
        }
        self.unnamed_target = Some(UnnamedTarget::Slot('0'));
    }

    /// Records a yank in an explicit register and points the unnamed register
    /// at the canonical destination: an uppercase write canonicalizes to its
    /// lowercase slot, a `"` write selects register `0`.
    ///
    /// The black-hole register discards the yank and leaves the target
    /// unchanged; non-pointable registers (`/`, `=`, `.`, `:`) store their
    /// value without moving it.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidName`] if `name` is not a supported
    /// register.
    pub fn yank_to(&mut self, name: char, content: RegisterContent) -> Result<(), RegisterError> {
        let parsed = RegisterName::try_from(name)?;
        if parsed == RegisterName::BlackHole {
            return Ok(());
        }
        self.set(name, content)?;
        if let Some(target) = unnamed_pointer_name(parsed) {
            self.unnamed_target = Some(UnnamedTarget::Slot(target));
        }
        Ok(())
    }

    /// Records a yank, forwarding selection registers to the clipboard host.
    ///
    /// A selection's value is owned by the provider, so the unnamed target
    /// keeps the selection name plus a snapshot of what this yank published;
    /// every other register points at its canonical slot.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidName`] if `name` is not a supported
    /// register, or propagates a clipboard-provider failure.
    pub fn yank_to_with_clipboard(
        &mut self,
        name: char,
        content: RegisterContent,
        clipboard: &mut dyn ClipboardProvider,
    ) -> Result<(), RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Selection(selection) => {
                clipboard.set(selection, &content)?;
                self.unnamed_target = Some(UnnamedTarget::ProviderSnapshot {
                    name: match selection {
                        Selection::Primary => '*',
                        Selection::Clipboard => '+',
                    },
                    content,
                });
                Ok(())
            }
            _ => self.yank_to(name, content),
        }
    }

    /// Records a delete using Vim's small-delete and numbered rotation rules.
    ///
    /// A one-row characterwise deletion uses `-`; every other deletion shifts
    /// registers `1` through `9`. Both paths point the unnamed register at
    /// the register that received the deletion (`shift`'s
    /// `y_previous = &y_regs[DELETION_REGISTER]` and
    /// `y_previous = &y_regs[1]` updates).
    pub fn delete(&mut self, content: RegisterContent) {
        let small = content.kind == RegisterKind::CharacterWise && content.lines.len() == 1;
        if small {
            self.small_delete = Some(content);
            self.unnamed_target = Some(UnnamedTarget::Slot('-'));
        } else {
            self.shift_numbered(content);
        }
    }

    /// Shifts registers `1` through `9` and records a big deletion in `1`
    /// (`shift_delete_registers`, `register.c:969-985`), pointing the
    /// unnamed register there.
    fn shift_numbered(&mut self, content: RegisterContent) {
        for destination in (2..=9).rev() {
            let source = destination - 1;
            let shifted = self.numbered.get(source).cloned().flatten();
            if let Some(slot) = self.numbered.get_mut(destination) {
                *slot = shifted;
            }
        }
        if let Some(slot) = self.numbered.get_mut(1) {
            *slot = Some(content);
        }
        self.unnamed_target = Some(UnnamedTarget::Slot('1'));
    }

    /// Records a delete in an explicit register and updates unnamed.
    ///
    /// A linewise or multi-line deletion also shifts the numbered registers
    /// (`op_delete`, `ops.c:808-814`): the text lands in the named slot and
    /// in `1`. Single-line characterwise deletions stay in the named slot
    /// only. Motion-driven `use_reg_one` shifts for single-line `%`, `(`, or
    /// search motions are not representable here — the model never sees the
    /// motion — so those stay residual. The black hole discards everything
    /// without shifting.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidName`] if `name` is not a supported
    /// register.
    pub fn delete_to(&mut self, name: char, content: RegisterContent) -> Result<(), RegisterError> {
        let shift = name != '_'
            && (content.kind == RegisterKind::LineWise || content.lines.len() > 1);
        self.yank_to(name, content.clone())?;
        if shift {
            self.shift_numbered(content);
        }
        Ok(())
    }

    /// The canonical single-character name of the register the unnamed
    /// register points at, or `'"'` when nothing has been written yet — the
    /// same answer `get_register_name(get_unname_register())` gives upstream.
    #[must_use]
    pub fn unnamed_target_name(&self) -> char {
        self.unnamed_target
            .as_ref()
            .map_or('"', UnnamedTarget::name)
    }

    /// Points the unnamed register at `name`'s slot
    /// (`op_reg_set_previous`, `register.c:298-307`): digits, letters
    /// (canonicalized to lowercase), `-`, `*`, and `+` are pointable. Any
    /// other name — the black hole, the read-only `/`, `=`, `.`, `:`, `%`,
    /// `#`, or an unsupported character — leaves the current target
    /// unchanged, like upstream's failed index lookup.
    pub fn set_unnamed_target(&mut self, name: char) {
        if let Ok(parsed) = RegisterName::try_from(name)
            && let Some(target) = unnamed_pointer_name(parsed)
        {
            self.unnamed_target = Some(UnnamedTarget::Slot(target));
        }
    }

    /// Resolves the unnamed register through its current target: a slot
    /// target re-reads the named slot, so overwriting that slot later is
    /// visible through `"` without another pointer update.
    fn unnamed_slot(&self) -> Option<&RegisterContent> {
        match self.unnamed_target.as_ref()? {
            UnnamedTarget::Slot(name) => self.get(*name).ok().flatten(),
            UnnamedTarget::ProviderSnapshot { content, .. } => Some(content),
        }
    }

    /// Returns the expression register's source text, if any (`getreg('=')`).
    #[must_use]
    pub fn expression_source(&self) -> Option<&[u8]> {
        self.expression_source.as_deref()
    }

    /// Stores `setreg()` content, whose append operation adopts the incoming
    /// type instead of preserving the yank type already in the slot.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidName`] if `name` is not a supported
    /// register.
    pub fn set_from_setreg(
        &mut self,
        name: char,
        content: RegisterContent,
        append: bool,
    ) -> Result<(), RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Named {
                index,
                append: uppercase,
            } if append || uppercase => {
                let Some(slot) = self.named.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                append_slot_from_setreg(slot, content);
                Ok(())
            }
            RegisterName::Named { index, .. } => {
                let Some(slot) = self.named.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                append_slot_from_setreg_if(slot, content, append);
                Ok(())
            }
            RegisterName::Numbered(index) => {
                let Some(slot) = self.numbered.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                append_slot_from_setreg_if(slot, content, append);
                Ok(())
            }
            RegisterName::Unnamed => {
                let Some(slot) = self.numbered.get_mut(0) else {
                    return Err(RegisterError::InvalidName(name));
                };
                append_slot_from_setreg_if(slot, content, append);
                self.unnamed_target = Some(UnnamedTarget::Slot('0'));
                Ok(())
            }
            RegisterName::SmallDelete => {
                append_slot_from_setreg_if(&mut self.small_delete, content, append);
                Ok(())
            }
            RegisterName::Expression => {
                let incoming = content.to_bytes();
                if append {
                    if let Some(existing) = &mut self.expression_source {
                        existing.extend_from_slice(&incoming);
                    } else {
                        self.expression_source = Some(incoming);
                    }
                } else {
                    self.expression_source = Some(incoming);
                }
                Ok(())
            }
            // `write_reg_contents_ex` ignores the append flag for `/`
            // (`register.c:2755-2766`): setting the search pattern always
            // replaces it, and empty text clears it.
            RegisterName::SearchPattern => {
                if content.to_bytes().is_empty() {
                    self.search_pattern = None;
                } else {
                    self.search_pattern = Some(content);
                }
                Ok(())
            }
            // `.`, `:`, `%`, `#`, and the selections fall through to
            // `set`, which stages `#`, rejects the read-only names, and
            // demands a provider for `*`/`+`.
            _ => self.set(name, content),
        }
    }

    /// Clears a register the way `write_reg_contents_lst` with an empty list
    /// does (`eval/funcs.c:6461-6468`): `/` and `=` take one empty line, the
    /// staged `#` text is dropped (the caller clears the window's alternate
    /// buffer, which needs host access), and every other writable slot is
    /// unset so reads answer empty.
    ///
    /// # Errors
    ///
    /// Returns [`RegisterError::InvalidName`] if `name` is not a supported or
    /// writable register, or [`RegisterError::ProviderRequired`] for selection
    /// registers without a clipboard host.
    pub fn clear_for_setreg(&mut self, name: char) -> Result<(), RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Named { index, .. } => {
                let Some(slot) = self.named.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                *slot = None;
                Ok(())
            }
            RegisterName::Numbered(index) => {
                let Some(slot) = self.numbered.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                *slot = None;
                Ok(())
            }
            RegisterName::Unnamed => {
                if let Some(slot) = self.numbered.get_mut(0) {
                    *slot = None;
                }
                self.unnamed_target = Some(UnnamedTarget::Slot('0'));
                Ok(())
            }
            RegisterName::SmallDelete => {
                self.small_delete = None;
                Ok(())
            }
            RegisterName::BlackHole => Ok(()),
            RegisterName::Expression => {
                self.expression_source = Some(Vec::new());
                Ok(())
            }
            RegisterName::SearchPattern => {
                self.search_pattern = None;
                Ok(())
            }
            RegisterName::AlternateFile => {
                self.alternate_override = None;
                Ok(())
            }
            RegisterName::CommandLine
            | RegisterName::InsertContent
            | RegisterName::CurrentFile => Err(RegisterError::InvalidName(name)),
            RegisterName::Selection(_) => Err(RegisterError::ProviderRequired(name)),
        }
    }
}

fn append_slot_from_setreg(slot: &mut Option<RegisterContent>, content: RegisterContent) {
    if let Some(existing) = slot {
        existing.append_from_setreg(&content);
    } else {
        *slot = Some(content);
    }
}

fn append_slot_from_setreg_if(
    slot: &mut Option<RegisterContent>,
    content: RegisterContent,
    append: bool,
) {
    if append {
        append_slot_from_setreg(slot, content);
    } else {
        *slot = Some(content);
    }
}

fn validate_register_line(line: &[u8]) -> Result<(), RegisterError> {
    if line.contains(&b'\n') {
        return Err(RegisterError::NewlineInLine);
    }
    std::str::from_utf8(line)
        .map(|_| ())
        .map_err(|_| RegisterError::InvalidUtf8)
}

fn write_slot(slot: &mut Option<RegisterContent>, content: RegisterContent, append: bool) {
    if append && let Some(existing) = slot {
        existing.append(&content);
        return;
    }
    *slot = Some(content);
}
