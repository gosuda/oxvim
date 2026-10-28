//! Register builtins backed by the editor's existing register bank.

use crate::excmd_exec::ExEditorAccess;
use ox_eval::{EvalError, Scope, ScopeKind, builtin_spec};
use ox_types::{OxStr, Typval};

use crate::excmd_exec::{EvalHost, eval_text};
use crate::register::{RegisterContent, RegisterKind};
use crate::script::FileIO;

use super::input_string_arg;

pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    match name {
        "getreg" => getreg(host, args, scope),
        "getregtype" => getregtype(host, args, scope),
        "setreg" => setreg(host, args, scope),
        "getreginfo" => getreginfo(host, args, scope),
        // The route table admitted a name this dispatcher does not serve;
        // answer the same typed error an unknown name gets instead of
        // aborting the process.
        _ => Err(EvalError::new(
            "E117",
            0,
            format!("Unknown function: {name}"),
        )),
    }
}

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

/// The single byte `getreg_get_regname` (`eval/funcs.c:2353-2368`) reads:
/// the first byte of the name, with an empty name selecting the unnamed
/// register.
fn first_register_byte(name: &[u8]) -> char {
    name.first().map_or('"', |&byte| byte as char)
}

/// The register a query builtin reads: an explicit argument contributes its
/// first byte, and an omitted argument reads `v:register` from the current
/// scope, falling back to the unnamed register while it is unset.
fn query_register_name(args: &[Typval], scope: &Scope) -> ox_eval::Result<char> {
    match args.first() {
        Some(value) => Ok(first_register_byte(input_string_arg(value)?.as_bytes())),
        None => Ok(scope
            .get_scoped(ScopeKind::Vim, b"register", 0)
            .ok()
            .and_then(|value| input_string_arg(value).ok())
            .map_or('"', |name| first_register_byte(name.as_bytes()))),
    }
}

/// The register `setreg` writes: an empty name and `@` normalize to the
/// unnamed register, and a longer name contributes its first byte
/// (`f_setreg`, `eval/funcs.c:6461-6468`).
fn target_register_name(args: &[Typval]) -> ox_eval::Result<char> {
    let name = match args.first() {
        Some(value) => first_register_byte(input_string_arg(value)?.as_bytes()),
        None => '"',
    };
    Ok(if name == '@' { '"' } else { name })
}

/// Reads a `getreg()` truthiness argument the way `tv_get_bool_chk`
/// (`typval.c:4151-4187`) does: numbers, booleans, floats, and strings
/// convert, while containers fail with their `E74x` type error.
fn bool_arg(value: Option<&Typval>) -> ox_eval::Result<bool> {
    match value {
        None => Ok(false),
        Some(Typval::Number(number)) => Ok(*number != 0),
        Some(Typval::Bool(flag)) => Ok(*flag),
        Some(Typval::Float(number)) => Ok(*number != 0.0),
        Some(Typval::String(text)) => Ok(leading_number(text.as_bytes()) != 0),
        Some(Typval::List(_)) => Err(EvalError::new("E745", 0, "Using a List as a Number")),
        Some(Typval::Dict(_)) => Err(EvalError::new("E728", 0, "Using a Dictionary as a Number")),
        Some(_) => Err(EvalError::new("E745", 0, "Using invalid value as a Number")),
    }
}

/// Parses a leading ASCII number the way `tv_get_number` does for a
/// `tv_get_bool` string: an optional sign with digits, else zero.
fn leading_number(text: &[u8]) -> i64 {
    let mut bytes = text.iter().peekable();
    let negative = matches!(bytes.peek(), Some(b'-'));
    if matches!(bytes.peek(), Some(b'-' | b'+')) {
        bytes.next();
    }
    let mut value: i64 = 0;
    let mut found = false;
    for byte in bytes {
        let Some(digit) = byte.checked_sub(b'0').filter(|digit| *digit < 10) else {
            break;
        };
        found = true;
        value = value.saturating_mul(10).saturating_add(i64::from(digit));
    }
    if !found {
        return 0;
    }
    if negative { -value } else { value }
}

fn getreg<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let name = query_register_name(args, scope)?;
    let expression_source = bool_arg(args.get(1))?;
    let as_list = bool_arg(args.get(2))?;
    // `@` reads the unnamed register (`get_reg_contents`,
    // `register.c:2488-2491`).
    let name = if name == '@' { '"' } else { name };

    if name == '%' {
        let value = percent_reg_name(host);
        if as_list {
            return Ok(Typval::list(vec![Typval::String(value)]));
        }
        return Ok(Typval::String(value));
    }
    if name == '#' {
        let Some(value) = alternate_reg_name(host) else {
            return Ok(empty_getreg(as_list));
        };
        if as_list {
            return Ok(Typval::list(vec![Typval::String(value)]));
        }
        return Ok(Typval::String(value));
    }
    if name == '=' {
        let Some(source) = host
            .access
            .with_ex_editor(|editor| editor.registers().expression_source().map(<[u8]>::to_vec))
        else {
            return Ok(empty_getreg(as_list));
        };
        if expression_source {
            return Ok(getreg_value(vec![source], RegisterKind::CharacterWise, as_list));
        }
        let source = String::from_utf8_lossy(&source);
        let value = eval_text(host.runtime, host.access, scope, host.lua, &source)
            .map_err(|flow| EvalError::new("E15", 0, format!("{flow:?}")))?;
        let text = input_string_arg(&value)?;
        return Ok(getreg_value(
            vec![text.as_bytes().to_vec()],
            RegisterKind::CharacterWise,
            as_list,
        ));
    }

    // Invalid names read as empty (`get_reg_contents` answers NULL for
    // them, `register.c:2485-2497`); only argument type errors propagate.
    let content = host.access.with_ex_editor(|editor| {
        editor
            .registers()
            .get(name)
            .ok()
            .flatten()
            .cloned()
    });
    let Some(content) = content else {
        return Ok(empty_getreg(as_list));
    };
    if as_list {
        return Ok(Typval::list(
            content
                .getreg_lines()
                .into_iter()
                .map(|line| Typval::String(OxStr(line)))
                .collect(),
        ));
    }
    Ok(Typval::String(OxStr(content.getreg_bytes())))
}

fn empty_getreg(as_list: bool) -> Typval {
    if as_list {
        Typval::list(Vec::new())
    } else {
        Typval::String(OxStr::from(""))
    }
}

fn getreg_value(lines: Vec<Vec<u8>>, kind: RegisterKind, as_list: bool) -> Typval {
    if as_list {
        return Typval::list(
            lines
                .into_iter()
                .map(|line| Typval::String(OxStr(line)))
                .collect(),
        );
    }
    // The evaluated text is stored verbatim, like upstream's unwrapped
    // `get_reg_contents`, so no validation applies here.
    let content = RegisterContent::from_binary_lines(kind, lines);
    Typval::String(OxStr(content.getreg_bytes()))
}

/// Reads the `%` register: the current buffer's name, or empty when the
/// buffer is unnamed (`get_spec_reg`, `register.c:855-860`).
fn percent_reg_name<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
) -> OxStr {
    host.access.with_ex_editor(|editor| current_buffer_name(editor))
}

/// Reads the `#` register: staged `#` text resolved against live buffers,
/// else the window's alternate buffer, else `None` (`get_spec_reg`,
/// `register.c:862-864`).
fn alternate_reg_name<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
) -> Option<OxStr> {
    host
        .access
        .with_ex_editor(|editor| alternate_buffer_content(editor).and_then(|mut lines| lines.pop()))
        .map(OxStr)
}

/// Resolves the `#` register to its lines: staged `#` text resolved against
/// live buffers, else the window's alternate buffer. `None` when cleared or
/// missing (upstream's NULL `get_spec_reg` result).
fn alternate_buffer_content(editor: &crate::Editor) -> Option<Vec<Vec<u8>>> {
    if let Some(staged) = editor.registers().alternate_override() {
        if staged.is_empty() {
            return None;
        }
        if let Some(found) = resolve_alternate_buffer(editor, staged) {
            return editor
                .buffer(found)
                .ok()
                .map(|state| vec![state.name().0.clone()]);
        }
    }
    alternate_buffer_name(editor).map(|name| vec![name.0])
}

/// The current buffer's name, or empty when unnamed.
fn current_buffer_name(editor: &crate::Editor) -> OxStr {
    editor
        .current_buffer()
        .and_then(|buffer| editor.buffer(buffer).ok())
        .map_or_else(|| OxStr::from(""), |state| state.name().clone())
}

/// The window's alternate buffer name, or `None` when there is none.
fn alternate_buffer_name(editor: &crate::Editor) -> Option<OxStr> {
    let alternate = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .and_then(|window| window.alternate_buffer)?;
    editor
        .buffer(alternate)
        .ok()
        .map(|state| state.name().clone())
}

/// Resolves staged `#` text to a buffer: a leading number selects by buffer
/// number (`buflist_findnr`), else the first buffer whose name contains the
/// text (`buflist_findpat`). Returns `None` when nothing matches.
fn resolve_alternate_buffer(editor: &crate::Editor, text: &[u8]) -> Option<ox_types::BufHandle> {
    let digits = text
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits > 0 {
        let number: i64 = text[..digits].iter().fold(0, |value, byte| {
            value.saturating_mul(10).saturating_add(i64::from(byte - b'0'))
        });
        return ox_types::BufHandle::try_from(number)
            .ok()
            .filter(|handle| editor.buffer(*handle).is_ok());
    }
    editor.buffers().into_iter().find(|handle| {
        editor.buffer(*handle).is_ok_and(|state| {
            let name = state.name();
            !name.as_bytes().is_empty()
                && text.len() <= name.as_bytes().len()
                && name
                    .as_bytes()
                    .windows(text.len())
                    .any(|window| window == text)
        })
    })
}

fn getregtype<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    let name = query_register_name(args, scope)?;
    // `get_reg_type` (`register.c:2425-2458`): the special registers are
    // characterwise whether or not they are set; anything else invalid is
    // unknown, which formats as an empty string.
    if matches!(
        name,
        '%' | '#' | '=' | ':' | '/' | '.' | '\u{6}' | '\u{10}' | '\u{17}' | '\u{1}' | '_'
    ) {
        return Ok(string_regtype(RegisterKind::CharacterWise));
    }
    let content = host.access.with_ex_editor(|editor| {
        editor
            .registers()
            .get(name)
            .ok()
            .flatten()
            .map(RegisterContent::kind)
    });
    let Some(content) = content else {
        return Ok(Typval::String(OxStr::from("")));
    };
    Ok(string_regtype(content))
}

/// The `v`/`V`/Ctrl-V-plus-width register type rendering shared by
/// `getregtype` and `getreginfo` (`format_reg_type`).
fn string_regtype(kind: RegisterKind) -> Typval {
    let rendered = match kind {
        RegisterKind::CharacterWise => "v".to_owned(),
        RegisterKind::LineWise => "V".to_owned(),
        RegisterKind::BlockWise { width } => format!("\u{16}{width}"),
    };
    Typval::String(OxStr::from(rendered.as_str()))
}

/// `f_getreginfo` (`eval/funcs.c:4980-5032`): a Dictionary of `regcontents`,
/// `regtype`, and — for the unnamed register — `points_to`, else
/// `isunnamed`. Invalid or unset registers answer an empty Dictionary; only
/// argument type errors propagate. `=` reports the stored expression source
/// itself as one characterwise line, never its evaluated value.
fn getreginfo<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    let name = query_register_name(args, scope)?;
    let name = if name == '@' { '"' } else { name };
    let (resolved, target) = host.access.with_ex_editor(|editor| {
        let registers = editor.registers();
        // The black hole is a valid register that is always empty, not an unset
        // one (`get_spec_reg`, `register.c:912-914`).
        let resolved = match name {
            '=' => registers
                .expression_source()
                .map(|source| (vec![source.to_vec()], RegisterKind::CharacterWise)),
            '_' => Some((vec![Vec::new()], RegisterKind::CharacterWise)),
            // File registers read live buffer names; an unnamed current
            // buffer or a missing alternate answers empty, like upstream's
            // NULL `get_spec_reg` result.
            '%' => {
                let current = current_buffer_name(editor);
                (!current.as_bytes().is_empty())
                    .then(|| (vec![current.0], RegisterKind::CharacterWise))
            }
            '#' => alternate_buffer_content(editor)
                .map(|lines| (lines, RegisterKind::CharacterWise)),
            _ => registers
                .get(name)
                .ok()
                .flatten()
                .map(|content| (content.getreg_lines(), content.kind())),
        };
        (resolved, registers.unnamed_target_name())
    });
    let Some((lines, kind)) = resolved else {
        return Ok(Typval::dict(Vec::new()));
    };

    let mut entries = vec![
        (
            OxStr::from("regcontents"),
            Typval::list(
                lines
                    .into_iter()
                    .map(|line| Typval::String(OxStr(line)))
                    .collect(),
            ),
        ),
        (OxStr::from("regtype"), string_regtype(kind)),
    ];
    // `target` is the unnamed register's current pointer, read above.
    if name == '"' {
        let mut buf = [0; 4];
        let pointer: &str = target.encode_utf8(&mut buf);
        entries.push((
            OxStr::from("points_to"),
            Typval::String(OxStr::from(pointer)),
        ));
    } else {
        // Compare the requested byte with the current unnamed target exactly:
        // an `A` query reads `a`'s slot but is not the pointer itself.
        entries.push((OxStr::from("isunnamed"), Typval::Bool(name == target)));
    }
    Ok(Typval::dict(entries))
}

fn setreg<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let name = target_register_name(args)?;
    // The Dictionary form (`f_setreg`, `eval/funcs.c:6398-6444`) restores a
    // `getreginfo()` snapshot: optional `regcontents`, an optional `regtype`
    // that must consume the whole string, and — for `"` — `points_to`, else
    // `isunnamed`.
    let third = args
        .get(2)
        .map(input_string_arg)
        .transpose()?
        .map(|flags| flags.to_string_lossy().into_owned());
    if let Some(Typval::Dict(entries)) = args.get(1) {
        return setreg_dict(host, scope, name, entries, third.as_deref());
    }
    let (inferred, rows, is_list) = setreg_content(&args[1])?;
    // `write_reg_contents_lst` rejects multi-line lists for `/`, `=`, and
    // `#` (`E883`); plain strings take the single-line `write_reg_contents_ex`
    // path, which has no such check.
    if matches!(name, '/' | '=' | '#') && is_list && rows.len() > 1 {
        return Err(EvalError::new(
            "E883",
            0,
            "Search pattern and expression register may not contain two or more lines",
        ));
    }
    let (kind, append, unnamed) = parse_register_type(third.as_deref().unwrap_or(""), &rows, inferred);
    let content = RegisterContent::from_binary_lines(kind, rows);
    if name == '#' {
        setreg_alternate(host, scope, &content)?;
        return Ok(Typval::Number(0));
    }
    host.access
        .with_ex_editor(|editor| editor.registers_mut().set_from_setreg(name, content, append))
        .map_err(|error| EvalError::new("E354", 0, error.to_string()))?;
    // `setreg(..., 'u'/'"')` points the unnamed register at the written slot
    // rather than cloning the content into an independent `"` copy
    // (`op_reg_set_previous`, `register.c:298-307`).
    if unnamed {
        host.access
            .with_ex_editor(|editor| editor.registers_mut().set_unnamed_target(name));
        sync_unnamed_scope(host, scope);
    }
    sync_scope_register(host, scope, name)?;
    Ok(Typval::Number(0))
}

/// Refreshes the scope `"` cache from the unnamed register's live target, so
/// `@` reads observe a repointed unnamed register.
fn sync_unnamed_scope<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
) {
    let bytes = host.access.with_ex_editor(|editor| {
        editor
            .registers()
            .get('"')
            .ok()
            .flatten()
            .map(RegisterContent::getreg_bytes)
    });
    if let Some(bytes) = bytes {
        scope.set_register(b"\"", Typval::String(OxStr(bytes)));
    }
}

/// Mirrors a successful `setreg()` write into the scope register cache that
/// `@` reads observe. Upstream has no such cache; the sync keeps expression
/// reads coherent with the bank.
fn sync_scope_register<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    name: char,
) -> ox_eval::Result<()> {
    if name == '_' {
        // The black hole always reads empty.
        return Ok(());
    }
    if name == '#' {
        let alternate = host
            .access
            .with_ex_editor(|editor| alternate_buffer_content(editor).and_then(|mut lines| lines.pop()))
            .map_or_else(|| OxStr::from(""), OxStr);
        scope.set_register(b"#", Typval::String(alternate));
        return Ok(());
    }
    if name == '=' {
        let source = host.access.with_ex_editor(|editor| {
            editor
                .registers()
                .expression_source()
                .map(<[u8]>::to_vec)
                .unwrap_or_default()
        });
        scope.set_register(b"=", Typval::String(OxStr(source)));
        return Ok(());
    }
    let stored_name = name.to_ascii_lowercase();
    if stored_name.is_ascii_alphabetic() || matches!(stored_name, '0'..='9' | '"' | '-' | '/' | '*' | '+') {
        if let Some(stored_bytes) = host.access.with_ex_editor(|editor| {
            editor
                .registers()
                .get(stored_name)
                .map_err(|error| EvalError::new("E354", 0, error.to_string()))
                .map(|content| content.map(RegisterContent::getreg_bytes))
        })? {
            let byte = u8::try_from(stored_name)
                .map_err(|_| EvalError::new("E354", 0, format!("invalid register name {stored_name:?}")))?;
            scope.set_register(&[byte], Typval::String(OxStr(stored_bytes)));
        }
    }
    Ok(())
}

/// Writes staged `#` text: empty text clears the alternate file, a leading
/// number selects by buffer number (`E86` when missing), else the first
/// buffer whose name contains the text (`buflist_findpat`). An unmatched
/// pattern leaves the alternate unchanged, like upstream's silent return.
fn setreg_alternate<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    content: &RegisterContent,
) -> ox_eval::Result<()> {
    let text = content.to_bytes();
    if text.is_empty() {
        host.access.with_ex_editor(|editor| {
            editor.registers_mut().set_alternate_override(None);
            if let Some(window) = editor.current_window() {
                if let Ok(state) = editor.window_mut(window) {
                    state.alternate_buffer = None;
                }
            }
        });
        scope.set_register(b"#", Typval::String(OxStr::from("")));
        return Ok(());
    }
    let digits = text
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let found = if digits > 0 {
        let number: i64 = text[..digits].iter().fold(0, |value, byte| {
            value.saturating_mul(10).saturating_add(i64::from(byte - b'0'))
        });
        let found = host.access.with_ex_editor(|editor| {
            ox_types::BufHandle::try_from(number)
                .ok()
                .filter(|handle| editor.buffer(*handle).is_ok())
        });
        let Some(found) = found else {
            return Err(EvalError::new(
                "E86",
                0,
                format!("Buffer {number} does not exist"),
            ));
        };
        Some(found)
    } else {
        host.access
            .with_ex_editor(|editor| resolve_alternate_buffer(editor, &text))
    };
    if let Some(found) = found {
        host.access.with_ex_editor(|editor| {
            editor.registers_mut().set_alternate_override(None);
            if let Some(window) = editor.current_window() {
                if let Ok(state) = editor.window_mut(window) {
                    state.alternate_buffer = Some(found);
                }
            }
        });
        sync_scope_register(host, scope, '#')?;
    }
    Ok(())
}

/// Reads the Dictionary form of `setreg()` (`f_setreg`,
/// `eval/funcs.c:6398-6444`). An empty Dictionary clears the register; a
/// `regtype` entry must consume its whole value (`E475` otherwise) and
/// forbids a third argument (`E118`). A missing `regcontents` entry writes
/// nothing and only moves the unnamed pointer.
fn setreg_dict<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    name: char,
    entries: &ox_types::DictRef,
    has_third: Option<&str>,
) -> ox_eval::Result<Typval> {
    let entries = entries
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    if entries.entries.is_empty() {
        drop(entries);
        if name == '#' {
            host.access.with_ex_editor(|editor| {
                editor.registers_mut().set_alternate_override(None);
                if let Some(window) = editor.current_window() {
                    if let Ok(state) = editor.window_mut(window) {
                        state.alternate_buffer = None;
                    }
                }
            });
        } else {
            host.access
                .with_ex_editor(|editor| editor.registers_mut().clear_for_setreg(name))
                .map_err(|error| EvalError::new("E354", 0, error.to_string()))?;
        }
        sync_scope_register(host, scope, name)?;
        return Ok(Typval::Number(0));
    }
    let contents = entries
        .get(b"regcontents")
        .map(setreg_content_value)
        .transpose()?;
    // A `regtype` entry pins the shape; a non-string entry is ignored, like
    // upstream's NULL `tv_dict_get_string` result. A pinned shape forbids
    // any third argument (`E118`).
    let dict_kind = match entries.get(b"regtype") {
        Some(Typval::String(regtype)) => {
            if has_third.is_some() {
                return Err(EvalError::new(
                    "E118",
                    0,
                    "Too many arguments for function: setreg".to_owned(),
                ));
            }
            let rows = contents.as_ref().map_or(&[][..], |(_, rows, _)| rows);
            Some(parse_dict_regtype(&regtype.to_string_lossy(), rows)?)
        }
        _ => None,
    };
    // For `"`, `points_to` redirects the write at another register; anywhere
    // else a true `isunnamed` points the unnamed register at the written
    // slot after the write.
    let mut effective = name;
    let mut pointreg = None;
    if name == '"' {
        if let Some(Typval::String(target)) = entries.get(b"points_to") {
            // A non-string `points_to` is ignored like a missing one.
            pointreg = Some(first_register_byte(target.as_bytes()));
            effective = pointreg.unwrap_or('"');
        }
    } else if dict_points_unnamed(entries.get(b"isunnamed")) {
        pointreg = Some(name);
    }
    drop(entries);
    if let Some((inferred, rows, is_list)) = contents {
        if matches!(effective, '/' | '=' | '#') && is_list && rows.len() > 1 {
            return Err(EvalError::new(
                "E883",
                0,
                "Search pattern and expression register may not contain two or more lines",
            ));
        }
        // Without a pinned `regtype`, the third argument still contributes
        // its shape, append, and unnamed flags.
        let third_kind;
        let (append, unnamed) = match has_third {
            Some(flags) => {
                let (kind, append, unnamed) =
                    parse_register_type(flags, &rows, RegisterKind::CharacterWise);
                third_kind = Some(kind);
                (append, unnamed)
            }
            None => {
                third_kind = None;
                (false, false)
            }
        };
        let kind = dict_kind.or(third_kind).unwrap_or(inferred);
        let content = RegisterContent::from_binary_lines(kind, rows);
        if effective == '#' {
            setreg_alternate(host, scope, &content)?;
            return Ok(Typval::Number(0));
        }
        host.access
            .with_ex_editor(|editor| {
                editor
                    .registers_mut()
                    .set_from_setreg(effective, content, append)
            })
            .map_err(|error| EvalError::new("E354", 0, error.to_string()))?;
        if unnamed {
            host.access.with_ex_editor(|editor| {
                editor.registers_mut().set_unnamed_target(effective);
            });
        }
    }
    if let Some(pointed) = pointreg {
        host.access.with_ex_editor(|editor| {
            editor.registers_mut().set_unnamed_target(pointed);
        });
        sync_unnamed_scope(host, scope);
    }
    sync_scope_register(host, scope, effective)?;
    Ok(Typval::Number(0))
}

/// Parses the `setreg()` third argument the way `f_setreg` loops over it
/// (`eval/funcs.c:6448-6458`): `a` appends, `u`/`"` points the unnamed
/// register at the write, and each type letter reseeds the shape, with the
/// last one winning. A `b` consumes following ASCII digits as the block
/// width, defaulting to the widest row in display cells.
fn parse_register_type(
    flags: &str,
    lines: &[Vec<u8>],
    inferred: RegisterKind,
) -> (RegisterKind, bool, bool) {
    let mut kind = None;
    let mut append = false;
    let mut unnamed = false;
    let bytes = flags.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'a' | b'A' => append = true,
            b'u' | b'"' => unnamed = true,
            b'v' | b'c' => kind = Some(RegisterKind::CharacterWise),
            b'V' | b'l' => kind = Some(RegisterKind::LineWise),
            b'b' | 0x16 => {
                let start = index + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                // A missing or zero width selects the widest row, like
                // upstream's `blocklen == -1` auto path.
                let parsed: usize = std::str::from_utf8(&bytes[start..end])
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let width = if parsed == 0 {
                    lines
                        .iter()
                        .map(|line| crate::register::RegisterContent::display_cells(line))
                        .max()
                        .unwrap_or(0)
                        .max(1)
                } else {
                    parsed
                };
                kind = Some(RegisterKind::BlockWise { width });
                index = end.saturating_sub(1);
            }
            _ => {}
        }
        index += 1;
    }
    (kind.unwrap_or(inferred), append, unnamed)
}

/// Parses a Dictionary `regtype` value the way `get_yank_type`
/// (`eval/funcs.c:6339-6366`) does: one shape letter, an optional digit run
/// for blocks, and nothing else (`E475` otherwise). A missing or zero block
/// width selects the widest row, like upstream's `blocklen == -1` auto path.
fn parse_dict_regtype(regtype: &str, lines: &[Vec<u8>]) -> ox_eval::Result<RegisterKind> {
    let invalid = || EvalError::new("E475", 0, "Invalid argument: value");
    let bytes = regtype.as_bytes();
    let first = bytes.first().ok_or_else(invalid)?;
    let kind = match first {
        b'v' | b'c' => RegisterKind::CharacterWise,
        b'V' | b'l' => RegisterKind::LineWise,
        b'b' | 0x16 => {
            let digits: Vec<u8> = bytes[1..]
                .iter()
                .copied()
                .take_while(u8::is_ascii_digit)
                .collect();
            if digits.len() != bytes.len() - 1 {
                return Err(invalid());
            }
            let parsed: usize = std::str::from_utf8(&digits)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let width = if parsed == 0 {
                lines
                    .iter()
                    .map(|line| crate::register::RegisterContent::display_cells(line))
                    .max()
                    .unwrap_or(0)
                    .max(1)
            } else {
                parsed
            };
            RegisterKind::BlockWise { width }
        }
        _ => return Err(invalid()),
    };
    if !matches!(kind, RegisterKind::BlockWise { .. }) && bytes.len() != 1 {
        return Err(invalid());
    }
    Ok(kind)
}

/// Builds `setreg()` content without validating it, mirroring `str_to_reg`:
/// list items stay whole (embedded line feeds render as `^@`) and infer
/// linewise; strings split at line feeds and infer the shape from a trailing
/// line feed or carriage return. Returns the inferred shape, the rows, and
/// whether the value was a list (only lists can trip `E883`).
fn setreg_content(value: &Typval) -> ox_eval::Result<(RegisterKind, Vec<Vec<u8>>, bool)> {
    if let Typval::List(values) = value {
        let values = values
            .try_borrow()
            .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
        let lines = values
            .items
            .iter()
            .map(input_string_arg)
            .collect::<ox_eval::Result<Vec<_>>>()?
            .into_iter()
            .map(|line| line.as_bytes().to_vec())
            .collect();
        return Ok((RegisterKind::LineWise, lines, true));
    }
    let text = input_string_arg(value)?;
    let bytes = text.as_bytes();
    let linewise = bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'));
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    let kind = if linewise {
        RegisterKind::LineWise
    } else {
        RegisterKind::CharacterWise
    };
    Ok((kind, lines, false))
}

/// Reads a Dictionary `regcontents` entry, which is a string or a list of
/// strings like the plain second argument.
fn setreg_content_value(value: &Typval) -> ox_eval::Result<(RegisterKind, Vec<Vec<u8>>, bool)> {
    setreg_content(value)
}

/// Reads a Dictionary `isunnamed` entry the way `tv_dict_get_bool` does for
/// `f_setreg`: booleans, numbers, and numeric strings convert, anything
/// else counts as false.
fn dict_points_unnamed(value: Option<&Typval>) -> bool {
    match value {
        Some(Typval::Bool(flag)) => *flag,
        Some(Typval::Number(number)) => *number != 0,
        Some(Typval::String(text)) => leading_number(text.as_bytes()) != 0,
        _ => false,
    }
}
