//! Mapping and abbreviation query builtins: `maparg`, `mapcheck`, `hasmapto`.
//!
//! `f_maparg` is `get_maparg(argvars, rettv, exact = 1)` (`mapping.c:2148-2227`),
//! which resolves the left-hand side through `replace_termcodes`, finds the
//! mapping with `check_map` (`mapping.c:2010-2061`), and then answers either
//! `str2special` of the stored right-hand side or the dictionary
//! `mapblock_fill_dict` builds in its *compatible* form (`mapping.c:2090-2146`).
//!
//! The compatible form is sixteen keys, plus `lhsrawalt` when the left-hand
//! side simplified and `desc`/`callback` when the mapping carries them. `buf`
//! is deliberately absent: `mapblock_fill_dict` adds it only when `compatible`
//! is false, which is the `nvim_get_keymap` path and not this one.
//!
//! With `abbr` set the same entry points read the abbreviation table instead
//! (`check_map`'s `abbr` leg, `mapping.c:2030-2061`): the trigger replaces the
//! left-hand side, `mode` still reports `map_mode_to_chars`, and `abbr` is 1.
//! [`crate::mapping::Abbreviation`] now carries the mode set, `nowait`,
//! `silent` and original right-hand side those answers need, because the
//! `:abbreviate` family in this module writes through it.
//!
//! Named gaps, each one a thing that would have to exist first:
//!
//!  * **`callback`** needs a Funcref. [`MappingAction::Callback`] is a `u64`
//!    host-callback identity, not a callable value, and upstream's key holds
//!    the `LuaRef` itself (`mapping.c:2111-2112`). Nothing script-reachable
//!    creates one — `:map` cannot — so a mapping with a callback answers the
//!    empty dictionary rather than an invented Funcref.
//!  * **`lhsrawalt`** needs key simplification. Upstream emits it only when
//!    `replace_termcodes` reported `did_simplify` (`mapping.c:2124-2127`), that
//!    is when the written left-hand side has a second, simplified byte form
//!    (`<C-I>` and `<Tab>`). This port has no `m_simplified` model and no
//!    `REPTERM_NO_SIMPLIFY` pass, so it never emits the key — which is also
//!    what upstream does for every left-hand side that does not simplify.
//!
//! `sid`, `lnum`, `script` and `replace_keycodes` are *not* gaps. The first two
//! come from the script stack, which [`crate::script::Scripts`] tracks; `script`
//! comes from the `<script>` flag the options now record; and
//! `replace_keycodes` is only settable through `nvim_set_keymap`'s option
//! table, so upstream reports 0 for every mapping a `:map` or `:abbreviate`
//! command can create, which is every entry here.

use ox_eval::EvalError;
use ox_eval::Scope;
use ox_excmd::ExCommand;
use ox_types::{BufHandle, OxStr, Typval};

use crate::excmd_exec::{
    EvalHost, ExEditorAccess, ExRuntime, Flow, map_leader, push_info_text_message,
};
use crate::mapping::{
    AbbrevCommandKind, AbbrevOptions, Abbreviation, MapFlags, MapModes, MapScope, Mapping,
    MappingAction, abbrev_modes_for_command, classify_abbrev_command,
};
use crate::script::FileIO;
use crate::typeahead::{Keys, special_notation};

/// Routes one mapping query builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    match name {
        "maparg" => maparg(host, args, scope),
        "mapcheck" => mapcheck(host, args, scope),
        "hasmapto" => hasmapto(host, args, scope),
        _ => unreachable!("mapping builtin route and dispatcher disagree"),
    }
}

/// `get_maparg` (`mapping.c:2148-2227`).
fn maparg<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    check_arity("maparg", args.len())?;
    let keys = super::input_string_arg(&args[0])?;
    // An empty left-hand side returns the empty *string* even when a
    // dictionary was asked for: `get_maparg` returns before it reads
    // `get_dict` (`mapping.c:2155-2157`).
    if keys.0.is_empty() {
        return Ok(Typval::String(OxStr::from("")));
    }
    let which = match args.get(1) {
        Some(value) => super::input_string_arg(value)?
            .to_string_lossy()
            .into_owned(),
        None => String::new(),
    };
    let abbr = args.get(2).is_some_and(Typval::is_truthy);
    let want_dict = args.get(3).is_some_and(Typval::is_truthy);
    let modes = MapModes::from_mode_string(&which);

    let empty = || {
        if want_dict {
            Typval::dict(Vec::new())
        } else {
            Typval::String(OxStr::from(""))
        }
    };
    // `get_maparg`'s `abbr` leg reads the abbreviation table the same way.
    if abbr {
        let lhs = Keys::parse_notation(
            &keys.to_string_lossy(),
            &map_leader(scope, "mapleader"),
            &map_leader(scope, "maplocalleader"),
        );
        let trigger = String::from_utf8_lossy(lhs.as_bytes()).into_owned();
        let found = host.access.with_ex_editor(|editor| {
            editor
                .mappings()
                .find_abbrev_exact(&trigger, modes, editor.current_buffer())
                .map(|(entry, local)| (entry.clone(), local))
        });
        let Some((entry, local)) = found else {
            return Ok(empty());
        };
        // A callback abbreviation has no key string, like a callback mapping.
        let Some(replaced) = entry.action.replaced_keys() else {
            return Ok(empty());
        };
        if want_dict {
            return Ok(abbrev_fill_dict(&entry, local));
        }
        return Ok(Typval::String(if replaced.is_empty() {
            OxStr::from("<Nop>")
        } else {
            OxStr::from(special_notation(replaced, false, false).as_str())
        }));
    }
    // `replace_termcodes` on the queried keys, the same translation `:map`
    // applied when the mapping was stored, so `maparg('<Esc>x')` finds what
    // `:nnoremap <Esc>x` defined.
    let lhs = Keys::parse_notation(
        &keys.to_string_lossy(),
        &map_leader(scope, "mapleader"),
        &map_leader(scope, "maplocalleader"),
    );
    let found = host.access.with_ex_editor(|editor| {
        editor
            .mappings()
            .find_exact(lhs.as_bytes(), modes, editor.current_buffer())
            .map(|(mapping, local)| (mapping.clone(), local))
    });
    let Some((mapping, local)) = found else {
        return Ok(empty());
    };
    // A callback mapping has no key string, and this port cannot hand back the
    let Some(replaced) = mapping.action.replaced_keys() else {
        return Ok(empty());
    };
    if want_dict {
        return Ok(fill_dict(&mapping, local));
    }
    // `get_maparg`'s string form (`mapping.c:2200-2210`): an empty right-hand
    // side prints as the literal `<Nop>`, everything else through
    // `str2special` with neither spaces nor `<` replaced.
    Ok(Typval::String(if replaced.is_empty() {
        OxStr::from("<Nop>")
    } else {
        OxStr::from(special_notation(replaced, false, false).as_str())
    }))
}

/// `f_mapcheck` — `get_maparg` with `exact = 0` (`mapping.c:2148-2227`):
/// answers the right-hand side of the first mapping whose left-hand side
/// begins with the queried keys, '' when none does. Abbreviations answer
/// nothing for the same reason `maparg`'s does (module header).
fn mapcheck<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    check_arity("mapcheck", args.len())?;
    let keys = super::input_string_arg(&args[0])?;
    if keys.0.is_empty() {
        return Ok(Typval::String(OxStr::from("")));
    }
    let which = match args.get(1) {
        Some(value) => super::input_string_arg(value)?
            .to_string_lossy()
            .into_owned(),
        None => String::new(),
    };
    let modes = MapModes::from_mode_string(&which);
    // `f_mapcheck` with `abbr` set prefix-matches the abbreviation table.
    if args.get(2).is_some_and(Typval::is_truthy) {
        let lhs = Keys::parse_notation(
            &keys.to_string_lossy(),
            &map_leader(scope, "mapleader"),
            &map_leader(scope, "maplocalleader"),
        );
        let trigger = String::from_utf8_lossy(lhs.as_bytes()).into_owned();
        let found = host.access.with_ex_editor(|editor| {
            editor
                .mappings()
                .abbrev_matching(&trigger, modes, editor.current_buffer())
                .into_iter()
                .filter(|(entry, _)| entry.lhs.as_bytes().starts_with(lhs.as_bytes()))
                // `map_check` prefers the exact lhs over any longer one.
                .min_by_key(|(entry, _)| entry.lhs.as_bytes().len())
                .and_then(|(entry, _)| entry.action.replaced_keys().map(<[u8]>::to_vec))
        });
        let Some(replaced) = found else {
            return Ok(Typval::String(OxStr::from("")));
        };
        return Ok(Typval::String(OxStr::from(
            special_notation(&replaced, false, false).as_str(),
        )));
    }
    let lhs = Keys::parse_notation(
        &keys.to_string_lossy(),
        &map_leader(scope, "mapleader"),
        &map_leader(scope, "maplocalleader"),
    );
    let found = host.access.with_ex_editor(|editor| {
        editor
            .mappings()
            .matching(lhs.as_bytes(), modes, editor.current_buffer())
            .into_iter()
            .filter(|(mapping, _)| mapping.lhs.as_bytes().starts_with(lhs.as_bytes()))
            // `map_check` prefers the exact lhs over any longer one.
            .min_by_key(|(mapping, _)| mapping.lhs.as_bytes().len())
            .and_then(|(mapping, _)| mapping.action.replaced_keys().map(<[u8]>::to_vec))
    });
    let Some(replaced) = found else {
        return Ok(Typval::String(OxStr::from("")));
    };
    Ok(Typval::String(OxStr::from(
        special_notation(&replaced, false, false).as_str(),
    )))
}

/// `f_hasmapto` (`mapping.c:1980-2022`): 1 when any mapping's right-hand
/// side contains `what` (after `replace_termcodes`), 0 otherwise.
fn hasmapto<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    check_arity("hasmapto", args.len())?;
    let what = super::input_string_arg(&args[0])?;
    let which = match args.get(1) {
        Some(value) => super::input_string_arg(value)?
            .to_string_lossy()
            .into_owned(),
        None => String::new(),
    };
    let modes = MapModes::from_mode_string(&which);
    // `f_hasmapto` with `abbr` set scans abbreviation right-hand sides.
    if args.get(2).is_some_and(Typval::is_truthy) {
        let needle = Keys::parse_notation(
            &what.to_string_lossy(),
            &map_leader(scope, "mapleader"),
            &map_leader(scope, "maplocalleader"),
        );
        let found = host.access.with_ex_editor(|editor| {
            editor
                .mappings()
                .abbrev_matching("", modes, editor.current_buffer())
                .into_iter()
                .any(|(entry, _)| {
                    entry
                        .action
                        .replaced_keys()
                        .is_some_and(|rhs| windows_contains(rhs, needle.as_bytes()))
                })
        });
        return Ok(Typval::Number(i64::from(found)));
    }
    let needle = Keys::parse_notation(
        &what.to_string_lossy(),
        &map_leader(scope, "mapleader"),
        &map_leader(scope, "maplocalleader"),
    );
    let found = host.access.with_ex_editor(|editor| {
        editor
            .mappings()
            .matching(b"", modes, editor.current_buffer())
            .into_iter()
            .any(|(mapping, _)| {
                mapping
                    .action
                    .replaced_keys()
                    .is_some_and(|rhs| windows_contains(rhs, needle.as_bytes()))
            })
    });
    Ok(Typval::Number(i64::from(found)))
}

/// `windows()`-style substring test that answers false for an empty needle,
/// matching `vim_strchr`-based containment upstream.
fn windows_contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// `mapblock_fill_dict` with `compatible` true (`mapping.c:2090-2146`).
fn fill_dict(mapping: &Mapping, local: bool) -> Typval {
    let options = &mapping.options;
    let context = options.script_context;
    let mut entries = vec![
        // `compatible` reports `m_orig_str`, the right-hand side as written,
        // rather than `str2special` of the replaced form.
        (
            OxStr::from("rhs"),
            Typval::String(OxStr::from(options.orig_rhs.as_str())),
        ),
        (
            OxStr::from("lhs"),
            Typval::String(OxStr::from(
                special_notation(mapping.lhs.as_bytes(), true, false).as_str(),
            )),
        ),
        (
            OxStr::from("lhsraw"),
            Typval::String(OxStr(mapping.lhs.as_bytes().to_vec())),
        ),
        // The compatible form cannot distinguish `<script>`, so `noremap` is
        // just "does not remap" (`mapping.c:2101-2104`).
        (
            OxStr::from("noremap"),
            Typval::Number(i64::from(!options.flags.contains(crate::MapFlags::REMAP))),
        ),
        (
            OxStr::from("script"),
            Typval::Number(i64::from(options.flags.contains(crate::MapFlags::SCRIPT))),
        ),
        (
            OxStr::from("silent"),
            Typval::Number(i64::from(options.flags.contains(crate::MapFlags::SILENT))),
        ),
        (
            OxStr::from("expr"),
            Typval::Number(i64::from(matches!(mapping.action, MappingAction::Expr(_)))),
        ),
        // Hard-coded upstream too (`mapping.c:2133`).
        (OxStr::from("scriptversion"), Typval::Number(1)),
        (
            OxStr::from("sid"),
            Typval::Number(i64::try_from(context.sid).unwrap_or(0)),
        ),
        (
            OxStr::from("lnum"),
            Typval::Number(i64::try_from(context.lnum).unwrap_or(0)),
        ),
        (OxStr::from("buffer"), Typval::Number(i64::from(local))),
        (
            OxStr::from("nowait"),
            Typval::Number(i64::from(options.flags.contains(crate::MapFlags::NOWAIT))),
        ),
        // Only `nvim_set_keymap`'s option table sets `m_replace_keycodes`, and
        // this port has no such surface, so every mapping reports upstream's
        // value for a `:map`-created one.
        (OxStr::from("replace_keycodes"), Typval::Number(0)),
        (
            OxStr::from("mode"),
            Typval::String(OxStr::from(options.modes.to_chars().as_str())),
        ),
        (OxStr::from("abbr"), Typval::Number(0)),
        (
            OxStr::from("mode_bits"),
            Typval::Number(i64::from(options.modes.bits())),
        ),
    ];
    if let Some(description) = &options.description {
        entries.push((
            OxStr::from("desc"),
            Typval::String(OxStr::from(description.as_str())),
        ));
    }
    Typval::dict(entries)
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

/// `mapblock_fill_dict` with `compatible` true over an abbreviation
/// (`mapping.c:2090-2146`): the trigger answers `lhs`/`lhsraw`, `rhs` is the
/// right-hand side as written, `mode` still reports `map_mode_to_chars`, and
/// `abbr` is 1.
fn abbrev_fill_dict(entry: &Abbreviation, local: bool) -> Typval {
    let context = entry.script_context;
    let entries = vec![
        (
            OxStr::from("rhs"),
            Typval::String(OxStr::from(entry.orig_rhs.as_str())),
        ),
        (
            OxStr::from("lhs"),
            Typval::String(OxStr::from(
                special_notation(entry.lhs.as_bytes(), true, false).as_str(),
            )),
        ),
        (
            OxStr::from("lhsraw"),
            Typval::String(OxStr(entry.lhs.as_bytes().to_vec())),
        ),
        // The compatible form cannot distinguish `<script>`, so `noremap` is
        // just "does not remap" (`mapping.c:2101-2104`).
        (
            OxStr::from("noremap"),
            Typval::Number(i64::from(!entry.flags.contains(MapFlags::REMAP))),
        ),
        (
            OxStr::from("script"),
            Typval::Number(i64::from(entry.flags.contains(MapFlags::SCRIPT))),
        ),
        (
            OxStr::from("silent"),
            Typval::Number(i64::from(entry.flags.contains(MapFlags::SILENT))),
        ),
        (
            OxStr::from("expr"),
            Typval::Number(i64::from(matches!(entry.action, MappingAction::Expr(_)))),
        ),
        // Hard-coded upstream too (`mapping.c:2133`).
        (OxStr::from("scriptversion"), Typval::Number(1)),
        (
            OxStr::from("sid"),
            Typval::Number(i64::try_from(context.sid).unwrap_or(0)),
        ),
        (
            OxStr::from("lnum"),
            Typval::Number(i64::try_from(context.lnum).unwrap_or(0)),
        ),
        (OxStr::from("buffer"), Typval::Number(i64::from(local))),
        (
            OxStr::from("nowait"),
            Typval::Number(i64::from(entry.flags.contains(MapFlags::NOWAIT))),
        ),
        // Only `nvim_set_keymap`'s option table sets `m_replace_keycodes`, and
        // this port has no such surface, so every abbreviation reports
        // upstream's value for a `:abbreviate`-created one.
        (OxStr::from("replace_keycodes"), Typval::Number(0)),
        (
            OxStr::from("mode"),
            Typval::String(OxStr::from(entry.modes.to_chars().as_str())),
        ),
        (OxStr::from("abbr"), Typval::Number(1)),
        (
            OxStr::from("mode_bits"),
            Typval::Number(i64::from(entry.modes.bits())),
        ),
    ];
    Typval::dict(entries)
}

/// The `:abbreviate` family: `do_exmap` with `isabbrev` (`mapping.c:2673-2679`)
/// for the define/undefine forms, and `do_mapclear`'s abbreviation leg
/// (`mapping.c:1066-1073`) for the `abclear` forms.
///
/// All twelve canonical names share this path. Dispatch routes ten of them
/// here through the `:map` family arm; `"iabbrev"` and `"abclear"` keep their
/// historic arms and delegate here too. Bang is meaningless (no `BANG` flag in
/// `ex_cmds.lua`'s abbreviation entries) and ignored, and a range never
/// reaches here: those commands carry no `RANGE` flag, so the parser reports
/// `E481` first.
pub(crate) fn command_abbrev<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    command: &ExCommand,
) -> Flow {
    let name = command.command.name();
    let kind = classify_abbrev_command(name);
    let modes = abbrev_modes_for_command(name);
    let (modifiers, rest) = split_abbrev_modifiers(&command.args);
    let leader = map_leader(scope, "mapleader");
    let local_leader = map_leader(scope, "maplocalleader");
    let map_scope = access.with_ex_editor(|editor| {
        if modifiers.buffer {
            MapScope::Buffer(editor.current_buffer().unwrap_or(BufHandle::CURRENT))
        } else {
            MapScope::Global
        }
    });

    if matches!(kind, AbbrevCommandKind::Clear) {
        // `do_mapclear`: anything but bare or `<buffer>` is `E474`.
        if !rest.trim().is_empty() {
            return Flow::Exception(runtime.exception("E474", "Invalid argument"));
        }
        access.with_ex_editor(|editor| {
            editor.mappings_mut().abbrevclear_modes(modes, map_scope);
        });
        return Flow::Normal;
    }

    if matches!(kind, AbbrevCommandKind::Undefine) {
        // Like `:unmap`, the whole tail is the trigger, whitespace included.
        if rest.is_empty() {
            return Flow::Exception(runtime.exception("E474", "Invalid argument"));
        }
        let lhs = Keys::parse_notation(rest, &leader, &local_leader);
        let trigger = String::from_utf8_lossy(lhs.as_bytes()).into_owned();
        if trigger.is_empty() {
            return Flow::Exception(runtime.exception("E474", "Invalid argument"));
        }
        let stripped = access.with_ex_editor(|editor| {
            editor
                .mappings_mut()
                .unabbreviate_modes(&trigger, modes, map_scope)
        });
        return if stripped == 0 {
            Flow::Exception(runtime.exception("E24", "No such abbreviation"))
        } else {
            Flow::Normal
        };
    }

    let AbbrevCommandKind::Define { noremap } = kind else {
        unreachable!("abbreviation command classification is exhaustive");
    };
    // `str_to_mapargs` (`mapping.c:463-475`): the trigger runs to the next
    // space or tab, with CTRL-V or backslash pulling the following byte in;
    // the right-hand side is `skipwhite(lhs_end)` to the end of the argument.
    let bytes = rest.as_bytes();
    let mut lhs_end = 0;
    while lhs_end < bytes.len() && !matches!(bytes[lhs_end], b' ' | b'\t') {
        if matches!(bytes[lhs_end], 0x16 | b'\\') && lhs_end + 1 < bytes.len() {
            lhs_end += 1;
        }
        lhs_end += 1;
    }
    // A CTRL-V escape can strand `lhs_end` inside a multi-byte character;
    // floor to the boundary instead of slicing invalid UTF-8.
    while !rest.is_char_boundary(lhs_end) {
        lhs_end -= 1;
    }
    let lhs = Keys::parse_notation(&rest[..lhs_end], &leader, &local_leader);
    let trigger = String::from_utf8_lossy(lhs.as_bytes()).into_owned();
    let rhs = &rest[lhs_end
        + rest[lhs_end..]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count()..];
    // `do_map` prints instead of defining whenever the rhs is missing
    // (`mapping.c:873-883`).
    if rhs.is_empty() {
        return list_abbreviations(access, &trigger, modes, modifiers.buffer);
    }
    if modifiers.unique {
        let exists = access.with_ex_editor(|editor| {
            editor
                .mappings()
                .abbrev_conflicts(&trigger, modes, map_scope)
        });
        if exists {
            return Flow::Exception(
                runtime.exception("E226", format!("Abbreviation already exists for {trigger}")),
            );
        }
        if matches!(map_scope, MapScope::Buffer(_)) {
            let global = access.with_ex_editor(|editor| {
                editor
                    .mappings()
                    .abbrev_conflicts(&trigger, modes, MapScope::Global)
            });
            if global {
                return Flow::Exception(runtime.exception(
                    "E224",
                    format!("Global abbreviation already exists for {trigger}"),
                ));
            }
        }
    }
    let action = if modifiers.expr {
        MappingAction::Expr(rhs.to_owned())
    } else {
        match MappingAction::parse_rhs(rhs, &leader, &local_leader) {
            Ok(action) => action,
            Err(error) => return Flow::Exception(runtime.exception("E474", error.to_string())),
        }
    };
    // `buf_do_map`'s `noremap` (`mapping.c:612-618`): `<script>` restricts
    // remapping to `<SID>` mappings, and with no script-local mappings here
    // that behaves as no-remap — but the flag is recorded, because
    // `maparg()`'s `script` key is the only thing that tells `<script>` apart
    // from `:noreabbrev`.
    let mut flags = MapFlags::default();
    flags.set(MapFlags::REMAP, !noremap && !modifiers.script);
    flags.set(MapFlags::NOWAIT, modifiers.nowait);
    flags.set(MapFlags::SILENT, modifiers.silent);
    flags.set(MapFlags::SCRIPT, modifiers.script);
    let script_context = runtime.scripts.current_context();
    let defined = access.with_ex_editor(|editor| {
        editor
            .mappings_mut()
            .abbreviate_full(
                &trigger,
                action,
                AbbrevOptions {
                    modes,
                    scope: map_scope,
                    flags,
                    orig_rhs: rhs.to_owned(),
                    script_context,
                },
            )
            .is_ok()
    });
    if defined {
        Flow::Normal
    } else {
        // `buf_do_map`'s `retval = 1`: an empty, overlong, whitespace-bearing
        // or keyword-mixed trigger is `E474` (`mapping.c:646-681`).
        Flow::Exception(runtime.exception("E474", "Invalid argument"))
    }
}

/// Front modifiers the `:abbreviate` family accepts, in any order
/// (`str_to_mapargs`, `mapping.c:425-460`).
#[derive(Clone, Copy, Default)]
struct AbbrevModifiers {
    buffer: bool,
    nowait: bool,
    silent: bool,
    script: bool,
    expr: bool,
    unique: bool,
}

/// Mirror of the Ex dispatcher's front-modifier split: the same six modifiers
/// the `:map` family strips before the left-hand side, with obsolete
/// `<special>` accepted and ignored. Kept beside the abbreviation command so
/// both dispatch arms share it without reaching into the dispatcher; keep the
/// two in sync.
fn split_abbrev_modifiers(args: &str) -> (AbbrevModifiers, &str) {
    let mut modifiers = AbbrevModifiers::default();
    let mut rest = args.trim_start();
    loop {
        let next = if let Some(tail) = rest.strip_prefix("<buffer>") {
            modifiers.buffer = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("<nowait>") {
            modifiers.nowait = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("<silent>") {
            modifiers.silent = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("<special>") {
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("<script>") {
            modifiers.script = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("<expr>") {
            modifiers.expr = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("<unique>") {
            modifiers.unique = true;
            Some(tail)
        } else {
            None
        };
        match next {
            Some(tail) => rest = tail.trim_start(),
            None => return (modifiers, rest),
        }
    }
}

/// `do_map`'s listing passes for abbreviations (`mapping.c:698-793`): every
/// matching trigger, buffer-local table first, or `No abbreviation found`.
fn list_abbreviations<E: ExEditorAccess>(
    access: &E,
    filter: &str,
    modes: MapModes,
    buffer_only: bool,
) -> Flow {
    let buffer = access.with_ex_editor(|editor| editor.current_buffer());
    let rows: Vec<String> = access.with_ex_editor(|editor| {
        editor
            .mappings()
            .abbrev_matching(filter, modes, buffer)
            .into_iter()
            .filter(|(entry, local)| {
                !buffer_only || (*local && matches!(entry.scope, MapScope::Buffer(_)))
            })
            .map(|(entry, local)| entry.listing_row(local))
            .collect()
    });
    access.with_ex_editor(|editor| {
        push_info_text_message(editor, String::new());
        if rows.is_empty() {
            push_info_text_message(editor, "No abbreviation found".to_owned());
        } else {
            for row in rows {
                push_info_text_message(editor, row);
            }
        }
    });
    Flow::Normal
}
