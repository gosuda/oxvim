//! Buffer-local folds and the host-facing fold computation seams.
//!
//! Neovim identifies the six methods in `src/nvim/fold.c:321-361`, lazily
//! invalidates computed folds in `src/nvim/fold.c:763-829`, and recursively
//! searches nested folds in `src/nvim/fold.c:1086-1107`. This module keeps
//! those contracts independent of a window or Lua runtime.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use thiserror::Error;

/// Neovim's maximum representable fold nesting depth.
///
/// See `src/nvim/fold.c:78-84`.
pub const MAX_FOLD_DEPTH: usize = 20;

/// The same bound as an `i32` for level arithmetic (`MAX_LEVEL` in
/// `src/nvim/fold.c:78-84`).
const MAX_LEVEL: i32 = 20;

/// A zero-based, byte-oriented buffer position.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Position {
    /// Zero-based logical row.
    pub row: usize,
    /// Zero-based byte column.
    pub column: usize,
}

impl Position {
    /// Creates a position from a zero-based row and byte column.
    #[must_use]
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// A half-open fold range, `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FoldRange {
    /// First position in the fold.
    pub start: Position,
    /// First position after the fold.
    pub end: Position,
}

impl FoldRange {
    /// Creates a range and reverses its endpoints when necessary.
    ///
    /// Neovim reverses manual fold endpoints in `src/nvim/fold.c:535-552`.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::EmptyRange`] when the normalized range is empty.
    pub fn normalized(start: Position, end: Position) -> Result<Self, FoldError> {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        if start == end {
            return Err(FoldError::EmptyRange);
        }
        Ok(Self { start, end })
    }

    /// Creates a whole-line half-open range.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::EmptyRange`] when the two rows are equal.
    pub fn lines(start_row: usize, end_row_exclusive: usize) -> Result<Self, FoldError> {
        Self::normalized(
            Position::new(start_row, 0),
            Position::new(end_row_exclusive, 0),
        )
    }

    /// Returns whether this range contains the position.
    #[must_use]
    pub fn contains(self, position: Position) -> bool {
        self.start <= position && position < self.end
    }

    /// Returns whether this range fully contains another range.
    #[must_use]
    pub fn contains_range(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// Returns whether the ranges share at least one position.
    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Returns the smallest range enclosing both ranges.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Returns whether the fold occupies any part of a zero-based row.
    ///
    /// Neovim's folds are whole lines: `fold_T` stores `fd_top` and `fd_len`
    /// as line counts (`src/nvim/fold_defs.h:31-45`). A whole-line range
    /// therefore occupies rows `[start.row, end.row)`, while a range ending
    /// inside a row still occupies that row.
    #[must_use]
    pub const fn covers_row(self, row: usize) -> bool {
        self.start.row <= row
            && (row < self.end.row || (row == self.end.row && self.end.column > 0))
    }

    /// Returns the last zero-based row the fold occupies.
    #[must_use]
    pub const fn last_row(self) -> usize {
        if self.end.column > 0 {
            self.end.row
        } else {
            self.end.row.saturating_sub(1)
        }
    }
}

/// The source used to derive folds.
///
/// The method set follows `src/nvim/fold.c:321-361`. Expr, syntax, and diff
/// deliberately have no evaluator here; [`FoldComputeRequest`] is their typed
/// host boundary.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum FoldMethod {
    /// Folds are explicitly created and deleted.
    #[default]
    Manual,
    /// Fold levels derive from leading indentation.
    Indent,
    /// Fold levels are supplied by the configured host expression.
    Expr,
    /// Fold levels derive from start and end marker bytes.
    Marker,
    /// Fold levels are supplied by the syntax host.
    Syntax,
    /// Fold ranges are supplied by the diff host.
    Diff,
}

impl FoldMethod {
    /// Returns the host kind for methods that require host computation.
    #[must_use]
    pub const fn host_kind(self) -> Option<HostFoldKind> {
        match self {
            Self::Expr => Some(HostFoldKind::Expr),
            Self::Syntax => Some(HostFoldKind::Syntax),
            Self::Diff => Some(HostFoldKind::Diff),
            Self::Manual | Self::Indent | Self::Marker => None,
        }
    }

    /// Returns the method `'foldmethod'` names, defaulting to manual.
    ///
    /// Neovim validates the string in `src/nvim/optionstr.c` and compares it
    /// with the same six names through the `foldmethodIs*` macros in
    /// `src/nvim/fold.h:15-21`.
    #[must_use]
    pub fn from_option_value(value: &str) -> Self {
        match value {
            "indent" => Self::Indent,
            "expr" => Self::Expr,
            "marker" => Self::Marker,
            "syntax" => Self::Syntax,
            "diff" => Self::Diff,
            _ => Self::Manual,
        }
    }
}

/// A fold's explicit display state.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum FoldState {
    /// The fold's contents are visible.
    Open,
    /// The fold is collapsed.
    #[default]
    Closed,
}

/// One normalized fold in deterministic outer-before-inner order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fold {
    /// Half-open byte range occupied by this fold.
    pub range: FoldRange,
    /// One-based nesting depth.
    pub depth: usize,
    /// Explicit open or closed state.
    pub state: FoldState,
}

/// A fold computation delegated to the editor host.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HostFoldKind {
    /// Evaluate the buffer's fold expression.
    Expr,
    /// Query syntax-derived fold levels.
    Syntax,
    /// Query diff-derived folded regions.
    Diff,
}

/// An immutable request for host-computed fold ranges.
///
/// Neovim dispatches expr, syntax, and diff to separate level getters in
/// `src/nvim/fold.c:1933-1983`. The request keeps that dispatch typed without
/// executing Lua or depending on syntax/diff implementations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoldComputeRequest {
    /// Required host computation.
    pub kind: HostFoldKind,
    /// Text generation for which the result is valid.
    pub changedtick: u64,
    /// Number of logical buffer lines visible to the host.
    pub line_count: usize,
}

/// A host's completed fold computation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldComputeResult {
    /// Request this result answers.
    pub request: FoldComputeRequest,
    /// Normalized half-open ranges returned by the host.
    pub ranges: Vec<FoldRange>,
}

/// Outcome of lazily refreshing the active fold method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FoldRefresh {
    /// The cache is ready for queries.
    Ready {
        /// Text generation represented by the cache.
        changedtick: u64,
        /// Number of active folds.
        fold_count: usize,
    },
    /// The host must compute the requested ranges.
    Host(FoldComputeRequest),
}

/// A request to evaluate fold text for one closed fold.
///
/// The host receives the same essential context Neovim installs for foldtext:
/// start, end, and fold level (`src/nvim/fold.c:1681-1724`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoldTextRequest {
    /// Text generation containing the fold.
    pub changedtick: u64,
    /// Fold being rendered.
    pub range: FoldRange,
    /// One-based fold depth.
    pub level: usize,
}

/// One highlighted chunk returned by a foldtext host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldTextChunk {
    /// Display bytes.
    pub text: Vec<u8>,
    /// Optional highlight group name.
    pub highlight: Option<String>,
}

/// A typed foldtext host result.
///
/// Neovim accepts either text or virtual-text chunks in
/// `src/nvim/fold.c:1726-1750`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FoldText {
    /// Unhighlighted display bytes.
    Plain(Vec<u8>),
    /// Highlighted virtual-text chunks.
    Virtual(Vec<FoldTextChunk>),
}

/// A completed foldtext computation tied to its request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldTextResult {
    /// Request this result answers.
    pub request: FoldTextRequest,
    /// Host-rendered fold text.
    pub text: FoldText,
}

/// A rejected fold operation or host result.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum FoldError {
    /// A half-open fold must contain at least one position.
    #[error("fold range is empty")]
    EmptyRange,
    /// Indent folding cannot divide by a zero shift width.
    #[error("fold shift width must be greater than zero")]
    ZeroShiftWidth,
    /// Start and end marker byte strings must both be non-empty.
    #[error("fold markers must be non-empty")]
    EmptyMarker,
    /// Manual folds may only be changed while the manual method is active.
    #[error("manual fold operation requires the manual fold method")]
    NotManual,
    /// The normalized manual range already exists.
    #[error("fold range already exists")]
    DuplicateRange,
    /// No fold contains the requested position or matches the requested range.
    #[error("no matching fold")]
    NoFold,
    /// Host ranges crossed instead of being disjoint or nested.
    #[error("fold ranges cross and cannot form a deterministic nesting tree")]
    CrossingRanges,
    /// A host result does not match the currently active host method.
    #[error("fold result is for a different fold method")]
    WrongHostMethod,
    /// A host result was computed for an obsolete text generation.
    #[error("fold result is stale")]
    StaleResult,
}

/// Buffer-local fold data and lazy computation state.
#[derive(Clone, Debug)]
pub struct Folds {
    method: FoldMethod,
    shift_width: usize,
    tab_width: usize,
    fold_ignore: Vec<u8>,
    nest_max: i64,
    min_lines: usize,
    marker_start: Vec<u8>,
    marker_end: Vec<u8>,
    manual: Vec<Fold>,
    computed: Vec<Fold>,
    changedtick: Option<u64>,
    requested_tick: Option<u64>,
    dirty: bool,
}

impl Default for Folds {
    fn default() -> Self {
        Self::new()
    }
}

impl Folds {
    /// Creates empty manual fold state with an eight-column shift width.
    #[must_use]
    pub fn new() -> Self {
        Self {
            method: FoldMethod::Manual,
            shift_width: 8,
            tab_width: 8,
            fold_ignore: b"#".to_vec(),
            nest_max: i64::try_from(MAX_FOLD_DEPTH).unwrap_or(i64::MAX),
            min_lines: 1,
            marker_start: b"{{{".to_vec(),
            marker_end: b"}}}".to_vec(),
            manual: Vec::new(),
            computed: Vec::new(),
            changedtick: None,
            requested_tick: None,
            dirty: true,
        }
    }

    /// Returns the active fold method.
    #[must_use]
    pub const fn method(&self) -> FoldMethod {
        self.method
    }

    /// Selects a fold method and invalidates computed data.
    ///
    /// Upstream's `foldUpdate` (`src/nvim/fold.c:742-776`) is a no-op for the
    /// manual method: the window's fold tree survives a method switch, so
    /// moving *to* manual adopts the computed folds as manual ones instead
    /// of dropping them (`set fdm=manual` after indent keeps the folds).
    /// A non-empty manual set wins over adopted folds.
    pub fn set_method(&mut self, method: FoldMethod) {
        if self.method != method {
            let previous = self.method;
            self.method = method;
            if method == FoldMethod::Manual
                && previous != FoldMethod::Manual
                && self.manual.is_empty()
                && !self.computed.is_empty()
            {
                self.manual = std::mem::take(&mut self.computed);
            } else {
                self.computed.clear();
            }
            self.changedtick = None;
            self.requested_tick = None;
            self.dirty = true;
        }
    }

    /// Returns the indentation shift width.
    #[must_use]
    pub const fn shift_width(&self) -> usize {
        self.shift_width
    }

    /// Sets the nonzero indentation shift width.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::ZeroShiftWidth`] when `shift_width` is zero.
    pub fn set_shift_width(&mut self, shift_width: usize) -> Result<(), FoldError> {
        if shift_width == 0 {
            return Err(FoldError::ZeroShiftWidth);
        }
        if self.shift_width != shift_width {
            self.shift_width = shift_width;
            if self.method == FoldMethod::Indent {
                self.dirty = true;
            }
        }
        Ok(())
    }

    /// Returns the tab-stop width used when measuring indent columns.
    ///
    /// Upstream's `get_indent_buf` counts a tab against `'tabstop'`
    /// (`src/nvim/indent.c:369-372`), while the level divisor is the
    /// effective shift width.
    #[must_use]
    pub const fn tab_width(&self) -> usize {
        self.tab_width
    }

    /// Sets the nonzero tab-stop width used when measuring indent columns.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::ZeroShiftWidth`] when `tab_width` is zero.
    pub fn set_tab_width(&mut self, tab_width: usize) -> Result<(), FoldError> {
        if tab_width == 0 {
            return Err(FoldError::ZeroShiftWidth);
        }
        if self.tab_width != tab_width {
            self.tab_width = tab_width;
            if self.method == FoldMethod::Indent {
                self.dirty = true;
            }
        }
        Ok(())
    }

    /// Returns the `'foldignore'` bytes: lines whose first non-blank byte is
    /// one of these have an undefined indent level.
    #[must_use]
    pub fn fold_ignore(&self) -> &[u8] {
        &self.fold_ignore
    }

    /// Sets the `'foldignore'` bytes.
    pub fn set_fold_ignore(&mut self, ignore: impl Into<Vec<u8>>) {
        let ignore = ignore.into();
        if self.fold_ignore != ignore {
            self.fold_ignore = ignore;
            if self.method == FoldMethod::Indent {
                self.dirty = true;
            }
        }
    }

    /// Returns the `'foldnestmax'` cap, clamped at use to `0..=MAX_FOLD_DEPTH`.
    #[must_use]
    pub const fn nest_max(&self) -> i64 {
        self.nest_max
    }

    /// Sets the `'foldnestmax'` cap (`src/nvim/fold.c:2783-2799` clamps each
    /// indent level to `MAX(0, 'foldnestmax')`).
    pub fn set_nest_max(&mut self, nest_max: i64) {
        if self.nest_max != nest_max {
            self.nest_max = nest_max;
            if self.method == FoldMethod::Indent {
                self.dirty = true;
            }
        }
    }

    /// Returns the `'foldminlines'` floor: a closed fold with no more rows
    /// than this is displayed open (`checkSmall`, `src/nvim/fold.c:1466-1491`).
    #[must_use]
    pub const fn min_lines(&self) -> usize {
        self.min_lines
    }

    /// Sets the `'foldminlines'` floor.
    pub fn set_fold_min_lines(&mut self, min_lines: usize) {
        self.min_lines = min_lines;
    }

    /// Returns the marker byte strings as `(start, end)`.
    #[must_use]
    pub fn markers(&self) -> (&[u8], &[u8]) {
        (&self.marker_start, &self.marker_end)
    }

    /// Sets non-empty marker byte strings.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::EmptyMarker`] when either marker is empty.
    pub fn set_markers(
        &mut self,
        start: impl Into<Vec<u8>>,
        end: impl Into<Vec<u8>>,
    ) -> Result<(), FoldError> {
        let start = start.into();
        let end = end.into();
        if start.is_empty() || end.is_empty() {
            return Err(FoldError::EmptyMarker);
        }
        if self.marker_start != start || self.marker_end != end {
            self.marker_start = start;
            self.marker_end = end;
            if self.method == FoldMethod::Marker {
                self.dirty = true;
            }
        }
        Ok(())
    }

    /// Marks computed folds stale for `changedtick`.
    ///
    /// The next [`Self::refresh`] performs local computation or returns a host
    /// request. This mirrors Neovim's postponed full update in
    /// `src/nvim/fold.c:820-829`.
    pub fn invalidate(&mut self, changedtick: u64) {
        self.requested_tick = Some(changedtick);
        self.dirty = true;
    }

    /// Adjusts persistent manual fold rows through a line replacement.
    ///
    /// Starts use right gravity and half-open ends use left gravity, so text
    /// inserted inside a fold extends it while text inserted at its exclusive
    /// end does not. Ranges fully consumed by deletion are removed.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::CrossingRanges`] if row adjustment produces
    /// crossing ranges — a contract violation of the non-decreasing splice
    /// map, since properly nested manual folds cannot cross after adjustment.
    pub(crate) fn splice_rows(
        &mut self,
        start: usize,
        old_rows: usize,
        new_rows: usize,
    ) -> Result<(), FoldError> {
        if self.method != FoldMethod::Manual {
            return Ok(());
        }
        for fold in &mut self.manual {
            fold.range.start.row =
                splice_row(fold.range.start.row, start, old_rows, new_rows, true);
            fold.range.end.row = splice_row(fold.range.end.row, start, old_rows, new_rows, false);
        }
        self.manual.retain(|fold| fold.range.start < fold.range.end);
        // `splice_row` is a non-decreasing map, so properly nested manual
        // folds cannot cross after row adjustment; a crossing range would
        // mean the adjustment map itself broke its own contract.
        normalize_folds(&mut self.manual)?;
        Ok(())
    }

    /// Returns whether active fold data needs refreshing.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns the changedtick represented by the active cache.
    #[must_use]
    pub const fn cached_changedtick(&self) -> Option<u64> {
        self.changedtick
    }

    /// Returns active folds in deterministic outer-before-inner order.
    #[must_use]
    pub fn folds(&self) -> &[Fold] {
        self.active()
    }

    /// Lazily refreshes local methods or returns typed host work.
    ///
    /// Each line is treated as bytes. Indent levels count ASCII spaces as one
    /// column and advance an ASCII tab to the next tab-stop boundary, divided
    /// by the shift width (`get_indent_buf`, `src/nvim/indent.c:369-372`).
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::ZeroShiftWidth`] if indent computation runs with a
    /// zero shift or tab width, or [`FoldError::WrongHostMethod`] when the
    /// active method has no host kind.
    pub fn refresh<B: AsRef<[u8]>>(
        &mut self,
        changedtick: u64,
        lines: &[B],
    ) -> Result<FoldRefresh, FoldError> {
        self.requested_tick = Some(changedtick);
        if !self.dirty && self.changedtick == Some(changedtick) {
            return Ok(FoldRefresh::Ready {
                changedtick,
                fold_count: self.active().len(),
            });
        }

        match self.method {
            FoldMethod::Manual => {
                self.changedtick = Some(changedtick);
                self.dirty = false;
            }
            FoldMethod::Indent => {
                let records = indent_records(
                    lines,
                    self.shift_width,
                    self.tab_width,
                    &self.fold_ignore,
                    self.nest_max,
                )?;
                self.replace_computed(folds_from_records(&records)?)?;
                self.changedtick = Some(changedtick);
                self.dirty = false;
            }
            FoldMethod::Marker => {
                let records = marker_records(lines, &self.marker_start, &self.marker_end);
                self.replace_computed(folds_from_records(&records)?)?;
                self.changedtick = Some(changedtick);
                self.dirty = false;
            }
            FoldMethod::Expr | FoldMethod::Syntax | FoldMethod::Diff => {
                let Some(kind) = self.method.host_kind() else {
                    return Err(FoldError::WrongHostMethod);
                };
                return Ok(FoldRefresh::Host(FoldComputeRequest {
                    kind,
                    changedtick,
                    line_count: lines.len(),
                }));
            }
        }

        Ok(FoldRefresh::Ready {
            changedtick,
            fold_count: self.active().len(),
        })
    }

    /// Applies expr, syntax, or diff ranges returned by the host.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::WrongHostMethod`] when the result's kind does not
    /// match the active method, or [`FoldError::StaleResult`] when the result's
    /// changedtick is no longer current. Returns [`FoldError::CrossingRanges`]
    /// if the host-supplied ranges cross.
    pub fn apply_host_result(&mut self, result: FoldComputeResult) -> Result<(), FoldError> {
        let Some(expected_kind) = self.method.host_kind() else {
            return Err(FoldError::WrongHostMethod);
        };
        if result.request.kind != expected_kind {
            return Err(FoldError::WrongHostMethod);
        }
        if self.requested_tick != Some(result.request.changedtick) {
            return Err(FoldError::StaleResult);
        }

        self.replace_computed(result.ranges)?;
        self.changedtick = Some(result.request.changedtick);
        self.dirty = false;
        Ok(())
    }

    /// Creates a normalized manual fold, initially closed.
    ///
    /// Crossing existing folds expand the new range until the set is properly
    /// nested, matching Neovim's enclosing normalization in
    /// `src/nvim/fold.c:564-655`.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NotManual`] unless the manual method is active,
    /// [`FoldError::EmptyRange`] if the normalized range is empty,
    /// [`FoldError::DuplicateRange`] if the range already exists, or
    /// [`FoldError::CrossingRanges`] if normalization fails.
    pub fn create_manual(
        &mut self,
        start: Position,
        end: Position,
    ) -> Result<FoldRange, FoldError> {
        self.require_manual()?;
        let mut range = FoldRange::normalized(start, end)?;

        loop {
            let mut expanded = false;
            for fold in &self.manual {
                if fold.range == range {
                    return Err(FoldError::DuplicateRange);
                }
                if fold.range.overlaps(range)
                    && !fold.range.contains_range(range)
                    && !range.contains_range(fold.range)
                {
                    range = range.union(fold.range);
                    expanded = true;
                }
            }
            if !expanded {
                break;
            }
        }
        if self.manual.iter().any(|fold| fold.range == range) {
            return Err(FoldError::DuplicateRange);
        }

        self.manual.push(Fold {
            range,
            depth: 1,
            state: FoldState::Closed,
        });
        normalize_folds(&mut self.manual)?;
        Ok(range)
    }

    /// Deletes one exact manual fold, retaining its nested folds.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NotManual`] unless the manual method is active,
    /// [`FoldError::NoFold`] if no manual fold matches the range, or
    /// [`FoldError::CrossingRanges`] if normalization fails.
    pub fn delete_manual(&mut self, range: FoldRange) -> Result<Fold, FoldError> {
        self.require_manual()?;
        let index = self
            .manual
            .iter()
            .position(|fold| fold.range == range)
            .ok_or(FoldError::NoFold)?;
        let removed = self.manual.remove(index);
        normalize_folds(&mut self.manual)?;
        Ok(removed)
    }

    /// Deletes the deepest manual fold containing `position`.
    ///
    /// With `recursive`, descendants are deleted too. Without it, descendants
    /// remain and are promoted, corresponding to Neovim's recursive deletion
    /// switch at `src/nvim/fold.c:662-725`.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NotManual`] unless the manual method is active,
    /// [`FoldError::NoFold`] if no fold contains the position, or
    /// [`FoldError::CrossingRanges`] if normalization fails.
    pub fn delete_manual_at(
        &mut self,
        position: Position,
        recursive: bool,
    ) -> Result<Vec<Fold>, FoldError> {
        self.require_manual()?;
        let target = self
            .manual
            .iter()
            .rfind(|fold| fold.range.contains(position))
            .map(|fold| fold.range)
            .ok_or(FoldError::NoFold)?;

        let mut removed = Vec::new();
        self.manual.retain(|fold| {
            let delete = fold.range == target
                || (recursive && target.contains_range(fold.range) && target != fold.range);
            if delete {
                removed.push(*fold);
            }
            !delete
        });
        normalize_folds(&mut self.manual)?;
        Ok(removed)
    }

    /// Removes every manual fold.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NotManual`] unless the manual method is active.
    pub fn clear_manual(&mut self) -> Result<usize, FoldError> {
        self.require_manual()?;
        let count = self.manual.len();
        self.manual.clear();
        Ok(count)
    }

    /// Returns all folds containing a position, outermost first.
    #[must_use]
    pub fn containing_folds(&self, position: Position) -> impl DoubleEndedIterator<Item = &Fold> {
        self.active()
            .iter()
            .filter(move |fold| fold.range.contains(position))
    }

    /// Returns the deepest fold containing a position.
    ///
    /// Neovim descends through every containing fold in
    /// `src/nvim/fold.c:1086-1107` and uses the deepest match for deletion in
    /// `src/nvim/fold.c:681-707`.
    #[must_use]
    pub fn deepest_fold(&self, position: Position) -> Option<&Fold> {
        self.active()
            .iter()
            .rev()
            .find(|fold| fold.range.contains(position))
    }

    /// Returns the nesting level at a position.
    #[must_use]
    pub fn level_at(&self, position: Position) -> usize {
        self.active()
            .iter()
            .filter(|fold| fold.range.contains(position))
            .count()
    }

    /// Returns the first and last zero-based rows of the closed fold covering
    /// `row`, or `None` when no fold covering it is closed.
    ///
    /// `hasFoldingWin` (`src/nvim/fold.c:173-263`) descends from the outermost
    /// fold and stops at the first closed one, so a closed fold nested inside
    /// an open one is reported while an open fold nested inside a closed one is
    /// not: the answer is always the outermost closed fold covering `row`.
    /// A fold with no more rows than [`Self::min_lines`] is never reported
    /// closed (`checkSmall`, `src/nvim/fold.c:1466-1491`); the search then
    /// continues into its nested folds, which is how a two-line fold shrunk
    /// to one line by an edit reads open again.
    #[must_use]
    pub fn closed_rows_at(&self, row: usize) -> Option<(usize, usize)> {
        self.active()
            .iter()
            .filter(|fold| fold.range.covers_row(row) && fold.state == FoldState::Closed)
            .filter(|fold| fold.range.last_row() + 1 - fold.range.start.row > self.min_lines)
            .map(|fold| (fold.range.start.row, fold.range.last_row()))
            .next()
    }

    /// Returns the nesting level at a zero-based row, zero when no fold covers
    /// it.
    ///
    /// `foldLevelWin` (`src/nvim/fold.c:1088-1107`) counts the folds containing
    /// the line whatever their open or closed state.
    #[must_use]
    pub fn level_at_row(&self, row: usize) -> usize {
        self.active()
            .iter()
            .filter(|fold| fold.range.covers_row(row))
            .count()
    }

    /// Returns a foldtext request for an exact closed fold.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NoFold`] if no closed fold matches the range.
    pub fn fold_text_request(
        &self,
        range: FoldRange,
        changedtick: u64,
    ) -> Result<FoldTextRequest, FoldError> {
        let fold = self
            .active()
            .iter()
            .rev()
            .find(|fold| fold.range == range && fold.state == FoldState::Closed)
            .ok_or(FoldError::NoFold)?;
        Ok(FoldTextRequest {
            changedtick,
            range,
            level: fold.depth,
        })
    }

    /// Opens the outermost closed fold containing `position`.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NoFold`] if no fold contains the position.
    pub fn open(&mut self, position: Position) -> Result<bool, FoldError> {
        let folds = self.active_mut();
        let mut found = false;
        for fold in folds {
            if fold.range.contains(position) {
                found = true;
                if fold.state == FoldState::Closed {
                    fold.state = FoldState::Open;
                    return Ok(true);
                }
            }
        }
        if found {
            Ok(false)
        } else {
            Err(FoldError::NoFold)
        }
    }

    /// Closes the deepest currently visible open fold containing `position`.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NoFold`] if no fold contains the position.
    pub fn close(&mut self, position: Position) -> Result<bool, FoldError> {
        let folds = self.active_mut();
        let mut deepest_open = None;
        let mut found = false;
        for (index, fold) in folds.iter().enumerate() {
            if !fold.range.contains(position) {
                continue;
            }
            found = true;
            if fold.state == FoldState::Closed {
                break;
            }
            deepest_open = Some(index);
        }
        if let Some(index) = deepest_open {
            folds[index].state = FoldState::Closed;
            Ok(true)
        } else if found {
            Ok(false)
        } else {
            Err(FoldError::NoFold)
        }
    }

    /// Opens a closed fold at `position`, otherwise closes the deepest one.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NoFold`] if no fold contains the position.
    pub fn toggle(&mut self, position: Position) -> Result<bool, FoldError> {
        let folds = self.active_mut();
        let mut deepest = None;
        let mut first_closed = None;
        for (index, fold) in folds.iter().enumerate() {
            if !fold.range.contains(position) {
                continue;
            }
            if fold.state == FoldState::Closed {
                first_closed = Some(index);
                break;
            }
            deepest = Some(index);
        }
        if let Some(index) = first_closed {
            folds[index].state = FoldState::Open;
            return Ok(true);
        }
        let index = deepest.ok_or(FoldError::NoFold)?;
        folds[index].state = FoldState::Closed;
        Ok(true)
    }

    /// Opens the first closed containing fold and all of its descendants.
    ///
    /// Neovim opens nested folds recursively in `src/nvim/fold.c:1226-1234`
    /// and `src/nvim/fold.c:1267-1275`.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NoFold`] if no fold contains the position.
    pub fn open_recursive(&mut self, position: Position) -> Result<usize, FoldError> {
        let folds = self.active_mut();
        let mut deepest = None;
        let mut first_closed = None;
        for (index, fold) in folds.iter().enumerate() {
            if fold.range.contains(position) {
                deepest = Some(index);
                if first_closed.is_none() && fold.state == FoldState::Closed {
                    first_closed = Some(index);
                }
            }
        }
        let target = first_closed.or(deepest).ok_or(FoldError::NoFold)?;
        let target_range = folds[target].range;
        let mut changed = 0;
        for fold in &mut folds[target..] {
            if target_range.contains_range(fold.range) && fold.state == FoldState::Closed {
                fold.state = FoldState::Open;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Closes the outermost fold containing `position`.
    ///
    /// Descendant state is retained so reopening non-recursively remains
    /// deterministic, as in `src/nvim/fold.c:1220-1250`.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::NoFold`] if no fold contains the position.
    pub fn close_recursive(&mut self, position: Position) -> Result<usize, FoldError> {
        let fold = self
            .active_mut()
            .iter_mut()
            .find(|fold| fold.range.contains(position))
            .ok_or(FoldError::NoFold)?;
        if fold.state == FoldState::Closed {
            Ok(0)
        } else {
            fold.state = FoldState::Closed;
            Ok(1)
        }
    }

    /// Opens every active fold, corresponding to `zR`.
    pub fn open_all(&mut self) -> usize {
        set_all(self.active_mut(), FoldState::Open)
    }

    /// Closes every active fold, corresponding to `zM`.
    pub fn close_all(&mut self) -> usize {
        set_all(self.active_mut(), FoldState::Closed)
    }

    fn require_manual(&self) -> Result<(), FoldError> {
        if self.method == FoldMethod::Manual {
            Ok(())
        } else {
            Err(FoldError::NotManual)
        }
    }

    fn active(&self) -> &[Fold] {
        if self.method == FoldMethod::Manual {
            &self.manual
        } else {
            &self.computed
        }
    }

    fn active_mut(&mut self) -> &mut Vec<Fold> {
        if self.method == FoldMethod::Manual {
            &mut self.manual
        } else {
            &mut self.computed
        }
    }

    fn replace_computed(&mut self, ranges: Vec<FoldRange>) -> Result<(), FoldError> {
        let states: BTreeMap<_, _> = self
            .computed
            .iter()
            .map(|fold| ((fold.range, fold.depth), fold.state))
            .collect();
        let mut folds: Vec<_> = ranges
            .into_iter()
            .map(|range| Fold {
                range,
                depth: 1,
                state: FoldState::Closed,
            })
            .collect();
        normalize_folds(&mut folds)?;
        for fold in &mut folds {
            fold.state = states
                .get(&(fold.range, fold.depth))
                .copied()
                .unwrap_or(FoldState::Closed);
        }
        self.computed = folds;
        Ok(())
    }
}

/// Computes indent fold levels for byte lines.
///
/// Neovim reports an undefined level for blank/ignored lines and otherwise
/// divides indentation by shift width in `src/nvim/fold.c:2841-2862`, then
/// skips over undefined lines to resolve a fold level in
/// `src/nvim/fold.c:2410-2425`. Per `src/nvim/fold.txt:54-61` such lines take
/// the level of the line above or below, whichever is lower. Here an interior
/// blank line resolves to the lower of the nearest concrete level above and
/// below (looking across runs of blanks); the first and last lines are never
/// undefined and stay at level zero (`src/nvim/fold.c:2852-2854`).
///
/// # Errors
///
/// Returns [`FoldError::ZeroShiftWidth`] when `shift_width` is zero.
pub fn indent_levels<B: AsRef<[u8]>>(
    lines: &[B],
    shift_width: usize,
) -> Result<Vec<usize>, FoldError> {
    if shift_width == 0 {
        return Err(FoldError::ZeroShiftWidth);
    }
    let mut raw = Vec::with_capacity(lines.len());
    for line in lines {
        let bytes = line.as_ref();
        let mut columns = 0usize;
        let mut blank = true;
        for &byte in bytes {
            match byte {
                b' ' => columns = columns.saturating_add(1),
                b'\t' => {
                    let remainder = columns % shift_width;
                    columns = columns.saturating_add(shift_width - remainder);
                }
                b'\r' | b'\n' => {}
                _ => {
                    blank = false;
                    break;
                }
            }
        }
        raw.push((!blank).then_some((columns / shift_width).min(MAX_FOLD_DEPTH)));
    }

    let line_count = raw.len();
    let mut levels = vec![0usize; line_count];
    let mut previous = 0usize;
    // Forward pass: for each blank seed the nearest concrete level above
    // (0 before the buffer start).
    for (index, level) in raw.iter().copied().enumerate() {
        if let Some(lvl) = level {
            previous = lvl;
            levels[index] = lvl;
        } else {
            levels[index] = previous;
        }
    }
    // Backward pass: take the lower of the level above and the nearest
    // concrete level below (0 past the buffer end), which is how a blank
    // run inside a fold resolves.
    let mut next = 0usize;
    for (index, level) in raw.iter().copied().enumerate().rev() {
        if let Some(lvl) = level {
            next = lvl;
        } else {
            levels[index] = levels[index].min(next);
        }
    }
    Ok(levels)
}

/// One line's fold-level record for [`folds_from_records`].
///
/// The fields mirror the per-line state `foldUpdateIEMSRecurse`
/// (`src/nvim/fold.c:2074-2425`) threads through a refresh: the level getter
/// fills them in for every line in order, then the builder walks them to cut
/// folds. `start` counts start markers on the line (marker `{{{`, expr `>`
/// and `a`); `had_end` carries the *previous* line's end value (expr `<` and
/// `s`), closing folds the line after it was set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LevelRecord {
    /// Fold level of this line; negative means undefined (blank line under
    /// indent, or a negative expression result), resolved by looking ahead.
    level: i32,
    /// Start markers found on this line.
    start: u32,
    /// Previous line's end value, or [`NO_FOLD_END`] when the previous line
    /// set none.
    had_end: i32,
}

/// [`LevelRecord::had_end`] value meaning the previous line ended no fold.
const NO_FOLD_END: i32 = i32::MAX;

/// Computes one [`LevelRecord`] per line for the marker method.
///
/// This is `foldlevelMarker` (`src/nvim/fold.c:2924-2980`) in record form:
/// the carried level starts at the previous line's next level, a start
/// marker raises both the line level and the next level (an explicit
/// `{{{n` sets both to `n`), and an end marker only lowers the next level
/// (an explicit `}}}n` additionally sets the line level to `n`, capped so
/// it never starts a fold above the level the line began at).
fn marker_records<B: AsRef<[u8]>>(lines: &[B], start: &[u8], end: &[u8]) -> Vec<LevelRecord> {
    let mut level: i32 = 0;
    let mut records = Vec::with_capacity(lines.len());
    for line in lines {
        let bytes = line.as_ref();
        let start_level = level;
        let mut next_level = level;
        let mut starts: u32 = 0;
        let mut offset = 0usize;
        while offset < bytes.len() {
            let start_at = find_bytes(&bytes[offset..], start).map(|at| offset + at);
            let end_at = find_bytes(&bytes[offset..], end).map(|at| offset + at);
            if start_at.is_some_and(|at| end_at.is_none_or(|end| at <= end)) {
                let at = start_at.unwrap_or(offset);
                (level, next_level, starts) = apply_start_marker(
                    bytes,
                    at + start.len(),
                    level,
                    next_level,
                    start_level,
                    starts,
                );
                offset = at + start.len();
            } else if let Some(at) = end_at {
                (level, next_level) =
                    apply_end_marker(bytes, at + end.len(), level, next_level, start_level);
                offset = at + end.len();
            } else {
                break;
            }
        }
        next_level = next_level.max(0);
        records.push(LevelRecord {
            level,
            start: starts,
            had_end: NO_FOLD_END,
        });
        level = next_level;
    }
    records
}

/// Applies one start marker found at `digit_at` (the first byte after it).
///
/// Mirrors the start branch of `foldlevelMarker`: a plain marker raises both
/// the line level and the next level and counts one start, while an explicit
/// `{{{n` sets both levels to `n` and reports `MAX(n - start_level, 1)`
/// starts, which is how the builder knows the line begins new folds.
fn apply_start_marker(
    bytes: &[u8],
    digit_at: usize,
    level: i32,
    next_level: i32,
    start_level: i32,
    starts: u32,
) -> (i32, i32, u32) {
    if let Some(number) = decimal_after(bytes, digit_at)
        && number > 0
    {
        let numbered = i32::try_from(number).unwrap_or(i32::MAX).min(MAX_LEVEL);
        let count = numbered
            .saturating_sub(start_level)
            .max(1)
            .try_into()
            .unwrap_or(u32::MAX);
        return (numbered, numbered, count);
    }
    (
        level.saturating_add(1).min(MAX_LEVEL),
        next_level.saturating_add(1).min(MAX_LEVEL),
        starts.saturating_add(1),
    )
}

/// Applies one end marker found at `digit_at` (the first byte after it).
///
/// Mirrors the end branch of `foldlevelMarker`: a plain marker only lowers
/// the next level, so the end-marker line keeps the fold's level, while an
/// explicit `}}}n` additionally sets the line level to `n` without ever
/// starting a fold above the level the line began at.
fn apply_end_marker(
    bytes: &[u8],
    digit_at: usize,
    level: i32,
    next_level: i32,
    start_level: i32,
) -> (i32, i32) {
    if let Some(number) = decimal_after(bytes, digit_at)
        && number > 0
    {
        let numbered = i32::try_from(number).unwrap_or(i32::MAX).min(MAX_LEVEL);
        return (numbered, numbered.saturating_sub(1).min(start_level));
    }
    (level, next_level.saturating_sub(1))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decimal_after(bytes: &[u8], start: usize) -> Option<usize> {
    let mut value = 0usize;
    let mut found = false;
    for &byte in bytes.get(start..)? {
        if !byte.is_ascii_digit() {
            break;
        }
        found = true;
        value = value
            .saturating_mul(10)
            .saturating_add(usize::from(byte - b'0'));
    }
    found.then_some(value)
}

/// Computes one [`LevelRecord`] per line for the indent method.
///
/// This is `foldlevelIndent` (`src/nvim/fold.c:2783-2801`) in record form:
/// the level is the indent width (spaces count one column, tabs advance to
/// the next `tab_width` boundary) divided by `shift_width`, capped at
/// `MAX(0, nest_max)`. A blank line, or one whose first non-blank byte is in
/// `ignore` (`'foldignore'`), has an undefined (`-1`) level, except that the
/// first and last buffer lines always report zero
/// (`src/nvim/fold.c:2852-2854`).
///
/// # Errors
///
/// Returns [`FoldError::ZeroShiftWidth`] when `shift_width` or `tab_width`
/// is zero.
fn indent_records<B: AsRef<[u8]>>(
    lines: &[B],
    shift_width: usize,
    tab_width: usize,
    ignore: &[u8],
    nest_max: i64,
) -> Result<Vec<LevelRecord>, FoldError> {
    if shift_width == 0 || tab_width == 0 {
        return Err(FoldError::ZeroShiftWidth);
    }
    let cap = nest_max
        .clamp(0, MAX_LEVEL.into())
        .try_into()
        .unwrap_or(0usize);
    let last = lines.len().saturating_sub(1);
    let mut records = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let bytes = line.as_ref();
        let mut columns = 0usize;
        let mut first_content = None;
        for &byte in bytes {
            match byte {
                b' ' => columns = columns.saturating_add(1),
                b'\t' => {
                    let advance = tab_width.saturating_sub(columns % tab_width);
                    columns = columns.saturating_add(advance.max(1));
                }
                _ => {
                    first_content = Some(byte);
                    break;
                }
            }
        }
        let undefined = match first_content {
            None => true,
            Some(byte) => ignore.contains(&byte),
        };
        let level = if undefined {
            if index == 0 || index == last { 0 } else { -1 }
        } else {
            columns
                .saturating_div(shift_width)
                .min(cap)
                .try_into()
                .unwrap_or(MAX_LEVEL)
        };
        records.push(LevelRecord {
            level,
            start: 0,
            had_end: NO_FOLD_END,
        });
    }
    Ok(records)
}

/// Cuts whole-line fold ranges from per-line [`LevelRecord`]s.
///
/// This is a fresh whole-buffer `foldUpdateIEMSRecurse`
/// (`src/nvim/fold.c:2074-2425`) without incremental reuse: a level-1 fold
/// starts at the first line with a positive level, and each fold extends
/// while following lines report at least its level. A line past the first
/// whose `start` count exceeds its level surplus (`level > lvl - start`,
/// `src/nvim/fold.c:2410`) or whose `had_end` reaches the fold level
/// (`level >= had_end`) ends the current fold and starts a new one, which is
/// how adjacent same-level marker folds stay apart. Undefined (`-1`) lines
/// are skipped to the next defined line: they join the fold when that line
/// continues it, and are left out when it ends the fold, exactly like the
/// `ll` lookahead in `src/nvim/fold.c:2332-2372`.
///
/// # Errors
///
/// Returns [`FoldError::EmptyRange`] if a computed range is degenerate.
fn folds_from_records(records: &[LevelRecord]) -> Result<Vec<FoldRange>, FoldError> {
    let mut ranges = Vec::new();
    let mut row = 0;
    while row < records.len() {
        if records[row].level <= 0 {
            row += 1;
            continue;
        }
        row = build_fold(records, 1, row, &mut ranges)?;
    }
    Ok(ranges)
}

/// Builds the single fold at `level` starting at the defined row `first`
/// (whose level is at least `level`), appends its range when the scan
/// created one, and answers the first row below it.
///
/// The loop mirrors `foldUpdateIEMSRecurse` for a fresh build: each visit
/// first offers the line to fold creation (a fold starts here when the level
/// differs, a start marker sits on it, the previous line ended a fold, or it
/// is the last line), then ends the fold below the line when the effective
/// level drops below `level`, and otherwise recurses into nested folds or
/// advances past undefined lines through [`next_defined_row`]. A frame that
/// ends before creating anything emits no range, which is how a lone deeper
/// line over a lower one leaves no nested fold.
fn build_fold(
    records: &[LevelRecord],
    level: i32,
    first: usize,
    ranges: &mut Vec<FoldRange>,
) -> Result<usize, FoldError> {
    let last = records.len().saturating_sub(1);
    let top = first;
    let mut lnum = first;
    let mut save = first;
    let mut current = records[first];
    let mut created = false;
    loop {
        if lnum >= records.len() {
            break;
        }
        if !created
            && (current.level != level
                || current.start != 0
                || current.had_end != NO_FOLD_END
                || save == last
                || lnum == last)
        {
            created = true;
        }
        let forced = lnum > top
            && (level > current.level - start_level(current) || level >= current.had_end);
        let effective = if forced { 0 } else { current.level };
        if effective < level {
            break;
        }
        if effective > level && created {
            let below = build_fold(records, level + 1, save, ranges)?;
            lnum = below;
            if lnum >= records.len() {
                break;
            }
            // The last buffer row always carries a defined level, so a
            // defined row at or after `below` always exists; keeping the
            // previous records on the impossible miss ends the fold here
            // instead of looping forever.
            if let Some(defined) = next_defined_row(records, lnum) {
                save = defined;
                current = records[defined];
            }
            continue;
        }
        // The last buffer row always carries a defined level, so reaching
        // past it means the fold runs to the end of the buffer.
        let Some(next_save) = next_defined_row(records, save + 1) else {
            lnum = records.len();
            break;
        };
        lnum = save + 1;
        save = next_save;
        current = records[next_save];
    }
    if created {
        ranges.push(FoldRange::lines(top, lnum)?);
    }
    Ok(lnum)
}

/// Answers the first row at or after `from` whose level is defined
/// (non-negative), or `None` when every remaining line is undefined.
fn next_defined_row(records: &[LevelRecord], from: usize) -> Option<usize> {
    records
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, record)| record.level >= 0)
        .map(|(row, _)| row)
}

/// Reads the `start` count as an `i32` for level arithmetic.
fn start_level(record: LevelRecord) -> i32 {
    record.start.try_into().unwrap_or(i32::MAX)
}

fn normalize_folds(folds: &mut Vec<Fold>) -> Result<(), FoldError> {
    folds.sort_by(|left, right| compare_ranges(left.range, right.range));
    let mut stack: Vec<FoldRange> = Vec::new();
    for fold in folds {
        while let Some(candidate) = stack.last().copied() {
            if candidate.contains_range(fold.range) {
                break;
            }
            if candidate.overlaps(fold.range) {
                return Err(FoldError::CrossingRanges);
            }
            stack.pop();
        }
        fold.depth = stack.len() + 1;
        stack.push(fold.range);
    }
    Ok(())
}

fn compare_ranges(left: FoldRange, right: FoldRange) -> Ordering {
    left.start
        .cmp(&right.start)
        .then_with(|| right.end.cmp(&left.end))
}

fn splice_row(
    row: usize,
    start: usize,
    old_rows: usize,
    new_rows: usize,
    right_gravity: bool,
) -> usize {
    let old_end = start.saturating_add(old_rows);
    let new_end = start.saturating_add(new_rows);
    if old_rows == 0 {
        if row > start || (row == start && right_gravity) {
            return row.saturating_add(new_rows);
        }
        return row;
    }
    if row < start || (row == start && !right_gravity) {
        return row;
    }
    if row > old_end || (row == old_end && right_gravity) {
        return new_end.saturating_add(row.saturating_sub(old_end));
    }
    if right_gravity { new_end } else { start }
}

fn set_all(folds: &mut [Fold], state: FoldState) -> usize {
    let mut changed = 0;
    for fold in folds {
        if fold.state != state {
            fold.state = state;
            changed += 1;
        }
    }
    changed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Every expectation below is the value the oracle at
    //! `.references/neovim/build/bin/nvim` (v0.13.0-dev-1390) answers for the
    //! same buffer, recorded next to each assertion.

    use super::*;

    fn byte_lines(lines: &[&str]) -> Vec<Vec<u8>> {
        lines.iter().map(|line| line.as_bytes().to_vec()).collect()
    }

    /// Refreshes `folds` over `lines` and answers `(start, end)` row pairs.
    fn computed_ranges(folds: &mut Folds, lines: &[&str]) -> Vec<(usize, usize)> {
        let owned = byte_lines(lines);
        let tick = folds.cached_changedtick().unwrap_or(0) + 1;
        folds.invalidate(tick);
        match folds.refresh(tick, &owned).unwrap() {
            FoldRefresh::Ready { .. } => {}
            FoldRefresh::Host(_) => panic!("unexpected host request"),
        }
        folds
            .folds()
            .iter()
            .map(|fold| (fold.range.start.row, fold.range.last_row()))
            .collect()
    }

    // fold.c:2924 — adjacent same-level marker folds stay apart: the second
    // `{{{` line starts a new fold instead of merging into the first.
    // Oracle (`['{{{','}}}','{{{','', '}}}']`): folds [1,2] and [3,5].
    #[test]
    fn marker_folds_split_at_each_start_marker() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Marker);
        assert_eq!(
            computed_ranges(&mut folds, &["{{{", "}}}", "{{{", "", "}}}"]),
            vec![(0, 1), (2, 4)]
        );
    }

    // fold.c:2924 — `['{{{','}}}','{{{','}}}']` gives [1,2] and [3,4].
    #[test]
    fn marker_folds_split_adjacent_pairs() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Marker);
        assert_eq!(
            computed_ranges(&mut folds, &["{{{", "}}}", "{{{", "}}}"]),
            vec![(0, 1), (2, 3)]
        );
    }

    // fold.c:2946 — an end marker with an explicit level sets the line's
    // level to that number. Oracle (`['one }}}4','two']`): levels [4, 0],
    // i.e. four nested single-line folds on line one.
    #[test]
    fn marker_end_digit_sets_the_line_level() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Marker);
        assert_eq!(
            computed_ranges(&mut folds, &["one }}}4", "two"]),
            vec![(0, 0), (0, 0), (0, 0), (0, 0)]
        );
        assert_eq!(folds.level_at_row(0), 4);
        assert_eq!(folds.level_at_row(1), 0);
    }

    // fold.c:2924 — a lone `{{{ }}}` line is a single-line fold, which stays
    // open because it is no bigger than 'foldminlines'. Oracle: level 1,
    // `foldclosed(1)` is -1.
    #[test]
    fn marker_single_line_fold_is_reported_open() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Marker);
        assert_eq!(computed_ranges(&mut folds, &["{{{ }}}"]), vec![(0, 0)]);
        assert_eq!(folds.closed_rows_at(0), None);
    }

    // fold.c:2783 — a tab counts against 'tabstop', not 'shiftwidth':
    // six tabs at `tabstop=8` indent 48 columns, i.e. level 24 at sw=2,
    // capped by the default 'foldnestmax' of 20. Oracle: foldlevel 20.
    #[test]
    fn indent_tabs_measure_against_tabstop() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Indent);
        folds.set_shift_width(2).unwrap();
        folds.set_tab_width(8).unwrap();
        let owned = byte_lines(&["\t\t\t\t\t\ta"]);
        folds.invalidate(1);
        folds.refresh(1, &owned).unwrap();
        assert_eq!(folds.level_at_row(0), 20);
    }

    // fold.c:2783 — `'foldnestmax'` caps the reported level. Oracle:
    // `setlocal foldnestmax=10` gives 10, `=-1` gives 0.
    #[test]
    fn indent_respects_foldnestmax() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Indent);
        folds.set_shift_width(2).unwrap();
        folds.set_tab_width(8).unwrap();
        folds.set_nest_max(10);
        let owned = byte_lines(&["\t\t\t\t\t\ta"]);
        folds.invalidate(1);
        folds.refresh(1, &owned).unwrap();
        assert_eq!(folds.level_at_row(0), 10);
        folds.set_nest_max(-1);
        folds.invalidate(2);
        folds.refresh(2, &owned).unwrap();
        assert_eq!(folds.level_at_row(0), 0);
    }

    // fold.c:2794 — a line whose first non-blank byte is in 'foldignore'
    // has an undefined level. Oracle (`['  a','  # x','  b']`, fdi=#):
    // the comment line joins the surrounding fold.
    #[test]
    fn indent_ignores_foldignore_lines() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Indent);
        folds.set_shift_width(2).unwrap();
        let owned = byte_lines(&["  a", "  # x", "  b"]);
        folds.invalidate(1);
        folds.refresh(1, &owned).unwrap();
        assert_eq!(folds.level_at_row(1), 1);
        assert_eq!(
            computed_ranges(&mut folds, &["  a", "  # x", "  b"]),
            vec![(0, 2)]
        );
    }

    // fold.c:1466 — a fold shrunk to one line by an edit reads open again:
    // its single row is no bigger than the default 'foldminlines' of 1.
    // Oracle (`['line 1','  line 2']` after deleting line 3): fc2=-1, fl2=1.
    #[test]
    fn shrunk_single_line_fold_reads_open() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Indent);
        folds.set_shift_width(2).unwrap();
        assert_eq!(
            computed_ranges(&mut folds, &["line 1", "  line 2"]),
            vec![(1, 1)]
        );
        assert_eq!(folds.level_at_row(1), 1);
        assert_eq!(folds.closed_rows_at(1), None);
    }

    // fold.c:1466 — `'foldminlines'=5` keeps a two-line manual fold displayed
    // open. Oracle: `foldclosed(2)` is -1 while the window uses 5.
    #[test]
    fn foldminlines_keeps_small_folds_open() {
        let mut folds = Folds::new();
        folds
            .create_manual(Position::new(1, 0), Position::new(3, 0))
            .unwrap();
        assert_eq!(folds.closed_rows_at(1), Some((1, 2)));
        folds.set_fold_min_lines(5);
        assert_eq!(folds.closed_rows_at(1), None);
        folds.set_fold_min_lines(0);
        assert_eq!(folds.closed_rows_at(1), Some((1, 2)));
    }

    // fold.c:742 — switching to manual adopts the computed folds instead of
    // dropping them. Oracle (`set fdm=indent`, redraw, `set fdm=manual`):
    // the indent folds answer `foldclosed` as manual ones.
    #[test]
    fn switching_to_manual_adopts_computed_folds() {
        let mut folds = Folds::new();
        folds.set_method(FoldMethod::Indent);
        folds.set_shift_width(2).unwrap();
        let owned = byte_lines(&["a", "  b", "  c"]);
        folds.invalidate(1);
        folds.refresh(1, &owned).unwrap();
        assert_eq!(folds.method(), FoldMethod::Indent);
        folds.set_method(FoldMethod::Manual);
        assert_eq!(folds.method(), FoldMethod::Manual);
        assert_eq!(folds.closed_rows_at(1), Some((1, 2)));
    }
}
