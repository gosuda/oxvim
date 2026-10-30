//! The Vimscript value model (`typval_T`), for `ox-eval`.
//!
//! Mirrors upstream `typval_defs.h` `VarType` kinds. Vimscript strings are
//! byte strings ([`OxStr`]); lists and dictionaries are shared reference types.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::rc::Rc;

use crate::byte_str::OxStr;

/// Unspecified value tag.
pub const VAR_UNKNOWN: u8 = 0;
/// Number value tag.
pub const VAR_NUMBER: u8 = 1;
/// String value tag.
pub const VAR_STRING: u8 = 2;
/// Funcref value tag.
pub const VAR_FUNC: u8 = 3;
/// List value tag.
pub const VAR_LIST: u8 = 4;
/// Dictionary value tag.
pub const VAR_DICT: u8 = 5;
/// Float value tag.
pub const VAR_FLOAT: u8 = 6;
/// Boolean value tag.
pub const VAR_BOOL: u8 = 7;
/// Special value tag.
pub const VAR_SPECIAL: u8 = 8;
/// Partial value tag.
pub const VAR_PARTIAL: u8 = 9;
/// Blob value tag.
pub const VAR_BLOB: u8 = 10;

/// Vimscript special values.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Special {
    /// `v:null` (and `v:none`).
    Null,
}

/// The depth at which a container was locked.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash)]
pub enum LockScope {
    /// No container lock.
    #[default]
    None,
    /// Lock this container only.
    Shallow,
    /// Lock every reachable container.
    Deep,
}

/// Mutable lock metadata stored with every reference container.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash)]
pub struct LockState {
    /// Applied lock depth.
    pub scope: LockScope,
    /// Whether this container rejects mutation.
    pub locked: bool,
}
/// Mutability flags stored on a single Dictionary entry.
///
/// These mirror upstream's `dictitem_T.di_flags`. The allocation bit has no
/// Rust equivalent because ownership is represented by the type system.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash)]
pub struct DictEntryFlags(u8);

impl DictEntryFlags {
    /// An ordinary mutable entry.
    pub const NONE: Self = Self(0);
    /// The entry's value may not be replaced.
    pub const READ_ONLY: Self = Self(1);
    /// Read-only only while evaluating in a sandbox.
    pub const READ_ONLY_SANDBOX: Self = Self(2);
    /// The entry may not be removed.
    pub const FIXED: Self = Self(4);
    /// The entry was locked with `:lockvar`.
    pub const LOCKED: Self = Self(8);

    /// Whether all bits in `other` are set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any bit in `other` is set.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether no flags are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for DictEntryFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for DictEntryFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Lock status stored on an individual Dictionary entry's value.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash)]
pub enum EntryValueLock {
    /// The value may be changed.
    #[default]
    Unlocked,
    /// The value was locked with `:lockvar`.
    Locked,
    /// The value is owned by Vim and cannot be locked or replaced.
    Fixed,
}

/// One ordered Dictionary entry.
#[derive(Clone, Debug)]
pub struct DictEntry {
    /// Entry key.
    pub key: OxStr,
    /// Entry value.
    pub value: Typval,
    /// Entry mutability flags.
    pub flags: DictEntryFlags,
    /// Lock status of the entry's value.
    pub value_lock: EntryValueLock,
}

impl DictEntry {
    /// Construct an ordinary mutable entry.
    #[must_use]
    pub fn new(key: OxStr, value: Typval) -> Self {
        Self {
            key,
            value,
            flags: DictEntryFlags::NONE,
            value_lock: EntryValueLock::Unlocked,
        }
    }

    /// Construct an entry with explicit mutability metadata.
    #[must_use]
    pub fn marked(
        key: OxStr,
        value: Typval,
        flags: DictEntryFlags,
        value_lock: EntryValueLock,
    ) -> Self {
        Self {
            key,
            value,
            flags,
            value_lock,
        }
    }

    /// Clone the key and value into a new ordinary mutable entry.
    #[must_use]
    pub fn copy_cloned(&self) -> Self {
        Self::new(self.key.clone(), self.value.clone())
    }
}

/// Mutable payload of a shared List reference.
#[derive(Clone)]
pub struct ListData {
    /// Ordered List items.
    pub items: Vec<Typval>,
    /// Container lock metadata.
    pub lock: LockState,
}

/// Mutable payload of a shared Dictionary reference.
#[derive(Clone)]
pub struct DictData {
    /// Ordered Dictionary entries.
    pub entries: Vec<DictEntry>,
    /// Container lock metadata.
    pub lock: LockState,
    /// Whether this is the null dictionary sentinel.
    is_null: bool,
}

impl DictData {
    /// Look up an entry's value by byte key.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&Typval> {
        self.get_entry(key).map(|entry| &entry.value)
    }

    /// Look up an entry and its metadata by byte key.
    #[must_use]
    pub fn get_entry(&self, key: &[u8]) -> Option<&DictEntry> {
        self.entries
            .iter()
            .find(|entry| entry.key.as_bytes() == key)
    }
}

/// Shared List identity.
pub type ListRef = Rc<RefCell<ListData>>;
/// Shared Dictionary identity.
pub type DictRef = Rc<RefCell<DictData>>;

/// A function reference — plain named function or registered closure.
/// Named function or registered closure reference.
#[derive(Clone, PartialEq)]
pub struct Funcref {
    /// Function name.
    pub name: OxStr,
    /// Bound arguments.
    pub args: Vec<Typval>,
    /// Optional bound self dictionary.
    pub dict: Option<Vec<(OxStr, Typval)>>,
    /// Closure registry identity, for lambdas.
    pub registry: Option<usize>,
}

/// A Vimscript value (`typval_T`).
/// A Vimscript runtime value.
#[derive(Clone)]
pub enum Typval {
    /// Signed integer.
    Number(i64),
    /// Floating-point number.
    Float(f64),
    /// Byte string.
    String(OxStr),
    /// Byte blob.
    Blob(Vec<u8>),
    /// Shared List.
    List(ListRef),
    /// Shared ordered Dictionary.
    Dict(DictRef),
    /// Named function reference.
    Funcref(Funcref),
    /// Closure or partially-bound function.
    Partial(Funcref),
    /// Boolean.
    Bool(bool),
    /// Special value.
    Special(Special),
    /// Channel identifier, represented as a Number upstream.
    Channel(u64),
    /// Job identifier, represented as a Number upstream.
    Job(u64),
}

impl Typval {
    /// Construct a new unlocked List identity.
    #[must_use]
    pub fn list(items: Vec<Self>) -> Self {
        Self::List(Rc::new(RefCell::new(ListData {
            items,
            lock: LockState::default(),
        })))
    }

    /// Construct a new unlocked ordered Dictionary identity.
    #[must_use]
    pub fn dict(entries: Vec<(OxStr, Self)>) -> Self {
        Self::dict_with_entries(
            entries
                .into_iter()
                .map(|(key, value)| DictEntry::new(key, value))
                .collect(),
        )
    }

    /// Construct a Dictionary from entries carrying explicit metadata.
    #[must_use]
    pub fn dict_with_entries(entries: Vec<DictEntry>) -> Self {
        Self::Dict(Rc::new(RefCell::new(DictData {
            entries,
            lock: LockState::default(),
            is_null: false,
        })))
    }

    /// Construct the null dictionary sentinel.
    #[must_use]
    pub fn null_dict() -> Self {
        Self::Dict(Rc::new(RefCell::new(DictData {
            entries: Vec::new(),
            lock: LockState::default(),
            is_null: true,
        })))
    }

    /// Return whether this value is the null dictionary sentinel.
    #[must_use]
    pub fn is_null_dict(&self) -> bool {
        matches!(self, Self::Dict(dict) if dict.try_borrow().is_ok_and(|data| data.is_null))
    }

    /// Return the upstream numeric `VAR_*` tag.
    #[must_use]
    pub const fn vartype(&self) -> u8 {
        match self {
            Self::Number(_) | Self::Channel(_) | Self::Job(_) => VAR_NUMBER,
            Self::Float(_) => VAR_FLOAT,
            Self::String(_) => VAR_STRING,
            Self::Blob(_) => VAR_BLOB,
            Self::List(_) => VAR_LIST,
            Self::Dict(_) => VAR_DICT,
            Self::Funcref(_) => VAR_FUNC,
            Self::Partial(_) => VAR_PARTIAL,
            Self::Bool(_) => VAR_BOOL,
            Self::Special(_) => VAR_SPECIAL,
        }
    }

    /// Vimscript `tv2bool()` truthiness. A conflicting mutable borrow is never
    /// observable during well-formed evaluation; treating it as empty keeps
    /// this infallible query from panicking during host re-entry.
    #[must_use]
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Number(n) => *n != 0,
            Self::Float(f) => *f != 0.0,
            Self::String(s) => !s.as_bytes().is_empty(),
            Self::Blob(b) => !b.is_empty(),
            Self::List(list) => list.try_borrow().is_ok_and(|data| !data.items.is_empty()),
            Self::Dict(dict) => dict.try_borrow().is_ok_and(|data| !data.entries.is_empty()),
            Self::Funcref(funcref) => !funcref.name.as_bytes().is_empty(),
            Self::Partial(_) => true,
            Self::Bool(value) => *value,
            Self::Special(Special::Null) => false,
            Self::Channel(id) | Self::Job(id) => *id != 0,
        }
    }
}

/// Format a float exactly as `:echo` stringifies it.
///
/// This mirrors `TYPVAL_ENCODE_CONV_FLOAT` (`eval/encode.c`): finite values
/// render with `vim_snprintf` `%g` handling (`strings.c`), which rewrites
/// `%g` to plain `%f` or `%e` — so six digits after the point (the C default
/// precision), not six significant digits: `%f`-style for `abs` in
/// `[0.001, 1e7)` (and zero), `%e`-style elsewhere, trailing zeroes stripped
/// while keeping the `.0` suffix — and
/// NaN and the infinities render as re-readable `str2float()` calls, since
/// `%g` cannot round-trip them. `string()` shares this spelling
/// (`vim_float_string` in `ox-eval`); plain `Float`-to-`String` coercion
/// (`tv_get_string_buf_chk`) instead prints bare `nan`/`inf`.
#[must_use]
pub fn vim_float_text(value: f64) -> String {
    if value.is_nan() {
        return "str2float('nan')".to_owned();
    }
    if value.is_infinite() {
        let sign = if value > 0.0 { "" } else { "-" };
        return format!("{sign}str2float('inf')");
    }
    let abs = value.abs();
    if abs == 0.0 || (0.001..10_000_000.0).contains(&abs) {
        let mut text = format!("{value:.6}");
        strip_float_zeros(&mut text);
        text
    } else {
        let rendered = format!("{value:.6e}");
        let (mantissa, exponent) = match rendered.split_once('e') {
            Some((m, e)) => (m, e),
            None => (rendered.as_str(), ""),
        };
        let mut mantissa = mantissa.to_owned();
        strip_float_zeros(&mut mantissa);
        let exponent = normalize_float_exponent(exponent);
        format!("{mantissa}e{exponent}")
    }
}

/// Strip trailing mantissa zeroes the way `%g` does, keeping the lone digit
/// after the dot (`1.000000` becomes `1.0`, never `1`). One condition covers
/// every input: stop once the trailing zero is the kept digit after the dot.
/// Callers pass a mantissa that always contains a dot, and `ends_with` is
/// total, so no length guard is needed.
fn strip_float_zeros(text: &mut String) {
    while text.ends_with('0') && !text.ends_with(".0") {
        text.pop();
    }
}

/// Drop the `+` and leading exponent zeroes `vim_snprintf` removes
/// (`1.0e+07` becomes `1.0e7`). Rust never emits either, so this is a
/// defensive mirror of the `STRMOVE` cleanup rather than live behavior.
fn normalize_float_exponent(exponent: &str) -> String {
    let (sign, digits) = match exponent.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", exponent.strip_prefix('+').unwrap_or(exponent)),
    };
    let digits = digits.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    format!("{sign}{digits}")
}

impl Typval {
    /// `:echo` spelling of this value: top-level strings render raw, nested
    /// strings render single-quoted, matching `typval_to_display(value,
    /// false)` in `excmd_exec.rs`. Container cycles render as `[...]`/`{...}`
    /// instead of recursing forever.
    fn fmt_echo(
        &self,
        formatter: &mut fmt::Formatter<'_>,
        quoted_strings: bool,
        seen: &mut Vec<(u8, *const ())>,
    ) -> fmt::Result {
        match self {
            Self::Number(value) => write!(formatter, "{value}"),
            Self::Float(value) => formatter.write_str(&vim_float_text(*value)),
            Self::String(value) => {
                if quoted_strings {
                    formatter.write_str("'")?;
                    formatter.write_str(&value.to_string_lossy().replace('\'', "''"))?;
                    formatter.write_str("'")
                } else {
                    formatter.write_str(&value.to_string_lossy())
                }
            }
            Self::Blob(bytes) => {
                formatter.write_str("0z")?;
                for byte in bytes {
                    write!(formatter, "{byte:02X}")?;
                }
                Ok(())
            }
            Self::List(list) => {
                let identity = (VAR_LIST, Rc::as_ptr(list).cast::<()>());
                if seen.contains(&identity) {
                    return formatter.write_str("[...]");
                }
                let items = list
                    .try_borrow()
                    .map_err(|_| fmt::Error)?
                    .items
                    .clone();
                seen.push(identity);
                formatter.write_str("[")?;
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    item.fmt_echo(formatter, true, seen)?;
                }
                seen.pop();
                formatter.write_str("]")
            }
            Self::Dict(dict) => {
                let identity = (VAR_DICT, Rc::as_ptr(dict).cast::<()>());
                if seen.contains(&identity) {
                    return formatter.write_str("{...}");
                }
                let entries = dict
                    .try_borrow()
                    .map_err(|_| fmt::Error)?
                    .entries
                    .clone();
                seen.push(identity);
                formatter.write_str("{")?;
                for (index, entry) in entries.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(
                        formatter,
                        "'{}': ",
                        entry.key.to_string_lossy().replace('\'', "''")
                    )?;
                    entry.value.fmt_echo(formatter, true, seen)?;
                }
                seen.pop();
                formatter.write_str("}")
            }
            Self::Funcref(function) | Self::Partial(function) => write!(
                formatter,
                "function('{}')",
                function.name.to_string_lossy()
            ),
            Self::Bool(value) => formatter.write_str(if *value { "v:true" } else { "v:false" }),
            Self::Special(Special::Null) => formatter.write_str("v:null"),
            Self::Channel(id) | Self::Job(id) => write!(formatter, "{id}"),
        }
    }
}

impl fmt::Display for Typval {
    /// `:echo` spelling; see [`Typval::fmt_echo`].
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_echo(formatter, false, &mut Vec::new())
    }
}

/// One echo renderer shared by both string-quoting modes below.
struct Echo<'a> {
    value: &'a Typval,
    quoted_strings: bool,
}

impl fmt::Display for Echo<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value
            .fmt_echo(formatter, self.quoted_strings, &mut Vec::new())
    }
}

/// `:echo` spelling of `value`: top-level strings render raw, exactly what
/// `:echo` prints. See [`echo_text_quoted`] for the container-nested mode.
/// Together these let the editor lane delete its own renderer.
#[must_use]
pub fn echo_text(value: &Typval) -> String {
    Echo {
        value,
        quoted_strings: false,
    }
    .to_string()
}

/// `:echo` spelling of `value` with strings single-quoted: what `:echo`
/// prints for strings nested in containers, matching
/// `typval_to_display(value, true)` in `excmd_exec.rs`.
#[must_use]
pub fn echo_text_quoted(value: &Typval) -> String {
    Echo {
        value,
        quoted_strings: true,
    }
    .to_string()
}

impl PartialEq for Typval {
    fn eq(&self, other: &Self) -> bool {
        fn equal(left: &Typval, right: &Typval, seen: &mut HashSet<(usize, usize, u8)>) -> bool {
            match (left, right) {
                (Typval::Number(a), Typval::Number(b)) => a == b,
                // Upstream tv_equal (typval.c:4090) compares v_float with
                // `==` exactly; an epsilon would change Vimscript semantics.
                #[allow(clippy::float_cmp)]
                (Typval::Float(a), Typval::Float(b)) => a == b,
                (Typval::String(a), Typval::String(b)) => a == b,
                (Typval::Blob(a), Typval::Blob(b)) => a == b,
                (Typval::Bool(a), Typval::Bool(b)) => a == b,
                (Typval::Special(a), Typval::Special(b)) => a == b,
                (Typval::Channel(a), Typval::Channel(b)) | (Typval::Job(a), Typval::Job(b)) => {
                    a == b
                }
                (Typval::Funcref(a), Typval::Funcref(b))
                | (Typval::Partial(a), Typval::Partial(b)) => a == b,
                (Typval::List(a), Typval::List(b)) => {
                    let pair = (Rc::as_ptr(a) as usize, Rc::as_ptr(b) as usize, VAR_LIST);
                    if !seen.insert(pair) {
                        return true;
                    }
                    let (Ok(a), Ok(b)) = (a.try_borrow(), b.try_borrow()) else {
                        return false;
                    };
                    let left = a.items.clone();
                    let right = b.items.clone();
                    drop(a);
                    drop(b);
                    left.len() == right.len()
                        && left.iter().zip(&right).all(|(a, b)| equal(a, b, seen))
                }
                (Typval::Dict(a), Typval::Dict(b)) => {
                    let pair = (Rc::as_ptr(a) as usize, Rc::as_ptr(b) as usize, VAR_DICT);
                    if !seen.insert(pair) {
                        return true;
                    }
                    let (Ok(a), Ok(b)) = (a.try_borrow(), b.try_borrow()) else {
                        return false;
                    };
                    let left = a.entries.clone();
                    let right = b.entries.clone();
                    drop(a);
                    drop(b);
                    left.len() == right.len()
                        && left.iter().all(|entry| {
                            right
                                .iter()
                                .find(|candidate| candidate.key == entry.key)
                                .is_some_and(|other| equal(&entry.value, &other.value, seen))
                        })
                }
                _ => false,
            }
        }

        equal(self, other, &mut HashSet::new())
    }
}

impl fmt::Debug for Funcref {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Funcref")
            .field("name", &self.name)
            .field("args", &self.args)
            .field("dict", &self.dict)
            .field("registry", &self.registry)
            .finish()
    }
}

impl fmt::Debug for Typval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn write_value(
            value: &Typval,
            f: &mut fmt::Formatter<'_>,
            active: &mut HashSet<(usize, u8)>,
        ) -> fmt::Result {
            match value {
                Typval::List(list) => {
                    let key = (Rc::as_ptr(list) as usize, VAR_LIST);
                    if !active.insert(key) {
                        return f.write_str("List([...])");
                    }
                    f.write_str("List([")?;
                    let items = list.try_borrow().map_err(|_| fmt::Error)?.items.clone();
                    for (index, item) in items.iter().enumerate() {
                        if index != 0 {
                            f.write_str(", ")?;
                        }
                        write_value(item, f, active)?;
                    }
                    active.remove(&key);
                    f.write_str("])")
                }
                Typval::Dict(dict) => {
                    let key = (Rc::as_ptr(dict) as usize, VAR_DICT);
                    if !active.insert(key) {
                        return f.write_str("Dict({...})");
                    }
                    f.write_str("Dict({")?;
                    let entries = dict.try_borrow().map_err(|_| fmt::Error)?.entries.clone();
                    for (index, entry) in entries.iter().enumerate() {
                        if index != 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{:?}: ", entry.key)?;
                        write_value(&entry.value, f, active)?;
                    }
                    active.remove(&key);
                    f.write_str("})")
                }
                Typval::Number(v) => f.debug_tuple("Number").field(v).finish(),
                Typval::Float(v) => f.debug_tuple("Float").field(v).finish(),
                Typval::String(v) => f.debug_tuple("String").field(v).finish(),
                Typval::Blob(v) => f.debug_tuple("Blob").field(v).finish(),
                Typval::Funcref(v) => f.debug_tuple("Funcref").field(v).finish(),
                Typval::Partial(v) => f.debug_tuple("Partial").field(v).finish(),
                Typval::Bool(v) => f.debug_tuple("Bool").field(v).finish(),
                Typval::Special(v) => f.debug_tuple("Special").field(v).finish(),
                Typval::Channel(v) => f.debug_tuple("Channel").field(v).finish(),
                Typval::Job(v) => f.debug_tuple("Job").field(v).finish(),
            }
        }

        write_value(self, formatter, &mut HashSet::new())
    }
}

#[cfg(test)]
mod tests {
    use super::{Funcref, LockScope, Special, Typval};
    use crate::byte_str::OxStr;

    #[test]
    fn vartype_and_truthiness_follow_upstream() {
        assert_eq!(Typval::list(vec![]).vartype(), super::VAR_LIST);
        assert_eq!(Typval::dict(vec![]).vartype(), super::VAR_DICT);
        assert!(!Typval::list(vec![]).is_truthy());
        assert!(Typval::list(vec![Typval::Number(0)]).is_truthy());
        assert!(!Typval::String(OxStr::from("")).is_truthy());
        assert!(Typval::String(OxStr::from("0")).is_truthy());
        assert_eq!(super::VAR_UNKNOWN, 0);
        assert_eq!(super::VAR_BLOB, 10);
    }

    #[test]
    fn clone_shares_container_and_equality_handles_cycles() {
        let list = Typval::list(vec![]);
        let Typval::List(reference) = &list else {
            return;
        };
        reference.borrow_mut().items.push(list.clone());
        let alias = list.clone();
        assert_eq!(list, alias);
        assert_eq!(format!("{list:?}"), "List([List([...])])");
    }

    #[test]
    fn lock_state_defaults_unlocked() {
        let Typval::List(list) = Typval::list(vec![]) else {
            return;
        };
        let data = list.borrow();
        assert!(!data.lock.locked);
        assert_eq!(data.lock.scope, LockScope::None);
    }

    #[test]
    fn funcref_types_remain_distinct() {
        let function = Funcref {
            name: OxStr::from("g:fn"),
            args: vec![],
            dict: None,
            registry: None,
        };
        assert_eq!(Typval::Funcref(function.clone()).vartype(), super::VAR_FUNC);
        assert_eq!(Typval::Partial(function).vartype(), super::VAR_PARTIAL);
        assert_eq!(Typval::Special(Special::Null).vartype(), super::VAR_SPECIAL);
    }

    #[test]
    fn float_special_values_follow_vim_spelling() {
        // `echo_spec.lua` dumps NaN/infinities as re-readable `str2float()`
        // calls (`TYPVAL_ENCODE_CONV_FLOAT`, `encode.c`), not bare `%g` text.
        assert_eq!(Typval::Float(f64::NAN).to_string(), "str2float('nan')");
        assert_eq!(Typval::Float(-f64::NAN).to_string(), "str2float('nan')");
        assert_eq!(
            Typval::Float(f64::INFINITY).to_string(),
            "str2float('inf')"
        );
        assert_eq!(
            Typval::Float(f64::NEG_INFINITY).to_string(),
            "-str2float('inf')"
        );
    }

    #[test]
    fn float_echo_uses_six_fraction_digits() {
        assert_eq!(Typval::Float(0.0).to_string(), "0.0");
        assert_eq!(Typval::Float(-0.0).to_string(), "-0.0");
        assert_eq!(Typval::Float(1.5).to_string(), "1.5");
        assert_eq!(Typval::Float(-1.5).to_string(), "-1.5");
        assert_eq!(Typval::Float(0.001).to_string(), "0.001");
        assert_eq!(Typval::Float(0.0009).to_string(), "9.0e-4");
        assert_eq!(Typval::Float(10_000_000.0).to_string(), "1.0e7");
        assert_eq!(
            Typval::Float(1_234_567.891_234_567_9).to_string(),
            "1234567.891235"
        );
        assert_eq!(Typval::Float(1.234_568e-20).to_string(), "1.234568e-20");
    }

    #[test]
    fn float_echo_trims_but_keeps_dot_zero() {
        assert_eq!(Typval::Float(5.0).to_string(), "5.0");
        assert_eq!(Typval::Float(2.000_000_1).to_string(), "2.0");
        assert_eq!(Typval::Float(1.999_999_999_9).to_string(), "2.0");
        assert_eq!(Typval::Float(9_999_999.5).to_string(), "9999999.5");
    }

    #[test]
    fn echo_spec_table_float_values_render_per_reference() {
        // Exact table from `test/functional/ex_cmds/echo_spec.lua` lines 54-107:
        // "used to represent floating-point values"
        let table = [
            // NaN values
            (f64::NAN, "str2float('nan')"),
            (-f64::NAN, "str2float('nan')"),
            // Infinite values
            (f64::INFINITY, "str2float('inf')"),
            (f64::NEG_INFINITY, "-str2float('inf')"),
            // Regular values
            (1.5, "1.5"),
            (1.56000e-020, "1.56e-20"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            // At most six digits after the decimal point
            (1.23456789123456789123456789e-020, "1.234568e-20"),
            (1.23456789123456789123456789, "1.234568"),
            // At most seven digits before the decimal point
            (1234567.89123456789123456789, "1234567.891235"),
            (12345678.9123456789123456789, "1.234568e7"),
            // Negative values
            (-1.5, "-1.5"),
            (-1.56000e-020, "-1.56e-20"),
            (-1.23456789123456789123456789e-020, "-1.234568e-20"),
            (-1.23456789123456789123456789, "-1.234568"),
            (-1234567.89123456789123456789, "-1234567.891235"),
            (-12345678.9123456789123456789, "-1.234568e7"),
        ];

        for (val, expected) in table {
            let tv = Typval::Float(val);
            assert_eq!(super::vim_float_text(val), expected);
            assert_eq!(tv.to_string(), expected);
            assert_eq!(super::echo_text(&tv), expected);
            assert_eq!(super::echo_text_quoted(&tv), expected);
        }
    }

    #[test]
    fn display_echo_scalars_match_upstream() {
        assert_eq!(Typval::Number(41).to_string(), "41");
        assert_eq!(
            Typval::String(OxStr::from("hi")).to_string(),
            "hi"
        );
        assert_eq!(Typval::Bool(true).to_string(), "v:true");
        assert_eq!(Typval::Bool(false).to_string(), "v:false");
        assert_eq!(Typval::Special(Special::Null).to_string(), "v:null");
    }

    #[test]
    fn echo_text_modes_quote_nested_strings() {
        let list = Typval::list(vec![Typval::String(OxStr::from("it's"))]);
        assert_eq!(super::echo_text(&list), "['it''s']");
        assert_eq!(super::echo_text_quoted(&list), "['it''s']");
        assert_eq!(
            super::echo_text(&Typval::String(OxStr::from("it's"))),
            "it's"
        );
        assert_eq!(
            super::echo_text_quoted(&Typval::String(OxStr::from("it's"))),
            "'it''s'"
        );
    }
}
