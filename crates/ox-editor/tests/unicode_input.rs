//! Real queued input must decode characters without changing its byte storage.

use ox_editor::{Editor, Keys, ModeMachine, Step, TypeaheadFlags};

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
