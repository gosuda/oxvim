//! Audited Unix system queries used by the safe runtime.
//!
//! Each entry point mirrors the libc call upstream Neovim (through libuv)
//! makes for the same datum: `gethostname` for the node name (`f_hostname`),
//! `getuid` for the real user id (`os_get_username`), and `getpwuid_r` for
//! the password-file record (`uv_os_get_passwd` — on macOS this resolves
//! through OpenDirectory, not `/etc/passwd`).

use std::ffi::{CStr, c_char};
use std::io;
use std::ptr;

/// The subset of a `passwd` record that `uv.os_get_passwd` surfaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasswdEntry {
    /// Login name.
    pub name: String,
    /// Numeric group id.
    pub gid: u32,
    /// Login shell.
    pub shell: String,
    /// Home directory.
    pub home: String,
}

/// The system node name from `gethostname(2)`, matching `f_hostname` →
/// `uv_os_gethostname`.
///
/// # Errors
///
/// Returns the `gethostname` errno on failure.
pub fn hostname() -> io::Result<String> {
    // POSIX leaves truncation behaviour undefined beyond HOST_NAME_MAX, so
    // one extra byte keeps a name that fills the buffer NUL-terminated.
    let mut buffer = [0 as c_char; 256];
    // SAFETY: `buffer` is writable for `len` bytes; the result is read only
    // up to the first NUL inside the same buffer.
    if unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len() as _) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let bytes = buffer[..end]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// The real user id of the process, like `getuid(2)` (`os/users.c` uses the
/// real uid for `os_get_username`).
#[must_use]
pub fn real_uid() -> u32 {
    // SAFETY: takes no pointers and cannot fail.
    unsafe { libc::getuid() }
}

/// The parent process id, like `getppid(2)`.
#[must_use]
pub fn parent_pid() -> u32 {
    // SAFETY: takes no pointers and cannot fail.
    u32::try_from(unsafe { libc::getppid() }).unwrap_or(0)
}

/// Whether `pid` currently names a live process, probing with `kill(pid, 0)`
/// (`uv_process_kill`'s no-signal contract): `ESRCH` means gone, while
/// `EPERM` still counts as alive.
#[must_use]
pub fn process_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` with signal 0 sends nothing; the descriptor is an
    // integer, not a pointer.
    let status = unsafe { libc::kill(raw, 0) };
    if status == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Password-file entry for `uid` via `getpwuid_r` (`uv_os_get_passwd`). On
/// macOS this resolves through OpenDirectory — the `/etc/passwd` file only
/// holds system accounts.
///
/// # Errors
///
/// Returns the `getpwuid_r` errno. A `uid` with no entry is `Ok(None)`, not
/// an error, matching libuv's "missing user" result.
pub fn passwd_entry(uid: u32) -> io::Result<Option<PasswdEntry>> {
    // SAFETY: `sysconf` takes no pointers; a negative return marks the limit
    // as indeterminate and a fixed starting size covers that.
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = usize::try_from(suggested).unwrap_or(0).max(4096);
    loop {
        let mut buffer = vec![0_u8; size];
        // SAFETY: plain-data struct; `getpwuid_r` fills the fields it knows.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = ptr::null_mut();
        // SAFETY: `entry` is writable, `buffer` is a `size`-byte scratch, and
        // `result` is set to `entry` on success or stays null.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                &raw mut entry,
                buffer.as_mut_ptr().cast::<c_char>(),
                buffer.len(),
                &raw mut result,
            )
        };
        if status == libc::ERANGE {
            size = size.saturating_mul(2);
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if result.is_null() {
            return Ok(None);
        }
        let field = |value: *const c_char| -> String {
            if value.is_null() {
                String::new()
            } else {
                // SAFETY: getpwuid_r returns NUL-terminated strings backed by
                // `buffer`, which outlives this read.
                unsafe { CStr::from_ptr(value) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        return Ok(Some(PasswdEntry {
            name: field(entry.pw_name),
            gid: entry.pw_gid,
            shell: field(entry.pw_shell),
            home: field(entry.pw_dir),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::{hostname, passwd_entry, real_uid};

    #[test]
    fn hostname_reports_the_kernel_node_name() {
        let name = hostname().expect("hostname");
        assert!(!name.is_empty());
    }

    #[test]
    fn passwd_entry_resolves_current_user() -> std::io::Result<()> {
        let uid = real_uid();
        let entry = passwd_entry(uid)?.expect("current uid resolves");
        assert!(!entry.name.is_empty());
        assert!(!entry.home.is_empty());
        Ok(())
    }
}
