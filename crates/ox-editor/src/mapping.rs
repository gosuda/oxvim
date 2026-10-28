//! Mode-aware mappings and insert-mode abbreviations.
//!
//! Local-first and longest-prefix behavior follows `src/nvim/input.c:2319-2438`.
//! Abbreviation validation and word-boundary scanning follow
//! `src/nvim/mapping.c:624-629,1455-1529`.
//!
//! Abbreviations ride this same engine the way upstream's `buf_do_map` does:
//! they are `mapblock_T` entries with `m_mode` restricted to the
//! insert/command-line pair (`abbrev_modes_for_command`), `do_exmap`'s
//! `MAPTYPE_*` classification (`classify_abbrev_command`), and the listing,
//! `maparg`, `mapcheck` and `hasmapto` reporting the `:map` family shares.
//! Expansion itself (`check_abbr`, `mapping.c:1505-1660`) is an insert- and
//! command-line-layer concern that reads back through
//! [`Mappings::lookup_abbreviation_in_mode`]; the engine only stores the
//! mode-filtered table and the word-boundary rules here.

use std::ops::{BitOr, BitOrAssign};

use ox_excmd::{ExCommand, ParseError, Parser};
use ox_types::BufHandle;
use thiserror::Error;

use crate::script::SourceContext;
use crate::typeahead::{Keys, Typeahead, special_notation};

/// One mapping mode accepted by the `:map` family.
///
/// The discriminants are upstream's `MODE_*` bits (`state_defs.h:21-28`), not a
/// private numbering: `maparg()`'s `mode_bits` key reports them verbatim
/// (`mapblock_fill_dict`, `mapping.c:2143`), so any other assignment would
/// need a translation table beside them that can drift.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u16)]
pub enum MapMode {
    /// Normal mode (`n`), `MODE_NORMAL`.
    Normal = 0x01,
    /// Visual mode (`x`), `MODE_VISUAL`.
    Visual = 0x02,
    /// Operator-pending mode (`o`), `MODE_OP_PENDING`.
    OperatorPending = 0x04,
    /// Command-line mode (`c`), `MODE_CMDLINE`.
    CommandLine = 0x08,
    /// Insert mode (`i`), `MODE_INSERT`.
    Insert = 0x10,
    /// Language-argument mode (`l`), `MODE_LANGMAP`.
    LangArg = 0x20,
    /// Select mode (`s`), `MODE_SELECT`.
    Select = 0x40,
    /// Terminal mode (`t`), `MODE_TERMINAL`.
    Terminal = 0x80,
}

/// Compact set of mapping modes.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct MapModes(u16);

impl MapModes {
    /// Empty mode set.
    pub const NONE: Self = Self(0);
    /// Every supported mode.
    pub const ALL: Self = Self(0xff);
    /// Traditional `:map` modes.
    pub const MAP: Self = Self(
        MapMode::Normal as u16
            | MapMode::Visual as u16
            | MapMode::Select as u16
            | MapMode::OperatorPending as u16,
    );
    /// Traditional `:map!` modes.
    pub const MAP_BANG: Self = Self(MapMode::Insert as u16 | MapMode::CommandLine as u16);

    /// Creates a singleton mode set.
    #[must_use]
    pub const fn one(mode: MapMode) -> Self {
        Self(mode as u16)
    }

    /// Whether this set contains a mode.
    #[must_use]
    pub const fn contains(self, mode: MapMode) -> bool {
        self.0 & mode as u16 != 0
    }

    /// Whether two mode sets overlap.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Removes every mode in `other`.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether no modes are present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw `MODE_*` bit set, which `maparg()` reports as `mode_bits`
    /// (`mapping.c:2143`).
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// `map_mode_to_chars` (`mapping.c:170-208`): the mode characters
    /// `:map`'s listing and `maparg()`'s `mode` key show. Never longer than
    /// six characters, hence upstream's seven-byte buffer.
    #[must_use]
    pub fn to_chars(self) -> String {
        let mut out = String::with_capacity(6);
        if self.contains(MapMode::Insert) && self.contains(MapMode::CommandLine) {
            out.push('!');
        } else if self.contains(MapMode::Insert) {
            out.push('i');
        } else if self.contains(MapMode::LangArg) {
            out.push('l');
        } else if self.contains(MapMode::CommandLine) {
            out.push('c');
        } else if Self(self.0 & Self::MAP.0) == Self::MAP {
            out.push(' ');
        } else {
            if self.contains(MapMode::Normal) {
                out.push('n');
            }
            if self.contains(MapMode::OperatorPending) {
                out.push('o');
            }
            if self.contains(MapMode::Terminal) {
                out.push('t');
            }
            if self.contains(MapMode::Visual) && self.contains(MapMode::Select) {
                out.push('v');
            } else {
                if self.contains(MapMode::Visual) {
                    out.push('x');
                }
                if self.contains(MapMode::Select) {
                    out.push('s');
                }
            }
        }
        out
    }

    /// `get_map_mode` (`mapping.c:988-1023`) over a mode string rather than a
    /// command name: only the first character decides, an unrecognized or
    /// empty string means `:map`, and `n` followed by `o` is `:noremap`
    /// rather than `:nmap`.
    #[must_use]
    pub fn from_mode_string(mode: &str) -> Self {
        let bytes = mode.as_bytes();
        match bytes.first().copied() {
            Some(b'i') => Self::one(MapMode::Insert),
            Some(b'l') => Self::one(MapMode::LangArg),
            Some(b'c') => Self::one(MapMode::CommandLine),
            Some(b'n') if bytes.get(1) != Some(&b'o') => Self::one(MapMode::Normal),
            Some(b'v') => Self::one(MapMode::Visual) | Self::one(MapMode::Select),
            Some(b'x') => Self::one(MapMode::Visual),
            Some(b's') => Self::one(MapMode::Select),
            Some(b'o') => Self::one(MapMode::OperatorPending),
            Some(b't') => Self::one(MapMode::Terminal),
            _ => Self::MAP,
        }
    }

    /// `MAP_HASH` (`mapping.c:75-78`): the `maphash[]` bucket a mapping with
    /// this mode set and first lhs byte lives in. `:map`'s listing walks the
    /// buckets in ascending order, so this is the primary sort key of every
    /// listing.
    #[must_use]
    pub const fn hash_bucket(self, first: u8) -> u16 {
        const HASHED: u16 = MapMode::Normal as u16
            | MapMode::Visual as u16
            | MapMode::Select as u16
            | MapMode::OperatorPending as u16
            | MapMode::Terminal as u16;
        if self.0 & HASHED != 0 {
            first as u16
        } else {
            first as u16 ^ 0x80
        }
    }
}

impl From<MapMode> for MapModes {
    fn from(value: MapMode) -> Self {
        Self::one(value)
    }
}

impl BitOr for MapModes {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOr<MapMode> for MapMode {
    type Output = MapModes;

    fn bitor(self, rhs: MapMode) -> Self::Output {
        MapModes::one(self) | MapModes::one(rhs)
    }
}

impl BitOrAssign for MapModes {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Global or buffer-local mapping scope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MapScope {
    /// Process-editor global mapping.
    #[default]
    Global,
    /// Mapping visible only while one buffer is current.
    Buffer(BufHandle),
}

/// Deferred right-hand-side behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MappingAction {
    /// Encoded replacement keys.
    Keys(Keys),
    /// Parsed `<Cmd>...<CR>` or `:...<CR>` Ex commands.
    ///
    /// `keys` is the same right-hand side as `commands` before it was parsed —
    /// upstream's `m_str`, which `maparg()`'s string form and `:map`'s listing
    /// both render. Parsing loses it (a `Vec<ExCommand>` does not print back
    /// to its source text), so it is carried here rather than reconstructed.
    ExCommands {
        /// The right-hand side after `replace_termcodes`, upstream's `m_str`.
        keys: Keys,
        /// The same text parsed into the Ex commands the mapping runs.
        commands: Vec<ExCommand>,
    },
    /// `<expr>` right-hand side, re-evaluated into replacement keys on every
    /// use (`str_to_mapargs`'s `expr` flag, `mapping.c:439-443`).
    Expr(String),
    /// Host callback identity.
    Callback(u64),
    /// `<Nop>` consumes the lhs without producing input.
    Nop,
}
impl MappingAction {
    /// Parses command-shaped mapping right-hand sides and decodes all others
    /// as key notation ([`Keys::parse_notation`]).
    ///
    /// The `<Cmd>`/`:` forms are recognized *before* the notation pass, so the
    /// `<CR>` that terminates them stays a terminator rather than becoming a
    /// carriage return inside the command text.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::ExCommand`] when a command-shaped right-hand
    /// side contains invalid Ex syntax.
    pub fn parse_rhs(rhs: &str, leader: &str, local_leader: &str) -> Result<Self, MappingError> {
        if rhs.eq_ignore_ascii_case("<nop>") {
            return Ok(Self::Nop);
        }
        let command = rhs
            .strip_prefix("<Cmd>")
            .and_then(|body| body.strip_suffix("<CR>"))
            .or_else(|| {
                rhs.strip_prefix(':')
                    .and_then(|body| body.strip_suffix("<CR>"))
            });
        // A `:…<CR>` right-hand side runs as typed keys: `:` opens the
        // cmdline, `<C-U>` clears it, the text types itself. The pre-parsed
        // form is only a fast path for bodies that are pure Ex commands;
        // anything carrying key notation inside (`:<C-U>call f()<CR>`,
        // matchit's maps) falls back to keys, which is upstream's one true
        // execution model anyway.
        if let Some(command) = command
            && let Ok(commands) = Parser::new().parse(command)
        {
            return Ok(Self::ExCommands {
                keys: Keys::parse_notation(rhs, leader, local_leader),
                commands,
            });
        }
        Ok(Self::Keys(Keys::parse_notation(rhs, leader, local_leader)))
    }

    /// The right-hand side after `replace_termcodes`, upstream's
    /// `mapblock_T.m_str`, which `maparg()`'s string form and `:map`'s listing
    /// render through `str2special`.
    ///
    /// `None` is upstream's `m_luaref != LUA_NOREF`: a callback has no key
    /// string at all.
    #[must_use]
    pub fn replaced_keys(&self) -> Option<&[u8]> {
        match self {
            Self::Keys(keys) | Self::ExCommands { keys, .. } => Some(keys.as_bytes()),
            // `<expr>` stores the expression itself as `m_str` (`do_map` runs
            // `replace_termcodes` over every right-hand side before
            // `map_add`), so it renders the same way.
            Self::Expr(text) => Some(text.as_bytes()),
            // `<Nop>` is an empty `m_str`, which is what makes `showmap` and
            // `get_maparg` print the literal `<Nop>` for it.
            Self::Nop => Some(&[]),
            Self::Callback(_) => None,
        }
    }
}

/// Independently combinable mapping registration flags.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MapFlags(u8);

impl MapFlags {
    /// Produced keys may themselves be mapped. Enabled by default;
    /// `:noremap` clears it.
    pub const REMAP: Self = Self(1 << 0);
    /// Prefer a complete match immediately despite longer candidates.
    pub const NOWAIT: Self = Self(1 << 1);
    /// Suppress command echo while the mapping runs.
    pub const SILENT: Self = Self(1 << 2);
    /// Whether `<script>` was given (`REMAP_SCRIPT`, `mapping.c:2108,2129`).
    ///
    /// It restricts remapping to `<SID>` mappings, and this port has no
    /// script-local mappings, so execution folds it into [`MapFlags::REMAP`]
    /// being clear. The flag is still recorded because `maparg()`'s `script`
    /// key is the only thing that can tell `<script>` from `:noremap` apart.
    pub const SCRIPT: Self = Self(1 << 3);

    /// Whether every flag in `other` is enabled.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Enables or disables `flag` without touching the other bits.
    pub const fn set(&mut self, flag: Self, enabled: bool) {
        if enabled {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

/// Everything `:map` recorded about one mapping besides its lhs and its
/// decoded action: the flags the input loop reads, and the text and script
/// context `maparg()` and `:map`'s listing report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MappingOptions {
    /// Active modes.
    pub modes: MapModes,
    /// Global or buffer-local scope.
    pub scope: MapScope,
    /// Registration and execution flags (`MapFlags`).
    pub flags: MapFlags,
    /// Optional user-facing description.
    pub description: Option<String>,
    /// The right-hand side exactly as written, before `<>` notation was
    /// decoded (`mapblock_T.m_orig_str`), which is what `maparg()`'s `rhs`
    /// key reports in its compatible form (`mapping.c:2114-2117`).
    pub orig_rhs: String,
    /// Script context of the `:map` that defined this mapping
    /// (`mapblock_T.m_script_ctx`), reported as `maparg()`'s `sid` and `lnum`.
    /// All zeroes when no script was sourcing, as upstream's `current_sctx` is
    /// then at the command line.
    pub script_context: SourceContext,
}

impl Default for MappingOptions {
    fn default() -> Self {
        Self {
            modes: MapModes::MAP,
            scope: MapScope::Global,
            flags: MapFlags::REMAP,
            description: None,
            orig_rhs: String::new(),
            script_context: SourceContext::default(),
        }
    }
}

/// One registered mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mapping {
    /// Encoded left-hand side.
    pub lhs: Keys,
    /// Deferred right-hand-side action.
    pub action: MappingAction,
    /// Registration flags.
    pub options: MappingOptions,
    sequence: u64,
}

/// Prefix lookup result for the input loop's timeout decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lookup<'a> {
    /// Longest complete mapping; number of encoded bytes to consume.
    Exact(&'a Mapping, usize),
    /// More bytes may select a longer mapping. A complete fallback may be present.
    Prefix(Option<&'a Mapping>),
    /// No mapping shares the queued prefix.
    None,
}

/// One abbreviation entry: upstream's `mapblock_T` on the abbreviation list.
///
/// `modes` is always a non-empty subset of insert/command-line
/// ([`MapModes::MAP_BANG`], [`MapMode::Insert`], or [`MapMode::CommandLine`]),
/// which is what lets insert-mode and command-line expansion share one table
/// while `maparg`'s `mode` key and `:abbreviate`'s listing still tell `i`,
/// `c` and `!` apart (`map_mode_to_chars`, `mapping.c:170-208`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Abbreviation {
    /// Literal trigger text, already through `replace_termcodes`.
    pub lhs: String,
    /// Deferred replacement behavior.
    pub action: MappingAction,
    /// Insert/command-line mode subset this entry answers in.
    pub modes: MapModes,
    /// Global or buffer-local scope.
    pub scope: MapScope,
    /// Registration and execution flags: `REMAP` clear means `:noreabbrev`,
    /// `SCRIPT` records `<script>`, `SILENT` quiets the expansion.
    pub flags: MapFlags,
    /// The right-hand side exactly as written, which `maparg()`'s `rhs` key
    /// and the string form report (`mapblock_fill_dict`, `mapping.c:2090`).
    pub orig_rhs: String,
    /// Script context of the `:abbreviate` that defined this entry, reported
    /// as `maparg()`'s `sid` and `lnum`.
    pub script_context: SourceContext,
    sequence: u64,
}

/// Everything `:abbreviate` records besides the trigger text and its decoded
/// action: the mode/scope/flags the expansion layer reads, and the text and
/// script context `maparg()` reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbbrevOptions {
    /// Insert/command-line mode subset.
    pub modes: MapModes,
    /// Global or buffer-local scope.
    pub scope: MapScope,
    /// Registration and execution flags.
    pub flags: MapFlags,
    /// The right-hand side exactly as written.
    pub orig_rhs: String,
    /// Script context of the defining command.
    pub script_context: SourceContext,
}

impl Default for AbbrevOptions {
    fn default() -> Self {
        Self {
            modes: MapModes::MAP_BANG,
            scope: MapScope::Global,
            flags: MapFlags::REMAP,
            orig_rhs: String::new(),
            script_context: SourceContext::default(),
        }
    }
}

/// What an abbreviation-family command name asks for: upstream `do_exmap`'s
/// `MAPTYPE_*` read off the command name (`mapping.c:2675-2684`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbbrevCommandKind {
    /// `:abbreviate` and its `i`/`c`/`nore` forms: define or list.
    Define {
        /// Whether the name carries `nore` (`MAPTYPE_NOREMAP`).
        noremap: bool,
    },
    /// `:unabbreviate` and its `i`/`c` forms (`MAPTYPE_UNMAP`).
    Undefine,
    /// `:abclear` and its `i`/`c` forms.
    Clear,
}

/// Classifies an abbreviation-family command name (`do_exmap`, `do_mapclear`).
///
/// `abclear` suffices for the clear family, `unabbrev` (also the prefix of
/// `unabbreviate`) for the undefine family, and `nore` for the no-remap
/// define forms — none of the twelve canonical names contains another
/// family's marker.
#[must_use]
pub fn classify_abbrev_command(name: &str) -> AbbrevCommandKind {
    if name.ends_with("abclear") {
        AbbrevCommandKind::Clear
    } else if name.contains("unabbrev") {
        AbbrevCommandKind::Undefine
    } else {
        AbbrevCommandKind::Define {
            noremap: name.contains("nore"),
        }
    }
}

/// Whether `name` is one of the abbreviation-family commands routed beside
/// the `:map` family. `"iabbrev"` and `"abclear"` keep their historic dispatch
/// arms; the other ten canonical names route through the `:map` family arm.
#[must_use]
pub fn is_abbrev_command(name: &str) -> bool {
    matches!(
        name,
        "abbreviate"
            | "cabbrev"
            | "cabclear"
            | "cnoreabbrev"
            | "cunabbrev"
            | "iabclear"
            | "inoreabbrev"
            | "iunabbrev"
            | "noreabbrev"
            | "unabbreviate"
    )
}

/// The mode subset an abbreviation-family command name defines, lists, or
/// clears: upstream `get_map_mode` over the command name with
/// `forceit || is_abbrev` set (`mapping.c:1026-1063,2677`), so a bare
/// `:abbreviate` covers insert and command-line while the `i`/`c` prefixes
/// narrow to one.
#[must_use]
pub fn abbrev_modes_for_command(name: &str) -> MapModes {
    match name.as_bytes().first() {
        Some(b'i') => MapModes::one(MapMode::Insert),
        Some(b'c') => MapModes::one(MapMode::CommandLine),
        _ => MapModes::MAP_BANG,
    }
}

/// Invalid mapping or abbreviation request.
#[derive(Debug, Error)]
pub enum MappingError {
    /// Mapping lhs was empty.
    #[error("mapping lhs must not be empty")]
    EmptyLhs,
    /// No modes were selected.
    #[error("mapping mode set must not be empty")]
    EmptyModes,
    /// Abbreviation contains whitespace or crosses keyword classes.
    #[error("invalid abbreviation lhs {0:?}")]
    InvalidAbbreviation(String),
    /// Command-shaped rhs did not parse.
    #[error(transparent)]
    ExCommand(#[from] ParseError),
}

/// Editor-owned mapping and abbreviation tables.
#[derive(Clone, Debug)]
pub struct Mappings {
    entries: Vec<Mapping>,
    abbreviations: Vec<Abbreviation>,
    next_sequence: u64,
    timeout_len_ms: u32,
}

impl Default for Mappings {
    fn default() -> Self {
        Self::new()
    }
}

impl Mappings {
    /// Creates empty tables with upstream's default `timeoutlen` data value.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            abbreviations: Vec::new(),
            next_sequence: 1,
            timeout_len_ms: 1_000,
        }
    }

    /// Current mapping ambiguity timeout. Timer wiring belongs to the input loop.
    #[must_use]
    pub const fn timeout_len_ms(&self) -> u32 {
        self.timeout_len_ms
    }

    /// Updates timeout data without starting a timer.
    pub const fn set_timeout_len_ms(&mut self, value: u32) {
        self.timeout_len_ms = value;
    }

    /// Whether a mapping with exactly this lhs already covers any of `modes`
    /// in `scope`.
    ///
    /// `<unique>`'s rejection (`do_map`'s `retval = 5`, `mapping.c:802`, which
    /// becomes `E227`). Upstream also rejects a *prefix* overlap and a
    /// buffer-local definition shadowed by a global one; this answers the
    /// exact-lhs case, which is the one `:map <unique>` is written for.
    #[must_use]
    pub fn conflicts(&self, lhs: &Keys, modes: MapModes, scope: MapScope) -> bool {
        self.entries.iter().any(|mapping| {
            &mapping.lhs == lhs
                && mapping.options.scope == scope
                && mapping.options.modes.intersects(modes)
        })
    }

    /// Defines or replaces overlapping mode bits for one lhs and scope.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::EmptyLhs`] when `lhs` is empty, or
    /// [`MappingError::EmptyModes`] when `options` selects no modes.
    pub fn map(
        &mut self,
        lhs: Keys,
        action: MappingAction,
        options: MappingOptions,
    ) -> Result<(), MappingError> {
        if lhs.is_empty() {
            return Err(MappingError::EmptyLhs);
        }
        if options.modes.is_empty() {
            return Err(MappingError::EmptyModes);
        }
        for mapping in &mut self.entries {
            if mapping.lhs == lhs && mapping.options.scope == options.scope {
                mapping.options.modes = mapping.options.modes.without(options.modes);
            }
        }
        self.entries
            .retain(|mapping| !mapping.options.modes.is_empty());
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.entries.push(Mapping {
            lhs,
            action,
            options,
            sequence,
        });
        Ok(())
    }

    /// Defines a noremap without a boolean selector at call sites.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::EmptyLhs`] when `lhs` is empty, or
    /// [`MappingError::EmptyModes`] when `options` selects no modes.
    pub fn noremap(
        &mut self,
        lhs: Keys,
        action: MappingAction,
        mut options: MappingOptions,
    ) -> Result<(), MappingError> {
        options.flags.set(MapFlags::REMAP, false);
        self.map(lhs, action, options)
    }

    /// Removes matching mode bits from one lhs and scope.
    pub fn unmap(&mut self, lhs: &Keys, modes: MapModes, scope: MapScope) -> usize {
        let mut changed = 0;
        for mapping in &mut self.entries {
            if &mapping.lhs == lhs
                && mapping.options.scope == scope
                && mapping.options.modes.intersects(modes)
            {
                mapping.options.modes = mapping.options.modes.without(modes);
                changed += 1;
            }
        }
        self.entries
            .retain(|mapping| !mapping.options.modes.is_empty());
        changed
    }

    /// Implements `mapclear` for selected modes and one scope.
    pub fn mapclear(&mut self, modes: MapModes, scope: MapScope) -> usize {
        let mut changed = 0;
        for mapping in &mut self.entries {
            if mapping.options.scope == scope && mapping.options.modes.intersects(modes) {
                mapping.options.modes = mapping.options.modes.without(modes);
                changed += 1;
            }
        }
        self.entries
            .retain(|mapping| !mapping.options.modes.is_empty());
        changed
    }

    /// `check_map` with `exact` set (`mapping.c:2010-2061`): the mapping whose
    /// lhs is exactly `lhs` and whose mode set overlaps `modes`, buffer-local
    /// table searched before the global one. The `bool` is upstream's
    /// `local_ptr`, which `maparg()` reports as `buffer`.
    ///
    /// Upstream returns the *first* hit in hash order rather than the longest
    /// or newest; with an exact length test at most one entry per scope can
    /// match, because [`Self::map`] never keeps two entries with the same lhs,
    /// scope and overlapping modes.
    #[must_use]
    pub fn find_exact(
        &self,
        lhs: &[u8],
        modes: MapModes,
        buffer: Option<BufHandle>,
    ) -> Option<(&Mapping, bool)> {
        buffer
            .and_then(|buffer| {
                self.exact_in_scope(lhs, modes, MapScope::Buffer(buffer))
                    .map(|mapping| (mapping, true))
            })
            .or_else(|| {
                self.exact_in_scope(lhs, modes, MapScope::Global)
                    .map(|mapping| (mapping, false))
            })
    }

    /// `do_map`'s listing passes (`mapping.c:698-726,746-793`): every mapping
    /// whose mode set overlaps `modes` and whose lhs and `lhs` are a prefix of
    /// one another — upstream compares only `min(keylen, len)` bytes, so an
    /// empty `lhs` matches everything.
    ///
    /// The order is the order upstream prints in: the buffer-local table
    /// first, then within each table the `maphash[]` buckets ascending
    /// ([`MapModes::hash_bucket`]) and, inside one bucket, newest first
    /// because `map_add` pushes onto the bucket head (`mapping.c:545-547`).
    #[must_use]
    pub fn matching(
        &self,
        lhs: &[u8],
        modes: MapModes,
        buffer: Option<BufHandle>,
    ) -> Vec<(&Mapping, bool)> {
        let mut found: Vec<(&Mapping, bool)> = self
            .entries
            .iter()
            .filter(|mapping| mapping.options.modes.intersects(modes))
            .filter(|mapping| {
                let keys = mapping.lhs.as_bytes();
                let shared = keys.len().min(lhs.len());
                keys[..shared] == lhs[..shared]
            })
            .filter_map(|mapping| match mapping.options.scope {
                MapScope::Global => Some((mapping, false)),
                MapScope::Buffer(handle) => (Some(handle) == buffer).then_some((mapping, true)),
            })
            .collect();
        found.sort_by_key(|(mapping, local)| {
            let bucket = mapping
                .lhs
                .as_bytes()
                .first()
                .map_or(0, |first| mapping.options.modes.hash_bucket(*first));
            (!*local, bucket, u64::MAX - mapping.sequence)
        });
        found
    }

    fn exact_in_scope(&self, lhs: &[u8], modes: MapModes, scope: MapScope) -> Option<&Mapping> {
        self.entries.iter().find(|mapping| {
            mapping.options.scope == scope
                && mapping.options.modes.intersects(modes)
                && mapping.lhs.as_bytes() == lhs
        })
    }

    /// Looks up the typeahead bytes using local-first, longest-prefix rules.
    #[must_use]
    pub fn lookup(&self, typeahead: &[u8], mode: MapMode, buffer: Option<BufHandle>) -> Lookup<'_> {
        if let Some(buffer) = buffer {
            let local = self.lookup_scope(typeahead, mode, MapScope::Buffer(buffer));
            if local != Lookup::None {
                return local;
            }
        }
        self.lookup_scope(typeahead, mode, MapScope::Global)
    }

    /// Convenience lookup against the editor's typeahead stack.
    #[must_use]
    pub fn lookup_typeahead(
        &self,
        typeahead: &Typeahead,
        mode: MapMode,
        buffer: Option<BufHandle>,
    ) -> Lookup<'_> {
        self.lookup(typeahead.as_bytes(), mode, buffer)
    }

    /// Registers an abbreviation with upstream's default `:abbreviate` shape:
    /// both insert and command-line modes, remappable per `remap`, no script
    /// context. New command code prefers [`Self::abbreviate_full`].
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::InvalidAbbreviation`] when `lhs` is empty,
    /// overlong, contains whitespace, or crosses keyword-character classes.
    pub fn abbreviate(
        &mut self,
        lhs: &str,
        action: MappingAction,
        scope: MapScope,
        remap: bool,
    ) -> Result<(), MappingError> {
        let mut flags = MapFlags::default();
        flags.set(MapFlags::REMAP, remap);
        let orig_rhs = action
            .replaced_keys()
            .map(|keys| String::from_utf8_lossy(keys).into_owned())
            .unwrap_or_default();
        self.abbreviate_full(
            lhs,
            action,
            AbbrevOptions {
                modes: MapModes::MAP_BANG,
                scope,
                flags,
                orig_rhs,
                script_context: SourceContext::default(),
            },
        )
    }

    /// Defines an abbreviation the way `buf_do_map` adds one
    /// (`mapping.c:602-958`): overlapping mode bits for the same trigger and
    /// scope are stripped first, so a redefinition replaces exactly the modes
    /// the command names.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::EmptyModes`] when `options` selects no modes,
    /// or [`MappingError::InvalidAbbreviation`] when `lhs` is empty, overlong
    /// (`MAXMAPLEN`, `mapping_defs.h:7`), contains whitespace, or crosses
    /// keyword-character classes (`mapping.c:646-681`).
    pub fn abbreviate_full(
        &mut self,
        lhs: &str,
        action: MappingAction,
        options: AbbrevOptions,
    ) -> Result<(), MappingError> {
        if options.modes.is_empty() {
            return Err(MappingError::EmptyModes);
        }
        if !valid_abbreviation(lhs) {
            return Err(MappingError::InvalidAbbreviation(lhs.to_owned()));
        }
        for entry in &mut self.abbreviations {
            if entry.lhs == lhs && entry.scope == options.scope {
                entry.modes = entry.modes.without(options.modes);
            }
        }
        self.abbreviations.retain(|entry| !entry.modes.is_empty());
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.abbreviations.push(Abbreviation {
            lhs: lhs.to_owned(),
            action,
            modes: options.modes,
            scope: options.scope,
            flags: options.flags,
            orig_rhs: options.orig_rhs,
            script_context: options.script_context,
            sequence,
        });
        Ok(())
    }

    /// Whether an abbreviation with exactly this trigger already covers any
    /// of `modes` in `scope`: the `<unique>` rejection (`do_map`'s
    /// `retval = 5`, `mapping.c:802`, surfacing as `E226`).
    #[must_use]
    pub fn abbrev_conflicts(&self, lhs: &str, modes: MapModes, scope: MapScope) -> bool {
        self.abbreviations
            .iter()
            .any(|entry| entry.lhs == lhs && entry.scope == scope && entry.modes.intersects(modes))
    }

    /// Removes one abbreviation in one scope, every mode at once.
    pub fn unabbreviate(&mut self, lhs: &str, scope: MapScope) -> bool {
        self.unabbreviate_modes(lhs, MapModes::MAP_BANG, scope) > 0
    }

    /// Removes the named abbreviation's `modes` bits in one scope, the way
    /// `buf_do_map` unmaps (`mapping.c:794-826`): the trigger match ignores
    /// trailing whitespace (`*skipwhite(lhs + n)`, `mapping.c:805`), and when
    /// no trigger matches the right-hand side is tried exactly
    /// (`MAPTYPE_UNMAP`'s second round), which is how `:unabbr bar` removes
    /// `foo -> bar`. Returns how many entries were stripped; zero means
    /// upstream's `retval = 2`, surfacing as `E24`.
    pub fn unabbreviate_modes(&mut self, lhs: &str, modes: MapModes, scope: MapScope) -> usize {
        let mut stripped = 0;
        let mut trigger_hit = false;
        for entry in &mut self.abbreviations {
            if entry.scope == scope
                && entry.modes.intersects(modes)
                && abbrev_lhs_matches(&entry.lhs, lhs)
            {
                entry.modes = entry.modes.without(modes);
                trigger_hit = true;
                stripped += 1;
            }
        }
        if !trigger_hit {
            let wanted = lhs.as_bytes();
            for entry in &mut self.abbreviations {
                if entry.scope == scope
                    && entry.modes.intersects(modes)
                    && entry
                        .action
                        .replaced_keys()
                        .is_some_and(|rhs| rhs == wanted)
                {
                    entry.modes = entry.modes.without(modes);
                    stripped += 1;
                }
            }
        }
        self.abbreviations.retain(|entry| !entry.modes.is_empty());
        stripped
    }

    /// Clears abbreviations in exactly one scope.
    pub fn abbrevclear(&mut self, scope: MapScope) -> usize {
        self.abbrevclear_modes(MapModes::MAP_BANG, scope)
    }

    /// Implements `do_mapclear` over abbreviations (`mapping.c:1066-1111`):
    /// strips `modes` bits in exactly one scope (global for `:abclear`,
    /// buffer-local for `:abclear <buffer>`), deleting entries left empty.
    /// Returns how many entries were stripped.
    pub fn abbrevclear_modes(&mut self, modes: MapModes, scope: MapScope) -> usize {
        let mut stripped = 0;
        for entry in &mut self.abbreviations {
            if entry.scope == scope && entry.modes.intersects(modes) {
                entry.modes = entry.modes.without(modes);
                stripped += 1;
            }
        }
        self.abbreviations.retain(|entry| !entry.modes.is_empty());
        stripped
    }

    /// `check_map` over abbreviations with `exact` set (`mapping.c:2030`):
    /// the entry whose trigger is exactly `lhs` and whose modes overlap,
    /// buffer-local table searched before the global one.
    #[must_use]
    pub fn find_abbrev_exact(
        &self,
        lhs: &str,
        modes: MapModes,
        buffer: Option<BufHandle>,
    ) -> Option<(&Abbreviation, bool)> {
        buffer
            .and_then(|buffer| {
                self.exact_abbrev_in_scope(lhs, modes, MapScope::Buffer(buffer))
                    .map(|entry| (entry, true))
            })
            .or_else(|| {
                self.exact_abbrev_in_scope(lhs, modes, MapScope::Global)
                    .map(|entry| (entry, false))
            })
    }

    /// `do_map`'s listing passes over abbreviations (`mapping.c:698-793`):
    /// every entry whose modes overlap and whose trigger and `filter` are a
    /// prefix of one another, buffer-local table first and newest first
    /// (`map_add` pushes onto the list head, `mapping.c:660-666`).
    #[must_use]
    pub fn abbrev_matching(
        &self,
        filter: &str,
        modes: MapModes,
        buffer: Option<BufHandle>,
    ) -> Vec<(&Abbreviation, bool)> {
        let filter = filter.as_bytes();
        let mut found: Vec<(&Abbreviation, bool)> = self
            .abbreviations
            .iter()
            .filter(|entry| entry.modes.intersects(modes))
            .filter(|entry| {
                let trigger = entry.lhs.as_bytes();
                let shared = trigger.len().min(filter.len());
                trigger[..shared] == filter[..shared]
            })
            .filter_map(|entry| match entry.scope {
                MapScope::Global => Some((entry, false)),
                MapScope::Buffer(handle) => (Some(handle) == buffer).then_some((entry, true)),
            })
            .collect();
        found.sort_by_key(|(entry, local)| (!*local, u64::MAX - entry.sequence));
        found
    }

    fn exact_abbrev_in_scope(
        &self,
        lhs: &str,
        modes: MapModes,
        scope: MapScope,
    ) -> Option<&Abbreviation> {
        self.abbreviations
            .iter()
            .find(|entry| entry.scope == scope && entry.modes.intersects(modes) && entry.lhs == lhs)
    }

    /// Resolves an abbreviation before inserting a typed delimiter, in either
    /// mode. New expansion code prefers
    /// [`Self::lookup_abbreviation_in_mode`].
    #[must_use]
    pub fn lookup_abbreviation(
        &self,
        before_cursor: &str,
        typed: char,
        buffer: Option<BufHandle>,
    ) -> Option<&Abbreviation> {
        if is_keyword(typed) {
            return None;
        }
        if let Some(found) =
            self.lookup_abbreviation_in_mode(before_cursor, typed, MapMode::Insert, buffer)
        {
            return Some(found);
        }
        self.lookup_abbreviation_in_mode(before_cursor, typed, MapMode::CommandLine, buffer)
    }

    /// `check_abbr`'s table half (`mapping.c:1505-1595`): the longest trigger
    /// ending at the cursor whose word boundary holds and whose modes carry
    /// the expanding `mode` (`(mp->m_mode & State)`), buffer-local table
    /// before the global one. Insert-mode callers pass [`MapMode::Insert`],
    /// command-line callers [`MapMode::CommandLine`.
    #[must_use]
    pub fn lookup_abbreviation_in_mode(
        &self,
        before_cursor: &str,
        typed: char,
        mode: MapMode,
        buffer: Option<BufHandle>,
    ) -> Option<&Abbreviation> {
        if is_keyword(typed) {
            return None;
        }
        if let Some(buffer) = buffer
            && let Some(found) =
                self.lookup_abbreviation_scope(before_cursor, mode, MapScope::Buffer(buffer))
        {
            return Some(found);
        }
        self.lookup_abbreviation_scope(before_cursor, mode, MapScope::Global)
    }

    /// Removes local mappings and abbreviations when a buffer is wiped.
    pub fn remove_buffer(&mut self, buffer: BufHandle) {
        let scope = MapScope::Buffer(buffer);
        self.entries
            .retain(|mapping| mapping.options.scope != scope);
        self.abbreviations.retain(|entry| entry.scope != scope);
    }

    /// Number of mapping entries.
    #[must_use]
    pub fn mapping_len(&self) -> usize {
        self.entries.len()
    }

    /// Number of abbreviation entries.
    #[must_use]
    pub fn abbreviation_len(&self) -> usize {
        self.abbreviations.len()
    }

    fn lookup_scope(&self, input: &[u8], mode: MapMode, scope: MapScope) -> Lookup<'_> {
        let mut full: Option<&Mapping> = None;
        let mut longer = false;
        for mapping in self.entries.iter().filter(|mapping| {
            mapping.options.scope == scope && mapping.options.modes.contains(mode)
        }) {
            let lhs = mapping.lhs.as_bytes();
            if lhs.starts_with(input) && lhs.len() > input.len() {
                longer = true;
            }
            if input.starts_with(lhs)
                && full.is_none_or(|found| {
                    lhs.len() > found.lhs.len()
                        || (lhs.len() == found.lhs.len() && mapping.sequence > found.sequence)
                })
            {
                full = Some(mapping);
            }
        }
        if longer && full.is_none_or(|mapping| !mapping.options.flags.contains(MapFlags::NOWAIT)) {
            return Lookup::Prefix(full);
        }
        full.map_or(Lookup::None, |mapping| {
            Lookup::Exact(mapping, mapping.lhs.len())
        })
    }

    fn lookup_abbreviation_scope(
        &self,
        before_cursor: &str,
        mode: MapMode,
        scope: MapScope,
    ) -> Option<&Abbreviation> {
        self.abbreviations
            .iter()
            .filter(|entry| entry.scope == scope && entry.modes.contains(mode))
            .filter(|entry| before_cursor.ends_with(&entry.lhs))
            .filter(|entry| abbreviation_boundary(before_cursor, &entry.lhs))
            .max_by_key(|entry| (entry.lhs.len(), entry.sequence))
    }
}

impl Abbreviation {
    /// One `showmap` row for an abbreviation (`mapping.c:220-266`), the same
    /// shape mappings print: mode characters padded to three columns, the
    /// trigger padded to twelve with one trailing blank guaranteed, the
    /// `*`/`&`/blank remap marker, the `@`/blank buffer-local marker, then
    /// the right-hand side (`<Nop>` for an empty one).
    #[must_use]
    pub fn listing_row(&self, local: bool) -> String {
        let mut row = self.modes.to_chars();
        while row.len() < 3 {
            row.push(' ');
        }
        let shown = special_notation(self.lhs.as_bytes(), true, false);
        let mut width = shown.chars().count();
        row.push_str(&shown);
        loop {
            row.push(' ');
            width += 1;
            if width >= 12 {
                break;
            }
        }
        row.push(if self.flags.contains(MapFlags::SCRIPT) {
            '&'
        } else if self.flags.contains(MapFlags::REMAP) {
            ' '
        } else {
            '*'
        });
        row.push(if local { '@' } else { ' ' });
        match self.action.replaced_keys() {
            Some([]) => row.push_str("<Nop>"),
            Some(keys) => row.push_str(&special_notation(keys, false, false)),
            None => row.push_str("<Nop>"),
        }
        row
    }
}

/// The trigger half of `buf_do_map`'s abbreviation validation
/// (`mapping.c:646-681`): an exact trigger match, or the trigger followed
/// only by whitespace — upstream ignores trailing space when matching the
/// `lhs`, since an abbreviation cannot carry trailing space.
fn abbrev_lhs_matches(trigger: &str, lhs: &str) -> bool {
    lhs.strip_prefix(trigger)
        .is_some_and(|rest| rest.chars().all(char::is_whitespace))
}

fn valid_abbreviation(lhs: &str) -> bool {
    if lhs.len() > 50 {
        return false;
    }
    if lhs.is_empty() || lhs.chars().any(char::is_whitespace) {
        return false;
    }
    let mut characters = lhs.chars().peekable();
    let Some(first) = characters.next() else {
        return false;
    };
    let prefix_class = is_keyword(first);
    let mut last = first;
    let mut count = 1usize;
    let mut prefix_uniform = true;
    while let Some(ch) = characters.next() {
        if characters.peek().is_some() {
            prefix_uniform &= is_keyword(ch) == prefix_class;
        }
        last = ch;
        count += 1;
    }
    !is_keyword(last) || count <= 2 || prefix_uniform
}

fn abbreviation_boundary(before_cursor: &str, lhs: &str) -> bool {
    let prefix_len = before_cursor.len().saturating_sub(lhs.len());
    let prefix = &before_cursor[..prefix_len];
    let Some(first) = lhs.chars().next() else {
        return false;
    };
    let Some(last) = lhs.chars().next_back() else {
        return false;
    };
    let Some(previous) = prefix.chars().next_back() else {
        return true;
    };
    if is_keyword(last) {
        previous.is_whitespace() || is_keyword(previous) != is_keyword(first)
    } else {
        previous.is_whitespace()
    }
}

fn is_keyword(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_only(_lhs: &str, rhs: &str) -> AbbrevOptions {
        AbbrevOptions {
            modes: MapModes::one(MapMode::Insert),
            orig_rhs: rhs.to_owned(),
            ..AbbrevOptions::default()
        }
    }

    fn cmdline_only(lhs: &str, rhs: &str) -> AbbrevOptions {
        let _ = lhs;
        AbbrevOptions {
            modes: MapModes::one(MapMode::CommandLine),
            orig_rhs: rhs.to_owned(),
            ..AbbrevOptions::default()
        }
    }

    #[test]
    fn abbrev_command_modes_cover_insert_cmdline_and_both() {
        assert_eq!(
            abbrev_modes_for_command("abbreviate"),
            MapModes::MAP_BANG
        );
        assert_eq!(
            abbrev_modes_for_command("noreabbrev"),
            MapModes::MAP_BANG
        );
        assert_eq!(
            abbrev_modes_for_command("unabbreviate"),
            MapModes::MAP_BANG
        );
        assert_eq!(
            abbrev_modes_for_command("abclear"),
            MapModes::MAP_BANG
        );
        for name in ["iabbrev", "iabclear", "inoreabbrev", "iunabbrev"] {
            assert_eq!(abbrev_modes_for_command(name), MapModes::one(MapMode::Insert));
        }
        for name in ["cabbrev", "cabclear", "cnoreabbrev", "cunabbrev"] {
            assert_eq!(
                abbrev_modes_for_command(name),
                MapModes::one(MapMode::CommandLine)
            );
        }
    }

    #[test]
    fn abbrev_command_classification_matches_do_exmap() {
        assert!(matches!(
            classify_abbrev_command("abbreviate"),
            AbbrevCommandKind::Define { noremap: false }
        ));
        assert!(matches!(
            classify_abbrev_command("inoreabbrev"),
            AbbrevCommandKind::Define { noremap: true }
        ));
        assert!(matches!(
            classify_abbrev_command("noreabbrev"),
            AbbrevCommandKind::Define { noremap: true }
        ));
        assert_eq!(
            classify_abbrev_command("unabbreviate"),
            AbbrevCommandKind::Undefine
        );
        assert_eq!(
            classify_abbrev_command("iunabbrev"),
            AbbrevCommandKind::Undefine
        );
        assert_eq!(
            classify_abbrev_command("cunabbrev"),
            AbbrevCommandKind::Undefine
        );
        assert_eq!(
            classify_abbrev_command("abclear"),
            AbbrevCommandKind::Clear
        );
        assert_eq!(
            classify_abbrev_command("iabclear"),
            AbbrevCommandKind::Clear
        );
        assert_eq!(
            classify_abbrev_command("cabclear"),
            AbbrevCommandKind::Clear
        );
    }

    #[test]
    fn narrow_and_widened_defines_share_one_trigger() {
        let mut mappings = Mappings::new();
        mappings
            .abbreviate_full(
                "teh",
                MappingAction::Keys(Keys::parse_notation("the", "\\", "\\")),
                insert_only("teh", "the"),
            )
            .unwrap();
        // A command-line define for the same trigger adds bits, it does not
        // evict the insert one.
        mappings
            .abbreviate_full(
                "teh",
                MappingAction::Keys(Keys::parse_notation("THE", "\\", "\\")),
                cmdline_only("teh", "THE"),
            )
            .unwrap();
        assert_eq!(mappings.abbreviation_len(), 2);
        let (insert, _) = mappings
            .find_abbrev_exact("teh", MapModes::one(MapMode::Insert), None)
            .unwrap();
        assert_eq!(insert.orig_rhs, "the");
        let (cmdline, _) = mappings
            .find_abbrev_exact("teh", MapModes::one(MapMode::CommandLine), None)
            .unwrap();
        assert_eq!(cmdline.orig_rhs, "THE");
    }

    #[test]
    fn undefine_strips_only_named_modes() {
        let mut mappings = Mappings::new();
        mappings
            .abbreviate(
                "teh",
                MappingAction::Keys(Keys::parse_notation("the", "\\", "\\")),
                MapScope::Global,
                true,
            )
            .unwrap();
        assert_eq!(
            mappings.unabbreviate_modes("teh", MapModes::one(MapMode::Insert), MapScope::Global),
            1
        );
        // The command-line half survives an insert-only undefine.
        assert!(mappings
            .find_abbrev_exact("teh", MapModes::one(MapMode::CommandLine), None)
            .is_some());
        assert!(mappings
            .find_abbrev_exact("teh", MapModes::one(MapMode::Insert), None)
            .is_none());
    }

    #[test]
    fn undefine_matches_rhs_and_trailing_space() {
        let mut mappings = Mappings::new();
        mappings
            .abbreviate(
                "foo",
                MappingAction::Keys(Keys::parse_notation("bar", "\\", "\\")),
                MapScope::Global,
                true,
            )
            .unwrap();
        // Upstream's second unmap round matches the right-hand side exactly.
        assert_eq!(
            mappings.unabbreviate_modes("bar", MapModes::MAP_BANG, MapScope::Global),
            1
        );
        mappings
            .abbreviate(
                "foo",
                MappingAction::Keys(Keys::parse_notation("bar", "\\", "\\")),
                MapScope::Global,
                true,
            )
            .unwrap();
        // Upstream ignores trailing space when matching the trigger.
        assert_eq!(
            mappings.unabbreviate_modes("foo  ", MapModes::MAP_BANG, MapScope::Global),
            1
        );
        assert_eq!(
            mappings.unabbreviate_modes("missing", MapModes::MAP_BANG, MapScope::Global),
            0
        );
    }

    #[test]
    fn clear_strips_only_named_modes() {
        let mut mappings = Mappings::new();
        mappings
            .abbreviate(
                "teh",
                MappingAction::Keys(Keys::parse_notation("the", "\\", "\\")),
                MapScope::Global,
                true,
            )
            .unwrap();
        assert_eq!(
            mappings.abbrevclear_modes(MapModes::one(MapMode::Insert), MapScope::Global),
            1
        );
        assert_eq!(mappings.abbreviation_len(), 1);
        assert_eq!(
            mappings.abbrevclear_modes(MapModes::MAP_BANG, MapScope::Global),
            1
        );
        assert_eq!(mappings.abbreviation_len(), 0);
    }

    #[test]
    fn listing_orders_buffer_local_first_then_newest() {
        let mut mappings = Mappings::new();
        mappings
            .abbreviate(
                "first",
                MappingAction::Keys(Keys::parse_notation("1", "\\", "\\")),
                MapScope::Global,
                true,
            )
            .unwrap();
        mappings
            .abbreviate(
                "second",
                MappingAction::Keys(Keys::parse_notation("2", "\\", "\\")),
                MapScope::Global,
                true,
            )
            .unwrap();
        let buffer = BufHandle::try_from(7i64).unwrap();
        mappings
            .abbreviate(
                "local",
                MappingAction::Nop,
                MapScope::Buffer(buffer),
                false,
            )
            .unwrap();
        let rows: Vec<String> = mappings
            .abbrev_matching("", MapModes::MAP_BANG, Some(buffer))
            .into_iter()
            .map(|(entry, local)| entry.listing_row(local))
            .collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].contains("local"));
        assert!(rows[0].contains('@'));
        assert!(rows[1].contains("second"));
        assert!(rows[2].contains("first"));
    }

    #[test]
    fn lookup_respects_expanding_mode() {
        let mut mappings = Mappings::new();
        mappings
            .abbreviate_full(
                "teh",
                MappingAction::Keys(Keys::parse_notation("the", "\\", "\\")),
                cmdline_only("teh", "the"),
            )
            .unwrap();
        assert!(mappings
            .lookup_abbreviation_in_mode("teh", ' ', MapMode::CommandLine, None)
            .is_some());
        assert!(mappings
            .lookup_abbreviation_in_mode("teh", ' ', MapMode::Insert, None)
            .is_none());
    }

    #[test]
    fn overlong_trigger_is_invalid() {
        let mut mappings = Mappings::new();
        let long = "a".repeat(51);
        assert!(matches!(
            mappings.abbreviate(
                &long,
                MappingAction::Nop,
                MapScope::Global,
                true,
            ),
            Err(MappingError::InvalidAbbreviation(_))
        ));
    }
}
