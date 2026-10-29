//! User-defined Vimscript functions and call-frame state.
//!
//! Function bodies remain source text and are executed by [`crate::ExExecutor`]
//! so command/control state has one owner. This module owns names, definition
//! flags, argument binding, local scopes, and `maxfuncdepth` enforcement.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::rc::Rc;

use ox_eval::Scope;
use ox_eval::scope::{ScopeKind, ScopeMap};
use ox_types::{Funcref, OxStr, Typval};

use crate::script::{Sid, SourceContext};

/// Upstream's default `'maxfuncdepth'`.
pub const MAX_FUNC_DEPTH: usize = 100;

/// Independently combinable flags accepted after a `:function` signature.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserFuncFlags(u8);

impl UserFuncFlags {
    /// Abort the function after an uncaught error.
    pub const ABORT: Self = Self(1 << 0);
    /// The function accepts an Ex line range as one invocation.
    pub const RANGE: Self = Self(1 << 1);
    /// The function expects a dictionary receiver (`self`).
    pub const DICT: Self = Self(1 << 2);
    /// The function may capture its defining local scope.
    pub const CLOSURE: Self = Self(1 << 3);

    /// Whether every flag in `other` is enabled.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Enables or disables `flag` without touching the other bits.
    pub const fn set(&mut self, flag: Self, enabled: bool) {
        if enabled {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

/// One user-defined function.
#[derive(Clone, Debug)]
pub struct UserFunc {
    /// Canonical function name. Script-local names use `<SNR>{sid}_Name`.
    pub name: String,
    /// Positional parameter names.
    pub args: Vec<String>,
    /// Source text of trailing default expressions, aligned with the last
    /// `default_args.len()` entries of `args` (userfunc.c `uf_def_args`).
    pub default_args: Vec<String>,
    /// Whether `...` was present.
    pub varargs: bool,
    /// Definition flags.
    pub flags: UserFuncFlags,
    /// Logical source lines forming the body.
    pub body: Vec<String>,
    /// Script context the `:function` was written in, upstream's
    /// `uf_script_ctx`. `map_add` adds the body-relative line to its `lnum`
    /// (`mapping.c:534-535`), which is what makes `maparg()`'s `lnum` point at
    /// the `:map` inside a function body rather than at the body's own line 1.
    pub context: SourceContext,
    /// Optional defining local-scope snapshot for `closure` functions.
    pub captured: ScopeMap,
}

/// How a `dictwatcheradd()` callback spells its function.
///
/// Upstream's `Callback` (`eval/typval.h`): a funcref name, a partial, or a
/// Lua reference. This port has no Lua callbacks on dicts, so a name or a
/// `Funcref`/`Partial` value covers the exercised shapes.
#[derive(Clone, Debug)]
pub enum WatchCallback {
    /// A function name as written (`"g:Changed"` keeps its prefix; call-time
    /// resolution strips `g:` the way `call_func` does).
    Name(OxStr),
    /// A `Funcref` value, cloned at registration with its bound arguments.
    Funcref(Funcref),
    /// A `Partial` value, cloned at registration.
    Partial(Funcref),
    /// A null callback (`v:null` on the add path): registers like any other
    /// watcher but never fires, matching upstream's no-op `callback_call`.
    None,
}

impl WatchCallback {
    /// Whether two callbacks identify the same watcher
    /// (`tv_callback_equal` in `eval/typval.c`): names compare by spelling,
    /// funcrefs by name, partials by name and binding shape (upstream
    /// compares partial identity, unobservable across calls here).
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Name(left), Self::Name(right)) => left == right,
            (Self::Name(name), Self::Funcref(function))
            | (Self::Funcref(function), Self::Name(name)) => &function.name == name,
            (Self::Funcref(left), Self::Funcref(right)) => left.name == right.name,
            (Self::Partial(left), Self::Partial(right)) => {
                left.name == right.name
                    && left.args.len() == right.args.len()
                    && left.dict.is_some() == right.dict.is_some()
                    && left.registry == right.registry
            }
            (Self::None, Self::None) => true,
            _ => false,
        }
    }
}

/// What a dictionary watcher is attached to.
///
/// Upstream attaches watchers to the live scope dicts (`vimvardict`, ...),
/// which this port snapshots per read (`Scope::scope_dict`). A bare `g:` /
/// `b:` / `w:` / `t:` argument therefore needs its scope recorded at the
/// builtin call site (the evaluator knows it; a bare `Typval` does not), so
/// both spellings exist: plain-dict variables key by live `Dict` identity,
/// scope namespaces key by kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WatchTarget {
    /// A scope namespace, resolved by the dictionary builtins.
    Scope(ScopeKind),
    /// One live `Dict` identity (`Rc::as_ptr` of its `DictRef`).
    Dict(usize),
}

/// One `dictwatcheradd()` registration: a key pattern plus its callback.
///
/// Patterns follow `tv_dict_watcher_matches` (`eval/typval.c`): a trailing
/// `*` matches a prefix, anything else matches exactly.
#[derive(Clone, Debug)]
pub struct DictWatcher {
    /// Registry-unique identity, so a firing round can tell duplicate
    /// `(pattern, callback)` registrations apart after one is removed.
    pub id: u64,
    /// Key pattern as written.
    pub pattern: OxStr,
    /// Function to invoke on a matching change.
    pub callback: WatchCallback,
    /// Set while the watcher fires; a `dictwatcherdel()` in that window
    /// defers the removal until the notification round ends.
    pub busy: bool,
    /// Removal requested while `busy`.
    pub needs_free: bool,
}

/// One in-flight dictionary notification round: the watchers that matched
/// when the round began. The registry hands these out and takes them back;
/// user code runs between the two calls holding no registry borrow, so a
/// `dictwatcherdel()` (or a nested firing round) from inside a callback
/// cannot alias the map.
#[derive(Clone, Debug)]
pub struct WatchRound {
    /// The fired target, for the completion call.
    pub target: WatchTarget,
    /// `(watcher id, pattern, callback)` for every watcher that matched.
    pub pending: Vec<(u64, OxStr, WatchCallback)>,
}

impl DictWatcher {
    /// Whether `key` matches the watcher's pattern.
    #[must_use]
    pub fn matches(&self, key: &[u8]) -> bool {
        let pattern = self.pattern.as_bytes();
        if let Some(prefix) = pattern.strip_suffix(b"*") {
            key.starts_with(prefix)
        } else {
            key == pattern
        }
    }
}

/// One active call frame.
#[derive(Clone, Debug)]
pub struct CallFrame {
    /// Canonical function name.
    pub name: String,
    /// Caller-local scope restored after the call.
    caller_local: ScopeMap,
    /// Caller argument scope restored after the call.
    caller_argument: ScopeMap,
    /// One-based function-body line currently executing.
    pub current_line: usize,
    /// Position on the merged execution stack (`estack_T` in
    /// `runtime_defs.h`): one counter shared with script-source frames, so
    /// the throwpoint renderer interleaves both frame kinds in push order.
    pub order: u64,
}

/// User-function definition/call failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserFuncError {
    /// Traditional Vim error code.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl UserFuncError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for UserFuncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for UserFuncError {}

/// Parsed `:function` signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionSignature {
    /// Name as written, before script-local normalization.
    pub name: String,
    /// Ordered positional parameter names.
    pub args: Vec<String>,
    /// Source text of trailing default expressions (`name = expr`), aligned
    /// with the last `default_args.len()` entries of `args`.
    pub default_args: Vec<String>,
    /// Whether the final parameter is `...`.
    pub varargs: bool,
    /// Definition flags.
    pub flags: UserFuncFlags,
}

/// Session-shared dictionary-watcher registrations (`dictwatcheradd()`).
///
/// One `Rc` per session executor pair: primary and nested executors hold the
/// same table through [`UserFunctions::share_watchers_from`], so a watcher
/// installed through either executor fires when the other drains a mutation,
/// and a `dictwatcherdel()` on either is dead on both. Never global, never
/// shared across sessions. The [`UserFunctions::call_stack`] stays
/// per-executor: only registrations (and their [`DictWatcher`] identity
/// counter, so ids stay unique across the pair) are shared.
#[derive(Clone, Debug, Default)]
struct WatcherRegistry {
    /// `dictwatcheradd()` registrations by target.
    watchers: HashMap<WatchTarget, Vec<DictWatcher>>,
    /// Monotonic identity source for [`DictWatcher`] ids.
    next_watcher_id: u64,
}

 /// Executor-owned user-function definitions and active call stack.
 #[derive(Clone, Debug, Default)]
 pub struct UserFunctions {
     functions: Rc<RefCell<BTreeMap<String, UserFunc>>>,
     call_stack: Vec<CallFrame>,
    /// Shared watcher registrations; per-executor call frames stay local.
    watchers: Rc<RefCell<WatcherRegistry>>,
 }

impl UserFunctions {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses `Name(arg, ...) abort range dict closure`.
    ///
    /// # Errors
    ///
    /// Returns the Vim error for a malformed declaration: `E124`/`E125`
    /// for missing parentheses, `E128`/`E129` for an invalid name, `E475`/
    /// `E1068` for a malformed parameter list, `E853` for a duplicate
    /// parameter, `E989` for a non-default argument after a default, and
    /// `E488` for trailing characters.
    pub fn parse_signature(source: &str) -> Result<FunctionSignature, UserFuncError> {
        let source = source.trim();
        let open = source
            .find('(')
            .ok_or_else(|| UserFuncError::new("E124", "Missing '(': function declaration"))?;
        let close = source[open + 1..]
            .find_top_level(b')')
            .map(|offset| open + 1 + offset)
            .ok_or_else(|| UserFuncError::new("E125", "Illegal argument: missing ')'"))?;
        let name = source[..open].trim();
        validate_function_name(name)?;

        let (args, default_args, varargs) =
            parse_arguments(&source[open + 1..close], &source[open + 1..])?;

        let mut flags = UserFuncFlags::default();
        for flag in source[close + 1..].split_ascii_whitespace() {
            match flag {
                "abort" => flags.set(UserFuncFlags::ABORT, true),
                "range" => flags.set(UserFuncFlags::RANGE, true),
                "dict" => flags.set(UserFuncFlags::DICT, true),
                "closure" => flags.set(UserFuncFlags::CLOSURE, true),
                _ => {
                    return Err(UserFuncError::new(
                        "E488",
                        format!("Trailing characters: {flag}"),
                    ));
                }
            }
        }
        Ok(FunctionSignature {
            name: name.to_owned(),
            args,
            default_args,
            varargs,
            flags,
        })
    }

    /// Canonicalizes `s:Name`/`<SID>Name` against the defining SID.
    ///
    /// A `g:Name` definition names the global `Name`, mirroring upstream's
    /// `trans_function_name` stripping `g:` before interning.
    #[must_use]
    pub fn canonical_name(name: &str, sid: Sid) -> String {
        if let Some(global) = name.strip_prefix("g:") {
            return global.to_owned();
        }
        if let Some(local) = name.strip_prefix("s:") {
            return format!("<SNR>{sid}_{local}");
        }
        if let Some(local) = name.strip_prefix("<SID>") {
            return format!("<SNR>{sid}_{local}");
        }
        name.to_owned()
    }

    /// Defines a function. `replace` implements `:function!`.
    ///
    /// # Errors
    ///
    /// Returns `E122` when a function with the canonical name already
    /// exists and `replace` is false.
    pub fn define(
        &mut self,
        signature: FunctionSignature,
        body: Vec<String>,
        context: SourceContext,
        replace: bool,
        scope: &Scope,
    ) -> Result<String, UserFuncError> {
        let name = Self::canonical_name(&signature.name, context.sid);
        let mut functions = self.functions.borrow_mut();
        if functions.contains_key(&name) && !replace {
            return Err(UserFuncError::new(
                "E122",
                format!("Function {name} already exists, add ! to replace it"),
            ));
        }
        let captured = if signature.flags.contains(UserFuncFlags::CLOSURE) {
            scope.local.clone()
        } else {
            ScopeMap::new()
        };
        functions.insert(
            name.clone(),
            UserFunc {
                name: name.clone(),
                args: signature.args,
                default_args: signature.default_args,
                varargs: signature.varargs,
                flags: signature.flags,
                body,
                context,
                captured,
            },
        );
        Ok(name)
    }

    /// Removes a function by canonical name.
    pub fn remove(&mut self, name: &str, sid: Sid) -> bool {
        self.functions
            .borrow_mut()
            .remove(&Self::canonical_name(name, sid))
            .is_some()
    }

    /// Looks up a function, resolving script-local spelling in `sid`.
    #[must_use]
    pub fn get(&self, name: &str, sid: Sid) -> Option<UserFunc> {
        self.functions
            .borrow()
            .get(&Self::canonical_name(name, sid))
            .cloned()
    }

    /// Clones a function for execution without holding a registry borrow.
    #[must_use]
    pub fn resolve(&self, name: &str, sid: Sid) -> Option<UserFunc> {
        self.get(name, sid)
    }

    /// Whether a canonical or script-local function exists.
    #[must_use]
    pub fn contains(&self, name: &str, sid: Sid) -> bool {
        self.functions
            .borrow()
            .contains_key(&Self::canonical_name(name, sid))
    }

    /// Shares definitions with another executor without sharing call frames.
    pub fn share_definitions_from(&mut self, other: &Self) {
        self.functions = Rc::clone(&other.functions);
    }

    /// Shares watcher registrations with another executor without sharing
    /// call frames: both registries observe the same table, so an add or a
    /// deletion on either is live on both. The in-flight `busy`/`needs_free`
    /// flags live in the shared table too, so a round opened on one executor
    /// still guards (and sweeps) watchers touched from the other.
    pub fn share_watchers_from(&mut self, other: &Self) {
        self.watchers = Rc::clone(&other.watchers);
    }

    /// Registers a dictionary watcher (`tv_dict_watcher_add`). Multiple
    /// watchers may share one target, key, and callback.
    pub fn dictwatcher_add(
        &mut self,
        target: WatchTarget,
        pattern: OxStr,
        callback: WatchCallback,
    ) {
        let mut registry = self.watchers.borrow_mut();
        let id = registry.next_watcher_id;
        registry.next_watcher_id = registry.next_watcher_id.wrapping_add(1);
        registry
            .watchers
            .entry(target)
            .or_default()
            .push(DictWatcher {
                id,
                pattern,
                callback,
                busy: false,
                needs_free: false,
            });
    }

    /// Removes the first watcher matching target, pattern, and callback
    /// (`tv_dict_watcher_remove`), honouring in-flight notifications: a
    /// watcher that is currently firing is only marked, and the sweep
    /// happens when its round ends.
    pub fn dictwatcher_remove(
        &mut self,
        target: &WatchTarget,
        pattern: &[u8],
        callback: &WatchCallback,
    ) -> bool {
        let mut registry = self.watchers.borrow_mut();
        let Some(watchers) = registry.watchers.get_mut(target) else {
            return false;
        };
        let position = watchers.iter().position(|watcher| {
            !watcher.needs_free
                && watcher.pattern.as_bytes() == pattern
                && watcher.callback.matches(callback)
        });
        let Some(position) = position else {
            return false;
        };
        if watchers.iter().any(|watcher| watcher.busy) {
            watchers[position].needs_free = true;
        } else {
            watchers.remove(position);
        }
        if watchers.is_empty() {
            registry.watchers.remove(target);
        }
        true
    }

    /// Whether any watcher on any target could observe `key`: the registry
    /// side of the mutation hooks, so hook sites can skip argv work when
    /// nothing watches.
    #[must_use]
    pub fn has_dictwatchers(&self) -> bool {
        !self.watchers.borrow().watchers.is_empty()
    }

    /// Begins a notification round for one key change
    /// (`tv_dict_watcher_notify` in `eval/typval.c`): snapshots every
    /// watcher on `target` whose pattern matches `key` and marks them busy.
    /// The caller invokes every pending callback with
    /// [`Self::dictwatcher_arguments`] (holding no registry borrow while
    /// user code runs), then closes the round with
    /// [`Self::dictwatcher_finish`]. A watcher deleted mid-round stays in
    /// the snapshot and still fires; the finish sweep frees it. Per-watcher
    /// outcomes (continue vs. propagate) are decided by the drain loop.
    pub fn dictwatcher_begin(
        &mut self,
        target: &WatchTarget,
        key: &[u8],
    ) -> Option<WatchRound> {
        let mut registry = self.watchers.borrow_mut();
        let watchers = registry.watchers.get_mut(target)?;
        let mut pending = Vec::new();
        for watcher in watchers.iter_mut() {
            // Upstream's notify loop fires every matcher that is not busy
            // (`eval/typval.c:2090`): a watcher marked `needs_free` by a
            // callback deletion still fires when its turn comes in the same
            // round; the finish sweep frees it. A callback that writes the
            // same dictionary re-enters through a nested round while the
            // outer round still holds `busy`, and must not fire again there.
            if watcher.busy
                || !watcher.matches(key)
                || matches!(watcher.callback, WatchCallback::None)
            {
                continue;
            }
            watcher.busy = true;
            pending.push((watcher.id, watcher.pattern.clone(), watcher.callback.clone()));
        }
        if pending.is_empty() {
            // No flags to clear here: any `busy` still set belongs to an
            // outer round in progress, and clearing it would reopen the
            // re-entrant fire this skip just closed.
            return None;
        }
        Some(WatchRound {
            target: *target,
            pending,
        })
    }

    /// Whether the registry still holds the round watcher `id`: false only
    /// once the entry itself is gone (removed outright, or swept by
    /// [`Self::dictwatcher_finish`]). A `dictwatcherdel()` during the round
    /// only marks `needs_free`, so the watcher stays live and still fires
    /// for the rest of that round. Duplicate `(pattern, callback)`
    /// registrations carry distinct ids, so removing one twin leaves the
    /// other live.
    #[must_use]
    pub fn dictwatcher_is_live(&self, target: &WatchTarget, id: u64) -> bool {
        self.watchers.borrow().watchers.get(target).is_some_and(|watchers| {
            watchers.iter().any(|watcher| watcher.id == id)
        })
    }

    /// Ends a notification round: clears `busy` and sweeps removals that
    pub fn dictwatcher_finish(&mut self, target: &WatchTarget) {
        let mut registry = self.watchers.borrow_mut();
        if let Some(watchers) = registry.watchers.get_mut(target) {
            for watcher in watchers.iter_mut() {
                watcher.busy = false;
            }
            watchers.retain(|watcher| !watcher.needs_free);
            if watchers.is_empty() {
                registry.watchers.remove(target);
            }
        }
    }

    /// Builds the `(dict, key, {new, old})` callback arguments for a key
    /// change: only present values get an entry, so an add carries `{new}`,
    /// a removal `{old}`, and a change both.
    #[must_use]
    pub fn dictwatcher_arguments(
        dict: Typval,
        key: &[u8],
        new_value: Option<Typval>,
        old_value: Option<Typval>,
    ) -> Vec<Typval> {
        let mut change = Vec::with_capacity(2);
        if let Some(value) = new_value {
            change.push((OxStr::from("new"), value));
        }
        if let Some(value) = old_value {
            change.push((OxStr::from("old"), value));
        }
        vec![
            dict,
            Typval::String(OxStr(key.to_vec())),
            Typval::dict(change),
        ]
    }

    /// Whether the named function is present on the active call stack.
    #[must_use]
    pub fn is_active(&self, name: &str, sid: Sid) -> bool {
        let canonical = Self::canonical_name(name, sid);
        self.call_stack.iter().any(|frame| frame.name == canonical)
    }

    /// All functions in canonical name order.
    pub fn iter(&self) -> impl Iterator<Item = (String, UserFunc)> {
        self.functions
            .borrow()
            .iter()
            .map(|(name, function)| (name.clone(), function.clone()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// Begins one call, replacing `l:` and `a:` with frame-local maps.
    /// Returns an owned function descriptor for the executor to run.
    ///
    /// # Errors
    ///
    /// Returns `E132` at the `maxfuncdepth` limit, `E117` for an unknown
    /// function, and `E118`/`E119` for a wrong argument count.
    pub fn begin_call(
        &mut self,
        name: &str,
        sid: Sid,
        values: Vec<Typval>,
        first_line: usize,
        last_line: usize,
        scope: &mut Scope,
        order: u64,
    ) -> Result<UserFunc, UserFuncError> {
        if self.call_stack.len() >= MAX_FUNC_DEPTH {
            return Err(UserFuncError::new(
                "E132",
                "Function call depth is higher than 'maxfuncdepth'",
            ));
        }
        let function = self
            .resolve(name, sid)
            .ok_or_else(|| UserFuncError::new("E117", format!("Unknown function: {name}")))?;
        // check_user_func_argcount: required = uf_args - uf_def_args;
        // calls may omit any trailing defaulted parameters.
        let required = function.args.len() - function.default_args.len();
        if values.len() < required || (!function.varargs && values.len() > function.args.len()) {
            let relation = if values.len() < required {
                "Not enough"
            } else {
                "Too many"
            };
            return Err(UserFuncError::new(
                if values.len() < required {
                    "E119"
                } else {
                    "E118"
                },
                format!("{relation} arguments for function: {}", function.name),
            ));
        }

        let caller_local = std::mem::take(&mut scope.local);
        let caller_argument = std::mem::take(&mut scope.argument);
        scope.local.clone_from(&function.captured);
        for (parameter, value) in function.args.iter().zip(values.iter()) {
            scope
                .argument
                .push((OxStr::from(parameter.as_str()), value.clone()));
        }
        let extras = values
            .into_iter()
            .skip(function.args.len())
            .collect::<Vec<_>>();
        scope
            .argument
            .push((OxStr::from("0"), Typval::Number(vim_number(extras.len()))));
        scope
            .argument
            .push((OxStr::from("000"), Typval::list(extras.clone())));
        for (index, value) in extras.into_iter().enumerate() {
            scope
                .argument
                .push((OxStr((index + 1).to_string().into_bytes()), value));
        }
        scope.argument.push((
            OxStr::from("firstline"),
            Typval::Number(vim_number(first_line)),
        ));
        scope.argument.push((
            OxStr::from("lastline"),
            Typval::Number(vim_number(last_line)),
        ));

        self.call_stack.push(CallFrame {
            name: function.name.clone(),
            caller_local,
            caller_argument,
            current_line: 0,
            order,
        });
        Ok(function)
    }

    /// Ends one call and restores the caller's `l:`/`a:` scopes.
    pub fn end_call(&mut self, scope: &mut Scope) -> Option<CallFrame> {
        let frame = self.call_stack.pop()?;
        scope.local.clone_from(&frame.caller_local);
        scope.argument.clone_from(&frame.caller_argument);
        Some(frame)
    }

    /// Sets the current function-body line for throwpoint rendering.
    pub fn set_current_line(&mut self, line: usize) {
        if let Some(frame) = self.call_stack.last_mut() {
            frame.current_line = line;
        }
    }

    /// Active call frames, outermost first.
    #[must_use]
    pub fn call_stack(&self) -> &[CallFrame] {
        &self.call_stack
    }
}

/// Parses the parameter list between the signature parentheses into
/// `(args, default_args, varargs)`.
fn parse_arguments(
    arg_text: &str,
    source_tail: &str,
) -> Result<(Vec<String>, Vec<String>, bool), UserFuncError> {
    let mut args: Vec<String> = Vec::new();
    let mut default_args: Vec<String> = Vec::new();
    let mut varargs = false;
    let pieces = split_argument_ranges(arg_text);
    let last = pieces.len() - 1;
    for (index, (start, end)) in pieces.into_iter().enumerate() {
        let raw = &arg_text[start..end];
        let argument = raw.trim();
        if argument.is_empty() {
            if index == last {
                // `F(a,)`: upstream tolerates a trailing comma.
                break;
            }
            if arg_text.trim().is_empty() {
                break;
            }
            return Err(UserFuncError::new(
                "E475",
                format!("Invalid argument: {arg_text}"),
            ));
        }
        if varargs {
            // `...` sets mustend upstream; anything after it is invalid.
            return Err(UserFuncError::new(
                "E475",
                format!("Invalid argument: {source_tail}"),
            ));
        }
        if argument == "..." {
            varargs = true;
            continue;
        }
        if index != last && raw.trim_end().len() != raw.len() {
            // get_function_args: no white space allowed before the
            // separating comma.
            let rest = &arg_text[start + raw.trim_end().len()..];
            return Err(UserFuncError::new(
                "E1068",
                format!("No white space allowed before ',': {rest}"),
            ));
        }
        let (parameter, default) = match argument.split_once('=') {
            Some((parameter, rest)) => (parameter.trim(), Some(rest.trim())),
            None => (argument, None),
        };
        if !is_identifier(parameter) || parameter == "firstline" || parameter == "lastline" {
            return Err(UserFuncError::new(
                "E125",
                format!("Illegal argument: {argument}"),
            ));
        }
        if args.iter().any(|existing| existing == parameter) {
            return Err(UserFuncError::new(
                "E853",
                format!("Duplicate argument name: {parameter}"),
            ));
        }
        match default {
            Some(expression) => {
                // get_function_args syntax-checks the default with eval1
                // (parse without evaluation) at definition time.
                if ox_eval::Parser::new(expression.as_bytes()).parse().is_err() {
                    return Err(UserFuncError::new(
                        "E475",
                        format!("Invalid argument: {source_tail}"),
                    ));
                }
                default_args.push(expression.to_owned());
            }
            None => {
                if !default_args.is_empty() {
                    return Err(UserFuncError::new(
                        "E989",
                        "Non-default argument follows default argument",
                    ));
                }
            }
        }
        args.push(parameter.to_owned());
    }
    Ok((args, default_args, varargs))
}

/// Total `usize` -> `i64` conversion for `Typval::Number` line/count values;
/// saturates at `i64::MAX`, the crate-wide sentinel for values beyond the
/// `i64` domain.
fn vim_number(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn validate_function_name(name: &str) -> Result<(), UserFuncError> {
    if name.is_empty() {
        return Err(UserFuncError::new("E129", "Function name required"));
    }
    if let Some((dictionary, member)) = name.rsplit_once('.') {
        let dictionary = dictionary
            .get(2..)
            .filter(|_| dictionary.as_bytes().get(1) == Some(&b':'))
            .unwrap_or(dictionary);
        if is_identifier(member)
            && !dictionary.is_empty()
            && dictionary.split('.').all(is_identifier)
        {
            return Ok(());
        }
        return Err(UserFuncError::new(
            "E128",
            format!("Invalid function name: {name}"),
        ));
    }
    // Upstream `trans_function_name` (`eval/userfunc.c`) skips over `s:` and
    // `g:` before validating: `function! g:Changed()` defines the global
    // `Changed`, which dictionary-watcher callbacks spell `g:Changed`.
    let (unqualified, script_local) = if let Some(local) = name.strip_prefix("s:") {
        (local, true)
    } else if let Some(global) = name.strip_prefix("g:") {
        (global, false)
    } else if let Some(local) = name.strip_prefix("<SID>") {
        (local, true)
    } else if let Some(local) = name
        .strip_prefix("<SNR>")
        .and_then(|local| local.split_once('_').map(|(_, name)| name))
    {
        (local, true)
    } else {
        (name, false)
    };
    if !script_local
        && !unqualified.contains('#')
        && !unqualified
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_uppercase)
    {
        return Err(UserFuncError::new(
            "E128",
            format!("Function name must start with a capital or contain '#': {name}"),
        ));
    }
    if unqualified.split('#').any(|part| !is_identifier(part)) {
        return Err(UserFuncError::new(
            "E128",
            format!("Invalid function name: {name}"),
        ));
    }
    Ok(())
}

fn is_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Byte-scanner used by [`UserFunctions::parse_signature`].
///
/// `get_function_args` walks the argument list with `eval1`, which steps over
/// nested `()`/`[]`/`{}` and quoted strings; comma and closing-paren
/// detection must do the same so default expressions may contain commas,
/// strings, and calls.
trait FindTopLevel {
    fn find_top_level(&self, needle: u8) -> Option<usize>;
}

impl FindTopLevel for str {
    fn find_top_level(&self, needle: u8) -> Option<usize> {
        let bytes = self.as_bytes();
        let mut depth = 0usize;
        let mut index = 0usize;
        while index < bytes.len() {
            match bytes[index] {
                b'\'' | b'"' => skip_quoted(bytes, &mut index),
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    index += 1;
                }
                close @ (b')' | b']' | b'}') => {
                    if close == needle && depth == 0 {
                        return Some(index);
                    }
                    depth = depth.saturating_sub(1);
                    index += 1;
                }
                b',' if needle == b',' && depth == 0 => return Some(index),
                _ => index += 1,
            }
        }
        None
    }
}

/// Advances `index` past one Vim single- or double-quoted string.
fn skip_quoted(bytes: &[u8], index: &mut usize) {
    let quote = bytes[*index];
    *index += 1;
    while *index < bytes.len() {
        if quote == b'"' && bytes[*index] == b'\\' && *index + 1 < bytes.len() {
            *index += 2;
            continue;
        }
        if bytes[*index] == quote {
            if quote == b'\'' && bytes.get(*index + 1) == Some(&b'\'') {
                *index += 2;
                continue;
            }
            *index += 1;
            return;
        }
        *index += 1;
    }
}

/// Splits `text` into `(start, end)` byte ranges at top-level commas.
fn split_argument_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while let Some(position) = text[offset..].find_top_level(b',') {
        ranges.push((offset, offset + position));
        offset += position + 1;
    }
    ranges.push((offset, text.len()));
    ranges
}

#[cfg(test)]
mod g_scope_tests {
    use super::*;

    /// `function! g:Changed()` must parse: upstream `trans_function_name`
    /// (`eval/userfunc.c`) skips `g:` before validating, and the
    /// `dict_notifications` functional suite defines its watcher callback
    /// with that spelling (76 gated tests failed at this `E128`).
    #[test]
    fn g_scoped_definition_parses_and_canonicalizes() {
        let signature = UserFunctions::parse_signature("g:Changed(dict, key, value)").unwrap();
        assert_eq!(signature.name, "g:Changed");
        assert_eq!(UserFunctions::canonical_name(&signature.name, 7), "Changed");
    }

    #[test]
    fn g_scoped_lowercase_still_rejected() {
        let error = UserFunctions::parse_signature("g:changed()").unwrap_err();
        assert_eq!(error.code, "E128");
    }

    #[test]
    fn g_scoped_define_and_lookup_share_global_name() {
        let mut functions = UserFunctions::new();
        let signature = UserFunctions::parse_signature("g:Changed()").unwrap();
        let context = SourceContext::default();
        let scope = Scope::new();
        let canonical = functions
            .define(signature, Vec::new(), context, false, &scope)
            .unwrap();
        assert_eq!(canonical, "Changed");
        assert!(functions.contains("Changed", 0));
        assert!(functions.contains("g:Changed", 0));
        assert!(functions.get("g:Changed", 0).is_some());
    }

    /// `dictwatcheradd()`/`dictwatcherdel()` registry semantics: exact and
    /// prefix patterns, name-equality for callbacks, and removal of only the
    /// fully matching watcher.
    #[test]
    fn dictwatcher_registry_add_match_remove() {
        let mut functions = UserFunctions::new();
        let target = WatchTarget::Dict(7);
        let callback = WatchCallback::Name(OxStr::from("g:Changed"));
        assert!(!functions.has_dictwatchers());
        functions.dictwatcher_add(target, OxStr::from("watched"), callback.clone());
        functions.dictwatcher_add(
            target,
            OxStr::from("wat*"),
            WatchCallback::Name(OxStr::from("g:Other")),
        );
        assert!(functions.has_dictwatchers());
        // A Funcref spelling of the same name matches the String form,
        // mirroring `tv_callback_equal` comparing funcref names.
        let funcref = WatchCallback::Funcref(Funcref {
            name: OxStr::from("g:Changed"),
            args: Vec::new(),
            dict: None,
            registry: None,
        });
        assert!(callback.matches(&funcref));
        // Firing reaches the exact and prefix watchers with
        // `(dict, key, {new, old})`. The round holds no registry borrow
        // while callbacks run: begin, invoke, finish.
        let dict = Typval::dict(vec![(OxStr::from("watched"), Typval::Number(1))]);
        let round = functions
            .dictwatcher_begin(&target, b"watched")
            .expect("watched key matches two watchers");
        assert_eq!(round.pending.len(), 2);
        let arguments = UserFunctions::dictwatcher_arguments(
            dict,
            b"watched",
            Some(Typval::Number(2)),
            Some(Typval::Number(1)),
        );
        let mut fired = Vec::new();
        for (id, _, callback) in &round.pending {
            if functions.dictwatcher_is_live(&target, *id) {
                let name = match callback {
                    WatchCallback::Name(name) => name.to_string_lossy().into_owned(),
                    WatchCallback::Funcref(function) | WatchCallback::Partial(function) => {
                        function.name.to_string_lossy().into_owned()
                    }
                    WatchCallback::None => "<none>".to_owned(),
                };
                fired.push((name, arguments.len()));
            }
        }
        functions.dictwatcher_finish(&target);
        assert_eq!(
            fired,
            vec![("g:Changed".to_owned(), 3), ("g:Other".to_owned(), 3)]
        );
        // Only the fully matching watcher is removed.
        assert!(
            !functions.dictwatcher_remove(
                &target,
                b"watched",
                &WatchCallback::Name(OxStr::from("g:Other")),
            )
        );
        assert!(functions.dictwatcher_remove(&target, b"watched", &callback));
        assert!(functions.has_dictwatchers());
        // An unwatched key opens no round.
        assert!(functions.dictwatcher_begin(&target, b"unwatched").is_none());
        assert!(functions.dictwatcher_remove(
            &target,
            b"wat*",
            &WatchCallback::Name(OxStr::from("g:Other")),
        ));
        assert!(!functions.has_dictwatchers());
    }
}

#[cfg(test)]
mod shared_watcher_registry_tests {
    use super::*;

    /// A watcher installed on one registry is visible to the other after
    /// `share_watchers_from`: a round opened on the second registry fires
    /// for it, the way shared function definitions resolve on both.
    #[test]
    fn watcher_installed_on_one_registry_fires_on_the_other() {
        let mut primary = UserFunctions::new();
        let mut nested = UserFunctions::new();
        nested.share_watchers_from(&primary);
        let target = WatchTarget::Scope(ScopeKind::Global);
        primary.dictwatcher_add(
            target,
            OxStr::from("watched"),
            WatchCallback::Name(OxStr::from("Changed")),
        );
        assert!(nested.has_dictwatchers());
        let round = nested
            .dictwatcher_begin(&target, b"watched")
            .expect("shared watcher matches the other registry's round");
        assert_eq!(round.pending.len(), 1);
        let id = round.pending[0].0;
        assert!(primary.dictwatcher_is_live(&target, id));
        nested.dictwatcher_finish(&target);
        // The shared id counter hands out distinct ids across the pair.
        nested.dictwatcher_add(
            target,
            OxStr::from("watched"),
            WatchCallback::Name(OxStr::from("Other")),
        );
        let round = primary
            .dictwatcher_begin(&target, b"watched")
            .expect("both shared watchers match");
        assert_eq!(round.pending.len(), 2);
        assert_ne!(round.pending[0].0, round.pending[1].0);
        primary.dictwatcher_finish(&target);
    }

    /// A watcher deleted mid-round through one registry stays live for the
    /// rest of the round on both (its pending entry still fires), and is
    /// swept before the next round (`eval/typval.c:2090` skips only `busy`).
    #[test]
    fn watcher_deleted_mid_round_is_dead_on_both_registries() {
        let mut primary = UserFunctions::new();
        let mut nested = UserFunctions::new();
        nested.share_watchers_from(&primary);
        let target = WatchTarget::Dict(7);
        primary.dictwatcher_add(
            target,
            OxStr::from("*"),
            WatchCallback::Name(OxStr::from("First")),
        );
        primary.dictwatcher_add(
            target,
            OxStr::from("*"),
            WatchCallback::Name(OxStr::from("Second")),
        );
        let round = primary
            .dictwatcher_begin(&target, b"key")
            .expect("both watchers match");
        assert_eq!(round.pending.len(), 2);
        let second = round.pending[1].0;
        assert!(nested.dictwatcher_remove(
            &target,
            b"*",
            &WatchCallback::Name(OxStr::from("Second")),
        ));
        // Still live: the pending entry fires later in this same round.
        assert!(primary.dictwatcher_is_live(&target, second));
        assert!(nested.dictwatcher_is_live(&target, second));
        primary.dictwatcher_finish(&target);
        let round = nested
            .dictwatcher_begin(&target, b"key")
            .expect("survivor still matches on the other registry");
        assert_eq!(round.pending.len(), 1);
        assert!(matches!(&round.pending[0].2, WatchCallback::Name(name) if name.as_bytes() == b"First"));
        nested.dictwatcher_finish(&target);
    }
}
