//! Terminal input pre-filter.
//!
//! Crossterm's byte parser keeps an `ESC [`-introduced sequence in its
//! internal buffer until a final byte lets it decide what to decode —
//! for an `ESC [ ?`-introduced sequence whose final byte is neither `u`
//! nor `c` that decision never arrives, so a DECRPM or mode report
//! parks the parser and every later keystroke is swallowed. For a lead
//! byte it does not dispatch on (`>`, `=`, `!`, a raw `0x9b` C1 CSI, or
//! a `0x90`/`0x9d`/`0x9f` C1 string) it drops the introducer alone and
//! re-parses the parameter tail as typed text, leaking reply bytes into
//! the editor. Upstream (`nvim/tui/termkey/driver-csi.c`) instead
//! consumes a complete CSI or control string to its final byte and
//! emits nothing for an unknown command.
//!
//! Crossterm owns the byte stream: `event::poll`/`event::read` read the
//! terminal descriptor themselves and expose no seam for feeding them
//! pre-scanned bytes. This module therefore interposes a real pty — a
//! pump thread reads the terminal, runs the bytes through [`Filter`],
//! and writes what crossterm can consume to the pty master; the slave
//! becomes stdin before crossterm's source is created. Every sequence
//! crossterm decodes today (keys, CSI forms, mouse, paste, focus) is
//! forwarded byte-for-byte, so chunk boundaries and escape/alt timing
//! are unchanged; only the sequences it mangles are consumed first.

#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use rustix::io::dup;
use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt};
use rustix::stdio::{dup2_stdin, stdin};
use rustix::termios::{OptionalActions, Termios, isatty, tcgetattr, tcsetattr};

/// How long a held `ESC [` prefix may wait for its lead byte before the
/// held bytes are forwarded verbatim. Covers relay-chunked writes
/// (tmux, ssh) that split a sequence across reads while staying below
/// perceptible `Escape`-then-`[` latency.
const LEAD_PEEK: Duration = Duration::from_millis(10);

/// Largest pending sequence the filter holds. A control string without
/// a terminator is unbounded input, so once a dropped sequence exceeds
/// this its collected prefix is discarded and the tail is forwarded
/// verbatim rather than growing the buffer.
const MAX_PENDING: usize = 8192;

/// CSI lead bytes crossterm's `parse_csi` dispatches on. Any other lead
/// (`?` excepted nowhere — its non-`u`/`c` finals park the parser, and
/// undispatched leads leak their tails) is consumed by the filter.
const DISPATCHED_LEADS: &[u8] = b"[0123456789;<MIOZPQSDCABHF";

/// Saved state needed to restore the real terminal's mode on teardown.
struct FilteredTerminal {
    saved_attributes: Termios,
    real_terminal: OwnedFd,
}

static FILTERED: OnceLock<Mutex<Option<FilteredTerminal>>> = OnceLock::new();

/// Install the input pre-filter: replace stdin with a pty slave fed by a
/// pump thread that scans the real terminal's bytes.
///
/// Must run before crossterm's event source is created (the first
/// `event::poll`/`event::read`) and after the capability probe has read
/// its own raw replies from stdin — the probe reads stdin directly, so
/// an earlier install would consume the replies it waits for. When
/// stdin is not a terminal crossterm opens `/dev/tty` itself and there
/// is no descriptor to interpose, so nothing is installed.
///
/// # Errors
///
/// Returns an error when the pty or the descriptor plumbing fails; the
/// caller may continue unfiltered, since the error paths leave stdin
/// and the real terminal's mode untouched.
pub(crate) fn install() -> io::Result<()> {
    if !isatty(stdin()) {
        return Ok(());
    }
    install_inner()
}

fn install_inner() -> io::Result<()> {
    // A duplicate of the descriptor crossterm would read, kept as the
    // pump's source so the pump sees exactly the stream it replaces.
    let real_terminal = dup(stdin())?;
    let saved_attributes = tcgetattr(stdin())?;

    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let slave = open_slave(&master)?;

    // The slave takes over stdin: its input must already behave like the
    // raw stream crossterm expects before any raw-mode call reaches it.
    let mut slave_raw = tcgetattr(&slave)?;
    slave_raw.make_raw();
    tcsetattr(&slave, OptionalActions::Now, &slave_raw)?;

    // The real terminal must deliver unbuffered, unechoed bytes to the
    // pump for the whole session; crossterm's own raw-mode calls now
    // land on the slave. The saved attributes are restored by
    // `restore_terminal_mode` at session teardown.
    let mut real_raw = saved_attributes.clone();
    real_raw.make_raw();
    tcsetattr(stdin(), OptionalActions::Now, &real_raw)?;

    if let Err(error) = dup2_stdin(&slave) {
        let _ = tcsetattr(stdin(), OptionalActions::Now, &saved_attributes);
        return Err(error.into());
    }

    let pump = Pump {
        real: File::from(real_terminal.try_clone()?),
        master: File::from(master),
    };
    FILTERED
        .set(Mutex::new(Some(FilteredTerminal {
            saved_attributes,
            real_terminal,
        })))
        .ok();
    thread::spawn(move || pump.run());
    Ok(())
}

/// Restore the real terminal's saved mode. Called from
/// [`crate::terminal::TerminalSession::restore`] beside crossterm's own
/// raw-mode restore, which now applies to the filter pty's slave.
pub(crate) fn restore_terminal_mode() {
    let Some(mutex) = FILTERED.get() else {
        return;
    };
    let Ok(mut guard) = mutex.lock() else {
        return;
    };
    if let Some(terminal) = guard.take() {
        let _ = tcsetattr(
            &terminal.real_terminal,
            OptionalActions::Now,
            &terminal.saved_attributes,
        );
    }
}

#[cfg(target_os = "linux")]
fn open_slave(master: &OwnedFd) -> io::Result<OwnedFd> {
    Ok(ioctl_tiocgptpeer(
        master,
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    )?)
}

#[cfg(not(target_os = "linux"))]
fn open_slave(master: &OwnedFd) -> io::Result<OwnedFd> {
    let name = rustix::pty::ptsname(master, Vec::new())?;
    Ok(rustix::fs::open(
        name.as_c_str(),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?)
}

struct Pump {
    real: File,
    master: File,
}

impl Pump {
    fn run(mut self) {
        let real_fd = self.real.as_raw_fd();
        let Ok(mut poll) = Poll::new() else {
            return;
        };
        if poll
            .registry()
            .register(&mut SourceFd(&real_fd), Token(0), Interest::READABLE)
            .is_err()
        {
            return;
        }
        let mut events = Events::with_capacity(4);
        let mut filter = Filter::new();
        let mut out = Vec::with_capacity(4096);
        let mut buffer = [0_u8; 4096];
        loop {
            let read = match self.real.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            filter.feed(&buffer[..read], &mut out);
            // A pending `ESC [` at a read boundary may still grow into a
            // sequence crossterm mangles; wait out a relay's chunk gap for
            // the lead byte before letting the prefix through.
            let deadline = Instant::now() + LEAD_PEEK;
            while filter.needs_lead() && Instant::now() < deadline {
                events.clear();
                let remaining = deadline.saturating_duration_since(Instant::now());
                if poll.poll(&mut events, Some(remaining)).is_err() || events.is_empty() {
                    break;
                }
                match self.real.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(more) => filter.feed(&buffer[..more], &mut out),
                }
            }
            filter.finish_chunk(&mut out);
            if self.master.write_all(&out).is_err() {
                return;
            }
            out.clear();
        }
    }
}

/// What the filter decided about the sequence it is accumulating.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Bytes pass through unchanged.
    Normal,
    /// `ESC` collected, waiting on the next byte.
    Escape,
    /// `ESC [` or `0x9b` collected and still undecided: no payload byte
    /// has been classified yet.
    CsiLead,
    /// Inside a CSI crossterm consumes correctly: forward everything.
    CsiForward,
    /// Inside a CSI crossterm mangles: consume through the final byte.
    CsiDrop,
    /// A `200~` bracketed paste was forwarded: pass payload through
    /// until `ESC [ 201 ~` closes it.
    Paste,
    /// `ESC [` `M` was forwarded: the X10 mouse report's three
    /// coordinate bytes pass through untouched.
    PassBytes(u8),
    /// A forwarded `ESC`-led control string (`DCS`/`OSC`/`APC`): pass
    /// payload through until `BEL`, `ESC \`, or `0x9c`.
    StringForward,
    /// `ESC` seen inside a forwarded string; `\` ends it.
    StringForwardEscape,
    /// A C1-introduced control string: consume to `BEL`, `ESC \`, or
    /// `0x9c` without forwarding.
    StringDrop,
    /// `ESC` seen inside a dropped string; `\` ends it.
    StringDropEscape,
    /// `0x8f` C1 SS3: consume the one remaining byte.
    Ss3Drop,
}

/// Byte-level framer between the real terminal and crossterm's parser.
struct Filter {
    mode: Mode,
    /// Bytes of the sequence under classification, or collected payload
    /// of a consumed one.
    pending: Vec<u8>,
    /// Tail window detecting the `ESC [ 201 ~` paste terminator and the
    /// `ESC \` string terminator while bytes pass through.
    tail: [u8; 8],
    tail_len: usize,
}

impl Filter {
    fn new() -> Self {
        Self {
            mode: Mode::Normal,
            pending: Vec::with_capacity(64),
            tail: [0; 8],
            tail_len: 0,
        }
    }

    /// The held prefix is `ESC [` with no payload byte yet: crossterm
    /// would park on `?`-led tails and leak for undispatched leads, so
    /// the decision must wait for one more byte.
    fn needs_lead(&self) -> bool {
        self.mode == Mode::CsiLead
    }

    /// Feed one read's bytes; forwarding decisions go to `out` in order.
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &byte in bytes {
            self.step(byte, out);
        }
    }

    /// Read boundary with a held prefix: a bare `ESC` is forwarded so a
    /// lone `Escape` keeps its read-boundary semantics, and an undecided
    /// `ESC [` the peek timed out on goes through verbatim — crossterm's
    /// own buffering reassembles the rest.
    fn finish_chunk(&mut self, out: &mut Vec<u8>) {
        if matches!(self.mode, Mode::Escape | Mode::CsiLead) {
            out.extend_from_slice(&self.pending);
            self.pending.clear();
            self.mode = Mode::Normal;
        }
    }

    fn forward_pending(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pending);
        self.pending.clear();
    }

    #[allow(clippy::too_many_lines)]
    fn step(&mut self, byte: u8, out: &mut Vec<u8>) {
        match self.mode {
            Mode::Normal => match byte {
                0x1b => {
                    self.pending.push(byte);
                    self.mode = Mode::Escape;
                }
                0x9b => {
                    self.pending.push(byte);
                    self.mode = Mode::CsiLead;
                }
                0x90 | 0x9d | 0x9f => {
                    // C1 DCS/OSC/APC: crossterm drops the introducer and
                    // leaks the payload as text; consume to the terminator.
                    self.mode = Mode::StringDrop;
                }
                0x98 | 0x9e => {
                    // C1 SOS/PM: same mangling — upstream does not frame
                    // these either, but its parser does not re-leak them.
                    self.mode = Mode::StringDrop;
                }
                0x8f => {
                    self.mode = Mode::Ss3Drop;
                }
                0x9c => {
                    // Lone C1 ST: crossterm already discards it.
                }
                _ => self.emit(byte, out),
            },
            Mode::Escape => {
                if byte == b'[' {
                    self.pending.push(byte);
                    self.mode = Mode::CsiLead;
                } else {
                    // `ESC O`-led SS3 and `ESC P`/`]`/`_`-led strings are
                    // sequences crossterm already frames and ox-tui's
                    // event-level consume handles; forward the pair.
                    self.pending.push(byte);
                    self.forward_pending(out);
                    self.mode = match byte {
                        b'P' | b']' | b'_' => Mode::StringForward,
                        _ => Mode::Normal,
                    };
                }
            }
            Mode::CsiLead => {
                if byte == 0x1b {
                    // Aborted sequence: a new escape starts over.
                    self.pending.clear();
                    self.pending.push(byte);
                    self.mode = Mode::Escape;
                    return;
                }
                if (0x20..=0x3f).contains(&byte) {
                    self.pending.push(byte);
                    self.mode = if self.csi_forwardable() {
                        Mode::CsiForward
                    } else {
                        Mode::CsiDrop
                    };
                    return;
                }
                if (0x40..=0x7e).contains(&byte) {
                    // Single-shot CSI: no parameter bytes.
                    self.pending.push(byte);
                    if self.csi_forwardable() {
                        let x10 = self.pending.starts_with(b"\x1b[M");
                        self.forward_pending(out);
                        self.mode = if x10 {
                            Mode::PassBytes(3)
                        } else {
                            Mode::Normal
                        };
                    } else {
                        self.pending.clear();
                        self.mode = Mode::Normal;
                    }
                    return;
                }
                // A byte that cannot belong to a CSI aborts it: forward
                // the collected prefix, then handle the byte normally.
                self.forward_pending(out);
                self.mode = Mode::Normal;
                self.step(byte, out);
            }
            Mode::CsiForward | Mode::CsiDrop => {
                let dropped = self.mode == Mode::CsiDrop;
                if !(0x20..=0x7e).contains(&byte) {
                    // A byte outside the CSI alphabet aborts the
                    // sequence. The prefix is forwarded like upstream's
                    // Alt-[ + tail: crossterm's own parser discards the
                    // truncated attempt on the offending byte, so this
                    // cannot park it either. The byte is reprocessed.
                    self.forward_pending(out);
                    self.mode = Mode::Normal;
                    self.step(byte, out);
                    return;
                }
                self.pending.push(byte);
                if self.pending.len() > MAX_PENDING {
                    self.pending.clear();
                    self.mode = Mode::Normal;
                    return;
                }
                if !(0x40..=0x7e).contains(&byte) {
                    return;
                }
                // Final byte reached.
                if dropped {
                    self.pending.clear();
                    self.mode = Mode::Normal;
                    return;
                }
                let mut paste = false;
                let mut pass = 0_u8;
                if self.pending.starts_with(b"\x1b[200~") {
                    paste = true;
                } else if self.pending.starts_with(b"\x1b[M") {
                    pass = 3;
                }
                self.forward_pending(out);
                self.mode = if paste {
                    Mode::Paste
                } else if pass > 0 {
                    Mode::PassBytes(pass)
                } else {
                    Mode::Normal
                };
            }
            Mode::Paste => {
                self.emit(byte, out);
                if self.tail_ends(b"\x1b[201~") {
                    self.mode = Mode::Normal;
                }
            }
            Mode::PassBytes(remaining) => {
                self.emit(byte, out);
                match remaining.checked_sub(1) {
                    Some(0) | None => self.mode = Mode::Normal,
                    Some(rest) => self.mode = Mode::PassBytes(rest),
                }
            }
            Mode::StringForward => {
                self.emit(byte, out);
                if byte == 0x1b {
                    self.mode = Mode::StringForwardEscape;
                } else if byte == 0x07 || byte == 0x9c {
                    self.mode = Mode::Normal;
                }
            }
            Mode::StringForwardEscape => {
                self.emit(byte, out);
                if byte == b'\\' || byte == 0x9c {
                    self.mode = Mode::Normal;
                } else {
                    self.mode = Mode::StringForward;
                }
            }
            Mode::StringDrop => {
                if byte == 0x1b {
                    self.mode = Mode::StringDropEscape;
                } else if byte == 0x07 || byte == 0x9c {
                    self.mode = Mode::Normal;
                }
            }
            Mode::StringDropEscape => {
                self.mode = if byte == b'\\' || byte == 0x9c {
                    Mode::Normal
                } else {
                    Mode::StringDrop
                };
            }
            Mode::Ss3Drop => {
                self.mode = Mode::Normal;
            }
        }
    }

    /// Whether a complete CSI with this lead byte decodes inside
    /// crossterm rather than mangling: `?`-led sequences whose final is
    /// not `u`/`c` park its parser, other undispatched leads drop the
    /// introducer and leak the tail, and `0x9b`-led sequences are C1
    /// bytes crossterm cannot frame at all.
    fn csi_forwardable(&self) -> bool {
        if self.pending.first() == Some(&0x9b) {
            return false;
        }
        matches!(
            self.pending.get(2).copied(),
            Some(lead) if DISPATCHED_LEADS.contains(&lead)
        )
    }

    /// Emit a byte while keeping the terminator tail window current.
    fn emit(&mut self, byte: u8, out: &mut Vec<u8>) {
        out.push(byte);
        if self.tail_len < self.tail.len() {
            self.tail[self.tail_len] = byte;
            self.tail_len += 1;
        } else {
            self.tail.copy_within(1.., 0);
            self.tail[self.tail.len() - 1] = byte;
        }
    }

    fn tail_ends(&self, needle: &[u8]) -> bool {
        self.tail_len >= needle.len()
            && self.tail[self.tail_len - needle.len()..self.tail_len] == *needle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bytes: &[u8]) -> Vec<u8> {
        let mut filter = Filter::new();
        let mut out = Vec::new();
        filter.feed(bytes, &mut out);
        filter.finish_chunk(&mut out);
        out
    }

    #[test]
    fn decrpm_reply_is_consumed() {
        assert_eq!(run(b"\x1b[?2026;2$y"), b"");
        assert_eq!(run(b"\x1b[?1u"), b"");
        assert_eq!(run(b"\x1b[?64;1;2;6;9;15;22;28c"), b"");
        // Input after a consumed reply still flows.
        let mut all = b"\x1b[?2026;2$yKEPT".to_vec();
        all.extend_from_slice(b"");
        assert_eq!(run(&all), b"KEPT");
    }

    #[test]
    fn private_lead_replies_are_consumed() {
        assert_eq!(run(b"\x1b[>0;136;0c"), b"");
        assert_eq!(run(b"\x1b[=1;2c"), b"");
        assert_eq!(run(b"\x1b[!p"), b"");
        assert_eq!(run(b"\x1b[>4;2m"), b"");
    }

    #[test]
    fn c1_sequences_are_consumed() {
        assert_eq!(run(b"\x9b61;1R"), b"");
        assert_eq!(run(b"\x90P\x9c"), b"");
        assert_eq!(run(b"\x9d52;c;e30=\x9c"), b"");
        assert_eq!(run(b"\x9d52;c;e30=\x1b\\"), b"");
        assert_eq!(run(b"\x9d52;c;e30=\x07"), b"");
        assert_eq!(run(b"\x8fA"), b"");
        assert_eq!(run(b"pre\x9c"), b"pre");
    }

    #[test]
    fn known_sequences_pass_through() {
        for sequence in [
            b"\x1b[A".as_slice(),
            b"\x1b[H",
            b"\x1b[Z",
            b"\x1bOA",
            b"\x1b[24;80R",
            b"\x1b[97;5u",
            b"\x1b[5;5~",
            b"\x1b[I",
            b"\x1b[O",
            b"\x1b[<35;10;5M",
            b"\x1b[200~pasted\x1b[201~",
        ] {
            assert_eq!(run(sequence), sequence, "{sequence:?}");
        }
    }

    #[test]
    fn escape_and_text_pass_through() {
        assert_eq!(run(b"hello"), b"hello");
        assert_eq!(run(b"\x1b"), b"\x1b");
        assert_eq!(run(b"\x1bx"), b"\x1bx");
        assert_eq!(run(b"\x1bPpayload\x1b\\"), b"\x1bPpayload\x1b\\");
        assert_eq!(run(b"\x1b]8;;u\x07"), b"\x1b]8;;u\x07");
    }

    #[test]
    fn paste_payload_passes_verbatim() {
        // The payload contains a would-be dropped sequence and its own
        // terminator-looking fragment: everything until `201~` is data.
        let bytes = b"\x1b[200~a\x1b[?2$y\x9db\x1b[201~after";
        assert_eq!(run(bytes), bytes);
    }

    #[test]
    fn aborted_csi_forwards_then_resumes() {
        // A newline aborts the partial sequence; the prefix is not a
        // complete sequence, so it passes through like upstream's
        // timeout path, and the byte is reprocessed as input.
        assert_eq!(run(b"\x1b[?\n"), b"\x1b[?\n");
        assert_eq!(run(b"\x1b[>12\x1bA"), b"\x1b[>12\x1bA");
    }

    #[test]
    fn x10_mouse_payload_passes_verbatim() {
        let bytes = b"\x1b[M\x9b!!";
        assert_eq!(run(bytes), bytes);
    }
}
