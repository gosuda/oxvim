#![allow(clippy::unwrap_used)]

//! Behavioral tests for Task 8e Ex execution — function/source families.
//!
//! Citations (READ-ONLY spec under .references/neovim/):
//! * `src/nvim/ex_docmd.c` — `ex_function`, `ex_call`, `do_source`, <SNR> expansion,
//!   line continuation (`getline_equal`), :finish, nested sourcing.
//! * `src/nvim/eval/userfunc.c` — function definition flags (abort/range/dict/
//!   closure), call-frame `l:`/`a:` save+restore, varargs (a:0/a:000), named
//!   parameter binding, 'maxfuncdepth' E132, E117 unknown function,
//!   E118/E119 argument count, E122 redefinition, function! replacement.
//! * src/nvim/runtime.c — autoload name-to-path resolution (autoload/ dir
//!   mapping), load-once registry, scriptnames SID allocation.
//! * `test/old/testdir/test_user_func.vim` — function definition, bang, args,
//!   varargs, flags, recursion, E117/E118/E119/E122.
//! * `test/old/testdir/test_source.vim` — sourcing, SID, s: isolation,
//!   <SID>/<SNR>, :finish, nested source, line continuation.
//! * `test/old/testdir/test_autoload.vim` — autoload path resolution, load-once.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ox_eval::{BUILTINS, Scope, ScopeKind};
use ox_text::Buffer;
use ox_types::{Object, OxStr, Typval};

use crate::script::{FileIO, ScriptCtx, SourceContext};
use crate::userfunc::{MAX_FUNC_DEPTH, UserFunctions};
use crate::{
    AutocmdFilter, AutocmdKind, AutocmdOptions, Editor, Event, ExExecutor, ExecError, Geometry,
    LuaExec, LuaExecError, RuntimeRoot, TestEditorAccess, VimExceptionKind,
};

// ---------------------------------------------------------------------------
// Deterministic in-memory FileIO for source/autoload tests.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct MemoryFileIO {
    files: Rc<RefCell<BTreeMap<PathBuf, String>>>,
}

impl MemoryFileIO {
    fn new() -> Self {
        Self {
            files: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }
    fn insert(&self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.files.borrow_mut().insert(path.into(), contents.into());
    }
}

impl FileIO for MemoryFileIO {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.files.borrow().get(path).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("not found: {}", path.display()),
            )
        })
    }
    fn write_string(&self, path: &Path, contents: &str) -> io::Result<()> {
        self.files
            .borrow_mut()
            .insert(path.to_path_buf(), contents.to_owned());
        Ok(())
    }
    fn write_bytes(&self, path: &Path, contents: &[u8], append: bool) -> io::Result<()> {
        let contents = std::str::from_utf8(contents)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8"))?;
        let mut files = self.files.borrow_mut();
        if append {
            files
                .entry(path.to_path_buf())
                .or_default()
                .push_str(contents);
        } else {
            files.insert(path.to_path_buf(), contents.to_owned());
        }
        Ok(())
    }
    fn exists(&self, path: &Path) -> bool {
        self.files.borrow().contains_key(path)
    }
    fn canonicalize(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }

    fn copy_file(&self, from: &Path, to: &Path) -> io::Result<()> {
        let content = self.files.borrow().get(from).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("not found: {}", from.display()),
            )
        })?;
        self.files.borrow_mut().insert(to.to_path_buf(), content);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn script(exec: &mut ExExecutor<MemoryFileIO>, editor: &TestEditorAccess, text: &str) {
    exec.execute_script(editor, "<test>", text).unwrap();
}

fn global_number(scope: &Scope, name: &str) -> Option<i64> {
    scope
        .global
        .iter()
        .find(|(k, _)| k.as_bytes() == name.as_bytes())
        .and_then(|(_, v)| {
            if let Typval::Number(n) = v {
                Some(*n)
            } else {
                None
            }
        })
}

fn global_string(scope: &Scope, name: &str) -> Option<String> {
    scope
        .global
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .and_then(|(_, value)| match value {
            Typval::String(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
}

fn error_code(err: &ExecError) -> String {
    match err {
        ExecError::Vim(e) => match &e.kind {
            VimExceptionKind::Error(code) => code.clone(),
            VimExceptionKind::Throw => "Throw".to_owned(),
        },
        other => panic!("expected Vim error, got {other:?}"),
    }
}

fn editor_with_lines(lines: &[&str]) -> Editor {
    let mut editor = Editor::new();
    let owned: Vec<Vec<u8>> = lines.iter().map(|l| l.as_bytes().to_vec()).collect();
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&owned, false).unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    editor
}

fn register_text(editor: &Editor, name: char) -> String {
    String::from_utf8(editor.registers().get(name).unwrap().unwrap().to_bytes()).unwrap()
}

// ---------------------------------------------------------------------------
// Family: function definition / call / return
// (eval/userfunc.c: ex_function, ex_call, get_return_value;
//  test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn function_define_and_call_sets_global() {
    // eval/userfunc.c: ex_function defines a user function; ex_call invokes
    // it and the body's side effects reach script-global scope.
    // test_user_func.vim: function definition and call with side effects.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Greet()\nlet g:hit = 1\nendfunction",
    );
    script(&mut exec, &editor, "call Greet()");
    assert_eq!(global_number(exec.scope(), "hit"), Some(1));
}

#[test]
fn function_return_value_via_expression_call() {
    // eval/userfunc.c: get_return_value — :return value reaches the caller
    // through the expression evaluator (BuiltinHost::call).
    // test_user_func.vim: function returning a computed value.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Add(a, b)\nreturn a + b\nendfunction",
    );
    script(&mut exec, &editor, "let g:r = Add(2, 3)");
    assert_eq!(global_number(exec.scope(), "r"), Some(5));
}

#[test]
fn eval_builtin_evaluates_string_and_scalar_sources_in_current_scope() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &editor, "let g:source_value = 42");
    script(
        &mut exec,
        &editor,
        "let g:evaluated = eval('g:source_value')",
    );
    script(&mut exec, &editor, "let g:scalar = eval(17)");
    assert_eq!(global_number(exec.scope(), "evaluated"), Some(42));
    assert_eq!(global_number(exec.scope(), "scalar"), Some(17));
}

#[test]
fn function_empty_signature_calls_cleanly() {
    // eval/userfunc.c: a function with no parameters accepts exactly zero
    // arguments and executes its body.
    // test_user_func.vim: empty-argument function.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Mark()\nlet g:marked = 1\nendfunction",
    );
    script(&mut exec, &editor, "call Mark()");
    assert_eq!(global_number(exec.scope(), "marked"), Some(1));
}

#[test]
fn function_bare_return_yields_zero() {
    // eval/userfunc.c: a bare :return with no expression yields numeric 0
    // (rettv set to &tv_zero). test_user_func.vim: return without argument.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &editor, "function! NoRet()\nreturn\nendfunction");
    script(&mut exec, &editor, "let g:r = NoRet()");
    assert_eq!(global_number(exec.scope(), "r"), Some(0));
}

// ---------------------------------------------------------------------------
// Family: bang replacement (function!)
// (eval/userfunc.c: ex_function `!` handling; test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn function_bang_replaces_existing() {
    // eval/userfunc.c: ex_function with `!` replaces an existing definition
    // instead of raising E122. test_user_func.vim: function! replacement.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Rep()\nlet g:v = 1\nendfunction",
    );
    script(
        &mut exec,
        &editor,
        "function! Rep()\nlet g:v = 2\nendfunction",
    );
    script(&mut exec, &editor, "call Rep()");
    assert_eq!(global_number(exec.scope(), "v"), Some(2));
}

#[test]
fn function_redefinition_without_bang_is_e122_across_scripts() {
    // eval/userfunc.c:2854-2860: a definition without `!` fails when the
    // existing function came from another script, or from the same script in
    // the same sourcing pass. Only re-sourcing one script allows it.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &editor, "function! Dup()\nendfunction");
    let err = exec
        .execute_script(&editor, "<other>", "function Dup()\nendfunction")
        .unwrap_err();
    assert_eq!(error_code(&err), "E122");
}

#[test]
fn function_redefinition_without_bang_is_allowed_when_one_script_loads_twice() {
    // eval/userfunc.c:2858-2860: the same script id with a new sequence number
    // replaces the function, so `:source` of one file twice is silent.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(&editor, "<test>", "function Dup()\nendfunction")
        .expect("first definition");
    exec.execute_script(&editor, "<test>", "function Dup()\nreturn 2\nendfunction")
        .expect("re-sourcing one script replaces its function");
    script(&mut exec, &editor, "let g:r = Dup()");
    assert_eq!(global_number(exec.scope(), "r"), Some(2));
}

// ---------------------------------------------------------------------------
// Family: named args / varargs
// (eval/userfunc.c: named parameter binding, a:0, a:000;
//  test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn named_arg_accessible_via_a_prefix() {
    // eval/userfunc.c: named parameters are bound to a:{name} inside the
    // function body. test_user_func.vim: a: prefix argument access.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Id(x)\nreturn a:x\nendfunction",
    );
    script(&mut exec, &editor, "let g:r = Id(42)");
    assert_eq!(global_number(exec.scope(), "r"), Some(42));
}

#[test]
fn varargs_count_a0_reflects_extra_args() {
    // eval/userfunc.c: `...` collects extra arguments; a:0 is their count.
    // test_user_func.vim: varargs a:0.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Va(first, ...)\nreturn a:0\nendfunction",
    );
    script(&mut exec, &editor, "let g:r = Va(1, 2, 3)");
    assert_eq!(global_number(exec.scope(), "r"), Some(2));
}

#[test]
fn varargs_a000_list_holds_extra_values() {
    // eval/userfunc.c: a:000 is the list of vararg values.
    // test_user_func.vim: varargs a:000 list.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Va2(...)\nreturn a:000\nendfunction",
    );
    script(&mut exec, &editor, "let g:r = Va2(10, 20)");
    let val = exec
        .scope()
        .global
        .iter()
        .find(|(k, _)| k.as_bytes() == b"r")
        .map(|(_, v)| v.clone())
        .expect("g:r should be set");
    match val {
        Typval::List(cell) => {
            let data = cell.borrow();
            assert_eq!(data.items.len(), 2);
            match &data.items[0] {
                Typval::Number(n) => assert_eq!(*n, 10),
                _ => panic!("expected first item Number(10)"),
            }
            match &data.items[1] {
                Typval::Number(n) => assert_eq!(*n, 20),
                _ => panic!("expected second item Number(20)"),
            }
        }
        _ => panic!("expected list for a:000"),
    }
}

#[test]
fn too_many_args_yields_e118() {
    // eval/userfunc.c: supplying more arguments than declared (without
    // varargs) raises E118 "Too many arguments".
    // test_user_func.vim: E118.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &editor, "function! One(x)\nendfunction");
    let err = exec
        .execute_script(&editor, "<test>", "call One(1, 2)")
        .unwrap_err();
    assert_eq!(error_code(&err), "E118");
}

#[test]
fn not_enough_args_yields_e119() {
    // eval/userfunc.c: supplying fewer arguments than declared raises
    // E119 "Not enough arguments". test_user_func.vim: E119.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &editor, "function! Two(x, y)\nendfunction");
    let err = exec
        .execute_script(&editor, "<test>", "call Two(1)")
        .unwrap_err();
    assert_eq!(error_code(&err), "E119");
}

#[test]
fn eval_builtin_reports_exact_arity_errors() {
    for (source, expected) in [
        ("let g:ignored = eval()", "E119"),
        ("let g:ignored = eval(1, 2)", "E118"),
    ] {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::with_io(MemoryFileIO::new());
        let error = exec.execute_script(&editor, "<test>", source).unwrap_err();
        assert_eq!(error_code(&error), expected);
    }
}

// ---------------------------------------------------------------------------
// Family: default arguments
// (eval/userfunc.c get_function_args / call_user_func; test_user_func.vim
//  Test_default_arg, Test_default_argument_expression_error_while_inside_of_
//  a_try_block)
// ---------------------------------------------------------------------------

#[test]
fn default_args_fill_omitted_positionals() {
    // call_user_func: omitted defaulted parameters are bound to the value of
    // their expression; supplied values win. test_user_func.vim: Log().
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Sum(x, y = 2)\nreturn a:x + a:y\nendfunction",
    );
    script(&mut exec, &editor, "let g:a = Sum(40)");
    script(&mut exec, &editor, "let g:b = Sum(40, 3)");
    assert_eq!(global_number(exec.scope(), "a"), Some(42));
    assert_eq!(global_number(exec.scope(), "b"), Some(43));
}

#[test]
fn default_args_do_not_count_toward_a0() {
    // call_user_func: a:0 is max(argcount - uf_args, 0); defaults fill
    // positionals and never count as varargs. test_user_func.vim: Args().
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Args(mandatory, optional = v:null, ...)\nreturn deepcopy(a:)\nendfunction",
    );
    script(&mut exec, &editor, "let g:one = Args(1)");
    script(&mut exec, &editor, "let g:three = Args(1, 2, 3)");
    let value = |scope: &Scope, name: &str| {
        scope
            .global
            .iter()
            .find(|(key, _)| key.as_bytes() == name.as_bytes())
            .map(|(_, value)| value.clone())
    };
    let entry = |scope: &Scope, name: &str, key: &[u8]| match value(scope, name) {
        Some(Typval::Dict(cell)) => {
            let data = cell.borrow();
            data.entries
                .iter()
                .find(|candidate| candidate.key.as_bytes() == key)
                .map(|entry| entry.value.clone())
        }
        _ => panic!("expected dict for {name}"),
    };
    assert_eq!(
        entry(exec.scope(), "one", b"mandatory"),
        Some(Typval::Number(1))
    );
    assert_eq!(
        entry(exec.scope(), "one", b"optional"),
        Some(Typval::Special(ox_types::Special::Null))
    );
    assert_eq!(entry(exec.scope(), "one", b"0"), Some(Typval::Number(0)));
    assert_eq!(
        entry(exec.scope(), "three", b"optional"),
        Some(Typval::Number(2))
    );
    assert_eq!(entry(exec.scope(), "three", b"0"), Some(Typval::Number(1)));
    assert_eq!(entry(exec.scope(), "three", b"1"), Some(Typval::Number(3)));
}

#[test]
fn default_args_still_e118_when_exceeding_total() {
    // check_user_func_argcount: too many arguments still applies to the full
    // positional count. test_user_func.vim: `call Log(1,2,3)` → E118.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Sum2(x, y = 2)\nreturn a:x + a:y\nendfunction",
    );
    let err = exec
        .execute_script(&editor, "<test>", "call Sum2(1, 2, 3)")
        .unwrap_err();
    assert_eq!(error_code(&err), "E118");
}

#[test]
fn default_args_still_e119_below_required() {
    // check_user_func_argcount: required = uf_args - uf_def_args.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Req(a, b, c = 1)\nreturn a:c\nendfunction",
    );
    let err = exec
        .execute_script(&editor, "<test>", "call Req(1)")
        .unwrap_err();
    assert_eq!(error_code(&err), "E119");
}

#[test]
fn non_default_argument_after_default_is_e989() {
    // get_function_args: E989. test_user_func.vim: MakeBadFunc().
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let err = exec
        .execute_script(&editor, "<test>", "function Bad(a, b=1, c)\nendfunction")
        .unwrap_err();
    assert_eq!(error_code(&err), "E989");
}

#[test]
fn white_space_before_comma_is_e1068() {
    // get_function_args: E1068. test_user_func.vim:
    // `fu F(a=1 ,) | endf` → E1068.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let err = exec
        .execute_script(&editor, "<test>", "function W(a=1 ,)\nendfunction")
        .unwrap_err();
    assert_eq!(error_code(&err), "E1068");
}

#[test]
fn default_expression_may_contain_commas_strings_and_nesting() {
    // get_function_args walks the default expression like eval1, so commas
    // inside strings/lists/dicts and parens inside strings do not split the
    // argument list or end it early.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! L(x, y = [1, 2, 3])\nreturn len(a:y) + a:x\nendfunction",
    );
    script(&mut exec, &editor, "let g:list = L(1)");
    assert_eq!(global_number(exec.scope(), "list"), Some(4));
    script(
        &mut exec,
        &editor,
        "function! S(x, y = \"a,b)\")\nreturn len(a:y)\nendfunction",
    );
    script(&mut exec, &editor, "let g:str = S(0)");
    assert_eq!(global_number(exec.scope(), "str"), Some(4));
}

#[test]
fn default_expression_evaluates_in_caller_scope_and_aborts_call() {
    // call_user_func evaluates defaults before entering the frame; an error
    // surfaces from the call site and the body never runs.
    // test_user_func.vim:
    // Test_default_argument_expression_error_while_inside_of_a_try_block.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        concat!(
            "function! s:f(v = s:undefined_variable)\n",
            "let s:entered_fn_body = 1\n",
            "return a:v\n",
            "endfunction\n",
            "let g:caught = 0\n",
            "try\n",
            "call s:f()\n",
            "catch\n",
            "let g:caught = 1\n",
            "let g:msg = v:exception\n",
            "endtry\n",
            "let g:entered = exists('s:entered_fn_body')"
        ),
    );
    assert_eq!(global_number(exec.scope(), "caught"), Some(1));
    // Oracle: an error raised while evaluating a default argument escapes
    // through `:call`, so `v:exception` is `Vim(call):E121: ...`.
    assert!(
        global_string(exec.scope(), "msg")
            .unwrap()
            .starts_with("Vim(call):E121: Undefined variable: s:undefined_variable")
    );
    assert_eq!(global_number(exec.scope(), "entered"), Some(0));
}

#[test]
fn evaluator_error_inside_user_function_enters_caller_catch_frame() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        concat!(
            "function! BrokenBuiltin()\n",
            "return screenrow()\n",
            "endfunction\n",
            "let g:caught = 0\n",
            "let g:finalized = 0\n",
            "try\n",
            "call BrokenBuiltin()\n",
            "catch\n",
            "let g:caught = 1\n",
            "let g:exception = v:exception\n",
            "let g:throwpoint = v:throwpoint\n",
            "finally\n",
            "let g:finalized = 1\n",
            "endtry"
        ),
    );
    assert_eq!(global_number(exec.scope(), "caught"), Some(1));
    assert_eq!(global_number(exec.scope(), "finalized"), Some(1));
    // `screenrow()` is implemented (returns E957 when no current window,
    // not E117 NotImplemented). The error prefix is `Vim(return)` because
    // `return` is the executing command (`do_one_cmd`'s `cmdidx`) when the
    // expression evaluation fails inside the function body — upstream
    // `do_errthrow(cstack, cmdnames[ea.cmdidx].cmd_name)` at
    // `ex_docmd.c:2385-2387` uses the current command's canonical name.
    assert_eq!(
        global_string(exec.scope(), "exception").as_deref(),
        Some("Vim(return):E957: Invalid window")
    );
    assert_eq!(
        global_string(exec.scope(), "throwpoint").as_deref(),
        Some("<test>[7]..function BrokenBuiltin, line 1")
    );
}

#[test]
fn public_function_entry_points_preserve_user_throw() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! Foo() abort\nthrow 'wtf'\nendfunction",
    );

    let errors = [
        exec.call_builtin(&editor, &OxStr::from("Foo"), Vec::new())
            .unwrap_err(),
        exec.evaluate_expression(&editor, "Foo()").unwrap_err(),
    ];
    for error in errors {
        let ExecError::Vim(exception) = error else {
            panic!("expected preserved Vim exception, got {error:?}");
        };
        assert_eq!(exception.kind, VimExceptionKind::Throw);
        assert_eq!(exception.message(), "wtf");
        assert_eq!(exception.throwpoint, "function Foo, line 1");
    }
}

// ---------------------------------------------------------------------------
// Family: l: / a: scope isolation and restoration
// (eval/userfunc.c: call_def_function saves/restores caller l: and a:;
//  test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn local_and_argument_scope_isolated_and_restored() {
    // eval/userfunc.c: call_def_function saves and restores the caller's
    // l: and a: scopes; function-local l: vars do not leak out, and the
    // caller's l: vars survive the call.
    // test_user_func.vim: scope isolation.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &editor, "let l:outer = 7");
    script(
        &mut exec,
        &editor,
        "function! Iso()\nlet l:inner = 99\nendfunction",
    );
    script(&mut exec, &editor, "call Iso()");
    // caller l:outer survives, l:inner does not leak
    let local = &exec.scope().local;
    assert!(
        local
            .iter()
            .any(|(k, v)| k.as_bytes() == b"outer" && matches!(v, Typval::Number(7)))
    );
    assert!(!local.iter().any(|(k, _)| k.as_bytes() == b"inner"));
    // a: scope is restored to empty (no active function)
    assert!(exec.scope().argument.is_empty());
}

// ---------------------------------------------------------------------------
// Family: abort / range / dict / closure flag storage
// (eval/userfunc.c: flag parsing in ex_function; test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn parse_signature_records_abort_flag() {
    // eval/userfunc.c: the `abort` flag makes a function stop on the first
    // uncaught error. test_user_func.vim: abort flag.
    let sig = UserFunctions::parse_signature("F() abort").unwrap();
    assert!(sig.flags.contains(crate::UserFuncFlags::ABORT));
    assert!(!sig.flags.contains(crate::UserFuncFlags::RANGE));
    assert!(!sig.flags.contains(crate::UserFuncFlags::DICT));
    assert!(!sig.flags.contains(crate::UserFuncFlags::CLOSURE));
}

#[test]
fn parse_signature_records_range_dict_closure_flags() {
    // eval/userfunc.c: range/dict/closure flags are parsed after the ')'.
    // test_user_func.vim: range, dict, closure flags.
    let sig = UserFunctions::parse_signature("F() range dict closure").unwrap();
    assert!(sig.flags.contains(crate::UserFuncFlags::RANGE));
    assert!(sig.flags.contains(crate::UserFuncFlags::DICT));
    assert!(sig.flags.contains(crate::UserFuncFlags::CLOSURE));
    assert!(!sig.flags.contains(crate::UserFuncFlags::ABORT));
}

#[test]
fn closure_captures_defining_local_scope() {
    // eval/userfunc.c: a `closure` function captures the defining l: scope
    // so it can reference locals after the defining block ends.
    // test_user_func.vim: closure captures.
    let mut funcs = UserFunctions::new();
    let mut scope = Scope::new();
    scope.set(b"captured", Typval::Number(123)).unwrap();
    let sig = UserFunctions::parse_signature("Clo() closure").unwrap();
    funcs
        .define(
            sig,
            vec!["return l:captured".to_owned()],
            SourceContext::default(),
            false,
            &scope,
        )
        .unwrap();
    let func = funcs.get("Clo", 0).unwrap();
    assert!(func.flags.contains(crate::UserFuncFlags::CLOSURE));
    let found = func
        .captured
        .iter()
        .find(|(k, _)| k.as_bytes() == b"captured");
    assert!(found.is_some());
    let (_, val) = found.unwrap();
    if let Typval::Number(n) = val {
        assert_eq!(*n, 123);
    } else {
        panic!("expected Number(123) in captured scope");
    }
}

#[test]
fn range_function_receives_firstline_and_lastline() {
    // eval/userfunc.c: a `range` function receives a:firstline / a:lastline
    // from the call's line range. test_user_func.vim: range function.
    let editor = TestEditorAccess::new(editor_with_lines(&["alpha", "beta", "gamma"]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function! RangeFn() range\nlet g:fl = a:firstline\nlet g:ll = a:lastline\nendfunction",
    );
    script(&mut exec, &editor, "1,3call RangeFn()");
    assert_eq!(global_number(exec.scope(), "fl"), Some(1));
    assert_eq!(global_number(exec.scope(), "ll"), Some(3));
}

// ---------------------------------------------------------------------------
// Family: recursion limit E132, unknown E117
// (eval/userfunc.c: 'maxfuncdepth', E117; test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn recursion_exceeds_maxfuncdepth_e132() {
    // eval/userfunc.c: 'maxfuncdepth' (default 100) limits call depth;
    // exceeding it raises E132. test_user_func.vim: recursion limit.
    let mut funcs = UserFunctions::new();
    let sig = UserFunctions::parse_signature("Recurse()").unwrap();
    funcs
        .define(sig, vec![], SourceContext::default(), false, &Scope::new())
        .unwrap();
    let mut scope = Scope::new();
    for _ in 0..MAX_FUNC_DEPTH {
        funcs
            .begin_call("Recurse", 0, vec![], 1, 1, &mut scope, 0)
            .unwrap();
    }
    let err = funcs
        .begin_call("Recurse", 0, vec![], 1, 1, &mut scope, 0)
        .unwrap_err();
    assert_eq!(err.code, "E132");
}

#[test]
fn call_unknown_function_yields_e117() {
    // eval/userfunc.c: calling a function not in the table raises
    // E117 "Unknown function". test_user_func.vim: E117.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let err = exec
        .execute_script(&editor, "<test>", "call DoesNotExist()")
        .unwrap_err();
    assert_eq!(error_code(&err), "E117");
}

// ---------------------------------------------------------------------------
// Family: source SID allocation, s: isolation, <SID>/<SNR>
// (runtime.c: do_source SID; ex_docmd.c: <SNR> expansion, s: scope;
//  test_source.vim)
// ---------------------------------------------------------------------------

#[test]
fn source_allocates_monotonic_sids() {
    // runtime.c: do_source allocates a monotonic SID per sourcing event;
    // scriptnames lists them in allocation order. test_source.vim: SID.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(&editor, "a.vim", "let g:xa = 1")
        .unwrap();
    exec.execute_script(&editor, "b.vim", "let g:xb = 2")
        .unwrap();
    let names = exec.scripts().script_names();
    assert_eq!(names, vec![(1, "a.vim"), (2, "b.vim")]);
}

#[test]
fn s_scope_isolated_between_scripts() {
    // runtime.c / ex_docmd.c: each sourced script gets its own s: scope;
    // s: variables from one script are not visible in another.
    // test_source.vim: script-local variable isolation.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(&editor, "a.vim", "let s:secret = 42")
        .unwrap();
    let err = exec
        .execute_script(&editor, "b.vim", "echo s:secret")
        .unwrap_err();
    assert_eq!(error_code(&err), "E121");
}

#[test]
fn s_function_canonical_name_and_snr_resolution() {
    // ex_docmd.c: s:Name canonicalizes to <SNR>{sid}_Name; <SID>Name
    // resolves the same way. <SNR> in sourced lines expands to <SNR>{sid}_.
    // test_source.vim: <SID>/s: function names.
    assert_eq!(UserFunctions::canonical_name("s:Foo", 3), "<SNR>3_Foo");
    assert_eq!(UserFunctions::canonical_name("<SID>Foo", 3), "<SNR>3_Foo");
    assert_eq!(UserFunctions::canonical_name("Global", 3), "Global");

    // <SNR> token expansion in sourced lines.
    let ctx: ScriptCtx<MemoryFileIO> = ScriptCtx::new(MemoryFileIO::new());
    assert_eq!(ctx.expand_snr("let x = <SNR>", 1), "let x = <SNR>1_");
    assert_eq!(ctx.expand_snr("let x = plain", 1), "let x = plain");

    // Define an s: function and call it from within the same script.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "lib.vim",
        "function! s:Helper()\nlet g:helped = 1\nendfunction\ncall s:Helper()",
    )
    .unwrap();
    assert!(exec.functions().contains("<SNR>1_Helper", 0));
    assert_eq!(global_number(exec.scope(), "helped"), Some(1));
}

#[test]
fn script_local_dictionary_member_function_can_be_defined_and_called() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_script(
        &editor,
        "plugin.vim",
        "let s:logger = {}\nfunction! s:logger.on_stdout()\nlet g:called = 1\nendfunction\ncall s:logger.on_stdout()",
    )
    .unwrap();

    assert_eq!(global_number(exec.scope(), "called"), Some(1));
}

#[test]
fn lowercase_bare_function_name_still_yields_e128() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let error = exec
        .execute_script(&editor, "plugin.vim", "function! lowercase()\nendfunction")
        .unwrap_err();
    assert_eq!(error_code(&error), "E128");
}

#[test]
fn lowercase_script_local_function_name_is_allowed() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "plugin.vim",
        "function! s:lowercase()\nlet g:called_local = 1\nendfunction\ncall s:lowercase()",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "called_local"), Some(1));
}

#[test]
fn same_script_function_call_keeps_live_script_scope() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "plugin.vim",
        "let s:state = {'value': 42}\nfunction! Main()\nlet g:from_script = s:state.value\nendfunction\ncall Main()",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "from_script"), Some(42));
}

#[test]
fn delfunction_removes_registry_entries_and_bang_ignores_missing() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "delete.vim",
        "function! DeleteMe()\nreturn 1\nendfunction\ndelfunction DeleteMe\ndelfunction! DeleteMe",
    )
    .unwrap();
    let error = exec.execute_line(&editor, "call DeleteMe()").unwrap_err();
    assert_eq!(error_code(&error), "E117");
    let error = exec
        .execute_line(&editor, "delfunction DeleteMe")
        .unwrap_err();
    assert_eq!(error_code(&error), "E130");
}

#[test]
fn direct_api_multiline_function_block_defines_and_calls() {
    // do_cmdline (ex_docmd.c:321-...): a direct multiline string is one
    // do_cmdline run — every physical line executes, so a `function` header,
    // its body lines, and the matching `endfunction`/`endfunc` define the
    // function and later lines on the same string can call it. nvim_command
    // feeds the whole string to do_cmdline_cmd; only the interactive
    // command-line path collects typed lines.
    // Upstream repro (fold_spec.lua, eval_spec.lua family): pcall
    // nvim_command, "function! Hello()\n  return 42\nendfunction\ncall
    // Hello()" — must not raise Vim(function):E126: Missing :endfunction.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(
        &editor,
        "let g:before = 7\nfunction! Hello()\nlet g:greeted = 1\nreturn 42\nendfunc\ncall Hello()\nlet g:after = 9",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "before"), Some(7));
    assert_eq!(global_number(exec.scope(), "greeted"), Some(1));
    assert_eq!(global_number(exec.scope(), "after"), Some(9));
}

#[test]
fn delfunction_rejects_the_active_function() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let error = exec
        .execute_script(
            &editor,
            "active.vim",
            "function! Active()\ndelfunction Active\nendfunction\ncall Active()",
        )
        .unwrap_err();
    assert_eq!(error_code(&error), "E131");
}

// ---------------------------------------------------------------------------
// Family: continuation / comment joining
// (ex_docmd.c: getline_equal; test_source.vim)
// ---------------------------------------------------------------------------

#[test]
fn continuation_joining_and_comment_skipping() {
    // ex_docmd.c: getline_equal — a line whose first non-blank is `\`
    // continues the previous logical line; comment lines (first non-blank
    // `"`) are skipped and do not break a pending continuation.
    // test_source.vim: line continuation.
    let ctx: ScriptCtx<MemoryFileIO> = ScriptCtx::new(MemoryFileIO::new());
    let lines = ctx
        .join_logical_lines("let g:x = 1\n\" a comment\nlet g:y = 2 +\n\\  3")
        .unwrap();
    let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, vec!["let g:x = 1", "let g:y = 2 +  3"]);
}

// ---------------------------------------------------------------------------
// Family: :finish, nested source restoration
// (ex_docmd.c: ex_finish, do_source; test_source.vim)
// ---------------------------------------------------------------------------

#[test]
fn finish_terminates_sourced_script() {
    // ex_docmd.c: :finish terminates sourcing the current script; commands
    // after :finish are not executed. test_source.vim: :finish.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "fin.vim",
        "let g:before = 1\nfinish\nlet g:after = 2",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "before"), Some(1));
    assert_eq!(global_number(exec.scope(), "after"), None);
}

#[test]
fn nested_source_restores_caller_sid_and_script_scope() {
    // ex_docmd.c / runtime.c: sourcing a script from within another script
    // pushes a new SID/s: scope and restores the caller's on return.
    // test_source.vim: nested sourcing.
    let io = MemoryFileIO::new();
    io.insert("/inner.vim", "let s:inner_var = 99\nlet g:inner_ran = 1");
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &editor,
        "/outer.vim",
        "let s:outer_var = 1\nsource /inner.vim\nlet g:after_inner = s:outer_var",
    )
    .unwrap();
    // inner script ran
    assert_eq!(global_number(exec.scope(), "inner_ran"), Some(1));
    // after returning from inner, outer's s: scope is restored
    assert_eq!(global_number(exec.scope(), "after_inner"), Some(1));
    // SID registry reflects both scripts
    let names = exec.scripts().script_names();
    assert_eq!(names, vec![(1, "/outer.vim"), (2, "/inner.vim")]);
}

#[test]
fn computed_name_script_variable_assignment_visible_to_later_read() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "test.vim",
        "function! s:Init(n, v)\n  let {a:n} = a:v\nendfunction\ncall s:Init('s:foo', 42)\ncall s:Init('g:bar', 99)\nlet g:val = s:foo",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "val"), Some(42));
    assert_eq!(global_number(exec.scope(), "bar"), Some(99));
}

#[test]
fn nested_source_calling_defining_script_function_updates_script_scope() {
    let io = MemoryFileIO::new();
    io.insert(
        "/inner.vim",
        "call Init('s:foo', 42)\ncall Init('g:bar', 99)",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &editor,
        "/outer.vim",
        "function! Init(n, v)\n  let {a:n} = a:v\nendfunction\nsource /inner.vim\nlet g:val = s:foo",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "val"), Some(42));
    assert_eq!(global_number(exec.scope(), "bar"), Some(99));
}

#[test]
fn autocmd_ex_string_preserves_defining_script_scope() {
    let io = MemoryFileIO::new();
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &editor,
        "/script.vim",
        "let s:x = 100\nfunction! s:Get()\n  return s:x\nendfunction\nau User MyEvent let g:result = s:Get()",
    )
    .unwrap();
    exec.execute_line(&editor, "doautocmd User MyEvent").unwrap();
    assert_eq!(global_number(exec.scope(), "result"), Some(100));
}

// ---------------------------------------------------------------------------
// Family: command resolution happens when a line runs, and re-sourcing a
// script reuses its SID (ex_docmd.c find_ex_command / do_one_cmd;
// runtime.c:2226,2333 find_script_by_name + sc_seq)
// ---------------------------------------------------------------------------

#[test]
fn a_user_command_is_resolved_when_its_line_runs_not_when_it_is_parsed() {
    // find_ex_command consults the live command table inside do_one_cmd, so a
    // :command created earlier in the same script — or in a script sourced by
    // it — is visible on a later line even though the whole body was read
    // first. check.vim's CheckFunction reaches test bodies exactly this way.
    let io = MemoryFileIO::new();
    io.insert(
        "/guard.vim",
        "command! -nargs=1 T69Guard let g:guarded = <q-args>",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &editor,
        "/main.vim",
        "command! -nargs=1 T69Same let g:same = <q-args>\n\
         T69Same here\n\
         source /guard.vim\n\
         function! T69Body()\n\
         T69Guard inside\n\
         endfunction\n\
         call T69Body()",
    )
    .unwrap();
    assert_eq!(global_string(exec.scope(), "same").as_deref(), Some("here"));
    assert_eq!(
        global_string(exec.scope(), "guarded").as_deref(),
        Some("inside")
    );
}

#[test]
fn an_unresolvable_command_still_reports_e492_after_the_retry() {
    // The retry is a re-resolution, not a rescue: a name no :command ever
    // created reports E492 exactly as before.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let err = exec
        .execute_script(&editor, "/main.vim", "T69NeverDefined arg")
        .unwrap_err();
    assert_eq!(error_code(&err), "E492");
}

#[test]
fn re_sourcing_a_script_keeps_its_script_local_variables() {
    // do_source looks the file up with find_script_by_name and reuses its SID
    // (runtime.c:2226,2335), so `if exists('s:did_load') | finish | endif`
    // — setup.vim:50-53, which guards the `comclear` that wipes every user
    // command — short-circuits on the second sourcing.
    let io = MemoryFileIO::new();
    io.insert(
        "/guarded.vim",
        "let g:runs = get(g:, 'runs', 0) + 1\n\
         if exists('s:did_load')\n\
         finish\n\
         endif\n\
         let s:did_load = 1\n\
         let g:bodies = get(g:, 'bodies', 0) + 1",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &editor,
        "/main.vim",
        "source /guarded.vim\nsource /guarded.vim\nsource /guarded.vim",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "runs"), Some(3));
    assert_eq!(global_number(exec.scope(), "bodies"), Some(1));
    // One registry entry for the file, not one per sourcing event.
    assert_eq!(
        exec.scripts().script_names(),
        vec![(1, "/main.vim"), (2, "/guarded.vim")]
    );
}

#[test]
fn a_reloaded_script_redefines_its_own_command_and_function_but_a_stranger_cannot() {
    // "can be replaced with ! and when sourcing the same script again, but
    // only once": usercmd.c:940-948 and eval/userfunc.c:2856-2863 both key on
    // (sc_sid, sc_seq). Same SID, new sequence — silent replace. Different
    // SID — E174/E122.
    let io = MemoryFileIO::new();
    io.insert(
        "/defs.vim",
        "command -nargs=1 T69Dup let g:dup = <q-args>\nfunc T69Fn()\nendfunc",
    );
    io.insert("/other.vim", "command -nargs=1 T69Dup let g:dup = 'other'");
    io.insert("/otherfn.vim", "func T69Fn()\nendfunc");
    // Two definitions inside *one* sourcing share a sequence number, so the
    // reload exemption must not cover them.
    io.insert(
        "/twice.vim",
        "command -nargs=1 T69Once echo 1\ncommand -nargs=1 T69Once echo 2",
    );
    io.insert(
        "/twicefn.vim",
        "func T69Twice()\nendfunc\nfunc T69Twice()\nendfunc",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(&editor, "/main.vim", "source /defs.vim\nsource /defs.vim")
        .unwrap();
    let command_err = exec
        .execute_script(&editor, "/caller.vim", "source /other.vim")
        .unwrap_err();
    assert_eq!(error_code(&command_err), "E174");
    let function_err = exec
        .execute_script(&editor, "/caller2.vim", "source /otherfn.vim")
        .unwrap_err();
    assert_eq!(error_code(&function_err), "E122");
    let same_seq_command = exec
        .execute_script(&editor, "/caller3.vim", "source /twice.vim")
        .unwrap_err();
    assert_eq!(error_code(&same_seq_command), "E174");
    let same_seq_function = exec
        .execute_script(&editor, "/caller4.vim", "source /twicefn.vim")
        .unwrap_err();
    assert_eq!(error_code(&same_seq_function), "E122");
}

// ---------------------------------------------------------------------------
// Family: autoload path resolution and load-once
// (runtime.c: autoload name-to-path, load-once; test_autoload.vim)
// ---------------------------------------------------------------------------

#[test]
fn autoload_path_resolution_and_load_once() {
    // runtime.c: a#b#c resolves to autoload/a/b.vim under a runtime root;
    // autoload scripts are sourced once per session. test_autoload.vim.
    let io = MemoryFileIO::new();
    io.insert(
        "/rt/autoload/mylib.vim",
        "function! mylib#Greet()\nlet g:auto_ran = 1\nendfunction",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.scripts_mut()
        .add_runtime_root(RuntimeRoot::new(PathBuf::from("/rt")));

    // Calling mylib#Greet() triggers autoload resolution + source-once.
    exec.execute_script(&editor, "<caller>", "call mylib#Greet()")
        .unwrap();
    assert_eq!(global_number(exec.scope(), "auto_ran"), Some(1));

    // The autoload script was registered as sourced-once.
    assert!(
        exec.scripts()
            .is_sourced_once(&PathBuf::from("/rt/autoload/mylib.vim"))
    );
}

// option.c did_set_runtimepackpath — runtime searches are glued to the
// 'runtimepath' option: `:set runtimepath=` re-roots them, and the
// compound `let &runtimepath ..=` form appends new roots exactly like
// oldtest runtest.vim does. test_autoload.vim / test_options.vim.
#[test]
fn runtime_lookups_follow_runtimepath_option() {
    let io = MemoryFileIO::new();
    io.insert("/first/colors/sample.vim", "let g:scheme_first = 1");
    io.insert("/second/colors/other.vim", "let g:scheme_second = 1");
    io.insert(
        "/appended/autoload/extra.vim",
        "function! extra#Ping()\nlet g:extra_ran = 1\nendfunction",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);

    // :set re-roots the search: /first only, /second unreachable.
    exec.execute_line(&editor, "set runtimepath=/first")
        .unwrap();
    exec.execute_line(&editor, "colorscheme sample").unwrap();
    assert_eq!(global_number(exec.scope(), "scheme_first"), Some(1));
    let error = exec.execute_line(&editor, "colorscheme other").unwrap_err();
    assert_eq!(error_code(&error), "E185");

    // `let &runtimepath ..=` appends a search root (runtest.vim:151).
    exec.execute_line(&editor, "let &runtimepath ..= ',/appended'")
        .unwrap();
    assert_eq!(
        editor.editor().options().get_global("runtimepath").unwrap(),
        &crate::options::OptionValue::String("/first,/appended".to_owned())
    );
    // Autoload resolution consults the appended root.
    exec.execute_script(&editor, "<caller>", "call extra#Ping()")
        .unwrap();
    assert_eq!(global_number(exec.scope(), "extra_ran"), Some(1));

    // A rewritten 'runtimepath' replaces the whole search list.
    exec.execute_line(&editor, "set runtimepath=/second")
        .unwrap();
    exec.execute_line(&editor, "colorscheme other").unwrap();
    assert_eq!(global_number(exec.scope(), "scheme_second"), Some(1));
}

// option.c stropt_expand_envvar + assign_option parity: a `:set` write is
// visible to `&opt` reads inside the same command batch (the eval scope
// mirror), and expand-flag options expand `$VAR`/`${VAR}` in the value.
// setup.vim:85 (`set rtp=$VIM/vimfiles,$VIMRUNTIME,...`) needs both.
#[test]
fn set_write_is_scope_visible_and_expands_env_vars() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let _guard = crate::lock_process_state();
    let _env = crate::test_guard::EnvGuard::new(&["OXVIM_TEST_SET_EXPAND"]);

    exec.execute_script(
        &editor,
        "<set>",
        "set runtimepath=/plain\nlet g:seen = &runtimepath",
    )
    .unwrap();
    assert_eq!(
        global_string(exec.scope(), "seen").as_deref(),
        Some("/plain")
    );

    ox_sys::set_env("OXVIM_TEST_SET_EXPAND", "/expanded");
    exec.execute_script(
        &editor,
        "<set>",
        "set runtimepath=$OXVIM_TEST_SET_EXPAND,${OXVIM_TEST_SET_EXPAND}/after\nlet g:expanded = &runtimepath",
    )
    .unwrap();
    assert_eq!(
        global_string(exec.scope(), "expanded").as_deref(),
        Some("/expanded,/expanded/after")
    );

    // An unset variable stays literal, like upstream vim_getenv returning
    // NULL (option_expand leaves the text unchanged).
    exec.execute_script(
        &editor,
        "<set>",
        "set runtimepath=$OXVIM_TEST_UNSET_VAR/x\nlet g:literal = &runtimepath",
    )
    .unwrap();
    assert_eq!(
        global_string(exec.scope(), "literal").as_deref(),
        Some("$OXVIM_TEST_UNSET_VAR/x")
    );
}

/// `let $VAR = value` changes the *process* environment, so a child process
/// sees it.
///
/// `ex_let_env` (`eval/vars.c`:1349-1351) assigns through `vim_setenv_ext`,
/// which is `os_setenv`. Recording the value only in the script scope is not a
/// smaller version of that, it is a different behavior with teeth:
/// `setup.vim:115` sandboxes the home directory with
/// `let $HOME = expand(getcwd() . '/XfakeHOME')`, and `runtest.vim:472` cleans
/// up with `call system('rm -rf  ' .. file)`. A file named `Xdir ~ dir`
/// word-splits there, so the shell expands `~` against the child's HOME. With
/// the assignment kept out of the environment the child inherits the real home
/// directory and the cleanup deletes it.
///
/// The child here is the shell `system()` uses, and HOME is a throwaway path
/// that is never written to.
#[cfg(unix)]
#[test]
fn let_env_assignment_reaches_child_processes() {
    let editor = TestEditorAccess::new(Editor::new());
    let _guard = crate::lock_process_state();
    let _env = crate::test_guard::EnvGuard::new(&["HOME"]);
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let sandbox = std::env::temp_dir().join(format!("ox-editor-fakehome-{}", std::process::id()));
    let sandbox = sandbox.to_string_lossy().into_owned();

    exec.execute_script(
        &editor,
        "<let-env>",
        &format!(
            "let $HOME = '{sandbox}'\n\
             let g:vim_side = $HOME\n\
             let g:child_side = substitute(system('printf %s \"$HOME\"'), '\\n', '', 'g')\n\
             let g:child_tilde = substitute(system('printf %s ~'), '\\n', '', 'g')"
        ),
    )
    .unwrap();

    let vim_side = global_string(exec.scope(), "vim_side");
    let child_side = global_string(exec.scope(), "child_side");
    let child_tilde = global_string(exec.scope(), "child_tilde");

    assert_eq!(vim_side.as_deref(), Some(sandbox.as_str()));
    assert_eq!(child_side.as_deref(), Some(sandbox.as_str()));
    // The shell's own `~` is the sandbox too, which is the expansion that
    // `rm -rf ~` in a suite cleanup lands on.
    assert_eq!(child_tilde.as_deref(), Some(sandbox.as_str()));
}

/// Windows counterpart: `cmd.exe` expands `%VAR%` in the command line, so the
/// same `let $HOME` reaches the child through the default `cmd /s /c` shell.
#[cfg(windows)]
#[test]
fn let_env_assignment_reaches_child_processes_on_windows() {
    let editor = TestEditorAccess::new(Editor::new());
    let _guard = crate::lock_process_state();
    let _env = crate::test_guard::EnvGuard::new(&["HOME"]);
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let sandbox = std::env::temp_dir().join(format!("ox-editor-fakehome-{}", std::process::id()));
    let sandbox = sandbox.to_string_lossy().into_owned();

    exec.execute_script(
        &editor,
        "<let-env>",
        &format!(
            "let $HOME = '{sandbox}'\n\
             let g:vim_side = $HOME\n\
             let g:child_side = substitute(system('echo %HOME%'), '\\r\\|\\n', '', 'g')"
        ),
    )
    .unwrap();

    assert_eq!(
        global_string(exec.scope(), "vim_side").as_deref(),
        Some(sandbox.as_str())
    );
    assert_eq!(
        global_string(exec.scope(), "child_side").as_deref(),
        Some(sandbox.as_str())
    );
}

/// `unlet $VAR` is the process-wide unset (`vim_unsetenv_ext`), so a child no
/// longer sees it either.
#[cfg(unix)]
#[test]
fn unlet_env_removes_the_variable_from_child_processes() {
    let editor = TestEditorAccess::new(Editor::new());
    let _guard = crate::lock_process_state();
    let _env = crate::test_guard::EnvGuard::new(&["OXVIM_TEST_UNLET"]);
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_script(
        &editor,
        "<unlet-env>",
        "let $OXVIM_TEST_UNLET = 'present'\n\
         let g:before = substitute(system('printf %s \"$OXVIM_TEST_UNLET\"'), '\\n', '', 'g')\n\
         unlet $OXVIM_TEST_UNLET\n\
         let g:after = substitute(system('printf %s \"$OXVIM_TEST_UNLET\"'), '\\n', '', 'g')\n\
         let g:read_back = $OXVIM_TEST_UNLET",
    )
    .unwrap();

    assert_eq!(
        global_string(exec.scope(), "before").as_deref(),
        Some("present")
    );
    assert_eq!(global_string(exec.scope(), "after").as_deref(), Some(""));
    assert_eq!(
        global_string(exec.scope(), "read_back").as_deref(),
        Some("")
    );
    assert!(std::env::var_os("OXVIM_TEST_UNLET").is_none());
}
#[test]
fn colorscheme_sources_runtime_file_then_fires_matching_autocmd() {
    let io = MemoryFileIO::new();
    io.insert(
        "/first/colors/sample.vim",
        "highlight Sample guifg=blue\nlet g:scheme_body = 1",
    );
    io.insert("/second/colors/sample.vim", "let g:wrong_runtime_root = 1");
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.scripts_mut()
        .add_runtime_root(RuntimeRoot::new(PathBuf::from("/first")));
    exec.scripts_mut()
        .add_runtime_root(RuntimeRoot::new(PathBuf::from("/second")));
    exec.execute_line(
        &editor,
        "autocmd ColorScheme sample let g:event_colors_name = g:colors_name",
    )
    .unwrap();

    exec.execute_line(&editor, "colorscheme sample").unwrap();

    assert_eq!(global_number(exec.scope(), "scheme_body"), Some(1));
    assert_eq!(global_number(exec.scope(), "wrong_runtime_root"), None);
    assert_eq!(
        global_string(exec.scope(), "colors_name").as_deref(),
        Some("sample")
    );
    assert_eq!(
        global_string(exec.scope(), "event_colors_name").as_deref(),
        Some("sample")
    );
    assert_eq!(editor.editor().highlights()["Sample"]["guifg"], "blue");
}

#[test]
fn colorscheme_missing_runtime_file_is_e185_without_state_or_event() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.scripts_mut()
        .add_runtime_root(RuntimeRoot::new(PathBuf::from("/rt")));
    exec.execute_line(&editor, "let g:colors_name = 'before'")
        .unwrap();
    exec.execute_line(
        &editor,
        "autocmd ColorScheme * let g:unexpected_colorscheme_event = 1",
    )
    .unwrap();

    let error = exec
        .execute_line(&editor, "colorscheme missing")
        .unwrap_err();

    assert_eq!(error_code(&error), "E185");
    assert!(
        error
            .to_string()
            .contains("Cannot find color scheme 'missing'")
    );
    assert_eq!(
        global_string(exec.scope(), "colors_name").as_deref(),
        Some("before")
    );
    assert_eq!(
        global_number(exec.scope(), "unexpected_colorscheme_event"),
        None
    );
}

#[derive(Default)]
struct ColorschemeLua {
    callback_colors_name: RefCell<Option<String>>,
}

impl LuaExec for ColorschemeLua {
    fn execute_chunk(&self, _code: &str, _args: Vec<Object>) -> Result<Object, LuaExecError> {
        Ok(Object::Nil)
    }

    fn execute_file(&self, _path: &Path) -> Result<(), LuaExecError> {
        Ok(())
    }

    fn invoke_callback(
        &self,
        _reference: usize,
        args: Vec<Object>,
    ) -> Result<Object, LuaExecError> {
        // The ColorScheme autocmd passes one dict (`file` carries the new
        // colorscheme name, per `apply_autocmds` building the event table).
        *self.callback_colors_name.borrow_mut() = args.first().and_then(|value| match value {
            Object::Dict(entries) => entries
                .iter()
                .find(|(key, _)| key.as_bytes() == b"file")
                .and_then(|(_, value)| match value {
                    Object::String(value) => Some(value.to_string_lossy().into_owned()),
                    _ => None,
                }),
            _ => None,
        });
        Ok(Object::Nil)
    }
}

#[test]
fn colorscheme_lua_autocmd_observes_and_preserves_new_global_name() {
    let io = MemoryFileIO::new();
    io.insert("/rt/colors/luaonly.lua", "");
    let host = Rc::new(ColorschemeLua::default());
    let editor = TestEditorAccess::new(Editor::new());
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::ColorScheme],
            "luaonly",
            &AutocmdKind::LuaCallback(7),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let mut exec = ExExecutor::with_io(io);
    exec.scripts_mut()
        .add_runtime_root(RuntimeRoot::new(PathBuf::from("/rt")));
    exec.set_lua_exec(host.clone());

    exec.execute_line(&editor, "colorscheme luaonly").unwrap();

    assert_eq!(
        host.callback_colors_name.borrow().as_deref(),
        Some("luaonly")
    );
    assert_eq!(
        global_string(exec.scope(), "colors_name").as_deref(),
        Some("luaonly")
    );
}

#[test]
fn job_callbacks_bind_the_options_dictionary_as_self() {
    let editor = TestEditorAccess::new(editor_with_lines(&[""]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let _guard = crate::lock_process_state();
    script(
        &mut exec,
        &editor,
        "let s:logger = {'events': []}\nfunction s:logger.on_stdout(id, data, event)\ncall add(self.events, a:event)\nendfunction\nlet s:logger.on_stderr = s:logger.on_stdout\nfunction s:logger.on_exit(id, data, event)\ncall add(self.events, a:event)\nendfunction\nlet g:job = jobstart(['sh', '-c', 'printf job-output'], s:logger)\nlet g:pid_ok = jobpid(g:job) > 0\nlet g:statuses = jobwait([g:job], 2000)\nlet g:exit_code = g:statuses[0]\nlet g:event_count = len(s:logger.events)",
    );
    assert_eq!(global_number(exec.scope(), "pid_ok"), Some(1));
    assert_eq!(global_number(exec.scope(), "exit_code"), Some(0));
    assert!(global_number(exec.scope(), "event_count").is_some_and(|count| count >= 2));
}

/// `ExExecutor::call_builtin` is the entry point the Lua `vim.fn` bridge comes
/// in through. It used to forward straight into a job-only dispatcher that
/// served five names -- `jobstop`, `jobpid`, `chansend`/`jobsend`, `jobwait` --
/// and ended in a bare `unreachable!()`, so every other name reached that
/// panic. The rows below are one per class of name the audit found behind it,
/// each arranged to answer differently if only its own route were wrong:
/// a name the old dispatcher did serve, the two Process-family names it did
/// not, an editor-stateful name from another family, a typval-only name, a
/// regex-backed typval-only name (which also proves the host carries a regex
/// engine rather than `Builtins::without_regex`), and an unknown name, which
/// must raise `E117` instead of aborting the process.
#[cfg(unix)]
#[test]
fn call_builtin_serves_every_family_instead_of_panicking_outside_the_job_arms() {
    let editor = TestEditorAccess::new(editor_with_lines(&["alpha"]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let _guard = crate::lock_process_state();
    let call =
        |exec: &mut ExExecutor<MemoryFileIO>,
         editor: &TestEditorAccess,
         name: &str,
         args: Vec<Typval>| { exec.call_builtin(editor, &OxStr::from(name), args) };

    // Served before and after: an unknown job id waits to -3.
    assert_eq!(
        call(
            &mut exec,
            &editor,
            "jobwait",
            vec![Typval::list(vec![Typval::Number(9_999)])]
        )
        .unwrap(),
        Typval::list(vec![Typval::Number(-3)]),
    );

    // Process family, no arm before: jobstart returns a live channel id, and
    // the job it started reaches exit status 0.
    let job = call(
        &mut exec,
        &editor,
        "jobstart",
        vec![Typval::list(vec![Typval::String(OxStr::from("true"))])],
    )
    .unwrap();
    let Typval::Number(id) = job else {
        panic!("jobstart did not answer a channel id: {job:?}")
    };
    assert!(id > 0, "jobstart answered {id}");
    assert_eq!(
        call(
            &mut exec,
            &editor,
            "jobwait",
            vec![
                Typval::list(vec![Typval::Number(id)]),
                Typval::Number(5_000)
            ]
        )
        .unwrap(),
        Typval::list(vec![Typval::Number(0)]),
    );

    // Process family, no arm before: system() runs through 'shell'.
    assert_eq!(
        call(
            &mut exec,
            &editor,
            "system",
            vec![Typval::String(OxStr::from("printf ok"))]
        )
        .unwrap(),
        Typval::String(OxStr::from("ok")),
    );

    // Another editor-stateful family entirely.
    assert_eq!(
        call(
            &mut exec,
            &editor,
            "bufnr",
            vec![Typval::String(OxStr::from("%"))]
        )
        .unwrap(),
        Typval::Number(1),
    );

    // Typval-only, no editor state.
    assert_eq!(
        call(
            &mut exec,
            &editor,
            "printf",
            vec![
                Typval::String(OxStr::from("%d-%s")),
                Typval::Number(7),
                Typval::String(OxStr::from("x"))
            ]
        )
        .unwrap(),
        Typval::String(OxStr::from("7-x")),
    );

    // Regex-backed typval-only: `Builtins::without_regex()` answers E54 here.
    assert_eq!(
        call(
            &mut exec,
            &editor,
            "substitute",
            vec![
                Typval::String(OxStr::from("aXbXc")),
                Typval::String(OxStr::from("X")),
                Typval::String(OxStr::from("-")),
                Typval::String(OxStr::from("g")),
            ]
        )
        .unwrap(),
        Typval::String(OxStr::from("a-b-c")),
    );

    // An unknown name is an error, not a panic and not an abort.
    let error = call(&mut exec, &editor, "nosuchbuiltin", Vec::new()).unwrap_err();
    assert!(error.to_string().contains("nosuchbuiltin"), "{error}");
}
#[cfg(unix)]
#[test]
fn jobstart_pty_allocates_terminal_buffer_and_records_pty() {
    let editor = TestEditorAccess::new(editor_with_lines(&["alpha"]));
    let _guard = crate::lock_process_state();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let call =
        |exec: &mut ExExecutor<MemoryFileIO>,
         editor: &TestEditorAccess,
         name: &str,
         args: Vec<Typval>| { exec.call_builtin(editor, &OxStr::from(name), args) };

    let options = Typval::dict(vec![(OxStr::from("pty"), Typval::Number(1))]);
    let job = call(
        &mut exec,
        &editor,
        "jobstart",
        vec![
            Typval::list(vec![
                Typval::String(OxStr::from("sh")),
                Typval::String(OxStr::from("-c")),
                Typval::String(OxStr::from("true")),
            ]),
            options,
        ],
    )
    .unwrap();
    let Typval::Number(id) = job else {
        panic!("jobstart did not answer a channel id: {job:?}")
    };
    assert!(id > 0, "jobstart answered {id}");

    {
        let ed = editor.editor();
        let info = ed
            .terminal_channel(u64::try_from(id).expect("positive channel id must fit in u64"))
            .expect("pty job must allocate a terminal channel");
        let buffer = ed.buffer(info.buffer).expect("terminal buffer must exist");
        assert_eq!(buffer.name().to_string_lossy(), "");
        // The slave device name is `/dev/pts/N` on Linux, `/dev/ttysNNN` on
        // macOS — either is a real pty path.
        assert!(
            info.pty.as_ref().is_some_and(|pty| {
                pty.starts_with("/dev/pts/") || pty.starts_with("/dev/ttys")
            }),
            "pty slave path must be real pts, got {:?}",
            info.pty
        );
    }

    call(
        &mut exec,
        &editor,
        "jobwait",
        vec![
            Typval::list(vec![Typval::Number(id)]),
            Typval::Number(5_000),
        ],
    )
    .unwrap();
}
#[cfg(unix)]
#[test]
fn terminal_command_marks_the_buffer_and_sleep_pumps_its_output() {
    let _guard = crate::lock_process_state();
    let editor = TestEditorAccess::new(editor_with_lines(&["alpha"]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "terminal echo terminal-marker\nsleep 500m\n",
    );

    let ed = editor.editor();
    let buffer = ed.current_buffer().expect("terminal buffer is current");
    let state = ed.buffer(buffer).expect("terminal buffer must exist");
    // `terminal_open` marks the buffer before any output arrives
    // (terminal.c:608), so 'buftype' reads "terminal" on the first frame.
    assert_eq!(
        ed.options().get_buffer(buffer, "buftype").unwrap(),
        &crate::options::OptionValue::String("terminal".to_owned())
    );
    assert!(
        state.name().to_string_lossy().starts_with("term://"),
        "terminal buffer name: {}",
        state.name().to_string_lossy()
    );
    // `:sleep` pumps the job reactor, so the child's output reaches the
    // emulator while the script waits. A bare thread sleep would leave the
    // screen empty and the buffer blank.
    let text = {
        let text = state.text().expect("terminal text is readable");
        (1..=text.line_count())
            .map(|lnum| {
                String::from_utf8_lossy(&text.line(lnum).expect("row is in range")).into_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        text.contains("terminal-marker"),
        "terminal buffer did not receive the child's output: {text:?}"
    );
}

#[cfg(unix)]
#[test]
fn pty_output_reaches_terminal_buffer_after_chansend() {
    let _guard = crate::lock_process_state();
    let editor = TestEditorAccess::new(editor_with_lines(&["alpha"]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let call =
        |exec: &mut ExExecutor<MemoryFileIO>,
         editor: &TestEditorAccess,
         name: &str,
         args: Vec<Typval>| { exec.call_builtin(editor, &OxStr::from(name), args) };

    let options = Typval::dict(vec![(OxStr::from("pty"), Typval::Number(1))]);
    let job = call(
        &mut exec,
        &editor,
        "jobstart",
        vec![
            Typval::list(vec![
                Typval::String(OxStr::from("sh")),
                Typval::String(OxStr::from("-c")),
                Typval::String(OxStr::from("read x; echo $x")),
            ]),
            options,
        ],
    )
    .unwrap();
    let Typval::Number(id) = job else {
        panic!("jobstart did not answer a channel id: {job:?}")
    };

    call(
        &mut exec,
        &editor,
        "chansend",
        vec![
            Typval::Number(id),
            Typval::String(OxStr::from("hello from pty\n")),
        ],
    )
    .unwrap();

    call(
        &mut exec,
        &editor,
        "jobwait",
        vec![
            Typval::list(vec![Typval::Number(id)]),
            Typval::Number(5_000),
        ],
    )
    .unwrap();

    let ed = editor.editor();
    let info = ed
        .terminal_channel(u64::try_from(id).expect("channel id must fit in u64"))
        .expect("pty job must keep its terminal channel");
    let buffer = ed.buffer(info.buffer).expect("terminal buffer must exist");
    let bytes = buffer.text().unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("hello from pty"),
        "terminal buffer should contain echoed PTY output, got {text:?}"
    );
}

#[test]
fn script_local_calls_inside_persisted_functions_use_defining_sid() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "plugin.vim",
        "function! s:Helper()\nlet g:called = 17\nendfunction\nfunction! Entry()\ncall s:Helper()\nendfunction",
    )
    .unwrap();

    exec.execute_line(&editor, "call Entry()").unwrap();
    assert_eq!(global_number(exec.scope(), "called"), Some(17));
}

#[test]
fn same_script_local_function_name_stays_isolated_after_source_returns() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "one.vim",
        "function! s:Helper()\nlet g:which = 1\nendfunction\nfunction! One()\ncall s:Helper()\nendfunction",
    )
    .unwrap();
    exec.execute_script(
        &editor,
        "two.vim",
        "function! s:Helper()\nlet g:which = 2\nendfunction\nfunction! Two()\ncall s:Helper()\nendfunction",
    )
    .unwrap();

    exec.execute_line(&editor, "call One()").unwrap();
    assert_eq!(global_number(exec.scope(), "which"), Some(1));
    exec.execute_line(&editor, "call Two()").unwrap();
    assert_eq!(global_number(exec.scope(), "which"), Some(2));
}

#[test]
fn nested_finish_returns_control_to_sourcing_caller() {
    let io = MemoryFileIO::new();
    io.insert(
        "/inner.vim",
        "let g:inner_before = 1\nfinish\nlet g:inner_after = 1",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &editor,
        "/outer.vim",
        "source /inner.vim\nlet g:outer_after = 1",
    )
    .unwrap();

    assert_eq!(global_number(exec.scope(), "inner_before"), Some(1));
    assert_eq!(global_number(exec.scope(), "inner_after"), None);
    assert_eq!(global_number(exec.scope(), "outer_after"), Some(1));
}

#[test]
fn finish_outside_sourced_script_is_e168() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    assert_eq!(
        error_code(&exec.execute_line(&editor, "finish").unwrap_err()),
        "E168"
    );
}

#[test]
fn exists_reports_editor_options_functions_commands_and_autocmds() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "exists.vim",
        "function! s:Local()\nendfunction\naugroup ExistGroup\nautocmd BufEnter *.rs let g:event_fired = 1\naugroup END\nlet g:opt = exists('&number')\nlet g:short_opt = exists('+nu')\nlet g:func = exists('*s:Local')\nlet g:command = exists(':set')\nlet g:abbrev = exists(':se')\nlet g:event = exists('##BufEnter')\nlet g:group = exists('#ExistGroup')\nlet g:registered = exists('#ExistGroup#BufEnter')\nlet g:missing = exists('#ExistGroup#BufLeave')\nlet g:update_command = exists(':update')",
    )
    .unwrap();

    for name in ["opt", "short_opt", "func", "event", "group", "registered"] {
        assert_eq!(global_number(exec.scope(), name), Some(1), "{name}");
    }
    assert_eq!(global_number(exec.scope(), "command"), Some(2));
    assert_eq!(global_number(exec.scope(), "update_command"), Some(2));
    assert_eq!(global_number(exec.scope(), "abbrev"), Some(1));
    assert_eq!(global_number(exec.scope(), "missing"), Some(0));
}

/// `execute_script` restores the caller's current augroup when the script
/// errors, so a later caller `:autocmd` still lands in the caller's group.
#[test]
fn execute_script_restores_caller_augroup_after_error() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&editor, "augroup Caller").unwrap();

    let error = exec
        .execute_script(
            &editor,
            "broken.vim",
            "augroup FromScript\nunlet g:missing\n",
        )
        .unwrap_err();
    assert_eq!(error_code(&error), "E108");
    exec.execute_line(&editor, "autocmd User after let g:after = 1")
        .unwrap();
    exec.execute_line(&editor, "augroup END").unwrap();
    let caller = editor.editor().autocmds().group("Caller").unwrap();
    let caller_definitions = editor.editor().autocmds().query(&AutocmdFilter {
        group: Some(caller),
        ..AutocmdFilter::default()
    });
    let names: Vec<&str> = caller_definitions
        .iter()
        .map(|definition| definition.pattern.as_str())
        .collect();
    assert_eq!(names, ["after"]);
}

#[cfg(unix)]
#[test]
fn system_builtin_captures_stdout_and_exit_status() {
    let editor = TestEditorAccess::new(Editor::new());
    let _guard = crate::lock_process_state();
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "let g:out = system('printf oxvim')")
        .unwrap();
    assert!(matches!(
        exec.scope().global.iter().find(|(key, _)| key.as_bytes() == b"out"),
        Some((_, Typval::String(value))) if value.as_bytes() == b"oxvim"
    ));
    assert!(matches!(
        exec.scope()
            .vim
            .iter()
            .find(|(key, _)| key.as_bytes() == b"shell_error"),
        Some((_, Typval::Number(0)))
    ));

    exec.execute_line(&editor, "call system('exit 7')").unwrap();
    assert!(matches!(
        exec.scope()
            .vim
            .iter()
            .find(|(key, _)| key.as_bytes() == b"shell_error"),
        Some((_, Typval::Number(7)))
    ));
}

#[cfg(unix)]
#[test]
fn systemlist_uses_job_channels_for_shell_and_argv_forms() {
    let editor = TestEditorAccess::new(Editor::new());
    // The child shell inherits the process cwd.  Cwd-mutating tests
    // (`:cd` into a tempdir, then deleting it) can leave the process
    // standing in a removed directory under parallel execution, so the
    // child emits `sh: 0: getcwd() failed: No such file or directory` on
    // stderr — which `os_system` merges into stdout as a spurious line.
    // Acquiring the crate-wide guard serializes this test against every
    // cwd mutator, keeping it a real job-channel integration test that
    // observes a valid process cwd.  Neovim itself returns the getcwd
    // diagnostic in a deleted cwd, so suppressing it in production would
    // diverge from the spec; the isolation belongs here, not in the
    // implementation.
    let _guard = crate::lock_process_state();
    let mut exec = ExExecutor::new();

    exec.execute_line(
        &editor,
        "let g:shell = systemlist('printf \"hello\\n\\n\"')",
    )
    .unwrap();
    exec.execute_line(
        &editor,
        "let g:argv = systemlist(['printf', 'one\\ntwo\\n'])",
    )
    .unwrap();
    exec.execute_line(&editor, "let g:kept = systemlist('printf \"x\\n\"', '', 1)")
        .unwrap();
    exec.execute_line(&editor, "let g:failed = systemlist('printf bad; exit 7')")
        .unwrap();

    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"shell", 0)
            .unwrap(),
        &Typval::list(vec![
            Typval::String(OxStr::from("hello")),
            Typval::String(OxStr::from(""))
        ])
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"argv", 0)
            .unwrap(),
        &Typval::list(vec![
            Typval::String(OxStr::from("one")),
            Typval::String(OxStr::from("two"))
        ])
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"kept", 0)
            .unwrap(),
        &Typval::list(vec![
            Typval::String(OxStr::from("x")),
            Typval::String(OxStr::from(""))
        ])
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"failed", 0)
            .unwrap(),
        &Typval::list(vec![Typval::String(OxStr::from("bad"))])
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Vim, b"shell_error", 0)
            .unwrap(),
        &Typval::Number(7)
    );
}

#[test]
fn window_screen_and_state_builtins_use_editor_state() {
    let editor = TestEditorAccess::new(Editor::new());
    let buffer = editor.editor_mut().create_buffer(true).unwrap();
    let _tabpage = editor
        .editor_mut()
        .create_tabpage(buffer, Geometry::new(0, 0, 20, 6).unwrap())
        .unwrap();
    let window = editor.editor().current_window().unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &editor,
        "builtins.vim",
        concat!(
            "call setline(1, '口x')\n",
            "let b:answer = 42\n",
            "let g:id = win_getid()\n",
            "let g:width = winwidth(0)\n",
            "let g:height = winheight(0)\n",
            "let g:attr = screenattr(1, 1)\n",
            "let g:char = screenchar(1, 1)\n",
            "let g:chars = screenchars(1, 1)\n",
            "let g:text = screenstring(1, 1)\n",
            "let g:missing = screenchar(-1, -1)\n",
            "let g:bufvar = getbufvar(bufnr('%'), 'answer')\n",
            "let g:command = fullcommand('res')\n",
            "let g:event = eventhandler()\n",
        ),
    )
    .unwrap();

    assert_eq!(global_number(exec.scope(), "id"), Some(i64::from(window)));
    assert_eq!(global_number(exec.scope(), "width"), Some(20));
    assert_eq!(global_number(exec.scope(), "height"), Some(6));
    assert_eq!(global_number(exec.scope(), "attr"), Some(0));
    assert_eq!(
        global_number(exec.scope(), "char"),
        Some(i64::from('口' as u32))
    );
    assert_eq!(global_number(exec.scope(), "missing"), Some(-1));
    assert_eq!(global_number(exec.scope(), "bufvar"), Some(42));
    assert_eq!(
        global_string(exec.scope(), "command").as_deref(),
        Some("resize")
    );
    assert_eq!(global_number(exec.scope(), "event"), Some(0));
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"chars", 0)
            .unwrap(),
        &Typval::list(vec![Typval::Number(i64::from('口' as u32))])
    );
    assert_eq!(global_string(exec.scope(), "text").as_deref(), Some("口"));
}

/// Blank screen cells are spaces, not holes: upstream's default grid answers
/// every in-range coordinate (`eval/funcs.c:5711-5729`), so a 24x80 screen
/// reports 32 for window rows past the buffer's last line and for grid rows
/// no window covers, and -1 only outside the grid. `Screenline(&lines - 1)`
/// in the oldtest suite feeds those cells straight into `nr2char()`, where
/// a -1 surfaces as `E5070: Character number must not be less than zero`.
#[test]
fn screen_builtins_answer_the_whole_grid_with_blanks() {
    let editor = TestEditorAccess::new(Editor::new());
    let buffer = editor.editor_mut().create_buffer(true).unwrap();
    let _tabpage = editor
        .editor_mut()
        .create_tabpage(buffer, Geometry::new(0, 0, 20, 6).unwrap())
        .unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &editor,
        "builtins.vim",
        concat!(
            "set lines=24 columns=80\n",
            "call setline(1, 'ab')\n",
            // Inside the window, past the end of the line and past the last
            // buffer line: blanks, never -1.
            "let g:past_eol = screenchar(1, 3)\n",
            "let g:past_eof = screenchar(2, 1)\n",
            "let g:window_edge = screenchar(6, 20)\n",
            // In-grid rows no window covers (status-line and message rows):
            // blanks with default attributes.
            "let g:uncovered = screenchar(23, 1)\n",
            "let g:last_cell = screenchar(24, 80)\n",
            "let g:uncovered_attr = screenattr(23, 1)\n",
            "let g:uncovered_string = screenstring(23, 1)\n",
            "let g:uncovered_chars = screenchars(23, 1)\n",
            // Outside the grid: no cell at all.
            "let g:beyond_rows = screenchar(25, 1)\n",
            "let g:beyond_cols = screenchar(1, 81)\n",
            "let g:row_zero = screenchar(0, 1)\n",
            "let g:col_zero = screenchar(1, 0)\n",
            "let g:beyond_attr = screenattr(25, 1)\n",
            "let g:beyond_string = screenstring(25, 1)\n",
            "let g:beyond_chars = screenchars(25, 1)\n",
        ),
    )
    .unwrap();

    assert_eq!(global_number(exec.scope(), "past_eol"), Some(32));
    assert_eq!(global_number(exec.scope(), "past_eof"), Some(32));
    assert_eq!(global_number(exec.scope(), "window_edge"), Some(32));
    assert_eq!(global_number(exec.scope(), "uncovered"), Some(32));
    assert_eq!(global_number(exec.scope(), "last_cell"), Some(32));
    assert_eq!(global_number(exec.scope(), "uncovered_attr"), Some(0));
    assert_eq!(
        global_string(exec.scope(), "uncovered_string").as_deref(),
        Some(" ")
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"uncovered_chars", 0)
            .unwrap(),
        &Typval::list(vec![Typval::Number(32)])
    );
    assert_eq!(global_number(exec.scope(), "beyond_rows"), Some(-1));
    assert_eq!(global_number(exec.scope(), "beyond_cols"), Some(-1));
    assert_eq!(global_number(exec.scope(), "row_zero"), Some(-1));
    assert_eq!(global_number(exec.scope(), "col_zero"), Some(-1));
    assert_eq!(global_number(exec.scope(), "beyond_attr"), Some(-1));
    assert_eq!(
        global_string(exec.scope(), "beyond_string").as_deref(),
        Some("")
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"beyond_chars", 0)
            .unwrap(),
        &Typval::list(vec![])
    );
}

#[test]
fn higher_order_callbacks_preserve_editor_host() {
    let editor = TestEditorAccess::new(Editor::new());
    let buffer = editor.editor_mut().create_buffer(true).unwrap();
    editor
        .editor_mut()
        .create_tabpage(buffer, Geometry::new(0, 0, 20, 6).unwrap())
        .unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &editor,
        "callbacks.vim",
        concat!(
            "call setline(1, 'abc')\n",
            "let g:mapped = join(map(range(1, 3), 'screenstring(1, v:val)'), '')\n",
            "let g:filtered = join(filter(['alpha', 'beta'], 'v:val =~# \"^a\"'), ',')\n",
        ),
    )
    .unwrap();

    assert_eq!(
        global_string(exec.scope(), "mapped").as_deref(),
        Some("abc")
    );
    assert_eq!(
        global_string(exec.scope(), "filtered").as_deref(),
        Some("alpha")
    );
}

#[test]
fn expand_builtin_reads_current_buffer_and_preserves_paths() {
    let editor = TestEditorAccess::new(Editor::new());
    let buffer = editor.editor_mut().create_buffer(true).unwrap();
    editor
        .editor_mut()
        .buffer_mut(buffer)
        .unwrap()
        .set_name("test_functions.vim".into());
    editor
        .editor_mut()
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "let g:name = expand('%')")
        .unwrap();
    exec.execute_line(&editor, "let g:path = expand('/tmp/build')")
        .unwrap();

    assert!(matches!(
        exec.scope().global.iter().find(|(key, _)| key.as_bytes() == b"name"),
        Some((_, Typval::String(value))) if value.as_bytes() == b"test_functions.vim"
    ));
    assert!(matches!(
        exec.scope().global.iter().find(|(key, _)| key.as_bytes() == b"path"),
        Some((_, Typval::String(value))) if value.as_bytes() == b"/tmp/build"
    ));
}

/// `expand()` applies a `:`-modifier chain after a special token the way
/// upstream `f_expand` does: `eval_vars` resolves the token base
/// (`ex_docmd.c:7551`), then `modify_fname` (eval/fs.c:69) — here
/// `ox_eval::apply_filename_modifiers` — eats the rest. This is the
/// termdebug shape: `expand('%:p')` inside the `-break-insert` command
/// (`test_plugin_termdebug.vim` `Test_termdebug_break_command_builder`).
///
/// The buffer name is absolute and non-existent so `:p` is deterministic:
/// `absolute_name` only consults the filesystem (`fs::canonicalize`) for
/// paths that exist. An empty base short-circuits to "" — upstream's
/// `eval_vars` marks the result invalid and `f_expand` returns "".
#[test]
fn expand_builtin_applies_filename_modifiers_to_special_tokens() {
    let editor = TestEditorAccess::new(Editor::new());
    let buffer = editor.editor_mut().create_buffer(true).unwrap();
    let directory = std::env::temp_dir().join(format!("ox-expand-mod-{}", std::process::id()));
    let stored = directory.join("XTD_break_cmd.c");
    let stored = stored.to_string_lossy().into_owned();
    editor
        .editor_mut()
        .buffer_mut(buffer)
        .unwrap()
        .set_name(OxStr::from(stored.as_str()));
    editor
        .editor_mut()
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "let g:exact = expand('%')")
        .unwrap();
    exec.execute_line(&editor, "let g:absolute = expand('%:p')")
        .unwrap();
    exec.execute_line(&editor, "let g:head = expand('%:p:h')")
        .unwrap();
    exec.execute_line(&editor, "let g:tail = expand('%:t')")
        .unwrap();

    assert_eq!(
        global_string(exec.scope(), "exact").as_deref(),
        Some(stored.as_str())
    );
    assert_eq!(
        global_string(exec.scope(), "absolute").as_deref(),
        Some(stored.as_str())
    );
    let parent = directory.to_string_lossy().into_owned();
    assert_eq!(
        global_string(exec.scope(), "head").as_deref(),
        Some(parent.as_str())
    );
    assert_eq!(
        global_string(exec.scope(), "tail").as_deref(),
        Some("XTD_break_cmd.c")
    );

    // An unnamed buffer has an empty base: both the exact token and the
    // modifier chain yield "". `<afile>` outside an autocommand is the
    // non-path token with the same empty-base rule.
    let editor = TestEditorAccess::new(Editor::new());
    let buffer = editor.editor_mut().create_buffer(true).unwrap();
    editor
        .editor_mut()
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "let g:blank = expand('%')")
        .unwrap();
    exec.execute_line(&editor, "let g:blank_p = expand('%:p')")
        .unwrap();
    exec.execute_line(&editor, "let g:afile = expand('<afile>')")
        .unwrap();
    exec.execute_line(&editor, "let g:afile_h = expand('<afile>:h')")
        .unwrap();

    assert_eq!(global_string(exec.scope(), "blank").as_deref(), Some(""));
    assert_eq!(global_string(exec.scope(), "blank_p").as_deref(), Some(""));
    assert_eq!(global_string(exec.scope(), "afile").as_deref(), Some(""));
    assert_eq!(global_string(exec.scope(), "afile_h").as_deref(), Some(""));
}

/// While a `FileReadPre` action runs, `<amatch>`, `<afile>`, `<afile>:h`, and
/// `<abuf>` resolve from the active autocmd context installed for that action.
#[test]
fn expand_builtin_resolves_autocmd_special_tokens_during_event() {
    let editor = TestEditorAccess::new(editor_with_lines(&["initial"]));
    let buffer = editor.editor().current_buffer().unwrap();
    let io = MemoryFileIO::new();
    io.insert("dir/target.txt", "content\n");
    let mut exec = ExExecutor::with_io(io);

    for (pattern, body) in [
        ("*.txt", "let g:m = expand('<amatch>')"),
        ("*.txt", "let g:f = expand('<afile>')"),
        ("*.txt", "let g:fh = expand('<afile>:h')"),
        ("*.txt", "let g:b = expand('<abuf>')"),
    ] {
        editor
            .editor_mut()
            .autocmds_mut()
            .register_legacy(
                &[Event::FileReadPre],
                pattern,
                &AutocmdKind::ExString { source: body.to_owned(), script: None },
                &AutocmdOptions::default(),
            )
            .unwrap();
    }
    exec.execute_line(&editor, "1read dir/target.txt").unwrap();

    let expected_match = std::env::current_dir()
        .unwrap()
        .join("dir/target.txt")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        global_string(exec.scope(), "m").as_deref(),
        Some(expected_match.as_str())
    );
    assert_eq!(
        global_string(exec.scope(), "f").as_deref(),
        Some("dir/target.txt")
    );
    assert_eq!(global_string(exec.scope(), "fh").as_deref(), Some("dir"));
    let expected_buf = i64::from(buffer).to_string();
    assert_eq!(
        global_string(exec.scope(), "b").as_deref(),
        Some(expected_buf.as_str())
    );
}

/// Nested `FileReadPre` actions replace then restore the outer active context,
/// and outside any event the autocmd tokens stay empty.
#[test]
fn expand_builtin_restores_autocmd_context_after_nested_event() {
    let editor = TestEditorAccess::new(editor_with_lines(&["initial"]));
    let io = MemoryFileIO::new();
    io.insert("outer.txt", "outer\n");
    io.insert("nested.txt", "nested\n");
    let mut exec = ExExecutor::with_io(io);

    for (pattern, body) in [
        ("nested.txt", "let g:inner_file = expand('<afile>')"),
        ("outer.txt", "let g:outer_pre = expand('<afile>')"),
        ("outer.txt", "1read nested.txt"),
        ("outer.txt", "let g:outer_post = expand('<afile>')"),
    ] {
        editor
            .editor_mut()
            .autocmds_mut()
            .register_legacy(
                &[Event::FileReadPre],
                pattern,
                &AutocmdKind::ExString { source: body.to_owned(), script: None },
                &AutocmdOptions::default(),
            )
            .unwrap();
    }
    exec.execute_line(&editor, "1read outer.txt").unwrap();

    assert_eq!(
        global_string(exec.scope(), "outer_pre").as_deref(),
        Some("outer.txt")
    );
    assert_eq!(
        global_string(exec.scope(), "inner_file").as_deref(),
        Some("nested.txt")
    );
    assert_eq!(
        global_string(exec.scope(), "outer_post").as_deref(),
        Some("outer.txt")
    );

    exec.execute_line(&editor, "let g:outside = expand('<afile>')")
        .unwrap();
    assert_eq!(global_string(exec.scope(), "outside").as_deref(), Some(""));
}

/// `expand()` resolves `~` and `$NAME` the way `ExpandOne` does, through
/// `expand_env_esc` (`os/env.c`).
///
/// Returning the pattern verbatim is not a smaller answer: `setup.vim`:115
/// builds its sandbox with `expand(getcwd() . '/XfakeHOME')`, and a caller that
/// hands an unexpanded `~` to a shell lets the *shell* expand it against its
/// own environment instead. HOME is a throwaway path here and nothing is
/// written to it.
#[test]
fn expand_builtin_resolves_home_and_environment_variables() {
    let editor = TestEditorAccess::new(Editor::new());
    let _guard = crate::lock_process_state();
    let buffer = editor.editor_mut().create_buffer(true).unwrap();
    editor
        .editor_mut()
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let mut exec = ExExecutor::new();
    let sandbox = std::env::temp_dir().join(format!("ox-editor-expand-{}", std::process::id()));
    let sandbox = sandbox.to_string_lossy().into_owned();
    let _env = crate::test_guard::EnvGuard::new(&["HOME", "OXVIM_TEST_EXPAND_DIR"]);
    ox_sys::set_env("HOME", &sandbox);
    ox_sys::set_env("OXVIM_TEST_EXPAND_DIR", "/sentinel");

    exec.execute_script(
        &editor,
        "<expand>",
        "let g:tilde = expand('~')\n\
         let g:tilde_path = expand('~/XfakeHOME')\n\
         let g:dollar = expand('$OXVIM_TEST_EXPAND_DIR/x')\n\
         let g:braced = expand('${OXVIM_TEST_EXPAND_DIR}/y')\n\
         let g:unset = expand('$OXVIM_TEST_EXPAND_MISSING/z')\n\
         let g:interior = expand('/keep/~/as-is')",
    )
    .unwrap();

    let values: Vec<Option<String>> = [
        "tilde",
        "tilde_path",
        "dollar",
        "braced",
        "unset",
        "interior",
    ]
    .iter()
    .map(|name| global_string(exec.scope(), name))
    .collect();

    assert_eq!(values[0].as_deref(), Some(sandbox.as_str()));
    assert_eq!(
        values[1].as_deref(),
        Some(format!("{sandbox}/XfakeHOME").as_str())
    );
    assert_eq!(values[2].as_deref(), Some("/sentinel/x"));
    assert_eq!(values[3].as_deref(), Some("/sentinel/y"));
    // An unset variable stays literal, as `vim_getenv` returning NULL leaves it.
    assert_eq!(values[4].as_deref(), Some("$OXVIM_TEST_EXPAND_MISSING/z"));
    // Only a leading `~` is a home reference.
    assert_eq!(values[5].as_deref(), Some("/keep/~/as-is"));
}

// ---------------------------------------------------------------------------
// :language (os/lang.c ex_language; oldtest test_excmd.vim Test_language_cmd)
// ---------------------------------------------------------------------------

fn vim_string(scope: &Scope, name: &str) -> Option<String> {
    scope
        .vim
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .and_then(|(_, value)| match value {
            Typval::String(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
}

#[test]
fn language_messages_sets_env_and_vim_vars() {
    let editor = TestEditorAccess::new(Editor::new());
    let _guard = crate::lock_process_state();
    let _env = crate::test_guard::EnvGuard::new(&["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"]);
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    // `lang mess C` exercises the abbreviated keyword form oldtest's
    // runtest.vim uses ("Output all messages in English").
    exec.execute_line(&editor, "lang mess C").unwrap();

    assert_eq!(vim_string(exec.scope(), "lang").as_deref(), Some("C"));
    assert_eq!(vim_string(exec.scope(), "ctype").as_deref(), Some("C"));
    assert_eq!(vim_string(exec.scope(), "lc_time").as_deref(), Some("C"));
    assert_eq!(vim_string(exec.scope(), "collate").as_deref(), Some("C"));
    assert_eq!(
        std::env::var_os("LC_ALL").as_deref(),
        Some(std::ffi::OsStr::new(""))
    );
    assert_eq!(
        std::env::var_os("LC_MESSAGES").as_deref(),
        Some(std::ffi::OsStr::new("C"))
    );
}

#[test]
fn language_without_keyword_sets_lang_and_language_env() {
    let _guard = crate::lock_process_state();
    let _env = crate::test_guard::EnvGuard::new(&["LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES"]);
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_line(&editor, "language C").unwrap();

    assert_eq!(vim_string(exec.scope(), "lang").as_deref(), Some("C"));
    assert_eq!(
        std::env::var_os("LANG").as_deref(),
        Some(std::ffi::OsStr::new("C"))
    );
    assert_eq!(
        std::env::var_os("LANGUAGE").as_deref(),
        Some(std::ffi::OsStr::new(""))
    );
    assert_eq!(
        std::env::var_os("LC_ALL").as_deref(),
        Some(std::ffi::OsStr::new(""))
    );
    assert_eq!(
        std::env::var_os("LC_MESSAGES").as_deref(),
        Some(std::ffi::OsStr::new("C"))
    );
}

#[test]
fn language_ctype_leaves_lang_and_messages_env_untouched() {
    let _guard = crate::lock_process_state();
    let _env = crate::test_guard::EnvGuard::new(&["LANG", "LC_MESSAGES", "LC_ALL", "LANGUAGE"]);
    ox_sys::set_env("LANG", "ox-language-sentinel");
    ox_sys::set_env("LC_MESSAGES", "ox-language-sentinel");
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_line(&editor, "language ctype C").unwrap();

    assert_eq!(vim_string(exec.scope(), "ctype").as_deref(), Some("C"));
    assert_eq!(
        std::env::var_os("LANG").as_deref(),
        Some(std::ffi::OsStr::new("ox-language-sentinel"))
    );
    assert_eq!(
        std::env::var_os("LC_MESSAGES").as_deref(),
        Some(std::ffi::OsStr::new("ox-language-sentinel"))
    );
}

#[test]
fn language_rejected_locale_is_e197_for_ctype_and_time() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    for keyword in ["ctype", "time"] {
        let error = exec
            .execute_line(
                &editor,
                &format!("language {keyword} non_existing_lang.bad"),
            )
            .unwrap_err();
        assert_eq!(error_code(&error), "E197");
        assert!(
            error
                .to_string()
                .contains("Cannot set language to \"non_existing_lang.bad\""),
            "{error}"
        );
    }
}

#[test]
fn language_without_name_reports_current_locale() {
    let _guard = crate::lock_process_state();
    let _env = crate::test_guard::EnvGuard::new(&["LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES"]);
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&editor, "language C").unwrap();

    exec.execute_line(&editor, "language messages").unwrap();
    {
        let ed = editor.editor();
        let last = ed.messages().last().unwrap();
        assert_eq!(
            last.content,
            Object::String(OxStr::from("Current messages language: \"C\""))
        );
    }

    exec.execute_line(&editor, "language").unwrap();
    {
        let ed = editor.editor();
        let last = ed.messages().last().unwrap();
        assert_eq!(
            last.content,
            Object::String(OxStr::from("Current language: \"C\""))
        );
    }
}

#[test]
fn redir_silent_function_pattern_lists_matching_signatures() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function Test_Alpha(required, optional = {'x': 1}) abort\nendfunction\nfunction Test_Beta(...) range\nendfunction\nfunction Other()\nendfunction",
    );

    script(
        &mut exec,
        &editor,
        "redir @q\nsilent function /^Test_\nredir END",
    );

    assert!(editor.editor().messages().is_empty());
    assert_eq!(
        register_text(&editor.editor(), 'q'),
        "function Test_Alpha(required, optional = {'x': 1}) abort\nfunction Test_Beta(...) range"
    );
}

#[test]
fn redirected_function_list_global_substitute_rewrites_every_signature() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "function Test_Alpha()\nendfunction\nfunction Test_Beta()\nendfunction\nfunction Test_Gamma()\nendfunction",
    );
    script(
        &mut exec,
        &editor,
        concat!(
            "redir @q\n",
            "silent function /^Test_\n",
            "redir END\n",
            "let g:listed = substitute(@q, 'function \\(\\k*()\\)', '\\1', 'g')"
        ),
    );
    assert_eq!(
        global_string(exec.scope(), "listed").as_deref(),
        Some("Test_Alpha()\nTest_Beta()\nTest_Gamma()")
    );
}

#[test]
fn redir_register_replaces_appends_and_keeps_unsilenced_output_visible() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_line(&editor, "let @q = 'old'").unwrap();
    exec.execute_line(&editor, "redir @q").unwrap();
    assert_eq!(register_text(&editor.editor(), 'q'), "");
    exec.execute_line(&editor, "echo 'first'").unwrap();
    assert_eq!(register_text(&editor.editor(), 'q'), "first");
    exec.execute_line(&editor, "redir END").unwrap();
    assert_eq!(register_text(&editor.editor(), 'q'), "first");
    assert_eq!(editor.editor().messages().len(), 1);

    script(
        &mut exec,
        &editor,
        "redir @q>>\nsilent echo 'second'\nredir END",
    );
    assert_eq!(register_text(&editor.editor(), 'q'), "firstsecond");
    assert_eq!(editor.editor().messages().len(), 1);

    script(
        &mut exec,
        &editor,
        "redir @w\nsilent echon 'a'\nsilent echon 'b'\nredir END",
    );
    assert_eq!(register_text(&editor.editor(), 'w'), "ab");
}

#[test]
fn redir_variable_replaces_then_appends() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_line(&editor, "let g:captured = 'old'")
        .unwrap();
    exec.execute_line(&editor, "redir => g:captured").unwrap();
    assert_eq!(global_string(exec.scope(), "captured").as_deref(), Some(""));
    exec.execute_line(&editor, "silent echo 'one'").unwrap();
    assert_eq!(global_string(exec.scope(), "captured").as_deref(), Some(""));
    exec.execute_line(&editor, "redir END").unwrap();
    assert_eq!(
        global_string(exec.scope(), "captured").as_deref(),
        Some("one")
    );

    script(
        &mut exec,
        &editor,
        "redir =>> g:captured\nsilent echo 'two'\nredir END",
    );
    assert_eq!(
        global_string(exec.scope(), "captured").as_deref(),
        Some("onetwo")
    );
}

#[test]
fn redir_file_replaces_then_appends() {
    let io = MemoryFileIO::new();
    io.insert("capture.txt", "old");
    let files = Rc::clone(&io.files);
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);

    exec.execute_line(&editor, "redir > capture.txt").unwrap();
    assert_eq!(
        files
            .borrow()
            .get(Path::new("capture.txt"))
            .map(String::as_str),
        Some("")
    );
    exec.execute_line(&editor, "silent echo 'one'").unwrap();
    assert_eq!(
        files
            .borrow()
            .get(Path::new("capture.txt"))
            .map(String::as_str),
        Some("one")
    );
    exec.execute_line(&editor, "redir END").unwrap();
    assert_eq!(
        files
            .borrow()
            .get(Path::new("capture.txt"))
            .map(String::as_str),
        Some("one")
    );
    script(
        &mut exec,
        &editor,
        "redir >> capture.txt\nsilent echo 'two'\nredir END",
    );
}

#[test]
fn nested_redir_is_e930_and_preserves_active_target() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&editor, "redir @q").unwrap();

    let error = exec.execute_line(&editor, "redir @w").unwrap_err();
    assert_eq!(error_code(&error), "E930");

    exec.execute_line(&editor, "silent echo 'still active'")
        .unwrap();
    exec.execute_line(&editor, "redir END").unwrap();
    assert_eq!(register_text(&editor.editor(), 'q'), "still active");
    assert!(editor.editor().registers().get('w').unwrap().is_none());
}

#[test]
fn function_builtin_constructs_named_and_bound_references() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "function-ref.vim",
        "let g:plain = function('extend')\nlet g:bound = function('extend', [1], {'answer': 42})",
    )
    .unwrap();
    let Typval::Funcref(plain) = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"plain", 0)
        .unwrap()
    else {
        panic!("expected plain Funcref")
    };
    assert_eq!(plain.name, OxStr::from("extend"));
    let Typval::Partial(bound) = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"bound", 0)
        .unwrap()
    else {
        panic!("expected bound Partial")
    };
    assert_eq!(bound.args, vec![Typval::Number(1)]);
    let dict = bound.dict.as_ref().unwrap().borrow();
    assert_eq!(dict.entries.len(), 1);
    assert_eq!(dict.entries[0].key, OxStr::from("answer"));
    assert_eq!(dict.entries[0].value, Typval::Number(42));
}

#[test]
fn function_builtin_reports_name_and_binding_errors() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    for (line, code) in [
        ("call function('MissingFunction')", "E700"),
        ("call function('extend', [], 1)", "E922"),
        ("call function('extend', 1)", "E923"),
    ] {
        let error = exec.execute_line(&editor, line).unwrap_err();
        assert_eq!(error_code(&error), code, "{line}");
    }
}

// eval/userfunc.c handle_defer_one 3487-3524, called from call_user_func's
// cleanup (1272) — `writefile(..., 'D')` deletes its file when the enclosing
// function returns, whatever the outcome was, and the nesting is per frame.
//
// Four cases, each isolating one part of the contract so no part can be
// dropped: the file must exist *inside* the frame (so a defer that deletes
// immediately fails), be gone *after* it (so a defer that never runs fails), an
// inner frame's defer must not take the outer frame's file with it (so a single
// shared list fails), and a frame that aborts must still delete (so running the
// deletes only on the success path fails).
#[test]
fn writefile_defer_flag_deletes_per_frame_on_return_and_on_abort() {
    let root = std::env::temp_dir().join(format!("ox-editor-defer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let base = root.display().to_string();

    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "defer.vim",
        &format!(
            "func Inner()\n\
             call writefile(['i'], '{base}/inner', 'D')\n\
             let g:inner_inside = filereadable('{base}/inner')\n\
             endfunc\n\
             func Outer()\n\
             call writefile(['o'], '{base}/outer', 'D')\n\
             call Inner()\n\
             let g:inner_after = filereadable('{base}/inner')\n\
             let g:outer_inside = filereadable('{base}/outer')\n\
             endfunc\n\
             call Outer()\n\
             let g:outer_after = filereadable('{base}/outer')\n\
             func Aborts()\n\
             call writefile(['a'], '{base}/aborted', 'D')\n\
             throw 'boom'\n\
             endfunc\n\
             try\n\
             call Aborts()\n\
             catch\n\
             let g:caught = v:exception\n\
             endtry\n\
             let g:aborted_after = filereadable('{base}/aborted')"
        ),
    )
    .unwrap();

    let flag = |name: &[u8]| exec.scope().get_scoped(ScopeKind::Global, name, 0).cloned();
    assert_eq!(
        flag(b"inner_inside"),
        Ok(Typval::Number(1)),
        "file missing inside its own frame"
    );
    assert_eq!(
        flag(b"inner_after"),
        Ok(Typval::Number(0)),
        "inner frame's defer did not run"
    );
    assert_eq!(
        flag(b"outer_inside"),
        Ok(Typval::Number(1)),
        "inner frame took the outer frame's file"
    );
    assert_eq!(
        flag(b"outer_after"),
        Ok(Typval::Number(0)),
        "outer frame's defer did not run"
    );
    assert!(
        matches!(&flag(b"caught"), Ok(Typval::String(text)) if text.to_string_lossy().contains("boom"))
    );
    assert_eq!(
        flag(b"aborted_after"),
        Ok(Typval::Number(0)),
        "an aborted frame's defer did not run"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Generic `:defer Func(args)` registers any call and fires
/// last-registered-first when the frame ends (`ex_defer_inner`,
/// `handle_defer_one`, eval/userfunc.c:3394-3524); a `Funcref` variable
/// contributes its bound arguments before the call's own, exactly like
/// `deref_func_name` supplying the partial.
#[test]
fn defer_registers_generic_calls_and_fires_last_registered_first() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "func AddSeen(val)\n\
         call add(g:order, a:val)\n\
         endfunc\n\
         func Main()\n\
         defer AddSeen('one')\n\
         defer AddSeen('two')\n\
         call add(g:order, 'body')\n\
         endfunc\n\
         let g:order = []\n\
         call Main()\n",
    );
    let order = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"order", 0)
        .cloned();
    match order {
        Ok(Typval::List(items)) => {
            let seen: Vec<String> = items
                .borrow()
                .items
                .iter()
                .filter_map(|item| match item {
                    Typval::String(text) => Some(text.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            assert_eq!(seen, ["body", "two", "one"]);
        }
        other => panic!("expected g:order list, got {other:?}"),
    }
}

/// `:defer` rejects a dictionary-bound partial (`E1300`), reports text after
/// the balanced call as trailing (`E488`), and requires a function frame
/// (`E193`); a partial without a dictionary fires with its bound arguments
/// first (test_user_func.vim `Test_defer`).
#[test]
fn defer_rejects_dict_partial_trailing_text_and_top_level() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "func OneArg(val)\n\
         endfunc\n\
         func TwoArgs(first, second)\n\
         call add(g:pargs, a:first)\n\
         call add(g:pargs, a:second)\n\
         endfunc\n\
         func Probe()\n\
         let D = function('tr', {'x': 1})\n\
         let v:errmsg = 'NONE'\n\
         silent! defer D('x')\n\
         let g:e1300 = v:errmsg\n\
         let v:errmsg = 'NONE'\n\
         silent! defer OneArg('x')->Another()\n\
         let g:earrow = v:errmsg\n\
         let v:errmsg = 'NONE'\n\
         silent! defer OneArg('x').member\n\
         let g:emember = v:errmsg\n\
         let P = function('TwoArgs', ['bound'])\n\
         defer P('own')\n\
         endfunc\n\
         let g:pargs = []\n\
         call Probe()\n\
         let v:errmsg = 'NONE'\n\
         silent! defer OneArg('x')\n\
         let g:e193 = v:errmsg\n",
    );
    let flag = |name: &[u8]| {
        exec.scope()
            .get_scoped(ScopeKind::Global, name, 0)
            .cloned()
    };
    for (name, code) in [
        ("e1300", "E1300"),
        ("earrow", "E488"),
        ("emember", "E488"),
        ("e193", "E193"),
    ] {
        match flag(name.as_bytes()) {
            Ok(Typval::String(text)) => assert!(
                text.to_string_lossy().starts_with(code),
                "{name} must report {code}, got {text:?}"
            ),
            other => panic!("expected {name} string, got {other:?}"),
        }
    }
    match flag(b"pargs") {
        Ok(Typval::List(items)) => {
            let seen: Vec<String> = items
                .borrow()
                .items
                .iter()
                .filter_map(|item| match item {
                    Typval::String(text) => Some(text.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            assert_eq!(seen, ["bound", "own"]);
        }
        other => panic!("expected g:pargs list, got {other:?}"),
    }
}

/// `defer()` on a lambda fires through the closure registry captured in its
/// Funcref — a `<lambda>N` name owns no user-function entry, so a name
/// redispatch at frame end would hit `E117` instead of running the body.
/// Covers the `defer()` builtin form and the `:defer` command resolving a
/// lambda variable.
#[test]
fn defer_lambda_fires_through_its_captured_closure() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &editor,
        "func Main()\n\
         let L = {-> add(g:seen, 'command')}\n\
         call defer({-> add(g:seen, 'builtin')})\n\
         defer L()\n\
         call add(g:seen, 'body')\n\
         endfunc\n\
         let g:seen = []\n\
         call Main()\n",
    );
    match exec.scope().get_scoped(ScopeKind::Global, b"seen", 0).cloned() {
        Ok(Typval::List(items)) => {
            let seen: Vec<String> = items
                .borrow()
                .items
                .iter()
                .filter_map(|item| match item {
                    Typval::String(text) => Some(text.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            assert_eq!(seen, ["body", "command", "builtin"]);
        }
        other => panic!("expected g:seen list, got {other:?}"),
    }
}

// eval/funcs.c f_system through os_system, os/shell.c shell_build_argv 60-97 —
// a String command runs through 'shell' + 'shellcmdflag', the second argument
// is the child's standard input and its pipe is closed, and a shell that
// cannot be spawned is reported through v:shell_error rather than raised.
//
// The E677 this replaces was fatal: `test_cmdline.vim` poisons $PATH and never
// restores it, so `system()` in runtest.vim's cleanup aborted FinishTesting()
// before it wrote `messages`, losing a 45-test record.
//
// One case per part: the input argument (a `cat` that echoes it back proves
// both that the input arrives and that its pipe is closed, since `cat` would
// otherwise never exit), 'shellcmdflag' (a flag the option names and nothing
// else supplies), and an unreachable 'shell' (which must not raise and must
// report -1).
#[cfg(unix)]
#[test]
fn system_uses_the_shell_options_feeds_input_and_never_raises_on_a_bad_shell() {
    let editor = TestEditorAccess::new(Editor::new());
    let _guard = crate::lock_process_state();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "system.vim",
        "set shell=/bin/sh shellcmdflag=-c\n\
         let g:echoed = system('echo ok')\n\
         let g:fed = system('cat', '123')\n\
         let g:listed = systemlist('cat', '123')\n\
         set shellcmdflag=-cx\n\
         let g:traced = system('true')\n\
         let g:traced_error = v:shell_error\n\
         set shellcmdflag=-c\n\
         set shell=/nonexistent/ox-no-shell\n\
         let g:missing = system('echo unreachable')\n\
         let g:missing_error = v:shell_error",
    )
    .unwrap();

    let global = |name: &[u8]| exec.scope().get_scoped(ScopeKind::Global, name, 0).cloned();
    assert_eq!(global(b"echoed"), Ok(Typval::String(OxStr::from("ok\n"))));
    // The input argument reaches the child and its pipe is closed afterwards.
    assert_eq!(global(b"fed"), Ok(Typval::String(OxStr::from("123"))));
    assert_eq!(
        global(b"listed"),
        Ok(Typval::list(vec![Typval::String(OxStr::from("123"))]))
    );
    // `-cx` traces to standard error, which `os_system` merges into the output,
    // so only a 'shellcmdflag' that is actually read produces this.
    assert!(
        matches!(&global(b"traced"), Ok(Typval::String(text)) if text.to_string_lossy().contains("true")),
        "shellcmdflag was not used: {:?}",
        global(b"traced")
    );
    assert_eq!(global(b"traced_error"), Ok(Typval::Number(0)));
    // An unreachable 'shell' is `v:shell_error` == -1 and no exception.
    assert_eq!(global(b"missing"), Ok(Typval::String(OxStr::from(""))));
    assert_eq!(global(b"missing_error"), Ok(Typval::Number(-1)));
}

// ---------------------------------------------------------------------------
// Family: dynamic script context (script.c current_sctx, usercmd.c do_ucmd,
// eval/userfunc.c call_user_func) — Task 4B
// ---------------------------------------------------------------------------

#[test]
fn user_command_uses_defining_script_context() {
    let io = MemoryFileIO::new();
    io.insert(
        "/def.vim",
        "function! s:Hit()\n  let g:hit = 1\nendfunction\ncommand Hit call s:Hit()",
    );
    io.insert(
        "/invoke.vim",
        "function! s:Hit()\n  let g:hit = 2\nendfunction\nHit",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.source_file(&editor, "/def.vim".as_ref()).unwrap();
    exec.source_file(&editor, "/invoke.vim".as_ref()).unwrap();
    assert_eq!(global_number(exec.scope(), "hit"), Some(1));
}

#[test]
fn keepscript_retains_invocation_script_context() {
    let io = MemoryFileIO::new();
    io.insert(
        "/def.vim",
        "function! s:Hit()\n  let g:hit = 1\nendfunction\ncommand -keepscript Hit call s:Hit()",
    );
    io.insert(
        "/invoke.vim",
        "function! s:Hit()\n  let g:hit = 2\nendfunction\nHit",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.source_file(&editor, "/def.vim".as_ref()).unwrap();
    exec.source_file(&editor, "/invoke.vim".as_ref()).unwrap();
    assert_eq!(global_number(exec.scope(), "hit"), Some(2));
}

#[test]
fn user_command_body_error_restores_caller_script_context() {
    let io = MemoryFileIO::new();
    io.insert(
        "/def.vim",
        "function! s:Boom()\n  throw 'boom'\nendfunction\ncommand Boom call s:Boom()",
    );
    io.insert(
        "/invoke.vim",
        "function! s:Mark()\n  let g:mark = 1\nendfunction\nBoom\nlet g:after = 1",
    );
    io.insert(
        "/later.vim",
        "function! s:Later()\n  let g:later = 1\nendfunction\ncall s:Later()",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.source_file(&editor, "/def.vim".as_ref()).unwrap();
    let err = exec
        .source_file(&editor, "/invoke.vim".as_ref())
        .unwrap_err();
    assert_eq!(error_code(&err), "Throw");
    assert_eq!(global_number(exec.scope(), "after"), None);
    exec.source_file(&editor, "/later.vim".as_ref()).unwrap();
    assert_eq!(global_number(exec.scope(), "later"), Some(1));
}
#[test]
fn nested_user_command_invocation_restores_script_context() {
    let io = MemoryFileIO::new();
    io.insert(
        "/a.vim",
        "function! s:A()\n  let g:a = 1\nendfunction\ncommand A call s:A()",
    );
    io.insert("/b.vim", "command B A");
    io.insert(
        "/c.vim",
        "function! s:After()\n  let g:after = 1\nendfunction\nB\ncall s:After()",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.source_file(&editor, "/a.vim".as_ref()).unwrap();
    exec.source_file(&editor, "/b.vim".as_ref()).unwrap();
    exec.source_file(&editor, "/c.vim".as_ref()).unwrap();
    assert_eq!(global_number(exec.scope(), "a"), Some(1));
    assert_eq!(global_number(exec.scope(), "after"), Some(1));
}

#[test]
fn function_call_uses_defining_script_context() {
    let io = MemoryFileIO::new();
    io.insert("/def.vim", "function! s:Hit()\n  let g:hit = 1\nendfunction\nfunction! GlobalHit()\n  call s:Hit()\nendfunction");
    io.insert(
        "/invoke.vim",
        "function! s:Hit()\n  let g:hit = 2\nendfunction\ncall GlobalHit()",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    exec.source_file(&editor, "/def.vim".as_ref()).unwrap();
    exec.source_file(&editor, "/invoke.vim".as_ref()).unwrap();
    assert_eq!(global_number(exec.scope(), "hit"), Some(1));
}

#[test]
fn search_moves_cursor_and_returns_line() {
    let editor = TestEditorAccess::new(editor_with_lines(&["aaa", "'foo' bar", "zzz"]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&editor, "call cursor(1, 1)").unwrap();
    let line = exec
        .call_builtin(
            &editor,
            &OxStr::from("search"),
            vec![
                Typval::String(OxStr::from("^'foo'")),
                Typval::String(OxStr::from("W")),
            ],
        )
        .unwrap();
    assert_eq!(line, Typval::Number(2));
}

#[test]
fn search_returns_zero_when_not_found() {
    let editor = TestEditorAccess::new(editor_with_lines(&["aaa"]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let line = exec
        .call_builtin(
            &editor,
            &OxStr::from("search"),
            vec![
                Typval::String(OxStr::from("nope")),
                Typval::String(OxStr::from("W")),
            ],
        )
        .unwrap();
    assert_eq!(line, Typval::Number(0));
}

#[test]
fn search_finds_multiline_option_header() {
    let editor = TestEditorAccess::new(editor_with_lines(&[
        "header",
        "'cpoptions' foo",
        "\t|global-local",
    ]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&editor, "call cursor(1, 1)").unwrap();
    let line = exec
        .call_builtin(
            &editor,
            &OxStr::from("search"),
            vec![
                Typval::String(OxStr::from("^'[^']*'.*\\n.*|global-local")),
                Typval::String(OxStr::from("W")),
            ],
        )
        .unwrap();
    assert_eq!(line, Typval::Number(2));
}

#[test]
fn function_reconstructs_lambda_by_name() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&editor, "let s:Len = {s -> strlen(s)}")
        .unwrap();
    exec.execute_line(&editor, "let name = string(s:Len)")
        .unwrap();
    exec.execute_line(&editor, "execute 'let Ref = ' .. name")
        .unwrap();
    exec.execute_line(&editor, "let g:got = Ref('text')")
        .unwrap();
    assert_eq!(global_number(exec.scope(), "got"), Some(4));
}

#[test]
fn function_accepts_routed_setqflist() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let value = exec
        .call_builtin(
            &editor,
            &OxStr::from("function"),
            vec![Typval::String(OxStr::from("setqflist"))],
        )
        .unwrap();
    assert!(
        matches!(value, Typval::Funcref(_) | Typval::Partial(_)),
        "function() returned {value:?}"
    );
    exec.execute_line(&editor, "let g:Xsetlist = function('setqflist')")
        .unwrap();
    exec.execute_line(&editor, "let g:t = type(g:Xsetlist)")
        .unwrap();
    assert_eq!(
        global_number(exec.scope(), "t"),
        Some(2),
        "expected v:t_func"
    );
    exec.execute_line(&editor, "call g:Xsetlist([{'lnum': 1, 'text': 'a'}])")
        .unwrap();
    exec.execute_line(&editor, "let g:n = len(getqflist())")
        .unwrap();
    assert_eq!(global_number(exec.scope(), "n"), Some(1));
}

#[test]
fn builtin_call_is_not_stolen_by_a_same_named_argument() {
    let editor = TestEditorAccess::new(editor_with_lines(&["aaa", "return x;", "zzz"]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &editor,
        "t.vim",
        "function! T(line)\n  return line('.')\nendfunction\n",
    )
    .unwrap();
    exec.execute_line(&editor, "call cursor(2, 1)").unwrap();
    exec.execute_line(&editor, "let g:got = T(99)").unwrap();
    assert_eq!(global_number(exec.scope(), "got"), Some(2));
}

// ---------------------------------------------------------------------------
// Routed Search and Register family contracts
// (builtins/mod.rs route table; eval/funcs.c f_search, searchpair_cmn,
//  f_searchcount, f_getreg, f_getregtype, f_setreg, f_getreginfo)
// ---------------------------------------------------------------------------

/// Reads the entry named `key` out of a Dict `Typval`, the shape several
/// routed builtins answer with.
fn routed_dict_entry(value: &Typval, key: &[u8]) -> Option<Typval> {
    match value {
        Typval::Dict(cell) => {
            let data = cell.borrow();
            data.entries
                .iter()
                .find(|e| e.key.as_bytes() == key)
                .map(|e| e.value.clone())
        }
        _ => panic!("expected dict, got {value:?}"),
    }
}

fn routed_family_names(family: crate::builtins::Family) -> Vec<&'static str> {
    BUILTINS
        .iter()
        .filter_map(|spec| (crate::builtins::route(spec.name) == Some(family)).then_some(spec.name))
        .collect()
}

/// The route table itself is a tested contract: every generated builtin name
/// whose `builtins::route` maps to `Family::Search` must reach a real dispatch
/// arm through `ExExecutor::call_builtin`, never the defensive `E117` branch.
/// The exact family set is asserted first, then each member is invoked with
/// valid arguments and its distinct normal result shape is checked. If a
/// future route addition lacks a real arm, the defensive `E117` error makes
/// the `Ok` assertion fail on that exact member.
///
/// Source-grounded fixture adapted from
/// `.references/neovim/test/old/testdir/test_search.vim:289-411`:
/// `start`/`end` token vocabulary so `searchpair` nesting is unambiguous.
#[test]
fn routed_search_family_dispatches_every_member() {
    let editor = TestEditorAccess::new(editor_with_lines(&[
        "start x",
        "  start y",
        "    stmt",
        "  end",
        "end",
    ]));
    let mut exec = search_family_executor(&editor);
    assert_eq!(
        routed_family_names(crate::builtins::Family::Search),
        vec!["search", "searchcount", "searchpair", "searchpairpos"],
        "Search family route set"
    );

    // search("stmt", "Wn") → Number(3): the only line containing "stmt".
    exec.execute_line(&editor, "call cursor(1, 1)").unwrap();
    let result = exec
        .call_builtin(
            &editor,
            &OxStr::from("search"),
            vec![
                Typval::String(OxStr::from("stmt")),
                Typval::String(OxStr::from("Wn")),
            ],
        )
        .unwrap();
    assert_eq!(result, Typval::Number(3), "search result");

    // searchpair("start", "", "end", "Wn") → Number(5): outermost end.
    exec.execute_line(&editor, "call cursor(1, 1)").unwrap();
    let result = exec
        .call_builtin(
            &editor,
            &OxStr::from("searchpair"),
            vec![
                Typval::String(OxStr::from("start")),
                Typval::String(OxStr::from("")),
                Typval::String(OxStr::from("end")),
                Typval::String(OxStr::from("Wn")),
            ],
        )
        .unwrap();
    assert_eq!(result, Typval::Number(5), "searchpair result");

    assert_searchpairpos_position(&mut exec, &editor);
    routed_searchcount_dict_fields(&mut exec, &editor);
}

/// The `searchpairpos` arm: `[5, 1]` — line 5, byte col 1.
fn assert_searchpairpos_position(exec: &mut ExExecutor<MemoryFileIO>, editor: &TestEditorAccess) {
    exec.execute_line(editor, "call cursor(1, 1)").unwrap();
    let result = exec
        .call_builtin(
            editor,
            &OxStr::from("searchpairpos"),
            vec![
                Typval::String(OxStr::from("start")),
                Typval::String(OxStr::from("")),
                Typval::String(OxStr::from("end")),
                Typval::String(OxStr::from("Wn")),
            ],
        )
        .unwrap();
    assert_eq!(
        result,
        Typval::list(vec![Typval::Number(5), Typval::Number(1)]),
        "searchpairpos result"
    );
}

/// The `searchcount` arm: the Dict keys upstream reports.
fn routed_searchcount_dict_fields(exec: &mut ExExecutor<MemoryFileIO>, editor: &TestEditorAccess) {
    exec.execute_line(editor, "call cursor(1, 1)").unwrap();
    let result = exec
        .call_builtin(
            editor,
            &OxStr::from("searchcount"),
            vec![Typval::dict(Vec::new())],
        )
        .unwrap();
    assert_eq!(
        routed_dict_entry(&result, b"current"),
        Some(Typval::Number(1)),
        "searchcount current"
    );
    assert_eq!(
        routed_dict_entry(&result, b"total"),
        Some(Typval::Number(2)),
        "searchcount total"
    );
    assert_eq!(
        routed_dict_entry(&result, b"exact_match"),
        Some(Typval::Number(1)),
        "searchcount exact_match (Number, not Bool)"
    );
    assert_eq!(
        routed_dict_entry(&result, b"incomplete"),
        Some(Typval::Number(0)),
        "searchcount incomplete"
    );
    assert_eq!(
        routed_dict_entry(&result, b"maxcount"),
        Some(Typval::Number(999)),
        "searchcount maxcount (default 999)"
    );
}

/// Builds the executor this family shares: the `/` register holds the pattern
/// `searchcount` reports on.
fn search_family_executor(editor: &TestEditorAccess) -> ExExecutor<MemoryFileIO> {
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(editor, "call cursor(1, 1)").unwrap();
    exec.call_builtin(
        editor,
        &OxStr::from("setreg"),
        vec![
            Typval::String(OxStr::from("/")),
            Typval::String(OxStr::from("start")),
        ],
    )
    .unwrap();
    exec
}

/// The same route contract for the `Family::Register` members: the exact
/// route set is asserted from live metadata, then every member is invoked
/// with valid arguments and its distinct normal result shape is checked.
#[test]
fn routed_register_family_dispatches_every_member() {
    let register_names = routed_family_names(crate::builtins::Family::Register);
    assert_eq!(
        register_names,
        vec!["getreg", "getreginfo", "getregtype", "setreg"],
        "Register family route set"
    );

    // --- Register setup: register a holds "hello" ---
    let editor = TestEditorAccess::new(editor_with_lines(&[
        "start x",
        "  start y",
        "    stmt",
        "  end",
        "end",
    ]));
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.call_builtin(
        &editor,
        &OxStr::from("setreg"),
        vec![
            Typval::String(OxStr::from("a")),
            Typval::String(OxStr::from("hello")),
        ],
    )
    .unwrap();

    // --- Register family: each member returns a distinct Ok shape ---

    // getreg("a") → String("hello").
    let result = exec
        .call_builtin(
            &editor,
            &OxStr::from("getreg"),
            vec![Typval::String(OxStr::from("a"))],
        )
        .unwrap();
    assert_eq!(
        result,
        Typval::String(OxStr::from("hello")),
        "getreg result"
    );

    // getregtype("a") → String("v"): characterwise.
    let result = exec
        .call_builtin(
            &editor,
            &OxStr::from("getregtype"),
            vec![Typval::String(OxStr::from("a"))],
        )
        .unwrap();
    assert_eq!(
        result,
        Typval::String(OxStr::from("v")),
        "getregtype result"
    );

    // setreg("a", "world") → Number(0): success.
    let result = exec
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![
                Typval::String(OxStr::from("a")),
                Typval::String(OxStr::from("world")),
            ],
        )
        .unwrap();
    assert_eq!(result, Typval::Number(0), "setreg result");

    // getreginfo("a") → Dict with regcontents, regtype, isunnamed.
    let result = exec
        .call_builtin(
            &editor,
            &OxStr::from("getreginfo"),
            vec![Typval::String(OxStr::from("a"))],
        )
        .unwrap();
    assert_eq!(
        routed_dict_entry(&result, b"regcontents"),
        Some(Typval::list(vec![Typval::String(OxStr::from("world"))])),
        "getreginfo regcontents (reflects the setreg update to world)"
    );
    assert_eq!(
        routed_dict_entry(&result, b"regtype"),
        Some(Typval::String(OxStr::from("v"))),
        "getreginfo regtype"
    );
    // isunnamed is Bool, not Number — upstream intentionally differs from
    // searchcount.exact_match which is Number.
    assert_eq!(
        routed_dict_entry(&result, b"isunnamed"),
        Some(Typval::Bool(false)),
        "getreginfo isunnamed (Bool, not Number)"
    );
}

// ---------------------------------------------------------------------------
// Scoped unit tests for eval-recon items (b), (c), (d)
// ---------------------------------------------------------------------------

/// Item (b): `let @" = 'X'` must split at `=` without treating the `"` after
/// `@` as a quote delimiter. The `@=` expression register must split at the
/// second `=`, not the first.
#[test]
fn split_assignment_let_quote_register_finds_equals() {
    let (target, op, expr) = crate::excmd_exec::split_assignment(r#"@" = 'X'"#).unwrap();
    assert_eq!(target, "@\"");
    assert_eq!(op, "=");
    assert_eq!(expr, "'X'");
}

/// Item (b): `@= = '"abc"'` — the first `=` is part of the `@=` register
/// name, the second `=` is the assignment operator.
#[test]
fn split_assignment_at_equal_register_splits_at_second_equals() {
    let (target, op, expr) = crate::excmd_exec::split_assignment(r#"@= = '"abc"'"#).unwrap();
    assert_eq!(target, "@=");
    assert_eq!(op, "=");
    assert_eq!(expr, r#"'"abc"'"#);
}

#[test]
fn split_assignment_at_at_no_space_finds_equals() {
    let (target, op, expr) = crate::excmd_exec::split_assignment("@@= ykeep").unwrap();
    assert_eq!(target, "@@");
    assert_eq!(op, "=");
    assert_eq!(expr, "ykeep");
}

/// Item (c): setreg append with LineWise-incoming into CharacterWise-existing
/// must concatenate the first incoming line onto the last existing line,
/// then extend, then set kind=LineWise.
#[test]
fn setreg_append_linewise_into_charwise_concatenates_first_line() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    // Set register 'a' as CharacterWise with "hello"
    exec.execute_line(&editor, "let @a = 'hello'").unwrap();
    // Append LineWise data via setreg with uppercase 'A' (append) and 'L' kind
    exec.execute_line(&editor, "call setreg('A', ['world'], 'L')")
        .unwrap();

    // LineWise append into CharacterWise: "hello" + "world" on same line
    let binding = editor.editor();
    let reg = binding.registers().get('a').unwrap().unwrap();
    let text = String::from_utf8(reg.to_bytes()).unwrap();
    assert_eq!(
        text, "helloworld",
        "LineWise append into CharacterWise should concatenate first line"
    );
}

/// Item (d): `exists('version')` should return 1 when `v:version` is seeded
/// in the vim scope (scriptversion-2 context).
#[test]
fn exists_version_returns_true_when_v_version_seeded() {
    let mut scope = Scope::new();
    scope.replace_pair(ScopeKind::Vim, "version", Typval::Number(1000));
    let result =
        ox_eval::builtins::exists(&Typval::String(OxStr::from("version")), &scope).unwrap();
    assert_eq!(
        result,
        Typval::Number(1),
        "exists('version') should find v:version"
    );
}

// ---------------------------------------------------------------------------
// Full-path verification probes for eval-finish items (b), (d), (e)
// ---------------------------------------------------------------------------

/// Item (b) full-path: `let @" = 'abc'` must execute through the full Ex
/// pipeline — `split_assignment` must not treat the `"` after `@` as a quote
/// delimiter, and the unnamed register must receive the value.
#[test]
fn let_quote_register_executes_and_sets_unnamed() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&editor, r#"let @" = 'abc'"#).unwrap();
    let binding = editor.editor();
    let reg = binding
        .registers()
        .get('"')
        .unwrap()
        .expect("unnamed register should be set");
    assert_eq!(
        String::from_utf8(reg.to_bytes()).unwrap(),
        "abc",
        "let @\" = 'abc' should write 'abc' to the unnamed register"
    );
}

/// Item (d) full-path: `exists('version')` called from a script should
/// return 1, because `v:version` is seeded in the editor's vvars and the
/// `exists` builtin maps bare `version` to `v:version`.
#[test]
fn exists_version_through_executor_returns_one() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(&editor, "exists.vim", "let g:r = exists('version')")
        .unwrap();
    let result = exec.evaluate_expression(&editor, "g:r").unwrap();
    assert_eq!(
        result,
        Typval::Number(1),
        "exists('version') should return 1 via the full executor path"
    );
}

/// Item (e): `getchar()` after `feedkeys("\\<M-…>", 't')` must return the
/// full internal encoding of `<M-…>`, including the `K_SPECIAL` byte inside
/// the multibyte ellipsis character (U+2026 = E2 80 A6).  The `…`
/// character's middle byte 0x80 is `K_SPECIAL`, so it is stored in the
/// typeahead as the 3-byte escape `K_SPECIAL KS_SPECIAL KE_FILLER`.
/// `getchar()` must consume all continuation bytes after the modifier
/// prefix and return the un-escaped raw bytes, matching `\<M-…>`.
#[test]
fn getchar_returns_full_multibyte_modified_character() {
    use crate::typeahead::K_SPECIAL;

    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    // Feed <M-…> and read it back with getchar().
    // In the typeahead, <M-…> is stored as:
    //   K_SPECIAL  KS_MODIFIER(0xFC)  MOD_ALT(0x08)   — modifier prefix
    //   0xE2                                           — first byte of …
    //   K_SPECIAL  KS_SPECIAL  KE_FILLER              — escaped 0x80
    //   0xA6                                           — third byte of …
    // But `\<M-…>` as a string value (and getchar() output) uses the
    // un-escaped form: K_SPECIAL 0xFC 0x08 0xE2 0x80 0xA6.
    exec.execute_script(
        &editor,
        "getchar.vim",
        r#"call feedkeys("\<M-…>", 't')
           let g:ch = getchar()"#,
    )
    .unwrap();
    let result = exec.evaluate_expression(&editor, "g:ch").unwrap();
    let Typval::String(got) = &result else {
        panic!("getchar() should return a String, got {result:?}");
    };
    // Expected: un-escaped form matching `\<M-…>` string literal.
    let expected: &[u8] = &[
        K_SPECIAL, 0xFC, 0x08, // modifier prefix (Meta)
        0xE2, // first byte of ellipsis (U+2026)
        0x80, // second byte (K_SPECIAL, un-escaped)
        0xA6, // third byte of ellipsis
    ];
    assert_eq!(
        got.as_bytes(),
        expected,
        "getchar() must return the full <M-…> with un-escaped K_SPECIAL"
    );
}

// ---------------------------------------------------------------------------
// Family: channel/terminal builtins (channel.c f_chansend/f_chanclose,
// f_sockconnect, f_rpcnotify, f_rpcrequest; terminal.c termopen;
// channel.txt, job_control.txt)
// ---------------------------------------------------------------------------

/// `jobstart` fixtures share one process-state lock: the reactor-backed
/// `JobManager` is process-global through the test host.
#[cfg(unix)]
fn channel_suite() -> (
    TestEditorAccess,
    ExExecutor<MemoryFileIO>,
    crate::ProcessStateGuard,
) {
    let guard = crate::lock_process_state();
    let editor = TestEditorAccess::new(editor_with_lines(&["alpha"]));
    let exec = ExExecutor::with_io(MemoryFileIO::new());
    (editor, exec, guard)
}

/// Unique socket path per test: parallel `nextest` workers share `/tmp`.
#[cfg(unix)]
fn unique_socket_path(tag: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "oxvim-chan-{tag}-{}-{id}.sock",
        std::process::id()
    ));
    let _ignored = std::fs::remove_file(&path);
    path
}

/// Bind a peer listener synchronously so `sockconnect` cannot race it: the
/// bound socket exists before the script connects.
#[cfg(unix)]
fn bind_peer(path: &std::path::Path) -> std::os::unix::net::UnixListener {
    std::os::unix::net::UnixListener::bind(path).expect("peer binds its socket")
}

/// Accept one peer, record the first RPC notification, then hang up.
#[cfg(unix)]
fn spawn_notify_peer(
    listener: std::os::unix::net::UnixListener,
) -> (
    std::thread::JoinHandle<()>,
    std::sync::mpsc::Receiver<(String, Vec<Object>)>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("peer accepts");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("peer read timeout");
        let mut decoder = ox_rpc::IncrementalDecoder::new();
        let mut chunk = [0_u8; 4096];
        loop {
            use std::io::Read as _;
            let read = stream
                .read(&mut chunk)
                .expect("peer reads the notification");
            if read == 0 {
                // The test closed its side without notifying: done.
                return;
            }
            let feed = decoder.feed(&chunk[..read]).expect("peer decodes frames");
            for message in feed {
                if let ox_rpc::Message::Notification { method, params } = message {
                    tx.send((method.to_string_lossy().into_owned(), params))
                        .expect("peer reports the notification");
                    return;
                }
            }
        }
    });
    (handle, rx)
}

/// Accept one peer, answer the first RPC request with `answer`, then hang up.
#[cfg(unix)]
fn spawn_request_peer(
    listener: std::os::unix::net::UnixListener,
    answer: Result<Object, ox_types::ApiError>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("peer accepts");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("peer read timeout");
        let mut decoder = ox_rpc::IncrementalDecoder::new();
        let mut chunk = [0_u8; 4096];
        loop {
            use std::io::Read as _;
            use std::io::Write as _;
            let read = stream.read(&mut chunk).expect("peer reads the request");
            if read == 0 {
                // The test went away without asking: done.
                return;
            }
            let feed = decoder.feed(&chunk[..read]).expect("peer decodes frames");
            for message in feed {
                if let ox_rpc::Message::Request {
                    msgid, method: _method, params: _params,
                } = message
                {
                    let frame = ox_rpc::Message::Response {
                        msgid,
                        result: match answer {
                            Ok(ref object) => Ok(object.clone()),
                            Err(ref error) => Err(error.clone()),
                        },
                    }
                    .encode_bytes()
                    .expect("peer encodes its reply");
                    stream.write_all(&frame).expect("peer answers");
                    stream.flush().expect("peer flushes");
                    return;
                }
            }
        }
    })
}

/// Accept one peer, read exactly `want` bytes, report them.
#[cfg(unix)]
fn spawn_byte_peer(
    listener: std::os::unix::net::UnixListener,
    want: usize,
) -> (
    std::thread::JoinHandle<()>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("peer accepts");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("peer read timeout");
        let mut bytes = vec![0_u8; want];
        {
            use std::io::Read as _;
            stream.read_exact(&mut bytes).expect("peer reads raw bytes");
        }
        tx.send(bytes).expect("peer reports raw bytes");
    });
    (handle, rx)
}

#[cfg(unix)]
#[test]
fn termopen_spawns_a_terminal_and_names_it() {
    let (editor, mut exec, _guard) = channel_suite();
    script(
        &mut exec,
        &editor,
        "let g:tid = termopen(['sh', '-c', 'echo term-hi-marker'])\ncall jobwait([g:tid], 5000)\nlet g:tbuf = bufnr('%')\nlet g:tbtype = getbufvar('%', '&buftype')\nlet g:tlines = join(getbufline('%', 1, '$'), \"\\n\")\nlet g:tname = bufname('%')\n",
    );
    assert!(global_number(exec.scope(), "tid") > Some(0));
    assert!(global_number(exec.scope(), "tbuf") > Some(0));
    // `termopen` marks the current buffer, like `terminal_open` does.
    assert_eq!(global_string(exec.scope(), "tbtype"), Some("terminal".to_owned()));
    let lines = global_string(exec.scope(), "tlines").unwrap_or_default();
    assert!(lines.contains("term-hi-marker"), "terminal output missing: {lines:?}");
    assert!(
        global_string(exec.scope(), "tname")
            .is_some_and(|name| name.starts_with("term://")),
        "terminal buffer must carry a term:// name"
    );
}

#[cfg(unix)]
#[test]
fn termopen_rejects_a_relative_program_outside_the_process_cwd() {
    use std::os::unix::fs::PermissionsExt as _;
    let (editor, mut exec, _guard) = channel_suite();
    // Upstream checks executability with `os_can_exe` against the process
    // working directory, not the requested `cwd` option, so `./tool` present
    // only under `{'cwd': ...}` is refused with E475 (`eval/funcs.c`).
    let dir = std::env::temp_dir().join(format!(
        "oxvim-termrel-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock runs forward")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("test stages a cwd");
    let tool = dir.join("tool");
    std::fs::write(&tool, "#!/bin/sh\necho rel-marker\n").expect("test stages a tool");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
        .expect("tool is executable");
    let error = exec
        .execute_script(
            &editor,
            "<test>",
            &format!(
                "let g:tid = termopen(['./tool'], {{'cwd': '{}'}})\n",
                dir.display()
            ),
        )
        .unwrap_err();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        error_code(&error),
        "E475",
        "a relative program outside the process cwd must be refused"
    );
}

/// With the program resolvable from the process working directory, upstream
/// stores the absolute form in the `term://` name (`save_abs_path`), not the
/// word as typed.
#[cfg(unix)]
#[test]
fn termopen_absolutizes_a_relative_program_the_process_cwd_resolves() {
    use std::os::unix::fs::PermissionsExt as _;
    let (editor, mut exec, _guard) = channel_suite();
    let dir = std::env::temp_dir().join(format!(
        "oxvim-termabs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock runs forward")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("test stages a cwd");
    let tool = dir.join("tool");
    std::fs::write(&tool, "#!/bin/sh\necho abs-marker\n").expect("test stages a tool");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
        .expect("tool is executable");
    let previous = std::env::current_dir().expect("cwd is readable");
    std::env::set_current_dir(&dir).expect("test enters the staged cwd");
    script(
        &mut exec,
        &editor,
        "let g:tid = termopen(['./tool'], {'cwd': getcwd()})\ncall jobwait([g:tid], 5000)\nlet g:absname = bufname('%')\n",
    );
    std::env::set_current_dir(&previous).expect("test restores the cwd");
    let name = global_string(exec.scope(), "absname").unwrap_or_default();
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        name.ends_with(&format!(":{}/tool", dir.display())),
        "a resolvable relative program must be stored absolute, got {name:?}"
    );
}

#[cfg(unix)]
#[test]
fn termopen_rejects_a_busy_terminal_buffer() {
    let (editor, mut exec, _guard) = channel_suite();
    script(
        &mut exec,
        &editor,
        "let g:tid1 = termopen(['sh', '-c', 'sleep 5'])\nlet g:tid2 = termopen(['sh', '-c', 'echo late'])\ncall jobstop(g:tid1)\n",
    );
    assert!(global_number(exec.scope(), "tid1") > Some(0));
    // The current buffer already runs a terminal job: no double attach.
    assert_eq!(global_number(exec.scope(), "tid2"), Some(0));
}

#[cfg(unix)]
#[test]
fn termopen_rejects_bad_arguments() {
    let (editor, mut exec, _guard) = channel_suite();
    for (line, code) in [
        ("call termopen(123)", "E475"),
        ("call termopen()", "E119"),
        ("call termopen(['true'], [1])", "E475"),
        (
            "call termopen(['true'], {'cwd': '/no/such/oxvim-dir'})",
            "E475",
        ),
        (
            "call jobstart(['cat'], {'pty': v:true, 'rpc': v:true})",
            "E475",
        ),
    ] {
        let error = exec.execute_line(&editor, line).unwrap_err();
        assert_eq!(error_code(&error), code, "{line}");
        if line.contains("[1]") {
            assert!(
                error.to_string().contains("expected dictionary"),
                "{line}: got {error}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn jobclose_defaults_to_stdin_and_chanclose_closes_all() {
    let (editor, mut exec, _guard) = channel_suite();
    script(
        &mut exec,
        &editor,
        "let g:job = jobstart(['cat'])\nlet g:closed = jobclose(g:job)\nlet g:exit = jobwait([g:job], 5000)[0]\nlet g:job2 = jobstart(['cat'])\nlet g:closed2 = chanclose(g:job2)\nlet g:again = chanclose(g:job2)\n",
    );
    assert_eq!(global_number(exec.scope(), "closed"), Some(1));
    // Closing stdin gives `cat` its EOF, so it exits cleanly.
    assert_eq!(global_number(exec.scope(), "exit"), Some(0));
    assert_eq!(global_number(exec.scope(), "closed2"), Some(1));
    assert_eq!(
        global_number(exec.scope(), "again"),
        Some(1),
        "upstream allows double close"
    );
}

#[cfg(unix)]
#[test]
fn chanclose_rejects_unknown_channels_and_streams() {
    let (editor, mut exec, _guard) = channel_suite();
    script(
        &mut exec,
        &editor,
        "let g:job = jobstart(['cat'])\nlet g:rj = jobstart(['cat'], {'rpc': v:true})\n",
    );
    for (line, code, message) in [
        ("call chanclose(999998)", "E900", "Invalid channel id"),
        ("call chanclose(g:job, 'bogus')", "E906", "Invalid channel stream"),
        (
            "call chanclose(g:job, 'rpc')",
            "E906",
            "invalid stream for channel",
        ),
        ("call jobclose('x')", "E475", "Invalid argument"),
    ] {
        let error = exec.execute_line(&editor, line).unwrap_err();
        assert_eq!(error_code(&error), code, "{line}");
        assert!(error.to_string().contains(message), "{line}: got {error}");
    }
    // `jobclose` is the obsolete name for `chanclose`: an omitted stream
    // closes every part, even on an RPC job.
    script(
        &mut exec,
        &editor,
        "let g:rk = jobstart(['cat'], {'rpc': v:true})\nlet g:job_closed = jobclose(g:rk)\n",
    );
    assert_eq!(global_number(exec.scope(), "job_closed"), Some(1));
    // Closing the RPC half answers 1 and retires the RPC channel.
    script(&mut exec, &editor, "let g:rpc_closed = chanclose(g:rj, 'rpc')\n");
    assert_eq!(global_number(exec.scope(), "rpc_closed"), Some(1));
    let error = exec.execute_line(&editor, "call rpcrequest(g:rj, 'm')").unwrap_err();
    assert_eq!(error_code(&error), "E475");
}

#[cfg(unix)]
#[test]
fn rpc_notify_reaches_a_socket_peer() {
    let (editor, mut exec, _guard) = channel_suite();
    let path = unique_socket_path("notify");
    let (peer, rx) = spawn_notify_peer(bind_peer(&path));
    script(
        &mut exec,
        &editor,
        &format!(
            "let g:sock = sockconnect('pipe', '{}', {{'rpc': v:true}})\nlet g:sent = rpcnotify(g:sock, 'hello', 1, 2)\n",
            path.display()
        ),
    );
    assert!(global_number(exec.scope(), "sock") > Some(0));
    assert_eq!(global_number(exec.scope(), "sent"), Some(1));
    let (method, params) = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("peer receives the notification");
    assert_eq!(method, "hello");
    assert_eq!(
        params,
        vec![Object::Integer(1), Object::Integer(2)],
        "notification params must cross the socket untouched"
    );
    peer.join().expect("peer exits after one notification");
}

#[cfg(unix)]
#[test]
fn rpc_request_blocks_for_a_socket_reply() {
    let (editor, mut exec, _guard) = channel_suite();
    let path = unique_socket_path("request");
    let peer = spawn_request_peer(bind_peer(&path), Ok(Object::Integer(3)));
    script(
        &mut exec,
        &editor,
        &format!(
            "let g:sock = sockconnect('pipe', '{}', {{'rpc': v:true}})\nlet g:answer = rpcrequest(g:sock, 'sum', 1, 2)\n",
            path.display()
        ),
    );
    assert_eq!(global_number(exec.scope(), "answer"), Some(3));
    peer.join().expect("peer exits after answering");
}

#[cfg(unix)]
#[test]
fn rpc_request_reports_remote_errors() {
    let (editor, mut exec, _guard) = channel_suite();
    let path = unique_socket_path("remote-err");
    let peer = spawn_request_peer(
        bind_peer(&path),
        Err(ox_types::ApiError::Exception("boom".to_owned())),
    );
    script(
        &mut exec,
        &editor,
        &format!(
            "let g:sock = sockconnect('pipe', '{}', {{'rpc': v:true}})\n",
            path.display()
        ),
    );
    let error = exec
        .execute_line(&editor, "let g:bad = rpcrequest(g:sock, 'nope')")
        .unwrap_err();
    assert_eq!(error_code(&error), "E605");
    assert!(error.to_string().contains("boom"), "got {error}");
    peer.join().expect("peer exits after answering");
}

#[cfg(unix)]
#[test]
fn rpc_request_answers_over_a_job_channel() {
    // No msgpack helper ships with the shell, so the peer is a canned byte
    // exchange: the request `[0,1,"ping",[]]` is exactly 9 bytes, and the
    // reply `[1,1,nil,"pong"]` is written back verbatim.
    let (editor, mut exec, _guard) = channel_suite();
    script(
        &mut exec,
        &editor,
        "let g:rj = jobstart(['sh', '-c', 'head -c 9 >/dev/null; printf \"\\224\\001\\001\\300\\244pong\"'], {'rpc': v:true})\nlet g:pong = rpcrequest(g:rj, 'ping')\n",
    );
    assert_eq!(global_string(exec.scope(), "pong"), Some("pong".to_owned()));
}

#[cfg(unix)]
#[test]
fn rpc_notify_writes_to_a_job_channel() {
    let (editor, mut exec, _guard) = channel_suite();
    script(
        &mut exec,
        &editor,
        "let g:rj = jobstart(['cat'], {'rpc': v:true})\nlet g:sent = rpcnotify(g:rj, 'hello')\n",
    );
    assert_eq!(global_number(exec.scope(), "sent"), Some(1));
}

#[cfg(unix)]
#[test]
fn rpc_broadcast_reaches_every_open_channel() {
    let (editor, mut exec, _guard) = channel_suite();
    let path = unique_socket_path("broadcast");
    let (peer, rx) = spawn_notify_peer(bind_peer(&path));
    script(
        &mut exec,
        &editor,
        &format!(
            "let g:sock = sockconnect('pipe', '{}', {{'rpc': v:true}})\nlet g:sent = rpcnotify(0, 'leaving')\n",
            path.display()
        ),
    );
    assert_eq!(global_number(exec.scope(), "sent"), Some(1));
    let (method, _) = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("peer receives the broadcast");
    assert_eq!(method, "leaving");
    peer.join().expect("peer exits after one notification");
}

#[cfg(unix)]
#[test]
fn rpc_rejects_bad_channels_and_methods() {
    let (editor, mut exec, _guard) = channel_suite();
    script(
        &mut exec,
        &editor,
        "let g:job = jobstart(['cat'])\nlet g:sock = sockconnect('pipe', '/no/such/oxvim-test.sock')\n",
    );
    assert_eq!(global_number(exec.scope(), "sock"), Some(0));
    for (line, code) in [
        ("call rpcnotify('x', 'e')", "E475"),
        ("call rpcnotify(-1, 'e')", "E475"),
        ("call rpcnotify(g:job, 'e')", "E475"),
        ("call rpcnotify(999998, 'e')", "E475"),
        ("call rpcrequest(g:job, 'm')", "E475"),
        ("call rpcrequest(999998, 'm')", "E475"),
        ("call rpcrequest(g:job)", "E475"),
        ("call rpcnotify(g:job)", "E475"),
    ] {
        let error = exec.execute_line(&editor, line).unwrap_err();
        assert_eq!(error_code(&error), code, "{line}");
    }
    // A non-string event/method is E475 even on a live channel.
    let path = unique_socket_path("rpc-types");
    let (types_peer, _rx) = spawn_notify_peer(bind_peer(&path));
    script(
        &mut exec,
        &editor,
        &format!(
            "let g:rsock = sockconnect('pipe', '{}', {{'rpc': v:true}})\n",
            path.display()
        ),
    );
    for (line, code) in [
        ("call rpcnotify(g:rsock, 42)", "E475"),
        ("call rpcrequest(g:rsock, 42)", "E475"),
    ] {
        let error = exec.execute_line(&editor, line).unwrap_err();
        assert_eq!(error_code(&error), code, "{line}");
    }
    // Closing the RPC half of an RPC socket shuts it down, like `chanclose()`.
    script(&mut exec, &editor, "let g:rsock_rpc = chanclose(g:rsock, 'rpc')\n");
    assert_eq!(global_number(exec.scope(), "rsock_rpc"), Some(1));
    types_peer.join().expect("types peer exits on EOF");
}

#[cfg(unix)]
#[test]
fn sockconnect_opens_pipes_and_tcp() {
    let (editor, mut exec, _guard) = channel_suite();
    // Raw pipe socket: `chansend` writes bytes the peer reads.
    let pipe_path = unique_socket_path("bytes");
    let (byte_peer, bytes) = spawn_byte_peer(bind_peer(&pipe_path), 5);
    script(
        &mut exec,
        &editor,
        &format!(
            "let g:raw = sockconnect('pipe', '{}')\nlet g:wrote = chansend(g:raw, 'hello')\nlet g:raw_closed = chanclose(g:raw)\n",
            pipe_path.display()
        ),
    );
    assert!(global_number(exec.scope(), "raw") > Some(0));
    assert_eq!(global_number(exec.scope(), "wrote"), Some(1));
    assert_eq!(
        bytes
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("peer reads raw bytes"),
        b"hello".to_vec()
    );
    byte_peer.join().expect("byte peer exits");
    assert_eq!(global_number(exec.scope(), "raw_closed"), Some(1));
    // Raw TCP socket over loopback.
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = tcp.local_addr().expect("loopback port").port();
    let tcp_peer = std::thread::spawn(move || {
        let (mut stream, _) = tcp.accept().expect("accept tcp");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("tcp read timeout");
        let mut bytes = [0_u8; 3];
        {
            use std::io::Read as _;
            stream.read_exact(&mut bytes).expect("read tcp bytes");
        }
        bytes
    });
    script(
        &mut exec,
        &editor,
        &format!("let g:tcp = sockconnect('tcp', '127.0.0.1:{port}')\nlet g:twrote = chansend(g:tcp, 'bye')\n"),
    );
    assert!(global_number(exec.scope(), "tcp") > Some(0));
    assert_eq!(global_number(exec.scope(), "twrote"), Some(1));
    assert_eq!(
        tcp_peer.join().expect("tcp peer exits"),
        *b"bye",
        "tcp bytes must cross the socket"
    );
}

#[cfg(unix)]
#[test]
fn sockconnect_rejects_bad_modes_and_reports_refusals() {
    let (editor, mut exec, _guard) = channel_suite();
    for (line, code) in [
        ("call sockconnect('bogus', 'x')", "E475"),
        ("call sockconnect('pipe')", "E475"),
        ("call sockconnect(42, 'x')", "E475"),
        ("call sockconnect('pipe', 'x', [1])", "E475"),
    ] {
        let error = exec.execute_line(&editor, line).unwrap_err();
        assert_eq!(error_code(&error), code, "{line}");
    }
    // Refused peers answer 0 instead of raising.
    script(
        &mut exec,
        &editor,
        "let g:refused_pipe = sockconnect('pipe', '/no/such/oxvim-test.sock')\nlet g:refused_tcp = sockconnect('tcp', '127.0.0.1:1')\n",
    );
    assert_eq!(global_number(exec.scope(), "refused_pipe"), Some(0));
    assert_eq!(global_number(exec.scope(), "refused_tcp"), Some(0));
}

/// Sourcing is not `:try`: upstream leaves `trylevel` alone in `do_source`,
/// so an error inside a sourced script displays and `did_emsg` aborts the
/// rest of the file — verified against the reference binary, where the
/// line after an uncaught `call` error never executes.
#[test]
fn sourced_script_error_displays_and_aborts() {
    let io = MemoryFileIO::new();
    io.insert(
        "/inner.vim",
        "call NoSuchFunc123()\nlet g:inner_after = 1\n",
    );
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::with_io(io);
    let error = exec
        .source_file(&editor, "/inner.vim".as_ref())
        .unwrap_err();
    assert!(error.to_string().contains("NoSuchFunc123"), "got {error}");
    assert_eq!(global_number(exec.scope(), "inner_after"), None);
    assert!(exec.did_emsg());
}
