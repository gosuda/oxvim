//! `<...>` notation resolution: multi-byte named keys and modifier-prefixed
//! named keys must decode to their internal keycodes instead of arriving as
//! literal text (`mapping.c` lhs/rhs and the `nvim_input` termcode path both
//! run `Keys::parse_notation`).

use ox_editor::{
    K_SPECIAL, KS_EXTRA, KS_MODIFIER, Keys, MOD_MASK_ALT, MOD_MASK_CTRL,
};

fn notation(text: &str) -> Vec<u8> {
    Keys::parse_notation(text, "\\", "\\").as_bytes().to_vec()
}

#[test]
fn modified_cursor_keys_resolve_through_the_names_table() {
    // `<C-Down>` has no dedicated keycode upstream (`keycodes.c`
    // `modifier_keys_table` only lists S-Up/S-Down and C-Left/C-Right), so it
    // stays the KS_MODIFIER prefix plus the `K_DOWN` triple.
    assert_eq!(
        notation("<C-Down>"),
        vec![K_SPECIAL, KS_MODIFIER, MOD_MASK_CTRL, K_SPECIAL, b'k', b'd']
    );
    assert_eq!(
        notation("<C-Up>"),
        vec![K_SPECIAL, KS_MODIFIER, MOD_MASK_CTRL, K_SPECIAL, b'k', b'u']
    );
    assert_eq!(
        notation("<A-Left>"),
        vec![K_SPECIAL, KS_MODIFIER, MOD_MASK_ALT, K_SPECIAL, b'k', b'l']
    );
    // `simplify_key` folds (SHIFT, K_DOWN) into the dedicated `KE_S_DOWN`.
    assert_eq!(notation("<S-Down>"), vec![K_SPECIAL, KS_EXTRA, 5]);
    assert_eq!(notation("<S-Tab>"), vec![K_SPECIAL, b'k', b'B']);
    assert_eq!(notation("<C-Left>"), vec![K_SPECIAL, KS_EXTRA, 85]);
    // Two modifiers keep the surviving one on the simplified key.
    assert_eq!(
        notation("<C-S-Left>"),
        vec![K_SPECIAL, KS_MODIFIER, MOD_MASK_CTRL, K_SPECIAL, b'#', b'4']
    );
}

#[test]
fn multi_byte_named_keys_resolve() {
    assert_eq!(notation("<Down>"), vec![K_SPECIAL, b'k', b'd']);
    assert_eq!(notation("<PageUp>"), vec![K_SPECIAL, b'k', b'P']);
    assert_eq!(notation("<End>"), vec![K_SPECIAL, b'@', b'7']);
    assert_eq!(notation("<Insert>"), vec![K_SPECIAL, b'k', b'I']);
    assert_eq!(notation("<k5>"), vec![K_SPECIAL, b'K', b'H']);
}

#[test]
fn function_keys_past_f9_resolve() {
    assert_eq!(notation("<F10>"), vec![K_SPECIAL, b'k', b';']);
    assert_eq!(notation("<F12>"), vec![K_SPECIAL, b'F', b'2']);
    assert_eq!(notation("<F37>"), vec![K_SPECIAL, b'F', b'R']);
    assert_eq!(notation("<S-F10>"), vec![K_SPECIAL, KS_EXTRA, 15]);
}

#[test]
fn single_byte_and_unknown_names_keep_prior_semantics() {
    // Single-byte names still emit the plain byte (the documented named gap
    // keeps `<Del>` at `0x7f` rather than `K_DEL`).
    assert_eq!(notation("<Del>"), vec![0x7f]);
    assert_eq!(notation("<Esc>"), vec![0x1b]);
    assert_eq!(notation("<C-a>"), vec![0x01]);
    // Unknown names remain literal text.
    assert_eq!(notation("<bogus>"), b"<bogus>".to_vec());
}
