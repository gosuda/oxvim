//! Expression- and script-evaluating builtins: they re-enter the parser, the
//! Ex interpreter, the Lua host, or the typeahead queue (upstream `eval.c`,
//! `userfunc.c`).

use crate::ModeMachine;
use crate::autocmd::Event;
use crate::excmd_exec::ExEditorAccess;
use crate::script::{FileIO, LogicalLine};
use crate::typeahead::Keys;
use ox_eval::BuiltinHost;
use ox_eval::ClosureRegistry;
use ox_eval::EvalError;
use ox_eval::Evaluator;
use ox_eval::Parser as ExprParser;
use ox_eval::Scope;
use ox_eval::builtin_spec;
use ox_eval::builtins::string_arg;
use ox_eval::closure_index;
use ox_eval::exists as exists_in_scope;
use ox_excmd::{ResolveError, ResolvedCommand, resolve_command};
use ox_types::{Funcref, Object, OxStr, Special, Typval};
use std::rc::Rc;

use super::input_string_arg;
use crate::excmd_exec::{
    EvalHost, ExRuntime, Flow, LuaExec, LuaExecError, VimRegex, drain_typeahead, exec_error_flow,
    expand_env_esc, flow_to_eval_error, parse_program, run_program, sync_editor_into_scope,
    sync_scope_into_editor, typval_number, typval_to_text,
};

/// Routes one expression- or script-evaluating builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match name {
        "eval" => call_eval_builtin(host, args, scope),
        "call" => call_call_builtin(host, args, scope),
        "uniq" => call_uniq_builtin(host, args, scope),
        "execute" => call_execute_builtin(host.runtime, host.access, scope, host.lua, args),
        "exists" => exists_with_editor(host.runtime, host.access, scope, args),
        "expand" => call_expand_builtin(host.runtime, host.access, args),
        "feedkeys" => call_feedkeys_builtin(host.runtime, host.access, scope, host.lua, args),
        "fullcommand" => Ok(call_fullcommand_builtin(host.runtime, host.access, args)),
        "function" | "funcref" => {
            let registry = BuiltinHost::closure_registry(host);
            call_function_builtin(host.runtime, name, registry, args)
        }
        "luaeval" => call_luaeval_builtin(host.runtime, host.access, scope, host.lua, args),
        "submatch" => Ok(call_submatch_builtin(host.submatches.as_deref(), args)),
        _ => unreachable!("eval builtin route and dispatcher disagree"),
    }
}

/// `eval()`: parse the argument and evaluate it against this same host, so the
/// editor seams stay reachable from the nested expression (`f_eval`).
/// `uniq({list} [, {func} [, {dict}]])` (`eval/list.c` `f_uniq`).
///
/// Without a comparator adjacent items are removed when they are equal; with
/// one, the comparator decides. The comparator runs with no borrow on the list
/// held, because it may reenter the editor, and a comparator that changes the
/// list is refused the way upstream refuses it.
fn call_uniq_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let container = args[0].clone();
    let Typval::List(reference) = &container else {
        return Err(EvalError::new(
            "E686",
            0,
            "Argument of uniq() must be a List",
        ));
    };
    let comparator = match args.get(1) {
        None | Some(Typval::Special(Special::Null)) => None,
        Some(value) => Some(value.clone()),
    };
    let receiver = match args.get(2) {
        None | Some(Typval::Special(Special::Null)) => None,
        Some(value @ Typval::Dict(_)) => Some(value.clone()),
        Some(_) => return Err(EvalError::new("E731", 0, "Using a List as a Dict")),
    };
    let snapshot = reference
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
        .items
        .clone();
    let mut removals = Vec::new();
    // Both paths compare the last kept item with the next original one, which
    // is what upstream does; comparing snapshot[index - 1] instead keeps an
    // item that the last kept item would have swallowed.
    let mut kept = 0_usize;
    for index in 1..snapshot.len() {
        let duplicate = match &comparator {
            None => snapshot[kept] == snapshot[index],
            Some(comparator) => {
                let callable = match (&receiver, comparator) {
                    (Some(receiver), Typval::String(name)) => {
                        let function = resolve_function_reference(
                            host.runtime,
                            None,
                            &Typval::String(name.clone()),
                        )?;
                        let function =
                            bind_function_arguments(function, std::slice::from_ref(receiver))?;
                        Typval::Partial(function)
                    }
                    (Some(receiver), Typval::Funcref(funcref) | Typval::Partial(funcref)) => {
                        let function = bind_function_arguments(
                            funcref.clone(),
                            std::slice::from_ref(receiver),
                        )?;
                        Typval::Partial(function)
                    }
                    (None, comparator) => comparator.clone(),
                    (Some(_), _) => return Err(EvalError::new("E1085", 0, "not a callable value")),
                };
                let value = Evaluator::new(host, &VimRegex).invoke(
                    callable,
                    vec![snapshot[kept].clone(), snapshot[index].clone()],
                    scope,
                )?;
                // The item is dropped when the comparator answers false:
                // `uniq([1,2,3], {a,b -> a < b})` keeps every item while
                // `{a,b -> a > b}` keeps only the first, and both follow from
                // that rule.
                match value {
                    Typval::Number(number) => number == 0,
                    Typval::Float(number) => number == 0.0,
                    Typval::Bool(value) => !value,
                    _ => return Err(EvalError::new("E474", 0, "Invalid argument")),
                }
            }
        };
        if duplicate {
            removals.push(index);
        } else {
            kept = index;
        }
    }
    let mut data = reference
        .try_borrow_mut()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    if data.items.len() != snapshot.len() {
        return Err(EvalError::new("E741", 0, "Cannot change list"));
    }
    for index in removals.into_iter().rev() {
        data.items.remove(index);
    }
    drop(data);
    Ok(container)
}

/// `call({func}, {arglist} [, {dict}])` (`eval/funcs.c` `f_call`).
///
/// The callee may be a Funcref or Partial, which goes through the evaluator's
/// own invocation path, or a function name, which goes through the host so a
/// user function and a builtin both resolve. A third argument binds `self`
/// for the duration of the call, exactly as a dictionary method call does.
fn call_call_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let callee = args[0].clone();
    let Typval::List(reference) = &args[1] else {
        return Err(EvalError::new("E714", 0, "List required"));
    };
    let call_args = reference
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
        .items
        .clone();
    // A third argument binds `self`: upstream turns the call into
    // `function({func}, {dict})` and runs that partial, which is the only
    // path whose frame the callee reads `self` from.
    let callable = match args.get(2) {
        None | Some(Typval::Special(Special::Null)) => callee,
        Some(value @ Typval::Dict(_)) => match &callee {
            Typval::String(_) => {
                let function = resolve_function_reference(host.runtime, None, &callee)?;
                let function = bind_function_arguments(function, std::slice::from_ref(value))?;
                Typval::Partial(function)
            }
            Typval::Funcref(funcref) | Typval::Partial(funcref) => {
                let function =
                    bind_function_arguments(funcref.clone(), std::slice::from_ref(value))?;
                Typval::Partial(function)
            }
            _ => return Err(EvalError::new("E1085", 0, "not a callable value")),
        },
        Some(_) => return Err(EvalError::new("E731", 0, "Using a List as a Dict")),
    };
    match callable {
        Typval::Funcref(_) | Typval::Partial(_) => {
            Evaluator::new(host, &VimRegex).invoke(callable, call_args, scope)
        }
        Typval::String(name) => {
            let name = OxStr(name.as_bytes().into());
            host.call(&name, call_args, scope)
        }
        _ => Err(EvalError::new("E1085", 0, "not a callable value")),
    }
}

fn call_eval_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let source = match args {
        [source] => string_arg(source)?,
        [] => {
            return Err(EvalError::new(
                "E119",
                0,
                "Not enough arguments for function: eval",
            ));
        }
        _ => {
            return Err(EvalError::new(
                "E118",
                0,
                "Too many arguments for function: eval",
            ));
        }
    };
    let expression = ExprParser::new(source.as_bytes()).parse()?;
    let regex = VimRegex;
    Evaluator::new(host, &regex).eval(&expression, scope)
}

/// `submatch()`: the groups captured by the `:substitute` whose replacement
/// expression is running; outside one every index reads empty (`f_submatch`).
fn call_submatch_builtin(submatches: Option<&[String]>, args: &[Typval]) -> Typval {
    let index = usize::try_from(args.first().and_then(typval_number).unwrap_or(0).max(0))
        .unwrap_or(usize::MAX);
    let value = submatches
        .and_then(|groups| groups.get(index))
        .cloned()
        .unwrap_or_default();
    Typval::String(OxStr(value.into_bytes()))
}

fn call_execute_builtin<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<dyn LuaExec>>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: execute",
        ));
    }
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: execute",
        ));
    }
    // A List argument is not stringified: `execute_common` (`eval/funcs.c`
    // 1206-1216) hands `do_cmdline` a `get_list_line` cookie, so every item is
    // its own source line and multi-line constructs such as `:if`/`:endif`
    // work. Only the non-list form goes through `do_cmdline_cmd` as one line.
    let logical = match &args[0] {
        Typval::List(items) => {
            let text = items
                .borrow()
                .items
                .iter()
                .map(typval_to_text)
                .collect::<Vec<String>>()
                .join("\n");
            runtime
                .scripts
                .join_logical_lines(&text)
                .map_err(|error| EvalError::new("E488", 0, error.to_string()))?
        }
        command => vec![LogicalLine {
            text: typval_to_text(command),
            first_line: runtime.scripts.current_line(),
        }],
    };
    let program = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    let message_start = access.with_ex_editor(|editor| editor.messages().len());
    let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
    if !matches!(flow, Flow::Normal) {
        return Err(flow_to_eval_error(flow, "execute"));
    }
    let mut output = String::new();
    let messages = access.with_ex_editor(|editor| editor.messages().to_vec());
    for message in &messages[message_start..] {
        let Object::String(text) = &message.content else {
            continue;
        };
        if message.leading_newline || !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&text.to_string_lossy());
    }

    access.with_ex_editor(|editor| editor.truncate_messages(message_start));
    Ok(Typval::String(OxStr(output.into_bytes())))
}

/// `luaeval({expr}[, {arg}])`: eval/funcs.c `f_luaeval` → lua/executor.c
/// `nlua_call_luaeval`. The host compiles `local _A=select(1,...) return
/// (<expr>)` and converts the argument and result with typval semantics.
/// Errors surface as E5107 (load) / E5108 (runtime) with the upstream
/// `Lua:` message prefix.
fn call_luaeval_builtin<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<dyn LuaExec>>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let Some(lua) = lua else {
        return Err(EvalError::not_implemented(OxStr::from("luaeval")));
    };
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: luaeval",
        ));
    }
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: luaeval",
        ));
    }
    // f_luaeval reads the expression through tv_get_string_chk.
    let expression = match &args[0] {
        Typval::String(value) => value.to_string_lossy().into_owned(),
        Typval::Number(value) => value.to_string(),
        Typval::Bool(value) => OxStr::from(if *value { "v:true" } else { "v:false" })
            .to_string_lossy()
            .into_owned(),
        Typval::Special(Special::Null) => "v:null".to_owned(),
        // `tv_get_string_buf_chk` (`typval.c:4684-4685`) renders a Float with
        // `%g`; E806 belongs only to `check_can_index` (`eval.c:3225-3229`).
        Typval::Float(number) => ox_eval::float_as_string(*number)
            .to_string_lossy()
            .into_owned(),
        Typval::List(_) => {
            return Err(EvalError::new("E730", 0, "Using a List as a String"));
        }
        Typval::Dict(_) => {
            return Err(EvalError::new("E731", 0, "Using a Dictionary as a String"));
        }
        _ => return Err(EvalError::new("E729", 0, "Using invalid value as a String")),
    };
    // The Lua host reads and writes editor variables (`vim.g` inside the
    // expression), so live Ex variables are synchronized in and back out
    // exactly like the `:lua` command path.
    if let Err(error) = access.with_ex_editor(|editor| sync_scope_into_editor(editor, scope)) {
        return Err(flow_to_eval_error(
            exec_error_flow(runtime, error),
            "luaeval",
        ));
    }
    let result = lua.eval_expression(&expression, args.get(1));
    let sync = access.with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
    match (result, sync) {
        (Err(LuaExecError::Load(message)), _) => {
            Err(EvalError::new("E5107", 0, format!("Lua: {message}")))
        }
        (Err(LuaExecError::Runtime(message) | LuaExecError::Conversion(message)), _) => {
            Err(EvalError::new("E5108", 0, format!("Lua: {message}")))
        }
        (Ok(_), Err(error)) => Err(flow_to_eval_error(
            exec_error_flow(runtime, error),
            "luaeval",
        )),
        (Ok(value), Ok(())) => Ok(value),
    }
}

/// Leading cmdline `sp_token` table for `expand()` (`find_cmdline_var`,
/// `ex_docmd.c:7488`), limited to the tokens this port resolves. Ordered
/// longest-first so a later `<argname>`-style addition cannot be shadowed.
const EXPAND_SPECIAL_TOKENS: &[&str] = &["<amatch>", "<afile>", "<abuf>", "<SID>", "%"];

/// The `eval_vars` (`ex_docmd.c:7551`) bases behind `expand()`'s special
/// tokens. A token this port does not know yields an empty base.
fn expand_special_base<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    token: &str,
) -> OxStr {
    match token {
        "%" => access.with_ex_editor(|editor| {
            editor
                .current_buffer()
                .and_then(|buffer| editor.buffer(buffer).ok())
                .map_or_else(
                    || OxStr(Vec::new()),
                    |buffer| {
                        let name = buffer.name().to_string_lossy();
                        OxStr::from(name.as_ref())
                    },
                )
        }),
        "<SID>" => runtime.scripts.current_sid().map_or_else(
            || OxStr(Vec::new()),
            |sid| {
                let name = format!("<SNR>{sid}_");
                OxStr::from(name.as_str())
            },
        ),
        "<amatch>" => runtime.active_autocmd.matched.clone(),
        "<afile>" => runtime.active_autocmd.file.clone(),
        "<abuf>" => runtime.active_autocmd.buffer.map_or_else(
            || OxStr(Vec::new()),
            |buffer| {
                let number = i64::from(buffer).to_string();
                OxStr::from(number.as_str())
            },
        ),
        _ => OxStr(Vec::new()),
    }
}

/// `expand()` (`f_expand`).
///
/// `%`, `#` and a `<...>` keyword go to `eval_vars`; this port resolves
/// `%`, `<SID>`, `<amatch>`, `<afile>` and `<abuf>` as leading tokens with
/// an optional trailing `:`-modifier chain applied through
/// `ox_eval::apply_filename_modifiers` (`modify_fname` parity), so
/// `expand('%:p')` is the absolute current-buffer name. Anything else is a
/// file pattern handed to `ExpandOne`, which resolves `~` and `$NAME`
/// through `expand_env_esc` before matching. Returning such a pattern
/// verbatim leaves `expand('~')` as the literal `~`, and callers that hand
/// the result to a shell -- `runtest.vim`'s `system('rm -rf  ' .. file)` --
/// then let the *shell* expand it against its own environment.
///
/// Named gap: the wildcard half of `ExpandOne` is not here, so a pattern
/// with `*` or `?` still comes back as itself; `glob()` is where this port
/// matches files. `#` and `<cword>`/`<sfile>`-style tokens are likewise
/// not recognized and pass through verbatim, and the `%<` extension-strip
/// form is not implemented.
fn call_expand_builtin<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let [Typval::String(value), ..] = args else {
        return Err(EvalError::new("E730", 0, "Using a List as a String"));
    };
    let text = value.to_string_lossy();
    let text: &str = text.as_ref();
    let special = EXPAND_SPECIAL_TOKENS
        .iter()
        .find_map(|token| text.strip_prefix(token).map(|rest| (*token, rest)));
    let expanded = match special {
        // Exact token or token-plus-`:`-modifier chain (`eval_vars` +
        // `modify_fname`).
        Some((token, rest)) if rest.is_empty() || rest.starts_with(':') => {
            let base = expand_special_base(runtime, access, token);
            // `expand()` on an unnamed source yields "" (upstream f_expand:
            // eval_vars marks the result invalid and f_expand returns "").
            if rest.is_empty() || base.as_bytes().is_empty() {
                base
            } else {
                let base_text = base.to_string_lossy();
                let modified = ox_eval::apply_filename_modifiers(
                    Some(&VimRegex),
                    base_text.as_ref(),
                    rest.as_bytes(),
                )?;
                OxStr::from(modified.as_str())
            }
        }
        _ => {
            let expanded = expand_env_esc(text);
            OxStr::from(expanded.as_str())
        }
    };
    Ok(Typval::String(expanded))
}

fn resolve_function_reference<F: FileIO>(
    runtime: &ExRuntime<F>,
    registry: Option<ClosureRegistry>,
    value: &Typval,
) -> ox_eval::Result<Funcref> {
    match value {
        Typval::Funcref(function) | Typval::Partial(function) => Ok(function.clone()),
        Typval::String(name) => {
            let text = name.to_string_lossy();
            if text.is_empty() {
                return Err(EvalError::new("E129", 0, "Function name required"));
            }
            if text.contains('(') || text.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                return Err(EvalError::new(
                    "E475",
                    0,
                    format!("Invalid argument: {text}"),
                ));
            }

            let mut lambda_registry = None;
            if let Some(index) = closure_index(name.as_bytes()) {
                match registry {
                    Some(registry) if registry.contains_closure(index) => {
                        lambda_registry = Some(registry.registry_id());
                    }
                    _ => {
                        return Err(EvalError::new(
                            "E700",
                            0,
                            format!("Unknown function: {text}"),
                        ));
                    }
                }
            } else {
                // `common_function` validates the *translated* name
                // (`funcs.c:1573-1577` → `translated_function_exists`), and
                // `save_function_name` strips a leading `g:` for a plain
                // string (`eval/userfunc.c:2117`). A script-local `s:` (or
                // `<SID>`) is expanded to its `<SNR>{sid}_` spelling so the
                // reference stays callable from other scripts
                // (`get_scriptlocal_funcname`, `userfunc.c:1559`); anything
                // else keeps the spelling the caller wrote, as upstream
                // stores `s`, not `trans_name`: `function('g:Fn')` prints
                // `function('g:Fn')`.
                let lookup = text.strip_prefix("g:").unwrap_or(&text);
                let sid = runtime.scripts.current_sid().unwrap_or(0);
                let known = builtin_spec(lookup).is_some()
                    || runtime.functions.contains(lookup, sid)
                    || lookup.contains('#')
                    || crate::builtins::route(lookup).is_some();
                if !known {
                    return Err(EvalError::new(
                        "E700",
                        0,
                        format!("Unknown function: {text}"),
                    ));
                }
                let stored = if sid == 0 {
                    name.clone()
                } else if let Some(local) = text.strip_prefix("s:") {
                    OxStr::from(format!("<SNR>{sid}_{local}").as_str())
                } else if let Some(local) = text.strip_prefix("<SID>") {
                    OxStr::from(format!("<SNR>{sid}_{local}").as_str())
                } else {
                    name.clone()
                };
                return Ok(Funcref {
                    name: stored,
                    args: Vec::new(),
                    dict: None,
                    registry: lambda_registry,
                });
            }
            Ok(Funcref {
                name: name.clone(),
                args: Vec::new(),
                dict: None,
                registry: lambda_registry,
            })
        }
        other => {
            let name = input_string_arg(other)?;
            Err(EvalError::new(
                "E475",
                0,
                format!("Invalid argument: {}", name.to_string_lossy()),
            ))
        }
    }
}

fn bind_function_arguments(mut function: Funcref, args: &[Typval]) -> ox_eval::Result<Funcref> {
    let mut bound = None;
    let mut dictionary = None;
    if let Some(second) = args.first() {
        match second {
            Typval::List(reference) => {
                bound = Some(
                    reference
                        .try_borrow()
                        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
                        .items
                        .clone(),
                );
            }
            Typval::Dict(reference) if args.len() == 1 => {
                dictionary = Some(
                    reference
                        .try_borrow()
                        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
                        .entries
                        .iter()
                        .map(|entry| (entry.key.clone(), entry.value.clone()))
                        .collect(),
                );
            }
            _ => {
                return Err(EvalError::new(
                    "E923",
                    0,
                    "Second argument of function() must be a list or a dict",
                ));
            }
        }
    }
    if let Some(third) = args.get(1) {
        let Typval::Dict(reference) = third else {
            return Err(EvalError::new("E922", 0, "Expected a dict"));
        };
        dictionary = Some(
            reference
                .try_borrow()
                .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
                .entries
                .iter()
                .map(|entry| (entry.key.clone(), entry.value.clone()))
                .collect(),
        );
    }
    if let Some(mut values) = bound {
        function.args.append(&mut values);
    }
    if dictionary.is_some() {
        function.dict = dictionary;
    }
    Ok(function)
}

fn call_function_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    kind: &str,
    registry: Option<ClosureRegistry>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 3 {
        return Err(EvalError::new(
            if args.is_empty() { "E119" } else { "E118" },
            0,
            format!("Invalid arguments for {kind}"),
        ));
    }
    let function = resolve_function_reference(runtime, registry, &args[0])?;
    let function = bind_function_arguments(function, &args[1..])?;
    let partial = kind == "funcref" || !function.args.is_empty() || function.dict.is_some();
    Ok(if partial {
        Typval::Partial(function)
    } else {
        Typval::Funcref(function)
    })
}

fn call_fullcommand_builtin<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    args: &[Typval],
) -> Typval {
    let Some(Typval::String(command)) = args.first() else {
        return Typval::String(OxStr(Vec::new()));
    };
    let command = command.to_string_lossy();
    let resolved = {
        let registry = runtime.user_commands.borrow();
        let provider = crate::excmd_exec::UserCommandLookup {
            registry: &registry,
            buffer: access.with_ex_editor(|editor| editor.current_buffer()),
        };
        resolve_command(&command, &provider)
            .ok()
            .map_or_else(String::new, |command| command.name().to_owned())
    };
    Typval::String(OxStr::from(resolved.as_str()))
}

fn call_feedkeys_builtin<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<dyn LuaExec>>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::new(
            if args.is_empty() { "E119" } else { "E118" },
            0,
            "Invalid arguments for feedkeys",
        ));
    }
    let keys = input_string_arg(&args[0])?;
    let mode = args
        .get(1)
        .map(input_string_arg)
        .transpose()?
        .unwrap_or_else(|| OxStr::from("m"));
    let execute = access.with_ex_editor(|editor| {
        editor
            .typeahead_mut()
            .feedkeys(&Keys::escape_ks(keys.as_bytes()), &mode.to_string_lossy())
            .map_err(|error| EvalError::new("E475", 0, error.to_string()))
    })?;
    if execute {
        let machine = std::rc::Rc::new(std::cell::RefCell::new(ModeMachine::default()));
        let flow = drain_typeahead(runtime, access, scope, lua, &machine, usize::MAX);
        if !matches!(flow, Flow::Normal) {
            return Err(flow_to_eval_error(flow, "feedkeys"));
        }
    }
    Ok(Typval::Number(0))
}

fn exists_with_editor<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    scope: &Scope,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let value = args
        .first()
        .cloned()
        .unwrap_or(Typval::String(OxStr::from("")));
    let operand = typval_to_text(&value);
    let result = if let Some(option) = operand
        .strip_prefix('&')
        .or_else(|| operand.strip_prefix('+'))
    {
        let option = option
            .strip_prefix("g:")
            .or_else(|| option.strip_prefix("l:"))
            .unwrap_or(option);
        i64::from(crate::options::OptionStore::metadata(option).is_ok())
    } else if let Some(name) = operand.strip_prefix('*') {
        let sid = runtime.scripts.current_sid().unwrap_or(0);
        i64::from(is_callable_function(name) || runtime.functions.contains(name, sid))
    } else if let Some(name) = operand.strip_prefix(':') {
        let registry = runtime.user_commands.borrow();
        let provider = crate::excmd_exec::UserCommandLookup {
            registry: &registry,
            buffer: access.with_ex_editor(|editor| editor.current_buffer()),
        };
        match resolve_command(name, &provider) {
            Ok(ResolvedCommand::Builtin(spec)) if !is_executed_command(spec.name) => 0,
            Ok(command) => {
                if command.name() == name {
                    2
                } else {
                    1
                }
            }
            Err(ResolveError::AmbiguousUserCommand) => 3,
            Err(ResolveError::NotFound) => 0,
        }
    } else if let Some(event) = operand.strip_prefix("##") {
        i64::from(Event::from_name(event).is_some())
    } else if let Some(query) = operand.strip_prefix('#') {
        i64::from(access.with_ex_editor(|editor| editor.autocmds().exists(query)))
    } else {
        return exists_in_scope(&value, scope);
    };
    Ok(Typval::Number(result))
}

/// Whether a builtin function name can actually be *called* here.
///
/// `f_exists`'s `*` form asks `function_exists`, not the metadata table, so the
/// answer is the union of the two dispatchers that serve builtins: the
/// typval-only one in `ox-eval` ([`ox_eval::is_builtin_implemented`]) and the
/// editor-stateful families in [`crate::builtins::route`]. Every other name in
/// the generated `eval.lua` table resolves to an `E117: not implemented` arm,
/// and answering 1 for those makes `check.vim`'s `CheckFunction` inert: the
/// guarded file runs code that cannot work instead of skipping honestly.
fn is_callable_function(name: &str) -> bool {
    ox_eval::is_builtin_implemented(name) || crate::builtins::route(name).is_some()
}

/// Whether this port executes the Ex command `name`, as opposed to merely
/// resolving it out of the 564-entry generated `COMMANDS` table.
///
/// `cmd_exists` (`ex_docmd.c:3226`) answers from upstream's own table, where
/// resolving and executing are the same question. Here they are not: an
/// unhandled name reaches `Flow::NotImplemented` (`excmd_exec.rs:1037`), so
/// `exists(':wshada')` answering 2 makes `CheckCommand` inert the same way.
///
/// The three tables below are the three places a command name is served, and
/// each is derived from that place — re-derive them there when it changes:
///  * `dispatch` (`excmd_exec.rs:903-1034`), one arm per name;
///  * `run_program` (`excmd_exec.rs:682-872`), the control-flow openers it
///    interprets before `dispatch` is reached, plus the closers it consumes;
///  * [`ox_excmd::ModifierKind`], the modifiers the Ex parser recognises.
///    Upstream answers for modifiers out of `cmdmods` before it consults the
///    command table, and a modifier has no execution separate from the command
///    it decorates, so recognition is the whole question for them.
const DISPATCHED_COMMANDS: &[&str] = &[
    "Next",
    "argadd",
    "argdelete",
    "argdo",
    "args",
    "augroup",
    "aunmenu",
    "autocmd",
    "bdelete",
    "bnext",
    "bprevious",
    "break",
    "buffer",
    "bunload",
    "bwipeout",
    "call",
    "cc",
    "cclose",
    "cd",
    "cexpr",
    "cfirst",
    "clearjumps",
    "close",
    "cmap",
    "cmapclear",
    "cnext",
    "cnoremap",
    "colorscheme",
    "comclear",
    "command",
    "const",
    "continue",
    "copen",
    "cprevious",
    "cquit",
    "cunmap",
    "cwindow",
    "delcommand",
    "delmarks",
    "delete",
    "delfunction",
    "display",
    "echo",
    "echoerr",
    "echohl",
    "echomsg",
    "echon",
    "edit",
    "enew",
    "eval",
    "execute",
    "file",
    "filetype",
    "packadd",
    "preserve",
    "runtime",
    "iabbrev",
    "abclear",
    "find",
    "finish",
    "fold",
    "foldclose",
    "foldopen",
    "global",
    "hide",
    "highlight",
    "imap",
    "imapclear",
    "lmapclear",
    "lnoremap",
    "lockvar",
    "ll",
    "llast",
    "lfirst",
    "lmap",
    "lopen",
    "lprevious",
    "lunmap",
    "lua",
    "luado",
    "luafile",
    "lwindow",
    "map",
    "mapclear",
    "mark",
    "marks",
    "new",
    "next",
    "nmap",
    "nmapclear",
    "nnoremap",
    "noremap",
    "normal",
    "nunmap",
    "omap",
    "omapclear",
    "only",
    "onoremap",
    "ounmap",
    "previous",
    "print",
    "put",
    "qall",
    "quit",
    "read",
    "redir",
    "redo",
    "redraw",
    "redrawstatus",
    "redrawtabline",
    "registers",
    "resize",
    "retab",
    "return",
    "rshada",
    "rviminfo",
    "scriptencoding",
    "scriptnames",
    "set",
    "setfiletype",
    "setglobal",
    "setlocal",
    "sleep",
    "smap",
    "smapclear",
    "snoremap",
    "source",
    "split",
    "substitute",
    "sunmap",
    "swapname",
    "syntax",
    "tabedit",
    "tabnew",
    "tabonly",
    "throw",
    "tlunmenu",
    "tmap",
    "tmapclear",
    "tnoremap",
    "tunmap",
    "undo",
    "undojoin",
    "unlet",
    "unlockvar",
    "unmap",
    "update",
    "vglobal",
    "vmap",
    "vmapclear",
    "vnew",
    "vnoremap",
    "vsplit",
    "vunmap",
    "windo",
    "wincmd",
    "wq",
    "write",
    "wshada",
    "wviminfo",
    "xit",
    "xmap",
    "xmapclear",
    "xnoremap",
    "xunmap",
    "yank",
    "z",
];

const PROGRAM_COMMANDS: &[&str] = &[
    "catch",
    "else",
    "elseif",
    "endfor",
    "endfunction",
    "endif",
    "endtry",
    "endwhile",
    "finally",
    "for",
    "function",
    "if",
    "try",
    "while",
];

const COMMAND_MODIFIERS: &[&str] = &[
    "aboveleft",
    "belowright",
    "botright",
    "browse",
    "confirm",
    "filter",
    "horizontal",
    "keepalt",
    "keepjumps",
    "keepmarks",
    "keeppatterns",
    "leftabove",
    "lockmarks",
    "noautocmd",
    "noswapfile",
    "rightbelow",
    "sandbox",
    "silent",
    "tab",
    "topleft",
    "unsilent",
    "verbose",
    "vertical",
];

fn is_executed_command(name: &str) -> bool {
    DISPATCHED_COMMANDS.contains(&name)
        || PROGRAM_COMMANDS.contains(&name)
        || COMMAND_MODIFIERS.contains(&name)
}

#[cfg(test)]
mod tests {
    use ox_types::{OxStr, Typval};

    use crate::script::RealFileIO;
    use crate::{Editor, ExExecutor, ExecError, TestEditorAccess};

    fn text(value: &str) -> Typval {
        Typval::String(OxStr::from(value))
    }

    fn executor() -> ExExecutor<RealFileIO> {
        ExExecutor::with_io(RealFileIO)
    }

    fn script(exec: &mut ExExecutor<RealFileIO>, editor: &TestEditorAccess, source: &str) {
        exec.execute_script(editor, "<test>", source)
            .expect("script runs");
    }

    fn eval(
        exec: &mut ExExecutor<RealFileIO>,
        editor: &TestEditorAccess,
        expression: &str,
    ) -> Typval {
        exec.evaluate_expression(editor, expression)
            .expect("expression evaluates")
    }

    fn failure(
        exec: &mut ExExecutor<RealFileIO>,
        editor: &TestEditorAccess,
        expression: &str,
    ) -> String {
        match exec.evaluate_expression(editor, expression) {
            Err(ExecError::Vim(exception)) => exception.message(),
            other => panic!("expected a Vim error, got {other:?}"),
        }
    }

    /// `call()` reaches a builtin by name, a Funcref, and a lambda, which are
    /// the three callee shapes `f_call` accepts (`eval/funcs.c`).
    #[test]
    fn call_dispatches_names_funcrefs_and_lambdas() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = executor();
        script(&mut exec, &editor, "let g:a = call('toupper', ['a'])");
        script(
            &mut exec,
            &editor,
            "let g:b = call(function('toupper'), ['abc'])",
        );
        script(&mut exec, &editor, "let g:c = call({x -> x * 2}, [21])");
        assert_eq!(eval(&mut exec, &editor, "g:a"), text("A"));
        assert_eq!(eval(&mut exec, &editor, "g:b"), text("ABC"));
        assert_eq!(eval(&mut exec, &editor, "g:c"), Typval::Number(42));
    }

    /// A dictionary argument binds `self`, both through `call()` and through a
    /// Partial built by `function({name}, {dict})`.
    #[test]
    fn a_dictionary_argument_binds_self() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = executor();
        script(
            &mut exec,
            &editor,
            "let d = {'v': 5}\nfunction! Get() dict abort\nreturn self.v\nendfunction",
        );
        script(&mut exec, &editor, "let g:direct = call('Get', [], d)");
        script(&mut exec, &editor, "let g:partial = function('Get', d)()");
        assert_eq!(eval(&mut exec, &editor, "g:direct"), Typval::Number(5));
        assert_eq!(eval(&mut exec, &editor, "g:partial"), Typval::Number(5));
    }

    /// The argument errors `f_call` reports: unknown callee, too few
    /// arguments for the callee, and a third argument that is not a
    /// dictionary.
    #[test]
    fn call_reports_upstream_argument_errors() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = executor();
        script(
            &mut exec,
            &editor,
            "function! Add(a, b) abort\nreturn a:a + a:b\nendfunction",
        );
        assert!(failure(&mut exec, &editor, "call('Nosuch', [])").contains("E117"));
        assert!(failure(&mut exec, &editor, "call('Add', [1])").contains("E119"));
        assert!(failure(&mut exec, &editor, "call('Add', [1, 2], [3])").contains("E731"));
    }

    /// `uniq()` without a comparator drops adjacent equal items.
    #[test]
    fn uniq_without_a_comparator_drops_adjacent_duplicates() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = executor();
        assert_eq!(
            eval(&mut exec, &editor, "uniq([1, 1, 2, 2, 1])"),
            eval(&mut exec, &editor, "[1, 2, 1]")
        );
        assert_eq!(
            eval(&mut exec, &editor, "uniq(['a', 'a'])"),
            eval(&mut exec, &editor, "['a']")
        );
    }

    /// With a comparator, the walk compares the last kept item with the next
    /// original one, and the item is dropped when the comparator is false.
    /// These six cases are the oracle's answers.
    #[test]
    fn uniq_honours_the_comparator() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = executor();
        let cases = [
            ("[1,1,2]", "{a,b -> a == b}", "[1, 1]"),
            ("[3,1,2]", "{a,b -> b > a}", "[3]"),
            ("[1,2,1]", "{a,b -> a == b}", "[1, 1]"),
            ("[1,2,3]", "{a,b -> a < b}", "[1, 2, 3]"),
            ("[1,2,3]", "{a,b -> a > b}", "[1]"),
        ];
        for (list, comparator, expected) in cases {
            assert_eq!(
                eval(&mut exec, &editor, &format!("uniq({list}, {comparator})")),
                eval(&mut exec, &editor, expected),
                "uniq({list}, {comparator})"
            );
        }
    }

    /// A comparator can be a named function bound to a dictionary, which is
    /// the `uniq(list, 'Name', dict)` form.
    #[test]
    fn uniq_comparator_can_be_a_dictionary_method() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = executor();
        script(
            &mut exec,
            &editor,
            "let g:d = {'same': 1}\nfunction! Same(a, b) dict abort\nreturn self.same && a:a == a:b\nendfunction",
        );
        assert_eq!(
            eval(&mut exec, &editor, "uniq([5, 5, 5], 'Same', g:d)"),
            eval(&mut exec, &editor, "[5, 5, 5]")
        );
    }

    /// `uniq()` refuses a non-list argument with upstream's error.
    #[test]
    fn uniq_rejects_a_non_list() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = executor();
        assert!(failure(&mut exec, &editor, "uniq(1)").contains("E686"));
    }
}
