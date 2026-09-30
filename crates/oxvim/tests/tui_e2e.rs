//! Black-box acceptance of the actual editor, RPC client, and terminal output.
//!
//! The independent VT parser retains visible cells and cursor state across
//! fragmented writes. Old output containing a word cannot satisfy an assertion
//! about the current screen. Failed sessions retain their terminal transcript.

use std::error::Error;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const TIMEOUT: Duration = Duration::from_secs(10);
// Each session owns a ConPTY host plus a render loop; concurrent sessions
// starve one another and lose cursor visibility races, so tests run serially.
static SERIAL: Mutex<()> = Mutex::new(());

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    // Optional so `finish` can close the ConPTY: on Windows the read pipe
    // stays open until the master is dropped, and joining the reader would
    // otherwise block forever after the child exits.
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    writer: Box<dyn Write + Send>,
    reader: Option<JoinHandle<()>>,
    incoming: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    transcript: Vec<u8>,
    directory: PathBuf,
    finished: bool,
    answered_cursor_query: bool,
    _serial: MutexGuard<'static, ()>,
}

impl Terminal {
    fn start(contents: &str) -> TestResult<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Failed sessions keep their directory for the transcript dump, so a
        // reused pid can collide with leftovers; skip to the next free name.
        let directory = loop {
            let candidate = std::env::temp_dir().join(format!(
                "oxvim-e2e-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let home = directory.join("home");
        fs::create_dir(&home)?;
        fs::write(directory.join("document.txt"), contents)?;
        let pair = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_oxvim"));
        command.args(["-u", "NONE", "-i", "NONE", "-n", "document.txt"]);
        command.cwd(&directory);
        for key in [
            "HOME",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            command.env(key, &home);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("COLORFGBG", "15;0");
        command.env("OXVIM_TUI_MOTION", "reduced");
        command.env(
            "VIMRUNTIME",
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime"),
        );
        let mut output = pair.master.try_clone_reader()?;
        let writer: Box<dyn Write + Send> = pair.master.take_writer()?;
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let (sender, incoming) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut bytes = [0; 8192];
            loop {
                match output.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if sender.send(bytes[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Self {
            child,
            master: Some(pair.master),
            writer,
            reader: Some(reader),
            incoming,
            parser: vt100::Parser::new(24, 80, 0),
            transcript: Vec::new(),
            directory,
            finished: false,
            answered_cursor_query: false,
            _serial: serial,
        })
    }

    fn receive(&mut self, timeout: Duration) {
        if let Ok(bytes) = self.incoming.recv_timeout(timeout) {
            self.parser.process(&bytes);
            self.transcript.extend(bytes);
        }
        while let Ok(bytes) = self.incoming.try_recv() {
            self.parser.process(&bytes);
            self.transcript.extend(bytes);
        }
        self.answer_cursor_query();
    }

    // portable-pty creates the console with PSUEDOCONSOLE_INHERIT_CURSOR, so
    // conhost emits `ESC[6n` and waits for the hosting terminal's
    // `ESC[<row>;<col>R` reply before it keeps servicing the child's output —
    // an unanswered query also deadlocks `ClosePseudoConsole`. The reply must
    // arrive while conhost is actually waiting: sent ahead of the query it is
    // forwarded to the child as input instead, which both drops it and leaks
    // a stray keypress. Answer only once the query itself is visible.
    fn answer_cursor_query(&mut self) {
        if self.answered_cursor_query {
            return;
        }
        if self
            .transcript
            .windows(4)
            .any(|window| window == b"\x1b[6n")
        {
            self.answered_cursor_query = true;
            let _ = self.writer.write_all(b"\x1b[1;1R");
            let _ = self.writer.flush();
        }
    }

    fn wait(&mut self, description: &str, ready: impl Fn(&vt100::Screen) -> bool) -> TestResult {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.receive(Duration::from_millis(10));
            if ready(self.parser.screen()) {
                return Ok(());
            }
            if Instant::now() >= deadline || self.child.try_wait()?.is_some() {
                return Err(io::Error::other(format!(
                    "waiting for {description}; cursor {:?}, hidden={}; artifacts: {}\n{}",
                    self.parser.screen().cursor_position(),
                    self.parser.screen().hide_cursor(),
                    self.directory.display(),
                    self.parser.screen().contents()
                ))
                .into());
            }
        }
    }

    fn send(&mut self, input: &[u8]) -> TestResult {
        // ConPTY WIN32 input mode parses bytes as a VT stream, so a lone ESC
        // waits forever for a sequence tail (and `ESC ESC` decodes as the
        // distinct `Alt+Esc` key). The Esc key itself goes in as a win32
        // input-mode record: `ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`, where
        // VK_ESCAPE is 27 and its scan code is 1.
        #[cfg(windows)]
        let input = {
            const ESCAPE_KEY: &[u8] = b"\x1b[27;1;27;1;0;1_";
            let mut framed = Vec::with_capacity(input.len());
            for (index, &byte) in input.iter().enumerate() {
                if byte == 0x1b && input.get(index + 1) != Some(&b'[') {
                    framed.extend_from_slice(ESCAPE_KEY);
                } else {
                    framed.push(byte);
                }
            }
            framed
        };
        // ConPTY's input VT parser keeps only ~256 bytes of a pending escape
        // sequence: a win32-input record that straddles that boundary is
        // dropped while the text around it still lands. Split the stream so
        // every `ESC[...final` unit stays inside one write (and each write is
        // under the parser's window) — the way a real terminal's bytes
        // actually arrive.
        let mut index = 0;
        let mut write_from = 0;
        let mut limit = input.len().min(256);
        while index < input.len() {
            if input[index] == 0x1b && index + 1 < input.len() {
                // Flush the accumulated text run before the sequence.
                if index > write_from {
                    self.writer.write_all(&input[write_from..index])?;
                    self.writer.flush()?;
                    std::thread::sleep(Duration::from_millis(1));
                }
                // A CSI runs to its final byte (0x40-0x7E, which covers both
                // `ESC[` sequences and `_`-terminated win32 records).
                let mut end = index + 2;
                while end < input.len() && !(0x40..=0x7e).contains(&input[end]) {
                    end += 1;
                }
                end = (end + 1).min(input.len());
                self.writer.write_all(&input[index..end])?;
                self.writer.flush()?;
                std::thread::sleep(Duration::from_millis(1));
                index = end;
                write_from = index;
                limit = index + 256;
            } else if index >= limit {
                self.writer.write_all(&input[write_from..index])?;
                self.writer.flush()?;
                std::thread::sleep(Duration::from_millis(1));
                write_from = index;
                limit = index + 256;
            } else {
                index += 1;
            }
        }
        if write_from < input.len() {
            self.writer.write_all(&input[write_from..])?;
            self.writer.flush()?;
        }
        Ok(())
    }

    fn resize(&mut self, rows: u16, columns: u16) -> TestResult {
        self.master
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal already finished"))?
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        self.parser.screen_mut().set_size(rows, columns);
        Ok(())
    }

    fn finish(&mut self) -> TestResult {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.receive(Duration::from_millis(10));
            if let Some(status) = self.child.try_wait()? {
                assert!(status.success(), "editor exit: {status}");
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("editor did not exit after quit").into());
            }
        }
        // Child exit and the last restore bytes race through conhost: drain
        // until the restore tail is visible (or give it a moment) before
        // closing the master, which would drop whatever is in flight. On
        // Windows `]104` never reaches the stream, so wait on the cursor
        // restore tail (`ESC[0 q`) instead.
        #[cfg(unix)]
        let tail: &[u8] = b"\x1b]104";
        #[cfg(windows)]
        let tail: &[u8] = b"\x1b[0 q";
        let drain_deadline = Instant::now() + Duration::from_secs(2);
        while !self
            .transcript
            .windows(tail.len())
            .any(|bytes| bytes == tail)
            && Instant::now() < drain_deadline
        {
            self.receive(Duration::from_millis(10));
        }
        // Drop the master before joining: the Windows ConPTY read end only
        // reaches EOF once the owning side closes.
        drop(self.master.take());
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| io::Error::other("PTY reader panicked"))?;
        }
        self.receive(Duration::ZERO);
        assert!(
            !self.parser.screen().hide_cursor(),
            "cursor hidden after exit"
        );
        // Windows conhost consumes OSC `]104` itself (it owns the console
        // palette) instead of forwarding the sequence onto the ConPTY stream,
        // so the palette-restore wire check only holds on unix.
        #[cfg(unix)]
        assert!(
            self.transcript.windows(5).any(|bytes| bytes == b"\x1b]104"),
            "palette not restored"
        );
        self.finished = true;
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.finished || thread::panicking() {
            let _ = fs::write(self.directory.join("terminal.ansi"), &self.transcript);
            let _ = fs::write(
                self.directory.join("screen.txt"),
                self.parser.screen().contents(),
            );
        }
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Close the ConPTY so the reader's blocking read reaches EOF.
        drop(self.master.take());
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if self.finished && !thread::panicking() {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

#[test]
fn editing_keeps_the_cursor_visible_and_saves_exact_bytes() -> TestResult {
    let mut terminal = Terminal::start("anchor\n")?;
    terminal.wait("initial visible editor cursor", |screen| {
        screen.contents().contains("anchor") && !screen.hide_cursor()
    })?;
    terminal.send(b"iHello")?;
    terminal.wait("inserted text and cursor", |screen| {
        screen.contents().contains("Helloanchor")
            && !screen.hide_cursor()
            && screen.cursor_position() == (0, 5)
    })?;
    terminal.send(b"\x1b")?;
    terminal.wait("normal-mode cursor", |screen| {
        !screen.hide_cursor() && screen.cursor_position() == (0, 4)
    })?;
    terminal.send(b"l")?;
    terminal.wait("cursor-only redraw", |screen| {
        !screen.hide_cursor() && screen.cursor_position() == (0, 5)
    })?;
    terminal.send(b":wq\r")?;
    terminal.finish()?;
    assert_eq!(
        fs::read(terminal.directory.join("document.txt"))?,
        b"Helloanchor\n"
    );
    Ok(())
}

#[test]
fn unicode_find_repeat_and_delete_reach_the_real_terminal() -> TestResult {
    let text = "x\u{e9}\u{3b1}\u{1f642}\u{e9}z\n";
    let mut terminal = Terminal::start(text)?;
    terminal.wait("Unicode find fixture", |screen| {
        screen.contents().contains(text.trim_end()) && !screen.hide_cursor()
    })?;
    terminal.send("f\u{e9}".as_bytes())?;
    terminal.wait("first Unicode target", |screen| {
        screen.cursor_position() == (0, 1)
    })?;
    terminal.send(b";")?;
    terminal.wait("repeated Unicode target", |screen| {
        screen.cursor_position() == (0, 5)
    })?;
    terminal.send(b",")?;
    terminal.wait("reverse Unicode target", |screen| {
        screen.cursor_position() == (0, 1)
    })?;
    terminal.send("0df\u{e9}".as_bytes())?;
    let expected = "\u{3b1}\u{1f642}\u{e9}z\n";
    terminal.wait("whole-scalar deletion", |screen| {
        screen.contents().starts_with(expected.trim_end()) && screen.cursor_position() == (0, 0)
    })?;
    terminal.send(b":wq\r")?;
    terminal.finish()?;
    assert_eq!(
        fs::read(terminal.directory.join("document.txt"))?,
        expected.as_bytes()
    );
    Ok(())
}

#[test]
fn command_line_cursor_tracks_utf8_and_cursor_only_updates() -> TestResult {
    let mut terminal = Terminal::start("anchor\n")?;
    terminal.wait("initial editor frame", |screen| {
        screen.contents().contains("anchor") && !screen.hide_cursor()
    })?;
    terminal.send(":echo 'caf\u{e9}Z".as_bytes())?;
    terminal.wait("cursor after command-line text", |screen| {
        let (row, column) = screen.cursor_position();
        screen.contents().contains(":echo 'caf\u{e9}Z")
            && !screen.hide_cursor()
            && column
                .checked_sub(1)
                .and_then(|left| screen.cell(row, left))
                .is_some_and(|cell| cell.contents() == "Z")
    })?;
    terminal.send(b"\x1b[D")?;
    terminal.wait(
        "command-line cursor moved left without changing text",
        |screen| {
            let (row, column) = screen.cursor_position();
            !screen.hide_cursor()
                && screen
                    .cell(row, column)
                    .is_some_and(|cell| cell.contents() == "Z")
        },
    )?;
    terminal.send(b"X")?;
    terminal.wait("insertion at the command-line cursor", |screen| {
        screen.contents().contains(":echo 'caf\u{e9}XZ")
    })?;
    terminal.send(b"\x7f\x7f")?;
    terminal.wait(
        "backspace removes complete characters before the cursor",
        |screen| screen.contents().contains(":echo 'cafZ") && !screen.contents().contains('é'),
    )?;
    terminal.send(b"\x1b")?;
    terminal.wait(
        "command-line cancellation restores editor cursor",
        |screen| {
            !screen.contents().contains(":echo")
                && !screen.hide_cursor()
                && screen.cursor_position() == (0, 0)
        },
    )?;
    terminal.send(b":q!\r")?;
    terminal.finish()
}

#[test]
fn unicode_insertion_arrows_undo_and_redo_preserve_file_bytes() -> TestResult {
    let mut terminal = Terminal::start("anchor\n")?;
    terminal.wait("initial editor frame", |screen| {
        screen.contents().contains("anchor") && !screen.hide_cursor()
    })?;
    terminal.send("icafé\u{ac00}🙂".as_bytes())?;
    terminal.wait("Unicode insertion and display-width cursor", |screen| {
        screen.contents().contains("café\u{ac00}🙂anchor")
            && screen.cursor_position() == (0, 8)
            && !screen.hide_cursor()
    })?;
    terminal.send(b"\x1b[D")?;
    terminal.wait("insert-mode cursor before the wide character", |screen| {
        screen.cursor_position() == (0, 6) && !screen.hide_cursor()
    })?;
    terminal.send(b"X\x1b")?;
    terminal.wait("insertion before the wide character", |screen| {
        screen.contents().contains("café\u{ac00}X🙂anchor")
    })?;
    terminal.send(b"u")?;
    terminal.wait("undo the insertion after the arrow movement", |screen| {
        screen.contents().contains("café\u{ac00}🙂anchor") && !screen.contents().contains('X')
    })?;
    terminal.send(b"\x12")?;
    terminal.wait("redo the insertion", |screen| {
        screen.contents().contains("café\u{ac00}X🙂anchor")
    })?;
    terminal.send(b":wq\r")?;
    terminal.finish()?;
    assert_eq!(
        fs::read(terminal.directory.join("document.txt"))?,
        "café\u{ac00}X🙂anchor\n".as_bytes()
    );
    Ok(())
}

#[test]
fn single_burst_of_keys_edits_and_saves_exact_bytes() -> TestResult {
    let mut terminal = Terminal::start("keep\n")?;
    terminal.wait("initial editor frame", |screen| {
        screen.contents().contains("keep") && !screen.hide_cursor()
    })?;
    // One write carrying insert text, the Esc transition, a motion and the
    // write-quit command: typeahead must segment it the same as separate
    // keystrokes.
    let mut burst = Vec::from(&b"i"[..]);
    for _ in 0..40 {
        burst.extend_from_slice(b"BURST-");
    }
    burst.extend_from_slice(b"\x1b0:wq\r");
    terminal.send(&burst)?;
    terminal.finish()?;
    let expected = format!("{}keep\n", "BURST-".repeat(40));
    assert_eq!(
        fs::read(terminal.directory.join("document.txt"))?,
        expected.as_bytes()
    );
    Ok(())
}

#[test]
fn resize_storm_leaves_a_consistent_editable_session() -> TestResult {
    let mut terminal = Terminal::start("storm\n")?;
    terminal.wait("initial editor frame", |screen| {
        screen.contents().contains("storm") && !screen.hide_cursor()
    })?;
    // Resize back-to-back with no settling time: every intermediate size is
    // transient, only the last matters, and the editor must not drop keys
    // buffered across the storm.
    for (rows, columns) in [
        (10, 40),
        (30, 100),
        (12, 30),
        (50, 120),
        (24, 80),
        (8, 20),
        (40, 90),
        (24, 80),
    ] {
        terminal.resize(rows, columns)?;
    }
    terminal.send(b"iEND\x1b:wq\r")?;
    terminal.finish()?;
    assert_eq!(
        fs::read(terminal.directory.join("document.txt"))?,
        b"ENDstorm\n"
    );
    Ok(())
}

#[test]
fn invalid_bytes_and_broken_sequences_do_not_wedge_the_editor() -> TestResult {
    let mut terminal = Terminal::start("sane\n")?;
    terminal.wait("initial editor frame", |screen| {
        screen.contents().contains("sane") && !screen.hide_cursor()
    })?;
    // Invalid UTF-8, NUL/DEL, an unknown-but-complete kitty CSI and a
    // padding key release record are all undeliverable keys; the editor may
    // beep but must keep servicing real input afterward. No dangling tails:
    // a pending sequence would swallow the following key on ConPTY.
    terminal.send(b"\xff\xfe\x80\x00\x7f\x1b[>0;1u\x1b[65;1;65;0;0;1_")?;
    terminal.send(b"G")?;
    terminal.wait("motion still lands after garbage input", |screen| {
        let (row, _column) = screen.cursor_position();
        row == 0 && !screen.hide_cursor()
    })?;
    terminal.send(b"dd:wq\r")?;
    terminal.finish()?;
    assert_eq!(fs::read(terminal.directory.join("document.txt"))?, b"\n");
    Ok(())
}

#[test]
fn killing_the_editor_mid_session_reaps_everything() -> TestResult {
    let mut terminal = Terminal::start("doomed\n")?;
    terminal.wait("initial editor frame", |screen| {
        screen.contents().contains("doomed") && !screen.hide_cursor()
    })?;
    terminal.send(b"inever-saved")?;
    terminal.receive(Duration::from_millis(300));
    terminal.child.kill()?;
    let deadline = Instant::now() + TIMEOUT;
    while terminal.child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            return Err(io::Error::other("killed editor did not exit").into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // `finish` asserts a clean quit, so close the session manually: drop the
    // master for reader EOF, join it, then hand the terminal to Drop.
    drop(terminal.master.take());
    if let Some(reader) = terminal.reader.take() {
        reader
            .join()
            .map_err(|_| io::Error::other("PTY reader panicked"))?;
    }
    terminal.finished = true;
    Ok(())
}

#[test]
fn deep_file_end_edit_saves_byte_exact_content() -> TestResult {
    let mut document = String::new();
    for line in 0..3000 {
        document.push_str("line ");
        document.push_str(&line.to_string());
        document.push('\n');
    }
    let mut terminal = Terminal::start(&document)?;
    terminal.wait("initial editor frame", |screen| {
        screen.contents().contains("line 0") && !screen.hide_cursor()
    })?;
    terminal.send(b"G")?;
    terminal.wait("cursor on the last line", |screen| {
        screen.contents().contains("line 2999") && !screen.hide_cursor()
    })?;
    terminal.send(b"A END\x1b:wq\r")?;
    terminal.finish()?;
    let expected = format!("{} END\n", document.trim_end());
    assert_eq!(
        fs::read(terminal.directory.join("document.txt"))?,
        expected.into_bytes()
    );
    Ok(())
}

#[test]
fn resizing_updates_the_editor_and_keeps_the_cursor_inside_the_terminal() -> TestResult {
    let mut terminal = Terminal::start("anchor\n")?;
    terminal.wait("initial editor frame", |screen| {
        screen.contents().contains("anchor") && !screen.hide_cursor()
    })?;
    for (rows, columns, expected) in [(10, 32, "32x10"), (24, 80, "80x24")] {
        terminal.resize(rows, columns)?;
        terminal.send(b":echo &columns . 'x' . &lines")?;
        terminal.wait("command input remains live during resize", |screen| {
            screen.contents().contains("&columns")
        })?;
        terminal.send(b"\r")?;
        terminal.wait("editor acknowledged the PTY resize", |screen| {
            let (row, column) = screen.cursor_position();
            screen.contents().contains(expected)
                && row < rows
                && column < columns
                && !screen.hide_cursor()
        })?;
    }
    terminal.send(b":q!\r")?;
    terminal.finish()
}
