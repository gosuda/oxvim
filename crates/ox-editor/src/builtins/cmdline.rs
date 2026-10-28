//! Command-line introspection builtins (`eval/funcs.c` `f_getcmd*`,
//! `f_setcmdpos`, `f_pumvisible`, `f_wildmenumode`).
//!
//! These read the live command line owned by [`Mode::Cmdline`] state on the
//! mode machine, or the [`PromptDialog`] backing `input()` (upstream's `@`
//! command line, `ex_getln.c:756-760`). The mode machine lives on
//! `ExRuntime`, not on `Editor`, so every entry snapshots what it needs
//! through a short `try_borrow` and drops it before touching the editor —
//! user code must never execute under a machine borrow.

use ox_eval::{EvalError, builtin_spec};
use ox_types::{OxStr, Typval};

use crate::excmd_exec::{EvalHost, ExEditorAccess};
use crate::mode::{CmdlineKind, Mode};
use crate::script::FileIO;
use crate::search::SearchDirection;

/// Routes one command-line builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    match name {
        "getcmdline" => Ok(Typval::String(OxStr::from(cmdline_text(host).as_str()))),
        "getcmdpos" => Ok(Typval::Number(cmdline_cursor(host))),
        "setcmdpos" => call_setcmdpos(host, args),
        "getcmdtype" => Ok(Typval::String(OxStr::from(cmdline_type(host)))),
        // `getcmdwintype` (`eval.c:f_getcmdwintype`): the command-line window's
        // type, or an empty string while no such window is open. This port has
        // no command-line window, so the answer is always empty, which is what
        // upstream returns outside `q:`, `q/` and `q?`.
        "getcmdwintype" => Ok(Typval::String(OxStr::from(""))),
        "getcmdprompt" => Ok(Typval::String(OxStr::from(cmdline_prompt(host).as_str()))),
        "pumvisible" => Ok(Typval::Number(i64::from(completion_visible(host)))),
        "wildmenumode" => Ok(Typval::Number(0)),
        _ => unreachable!("cmdline builtin route and dispatcher disagree"),
    }
}

/// Enforces the `eval.lua` argument counts the way upstream's function table
/// does before a builtin body runs.
fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = builtin_spec(name)
        .ok_or_else(|| EvalError::new("E117", 0, format!("Unknown function: {name}")))?;
    if count < spec.min_args {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    Ok(())
}

/// `tv_get_number` (`typval.c`): numbers pass, floats truncate, bools become
/// 0/1, strings parse their leading numeric prefix (else 0); containers are
/// `E728`/`E730`, anything else is `E729`.
fn tv_get_number(value: &Typval) -> ox_eval::Result<i64> {
    match value {
        Typval::Number(value) => Ok(*value),
        Typval::Float(value) => {
            // `as` is exact here: the range check proves the truncation
            // fits, and NaN/infinite magnitudes take the error arm instead
            // of saturating. The upper bound is exclusive: `i64::MAX as
            // f64` rounds up to 2^63, which does not fit, while every
            // `f64` below it truncates exactly. The lower bound is
            // inclusive: -2^63 is exactly representable.
            if value.is_finite() && *value >= i64::MIN as f64 && *value < i64::MAX as f64 {
                Ok(value.trunc() as i64)
            } else {
                Err(EvalError::new("E729", 0, "Using invalid value as a Number"))
            }
        }
        Typval::Bool(value) => Ok(i64::from(*value)),
        Typval::String(value) => Ok(number_prefix(&value.to_string_lossy())),
        Typval::List(_) => Err(EvalError::new("E730", 0, "Using a List as a Number")),
        Typval::Dict(_) => Err(EvalError::new("E728", 0, "Using a Dictionary as a Number")),
        _ => Err(EvalError::new("E729", 0, "Using invalid value as a Number")),
    }
}

/// Leading-numeric-prefix parse (`typval.c` `tv_get_number` string leg):
/// optional sign, then digits; no digits means 0.
fn number_prefix(text: &str) -> i64 {
    let bytes = text.trim_start().as_bytes();
    let mut end = 0;
    if bytes
        .first()
        .is_some_and(|byte| matches!(byte, b'+' | b'-'))
    {
        end = 1;
    }
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == 0 || (end == 1 && !bytes[0].is_ascii_digit()) {
        return 0;
    }
    std::str::from_utf8(&bytes[..end])
        .ok()
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

/// The live command-line text: the open `Mode::Cmdline` first, then the
/// `input()` dialog reply (`@`), else empty.
fn cmdline_text<F: FileIO, E: ExEditorAccess>(host: &EvalHost<'_, F, E>) -> String {
    if let Some(text) = host
        .runtime
        .mode_machine
        .as_ref()
        .and_then(|machine| machine.try_borrow().ok())
        .as_deref()
        .and_then(|machine| match machine.mode() {
            Mode::Cmdline(state) => Some(state.text.clone()),
            _ => None,
        })
    {
        return text;
    }
    host.access.with_ex_editor(|editor| {
        editor
            .prompt_dialog
            .as_ref()
            .map_or(String::new(), |dialog| {
                dialog.reply.to_string_lossy().into_owned()
            })
    })
}

/// 1-based character cursor for `getcmdpos()`; 0 when no command line is
/// open. Deferred `setcmdpos()` positions are deliberately invisible here:
/// upstream only applies `new_cmdpos` at the next `CTRL-R` paste.
fn cmdline_cursor<F: FileIO, E: ExEditorAccess>(host: &EvalHost<'_, F, E>) -> i64 {
    let (text, cursor_byte, active) = cmdline_snapshot(host);
    if !active {
        return 0;
    }
    let end = floor_char_boundary(&text, cursor_byte);
    i64::try_from(text[..end].chars().count() + 1).unwrap_or(i64::MAX)
}

/// `(line, cursor_byte, open)`: the raw snapshot behind [`cmdline_cursor`].
fn cmdline_snapshot<F: FileIO, E: ExEditorAccess>(
    host: &EvalHost<'_, F, E>,
) -> (String, usize, bool) {
    if let Some((text, cursor)) = host
        .runtime
        .mode_machine
        .as_ref()
        .and_then(|machine| machine.try_borrow().ok())
        .as_deref()
        .and_then(|machine| match machine.mode() {
            Mode::Cmdline(state) => Some((state.text.clone(), state.cursor_byte)),
            _ => None,
        })
    {
        return (text, cursor, true);
    }
    if let Some((reply, cursor)) = host.access.with_ex_editor(|editor| {
        editor
            .prompt_dialog
            .as_ref()
            .map(|dialog| (dialog.reply.to_string_lossy().into_owned(), dialog.cursor))
    }) {
        return (reply, cursor, true);
    }
    (String::new(), 0, false)
}

/// `:`/`/`/`?` for the open command line, `@` for `input()`, else empty.
fn cmdline_type<F: FileIO, E: ExEditorAccess>(host: &EvalHost<'_, F, E>) -> &'static str {
    if let Some(kind) = host
        .runtime
        .mode_machine
        .as_ref()
        .and_then(|machine| machine.try_borrow().ok())
        .as_deref()
        .and_then(|machine| match machine.mode() {
            Mode::Cmdline(state) => Some(state.kind),
            _ => None,
        })
    {
        return match kind {
            CmdlineKind::Ex => ":",
            CmdlineKind::Search(direction) => {
                if matches!(direction, SearchDirection::Forward) {
                    "/"
                } else {
                    "?"
                }
            }
        };
    }
    if host
        .access
        .with_ex_editor(|editor| editor.prompt_dialog.is_some())
    {
        return "@";
    }
    ""
}

/// Prompt text: the type character for `:`/`/`/`?`, the dialog prompt for
/// `input()`, else empty.
fn cmdline_prompt<F: FileIO, E: ExEditorAccess>(host: &EvalHost<'_, F, E>) -> String {
    if let Some(kind) = host
        .runtime
        .mode_machine
        .as_ref()
        .and_then(|machine| machine.try_borrow().ok())
        .as_deref()
        .and_then(|machine| match machine.mode() {
            Mode::Cmdline(state) => Some(state.kind),
            _ => None,
        })
    {
        return match kind {
            CmdlineKind::Ex => ":".to_owned(),
            CmdlineKind::Search(direction) => {
                if matches!(direction, SearchDirection::Forward) {
                    "/".to_owned()
                } else {
                    "?".to_owned()
                }
            }
        };
    }
    host.access.with_ex_editor(|editor| {
        editor
            .prompt_dialog
            .as_ref()
            .map_or(String::new(), |dialog| {
                dialog.prompt.to_string_lossy().into_owned()
            })
    })
}

/// `setcmdpos({pos})`: defers the cursor move into the session's `new_cmdpos`
/// slot, consumed by the next `CTRL-R` paste — never applied eagerly, so
/// `getcmdpos()` still reports the old position afterwards (`ex_getln.c`).
/// Returns 1 on success, 0 when `{pos} < 1`.
fn call_setcmdpos<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let position = tv_get_number(&args[0])?;
    if position < 1 {
        return Ok(Typval::Number(0));
    }
    // Outside an open command line there is no session slot; upstream keeps a
    // static one for the next command line, this port resets per session, so
    // there is nowhere to store it — still a success either way.
    if let Some(machine) = host
        .runtime
        .mode_machine
        .as_ref()
        .and_then(|machine| machine.try_borrow_mut().ok())
        .as_deref_mut()
        && let Mode::Cmdline(state) = machine.mode_mut()
    {
        state.new_cmdpos = Some(usize::try_from(position).unwrap_or(usize::MAX));
    }
    Ok(Typval::Number(1))
}

/// Whether the insert-completion popup is visible (`pum_visible()`). The
/// command line has no wildmenu/popup in this port, so this is only true
/// while an insert-mode completion list is live — otherwise an honest 0,
//  matching upstream outside completion.
fn completion_visible<F: FileIO, E: ExEditorAccess>(host: &EvalHost<'_, F, E>) -> bool {
    host.runtime
        .mode_machine
        .as_ref()
        .and_then(|machine| machine.try_borrow().ok())
        .is_some_and(|machine| {
            machine
                .completion
                .pum()
                .is_some_and(|pum| !pum.items.is_empty())
        })
}

/// Snaps a byte index down to the containing character boundary.
fn floor_char_boundary(text: &str, byte: usize) -> usize {
    let mut end = byte.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::{floor_char_boundary, number_prefix, tv_get_number};
    use ox_types::Typval;

    #[test]
    fn number_coercion_matches_tv_get_number() {
        assert_eq!(tv_get_number(&Typval::Number(3)).unwrap(), 3);
        // Numeric strings parse their leading prefix, the rest is 0.
        assert_eq!(number_prefix("  -12ab"), -12);
        assert_eq!(number_prefix("x"), 0);
        assert_eq!(number_prefix(""), 0);
        assert!(tv_get_number(&Typval::dict(vec![])).is_err());
        let error = tv_get_number(&Typval::dict(vec![])).unwrap_err();
        assert_eq!(error.code, "E728");
    }

    #[test]
    fn boundary_flooring_counts_characters() {
        let text = "éab";
        assert_eq!(floor_char_boundary(text, usize::MAX), text.len());
        assert_eq!(floor_char_boundary(text, 1), 0);
        assert_eq!(text[..floor_char_boundary(text, 2)].chars().count(), 1);
        assert_eq!(text[..floor_char_boundary(text, 3)].chars().count(), 2);
    }
}
