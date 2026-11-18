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
//! * `uv.spawn` duplicates a half for each child slot: `luvref.txt` counts
//!   numeric `stdio` entries as inherited descriptors, not consumed ones,
//!   so the same number may fill stdout and stderr alike, and the parent
//!   keeps the original until Lua closes it;
//! * `uv.fs_close` retires a stored entry.
//!
//! One registry belongs to one Lua host: it is created at install time and
//! shared by reference with every closure that needs it. `fs_close` consults
//! the real file-handle map before this registry — the synthetic file
//! descriptors live in their own band far above any raw OS number, so the
//! two owners can never share a number.

use std::cell::RefCell;
#[cfg(unix)]
use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

/// Numbers one Lua host owns. They only exist where `uv.pipe`, `pipe:open`
/// and `uv.spawn`'s stdio path run — all `#[cfg(unix)]` — so on other
/// platforms the registry is empty by construction and `fs_close`'s consult
/// always reports `EBADF`.
#[derive(Default)]
pub(crate) struct Registry {
    #[cfg(unix)]
    owned: HashMap<i32, OwnedFd>,
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
        self.owned.insert(read, reader);
        self.owned.insert(write, writer);
        (i64::from(read), i64::from(write))
    }

    /// Takes a stored descriptor out, for a handle to own.
    #[cfg(unix)]
    pub(crate) fn take(&mut self, descriptor: i32) -> Option<OwnedFd> {
        self.owned.remove(&descriptor)
    }

    /// Duplicates a stored descriptor for a child slot.
    ///
    /// `uv.spawn`'s numeric `stdio` entries are inherited, not consumed: a
    /// number may fill several slots at once (`{nil, p.write, p.write}`),
    /// and a successful spawn still leaves the original in the parent's
    /// hands until `fs_close` retires it.
    #[cfg(unix)]
    pub(crate) fn dup(&self, descriptor: i32) -> std::io::Result<Option<OwnedFd>> {
        self.owned
            .get(&descriptor)
            .map(OwnedFd::try_clone)
            .transpose()
    }

    /// Answers whether `descriptor` was one of this host's pipe halves.
    ///
    /// A live entry is dropped, which closes it — that counts as a
    /// successful close. Anything else is not ours, and the caller keeps
    /// its existing `EBADF`.
    #[cfg(unix)]
    pub(crate) fn close(&mut self, descriptor: i32) -> bool {
        self.owned.remove(&descriptor).is_some()
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

    /// Each half closes exactly once; the second `fs_close` answers EBADF.
    #[test]
    fn a_number_closes_once() {
        let (reader, writer) = pair();
        let mut registry = Registry::default();
        let (read, write) = registry.register_pair(reader, writer);
        let read = i32::try_from(read).expect("descriptor fits i32");
        let write = i32::try_from(write).expect("descriptor fits i32");

        assert!(registry.close(read), "first close retires the live half");
        assert!(!registry.close(read), "second close answers EBADF");
        assert!(registry.close(write), "the other half closes once");
        assert!(!registry.close(write), "and only once");
    }

    /// Duplicating a half for a child slot keeps the original live: several
    /// slots may inherit the same number, and `fs_close` still owns the
    /// parent's copy afterwards.
    #[test]
    fn dup_keeps_the_original_closable() {
        let (reader, writer) = pair();
        let mut registry = Registry::default();
        let (_read, write) = registry.register_pair(reader, writer);
        let write = i32::try_from(write).expect("descriptor fits i32");

        let first = registry.dup(write).expect("dup works");
        let second = registry.dup(write).expect("dup works again");
        assert!(first.is_some() && second.is_some(), "two slots inherit");
        assert!(
            registry.close(write),
            "the original still closes after handing out copies"
        );
        assert!(
            registry.dup(write).expect("dup answers").is_none(),
            "a retired number has nothing left to copy"
        );
    }

    /// A number taken by `pipe:open` leaves nothing behind: the handle owns
    /// it, and `fs_close` reports EBADF rather than closing it a second time.
    #[test]
    fn a_taken_number_is_no_longer_ours() {
        let (reader, writer) = pair();
        let mut registry = Registry::default();
        let (_read, write) = registry.register_pair(reader, writer);
        let write = i32::try_from(write).expect("descriptor fits i32");

        let taken = registry.take(write);
        assert!(taken.is_some(), "the half came out");
        assert!(
            !registry.close(write),
            "a number already taken has no close to answer"
        );
    }
}
