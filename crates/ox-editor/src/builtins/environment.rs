//! Ambient-environment builtins: working directory, system clock, standard
//! paths, shell quoting, display width, highlight table, and event-loop state.

use std::rc::Rc;

use crate::excmd_exec::ExEditorAccess;
use crate::mode::Mode;
use crate::options::OptionValue;
use crate::script::{FileIO, StdPath};
use crate::userfunc::{WatchCallback, WatchTarget};
use crate::visual::VisualKind;
use crate::{DirectoryScope, Editor};
use ox_eval::EvalError;
use ox_eval::Scope;
use ox_eval::ScopeKind;
use ox_eval::eval::CallArg;
use ox_types::{OxStr, Typval};

use super::input_string_arg;
use crate::builtins::position::cell_width;
use crate::excmd_exec::{EvalHost, change_directory, typval_number, typval_to_text};

/// Routes one ambient-environment builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    match name {
        "getcwd" => host
            .access
            .with_ex_editor(|editor| call_getcwd_builtin(editor, args)),
        "chdir" => host
            .access
            .with_ex_editor(|editor| call_chdir_builtin(editor, args)),
        // `f_defer` through `add_defer` (`eval/userfunc.c` 3390-3484).
        "defer" => call_defer_builtin(host, args),
        "eventhandler" => Ok(Typval::Number(0)),
        // `f_api_info` (eval/funcs.c:450-454): the metadata Object
        // re-expressed as a Typval. Reading the canonical table is
        // infallible in practice; a decode failure is a broken build.
        "api_info" => Ok(crate::excmd_exec::object_to_typval(
            &ox_rpc::canonical_metadata()
                .map_err(|err| EvalError::new("E5009", 0, format!("api_info: {err}")))?,
        )),
        "hlID" => host
            .access
            .with_ex_editor(|editor| call_hl_id_builtin(editor, args)),
        "highlight_exists" | "hlexists" => host
            .access
            .with_ex_editor(|editor| call_hlexists_builtin(editor, args)),
        "shellescape" => host
            .access
            .with_ex_editor(|editor| call_shellescape_builtin(editor, args)),
        "mode" => call_mode_builtin(host, args),
        "state" => call_state_builtin(host, args),
        "swapname" => {
            if args.len() > 1 {
                return Err(EvalError::new(
                    "E118",
                    0,
                    "Too many arguments for function: swapname",
                ));
            }
            crate::excmd_exec::swapname_builtin(host.runtime, host.access, args)
        }
        "stdpath" => call_stdpath_builtin(args),
        "strdisplaywidth" => host
            .access
            .with_ex_editor(|editor| call_strdisplaywidth_builtin(editor, args)),
        "strftime" => call_strftime_builtin(args),
        "shiftwidth" => host
            .access
            .with_ex_editor(|editor| call_shiftwidth_builtin(editor, args)),
        "haslocaldir" => host
            .access
            .with_ex_editor(|editor| call_haslocaldir_builtin(editor, args)),
        "bufloaded" => host
            .access
            .with_ex_editor(|editor| call_bufloaded_builtin(editor, args)),
        "dictwatcheradd" => call_dictwatcher_add_builtin(host, args),
        "dictwatcherdel" => call_dictwatcher_del_builtin(host, args),
        _ => unreachable!("environment builtin route and dispatcher disagree"),
    }
}

/// Routes one ambient-environment builtin with per-argument identities.
///
/// Only `dictwatcheradd()`/`dictwatcherdel()` read identities: a bare
/// `g:`/`b:`/`w:`/`t:` argument keeps the live scope map's identity, which
/// resolves to [`WatchTarget::Scope`], while a Dict-valued variable like
/// `g:d` keeps its slot identity and stays [`WatchTarget::Dict`]. Every
/// other builtin runs through [`call`] with plain values.
pub(crate) fn call_with_identities<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: Vec<CallArg>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match name {
        "dictwatcheradd" => call_dictwatcher_add_scoped(host, &args, scope),
        "dictwatcherdel" => call_dictwatcher_del_scoped(host, &args, scope),
        _ => {
            let values = args.into_iter().map(|arg| arg.value).collect::<Vec<_>>();
            call(host, name, &values)
        }
    }
}
/// `f_defer` through `add_defer` (`eval/userfunc.c` 3390-3484): register `{fn}`
/// — a Funcref or a function-name String — with its remaining arguments on
/// the innermost user-function frame, to be fired when that frame ends.
///
/// Registration is intentionally permissive: an unknown function is tolerated
/// ("it might be defined later", `userfunc.c` 3434); resolution happens at
/// fire time. Outside any function `can_add_defer` (3457-3464) gives
/// `E193: defer not inside a function`.
///
/// Dict partials are rejected with `E1300: Cannot use a partial with
/// dictionary for :defer`.
fn call_defer_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: defer",
        ));
    }
    if !host.runtime.can_add_defer() {
        return Err(EvalError::new("E193", 0, "defer not inside a function"));
    }
    // `userfunc.c:3398-3412`: a Partial's bound arguments come first, then
    // the arguments given to `defer()` itself.
    match &args[0] {
        Typval::Funcref(function) | Typval::Partial(function) => {
            if function.dict.is_some() {
                return Err(EvalError::new(
                    "E1300",
                    0,
                    "Cannot use a partial with dictionary for :defer",
                ));
            }
            // The value is deferred whole: a lambda's `<lambda>N` resolves
            // only through the closure registry captured in its Funcref, so
            // re-dispatching the bare name at fire time would hit `E117`.
            // Bound arguments stay inside the callee — `call_value` fires
            // them before `defer()`'s own.
            host.runtime
                .push_deferred_call_value(args[0].clone(), args[1..].to_vec());
        }
        value => {
            let name = input_string_arg(value)?;
            let text = name.to_string_lossy();
            if text.is_empty() {
                return Err(EvalError::new("E129", 0, "Function name required"));
            }
            host.runtime
                .push_deferred_call(text.into_owned(), args[1..].to_vec());
        }
    }
    Ok(Typval::Number(0))
}

/// `f_stdpath` (`eval/funcs.c:7011-7040`). The single-directory selectors
/// answer a String and `config_dirs`/`data_dirs` answer a List, per
/// `get_xdg_var_list`; an unrecognised selector is `E6100`
/// (`eval/funcs.c:7038`).
///
/// This is line 1 of every lazy.nvim bootstrap, and it resolves through the
/// same XDG helpers `'runtimepath'` is built from ([`crate::script::stdpath`]),
/// so the path a plugin manager installs into and the rtp entry it expects to
/// be found on are one answer, not two.
fn call_stdpath_builtin(args: &[Typval]) -> ox_eval::Result<Typval> {
    let what =
        input_string_arg(args.first().ok_or_else(|| {
            EvalError::new("E119", 0, "Not enough arguments for function: stdpath")
        })?)?;
    let what = what.to_string_lossy();
    let Some(selector) = StdPath::parse(&what) else {
        return Err(EvalError::new(
            "E6100",
            0,
            format!("\"{what}\" is not a valid stdpath"),
        ));
    };
    let mut dirs = crate::script::stdpath(selector);
    if selector.is_list() {
        return Ok(Typval::list(
            dirs.into_iter()
                .map(|dir| Typval::String(OxStr::from(dir.as_str())))
                .collect(),
        ));
    }
    Ok(Typval::String(OxStr::from(
        dirs.pop().unwrap_or_default().as_str(),
    )))
}

/// `f_shellescape` (`eval/funcs.c:6660-6667`) through
/// `vim_strsave_shellescape` (`strings.c:186-290`).
///
/// The whole string is single-quoted, `'` becomes `'\''`, and `!`/newline gain
/// a backslash when the shell is csh-like or the caller asked for special
/// handling -- two backslashes when both hold. A csh-like or fish-like shell is
/// decided by the tail of `'shell'` (`option.c:7095-7104`), so this reads the
/// editor's option rather than `$SHELL`.
fn call_shellescape_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let text = input_string_arg(args.first().ok_or_else(|| {
        EvalError::new("E119", 0, "Not enough arguments for function: shellescape")
    })?)?;
    let do_special = args.get(1).is_some_and(non_zero_arg);
    let shell = match editor.options().get_global("shell") {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => String::new(),
    };
    let tail = shell.rsplit('/').next().unwrap_or(shell.as_str());
    let csh_like = tail.contains("csh");
    let fish_like = tail.contains("fish");

    let mut escaped = vec![b'\''];
    let mut rest = text.as_bytes();
    while let Some((byte, tail)) = rest.split_first() {
        match byte {
            b'\'' => escaped.extend_from_slice(b"'\\''"),
            b'\n' | b'!' if csh_like || do_special => {
                escaped.push(b'\\');
                if csh_like && do_special {
                    escaped.push(b'\\');
                }
                escaped.push(*byte);
            }
            b'\\' if fish_like => {
                escaped.push(b'\\');
                escaped.push(*byte);
            }
            _ => {
                if do_special && let Some(length) = cmdline_var_length(rest) {
                    escaped.push(b'\\');
                    escaped.extend_from_slice(&rest[..length]);
                    rest = &rest[length..];
                    continue;
                }
                escaped.push(*byte);
            }
        }
        rest = tail;
    }
    escaped.push(b'\'');
    Ok(Typval::String(OxStr(escaped)))
}

/// The cmdline special-file names `find_cmdline_var` recognises
/// (`ex_docmd.c:7491-7508`), longest-safe order: no entry is a prefix of
/// another, so first match wins as it does upstream.
const CMDLINE_VARS: [&[u8]; 15] = [
    b"%",
    b"#",
    b"<cword>",
    b"<cWORD>",
    b"<cexpr>",
    b"<cfile>",
    b"<sfile>",
    b"<slnum>",
    b"<stack>",
    b"<script>",
    b"<afile>",
    b"<abuf>",
    b"<amatch>",
    b"<sflnum>",
    b"<SID>",
];

fn cmdline_var_length(text: &[u8]) -> Option<usize> {
    CMDLINE_VARS
        .iter()
        .find(|name| text.starts_with(name))
        .map(|name| name.len())
}

/// `f_strdisplaywidth` (`strings.c:2775-2785`): `linetabsize_col(col, s) - col`,
/// so the answer depends on where on screen the text starts and on the
/// buffer's `'tabstop'` -- a tab is measured to the next stop, not as one
/// cell. That is what separates it from `strwidth`, which the typval-only
/// table already serves.
fn call_strdisplaywidth_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let text = input_string_arg(args.first().ok_or_else(|| {
        EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: strdisplaywidth",
        )
    })?)?;
    let start = args.get(1).and_then(typval_number).unwrap_or(0).max(0);
    let tabstop = match editor
        .current_buffer()
        .map(|buffer| editor.options().get_buffer(buffer, "tabstop"))
    {
        Some(Ok(OptionValue::Number(value))) if *value > 0 => usize::try_from(*value).unwrap_or(8),
        _ => 8,
    };
    let mut vcol = usize::try_from(start).unwrap_or(0);
    let begin = vcol;
    for character in String::from_utf8_lossy(text.as_bytes()).chars() {
        vcol += cell_width(character, vcol, tabstop);
    }
    Ok(Typval::Number(
        i64::try_from(vcol - begin).unwrap_or(i64::MAX),
    ))
}

/// `non_zero_arg` (`eval/funcs.c`): a non-zero Number, or a non-empty String
/// that is not `"0"`.
fn non_zero_arg(value: &Typval) -> bool {
    match value {
        Typval::Number(number) => *number != 0,
        Typval::Bool(flag) => *flag,
        Typval::Float(number) => *number != 0.0,
        Typval::String(text) => !text.as_bytes().is_empty() && text.as_bytes() != b"0",
        _ => false,
    }
}

/// `f_chdir` (`eval/fs.c:400-435`): change the directory of `{scope}`, or of
/// the narrowest scope holding a local directory when `{scope}` is absent:
/// buffer-local, else window-local, else tabpage-local, else global.
/// Returns the previous directory, empty when the change failed or the old
/// directory could not be read.
fn call_chdir_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::new(
            if args.is_empty() { "E119" } else { "E118" },
            0,
            "Invalid arguments for chdir",
        ));
    }
    let Typval::String(path) = &args[0] else {
        return Ok(Typval::String(OxStr::from("")));
    };
    let scope = match args.get(1) {
        None => {
            // Narrowest scope currently holding a local directory, else
            // global (`f_chdir`'s `curbuf`/`curwin`/`curtab` chain).
            let buffer_local = editor
                .current_buffer()
                .and_then(|buffer| editor.buffer_local_directory(buffer).ok().flatten())
                .is_some();
            if buffer_local {
                DirectoryScope::Buffer
            } else {
                let window_local = editor
                    .current_window()
                    .and_then(|window| editor.window_local_directory(window).ok().flatten())
                    .is_some();
                if window_local {
                    DirectoryScope::Window
                } else {
                    let tab_local = editor
                        .current_tabpage()
                        .and_then(|tab| editor.tab_local_directory(tab).ok().flatten())
                        .is_some();
                    if tab_local {
                        DirectoryScope::Tabpage
                    } else {
                        DirectoryScope::Global
                    }
                }
            }
        }
        Some(value) => {
            let coerced;
            let scope = match value {
                Typval::String(scope) => scope,
                value => {
                    coerced = input_string_arg(value)?;
                    &coerced
                }
            };
            match scope.as_bytes() {
                b"global" => DirectoryScope::Global,
                b"window" => DirectoryScope::Window,
                b"tabpage" => DirectoryScope::Tabpage,
                b"buffer" => DirectoryScope::Buffer,
                _ => {
                    return Err(EvalError::new(
                        "E475",
                        0,
                        format!(
                            "Invalid value for argument scope: {}",
                            scope.to_string_lossy()
                        ),
                    ));
                }
            }
        }
    };
    let previous = change_directory(editor, &path.to_string_lossy(), scope)?;
    Ok(Typval::String(OxStr::from(
        previous.to_string_lossy().as_ref(),
    )))
}

fn call_strftime_builtin(args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: strftime",
        ));
    }
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: strftime",
        ));
    }
    if typval_to_text(&args[0]) != "%H:%M:%S" {
        return Err(EvalError::not_implemented(OxStr::from("strftime format")));
    }
    let timestamp = args.get(1).and_then(typval_number).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            })
    });
    let seconds = timestamp.rem_euclid(86_400);
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    Ok(Typval::String(OxStr::from(
        format!("{hours:02}:{minutes:02}:{seconds:02}").as_str(),
    )))
}

fn call_hlexists_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        return Err(EvalError::new(
            if args.is_empty() { "E119" } else { "E118" },
            0,
            "hlexists() requires one argument",
        ));
    }
    let name = input_string_arg(&args[0])?;
    let name = name.to_string_lossy();
    Ok(Typval::Number(i64::from(
        editor
            .highlights()
            .keys()
            .any(|candidate| candidate.eq_ignore_ascii_case(&name)),
    )))
}

/// `f_mode` (`eval/funcs.c:4454`): the full mode string, truncated to its
/// major character when the optional argument is absent or zero. This port
/// models the same major/minor shapes `get_mode` builds: `n`/`no`, `i`, `R`,
/// `c`, `t`/`nt` for terminal buffers, and `v`/`V`/`CTRL-V`.
fn call_mode_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: mode",
        ));
    }
    let full = host
        .runtime
        .mode_machine
        .as_ref()
        .and_then(|machine| {
            let machine = machine.try_borrow().ok()?;
            let terminal = host.access.with_ex_editor(|editor| {
                editor
                    .current_buffer()
                    .is_some_and(|buffer| editor.is_terminal_buffer(buffer))
            });
            let name: &str = match machine.mode() {
                Mode::Normal(_) if terminal => "nt",
                Mode::Normal(_) => "n",
                Mode::Insert(_) if terminal => "t",
                Mode::Insert(_) => "i",
                Mode::Replace(_) => "R",
                Mode::Cmdline(_) => "c",
                Mode::OperatorPending(_) => "no",
                Mode::Visual(state) => match state.kind {
                    VisualKind::Character => "v",
                    VisualKind::Line => "V",
                    VisualKind::Block => "\u{16}",
                },
            };
            Some(name.to_owned())
        })
        .unwrap_or_else(|| "n".to_owned());
    let keep_minor = args
        .first()
        .is_some_and(|value| value.is_truthy() || typval_number(value).unwrap_or(0) != 0);
    let text = if keep_minor {
        full
    } else {
        full.chars()
            .next()
            .map_or_else(String::new, |c| c.to_string())
    };
    Ok(Typval::String(OxStr::from(text.as_str())))
}

/// `state([{what}])` (`eval/funcs.c:4385-4416`): flag characters for the
/// editor's current busyness, filtered to `{what}` when given.
///
/// Each character reads live session state: `m` while typeahead waits,
/// `o` for a pending operator, `a` while insert completion is active, `x`
/// while an autocommand plan runs, and `c` per callback nesting level.
/// `S` marks a non-safe state, which is exactly when one of the other
/// conditions holds. `s` (messages scrolled off screen) has no counterpart
/// in this port's message model and is never emitted.
fn call_state_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: state",
        ));
    }
    // `may_add_state_char` (`eval/funcs.c:4372`): an empty `{what}` matches
    // no flag, so `state('')` is always `""`; only a missing `{what}` lifts
    // the filter.
    let include = match args.first() {
        None => None,
        Some(value) => Some(input_string_arg(value)?.to_string_lossy().into_owned()),
    };
    let completion_active = host
        .runtime
        .mode_machine
        .as_ref()
        .is_some_and(|machine| machine.borrow().completion.is_active());
    let typeahead_busy = host
        .access
        .with_ex_editor(|editor| !editor.typeahead().is_empty());
    let autocmd_busy = host.runtime.autocmd_busy > 0;
    // `o` is `op_pending()` (`normal.c:505-512`), which is true whenever no
    // clean Normal-mode operator context exists — including `current_oap ==
    // NULL`, which is every context this port can be in: there is no
    // `normal_enter` lifecycle and no operator argument block, so a pending
    // operator is indistinguishable from no operator context at all. A real
    // `OperatorPending` mode reads the same.
    // `S` is `!get_was_safe_state()` (`state.c:326-329`): safe state is only
    // entered by an idle main loop (`may_trigger_safestate`), which this
    // port never runs, so every builtin evaluation happens outside it.
    // `c` counts `Callback` invocations (`eval.c:call_callback`: funcref and
    // expression callbacks from timers, jobs, and `sort()`/`map()` — never
    // plain `:call` frames), which this port does not track, so it is never
    // emitted rather than invented from the user-function call stack.
    // `s` (messages scrolled off screen) has no counterpart in this port's
    // message model and is never emitted.
    let mut state = String::new();
    let push = |state: &mut String, flag: char, on: bool| {
        if on && include.as_ref().is_none_or(|filter| filter.contains(flag)) {
            state.push(flag);
        }
    };
    push(&mut state, 'm', typeahead_busy);
    push(&mut state, 'o', true);
    push(&mut state, 'a', completion_active);
    push(&mut state, 'x', autocmd_busy);
    push(&mut state, 'S', true);
    Ok(Typval::String(OxStr::from(state.as_str())))
}

/// Enforces the `eval.lua` argument counts the way upstream's function table
/// does before a builtin body runs.
fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = ox_eval::builtin_spec(name)
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

/// Reads a buffer-scoped numeric option for the current buffer, falling back
/// to the global baseline and then to `default` when neither exists.
fn buffer_number_option(editor: &Editor, name: &str, default: i64) -> i64 {
    let value = editor
        .current_buffer()
        .and_then(|buffer| editor.options().get_buffer(buffer, name).ok())
        .or_else(|| editor.options().get_global_baseline(name).ok());
    match value {
        Some(OptionValue::Number(value)) => *value,
        _ => default,
    }
}

/// `f_shiftwidth` (`eval/funcs.c:6589`): the effective `'shiftwidth'` — the
/// option itself unless it is zero, in which case `'tabstop'`. An optional
/// column selects the indent step at that column; without `'vartabstop'`
/// (unmodelled here) the column only matters through its coercion, and a
/// negative column answers 0.
fn call_shiftwidth_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    check_arity("shiftwidth", args.len())?;
    if let Some(column) = args.first() {
        // `tv_get_number_chk` (`ox_eval::builtins::number_arg`): coerce or
        // fail with the typed error, then bail out silently on a negative
        // column with the preset 0. Strings never error (`'x'` is 0).
        if ox_eval::builtins::number_arg(column)? < 0 {
            return Ok(Typval::Number(0));
        }
    }
    let shiftwidth = buffer_number_option(editor, "shiftwidth", 8);
    if shiftwidth != 0 {
        return Ok(Typval::Number(shiftwidth));
    }
    Ok(Typval::Number(buffer_number_option(editor, "tabstop", 8)))
}

/// Scope addressed by `getcwd()`/`haslocaldir()` arguments
/// (`getcwd_scope_args`, `eval/fs.c:657-760`). In an argument `0` means the
/// current object and `-1` skips the scope, requesting the next broader one;
/// the argument order (window, tab, buffer) differs from the `CdScope` order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CwdAddress {
    /// No arguments to `getcwd()`: the effective working directory.
    Effective,
    /// A window in a tabpage: the window's local directory, else the
    /// tabpage's. The tab is the addressed tabpage (or the current one when
    /// no `{tabnr}` was given), which the window lookup runs in.
    Window {
        /// Addressed window.
        window: ox_types::WinHandle,
        /// Addressed tabpage, if one is live.
        tab: Option<ox_types::TabHandle>,
    },
    /// A tabpage: its local directory.
    Tabpage {
        /// Addressed tabpage, if one is live.
        tab: Option<ox_types::TabHandle>,
    },
    /// A buffer: its local directory, skipping window and tab scopes. A
    /// buffer belongs to no particular window/tabpage, so this falls back
    /// to the global directory.
    Buffer {
        /// Addressed buffer.
        buffer: ox_types::BufHandle,
    },
    /// The session-global directory.
    Global,
}

/// Resolves `[{winnr}[, {tabnr}[, {bufnr}]]]` to the requested scope
/// (`getcwd_scope_args`, `eval/fs.c:657-760`). `unspecified` is the scope
/// with no arguments: [`CwdAddress::Effective`] for `getcwd()`,
/// the current window for `haslocaldir()`.
///
/// # Errors
///
/// Returns `E474` for non-number arguments or numbers below `-1`, `E5001`
/// for `-1` after an explicit scope, `E5006` for a buffer scope with a
/// window/tab argument, `E5007` for a missing buffer, `E5000` for a missing
/// tabpage, and `E5002` for a missing window.
fn resolve_cwd_address(
    editor: &Editor,
    args: &[Typval],
    unspecified: CwdAddress,
) -> ox_eval::Result<CwdAddress> {
    let mut selected = [0i64; 3];
    for (index, argument) in args.iter().enumerate() {
        // Upstream rejects anything but `VAR_NUMBER` with `E474`.
        let Typval::Number(number) = argument else {
            return Err(EvalError::new("E474", 0, "Invalid argument"));
        };
        if *number < -1 {
            return Err(EvalError::new("E474", 0, "Invalid argument"));
        }
        selected[index] = *number;
    }
    // An argument may only be -1 when every preceding argument is -1.
    let mut explicit = false;
    for number in selected.iter().take(args.len()) {
        explicit = explicit || *number >= 0;
        if explicit && *number < 0 {
            return Err(EvalError::new(
                "E5001",
                0,
                "Argument cannot be -1 if preceding argument is >= 0.",
            ));
        }
    }
    let argc = args.len();
    let (window_number, tab_number, buffer_number) = (selected[0], selected[1], selected[2]);
    if argc == 0 {
        return Ok(unspecified);
    }
    // A buffer target skips the window and tab scopes entirely; only
    // `-1, -1, {bufnr}` reaches it.
    if argc == 3 && buffer_number >= 0 {
        if window_number >= 0 || tab_number >= 0 {
            return Err(EvalError::new(
                "E5006",
                0,
                "Window and tab scope must be -1 when using buffer scope",
            ));
        }
        let buffer = if buffer_number == 0 {
            editor.current_buffer()
        } else {
            ox_types::BufHandle::try_from(buffer_number)
                .ok()
                .filter(|buffer| editor.buffer(*buffer).is_ok())
        };
        let Some(buffer) = buffer else {
            return Err(EvalError::new("E5007", 0, "Cannot find buffer number."));
        };
        return Ok(CwdAddress::Buffer { buffer });
    }
    // Narrowest requested scope. Missing `{tabnr}` implies the current
    // tabpage, exactly like the zero-initialized `argv` upstream.
    let tab = if argc >= 2 && tab_number > 0 {
        let tabs = editor.tabpages();
        let Some(tab) = usize::try_from(tab_number)
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| tabs.get(index).copied())
        else {
            return Err(EvalError::new("E5000", 0, "Cannot find tab number."));
        };
        Some(tab)
    } else {
        editor.current_tabpage()
    };
    if argc == 1 && window_number < 0 {
        return Ok(CwdAddress::Tabpage { tab });
    }
    if argc >= 2 && window_number < 0 {
        if tab_number >= 0 {
            return Ok(CwdAddress::Tabpage { tab });
        }
        return Ok(CwdAddress::Global);
    }
    // A window target: `0` answers the current window (even when a `{tabnr}`
    // was given, like `find_win_by_nr`); otherwise a live handle wins over
    // the 1-based position in the addressed tabpage.
    let window = if window_number == 0 {
        editor.current_window()
    } else {
        let windows = tab
            .and_then(|tab| editor.tabpage_windows(tab).ok())
            .unwrap_or_default();
        ox_types::WinHandle::try_from(window_number)
            .ok()
            .filter(|handle| windows.contains(handle))
            .or_else(|| {
                usize::try_from(window_number)
                    .ok()
                    .and_then(|number| number.checked_sub(1))
                    .and_then(|index| windows.get(index).copied())
            })
    };
    let Some(window) = window else {
        return Err(EvalError::new("E5002", 0, "Cannot find window number."));
    };
    Ok(CwdAddress::Window { window, tab })
}

/// `f_getcwd` (`eval/fs.c:771-832`): the working directory of the addressed
/// scope. A window address falls back to its tabpage, then the global
/// directory; tabpage and buffer addresses fall back to the global directory
/// (a buffer belongs to no particular window/tabpage); the global address
/// reads the saved fallback, else the live process directory. Whatever falls
/// through to the process directory answers `""` when it cannot be read.
fn call_getcwd_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    check_arity("getcwd", args.len())?;
    let address = resolve_cwd_address(editor, args, CwdAddress::Effective)?;
    let global_or_os = || {
        editor
            .global_directory()
            .map(std::path::Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
    };
    let directory = match address {
        CwdAddress::Effective => std::env::current_dir().ok(),
        CwdAddress::Window { window, tab } => editor
            .window_local_directory(window)
            .ok()
            .flatten()
            .or_else(|| {
                tab.and_then(|tab| editor.tab_local_directory(tab).ok().flatten())
            })
            .or_else(global_or_os),
        CwdAddress::Tabpage { tab } => tab
            .and_then(|tab| editor.tab_local_directory(tab).ok().flatten())
            .or_else(global_or_os),
        CwdAddress::Buffer { buffer } => editor
            .buffer_local_directory(buffer)
            .ok()
            .flatten()
            .or_else(global_or_os),
        CwdAddress::Global => global_or_os(),
    };
    Ok(Typval::String(OxStr::from(
        directory
            .map(|directory| directory.to_string_lossy().into_owned())
            .unwrap_or_default()
            .as_str(),
    )))
}

/// `f_haslocaldir` (`eval/fs.c:1041`): whether the addressed scope object
/// has a local working directory. The global scope never has one; argument
/// validation mirrors `getcwd_scope_args` exactly.
fn call_haslocaldir_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    check_arity("haslocaldir", args.len())?;
    // Without arguments the window scope is implied, like
    // `getcwd_scope_args` with a window default. A missing current window
    // has no window-local directory to report.
    let Some(current) = editor.current_window() else {
        return Ok(Typval::Number(0));
    };
    let unspecified = CwdAddress::Window {
        window: current,
        tab: editor.current_tabpage(),
    };
    let address = resolve_cwd_address(editor, args, unspecified)?;
    let present = match address {
        CwdAddress::Window { window, .. } => editor
            .window_local_directory(window)
            .ok()
            .flatten()
            .is_some(),
        CwdAddress::Tabpage { tab } => tab
            .and_then(|tab| editor.tab_local_directory(tab).ok().flatten())
            .is_some(),
        CwdAddress::Buffer { buffer } => editor
            .buffer_local_directory(buffer)
            .ok()
            .flatten()
            .is_some(),
        CwdAddress::Global | CwdAddress::Effective => false,
    };
    Ok(Typval::Number(i64::from(present)))
}

/// `f_bufloaded` (`eval/buffer.c:498`): whether `find_buffer()` resolves the
/// argument and the buffer text is resident. Unlike `bufexists()`, no
/// argument spells the current buffer: `0`, `""`, and `"%"` all miss, and
/// non-number/string values answer 0 without an error.
fn call_bufloaded_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    check_arity("bufloaded", args.len())?;
    let buffer = match args.first() {
        Some(Typval::Number(number)) if *number != 0 => ox_types::BufHandle::try_from(*number)
            .ok()
            .filter(|buffer| editor.buffer(*buffer).is_ok()),
        Some(Typval::String(name)) if !name.as_bytes().is_empty() => {
            editor.buffers().into_iter().find(|buffer| {
                editor
                    .buffer(*buffer)
                    .is_ok_and(|state| state.name() == name)
            })
        }
        _ => None,
    };
    let loaded = buffer.is_some_and(|buffer| {
        editor
            .buffer(buffer)
            .is_ok_and(|state| state.residency.is_loaded())
    });
    Ok(Typval::Number(i64::from(loaded)))
}

/// Whether a callback name needs the digit-lead rejection: upstream's
/// `callback_from_typval` answers `E921` for a `String` starting with a
/// digit on both the add and the delete path, while an undefined name
/// otherwise registers fine (resolution happens at fire time).
fn dictwatcher_name_rejected(name: &OxStr) -> bool {
    name.as_bytes().first().is_some_and(|byte| byte.is_ascii_digit())
}

/// Normalises a `dictwatcheradd()` callback: a `Funcref`/`Partial` value is
/// stored whole, a name is stored as written (an undefined name still
/// registers), and a null callback stores the never-firing
/// [`WatchCallback::None`]. Anything else — including a digit-leading name
/// — is `E921`.
fn dictwatcher_add_callback(argument: &Typval) -> ox_eval::Result<WatchCallback> {
    match argument {
        Typval::Funcref(function) => Ok(WatchCallback::Funcref(function.clone())),
        Typval::Partial(function) => Ok(WatchCallback::Partial(function.clone())),
        Typval::String(name) if !dictwatcher_name_rejected(name) => {
            Ok(WatchCallback::Name(name.clone()))
        }
        Typval::Special(_) => Ok(WatchCallback::None),
        _ => Err(EvalError::new("E921", 0, "Invalid callback argument")),
    }
}

/// Normalises a `dictwatcherdel()` callback: same shapes as the add path,
/// except a non-string/non-function (null included) is the `E475`
/// `funcref` refusal from the delete body's explicit type check.
fn dictwatcher_del_callback(argument: &Typval) -> ox_eval::Result<WatchCallback> {
    match argument {
        Typval::Funcref(function) => Ok(WatchCallback::Funcref(function.clone())),
        Typval::Partial(function) => Ok(WatchCallback::Partial(function.clone())),
        Typval::String(name) if dictwatcher_name_rejected(name) => Err(EvalError::new(
            "E921",
            0,
            "Invalid callback argument",
        )),
        Typval::String(name) => Ok(WatchCallback::Name(name.clone())),
        _ => Err(EvalError::new("E475", 0, "Invalid argument: funcref")),
    }
}

/// The watched `Dict` identity for `dictwatcheradd()`/`dictwatcherdel()`.
/// Bare `g:`/`b:`/`w:`/`t:` snapshots carry no provenance in a bare
/// `Typval`, so scope namespaces need the evaluator to record
/// `WatchTarget::Scope` (reported to the route owner); every other dict
/// keys by live identity.
fn dictwatcher_target(argument: &Typval) -> ox_eval::Result<WatchTarget> {
    match argument {
        Typval::Dict(reference) => Ok(WatchTarget::Dict(Rc::as_ptr(reference) as usize)),
        _ => Err(EvalError::new("E475", 0, "Invalid argument: dict")),
    }
}

/// The add-path target: `v:_null_dict` is refused with `E46`, while the
/// delete path lets the lookup miss instead (the `Couldn't find a watcher`
/// error).
fn dictwatcher_add_target(argument: &Typval) -> ox_eval::Result<WatchTarget> {
    if argument.is_null_dict() {
        return Err(EvalError::new(
            "E46",
            0,
            "Cannot change read-only variable \"dictwatcheradd() argument\"",
        ));
    }
    dictwatcher_target(argument)
}

/// Resolves a bare scope-namespace argument (`g:`, `b:`, `w:`, `t:`) to its
/// [`WatchTarget::Scope`]: the evaluator records the live map's identity for
/// exactly these expressions, so matching it against the same maps in `scope`
/// tells them apart from a Dict-valued variable like `g:d`, whose identity is
/// its variable slot's instead. Anything else is not a scope namespace.
fn scope_target(identity: Option<usize>, scope: &Scope) -> Option<WatchTarget> {
    let identity = identity?;
    if identity == std::ptr::from_ref(&scope.global) as usize {
        Some(WatchTarget::Scope(ScopeKind::Global))
    } else if identity == std::ptr::from_ref(&scope.buffer) as usize {
        Some(WatchTarget::Scope(ScopeKind::Buffer))
    } else if identity == std::ptr::from_ref(&scope.window) as usize {
        Some(WatchTarget::Scope(ScopeKind::Window))
    } else if identity == std::ptr::from_ref(&scope.tab) as usize {
        Some(WatchTarget::Scope(ScopeKind::Tab))
    } else {
        None
    }
}

/// The scoped `dictwatcheradd()`/`dictwatcherdel()` target: a bare scope
/// namespace keeps [`WatchTarget::Scope`]; every other argument validates
/// exactly like the values-only path.
fn dictwatcher_target_scoped(argument: &CallArg, scope: &Scope) -> ox_eval::Result<WatchTarget> {
    if let Some(target) = scope_target(argument.identity, scope) {
        return Ok(target);
    }
    dictwatcher_target(&argument.value)
}

/// The scoped add-path target: same `v:_null_dict` refusal as the
/// values-only path, with scope namespaces resolved first.
fn dictwatcher_add_target_scoped(
    argument: &CallArg,
    scope: &Scope,
) -> ox_eval::Result<WatchTarget> {
    if let Some(target) = scope_target(argument.identity, scope) {
        return Ok(target);
    }
    dictwatcher_add_target(&argument.value)
}

/// Validates scoped `dictwatcheradd()` arguments: same pattern/callback
/// checks as [`dictwatcher_add_args`], with the target resolved against the
/// live `scope` maps.
fn dictwatcher_add_args_scoped(
    args: &[CallArg],
    scope: &Scope,
) -> ox_eval::Result<(WatchTarget, Option<OxStr>, WatchCallback)> {
    check_arity("dictwatcheradd", args.len())?;
    let target = dictwatcher_add_target_scoped(&args[0], scope)?;
    let pattern = dictwatcher_pattern(&args[1].value)?;
    let callback = dictwatcher_add_callback(&args[2].value)?;
    Ok((target, pattern, callback))
}

/// Validates scoped `dictwatcherdel()` arguments: same pattern/callback
/// checks as [`dictwatcher_del_args`], with the target resolved against the
/// live `scope` maps.
fn dictwatcher_del_args_scoped(
    args: &[CallArg],
    scope: &Scope,
) -> ox_eval::Result<(WatchTarget, Option<OxStr>, WatchCallback)> {
    check_arity("dictwatcherdel", args.len())?;
    let target = dictwatcher_target_scoped(&args[0], scope)?;
    let pattern = dictwatcher_pattern(&args[1].value)?;
    let callback = dictwatcher_del_callback(&args[2].value)?;
    Ok((target, pattern, callback))
}

/// Scoped `f_dictwatcheradd` (`eval/funcs.c:965`): same registry round as
/// [`call_dictwatcher_add_builtin`], with the target resolved against `scope`.
fn call_dictwatcher_add_scoped<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[CallArg],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    let (target, pattern, callback) = dictwatcher_add_args_scoped(args, scope)?;
    let Some(pattern) = pattern else {
        return Ok(Typval::Number(0));
    };
    host.runtime
        .functions
        .dictwatcher_add(target, pattern, callback);
    Ok(Typval::Number(0))
}

/// Scoped `f_dictwatcherdel` (`eval/funcs.c:1003`): same match-all-or-miss
/// round as [`call_dictwatcher_del_builtin`], with the target resolved
/// against `scope`.
fn call_dictwatcher_del_scoped<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[CallArg],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    let (target, pattern, callback) = dictwatcher_del_args_scoped(args, scope)?;
    let removed = match pattern {
        Some(pattern) => host.runtime.functions.dictwatcher_remove(
            &target,
            pattern.as_bytes(),
            &callback,
        ),
        // A null pattern never matches an added watcher.
        None => false,
    };
    if removed {
        return Ok(Typval::Number(0));
    }
    Err(EvalError::new(
        "",
        0,
        "Couldn't find a watcher matching key and callback",
    ))
}

/// Coerces a dictwatcher key pattern: upstream accepts `VAR_STRING` and
/// `VAR_NUMBER` (`tv_get_string_chk`), refusing anything else. A null
/// pattern adds no watcher and answers 0.
fn dictwatcher_pattern(argument: &Typval) -> ox_eval::Result<Option<OxStr>> {
    match argument {
        Typval::String(pattern) => Ok(Some(pattern.clone())),
        Typval::Number(number) => Ok(Some(OxStr::from(number.to_string().as_str()))),
        Typval::Special(ox_types::Special::Null) => Ok(None),
        _ => Err(EvalError::new("E475", 0, "Invalid argument: key")),
    }
}

/// Validates `dictwatcheradd()` arguments without touching the registry so
/// the checks stay unit-testable without an executor host.
fn dictwatcher_add_args(
    args: &[Typval],
) -> ox_eval::Result<(WatchTarget, Option<OxStr>, WatchCallback)> {
    check_arity("dictwatcheradd", args.len())?;
    let target = dictwatcher_add_target(&args[0])?;
    let pattern = dictwatcher_pattern(&args[1])?;
    let callback = dictwatcher_add_callback(&args[2])?;
    Ok((target, pattern, callback))
}

/// Validates `dictwatcherdel()` arguments without touching the registry.
fn dictwatcher_del_args(
    args: &[Typval],
) -> ox_eval::Result<(WatchTarget, Option<OxStr>, WatchCallback)> {
    check_arity("dictwatcherdel", args.len())?;
    let target = dictwatcher_target(&args[0])?;
    let pattern = dictwatcher_pattern(&args[1])?;
    let callback = dictwatcher_del_callback(&args[2])?;
    Ok((target, pattern, callback))
}

/// `f_dictwatcheradd` (`eval/funcs.c:965`).
fn call_dictwatcher_add_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let (target, pattern, callback) = dictwatcher_add_args(args)?;
    let Some(pattern) = pattern else {
        return Ok(Typval::Number(0));
    };
    host.runtime
        .functions
        .dictwatcher_add(target, pattern, callback);
    Ok(Typval::Number(0))
}

/// `f_dictwatcherdel` (`eval/funcs.c:1003`): removes the watcher only when
/// dict, key, and callback all match, else the codeless
/// `Couldn't find a watcher matching key and callback`.
fn call_dictwatcher_del_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let (target, pattern, callback) = dictwatcher_del_args(args)?;
    let removed = match pattern {
        Some(pattern) => host.runtime.functions.dictwatcher_remove(
            &target,
            pattern.as_bytes(),
            &callback,
        ),
        // A null pattern never matches an added watcher.
        None => false,
    };
    if removed {
        return Ok(Typval::Number(0));
    }
    Err(EvalError::new(
        "",
        0,
        "Couldn't find a watcher matching key and callback",
    ))
}

#[cfg(test)]
mod tests {
    use ox_eval::Scope;
    use ox_types::Typval;

    use crate::{Editor, ExExecutor, ExecError, TestEditorAccess, VimExceptionKind};

    fn error_code(err: &ExecError) -> String {
        match err {
            ExecError::Vim(exception) => match &exception.kind {
                VimExceptionKind::Error(code) => code.clone(),
                VimExceptionKind::Throw => "Throw".to_owned(),
            },
            other => panic!("expected Vim error, got {other:?}"),
        }
    }

    fn global(scope: &Scope, name: &str) -> Option<Typval> {
        scope
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()
            .cloned()
    }

    fn global_number(scope: &Scope, name: &str) -> Option<i64> {
        match global(scope, name)? {
            Typval::Number(value) => Some(value),
            Typval::Bool(value) => Some(i64::from(value)),
            _ => None,
        }
    }

    // `can_add_defer` (`eval/userfunc.c` 3457-3464) gives E193 when no
    // function frame is active.
    #[test]
    fn defer_outside_a_function_reports_e193() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let err = exec
            .execute_script(&editor, "<test>", "defer('Absent')")
            .unwrap_err();
        assert_eq!(error_code(&err), "E193");
    }

    // `add_defer` (3469-3484) registers; `handle_defer_one` (3487-3524) fires
    // at frame end, so the deferred function's global is set only after the
    // caller returns.
    #[test]
    fn defer_fires_at_function_exit_and_sets_its_global() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let script = r"
            function! SetFlag()
                let g:deferred_ran = 1
            endfunction
            function! DeferUser()
                call defer('SetFlag')
                let g:before_exit = 1
            endfunction
            call DeferUser()
        ";
        exec.execute_script(&editor, "<test>", script).unwrap();
        assert_eq!(global_number(exec.scope(), "before_exit"), Some(1));
        assert_eq!(global_number(exec.scope(), "deferred_ran"), Some(1));
    }

    // `add_defer` stores the remaining arguments; `handle_defer_one` invokes
    // the callable with them. The Funcref form works as well as the name.
    #[test]
    fn defer_passes_arguments_and_funcref_form_fires() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let script = r"
            function! StoreValue(value)
                let g:deferred_value = a:value
            endfunction
            function! Mark(name)
                call add(g:order, a:name)
            endfunction
            function! DeferWithArgs()
                let g:order = []
                call defer(function('StoreValue'), 7)
                call defer('Mark', 'first')
                call defer('Mark', 'second')
            endfunction
            call DeferWithArgs()
        ";
        exec.execute_script(&editor, "<test>", script).unwrap();
        assert_eq!(global_number(exec.scope(), "deferred_value"), Some(7));
        let order = match global(exec.scope(), "order") {
            Some(Typval::List(list)) => list
                .borrow()
                .items
                .iter()
                .map(|value| match value {
                    Typval::String(text) => text.to_string_lossy().into_owned(),
                    other => panic!("expected strings, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("expected a List, got {other:?}"),
        };
        assert_eq!(order, vec!["second", "first"]);
    }
}

/// `f_hlID` → `syn_name2id` (eval/funcs.c): the group's 1-based id, 0 when
/// absent. Upstream assigns ids in `hl_table` allocation order; the port's
/// name-keyed table yields the sorted position instead, which is stable and
/// positive but not order-identical to upstream.
fn call_hl_id_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        return Err(EvalError::new(
            if args.is_empty() { "E119" } else { "E118" },
            0,
            "hlID() requires one argument",
        ));
    }
    let name = input_string_arg(&args[0])?;
    let name = name.to_string_lossy();
    let id = editor
        .highlights()
        .keys()
        .position(|candidate| candidate.eq_ignore_ascii_case(&name))
        .map_or(0, |index| i64::try_from(index).unwrap_or(i64::MAX) + 1);
    Ok(Typval::Number(id))
}

#[cfg(test)]
mod api_and_highlight_tests {
    use ox_types::Typval;

    use crate::{Editor, ExExecutor, TestEditorAccess};

    fn run(editor: &TestEditorAccess, exec: &mut ExExecutor, script: &str) {
        exec.execute_script(editor, "<test>", script).unwrap();
    }

    fn global_number(exec: &ExExecutor, name: &str) -> Option<i64> {
        let value = exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()?;
        match value {
            Typval::Number(n) => Some(*n),
            _ => None,
        }
    }

    fn global_keys(exec: &ExExecutor, name: &str) -> Option<Vec<String>> {
        let value = exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()?;
        match value {
            Typval::Dict(dict) => Some(
                dict.borrow()
                    .entries
                    .iter()
                    .map(|field| field.key.to_string_lossy().into_owned())
                    .collect(),
            ),
            _ => None,
        }
    }

    // f_api_info returns object_to_vim(api_metadata()) (funcs.c:450-454);
    // the metadata dict always carries these members.
    #[test]
    fn api_info_returns_metadata_dict() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        run(&editor, &mut exec, "let g:info = api_info()");
        let keys = global_keys(&exec, "info").expect("api_info() result is a dict");
        for member in ["version", "functions", "ui_events"] {
            assert!(
                keys.iter().any(|key| key == member),
                "api_info() missing '{member}' (has {keys:?})"
            );
        }
    }

    // f_hlID -> syn_name2id: a startup group answers a positive id, an
    // unknown name answers 0, and hlexists() agrees.
    #[test]
    fn hl_id_is_positive_for_known_group_and_zero_for_unknown() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        run(
            &editor,
            &mut exec,
            "let g:known = hlID('NonText')\nlet g:unknown = hlID('NoSuchGroupEver')\nlet g:exists = hlexists('NonText')",
        );
        let known = global_number(&exec, "known").expect("hlID() number");
        assert!(known > 0, "hlID('NonText') = {known}, want > 0");
        assert_eq!(global_number(&exec, "unknown"), Some(0));
        assert_eq!(global_number(&exec, "exists"), Some(1));
    }

    /// Reads a global string set by a probe script.
    fn global_string(exec: &ExExecutor, name: &str) -> Option<String> {
        match exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()?
        {
            Typval::String(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        }
    }

    /// `state()` (`eval/funcs.c:4385-4416`).
    ///
    /// Oracle ground truth (headless `nvim -u NONE -S`): a bare call answers
    /// `'oS'` — `op_pending()` with no Normal-mode operator context and no
    /// idle safe state; `state('o')` and `state('S')` select those flags;
    /// `state('')` is always `""`; and an autocommand body observes `'oxS'`.
    #[test]
    fn state_reports_busy_flags_and_filter() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        run(
            &editor,
            &mut exec,
            "let g:bare = state()\nlet g:o = state('o')\nlet g:s = state('S')\nlet g:none = state('')\nlet g:idle = state('xa')\nautocmd BufEnter * let g:au = state()\nenew",
        );
        assert_eq!(global_string(&exec, "bare").as_deref(), Some("oS"));
        assert_eq!(global_string(&exec, "o").as_deref(), Some("o"));
        assert_eq!(global_string(&exec, "s").as_deref(), Some("S"));
        assert_eq!(global_string(&exec, "none").as_deref(), Some(""));
        assert_eq!(global_string(&exec, "idle").as_deref(), Some(""));
        assert_eq!(global_string(&exec, "au").as_deref(), Some("oxS"));
    }
}

#[cfg(test)]
mod ambient_direct_tests {
    use ox_types::Typval;

    use crate::options::OptionValue;
    use crate::{DirectoryScope, Editor, TestEditorAccess};

    /// A fresh editor with one listed buffer shown in one tabpage, so
    /// current-buffer/window lookups resolve the way they do in a live
    /// session (`Editor::new()` starts with neither).
    fn editor_with_window() -> (TestEditorAccess, ox_types::BufHandle) {
        let editor = TestEditorAccess::new(Editor::new());
        let buffer = editor
            .editor_mut()
            .create_buffer_with(ox_text::Buffer::new(), true)
            .unwrap();
        editor
            .editor_mut()
            .create_tabpage(buffer, crate::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        (editor, buffer)
    }

    fn eval_builtin(
        editor: &Editor,
        name: &str,
        args: &[Typval],
    ) -> ox_eval::Result<Typval> {
        match name {
            "shiftwidth" => super::call_shiftwidth_builtin(editor, args),
            "haslocaldir" => super::call_haslocaldir_builtin(editor, args),
            "bufloaded" => super::call_bufloaded_builtin(editor, args),
            _ => unreachable!("covered builtin"),
        }
    }

    fn number_arg(value: &Typval) -> i64 {
        match value {
            Typval::Number(value) => *value,
            _ => panic!("expected a Number, got {value:?}"),
        }
    }

    fn text(value: &str) -> Typval {
        Typval::String(ox_types::OxStr::from(value))
    }

    // `shiftwidth()` (`eval/funcs.c:6589`): the buffer option, or `tabstop`
    // when it is zero; the column form coerces, a negative column is 0.
    #[test]
    fn shiftwidth_reports_effective_indent_step() {
        let (editor, buffer) = editor_with_window();
        editor
            .editor_mut()
            .options_mut()
            .set_buffer(buffer, "shiftwidth", OptionValue::Number(4))
            .unwrap();
        let editor_ref = editor.editor();
        assert_eq!(
            number_arg(&eval_builtin(&editor_ref, "shiftwidth", &[]).unwrap()),
            4
        );
        assert_eq!(
            number_arg(
                &eval_builtin(&editor_ref, "shiftwidth", &[Typval::Number(3)]).unwrap()
            ),
            4
        );
        assert_eq!(
            number_arg(
                &eval_builtin(&editor_ref, "shiftwidth", &[Typval::Number(-1)]).unwrap()
            ),
            0
        );
        drop(editor_ref);
        editor
            .editor_mut()
            .options_mut()
            .set_buffer(buffer, "shiftwidth", OptionValue::Number(0))
            .unwrap();
        editor
            .editor_mut()
            .options_mut()
            .set_buffer(buffer, "tabstop", OptionValue::Number(6))
            .unwrap();
        let editor_ref = editor.editor();
        assert_eq!(
            number_arg(&eval_builtin(&editor_ref, "shiftwidth", &[]).unwrap()),
            6
        );
        assert_eq!(
            number_arg(
                &eval_builtin(&editor_ref, "shiftwidth", &[Typval::Number(100)]).unwrap()
            ),
            6
        );
        // A non-numeric column coerces silently: `'x'` is 0.
        assert_eq!(
            number_arg(&eval_builtin(&editor_ref, "shiftwidth", &[text("x")]).unwrap()),
            6
        );
    }

    // `haslocaldir()` (`eval/fs.c:1041`): 1 after a window-local directory
    // is set, 0 without one, and upstream's validation errors for bad
    // shapes. Tab/buffer/global scopes store nothing in this port.
    #[test]
    fn haslocaldir_tracks_window_directories_and_validates() {
        let _guard = crate::lock_process_state();
        let original = std::env::current_dir().unwrap();
        let target =
            std::env::temp_dir().join(format!("ox-editor-haslocaldir-{}", std::process::id()));
        std::fs::create_dir_all(&target).unwrap();
        let (editor, _buffer) = editor_with_window();
        {
            let editor_ref = editor.editor();
            assert_eq!(
                number_arg(&eval_builtin(&editor_ref, "haslocaldir", &[]).unwrap()),
                0
            );
        }
        {
            let mut borrowed = editor.editor_mut();
            crate::excmd_exec::change_directory(
                &mut borrowed,
                &target.to_string_lossy(),
                DirectoryScope::Window,
            )
            .unwrap();
        }
        {
            let editor_ref = editor.editor();
            for args in [
                vec![],
                vec![Typval::Number(0)],
                vec![Typval::Number(0), Typval::Number(0)],
            ] {
                assert_eq!(
                    number_arg(&eval_builtin(&editor_ref, "haslocaldir", &args).unwrap()),
                    1,
                    "{args:?}"
                );
            }
            for args in [
                vec![Typval::Number(-1)],
                vec![Typval::Number(-1), Typval::Number(0)],
                vec![
                    Typval::Number(-1),
                    Typval::Number(-1),
                ],
            ] {
                assert_eq!(
                    number_arg(&eval_builtin(&editor_ref, "haslocaldir", &args).unwrap()),
                    0,
                    "{args:?}"
                );
            }
        }
        std::env::set_current_dir(&original).unwrap();
        std::fs::remove_dir(&target).unwrap();
        // Validation mirrors `getcwd_scope_args`.
        let editor_ref = editor.editor();
        for (args, code) in [
            (vec![text("x")], "E474"),
            (vec![Typval::Number(1), Typval::Number(-1)], "E5001"),
            (
                vec![
                    Typval::Number(0),
                    Typval::Number(0),
                    Typval::Number(0),
                ],
                "E5006",
            ),
            (
                vec![
                    Typval::Number(-1),
                    Typval::Number(-1),
                    Typval::Number(99999),
                ],
                "E5007",
            ),
            (
                vec![
                    Typval::Number(0),
                    Typval::Number(0),
                    Typval::Number(0),
                    Typval::Number(0),
                ],
                "E118",
            ),
            (vec![Typval::Number(-1), Typval::Number(99)], "E5000"),
            (vec![Typval::Number(99)], "E5002"),
        ] {
            assert_eq!(
                eval_builtin(&editor_ref, "haslocaldir", &args).unwrap_err().code,
                code,
                "{args:?}"
            );
        }
    }

    // `bufloaded()` (`eval/buffer.c:498`): the current buffer by number is
    // loaded, a missing number — and `0`, `""`, and every non-number/string
    // — is not.
    #[test]
    fn bufloaded_reports_resident_buffers() {
        let (editor, _buffer) = editor_with_window();
        let editor_ref = editor.editor();
        let current = editor_ref.current_buffer().unwrap();
        let current_number = Typval::Number(i64::from(current));
        assert_eq!(
            number_arg(&eval_builtin(&editor_ref, "bufloaded", &[current_number]).unwrap()),
            1
        );
        for argument in [
            Typval::Number(99999),
            Typval::Number(0),
            text(""),
            text("%"),
            Typval::Bool(true),
            Typval::Special(ox_types::Special::Null),
        ] {
            assert_eq!(
                number_arg(&eval_builtin(&editor_ref, "bufloaded", &[argument]).unwrap()),
                0
            );
        }
    }

    // `dictwatcheradd()`/`dictwatcherdel()` argument validation
    // (`eval/funcs.c:965,1003`): the registry round-trip lives in
    // `userfunc` tests; here the builtin layer's target/pattern/callback
    // checks and upstream error codes are pinned.
    #[test]
    fn dictwatcher_argument_validation_matches_upstream() {
        let dict = || Typval::dict(vec![(ox_types::OxStr::from("k"), Typval::Number(1))]);
        let name = || Typval::String(ox_types::OxStr::from("g:Changed"));
        // Valid shapes parse.
        let (target, pattern, callback) =
            super::dictwatcher_add_args(&[dict(), text("watched"), name()]).unwrap();
        assert!(matches!(target, crate::userfunc::WatchTarget::Dict(_)));
        assert_eq!(pattern.unwrap().as_bytes(), b"watched");
        assert!(matches!(callback, crate::userfunc::WatchCallback::Name(_)));
        // A null pattern adds nothing but still validates.
        assert!(
            super::dictwatcher_add_args(&[
                dict(),
                Typval::Special(ox_types::Special::Null),
                name(),
            ])
            .unwrap()
            .1
            .is_none()
        );
        // A null callback registers the never-firing variant.
        assert!(matches!(
            super::dictwatcher_add_args(&[
                dict(),
                text("x"),
                Typval::Special(ox_types::Special::Null),
            ])
            .unwrap()
            .2,
            crate::userfunc::WatchCallback::None
        ));
        for (arguments, code) in [
            // add path
            (vec![dict(), text("x"), Typval::Number(42)], "E921"),
            (vec![dict(), text("x"), text("123")], "E921"),
            (vec![text("x"), text("x"), name()], "E475"),
            (vec![Typval::null_dict(), text("x"), name()], "E46"),
            (vec![dict(), Typval::Bool(true), name()], "E475"),
        ] {
            assert_eq!(
                super::dictwatcher_add_args(&arguments).unwrap_err().code,
                code,
                "{arguments:?}"
            );
        }
        // A numeric pattern coerces like `tv_get_string_chk`.
        assert_eq!(
            super::dictwatcher_add_args(&[dict(), Typval::Number(1), name()])
                .unwrap()
                .1
                .unwrap()
                .as_bytes(),
            b"1"
        );
        for (arguments, code) in [
            // delete path: numbers are `E475`, digit names `E921`.
            (vec![dict(), text("x"), Typval::Number(42)], "E475"),
            (vec![dict(), text("x"), text("123")], "E921"),
            (vec![dict(), text("x"), Typval::Bool(true)], "E475"),
        ] {
            assert_eq!(
                super::dictwatcher_del_args(&arguments).unwrap_err().code,
                code,
                "{arguments:?}"
            );
        }
    }
}
