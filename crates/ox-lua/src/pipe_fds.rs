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
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

/// Numbers one Lua host owns, and numbers that moved into a child slot.
#[derive(Default)]
pub(crate) struct Registry {
    owned: HashMap<i32, OwnedFd>,
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
    pub(crate) fn register_pair(&mut self, reader: OwnedFd, writer: OwnedFd) -> (i64, i64) {
        let read = reader.as_raw_fd();
        let write = writer.as_raw_fd();
        self.owned.insert(read, reader);
        self.owned.insert(write, writer);
        (i64::from(read), i64::from(write))
    }

    /// Takes a stored descriptor out, for a handle or a child slot to own.
    pub(crate) fn take(&mut self, descriptor: i32) -> Option<OwnedFd> {
        self.owned.remove(&descriptor)
    }

    /// Records that `descriptor` moved into a child slot, so the close Lua
    /// owes it answers success exactly once.
    pub(crate) fn mark_spawned(&mut self, descriptor: i32) {
        self.awaiting_close.insert(descriptor);
    }

    /// Answers whether `descriptor` was one of this host's pipe halves.
    ///
    /// A live entry is dropped, which closes it, and a spawned entry is
    /// retired; both count as a successful close. Anything else is not ours,
    /// and the caller keeps its existing `EBADF`.
    pub(crate) fn close(&mut self, descriptor: i32) -> bool {
        if self.owned.remove(&descriptor).is_some() {
            return true;
        }
        self.awaiting_close.remove(&descriptor)
    }
}
