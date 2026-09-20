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
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const TIMEOUT: Duration = Duration::from_secs(10);

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    reader: Option<JoinHandle<()>>,
    incoming: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    transcript: Vec<u8>,
    directory: PathBuf,
    finished: bool,
}

impl Terminal {
    fn start(contents: &str) -> TestResult<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "oxvim-e2e-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory)?;
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
        let writer = pair.master.take_writer()?;
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
            master: pair.master,
            writer,
            reader: Some(reader),
            incoming,
            parser: vt100::Parser::new(24, 80, 0),
            transcript: Vec::new(),
            directory,
            finished: false,
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
        self.writer.write_all(input)?;
        self.writer.flush()?;
        Ok(())
    }

    fn resize(&mut self, rows: u16, columns: u16) -> TestResult {
        self.master.resize(PtySize {
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
