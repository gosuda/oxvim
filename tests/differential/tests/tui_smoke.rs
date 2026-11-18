//! TUI smoke tests driving the PTY harness end to end (Unix only).
#![cfg(unix)]
// Pure integration test: expect/panic on harness failures IS the assertion.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(missing_docs)]

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use differential::{OXVIM, binary};
use ox_rpc::RedrawEvent;
use ox_tui::chrome::{
    Chrome, MessageFlag, MessageFlags, MessageLifetime, MessageUpdate, PopupItem, TextChunk, TimeMs,
};
use ox_tui::{MotionPolicy, TuiState};
use ox_types::{Dict, Object, OxStr};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn edits_quits_and_restores_terminal_palette() {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let mut command = CommandBuilder::new(binary(OXVIM));
    command.env("TERM", "xterm-256color");
    command.env("OXVIM_TUI_MOTION", "reduced");
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn release oxvim in PTY");
    drop(pair.slave);

    let (sender, receiver) = mpsc::channel();
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) if sender.send(buffer[..count].to_vec()).is_err() => break,
                Ok(_) => {}
            }
        }
    });
    let mut output = Vec::new();
    let mut parser = vt100::Parser::new(24, 80, 0);
    let mut writer = pair.master.take_writer().expect("take PTY writer");
    wait_for(
        &mut parser,
        &receiver,
        &mut output,
        "terminal setup",
        |screen| screen.hide_cursor(),
    );

    drive_edit_overlay_session(&receiver, &mut parser, &mut output, &mut writer);

    writer.write_all(b":q!\r").expect("send forced quit");
    writer.flush().expect("flush forced quit");

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll oxvim") {
            break status;
        }
        assert!(Instant::now() < deadline, "oxvim did not exit after :q!");
        thread::sleep(Duration::from_millis(10));
    };
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join PTY reader");
    output.extend(receiver.try_iter().flatten());

    assert_eq!(
        status.exit_code(),
        0,
        "PTY status {status}; output {}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        contains(&output, b"\x1b[0 q"),
        "cursor mode restore missing"
    );
    assert!(
        contains(&output, b"\x1b]104"),
        "OSC 104 palette restore missing"
    );
}

fn drive_edit_overlay_session(
    receiver: &mpsc::Receiver<Vec<u8>>,
    parser: &mut vt100::Parser,
    output: &mut Vec<u8>,
    writer: &mut impl Write,
) {
    writer
        .write_all(b"iHello\x1b")
        .expect("send insert sequence");
    writer.flush().expect("flush insert sequence");
    wait_for(
        parser,
        receiver,
        output,
        "inserted grid content",
        |screen| screen.contents().contains("Hello"),
    );

    writer
        .write_all(b":edit x")
        .expect("type command-line overlay");
    writer.flush().expect("flush command-line overlay");
    wait_for(
        parser,
        receiver,
        output,
        "rendered :edit x overlay",
        |screen| screen.contents().contains(":edit x"),
    );

    writer
        .write_all(b"\x12=1+1")
        .expect("type nested expression command line");
    writer
        .flush()
        .expect("flush nested expression command line");
    wait_for(
        parser,
        receiver,
        output,
        "rendered Ctrl-R = level",
        |screen| screen.contents().contains("1+1"),
    );

    writer
        .write_all(b"\x1b")
        .expect("cancel nested command line");
    writer.flush().expect("flush nested cancel");
    writer
        .write_all(b"\x1b")
        .expect("cancel outer command line");
    writer.flush().expect("flush outer cancel");
}

#[test]
fn cmdline_overlay_nesting_and_wildmenu_strip_follow_protocol_levels() {
    let mut chrome = Chrome::default();
    chrome.cmdline_show(1, vec![chunk("edit x")], 6, ":", None, 0, -1);
    let first = chrome
        .cmdline
        .active()
        .expect("command-line overlay")
        .clone();
    assert_eq!(first.level, 1);
    assert_eq!(first.content, vec![chunk("edit x")]);
    assert!(chrome.layout(80, 24, Some(0)).cmdline.is_some());

    chrome.cmdline_show(2, vec![chunk("1+1")], 3, "=", None, 0, -1);
    assert_eq!(
        chrome.cmdline.active().map(|level| level.level),
        Some(2),
        "Ctrl-R = opens level two"
    );
    assert!(chrome.cmdline_hide(2, true), "Esc hides nested level");
    assert_eq!(
        chrome.cmdline.active(),
        Some(&first),
        "nested Esc restores level one"
    );

    chrome.popupmenu_show(vec![PopupItem::new("one", "", "", "")], Some(0), 0, 0, -1);
    assert!(
        chrome
            .message_show(update("wildlist", "one  two", Object::Nil))
            .is_ok()
    );
    assert!(
        chrome.messages.is_empty(),
        "wildmenu text must be stripped from normal messages"
    );
    let layout = chrome.layout(80, 24, Some(0));
    assert!(layout.cmdline.is_some() && layout.wildmenu.is_some() && layout.wildlist.is_some());
}

#[test]
fn sticky_expiry_and_same_id_replacement_are_distinct() {
    let mut chrome = Chrome::default();
    assert!(
        chrome
            .message_show(update("emsg", "sticky", Object::Integer(1)))
            .is_ok()
    );
    assert!(
        chrome
            .message_show(update("echo", "old", Object::Integer(2)))
            .is_ok()
    );
    chrome.finish_batch(TimeMs(10));
    assert!(matches!(
        chrome.messages[0].lifetime,
        MessageLifetime::StickyUntilKeypress
    ));
    assert!(matches!(
        chrome.messages[1].lifetime,
        MessageLifetime::Expiring { .. }
    ));

    assert!(
        chrome
            .message_show(update("echo", "new", Object::Integer(2)))
            .is_ok()
    );
    chrome.finish_batch(TimeMs(20));
    assert_eq!(
        chrome.messages.len(),
        2,
        "same kind and id replaces in place"
    );
    assert_eq!(chrome.messages[1].content, vec![chunk("new")]);
    chrome.advance_time(TimeMs(4_020));
    assert_eq!(
        chrome.messages.len(),
        1,
        "echo expires after its stable batch"
    );
    chrome.keypress();
    assert!(
        chrome.messages.is_empty(),
        "error-kind message persists only to keypress"
    );
}

#[test]
fn colorscheme_highlights_retheme_at_one_batch_boundary() {
    let mut state = TuiState::new(Some("15;0"), MotionPolicy::Reduced);
    let generation = state.theme.generation();
    let event = RedrawEvent {
        name: OxStr::from("hl_attr_define"),
        argsets: vec![vec![
            Object::Integer(1),
            Object::Dict(Dict(vec![
                (OxStr::from("foreground"), Object::Integer(0x11_22_33)),
                (OxStr::from("background"), Object::Integer(0xee_dd_cc)),
            ])),
            Object::Dict(Dict(Vec::new())),
            Object::Array(vec![Object::Dict(Dict(vec![(
                OxStr::from("hi_name"),
                Object::String(OxStr::from("NormalFloat")),
            )]))]),
        ]],
    };
    state
        .apply_redraw(&[event], TimeMs(1))
        .expect("apply complete colorscheme batch");
    assert_eq!(state.theme.generation(), generation + 1);
    assert!(state.theme.received_highlights());
}

fn chunk(text: &str) -> TextChunk {
    TextChunk::new(0, text, -1)
}

fn update(kind: &str, text: &str, id: Object) -> MessageUpdate {
    let mut flags = MessageFlags::default();
    flags.set(MessageFlag::History, true);
    MessageUpdate {
        kind: OxStr::from(kind),
        content: vec![chunk(text)],
        flags,
        id,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Feeds the PTY stream into a terminal model and waits for a screen predicate.
///
/// Asserting on the parsed screen keeps the check independent of how the
/// client encodes a frame: a cell that was repainted is not the same thing as
/// a byte sequence, and a delta that skips blank cells carries no spaces for
/// the text to contain.
fn wait_for(
    parser: &mut vt100::Parser,
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    description: &str,
    ready: impl Fn(&vt100::Screen) -> bool,
) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if ready(parser.screen()) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {description}; screen:\n{}",
            parser.screen().contents()
        );
        if let Ok(bytes) = receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            parser.process(&bytes);
            output.extend(bytes);
        }
    }
}
