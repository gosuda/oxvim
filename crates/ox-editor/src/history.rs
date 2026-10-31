//! Command-line history tables (`src/nvim/cmdhist.c`).
//!
//! The five `histadd()`/`histget()`/`histdel()`/`histnr()` banks (`:`, `/`,
//! `=`, `@`, `>`). Upstream keeps them as circular buffers sized by the
//! `'history'` option (`hislen`, `init_history`); this port keeps insertion
//! order oldest-first in a [`VecDeque`] and enforces the same capacity on
//! insert, which is exactly the wraparound behavior while the option never
//! shrinks. The port has no option-change realloc hook, so a shrink applies
//! lazily on the next add.
//!
//! Entry identity mirrors `hisnum`: every add — including a dedup move —
//! consumes a fresh monotonically increasing number, reset only by
//! [`History::clear`]. Positive `histget()` indices are absolute hisnums,
//! negative ones count back from the newest, and `0` never resolves
//! (`calc_hist_idx`, `cmdhist.c:374+`).
//!
//! One upstream quirk is preserved: deleting the final entry by index leaves
//! `hisidx` on a cleared slot, so `histnr()` reports `0` instead of `-1`
//! until the next add or clear (`del_history_idx`, `cmdhist.c:465+`).
//! Clearing, or regex-deleting down to empty, resets to `-1`
//! (`clr_history`, `del_history_entry`).

use std::collections::VecDeque;

/// One history bank, in upstream `HistoryType` order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryKind {
    /// `:` colon commands (`HIST_CMD`).
    Cmd,
    /// `/` and `?` searches (`HIST_SEARCH`).
    Search,
    /// `=` expressions (`HIST_EXPR`).
    Expr,
    /// `@` `input()` lines (`HIST_INPUT`).
    Input,
    /// `>` debug commands (`HIST_DEBUG`).
    Debug,
}

impl HistoryKind {
    /// All banks in `HistoryType` order.
    pub const ALL: [Self; 5] = [
        Self::Cmd,
        Self::Search,
        Self::Expr,
        Self::Input,
        Self::Debug,
    ];

    /// Table slot, matching the `history[HIST_COUNT]` layout.
    fn index(self) -> usize {
        match self {
            Self::Cmd => 0,
            Self::Search => 1,
            Self::Expr => 2,
            Self::Input => 3,
            Self::Debug => 4,
        }
    }

    /// `hist_char2type` (`cmdhist.c:71-89`). `NUL` has no bank here: an empty
    /// kind resolves to the live command line first, which this store cannot
    /// see, so the builtin maps that before calling.
    pub fn from_char(byte: u8) -> Option<Self> {
        match byte {
            b':' => Some(Self::Cmd),
            b'/' | b'?' => Some(Self::Search),
            b'=' => Some(Self::Expr),
            b'@' => Some(Self::Input),
            b'>' => Some(Self::Debug),
            _ => None,
        }
    }

    /// `get_histtype` (`cmdhist.c:270-289`): the case-insensitive significant
    /// prefix of `cmd`/`search`/`expr`/`input`/`debug`, or one history
    /// character. An empty name is `None`: upstream falls back to the current
    /// command line there, which the builtin resolves from mode state.
    pub fn from_name(name: &str) -> Option<Self> {
        const NAMES: [&str; 5] = ["cmd", "search", "expr", "input", "debug"];
        if name.is_empty() {
            return None;
        }
        let bytes = name.as_bytes();
        if bytes.len() == 1 {
            if let Some(kind) = Self::from_char(bytes[0]) {
                return Some(kind);
            }
        }
        // `STRNICMP(name, history_names[i], len) == 0`: the caller name must
        // be a prefix of the table name, so overlong names never match.
        NAMES
            .iter()
            .position(|full| {
                bytes.len() <= full.len()
                    && full.as_bytes()[..bytes.len()].eq_ignore_ascii_case(bytes)
            })
            .map(|index| Self::ALL[index])
    }
}

/// One stored entry: upstream `histentry_T` without timestamps or ShaDa data.
#[derive(Clone, Debug, Eq, PartialEq)]
struct HistoryEntry {
    /// Identifying number (`hisnum`): fresh on every add, never reused until
    /// [`History::clear`] resets the table counter.
    hisnum: i64,
    text: String,
}

/// One bank: insertion-ordered entries plus the `hisnum` counter.
#[derive(Clone, Debug, Default)]
struct HistoryTable {
    /// Oldest first; the back is the newest (`hisidx`).
    entries: VecDeque<HistoryEntry>,
    /// Next `hisnum` to hand out; reset to 0 only by clearing.
    next_hisnum: i64,
    /// `del_history_idx` emptied the table without resetting the counter:
    /// `histnr()` reports 0, not -1, until the next add or clear.
    index_drained: bool,
}

/// The five command-line history banks (`cmdhist.c` `history`).
#[derive(Clone, Debug, Default)]
pub struct History {
    tables: [HistoryTable; 5],
}

impl History {
    /// Empty banks, zeroed counters — the `:set history&` startup shape.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `add_to_history` (`cmdhist.c:301-342`): appends `item`, moving an
    /// existing copy to the newest slot with a fresh `hisnum` instead of
    /// duplicating it. Entries past `capacity` fall off the oldest end, like
    /// the circular buffer wrapping. A zero capacity stores nothing and
    /// leaves the counter alone (`hislen == 0` early return).
    pub fn add(&mut self, kind: HistoryKind, item: &str, capacity: usize) {
        if capacity == 0 {
            return;
        }
        let table = &mut self.tables[kind.index()];
        if let Some(position) = table.entries.iter().position(|entry| entry.text == item) {
            table.entries.remove(position);
        }
        table.next_hisnum += 1;
        table.entries.push_back(HistoryEntry {
            hisnum: table.next_hisnum,
            text: item.to_owned(),
        });
        table.index_drained = false;
        while table.entries.len() > capacity {
            table.entries.pop_front();
        }
    }

    /// `get_history_idx` (`cmdhist.c:353-363`): the newest entry's number, or
    /// -1 on an empty bank (`0` on an index-drained one — see module docs).
    #[must_use]
    pub fn newest(&self, kind: HistoryKind) -> i64 {
        let table = &self.tables[kind.index()];
        table.entries.back().map_or_else(
            // Fresh and cleared banks read -1; only an index-drained bank
            // reads 0 (see module docs).
            || if table.index_drained { 0 } else { -1 },
            |entry| entry.hisnum,
        )
    }

    /// `calc_hist_idx` (`cmdhist.c:374+`) as a slot: positive numbers are
    /// absolute hisnums, negative ones count back from the newest (`-1` is
    /// the newest), and `0` never resolves.
    fn resolve(&self, kind: HistoryKind, index: i64) -> Option<usize> {
        let table = &self.tables[kind.index()];
        if index == 0 || table.entries.is_empty() {
            return None;
        }
        if index > 0 {
            table
                .entries
                .iter()
                .position(|entry| entry.hisnum == index)
        } else {
            // Negative: count back from the newest (`-1` is the newest), so
            // the slot is `len - |index|`; a borrow past the oldest end is a
            // miss. `index != 0` here, so the subtrahend is at least 1.
            let back = usize::try_from(index.unsigned_abs()).ok()?;
            table.entries.len().checked_sub(back)
        }
    }

    /// `f_histget` lookup (`cmdhist.c:556-576`): the entry text, or `None`
    /// for a miss. Misses are never errors — the builtin renders `""`.
    #[must_use]
    pub fn get(&self, kind: HistoryKind, index: i64) -> Option<&str> {
        self.resolve(kind, index)
            .map(|position| self.tables[kind.index()].entries[position].text.as_str())
    }

    /// `clr_history` (`cmdhist.c:408+`): empties the bank and resets its
    /// counter, so the next add restarts at hisnum 1.
    pub fn clear(&mut self, kind: HistoryKind) {
        let table = &mut self.tables[kind.index()];
        table.entries.clear();
        table.next_hisnum = 0;
        table.index_drained = false;
    }

    /// `del_history_idx` (`cmdhist.c:465+`): removes one indexed entry,
    /// keeping the survivors in order. Draining the bank by index sets the
    /// `histnr() == 0` quirk flag (see module docs).
    pub fn delete_index(&mut self, kind: HistoryKind, index: i64) -> bool {
        match self.resolve(kind, index) {
            None => false,
            Some(position) => {
                let table = &mut self.tables[kind.index()];
                table.entries.remove(position);
                if table.entries.is_empty() {
                    table.index_drained = true;
                }
                true
            }
        }
    }

    /// `del_history_entry` (`cmdhist.c:429+`): removes every entry the
    /// caller-compiled pattern matches, keeping the survivors in order.
    /// Reports whether anything matched. Deleting down to empty resets to
    /// the cleared shape (`histnr() == -1`).
    pub fn delete_pattern(
        &mut self,
        kind: HistoryKind,
        matches: impl Fn(&str) -> bool,
    ) -> bool {
        let table = &mut self.tables[kind.index()];
        if table.entries.is_empty() {
            return false;
        }
        let before = table.entries.len();
        table.entries.retain(|entry| !matches(&entry.text));
        if table.entries.is_empty() {
            table.index_drained = false;
        }
        table.entries.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::{History, HistoryKind};

    #[test]
    fn kind_names_match_upstream_prefixes() {
        for (name, kind) in [
            ("cmd", HistoryKind::Cmd),
            ("c", HistoryKind::Cmd),
            ("search", HistoryKind::Search),
            ("s", HistoryKind::Search),
            ("expr", HistoryKind::Expr),
            ("e", HistoryKind::Expr),
            ("input", HistoryKind::Input),
            ("i", HistoryKind::Input),
            ("debug", HistoryKind::Debug),
            ("d", HistoryKind::Debug),
            (":", HistoryKind::Cmd),
            ("/", HistoryKind::Search),
            ("?", HistoryKind::Search),
            ("=", HistoryKind::Expr),
            ("@", HistoryKind::Input),
            (">", HistoryKind::Debug),
            ("CMD", HistoryKind::Cmd),
            ("Search", HistoryKind::Search),
        ] {
            assert_eq!(HistoryKind::from_name(name), Some(kind), "{name}");
        }
        for name in ["", "bogus", "cmdx", "x", " guess", "11"] {
            assert_eq!(HistoryKind::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn add_dedups_to_newest_with_fresh_hisnum() {
        let mut history = History::new();
        history.add(HistoryKind::Cmd, "one", 100);
        history.add(HistoryKind::Cmd, "two", 100);
        history.add(HistoryKind::Cmd, "one", 100);
        // The first hisnum is gone with the moved entry.
        assert_eq!(history.get(HistoryKind::Cmd, 1), None);
        assert_eq!(history.get(HistoryKind::Cmd, 2), Some("two"));
        assert_eq!(history.get(HistoryKind::Cmd, 3), Some("one"));
        assert_eq!(history.get(HistoryKind::Cmd, -1), Some("one"));
        assert_eq!(history.get(HistoryKind::Cmd, -2), Some("two"));
        assert_eq!(history.get(HistoryKind::Cmd, -3), None);
        assert_eq!(history.newest(HistoryKind::Cmd), 3);
    }

    #[test]
    fn zero_capacity_stores_nothing_and_keeps_counter() {
        let mut history = History::new();
        history.add(HistoryKind::Cmd, "lost", 0);
        assert_eq!(history.newest(HistoryKind::Cmd), -1);
        history.add(HistoryKind::Cmd, "kept", 100);
        assert_eq!(history.newest(HistoryKind::Cmd), 1);
    }

    #[test]
    fn capacity_wraps_off_the_oldest_end() {
        let mut history = History::new();
        history.add(HistoryKind::Cmd, "a", 2);
        history.add(HistoryKind::Cmd, "b", 2);
        history.add(HistoryKind::Cmd, "c", 2);
        assert_eq!(history.get(HistoryKind::Cmd, 1), None);
        assert_eq!(history.get(HistoryKind::Cmd, -2), Some("b"));
        assert_eq!(history.newest(HistoryKind::Cmd), 3);
    }

    #[test]
    fn index_delete_of_final_entry_reports_zero_nr() {
        let mut history = History::new();
        history.add(HistoryKind::Cmd, "solo", 100);
        assert!(history.delete_index(HistoryKind::Cmd, 1));
        assert_eq!(history.newest(HistoryKind::Cmd), 0);
        assert_eq!(history.get(HistoryKind::Cmd, -1), None);
        // The counter is not reset: the next add continues it.
        history.add(HistoryKind::Cmd, "again", 100);
        assert_eq!(history.newest(HistoryKind::Cmd), 2);
    }

    #[test]
    fn clear_resets_counter_while_pattern_delete_keeps_it() {
        let mut history = History::new();
        history.add(HistoryKind::Cmd, "foo one", 100);
        history.add(HistoryKind::Cmd, "bar", 100);
        history.add(HistoryKind::Cmd, "foo two", 100);
        assert!(history.delete_pattern(HistoryKind::Cmd, |text| text.contains("foo")));
        assert_eq!(history.get(HistoryKind::Cmd, -1), Some("bar"));
        assert_eq!(history.newest(HistoryKind::Cmd), 2);
        history.clear(HistoryKind::Cmd);
        assert_eq!(history.newest(HistoryKind::Cmd), -1);
        history.add(HistoryKind::Cmd, "fresh", 100);
        assert_eq!(history.newest(HistoryKind::Cmd), 1);
    }

    #[test]
    fn pattern_delete_to_empty_reads_cleared() {
        let mut history = History::new();
        history.add(HistoryKind::Cmd, "gone", 100);
        assert!(history.delete_pattern(HistoryKind::Cmd, |_| true));
        assert_eq!(history.newest(HistoryKind::Cmd), -1);
        assert!(!history.delete_pattern(HistoryKind::Cmd, |_| true));
    }
}
