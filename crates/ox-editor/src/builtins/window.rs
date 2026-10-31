//! Window and screen-cell builtins: window geometry, window identity, and the
//! rendered cell queries (upstream `eval/window.c`, `screen.c`).

use crate::Editor;
use crate::editor::LOWEST_WINDOW_ID;
use crate::excmd_exec::ExEditorAccess;
use crate::options::OptionValue;
use crate::script::FileIO;
use ox_eval::EvalError;
use ox_eval::scope::{ScopeKind, scope_var_entry};
use ox_types::{OxStr, Typval};

use crate::builtins::position::{
    cursor_vcol, getvcol, lnum_arg_for_window, number_value, tabstop as position_tabstop,
    vcol_to_byte,
};
use crate::excmd_exec::{
    EvalHost, Flow, buffer_lines, flow_to_eval_error, parse_program, run_program,
};
use crate::script::LogicalLine;
use crate::excmd_exec::{SetLayer, option_value, typval_number, typval_to_text};
use crate::layout::Frame;
use ox_eval::Scope;
use ox_types::{Object, WinHandle};

fn one_based_index(number: i64) -> Option<usize> {
    usize::try_from(number).ok()?.checked_sub(1)
}

/// Routes one window or screen-cell builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let spec = ox_eval::builtin_spec(name).ok_or_else(|| {
        EvalError::new("E117", 0, format!("Unknown function: {name}"))
    })?;
    if args.len() < spec.min_args {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if spec.max_args.is_some_and(|maximum| args.len() > maximum) {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    match name {
        "win_execute" => call_win_execute(host, args, scope),
        "win_screenpos" => host
            .access
            .with_ex_editor(|editor| call_win_screenpos(editor, args)),
        "screenpos" => host
            .access
            .with_ex_editor(|editor| call_screenpos(editor, args)),
        "screenattr" | "screenchar" | "screenchars" | "screenstring" => Ok(host
            .access
            .with_ex_editor(|editor| call_screen_builtin(editor, name, args))),
        "screencol" => host
            .access
            .with_ex_editor(|editor| call_screencol(editor, args)),
        "screenrow" => host
            .access
            .with_ex_editor(|editor| call_screenrow(editor, args)),
        "tabpagenr" => host
            .access
            .with_ex_editor(|editor| call_tabpagenr_builtin(editor, args)),
        "tabpagewinnr" => host
            .access
            .with_ex_editor(|editor| call_tabpagewinnr_builtin(editor, args)),
        "win_getid" | "winheight" | "winwidth" => Ok(host
            .access
            .with_ex_editor(|editor| call_window_builtin(editor, name, args))),
        "win_gotoid" => host
            .access
            .with_ex_editor(|editor| call_win_gotoid_builtin(editor, args)),
        "win_gettype" => host
            .access
            .with_ex_editor(|editor| call_win_gettype_builtin(editor, args)),
        "win_findbuf" => host
            .access
            .with_ex_editor(|editor| call_win_findbuf(editor, args)),
        "winbufnr" => host
            .access
            .with_ex_editor(|editor| call_winbufnr_builtin(editor, args)),
        "winnr" => host
            .access
            .with_ex_editor(|editor| call_winnr_builtin(editor, args)),
        "bufwinid" => host
            .access
            .with_ex_editor(|editor| call_bufwin_builtin(editor, args, false)),
        "bufwinnr" => host
            .access
            .with_ex_editor(|editor| call_bufwin_builtin(editor, args, true)),
        "winsaveview" => host
            .access
            .with_ex_editor(|editor| call_winsaveview(editor, args)),
        "winrestview" => host
            .access
            .with_ex_editor(|editor| call_winrestview(editor, args)),
        "winline" => host
            .access
            .with_ex_editor(|editor| call_winline(editor, args)),
        "wincol" => host
            .access
            .with_ex_editor(|editor| call_wincol(editor, args)),
        "getwinvar" => host
            .access
            .with_ex_editor(|editor| call_getwinvar(editor, args)),
        "setwinvar" => host
            .access
            .with_ex_editor(|editor| call_setwinvar(editor, args)),
        "getwininfo" => host
            .access
            .with_ex_editor(|editor| call_getwininfo(editor, args)),
        "winlayout" => Ok(host
            .access
            .with_ex_editor(|editor| call_winlayout(editor, args))),
        _ => unreachable!("window builtin route and dispatcher disagree"),
    }
}

/// `winnr()`: the current window's position in the tabpage, or the window
/// count for `$` (`f_winnr` → `get_winnr`).
///
/// Upstream counts within one tabpage: `get_winnr`
/// (`eval/window.c:278-292`) walks `tp`'s windows and uses `tp_lastwin` for
/// `$`. Counting every window in the editor only agreed with that while a
/// single tabpage was the only reachable state.
fn call_winnr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: winnr",
        ));
    }
    let windows = editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage_windows(tab).ok())
        .unwrap_or_default();
    let number = if args
        .first()
        .is_some_and(|value| typval_to_text(value) == "$")
    {
        windows.len()
    } else {
        editor
            .current_window()
            .and_then(|current| windows.iter().position(|window| *window == current))
            .map_or(0, |index| index + 1)
    };
    Ok(Typval::Number(i64::try_from(number).unwrap_or(i64::MAX)))
}

/// `bufwinid()` / `bufwinnr()` (`f_bufwinid`, `f_bufwinnr`): the first
/// window in the CURRENT tab page displaying the buffer named by the
/// argument, as a window id (`bufwinid`) or a window number in the tab
/// (`bufwinnr`); -1 when no window displays it.
fn call_bufwin_builtin(
    editor: &Editor,
    args: &[Typval],
    want_number: bool,
) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        let (code, message) = if args.is_empty() {
            ("E119", "Not enough arguments for function")
        } else {
            ("E118", "Too many arguments for function")
        };
        return Err(EvalError::new(code, 0, message));
    }
    let Some(target) = crate::excmd_exec::resolve_buffer_argument(editor, args.first()) else {
        return Ok(Typval::Number(-1));
    };
    let windows = editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage_windows(tab).ok())
        .unwrap_or_default();
    // bufwinnr numbers tiled windows only (buf_win_common counts
    // win_has_winnr windows, eval/buffer.c:566-580); bufwinid matches floats
    // too, so the numbering arm excludes the tabpage's float set.
    let floats: Vec<WinHandle> = editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage(tab).ok())
        .map(|tab| tab.floating_windows().map(|float| float.window).collect())
        .unwrap_or_default();
    let numbered: Vec<&WinHandle> = if want_number {
        windows
            .iter()
            .filter(|window| !floats.contains(window))
            .collect()
    } else {
        windows.iter().collect()
    };
    for (index, window) in numbered.into_iter().enumerate() {
        let displays = editor
            .window(*window)
            .is_ok_and(|state| state.buffer == target);
        if displays {
            let value = if want_number {
                i64::try_from(index + 1).unwrap_or(i64::MAX)
            } else {
                i64::from(*window)
            };
            return Ok(Typval::Number(value));
        }
    }
    Ok(Typval::Number(-1))
}

/// `tabpagenr()`: the current tabpage's position, or the tabpage count for
/// `$` (`f_tabpagenr`).
fn call_tabpagenr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: tabpagenr",
        ));
    }
    let tabs = editor.tabpages();
    let number = if args
        .first()
        .is_some_and(|value| typval_to_text(value) == "$")
    {
        tabs.len()
    } else {
        editor
            .current_tabpage()
            .and_then(|current| tabs.iter().position(|tab| *tab == current))
            .map_or(0, |index| index + 1)
    };
    Ok(Typval::Number(i64::try_from(number).unwrap_or(i64::MAX)))
}

/// `tabpagewinnr({tabnr} [, {arg}])`: a window number in one tabpage.
fn call_tabpagewinnr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: tabpagewinnr",
        ));
    }
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: tabpagewinnr",
        ));
    }
    let tab_number = typval_number(&args[0]).unwrap_or(0);
    let tabs = editor.tabpages();
    let Some(tab) = one_based_index(tab_number).and_then(|index| tabs.get(index).copied()) else {
        return Ok(Typval::Number(0));
    };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let Some(mut target) = editor
        .tabpage(tab)
        .ok()
        .map(crate::layout::TabpageState::current_window)
    else {
        return Ok(Typval::Number(0));
    };
    let Some(selector) = args.get(1) else {
        return Ok(window_number(&windows, target));
    };
    let selector = typval_to_text(selector);
    if selector == "$" {
        return Ok(Typval::Number(
            i64::try_from(windows.len()).unwrap_or(i64::MAX),
        ));
    }
    if selector == "#" {
        let previous = editor
            .tabpage(tab)
            .ok()
            .and_then(crate::layout::TabpageState::previous_window);
        return Ok(previous.map_or(Typval::Number(0), |window| window_number(&windows, window)));
    }

    let direction_start = selector
        .as_bytes()
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(selector.len());
    let (count, direction) = selector.split_at(direction_start);
    let Some(direction) = direction
        .strip_prefix(['h', 'j', 'k', 'l'])
        .filter(|rest| rest.is_empty())
        .and_then(|_| selector.chars().last())
    else {
        return Err(EvalError::new(
            "E15",
            0,
            format!("Invalid expression: \"{selector}\""),
        ));
    };
    let count = count.parse::<usize>().unwrap_or(0).max(1);
    for _ in 0..count {
        target = crate::excmd_exec::directional_window(editor, target, &windows, direction)
            .unwrap_or(target);
    }
    Ok(window_number(&windows, target))
}

fn window_number(windows: &[ox_types::WinHandle], target: ox_types::WinHandle) -> Typval {
    let number = windows
        .iter()
        .position(|window| *window == target)
        .map_or(0, |index| index + 1);
    Typval::Number(i64::try_from(number).unwrap_or(i64::MAX))
}

fn resolve_window_by_number(editor: &Editor, number: i64) -> Option<WinHandle> {
    resolve_window_by_number_impl(editor, number, false)
}

fn resolve_tiled_window_by_number(editor: &Editor, number: i64) -> Option<WinHandle> {
    resolve_window_by_number_impl(editor, number, true)
}

fn resolve_window_by_number_impl(
    editor: &Editor,
    number: i64,
    exclude_floats: bool,
) -> Option<WinHandle> {
    if number == 0 {
        return editor.current_window();
    }
    if number < 0 {
        return None;
    }
    if number >= LOWEST_WINDOW_ID {
        return editor.find_window_by_id(number);
    }
    let tab = editor.current_tabpage()?;
    let windows = editor.tabpage_windows(tab).ok()?;
    let floats: Vec<WinHandle> = if exclude_floats {
        editor
            .tabpage(tab)
            .ok()
            .map(|tab| tab.floating_windows().map(|float| float.window).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let index = one_based_index(number)?;
    windows
        .iter()
        .filter(|window| !floats.contains(window))
        .nth(index)
        .copied()
}

/// `winbufnr({nr})`: the buffer displayed by a window number or id.
fn call_winbufnr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: winbufnr",
        ));
    }
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: winbufnr",
        ));
    }
    let number = number_value(&args[0])?;
    let window = resolve_window_by_number(editor, number);
    let buffer = window
        .and_then(|window| editor.window(window).ok())
        .map_or(-1, |state| i64::from(state.buffer));
    Ok(Typval::Number(buffer))
}

/// `getwininfo([{winid}])` (`eval/window.c:f_getwininfo`): one dict of window
/// facts per window. Without an argument that is every window of every
/// tabpage; with an id it narrows to that one window, and an unknown id
/// yields an empty list like upstream `win_id2wp` returning NULL.
/// `win_findbuf({buf})` (`eval/window.c:516`): every window in every tabpage
/// that displays the buffer, in tabpage then window order. A buffer that is
/// loaded in no window answers an empty list, not an error.
fn call_win_findbuf(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: win_findbuf",
        ));
    }
    let Some(value) = args.first() else {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: win_findbuf",
        ));
    };
    let buffer = number_value(value)?;
    let mut windows = Vec::new();
    if let Ok(handle) = ox_types::BufHandle::try_from(buffer) {
        for tab in editor.tabpages() {
            for window in editor.tabpage_windows(tab).unwrap_or_default() {
                let shows = editor
                    .window(window)
                    .is_ok_and(|state| state.buffer == handle);
                if shows {
                    windows.push(Typval::Number(i64::from(window)));
                }
            }
        }
    }
    Ok(Typval::list(windows))
}

fn call_getwininfo(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: getwininfo",
        ));
    }
    // `win_id2wp` (`eval/window.c:118-137`): the argument is a window id, so
    // non-positive numbers and unknown ids produce an empty list.
    let target = match args.first() {
        None => None,
        Some(value) => {
            let id = number_value(value)?;
            let found = if id > 0 {
                editor.find_window_by_id(id)
            } else {
                None
            };
            if found.is_none() {
                return Ok(Typval::list(Vec::new()));
            }
            found
        }
    };
    let mut entries = Vec::new();
    for (tab_index, tab) in editor.tabpages().into_iter().enumerate() {
        let tabnr = Typval::Number(i64::try_from(tab_index + 1).unwrap_or(i64::MAX));
        let windows = editor.tabpage_windows(tab).unwrap_or_default();
        for &window in &windows {
            if target.is_some_and(|found| window != found) {
                continue;
            }
            entries.push(win_info(
                editor,
                window,
                window_number(&windows, window),
                tabnr.clone(),
            ));
            if target.is_some() {
                return Ok(Typval::list(entries));
            }
        }
    }
    Ok(Typval::list(entries))
}

/// `get_win_info` (`eval/window.c:347-377`), restricted to the fields this
/// port derives from real editor state. Upstream's screen-position fields
/// (`status_height`, `winrow`, `topline`, `botline`, `leftcol`, `winbar`,
/// `wincol`, `textoff`) and `variables` are left out rather than invented,
/// and `terminal`/`quickfix`/`loclist` are constant 0 because no such
/// window families exist here.
fn win_info(editor: &Editor, window: ox_types::WinHandle, winnr: Typval, tabnr: Typval) -> Typval {
    let buffer = editor
        .window(window)
        .map_or(0, |state| i64::from(state.buffer));
    let topline = editor.window(window).map_or(1, |state| state.topline);
    let (height, width, row, col) =
        editor
            .window_geometry(window)
            .map_or((-1, -1, 0, 0), |geometry| {
                (
                    i64::try_from(geometry.height).unwrap_or(i64::MAX),
                    i64::try_from(geometry.width).unwrap_or(i64::MAX),
                    geometry.row,
                    geometry.col,
                )
            });
    // `w_botline` needs the full scroll model; the last visible row is the
    // honest bound this port can state. No window carries a status line,
    // winbar, number column, or leftward scroll in the model, so those
    // report their zero-height/zero-offset values (`get_win_info`,
    // eval/window.c:347-377).
    let botline =
        (topline.saturating_add(usize::try_from(height.max(0)).unwrap_or(0))).saturating_sub(1);
    let variables = editor.window_variables(window).map_or_else(
        |_| Typval::dict(Vec::new()),
        |vars| {
            Typval::dict(
                vars.0
                    .iter()
                    .map(|(key, value)| (key.clone(), crate::excmd_exec::object_to_typval(value)))
                    .collect(),
            )
        },
    );
    Typval::dict(vec![
        (OxStr::from("tabnr"), tabnr),
        (OxStr::from("winnr"), winnr),
        (OxStr::from("winid"), Typval::Number(i64::from(window))),
        (OxStr::from("height"), Typval::Number(height)),
        (OxStr::from("status_height"), Typval::Number(0)),
        (
            OxStr::from("winrow"),
            Typval::Number(i64::try_from(row.saturating_add(1)).unwrap_or(i64::MAX)),
        ),
        (
            OxStr::from("topline"),
            Typval::Number(i64::try_from(topline).unwrap_or(i64::MAX)),
        ),
        (
            OxStr::from("botline"),
            Typval::Number(i64::try_from(botline).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("leftcol"), Typval::Number(0)),
        (OxStr::from("winbar"), Typval::Number(0)),
        (OxStr::from("width"), Typval::Number(width)),
        (OxStr::from("bufnr"), Typval::Number(buffer)),
        (
            OxStr::from("wincol"),
            Typval::Number(i64::try_from(col.saturating_add(1)).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("textoff"), Typval::Number(0)),
        (OxStr::from("terminal"), Typval::Number(0)),
        (OxStr::from("quickfix"), Typval::Number(0)),
        (OxStr::from("loclist"), Typval::Number(0)),
        (OxStr::from("variables"), variables),
    ])
}

/// `win_gotoid({expr})`: focus a live window by its global handle.
fn call_win_gotoid_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: win_gotoid",
        ));
    }
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: win_gotoid",
        ));
    }

    let id = number_value(&args[0])?;
    if id <= 0 {
        return Ok(Typval::Number(0));
    }
    let Some(window) = editor.find_window_by_id(id) else {
        return Ok(Typval::Number(0));
    };
    if editor.current_window() != Some(window) {
        editor
            .set_current_window(window)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    }
    Ok(Typval::Number(1))
}

/// `win_gettype([{nr}])`: the window's kind (`f_win_gettype`,
/// `eval/window.c:705-729`). Lookup matches upstream: ids at or above
/// `LOWEST_WINDOW_ID` resolve by handle, smaller numbers are current-
/// tabpage positions, and anything unresolvable answers `"unknown"`.
/// The kind order matches upstream too: preview, floating, command,
/// quickfix/loclist, else `""`. Two arms cannot fire in this model and
/// are documented, not faked: there is no context-switch window (never
/// `"autocmd"`) and no tracked command-line window buffer (never
/// `"command"`).
fn call_win_gettype_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: win_gettype",
        ));
    }
    let number = match args.first() {
        None => 0,
        Some(value) => number_value(value)?,
    };
    let window = resolve_tiled_window_by_number(editor, number);
    let Some(window) = window else {
        return Ok(Typval::String(OxStr::from("unknown")));
    };
    if matches!(
        editor.options().get_window(window, "previewwindow"),
        Ok(OptionValue::Boolean(true))
    ) {
        return Ok(Typval::String(OxStr::from("preview")));
    }
    if editor.window_config(window).ok().flatten().is_some() {
        return Ok(Typval::String(OxStr::from("popup")));
    }
    let buffer_quickfix = editor
        .window(window)
        .ok()
        .and_then(|state| editor.options().get_buffer(state.buffer, "buftype").ok())
        .is_some_and(|value| matches!(value, OptionValue::String(text) if text == "quickfix"));
    if buffer_quickfix {
        let source = editor
            .window(window)
            .ok()
            .and_then(|state| state.loclist_ref)
            .unwrap_or(window);
        let kind = if editor.loclist(source).is_some() {
            "loclist"
        } else {
            "quickfix"
        };
        return Ok(Typval::String(OxStr::from(kind)));
    }
    Ok(Typval::String(OxStr::from("")))
}

fn call_window_builtin(editor: &Editor, name: &str, args: &[Typval]) -> Typval {
    if name == "win_getid" {
        let window_number = args.first().and_then(typval_number).unwrap_or(0);
        let tab_number = args.get(1).and_then(typval_number).unwrap_or(0);
        let tab = if tab_number <= 0 {
            editor.current_tabpage()
        } else {
            one_based_index(tab_number).and_then(|index| editor.tabpages().get(index).copied())
        };
        let window = tab.and_then(|tab| {
            let windows = editor.tabpage_windows(tab).ok()?;
            if window_number <= 0 {
                editor
                    .current_window()
                    .filter(|window| windows.contains(window))
            } else {
                one_based_index(window_number).and_then(|index| windows.get(index).copied())
            }
        });
        return Typval::Number(window.map_or(0, i64::from));
    }

    let number = args.first().and_then(typval_number).unwrap_or(-1);
    let window = resolve_window_by_number(editor, number);
    // `winheight()` is the renderable viewport height, not the raw layout
    // frame height; keep its statusline/message reservations in one owner.
    let value = window
        .and_then(|window| {
            if name == "winheight" {
                editor.window_text_height(window).ok()
            } else {
                editor
                    .window_geometry(window)
                    .ok()
                    .map(|geometry| geometry.width)
            }
        })
        .map_or(-1, |value| i64::try_from(value).unwrap_or(i64::MAX));
    Typval::Number(value)
}

fn call_screen_builtin(editor: &Editor, name: &str, args: &[Typval]) -> Typval {
    let row = args.first().and_then(typval_number).unwrap_or(0);
    let column = args.get(1).and_then(typval_number).unwrap_or(0);
    let cell = screen_cell(editor, row, column);
    match name {
        "screenattr" => Typval::Number(if cell.is_some() { 0 } else { -1 }),
        "screenchar" => Typval::Number(
            cell.as_ref()
                .and_then(|text| text.chars().next())
                .map_or(-1, |character| i64::from(character as u32)),
        ),
        "screenchars" => Typval::list(
            cell.as_ref()
                .and_then(|text| text.chars().next())
                .map(|character| vec![Typval::Number(i64::from(character as u32))])
                .unwrap_or_default(),
        ),
        "screenstring" => Typval::String(OxStr::from(cell.as_deref().unwrap_or(""))),
        _ => unreachable!(),
    }
}

/// `screencol()` (`eval/funcs.c:f_screencol`): the cursor's one-based screen
/// column. Upstream returns `ui_current_col() + 1`, which is the window's
/// screen column plus the cursor's virtual column within the window.
fn call_screencol(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: screencol",
        ));
    }
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let geometry = editor
        .window_geometry(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let buffer = editor
        .buffer(state.buffer)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let text = buffer
        .text()
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let line = text
        .line(state.cursor.lnum)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let virtual_column = cursor_vcol(&line, state.cursor.col, position_tabstop(editor));
    let coladd = usize::try_from(state.coladd.max(0)).unwrap_or(usize::MAX);
    let column = geometry
        .col
        .saturating_add(virtual_column)
        .saturating_add(coladd)
        .saturating_add(1);
    Ok(Typval::Number(i64::try_from(column).unwrap_or(i64::MAX)))
}

/// `screenrow()` (`eval/funcs.c:f_screenrow`): the cursor's one-based screen
/// row. Upstream returns `ui_current_row() + 1`, which is the window's
/// screen row plus the cursor's row within the window.
fn call_screenrow(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: screenrow",
        ));
    }
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let geometry = editor
        .window_geometry(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let row_in_window = state.cursor.lnum.saturating_sub(state.topline);
    let row = geometry.row.saturating_add(row_in_window).saturating_add(1);
    Ok(Typval::Number(i64::try_from(row).unwrap_or(i64::MAX)))
}

/// Resolves one screen cell to the character the composed screen shows.
///
/// Only the current tabpage is on screen, so only its windows can own a cell.
/// Windows in other tabpages have overlapping geometries and would otherwise
/// win this search and return a character from something invisible.
///
/// Blank cells are spaces, not missing cells: a coordinate inside the
/// `&lines`/`&columns` grid that no window covers (the status-line and
/// message rows beneath the windows, or a window row past the buffer's last
/// line) still displays the background, exactly like upstream's default grid
/// whose every in-range cell has a character (`eval/funcs.c:5711-5729`).
/// Only coordinates outside the grid have no cell at all, which upstream
/// reports as -1. Callers must not feed that -1 to `nr2char()`: the resulting
/// `E5070` is what `Screenline(&lines - 1)` in the oldtest suite trips over
/// when the grid reports holes instead of blanks.
fn screen_cell(editor: &Editor, row: i64, column: i64) -> Option<String> {
    let row = usize::try_from(row.checked_sub(1)?).ok()?;
    let column = usize::try_from(column.checked_sub(1)?).ok()?;
    if row >= screen_size(editor, "lines") || column >= screen_size(editor, "columns") {
        return None;
    }
    let windows = editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage_windows(tab).ok())
        .unwrap_or_default();
    for window in windows {
        let Ok(geometry) = editor.window_geometry(window) else {
            continue;
        };
        if row < geometry.row
            || row >= geometry.row + geometry.height
            || column < geometry.col
            || column >= geometry.col + geometry.width
        {
            continue;
        }
        let Ok(state) = editor.window(window) else {
            continue;
        };
        let line = state.topline + row - geometry.row;
        let cell = editor
            .buffer(state.buffer)
            .ok()
            .and_then(|buffer| buffer.text().ok())
            .and_then(|text| text.line(line).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .and_then(|text| {
                text.chars()
                    .nth(column - geometry.col)
                    .map(|character| character.to_string())
            })
            .unwrap_or_else(|| " ".to_owned());
        return Some(cell);
    }
    // In-grid but uncovered: the background shows through.
    Some(" ".to_owned())
}

/// The screen grid extent in cells for `&lines`/`&columns`, mirroring
/// `screen_number_option` (`excmd_exec.rs`): the upstream default grid always
/// exists, so an unreadable option degrades to unbounded rather than empty.
fn screen_size(editor: &Editor, name: &str) -> usize {
    match option_value(editor, name, SetLayer::Effective) {
        Some(OptionValue::Number(value)) if *value > 0 => {
            usize::try_from(*value).unwrap_or(usize::MAX)
        }
        _ => usize::MAX,
    }
}

/// `winsaveview()` (`eval/window.c:f_winsaveview`): returns a dict with
/// `lnum`, `col`, `coladd`, `curswant`, `topline`, `topfill`, `leftcol`,
/// `skipcol` from the current window's state.
fn call_winsaveview(editor: &Editor, _args: &[Typval]) -> ox_eval::Result<Typval> {
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let cursor_col = i64::try_from(state.cursor.col).unwrap_or(i64::MAX);
    let curswant = if state.set_curswant {
        // Recompute from cursor virtual column (move.c:update_curswant)
        let lines = crate::excmd_exec::buffer_lines(editor, state.buffer)
            .map_err(|error| EvalError::new("E16", 0, error))?;
        let line = lines
            .get(state.cursor.lnum.saturating_sub(1))
            .map_or(&[][..], Vec::as_slice);
        i64::try_from(cursor_vcol(
            line,
            state.cursor.col,
            position_tabstop(editor),
        ))
        .unwrap_or(i64::MAX)
    } else {
        state.curswant
    };
    Ok(Typval::dict(vec![
        (
            OxStr::from("lnum"),
            Typval::Number(i64::try_from(state.cursor.lnum).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("col"), Typval::Number(cursor_col)),
        (OxStr::from("coladd"), Typval::Number(state.coladd)),
        (OxStr::from("curswant"), Typval::Number(curswant)),
        (
            OxStr::from("topline"),
            Typval::Number(i64::try_from(state.topline).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("topfill"), Typval::Number(0)),
        (OxStr::from("leftcol"), Typval::Number(0)),
        (OxStr::from("skipcol"), Typval::Number(0)),
    ]))
}

/// `winrestview({dict})` (`eval/window.c:f_winrestview`): restores the
/// view from a dict. Only the restorable subset is applied.
fn call_winrestview(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let Typval::Dict(dict_ref) = &args[0] else {
        return Err(EvalError::new("E1297", 0, "Dictionary required"));
    };
    let dict = dict_ref
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let mut lnum = None;
    let mut col = None;
    let mut coladd = None;
    let mut curswant = None;
    let mut topline = None;
    for entry in &dict.entries {
        match entry.key.as_bytes() {
            b"lnum" => lnum = typval_number(&entry.value),
            b"col" => col = typval_number(&entry.value),
            b"coladd" => coladd = typval_number(&entry.value),
            b"curswant" => curswant = typval_number(&entry.value),
            b"topline" => topline = typval_number(&entry.value),
            _ => {}
        }
    }
    if let Some(topline) = topline {
        let topline = usize::try_from(topline.max(1)).unwrap_or(usize::MAX);
        editor
            .set_window_topline(window, topline)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    }
    if let Some(lnum) = lnum {
        let state = editor
            .window(window)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
        let col = match col {
            Some(col) => usize::try_from(col.max(0)).unwrap_or(usize::MAX),
            None => state.cursor.col,
        };
        let coladd = coladd.unwrap_or(state.coladd);
        let position = ox_text::Position {
            lnum: usize::try_from(lnum.max(1)).unwrap_or(usize::MAX),
            col,
        };
        editor
            .set_window_cursor(window, position)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
        if let Ok(state_mut) = editor.window_mut(window) {
            state_mut.coladd = coladd;
            if let Some(curswant) = curswant {
                state_mut.curswant = curswant;
                state_mut.set_curswant = false;
            }
        }
    } else if coladd.is_some() || curswant.is_some() {
        // `winrestview` with only `coladd`/`curswant` (no `lnum`) still
        // applies those fields to the current cursor position.
        if let Ok(state_mut) = editor.window_mut(window) {
            if let Some(coladd) = coladd {
                state_mut.coladd = coladd;
            }
            if let Some(curswant) = curswant {
                state_mut.curswant = curswant;
                state_mut.set_curswant = false;
            }
        }
    }
    Ok(Typval::Number(0))
}

/// `wincol()` (`eval/window.c:f_wincol`): the cursor's one-based window column.
fn call_wincol(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: wincol",
        ));
    }
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let cursor = state.cursor;
    let coladd = state.coladd;
    let buffer = editor
        .buffer(state.buffer)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let text = buffer
        .text()
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let line = text
        .line(cursor.lnum)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let virtual_column = cursor_vcol(&line, cursor.col, position_tabstop(editor));
    let coladd = usize::try_from(coladd.max(0)).unwrap_or(usize::MAX);
    let column = virtual_column.saturating_add(coladd).saturating_add(1);
    Ok(Typval::Number(i64::try_from(column).unwrap_or(i64::MAX)))
}

/// `winline()` (`eval/window.c:f_winline`): the cursor's window row (1-based).
/// Computed from cursor lnum minus the window's topline.
fn call_winline(editor: &Editor, _args: &[Typval]) -> ox_eval::Result<Typval> {
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let row = state
        .cursor
        .lnum
        .saturating_sub(state.topline)
        .saturating_add(1);
    Ok(Typval::Number(i64::try_from(row).unwrap_or(i64::MAX)))
}

fn call_getwinvar(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: getwinvar",
        ));
    }
    if args.len() > 3 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: getwinvar",
        ));
    }
    let fallback = args
        .get(2)
        .cloned()
        .unwrap_or(Typval::String(OxStr::from("")));
    let number = typval_number(&args[0]).unwrap_or(0);
    let Some(tab) = editor.current_tabpage() else {
        return Ok(fallback);
    };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let window = if number <= 0 {
        editor.current_window()
    } else {
        one_based_index(number).and_then(|index| windows.get(index).copied())
    };
    let Some(window) = window else {
        return Ok(fallback);
    };
    let name = crate::excmd_exec::typval_to_text(&args[1]);
    if let Some(option) = name.strip_prefix('&') {
        let Some(metadata) = crate::option_metadata(option) else {
            return Ok(fallback);
        };
        let value = if metadata
            .scopes
            .contains(&crate::options::OptionScope::Window)
        {
            editor.options().get_window(window, metadata.name).ok()
        } else {
            editor.options().get_global(metadata.name).ok()
        };
        return Ok(value.map_or(fallback, |value| match value {
            crate::options::OptionValue::Boolean(flag) => Typval::Number(i64::from(*flag)),
            crate::options::OptionValue::Number(number) => Typval::Number(*number),
            crate::options::OptionValue::String(text) => Typval::String(OxStr::from(text.as_str())),
        }));
    }
    let Ok(variables) = editor.window_variables(window) else {
        return Ok(fallback);
    };
    if name.is_empty() {
        return Ok(Typval::dict_with_entries(
            variables
                .0
                .iter()
                .map(|(key, value)| {
                    scope_var_entry(
                        ScopeKind::Window,
                        key,
                        &crate::excmd_exec::object_to_typval(value),
                    )
                })
                .collect(),
        ));
    }
    Ok(variables
        .0
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .map_or(fallback, |(_, value)| {
            crate::excmd_exec::object_to_typval(value)
        }))
}

/// `winlayout([{tabnr}])` (`eval/window.c:f_winlayout`): returns the tiled
/// window layout of the current or named tabpage as a nested list.
///
/// Each leaf frame yields `['leaf', winid]`; a row of vertical splits yields
/// `['row', [children]]`; a column of horizontal splits yields
/// `['col', [children]]`. An unknown tabpage number returns an empty list,
/// matching upstream `find_tabpage` returning `NULL`.
fn call_winlayout(editor: &Editor, args: &[Typval]) -> Typval {
    let tab = if args.is_empty() {
        editor.current_tabpage()
    } else {
        let number = typval_number(&args[0]).unwrap_or(0);
        match number.cmp(&0) {
            std::cmp::Ordering::Equal => editor.current_tabpage(),
            std::cmp::Ordering::Greater => {
                one_based_index(number).and_then(|index| editor.tabpages().get(index).copied())
            }
            std::cmp::Ordering::Less => None,
        }
    };
    let Some(tab) = tab else {
        return Typval::list(Vec::new());
    };
    let Ok(tabpage) = editor.tabpage(tab) else {
        return Typval::list(Vec::new());
    };
    frame_layout(tabpage.layout().root())
}

/// Recursively converts a tiled frame into the `winlayout()` nested-list shape
/// (`eval/window.c:get_framelayout`).
fn frame_layout(frame: &Frame) -> Typval {
    match frame {
        Frame::Leaf(leaf) => Typval::list(vec![
            Typval::String(OxStr::from("leaf")),
            Typval::Number(i64::from(leaf.window)),
        ]),
        Frame::Row { children, .. } => Typval::list(vec![
            Typval::String(OxStr::from("row")),
            Typval::list(children.iter().map(frame_layout).collect()),
        ]),
        Frame::Column { children, .. } => Typval::list(vec![
            Typval::String(OxStr::from("col")),
            Typval::list(children.iter().map(frame_layout).collect()),
        ]),
    }
}

/// `f_setwinvar` (`eval/window.c:862-885`): set one window-local variable
/// (or `&option`) in the numbered window, current when the number is 0.
/// An unresolvable window fails E957 like upstream's `find_win_by_nr`.
fn call_setwinvar(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() < 3 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: setwinvar",
        ));
    }
    if args.len() > 3 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: setwinvar",
        ));
    }
    let number = typval_number(&args[0]).unwrap_or(0);
    let tab = editor
        .current_tabpage()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window number"))?;
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let window = if number <= 0 {
        editor.current_window()
    } else {
        one_based_index(number).and_then(|index| windows.get(index).copied())
    };
    let Some(window) = window else {
        return Err(EvalError::new("E957", 0, "Invalid window number"));
    };
    let name = typval_to_text(&args[1]);
    if let Some(option) = name.strip_prefix('&') {
        let Some(metadata) = crate::option_metadata(option) else {
            return Ok(Typval::Number(0));
        };
        if metadata
            .scopes
            .contains(&crate::options::OptionScope::Window)
        {
            let value = match args[2].clone() {
                Typval::Number(value) => {
                    if metadata.value_type == crate::options::OptionType::Boolean {
                        OptionValue::Boolean(value != 0)
                    } else {
                        OptionValue::Number(value)
                    }
                }
                Typval::String(text) => OptionValue::String(text.to_string_lossy().into_owned()),
                other => {
                    return Err(EvalError::new(
                        "E728",
                        0,
                        format!("Use a Number or a String for a window option: {other:?}"),
                    ));
                }
            };
            editor
                .options_mut()
                .set_window(window, metadata.name, value)
                .map_err(|error| EvalError::new("E355", 0, error.to_string()))?;
        }
        return Ok(Typval::Number(0));
    }
    let object = crate::excmd_exec::typval_to_object(&args[2]);
    editor
        .window_variables_mut(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?
        .insert(OxStr::from(name.as_str()), object);
    Ok(Typval::Number(0))
}

/// `win_execute({id}, {command} [, {silent}])` (`eval/window.c:523-543`):
/// runs `{command}` with `{id}` as the current window and answers the
/// captured output, like `execute()` does for the current window.
///
/// The switch itself fires no autocommands and keeps the working directory
/// (`kCtxNoDisplay | kCtxKeepCwd | kCtxValidate`): `set_current_window`
/// fires no autocommands either, and the directory the switch applies is
/// restored before the command runs. The command itself runs normally, so
/// autocommands and a `:cd` inside behave exactly like they do under
/// `execute()`. An unknown `{id}` answers `""` with no error; a failing
/// command propagates like `execute()`.
///
/// Small numbers are window numbers, not ids, and never resolve here:
/// `win_id2wp_tp` looks ids up by handle, so only `{id}` at or above
/// `LOWEST_WINDOW_ID` can name a window.
fn call_win_execute<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    // Arity (2..=3) is enforced by the dispatcher; `{command}` is `args[1]`.
    let id = win_id_number(&args[0]);
    // `execute_common` reads the silent flag through `tv_get_string_buf_chk`
    // (`eval/funcs.c:1179`), which rejects containers; the value itself only
    // affects display, which capture discards, so it is validated and ignored.
    if let Some(silent) = args.get(2) {
        super::input_string_arg(silent)?;
    }
    let Some(target) = (id >= LOWEST_WINDOW_ID)
        .then(|| host.access.with_ex_editor(|editor| editor.find_window_by_id(id)))
        .flatten()
    else {
        return Ok(Typval::String(OxStr::from("")));
    };
    // A List argument runs every item as its own source line
    // (`execute_common` hands `do_cmdline` a `get_list_line` cookie,
    // `eval/funcs.c:1206-1216`); the scalar form is one command line.
    let logical = match &args[1] {
        Typval::List(items) => {
            let text = items
                .borrow()
                .items
                .iter()
                .map(typval_to_text)
                .collect::<Vec<String>>()
                .join("\n");
            host.runtime
                .scripts
                .join_logical_lines(&text)
                .map_err(|error| EvalError::new("E488", 0, error.to_string()))?
        }
        command => vec![LogicalLine {
            text: typval_to_text(command),
            first_line: host.runtime.scripts.current_line(),
        }],
    };
    // `ctx_switch` (`context.c:618-636`): remember the full context the
    // switch disturbs — window, previous window, and working directory — so
    // the command runs in the target window and everything is restored after.
    let previous = host
        .access
        .with_ex_editor(|editor| editor.current_window());
    let previous_previous = host
        .access
        .with_ex_editor(|editor| editor.previous_window());
    let saved_directory = std::env::current_dir().ok();
    host.access
        .with_ex_editor(|editor| {
            if editor.current_window() != Some(target) {
                editor.set_current_window(target)
            } else {
                Ok(())
            }
        })
        .map_err(|error: crate::editor::EditorError| {
            EvalError::new("E957", 0, error.to_string())
        })?;
    // `kCtxKeepCwd`: the target window's local directory must not leak into
    // the command's working directory (`set_current_window` applies it).
    if let Some(directory) = &saved_directory {
        let _ = std::env::set_current_dir(directory);
    }
    // Addresses and buffer-local state resolve against the target window,
    // which is current from here until the restore below.
    let program = parse_program(
        &host.runtime.user_commands,
        host.access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    let message_start = host
        .access
        .with_ex_editor(|editor| editor.messages().len());
    let flow = run_program(
        host.runtime,
        host.access,
        scope,
        host.lua,
        &program,
        0,
        program.len(),
    );
    host.access.with_ex_editor(|editor| {
        if let Some(window) = previous
            && editor.current_window() != Some(window)
        {
            let _ = editor.set_current_window(window);
        }
        editor.set_previous_window(previous_previous);
    });
    if !matches!(flow, Flow::Normal) {
        return Err(flow_to_eval_error(flow, "win_execute"));
    }
    let mut output = String::new();
    let messages = host
        .access
        .with_ex_editor(|editor| editor.messages().to_vec());
    // `execute_common` appends raw message bytes to `capture_ga`
    // (`eval/funcs.c:1202-1216`): no separator joins messages, display
    // newlines are never captured, and a message's own newlines do not
    // survive either — `:ls`, `1,2print`, and `echo "a\nb"` all capture
    // with every `\n` removed. History entries are the exception only
    // because they store newlines as NUL (`msg_hist_add`), which the
    // capture then takes raw (`echom "a\nb"` captures `a\0b`).
    for message in &messages[message_start..] {
        let Object::String(text) = &message.content else {
            continue;
        };
        let rendered = text.to_string_lossy();
        if message.history {
            output.push_str(&rendered.replace('\n', "\0"));
        } else {
            output.push_str(&rendered.replace('\n', ""));
        }
    }
    host.access
        .with_ex_editor(|editor| editor.truncate_messages(message_start));
    Ok(Typval::String(OxStr(output.into_bytes())))
}

/// `tv_get_number` (`typval.c`) as `f_win_execute` uses it: strict numbers
/// pass through, strings contribute their leading numeric prefix, and
/// anything else is 0.
fn win_id_number(value: &Typval) -> i64 {
    match value {
        Typval::Number(value) => *value,
        Typval::Bool(value) => i64::from(*value),
        // `(int)` truncation (`tv_get_number`); below 2^53 the truncation
        // is exact and in range, so `as` cannot saturate. Anything else
        // (NaN, infinities, out-of-range magnitudes) cannot name a window
        // id, like upstream's 0 for a non-numeric value.
        Typval::Float(value) => {
            if *value >= 1.0 && *value < 9_007_199_254_740_992.0 {
                value.trunc() as i64
            } else {
                0
            }
        }
        Typval::String(value) => leading_number(&value.to_string_lossy()),
        Typval::Channel(value) | Typval::Job(value) => i64::try_from(*value).unwrap_or(0),
        _ => 0,
    }
}

/// `charset.c:vim_str2nr` as `tv_get_number` uses it: blanks, an optional
/// sign, then a decimal prefix, else zero.
fn leading_number(text: &str) -> i64 {
    let text = text.trim_start();
    let bytes = text.as_bytes();
    let mut end = 0;
    if bytes.first().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
        end = 1;
    }
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    text[..end].parse().unwrap_or(0)
}

/// `win_screenpos({nr})` (`eval/window.c:636-642`): `[row, col]`, 1-based,
/// `[0, 0]` for an invalid window. Lookup matches `find_win_by_nr_or_id`:
/// ids at or above `LOWEST_WINDOW_ID` resolve by handle, smaller numbers are
/// current-tabpage positions. The position is the composed screen position
/// the geometry reports, the same row `getwininfo()` reports as `winrow`.
fn call_win_screenpos(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let number = number_value(&args[0])?;
    let position = resolve_window_by_number_impl(editor, number, false)
        .and_then(|window| editor.window_geometry(window).ok())
        .map(|geometry| {
            let row = i64::try_from(geometry.row.saturating_add(1)).unwrap_or(i64::MAX);
            let col = i64::try_from(geometry.col.saturating_add(1)).unwrap_or(i64::MAX);
            (row, col)
        })
        .unwrap_or((0, 0));
    Ok(Typval::list(vec![
        Typval::Number(position.0),
        Typval::Number(position.1),
    ]))
}

/// `screenpos({winid}, {lnum}, {col})` (`move.c:1227-1259`): the screen
/// position of a buffer character as `{row, col, endcol, curscol}`, all zero
/// when the position is not visible and empty when the window is invalid.
///
/// The port follows `textpos2screenpos` (`move.c:1153-1225`) with this
/// window model: no number column, no leftward scroll, no tabline offset and
/// no smooth-scroll skip — `win_col_off` and `w_leftcol` are 0, so the
/// window-relative column is the virtual column and the row is the topline
/// distance plus diff filler. Wrapping shifts the character's own row and
/// column by the upstream formula. A tab's cursor sits on its last cell
/// while any other character's cursor sits on its first, which is what
/// `curscol == endcol` for tabs and `curscol == col` otherwise takes.
///
/// Named gaps: closed folds are computed unfolded (fold state is keyed to the
/// current window, while `screenpos` names any window), and conceal is
/// ignored exactly like upstream (`textpos2screenpos` ignores it too).
fn call_screenpos(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let number = number_value(&args[0])?;
    let Some(window) = resolve_window_by_number_impl(editor, number, false) else {
        return Ok(Typval::dict(Vec::new()));
    };
    let lnum = lnum_arg_for_window(editor, &args[1], window)?;
        // `tv_get_number ... - 1`, floored at zero (`move.c:1238-1241).
    let byte_col = number_value(&args[2])?.saturating_sub(1).max(0);
    // A valid window always answers all four keys (`f_screenpos` adds them
    // unconditionally, `move.c:1251-1254`); only an invalid window answers
    // the empty dict. Invisible positions report zero for every key.
    // Key order follows the `tv_dict_add_nr` sequence (`move.c:1251-1254`).
    // Display order differs (upstream hashes dicts); field values match.
    let zeros = || {
        Typval::dict(vec![
            (OxStr::from("row"), Typval::Number(0)),
            (OxStr::from("col"), Typval::Number(0)),
            (OxStr::from("curscol"), Typval::Number(0)),
            (OxStr::from("endcol"), Typval::Number(0)),
        ])
    };
    let buffer = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?
        .buffer;
    let lines = buffer_lines(editor, buffer)
        .map_err(|error| EvalError::new("E749", 0, error.to_string()))?;
    let line_count = i64::try_from(lines.len()).unwrap_or(i64::MAX);
    // `f_screenpos` errors only past the last line (`E966`,
    // `move.c:1242-1245`); a line at or below zero has no screen row, like
    // upstream's `existing_row` being false.
    if lnum > line_count {
        return Err(EvalError::new(
            "E966",
            0,
            format!("Invalid line number: {lnum}"),
        ));
    }
    if lnum < 1 {
        return Ok(zeros());
    }
    let Ok(line_index) = usize::try_from(lnum - 1) else {
        return Ok(zeros());
    };
    let Some(line) = lines.get(line_index) else {
        return Ok(zeros());
    };
    let geometry = editor.window_geometry(window).map_err(|error| {
        EvalError::new("E957", 0, error.to_string())
    })?;
    let topline = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?
        .topline;
    let tabstop = position_tabstop(editor);
    // `getvcol` (`plines.c`): the zero-based first virtual column and the
    // inclusive last one of the character covering `byte_col`, snapping
    // inside a multibyte character back onto its head and past the end of
    // the line onto the NUL that counts as one cell.
    let (start_vcol, end_last) = getvcol(line, byte_col, tabstop);
    let snap = vcol_to_byte(line, start_vcol, tabstop);
    let is_tab = line.get(snap) == Some(&b'\t');
    let cursor_vcol = if is_tab { end_last } else { start_vcol };
    // `textpos2screenpos`: rows below the topline plus filler above the line
    // (`win_get_fill`, `move.c:1174-1176`); lines above the topline are not
    // on screen.
    let filler = crate::diffmode::diff_filler_for(editor, window, lnum);
    let row_in_window = lnum
        .saturating_sub(i64::try_from(topline).unwrap_or(i64::MAX))
        .saturating_add(filler);
    if row_in_window < 0 {
        return Ok(zeros());
    }
    // Long-line wrapping (`move.c:1192-1197`): the character's own row and
    // column shift by whole window widths. There is no horizontal scroll in
    // this model, so `w_leftcol` is 0 and the check is the width alone.
    let wrap = editor
        .options()
        .get_window(window, "wrap")
        .map(|value| !matches!(value, OptionValue::Boolean(false)))
        .unwrap_or(true);
    let width = geometry.width;
    if width == 0 {
        return Ok(zeros());
    }
    let (col_adjusted, row_extra) = if wrap && start_vcol >= width {
        let shift = (start_vcol - width) / width + 1;
        (start_vcol - shift * width, shift)
    } else {
        (start_vcol, 0)
    };
    let total_row = row_in_window.saturating_add(i64::try_from(row_extra).unwrap_or(0));
    let height = i64::try_from(geometry.height).unwrap_or(0);
    if total_row >= height || col_adjusted >= width {
        // Left, right, or below the window: everything is zero, and a
        // non-local call zeroes the row too (`move.c:1204-1212`).
        return Ok(zeros());
    }
    // `coloff = col - scol + wincol + 1` (`move.c:1201`): every virtual
    // column shifts by the same window offset.
    let col_offset = i64::try_from(col_adjusted).unwrap_or(0)
        - i64::try_from(start_vcol).unwrap_or(0)
        + i64::try_from(geometry.col).unwrap_or(0)
        + 1;
    let row_out = i64::try_from(geometry.row).unwrap_or(0) + 1 + total_row;
    Ok(Typval::dict(vec![
        (OxStr::from("row"), Typval::Number(row_out)),
        (
            OxStr::from("col"),
            Typval::Number(i64::try_from(start_vcol).unwrap_or(0) + col_offset),
        ),
        (
            OxStr::from("curscol"),
            Typval::Number(i64::try_from(cursor_vcol).unwrap_or(0) + col_offset),
        ),
        (
            OxStr::from("endcol"),
            Typval::Number(i64::try_from(end_last).unwrap_or(0) + col_offset),
        ),
    ]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "editor and buffer setup must succeed for the lookup test"
    )]
    fn bufwin_id_and_number_find_first_displaying_window() {
        let mut editor = Editor::new();
        let first = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(first, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let first = editor.current_buffer().unwrap();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .set_window_buffer(
                editor.current_window().unwrap(),
                buffer,
                crate::editor::BufferRelease::KeepLoaded,
            )
            .unwrap();
        // The current window displays `buffer`: bufwinid resolves by handle
        // and bufwinnr by position in the current tab (f_bufwinid/f_bufwinnr).
        let id = call_bufwin_builtin(&editor, &[Typval::Number(i64::from(buffer))], false)
            .unwrap_or(Typval::Number(-1));
        assert_eq!(
            id,
            Typval::Number(i64::from(editor.current_window().unwrap()))
        );
        let nr = call_bufwin_builtin(&editor, &[Typval::Number(i64::from(buffer))], true)
            .unwrap_or(Typval::Number(-1));
        assert_eq!(nr, Typval::Number(1));
        // A buffer no window displays answers -1.
        let absent = call_bufwin_builtin(&editor, &[Typval::Number(i64::from(first))], false)
            .unwrap_or(Typval::Number(0));
        assert_eq!(absent, Typval::Number(-1));
    }

    /// `bufwinid()` / `bufwinnr()`
    use ox_text::Buffer;

    use super::*;

    /// One listed buffer shown in an 80x24 tabpage: the fresh editor state
    /// the other builtin test modules build.
    fn editor_with_window() -> Editor {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"alpha\nbravo\n").unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, crate::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        editor
    }

    /// `f_getwininfo` (`eval/window.c:431-464`): no argument covers every
    /// window of every tabpage; an id narrows to exactly that window, and
    /// the entry reports the shown buffer.
    #[test]
    fn getwininfo_lists_windows() {
        let editor = editor_with_window();
        let current = editor.current_window().unwrap();
        let winid = i64::from(current);

        let all = call_getwininfo(&editor, &[]).unwrap();
        let Typval::List(list) = &all else {
            panic!("getwininfo() must return a list, got {all:?}")
        };
        assert!(!list.borrow().items.is_empty());

        let one = call_getwininfo(&editor, &[Typval::Number(winid)]).unwrap();
        let Typval::List(list) = &one else {
            panic!("getwininfo(winid) must return a list, got {one:?}")
        };
        let items = list.borrow().items.clone();
        assert_eq!(items.len(), 1);
        let Typval::Dict(dict) = &items[0] else {
            panic!("getwininfo entry must be a dict")
        };
        let field = |key: &[u8]| {
            dict.borrow()
                .entries
                .iter()
                .find(|entry| entry.key.as_bytes() == key)
                .map(|entry| entry.value.clone())
        };
        let expected_buffer = i64::from(editor.window(current).unwrap().buffer);
        assert_eq!(field(b"bufnr"), Some(Typval::Number(expected_buffer)));
        assert_eq!(field(b"winid"), Some(Typval::Number(winid)));

        let unknown = call_getwininfo(&editor, &[Typval::Number(9_999_999)]).unwrap();
        let Typval::List(list) = &unknown else {
            panic!("getwininfo(unknown id) must return a list")
        };
        assert!(list.borrow().items.is_empty());
    }

    #[test]
    fn win_gettype_zero_resolves_current_window() {
        let editor = editor_with_window();
        let kind = call_win_gettype_builtin(&editor, &[Typval::Number(0)]).unwrap();
        assert_eq!(kind, Typval::String(OxStr::from("")));
    }

    #[test]
    fn win_gettype_explicit_id_resolves_window() {
        let editor = editor_with_window();
        let current = editor.current_window().unwrap();
        let kind =
            call_win_gettype_builtin(&editor, &[Typval::Number(i64::from(current))]).unwrap();
        assert_eq!(kind, Typval::String(OxStr::from("")));

        let unknown = call_win_gettype_builtin(&editor, &[Typval::Number(9_999_999)]).unwrap();
        assert_eq!(unknown, Typval::String(OxStr::from("unknown")));
    }

    #[test]
    fn win_gettype_numbered_lookup_skips_floating_window() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let tiled = editor.current_window().unwrap();

        let float_buffer = editor.create_buffer(true).unwrap();
        let config = crate::layout::WinConfig::new(
            crate::layout::RelativeTo::Editor,
            crate::layout::Anchor::NorthWest,
            0.0,
            0.0,
            3,
            2,
        )
        .unwrap();
        let float = editor.open_float(tab, float_buffer, config).unwrap();

        // The tiled window stays current; the float is not in the numbered
        // sequence, so `win_gettype(1)` still resolves the tiled window.
        assert_eq!(editor.current_window(), Some(tiled));
        let kind = call_win_gettype_builtin(&editor, &[Typval::Number(1)]).unwrap();
        assert_eq!(kind, Typval::String(OxStr::from("")));

        // An explicit floating-window id still reports "popup".
        let kind = call_win_gettype_builtin(&editor, &[Typval::Number(i64::from(float))]).unwrap();
        assert_eq!(kind, Typval::String(OxStr::from("popup")));
    }

    fn qf_item(buffer: ox_types::BufHandle, lnum: i64, text: &str) -> Typval {
        Typval::dict(vec![
            (OxStr::from("bufnr"), Typval::Number(i64::from(buffer))),
            (OxStr::from("lnum"), Typval::Number(lnum)),
            (OxStr::from("text"), Typval::String(OxStr::from(text))),
        ])
    }

    #[test]
    fn win_gettype_classifies_loclist_and_quickfix_windows() {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"alpha\nbravo\n").unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let source = editor.current_window().unwrap();

        // A `:lopen` display window points to the source window's loclist.
        crate::quickfix::call(
            &mut editor,
            "setloclist",
            &[
                Typval::Number(0),
                Typval::list(vec![qf_item(buffer, 1, "loc-a")]),
            ],
        )
        .unwrap();
        let loclist_window =
            crate::quickfix::open(&mut editor, crate::quickfix::QfScope::Loclist(source)).unwrap();
        let kind = call_win_gettype_builtin(&editor, &[Typval::Number(i64::from(loclist_window))])
            .unwrap();
        assert_eq!(kind, Typval::String(OxStr::from("loclist")));

        // A `:copen` quickfix window is not a location list.
        crate::quickfix::call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![qf_item(buffer, 2, "qf-a")])],
        )
        .unwrap();
        let quickfix_window =
            crate::quickfix::open(&mut editor, crate::quickfix::QfScope::Quickfix).unwrap();
        let kind = call_win_gettype_builtin(&editor, &[Typval::Number(i64::from(quickfix_window))])
            .unwrap();
        assert_eq!(kind, Typval::String(OxStr::from("quickfix")));
    }

    /// Reads a global string set by a probe script.
    fn probe_string(exec: &crate::ExExecutor, name: &str) -> Option<String> {
        match exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()?
        {
            Typval::String(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        }
    }

    /// Reads a global number set by a probe script.
    fn probe_number(exec: &crate::ExExecutor, name: &str) -> Option<i64> {
        match exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()?
        {
            Typval::Number(value) => Some(*value),
            Typval::Bool(value) => Some(i64::from(*value)),
            _ => None,
        }
    }

    /// Reads one `screenpos()`-shaped dict set by a probe script.
    fn probe_screenpos(exec: &crate::ExExecutor, name: &str) -> Option<(i64, i64, i64, i64)> {
        let Typval::Dict(dict) = exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()?
        else {
            return None;
        };
        let borrowed = dict.borrow();
        let number = |key: &str| match borrowed.get(key.as_bytes()) {
            Some(Typval::Number(value)) => Some(*value),
            _ => None,
        };
        Some((
            number("row")?,
            number("col")?,
            number("endcol")?,
            number("curscol")?,
        ))
    }

    /// `win_execute` (`eval/window.c:523-543`) and `win_screenpos`
    /// (`eval/window.c:636-642`).
    ///
    /// Oracle ground truth for the same buffer text (headless `nvim -u NONE
    /// -S`): `win_execute-basic:'hi'`, `win_execute-list:'ab'`,
    /// `win_execute-silent:'s'`, `win_execute-zero:''`,
    /// `win_screenpos-cur:[1, 1]`, `win_screenpos-bad:[0, 0]`.
    #[test]
    fn win_execute_captures_and_win_screenpos_reports_origin() {
        let editor = crate::TestEditorAccess::new(editor_with_window());
        let mut exec = crate::ExExecutor::new();
        exec.execute_script(
            &editor,
            "win_execute.vim",
            r#"
let g:basic = win_execute(win_getid(), 'echo "hi"')
let g:list = win_execute(win_getid(), ['echo "a"', 'echo "b"'])
let g:silent = win_execute(win_getid(), 'echo "s"', 'silent')
let g:zero = win_execute(0, 'echo "hi"')
let g:bad = win_execute(999999, 'echo "hi"')
let g:cur = win_screenpos(0)
let g:by_id = win_screenpos(win_getid())
let g:badpos = win_screenpos(999999)
"#,
        )
        .unwrap();
        assert_eq!(probe_string(&exec, "basic").as_deref(), Some("hi"));
        assert_eq!(probe_string(&exec, "list").as_deref(), Some("ab"));
        assert_eq!(probe_string(&exec, "silent").as_deref(), Some("s"));
        assert_eq!(probe_string(&exec, "zero").as_deref(), Some(""));
        assert_eq!(probe_string(&exec, "bad").as_deref(), Some(""));
        // Lists read back through the debug form: the point is the values.
        let rendered = |name: &str| match exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
        {
            Ok(value) => format!("{value:?}"),
            Err(error) => panic!("no g:{name}: {error:?}"),
        };
        assert_eq!(rendered("cur"), "List([Number(1), Number(1)])");
        assert_eq!(rendered("by_id"), "List([Number(1), Number(1)])");
        assert_eq!(rendered("badpos"), "List([Number(0), Number(0)])");
    }

    /// `win_execute` runs in the target window: a command that records
    /// `win_getid()` observes the target, and the previous window is
    /// restored afterwards. A failing command propagates `E492`, matching
    /// the oracle (`win_execute-throw:Vim:E492`, same as `execute()`).
    #[test]
    fn win_execute_runs_in_target_window_and_restores() {
        let editor = crate::TestEditorAccess::new(editor_with_window());
        let mut exec = crate::ExExecutor::new();
        exec.execute_script(
            &editor,
            "win_execute_ctx.vim",
            r#"
split
let g:ids = [win_getid(1), win_getid(2)]
let g:before = win_getid()
call win_execute(g:ids[1], 'let g:whereami = win_getid()')
let g:after = win_getid()
let g:saw_target = g:whereami == g:ids[1]
let g:kept_current = g:after == g:before
"#,
        )
        .unwrap();
        // The command observed the second window while the session stayed
        // on the window `:split` left current.
        assert_eq!(probe_number(&exec, "saw_target"), Some(1));
        assert_eq!(probe_number(&exec, "kept_current"), Some(1));
    }

    /// `win_execute` propagates a failing command like `execute()` does: the
    /// oracle throws `E492` for both (`win_execute-throw` and
    /// `execute-throw`), and this port maps a failed inner command to the
    /// same code for both, through the shared `flow_to_eval_error`
    /// plumbing (which reports failures as `E605` port-wide).
    #[test]
    fn win_execute_propagates_command_errors() {
        let editor = crate::TestEditorAccess::new(editor_with_window());
        let mut exec = crate::ExExecutor::new();
        let code = |exec: &mut crate::ExExecutor, script: &str| {
            let error = exec
                .execute_script(&editor, "win_execute_err.vim", script)
                .unwrap_err();
            match &error {
                crate::excmd_exec::ExecError::Vim(exception) => match &exception.kind {
                    crate::VimExceptionKind::Error(code) => code.clone(),
                    crate::VimExceptionKind::Throw => "Throw".to_owned(),
                },
                other => panic!("expected Vim error, got {other:?}"),
            }
        };
        let win_code = code(
            &mut exec,
            "let g:x = win_execute(win_getid(), 'boguscommand_xyz')",
        );
        let exec_code = code(&mut exec, "let g:y = execute('boguscommand_xyz')");
        assert_eq!(win_code, exec_code, "win_execute matches execute()");
    }

    /// `screenpos` (`move.c:1227-1259`).
    ///
    /// Oracle ground truth for `['aaa', "b\tc", 'wide:界x']` (headless
    /// `nvim -u NONE -S`): `(1, 1)` answers `col/endcol/curscol 1`;
    /// the tab at `(2, 2)` answers `col 2, endcol 8, curscol 8`;
    /// the wide character's second byte at `(3, 7)` snaps to `col 6,
    /// endcol 7, curscol 6`; past-end columns clamp onto the NUL cell;
    /// an invalid window answers the empty dict; line 0 answers zeros;
    /// and a line past the buffer raises `E966`.
    #[test]
    fn screenpos_reports_character_cells() {
        let editor = crate::TestEditorAccess::new(editor_with_window());
        let mut exec = crate::ExExecutor::new();
        exec.execute_script(
            &editor,
            "screenpos.vim",
            r#"
call setline(1, ['aaa', "b\tc", 'wide:界x'])
let g:first = screenpos(win_getid(), 1, 1)
let g:tab = screenpos(win_getid(), 2, 2)
let g:tabbed = screenpos(win_getid(), 2, 3)
let g:wide = screenpos(win_getid(), 3, 7)
let g:eol = screenpos(win_getid(), 1, 4)
let g:beyond = screenpos(win_getid(), 1, 99)
let g:badwin = screenpos(999999, 1, 1)
let g:low = screenpos(win_getid(), 0, 1)
"#,
        )
        .unwrap();
        assert_eq!(probe_screenpos(&exec, "first"), Some((1, 1, 1, 1)));
        assert_eq!(probe_screenpos(&exec, "tab"), Some((2, 2, 8, 8)));
        assert_eq!(probe_screenpos(&exec, "tabbed"), Some((2, 9, 9, 9)));
        assert_eq!(probe_screenpos(&exec, "wide"), Some((3, 6, 7, 6)));
        assert_eq!(probe_screenpos(&exec, "eol"), Some((1, 4, 4, 4)));
        assert_eq!(probe_screenpos(&exec, "beyond"), Some((1, 4, 4, 4)));
        assert_eq!(probe_screenpos(&exec, "low"), Some((0, 0, 0, 0)));
        // An invalid window answers the empty dict, not zeros.
        match exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, b"badwin", 0)
        {
            Ok(Typval::Dict(dict)) => assert!(dict.borrow().entries.is_empty()),
            other => panic!("expected empty dict, got {other:?}"),
        }
        let error = exec
            .execute_line(&editor, "call screenpos(win_getid(), 99, 1)")
            .unwrap_err();
        let code = match &error {
            crate::excmd_exec::ExecError::Vim(exception) => match &exception.kind {
                crate::VimExceptionKind::Error(code) => code.clone(),
                crate::VimExceptionKind::Throw => "Throw".to_owned(),
            },
            other => panic!("expected Vim error, got {other:?}"),
        };
        assert_eq!(code, "E966");
    }
}
