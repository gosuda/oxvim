//! Descriptor registry behind `uv.pipe`, `pipe:open`, `uv.spawn` and
//! `uv.fs_close`.
//!
//! luv hands Lua bare descriptor numbers, so the port has to remember which
//! numbers it owns and where each one goes. `runtime/lua/vim/_core/system.lua`
//! builds a pair with `uv.pipe({}, {})`, adopts one half with `pipe:open(fd)`,
//! passes the other to `uv.spawn` as a `stdio` entry, and finally
//! `uv.fs_close`s both numbers. Ownership moves accordingly:
//!
//! * `uv.pipe` creates both halves and stores them here;
//! * `pipe:open` takes a half out, and the handle owns it from then on;
//! * `uv.spawn` takes a half out and hands it to the child slot, so the
//!   parent's copy closes with the spawn; the number is then recorded as
//!   awaiting a close, so the first `uv.fs_close` answers true and a second
//!   one reports `EBADF`;
//! * `uv.fs_close` retires either kind of entry.
//!
//! One registry belongs to one Lua host: it is created at install time and
//! shared by reference with every closure that needs it. Descriptor numbers
//! are reused by the operating system, so a live entry always wins over a
//! stale awaiting-close marker, and `fs_close` consults the real file-handle
//! map before this registry, because a number can belong to `uv.fs_open` at
//! the same time.

use std::cell::RefCell;
#[cfg(unix)]
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

/// Numbers one Lua host owns, and numbers that moved into a child slot.
/// Both kinds only exist where `uv.pipe`, `pipe:open` and `uv.spawn`'s stdio
/// path run — all `#[cfg(unix)]` — so on other platforms the registry is
/// empty by construction and `fs_close`'s consult always reports `EBADF`.
#[derive(Default)]
pub(crate) struct Registry {
    #[cfg(unix)]
    owned: HashMap<i32, OwnedFd>,
    #[cfg(unix)]
    awaiting_close: HashSet<i32>,
}

/// Shared handle to one host's registry.
pub(crate) type PipeRegistry = Rc<RefCell<Registry>>;

impl Registry {
    /// Creates an empty registry for one Lua host.
    pub(crate) fn new() -> PipeRegistry {
        Rc::new(RefCell::new(Self::default()))
    }

    /// Stores both halves of a fresh pair and answers their numbers.
    #[cfg(unix)]
    pub(crate) fn register_pair(&mut self, reader: OwnedFd, writer: OwnedFd) -> (i64, i64) {
        let read = reader.as_raw_fd();
        let write = writer.as_raw_fd();
        // The operating system reuses numbers: a marker left by an earlier
        // spawn must not outlive the new live entry for the same number.
        self.awaiting_close.remove(&read);
        self.awaiting_close.remove(&write);
        self.owned.insert(read, reader);
        self.owned.insert(write, writer);
        (i64::from(read), i64::from(write))
    }

    /// Takes a stored descriptor out, for a handle or a child slot to own.
    #[cfg(unix)]
    pub(crate) fn take(&mut self, descriptor: i32) -> Option<OwnedFd> {
        self.awaiting_close.remove(&descriptor);
        self.owned.remove(&descriptor)
    }

    /// Records that `descriptor` moved into a child slot, so the close Lua
    /// owes it answers success exactly once.
    #[cfg(unix)]
    pub(crate) fn mark_spawned(&mut self, descriptor: i32) {
        self.awaiting_close.insert(descriptor);
    }

    /// Answers whether `descriptor` was one of this host's pipe halves.
    ///
    /// A live entry is dropped, which closes it, and a spawned entry is
    /// retired; both count as a successful close. Anything else is not ours,
    /// and the caller keeps its existing `EBADF`.
    #[cfg(unix)]
    pub(crate) fn close(&mut self, descriptor: i32) -> bool {
        let live = self.owned.remove(&descriptor);
        // Drop any marker for the same number either way: a reused number
        // must not keep a stale success path.
        self.awaiting_close.remove(&descriptor);
        live.is_some()
    }

    /// No number can be one of this host's pipe halves here: the pipe
    /// constructors are `#[cfg(unix)]`, so `fs_close` keeps its `EBADF`.
    #[cfg(not(unix))]
    pub(crate) fn close(&mut self, _descriptor: i32) -> bool {
        false
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::fd::OwnedFd;

    use super::Registry;

    fn pair() -> (OwnedFd, OwnedFd) {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        (reader.into(), writer.into())
    }

    /// A number can be both live and marked as spawned. Closing it must retire
    /// the live entry once and never report a second success from the marker.
    #[test]
    fn stale_marker_never_reports_a_second_close() {
        let (reader, writer) = pair();
        let mut registry = Registry::default();
        let (read, write) = registry.register_pair(reader, writer);
        let read = i32::try_from(read).expect("descriptor fits i32");
        let write = i32::try_from(write).expect("descriptor fits i32");
        registry.mark_spawned(read);

        assert!(registry.close(read), "first close retires the live half");
        assert!(!registry.close(read), "second close answers EBADF");
        assert!(registry.close(write), "the other half closes once");
        assert!(!registry.close(write), "and only once");
    }

    /// Taking a half for a handle or a child slot drops any marker for that
    /// number, so the later `fs_close` cannot succeed without an owner.
    #[test]
    fn taking_a_number_clears_its_marker() {
        let (reader, writer) = pair();
        let mut registry = Registry::default();
        let (_read, write) = registry.register_pair(reader, writer);
        let write = i32::try_from(write).expect("descriptor fits i32");

        registry.mark_spawned(write);
        let taken = registry.take(write);
        assert!(taken.is_some(), "the half came out");
        assert!(
            !registry.close(write),
            "a number already taken has no close to answer"
        );
    }
}
