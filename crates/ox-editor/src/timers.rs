//! Vimscript `timer_*` builtins (upstream `eval/funcs.c:7397-7486`,
//! `eval.c:5043-5191`).
//!
//! Timers live on the [`JobManager`] loop — the only loop the editor polls —
//! so firings surface during `:sleep`, `vim.wait`, and the RPC pump without
//! a second loop. A firing only enqueues a [`JobEvent`]; the Vimscript
//! callback runs later on a borrow-free boundary through the shared job-event
//! delivery, never inside the loop-handle closure.
//!
//! Wiring (owned by the route-table worker): add the names to the
//! [`Family::Process`] arm in `builtins/mod.rs` and delegate here:
//!
//! ```ignore
//! "timer_info" | "timer_pause" | "timer_start" | "timer_stop" | "timer_stopall" => {
//!     crate::timers::call(host, name, args, scope)
//! }
//! ```

use ox_eval::{EvalError, Scope};
use ox_types::{Funcref, Special, Typval};

use crate::excmd_exec::{EvalHost, ExEditorAccess, ExRuntime};
use crate::job::JobManager;
use crate::script::FileIO;

/// Routes one `timer_*` builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    _scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let runtime = &mut *host.runtime;
    let access = host.access;
    match name {
        "timer_start" => call_timer_start(runtime, access, args),
        "timer_stop" => call_timer_stop(runtime, args),
        "timer_stopall" => call_timer_stopall(runtime),
        "timer_pause" => call_timer_pause(runtime, args),
        "timer_info" => call_timer_info(runtime, args),
        _ => Err(EvalError::new("E117", 0, format!("Unknown function: {name}"))),
    }
}

/// `timer_start(timeout, callback[, opts])` (upstream `f_timer_start`,
/// `eval/funcs.c:7438-7456`): answers a monotonic id starting from 1.
/// `opts` honours `{'repeat': n}`; `repeat == 0` means 1 and a negative
/// repeat fires forever.
///
/// Past the arity gate every failure answers -1 with the message emitted
/// (upstream sets `rettv = -1` before emitting, so the script continues;
/// only a missing argument errors out of the call machinery with `E119`).
/// A loop/context failure reports `E523` (upstream `e_secure`, "Not allowed
/// here": this port has no secure/sandbox state, so the context arm is loop
/// or handle exhaustion).
fn call_timer_start<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: timer_start",
        ));
    }
    let timeout = tv_number(&args[0]);
    let callback = match timer_callback(&args[1]) {
        Ok(callback) => callback,
        Err(error) => return timer_start_failed(runtime, access, error),
    };
    let mut repeat = 1;
    if let Some(options) = args.get(2) {
        // Upstream `tv_check_for_nonnull_dict_arg` (`funcs.c:7445-7448`):
        // only a missing argument skips the options; `v:null` fails the
        // dictionary check with `E1206` (oracle-probed) and a null-dict
        // sentinel fails with `E1297`.
        match options {
            Typval::Dict(data) => {
                if options.is_null_dict() {
                    return timer_start_failed(
                        runtime,
                        access,
                        EvalError::new(
                            "E1297",
                            0,
                            "Non-NULL Dictionary required for argument 3",
                        ),
                    );
                }
                if let Some(value) = data.borrow().get(b"repeat") {
                    repeat = tv_number(value);
                }
            }
            _ => {
                return timer_start_failed(
                    runtime,
                    access,
                    EvalError::new("E1206", 0, "Dictionary required for argument 3"),
                );
            }
        }
    }
    let manager = match jobs_or_create(runtime) {
        Ok(manager) => manager,
        Err(error) => return timer_start_failed(runtime, access, error),
    };
    match manager.timer_start(timeout, repeat, callback) {
        Ok(id) => Ok(Typval::Number(id.cast_signed())),
        Err(message) => timer_start_failed(
            runtime,
            access,
            EvalError::new("E523", 0, format!("Not allowed here: {message}")),
        ),
    }
}

/// Answer a post-arity `timer_start` failure the way upstream does
/// (`f_timer_start` sets `rettv = -1` before emitting, `eval/funcs.c:7442`).
/// Outside `:try` the message is emitted with the same composition an
/// uncaught error displays with (`swallow_error_message`) and the answer is
/// -1, so the script continues. Inside `:try` the failure throws instead —
/// upstream suppresses the display there and the error is catchable.
fn timer_start_failed<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    error: EvalError,
) -> ox_eval::Result<Typval> {
    if runtime.try_depth != 0 {
        return Err(error);
    }
    let detail = format!("{}: {}", error.code, error.message);
    let throwpoint = runtime.throwpoint();
    let text = if throwpoint.is_empty() {
        detail
    } else {
        format!("Error detected while processing {throwpoint}:\n{detail}")
    };
    runtime.did_emsg = true;
    access.with_ex_editor(|editor| {
        crate::excmd_exec::push_text_message(editor, text, true, true);
    });
    Ok(Typval::Number(-1))
}

/// `timer_stop(id)` (upstream `f_timer_stop`): an unknown id is a silent
/// no-op, and the answer is always 0.
fn call_timer_stop<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let Some(first) = args.first() else {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: timer_stop",
        ));
    };
    require_number(first, 1)?;
    if let Some(manager) = runtime.jobs.as_mut()
        && let Typval::Number(number) = first
        && let Ok(id) = u64::try_from(*number)
    {
        manager.timer_stop(id);
    }
    Ok(Typval::Number(0))
}

/// `timer_stopall()` (upstream `f_timer_stopall`): stops every timer.
fn call_timer_stopall<F: FileIO>(runtime: &mut ExRuntime<F>) -> ox_eval::Result<Typval> {
    if let Some(manager) = runtime.jobs.as_mut() {
        manager.timer_stop_all();
    }
    Ok(Typval::Number(0))
}

/// `timer_pause(id, paused)` (upstream `f_timer_pause`): a non-Number id is
/// `E39`, an unknown id is silent, and the answer is always 0.
fn call_timer_pause<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: timer_pause",
        ));
    }
    let Typval::Number(number) = &args[0] else {
        return Err(EvalError::new("E39", 0, "Number expected"));
    };
    let paused = tv_number(&args[1]) != 0;
    if let Some(manager) = runtime.jobs.as_mut()
        && let Ok(id) = u64::try_from(*number)
    {
        manager.timer_pause(id, paused);
    }
    Ok(Typval::Number(0))
}

/// `timer_info([id])` (upstream `f_timer_info`): the timers' dicts, or the
/// single-id list, or empty when nothing matches. Without a loop installed
/// there are no timers to list.
fn call_timer_info<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let wanted = match args.first() {
        None => None,
        Some(value) => {
            require_number(value, 1)?;
            match value {
                Typval::Number(number) => match u64::try_from(*number) {
                    Ok(id) => Some(id),
                    // A negative id names nothing (upstream
                    // `find_timer_by_nr` misses), so the answer is empty.
                    Err(_) => return Ok(Typval::list(Vec::new())),
                },
                // Checked by `require_number` above.
                _ => return Ok(Typval::list(Vec::new())),
            }
        }
    };
    Ok(runtime
        .jobs
        .as_ref()
        .map_or_else(|| Typval::list(Vec::new()), |manager| manager.timer_info(wanted)))
}

/// The installed job manager, creating one when this executor has none yet
/// (the same lazy install `jobstart` uses). A creation failure means there is
/// no event loop to arm timers on.
fn jobs_or_create<F: FileIO>(runtime: &mut ExRuntime<F>) -> ox_eval::Result<&mut JobManager> {
    if runtime.jobs.is_none() {
        match JobManager::new() {
            Ok(manager) => runtime.jobs = Some(manager),
            Err(message) => {
                return Err(EvalError::new(
                    "E523",
                    0,
                    format!("Not allowed here: {message}"),
                ));
            }
        }
    }
    runtime
        .jobs
        .as_mut()
        .ok_or_else(|| EvalError::new("E523", 0, "Not allowed here"))
}

/// `typval.c:tv_check_for_number_arg`: only a Number passes.
fn require_number(value: &Typval, argument: usize) -> ox_eval::Result<()> {
    if matches!(value, Typval::Number(_)) {
        return Ok(());
    }
    Err(EvalError::new(
        "E1210",
        0,
        format!("Number required for argument {argument}"),
    ))
}

/// Unchecked `tv_get_number` coercion (`typval.c`): Bool and `v:null` count
/// as numbers, Strings scan Vim-style, Floats truncate, and anything else
/// counts as 0 without raising.
#[allow(
    clippy::cast_possible_truncation,
    reason = "float-to-int truncation mirrors tv_get_number"
)]
fn tv_number(value: &Typval) -> i64 {
    match value {
        Typval::Number(number) => *number,
        Typval::Bool(value) => i64::from(*value),
        Typval::Float(number) => *number as i64,
        Typval::Special(Special::Null) => 0,
        Typval::String(text) => vim_str2nr(text.as_bytes()),
        Typval::Channel(id) | Typval::Job(id) => id.cast_signed().max(0),
        _ => 0,
    }
}

/// Validate a `timer_start` callback (upstream `callback_from_typval`,
/// `eval.c:4860-4910`): funcrefs pass through, a String names a function —
/// digit-leading is `E921`, empty means no callback (`v:null` in
/// `timer_info`) — and `v:null` or numeric zero means no callback.
fn timer_callback(value: &Typval) -> ox_eval::Result<Typval> {
    match value {
        Typval::Funcref(_) | Typval::Partial(_) => Ok(value.clone()),
        Typval::String(name) => {
            let bytes = name.as_bytes();
            if bytes.is_empty() {
                return Ok(Typval::Special(Special::Null));
            }
            if bytes.first().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(EvalError::new("E921", 0, "Invalid callback argument"));
            }
            Ok(Typval::Funcref(Funcref {
                name: name.clone(),
                args: Vec::new(),
                dict: None,
                registry: None,
            }))
        }
        Typval::Number(0) | Typval::Special(Special::Null) => {
            Ok(Typval::Special(Special::Null))
        }
        Typval::Channel(0) | Typval::Job(0) => Ok(Typval::Special(Special::Null)),
        _ => Err(EvalError::new("E921", 0, "Invalid callback argument")),
    }
}

/// `vim_str2nr` (`strings.c`): leading whitespace, an optional sign, then
/// `0x`-hex, `0`-octal (only when the next digit is `0`-`7`, so `'08'`
/// stays decimal 8), or plain decimal; the unsigned accumulator saturates.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "unsigned-accumulate-then-negate mirrors vim_str2nr"
)]
fn vim_str2nr(bytes: &[u8]) -> i64 {
    let mut index = 0;
    while bytes.get(index).is_some_and(|byte| byte.is_ascii_whitespace()) {
        index += 1;
    }
    let negative = bytes.get(index) == Some(&b'-');
    if negative || bytes.get(index) == Some(&b'+') {
        index += 1;
    }
    let (base, mut at) = if bytes.get(index) == Some(&b'0')
        && matches!(bytes.get(index + 1), Some(b'x') | Some(b'X'))
    {
        (16_u32, index + 2)
    } else if bytes.get(index) == Some(&b'0')
        && matches!(bytes.get(index + 1), Some(b'0'..=b'7'))
    {
        (8_u32, index + 1)
    } else {
        (10_u32, index)
    };
    let mut magnitude: u64 = 0;
    while let Some(byte) = bytes.get(at) {
        let Some(digit) = (*byte as char).to_digit(base) else {
            break;
        };
        magnitude = magnitude
            .saturating_mul(u64::from(base))
            .saturating_add(u64::from(digit));
        at += 1;
    }
    if negative {
        if magnitude > i64::MAX as u64 {
            i64::MIN
        } else {
            -(magnitude as i64)
        }
    } else {
        i64::try_from(magnitude).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use ox_eval::Builtins;
    use ox_types::OxStr;

    use crate::excmd_exec::{EvalHost, ExEditorAccess, VimRegex};
    use crate::{Editor, ExExecutor, TestEditorAccess};

    fn global(scope: &Scope, name: &str) -> Option<Typval> {
        scope
            .global
            .iter()
            .find(|(key, _)| key.to_string_lossy() == name)
            .map(|(_, value)| value.clone())
    }

    fn global_number(scope: &Scope, name: &str) -> Option<i64> {
        match global(scope, name)? {
            Typval::Number(value) => Some(value),
            Typval::Bool(value) => Some(i64::from(value)),
            _ => None,
        }
    }

    /// Drives [`call`] without the name route, so dispatcher logic is
    /// verifiable before the route table lands.
    struct Direct<'a, E: ExEditorAccess> {
        host: EvalHost<'a, crate::script::RealFileIO, E>,
    }

    impl<'a, E: ExEditorAccess> Direct<'a, E> {
        fn new(
            runtime: &'a mut crate::excmd_exec::ExRuntime<crate::script::RealFileIO>,
            access: &'a E,
            scope: &'a mut Scope,
        ) -> (Self, &'a mut Scope) {
            static REGEX: VimRegex = VimRegex;
            let host = EvalHost {
                runtime,
                access,
                lua: None,
                builtins: Builtins::new(&REGEX),
                submatches: None,
                escaped_exception: None,
                call_receiver: None,
            };
            (Self { host }, scope)
        }

        fn call(&mut self, scope: &mut Scope, name: &str, args: &[Typval]) -> Typval {
            super::call(&mut self.host, name, args, scope)
                .unwrap_or_else(|error| panic!("{name} failed: {error}"))
        }

        fn error_code(&mut self, scope: &mut Scope, name: &str, args: &[Typval]) -> &'static str {
            match super::call(&mut self.host, name, args, scope) {
                Err(error) => error.code,
                Ok(value) => panic!("{name} must fail, answered {value:?}"),
            }
        }
    }

    fn dict_entry(value: &Typval, key: &[u8]) -> Option<Typval> {
        match value {
            Typval::Dict(data) => data.borrow().get(key).cloned(),
            _ => None,
        }
    }

    fn dict_number(value: &Typval, key: &[u8]) -> Option<i64> {
        match dict_entry(value, key)? {
            Typval::Number(number) => Some(number),
            _ => None,
        }
    }

    fn one_entry(value: &Typval) -> Typval {
        let Typval::List(items) = value else {
            panic!("timer_info must answer a List, got {value:?}");
        };
        let items = items.borrow();
        assert_eq!(items.items.len(), 1);
        items.items[0].clone()
    }

    fn funcref_name(value: &Typval) -> String {
        match value {
            Typval::Funcref(funcref) => funcref.name.to_string_lossy().into_owned(),
            other => panic!("expected a Funcref, got {other:?}"),
        }
    }

    fn number(value: &Typval) -> i64 {
        match value {
            Typval::Number(number) => *number,
            other => panic!("expected a Number, got {other:?}"),
        }
    }

    /// Mirrors the oracle probe: start answers a monotonic id, `timer_info`
    /// reports `{id, time, paused, repeat, callback}`, pause flips `paused`,
    /// and unknown ids are silent no-ops answering 0 or `[]`.
    #[test]
    fn info_pause_stop_match_oracle() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let (runtime, scope) = exec.runtime_scope_mut();
        let (mut direct, scope) = Direct::new(runtime, &editor, scope);
        let id1 = number(&direct.call(
            scope,
            "timer_start",
            &[Typval::Number(100_000), Typval::String(OxStr::from("Cb"))],
        ));
        assert!(id1 >= 1, "timer ids start at 1, got {id1}");
        let entry = one_entry(&direct.call(scope, "timer_info", &[Typval::Number(id1)]));
        assert_eq!(dict_number(&entry, b"id"), Some(id1));
        assert_eq!(dict_number(&entry, b"time"), Some(100_000));
        assert_eq!(dict_number(&entry, b"paused"), Some(0));
        assert_eq!(dict_number(&entry, b"repeat"), Some(1));
        assert_eq!(
            funcref_name(&dict_entry(&entry, b"callback").unwrap()),
            "Cb"
        );
        direct.call(
            scope,
            "timer_pause",
            &[Typval::Number(id1), Typval::Number(1)],
        );
        let entry = one_entry(&direct.call(scope, "timer_info", &[Typval::Number(id1)]));
        assert_eq!(dict_number(&entry, b"paused"), Some(1));
        direct.call(
            scope,
            "timer_pause",
            &[Typval::Number(id1), Typval::Number(0)],
        );
        let entry = one_entry(&direct.call(scope, "timer_info", &[Typval::Number(id1)]));
        assert_eq!(dict_number(&entry, b"paused"), Some(0));
        let stop_unknown = direct.call(scope, "timer_stop", &[Typval::Number(99_999)]);
        assert_eq!(number(&stop_unknown), 0);
        let missing = direct.call(scope, "timer_info", &[Typval::Number(99_999)]);
        let Typval::List(items) = missing else {
            panic!("timer_info must answer a List");
        };
        assert!(items.borrow().items.is_empty());
        let stopall = direct.call(scope, "timer_stopall", &[]);
        assert_eq!(number(&stopall), 0);
    }

    /// `repeat == 0` means 1 and a negative repeat fires forever (oracle:
    /// `repeat0` shows 1, `repeatneg` shows -1).
    #[test]
    fn repeat_zero_means_once_negative_means_forever() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let (runtime, scope) = exec.runtime_scope_mut();
        let (mut direct, scope) = Direct::new(runtime, &editor, scope);
        let callback = Typval::String(OxStr::from("Cb"));
        let once = number(&direct.call(
            scope,
            "timer_start",
            &[
                Typval::Number(100_000),
                callback.clone(),
                Typval::dict(vec![(OxStr::from("repeat"), Typval::Number(0))]),
            ],
        ));
        let forever = number(&direct.call(
            scope,
            "timer_start",
            &[
                Typval::Number(100_000),
                callback,
                Typval::dict(vec![(OxStr::from("repeat"), Typval::Number(-1))]),
            ],
        ));
        let entry = one_entry(&direct.call(scope, "timer_info", &[Typval::Number(once)]));
        assert_eq!(dict_number(&entry, b"repeat"), Some(1));
        let entry = one_entry(&direct.call(scope, "timer_info", &[Typval::Number(forever)]));
        assert_eq!(dict_number(&entry, b"repeat"), Some(-1));
        direct.call(scope, "timer_stopall", &[]);
    }

    /// A repeating timer fires while `:sleep` pumps the loop. Each `:sleep`
    /// polls the job loop (firing due timers into deferred events) and the
    /// server tick delivers them between commands; this test interleaves
    /// the same two steps, so the callbacks genuinely run across the sleep
    /// window instead of only queueing behind it.
    #[test]
    fn repeating_timer_fires_during_sleep() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            r#"
            let g:count = 0
            function! Tick(timer)
                let g:count += 1
            endfunction
            let g:tid = timer_start(20, 'Tick', {'repeat': -1})
            "#,
        )
        .unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(10);
        loop {
            exec.execute_line(&editor, "sleep 50m").unwrap();
            for event in exec.take_deferred_job_events() {
                exec.invoke_vimscript_job_callback(&editor, event).unwrap();
            }
            // Settle for no less than the 200ms acceptance window of
            // sleep-polling with at least two firings before stopping.
            if started.elapsed() >= Duration::from_millis(200)
                && global_number(exec.scope(), "count").unwrap_or(0) >= 2
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "repeating timer never fired across :sleep polls"
            );
        }
        exec.execute_line(&editor, "call timer_stop(g:tid)")
            .unwrap();
        match exec.evaluate_expression(&editor, "timer_info()").unwrap() {
            Typval::List(items) => assert!(
                items.borrow().items.is_empty(),
                "timer_stop must drop the timer from timer_info"
            ),
            other => panic!("timer_info must answer a List, got {other:?}"),
        }
    }

    /// The `vim.wait` path pumps `loop_poll` plus the scheduled-work drain
    /// (`pump_scheduled`), whose editor half is a loop poll plus deferred
    /// delivery — exactly the two steps below. (A literal Lua `vim.wait`
    /// needs a Lua host, which the `ox-editor` lib harness has no
    /// access to; that end-to-end belongs to the `oxvim` server harness.)
    /// A repeating timer must fire across those turns the same way it does
    /// under `:sleep`.
    #[test]
    fn repeating_timer_fires_across_wait_style_pumps() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            r#"
            let g:count = 0
            function! WaitTick(timer)
                let g:count += 1
            endfunction
            let g:tid = timer_start(20, 'WaitTick', {'repeat': -1})
            "#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            exec.flush_pty_output(&editor).unwrap();
            for event in exec.take_deferred_job_events() {
                exec.invoke_vimscript_job_callback(&editor, event).unwrap();
            }
            if global_number(exec.scope(), "count").unwrap_or(0) >= 2 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "repeating timer never fired across wait-style pumps"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        exec.execute_script(&editor, "<test>", "call timer_stop(g:tid)")
            .unwrap();
    }

    /// Error codes match the oracle: `E39` for a non-Number pause id,
    /// `E1210` for non-Number stop/info ids, and `E119` for missing
    /// arguments. Post-arity `timer_start` failures answer -1 with the
    /// message emitted (`E921` for a bad callback, `E1206` for non-Dict
    /// opts), so the script continues like upstream.
    #[test]
    fn timer_errors_match_oracle() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let (runtime, scope) = exec.runtime_scope_mut();
        let (mut direct, scope) = Direct::new(runtime, &editor, scope);
        let text = |bytes: &[u8]| Typval::String(OxStr::from(bytes));
        assert_eq!(
            direct.error_code(
                scope,
                "timer_pause",
                &[text(b"x"), Typval::Number(1)]
            ),
            "E39"
        );
        assert_eq!(
            direct.error_code(scope, "timer_stop", &[text(b"x")]),
            "E1210"
        );
        assert_eq!(
            direct.error_code(scope, "timer_info", &[text(b"x")]),
            "E1210"
        );
        assert_eq!(direct.error_code(scope, "timer_start", &[]), "E119");
        let bad_callback = direct.call(
            scope,
            "timer_start",
            &[Typval::Number(50), Typval::list(Vec::new())],
        );
        assert_eq!(number(&bad_callback), -1);
        assert!(last_error_message(&editor).contains("E921: Invalid callback argument"));
        let bad_opts = direct.call(
            scope,
            "timer_start",
            &[Typval::Number(50), text(b"Cb"), Typval::Number(1)],
        );
        assert_eq!(number(&bad_opts), -1);
        assert!(last_error_message(&editor).contains("E1206: Dictionary required for argument 3"));
        // `v:null` opts fail the same dictionary check (oracle-probed).
        let null_opts = direct.call(
            scope,
            "timer_start",
            &[
                Typval::Number(50),
                text(b"Cb"),
                Typval::Special(Special::Null),
            ],
        );
        assert_eq!(number(&null_opts), -1);
        assert!(last_error_message(&editor).contains("E1206: Dictionary required for argument 3"));
        // A null-dict sentinel fails the non-null check instead.
        let null_dict_opts = direct.call(
            scope,
            "timer_start",
            &[Typval::Number(50), text(b"Cb"), Typval::null_dict()],
        );
        assert_eq!(number(&null_dict_opts), -1);
        assert!(last_error_message(&editor)
            .contains("E1297: Non-NULL Dictionary required for argument 3"));
    }

    /// Inside `:try` a post-arity failure throws instead of answering -1:
    /// upstream suppresses the display there and the error is catchable.
    #[test]
    fn timer_start_failure_throws_inside_try() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            r#"
            let g:caught = ''
            try
                call timer_start(50, [])
            catch
                let g:caught = v:exception
            endtry
            "#,
        )
        .unwrap();
        match global(exec.scope(), "caught") {
            Some(Typval::String(text)) => assert!(
                text.to_string_lossy().contains("E921"),
                "caught exception must be E921, got {text:?}"
            ),
            other => panic!("expected a caught E921 string, got {other:?}"),
        }
    }

    fn last_error_message(editor: &TestEditorAccess) -> String {
        use crate::editor::MessageKind;
        use ox_types::Object;
        let rendered: Vec<String> = editor
            .editor()
            .messages()
            .iter()
            .filter(|message| message.kind == MessageKind::Error)
            .filter_map(|message| match &message.content {
                Object::String(text) => Some(text.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        rendered.last().cloned().unwrap_or_default()
    }

    /// A timer whose callback always fails stops after three deliveries
    /// (upstream `emsg_count`, `eval.c:5106-5116`). The firing is a real one
    /// collected by polling the loop; each failed delivery is counted
    /// through [`JobManager::record_timer_event_error`] — the same call the
    /// `jobwait` flush and the server tick make on a delivery failure — and
    /// the third failure drops the timer from `timer_info`. A stale fourth
    /// count is ignored, and a same-shaped non-timer event never counts.
    #[test]
    fn forever_erroring_timer_stops_after_three_deliveries() {
        use crate::JobEvent;

        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let (runtime, scope) = exec.runtime_scope_mut();
        let (mut direct, scope) = Direct::new(runtime, &editor, scope);
        let callback = Typval::String(OxStr::from("Cb"));
        let id = number(&direct.call(
            scope,
            "timer_start",
            &[
                Typval::Number(0),
                callback.clone(),
                Typval::dict(vec![(OxStr::from("repeat"), Typval::Number(-1))]),
            ],
        ));
        // Drive the loop once: the zero-timeout firing is collected into the
        // deferred queue exactly as `:sleep` and `vim.wait` pumps collect it.
        {
            let (runtime, _) = exec.runtime_scope_mut();
            let manager = runtime.jobs.as_mut().expect("timer installed a manager");
            let events = manager.poll().expect("loop poll");
            manager.defer_events(events);
        }
        let mut fired = None;
        for event in exec.take_deferred_job_events() {
            assert!(fired.is_none(), "one firing yields one event");
            fired = Some(event);
        }
        let event = fired.expect("zero-timeout timer fires on the first poll");
        {
            let (runtime, _) = exec.runtime_scope_mut();
            let manager = runtime.jobs.as_mut().expect("timer installed a manager");
            // Each strike is one finished delivery: the callback fails, the
            // firing is marked delivered, and a timer stopped on the third
            // strike is freed only once that delivery completes. A stale
            // firing for a freed timer is ignored.
            let timer_id = u64::try_from(id).expect("timer ids are positive");
            for _ in 0..3 {
                assert!(manager.record_timer_event_error(&event));
                manager.timer_delivered(timer_id);
            }
            assert!(
                !manager.record_timer_event_error(&event),
                "stale post-stop firings are ignored"
            );
            let info_id = u64::try_from(id).expect("timer ids are positive");
            let Typval::List(items) = manager.timer_info(Some(info_id)) else {
                panic!("timer_info must answer a List");
            };
            assert!(items.borrow().items.is_empty());
        }
        // A same-shaped non-timer event — one Number arg naming a live timer —
        // must not count: identity is the timer's own receiver `Rc`, which no
        // other event can present.
        let (runtime, scope) = exec.runtime_scope_mut();
        let (mut direct, scope) = Direct::new(runtime, &editor, scope);
        let other = number(&direct.call(scope, "timer_start", &[Typval::Number(100_000), callback]));
        let impostor = JobEvent {
            callback: Typval::String(OxStr::from("Cb")),
            receiver: match Typval::dict(Vec::new()) {
                Typval::Dict(reference) => reference,
                _ => unreachable!("Typval::dict builds a Dict"),
            },
            args: vec![Typval::Number(other)],
        };
        let (runtime, _) = exec.runtime_scope_mut();
        let manager = runtime.jobs.as_mut().expect("timer installed a manager");
        assert!(
            !manager.record_timer_event_error(&impostor),
            "a job event naming a live timer id must not count"
        );
        let other_id = u64::try_from(other).expect("timer ids are positive");
        let Typval::List(items) = manager.timer_info(Some(other_id)) else {
            panic!("timer_info must answer a List");
        };
        assert_eq!(items.borrow().items.len(), 1);
    }
}
