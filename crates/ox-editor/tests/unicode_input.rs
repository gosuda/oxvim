//! Real queued input must decode characters without changing its byte storage.

use ox_editor::{Editor, Keys, ModeMachine, Step, TypeaheadFlags};

fn queued_edit(
    text: &str,
    column: usize,
    keys: &str,
    cpoptions: Option<&str>,
) -> Result<(Vec<u8>, usize), Box<dyn std::error::Error>> {
    let mut editor = Editor::new();
    if let Some(value) = cpoptions {
        editor.options_mut().set_global(
            "cpoptions",
            ox_editor::OptionValue::String(value.to_owned()),
        )?;
    }
    let buffer = editor.create_buffer_with(ox_text::Buffer::from_bytes(text.as_bytes())?, true)?;
    let tab = editor.create_tabpage(buffer, ox_editor::Geometry::new(0, 0, 80, 24)?)?;
    let window = editor.tabpage(tab)?.current_window();
    editor.set_window_cursor(
        window,
        ox_text::Position {
            lnum: 1,
            col: column,
        },
    )?;
    editor
        .typeahead_mut()
        .append(&Keys::encode(keys.as_bytes()), TypeaheadFlags::default());
    let mut machine = ModeMachine::default();
    let mut eval = ox_editor::NullExprEval;
    while machine.run_once(&mut editor, &mut eval)? {}
    Ok((
        editor.buffer(buffer)?.text()?.to_bytes(),
        editor.window(window)?.cursor.col,
    ))
}

#[test]
fn unicode_find_targets_keep_scalar_identity_and_byte_columns()
-> Result<(), Box<dyn std::error::Error>> {
    let text = "x\u{e9}\u{3b1}\u{1f642}\u{e9}z";
    for (column, keys, expected) in [
        (0, "f\u{e9}", 1),
        (0, "2f\u{e9}", 9),
        (0, "f\u{e9};", 9),
        (0, "2f\u{e9},", 1),
        (0, "t\u{1f642}", 3),
        (11, "F\u{e9}", 9),
        (11, "2T\u{e9}", 3),
        (0, "t\u{e9};", 5),
        (0, "t\u{e9}2;", 5),
        (11, "T\u{e9};", 3),
        (11, "T\u{e9}2;", 3),
    ] {
        let (bytes, actual) = queued_edit(text, column, keys, None)?;
        assert_eq!(bytes, text.as_bytes(), "{keys:?}");
        assert_eq!(actual, expected, "{keys:?}");
        assert!(text.is_char_boundary(actual), "{keys:?}");
    }
    let (bytes, column) = queued_edit("x\u{100}y", 0, "f\u{100}", None)?;
    assert_eq!(bytes, "x\u{100}y".as_bytes());
    assert_eq!(column, 1);
    let (bytes, column) = queued_edit(text, 0, "t\u{e9};", Some(";"))?;
    assert_eq!(bytes, text.as_bytes());
    assert_eq!(column, 0);
    Ok(())
}

#[test]
fn unicode_find_operators_and_visual_ranges_include_whole_scalars()
-> Result<(), Box<dyn std::error::Error>> {
    let text = "x\u{e9}\u{3b1}\u{1f642}\u{e9}z";
    for (column, keys, expected, cursor) in [
        (0, "df\u{e9}", "\u{3b1}\u{1f642}\u{e9}z", 0),
        (0, "dt\u{1f642}", "\u{1f642}\u{e9}z", 0),
        (11, "dF\u{e9}", "x\u{e9}\u{3b1}\u{1f642}z", 9),
        (11, "dT\u{3b1}", "x\u{e9}\u{3b1}z", 5),
        (0, "cf\u{e9}Q\u{1b}", "Q\u{3b1}\u{1f642}\u{e9}z", 0),
        (0, "vf\u{e9}d", "\u{3b1}\u{1f642}\u{e9}z", 0),
        (11, "vF\u{e9}d", "x\u{e9}\u{3b1}\u{1f642}", 5),
    ] {
        let (bytes, actual) = queued_edit(text, column, keys, None)?;
        assert_eq!(bytes, expected.as_bytes(), "{keys:?}");
        assert_eq!(actual, cursor, "{keys:?}");
    }
    Ok(())
}

#[test]
fn queued_utf8_is_one_character_including_quoted_special_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let mut editor = Editor::new();
    let mut machine = ModeMachine::default();
    for character in ['é', '\u{100}', '\u{ac00}', '🙂'] {
        let mut bytes = [0; 4];
        let text = character.encode_utf8(&mut bytes);
        editor
            .typeahead_mut()
            .append(&Keys::encode(text.as_bytes()), TypeaheadFlags::default());
        assert_eq!(machine.check(&mut editor)?, Step::Key(character));
        assert!(editor.typeahead().is_empty());
    }
    Ok(())
}

#[test]
fn a_fragmented_scalar_waits_without_consuming_its_prefix() -> Result<(), Box<dyn std::error::Error>>
{
    let mut editor = Editor::new();
    let mut machine = ModeMachine::default();
    machine.set_no_more_input(false);
    for byte in [0xf0, 0x9f, 0x99] {
        editor
            .typeahead_mut()
            .append(&Keys::encode(&[byte]), TypeaheadFlags::default());
        assert_eq!(machine.check(&mut editor)?, Step::Idle);
    }
    assert_eq!(editor.typeahead().as_bytes(), &[0xf0, 0x9f, 0x99]);
    editor
        .typeahead_mut()
        .append(&Keys::encode(&[0x82, b'x']), TypeaheadFlags::default());
    assert_eq!(machine.check(&mut editor)?, Step::Key('🙂'));
    assert_eq!(machine.check(&mut editor)?, Step::Key('x'));
    Ok(())
}

#[test]
fn a_closed_input_drain_does_not_wait_for_more_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let mut editor = Editor::new();
    let mut machine = ModeMachine::default();
    editor
        .typeahead_mut()
        .append(&Keys::encode(&[0xc3]), TypeaheadFlags::default());
    assert_eq!(machine.check(&mut editor)?, Step::Key('Ã'));
    assert_eq!(machine.check(&mut editor)?, Step::Idle);
    Ok(())
}

#[test]
fn invalid_utf8_does_not_swallow_the_following_escape() -> Result<(), Box<dyn std::error::Error>> {
    let mut editor = Editor::new();
    let mut machine = ModeMachine::default();
    editor
        .typeahead_mut()
        .append(&Keys::encode(&[0xc3, 0x1b]), TypeaheadFlags::default());
    assert_eq!(machine.check(&mut editor)?, Step::Key('Ã'));
    assert_eq!(machine.check(&mut editor)?, Step::Key('\u{1b}'));
    assert!(editor.typeahead().is_empty());
    Ok(())
}
