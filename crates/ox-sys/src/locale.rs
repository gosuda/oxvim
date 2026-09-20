//! Locale inspection and mutation through the C library's `setlocale(3)`.
//!
//! Nvim's `:language` command (`os/lang.c` `ex_language`) delegates locale
//! validity, per-category state, and the current-locale queries behind
//! `v:lang`/`v:ctype` to the C library. This module is the audited unsafe
//! boundary for those calls, mirroring `os/lang.c` `get_locale_val`.

use std::ffi::{CStr, CString, c_char, c_int};

/// Locale category understood by [`current_locale`] and [`set_locale`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocaleCategory {
    /// `LC_ALL` — every category at once.
    All,
    /// `LC_MESSAGES` — message translation language.
    Messages,
    /// `LC_CTYPE` — character classification and encoding.
    CType,
    /// `LC_TIME` — date and time formatting.
    Time,
    /// `LC_COLLATE` — string collation order.
    Collate,
    /// `LC_NUMERIC` — number formatting.
    Numeric,
}

impl LocaleCategory {
    /// Windows has no C-library message category; the environment owns it.
    #[cfg_attr(
        not(windows),
        expect(clippy::unnecessary_wraps, reason = "Windows has an environment-owned category without a C constant")
    )]
    fn raw(self) -> Option<c_int> {
        Some(match self {
            Self::All => libc::LC_ALL,
            #[cfg(not(windows))]
            Self::Messages => libc::LC_MESSAGES,
            #[cfg(windows)]
            Self::Messages => return None,
            Self::CType => libc::LC_CTYPE,
            Self::Time => libc::LC_TIME,
            Self::Collate => libc::LC_COLLATE,
            Self::Numeric => libc::LC_NUMERIC,
        })
    }
}

/// Returns the process's current locale for `category` — the
/// `setlocale(category, NULL)` query behind `get_locale_val`.
///
/// The C library always answers a query; `None` is kept for the
/// null-pointer shape callers translate to an empty string upstream.
#[must_use]
pub fn current_locale(category: LocaleCategory) -> Option<String> {
    let Some(category) = category.raw() else {
        return message_locale_from_environment();
    };
    // SAFETY: a NULL locale is a pure query that never mutates locale state.
    // The returned pointer into the C library's static buffer is copied by
    // `cstr_to_string` before this function returns.
    let value = unsafe { libc::setlocale(category, std::ptr::null()) };
    cstr_to_string(value)
}

/// Sets the locale for `category` to `name`, returning the effective locale
/// string, or `None` when the C library rejects `name` (`setlocale` NULL —
/// the signal upstream reports as E197).
///
/// # Safety contract
///
/// `setlocale` mutates process-wide locale state and returns a pointer into
/// a library-owned buffer. Callers must invoke this only on the main thread
/// during initialization or script execution — the same process-wide
/// exclusion contract as [`crate::set_env`] — and must not query or set
/// locales from other threads concurrently. The returned `String` is copied
/// before returning, so it stays valid after later `setlocale` calls.
// Not `#[must_use]`: the call itself performs the mutation; the `Option` is
// only the success report, and best-effort callers may legitimately drop it.
#[allow(clippy::must_use_candidate)]
pub fn set_locale(category: LocaleCategory, name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    let Some(category) = category.raw() else {
        // ex_language accepts message-catalog names without setlocale when
        // LC_MESSAGES is absent. Its caller updates the environment afterward.
        return Some(String::new());
    };
    // SAFETY: `name` is a valid NUL-terminated C string for the duration of
    // the call. Interior NUL bytes were rejected above, so no silent locale
    // truncation is possible. The returned static-buffer pointer is copied
    // by `cstr_to_string` before this function returns.
    let value = unsafe { libc::setlocale(category, name.as_ptr()) };
    cstr_to_string(value)
}

/// Mirrors `get_mess_env` on platforms without `LC_MESSAGES`.
fn message_locale_from_environment() -> Option<String> {
    for name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        let Some(first) = value.as_bytes().first() else {
            continue;
        };
        if name == "LANG" && first.is_ascii_digit() {
            continue;
        }
        return Some(value);
    }
    // get_mess_env falls back to LC_CTYPE when LANG is absent or numeric.
    current_locale(LocaleCategory::CType)
}

fn cstr_to_string(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: non-NULL `setlocale` results are NUL-terminated strings owned
    // by the C library that remain valid until the next `setlocale` call;
    // copying here closes that window for the caller.
    Some(
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::{LocaleCategory, set_locale};

    #[test]
    fn locale_names_reject_embedded_nul() {
        assert!(set_locale(LocaleCategory::Messages, "C\0ignored").is_none());
    }

    #[test]
    fn message_locale_environment_precedence_and_ctype_fallback() -> std::io::Result<()> {
        const EXPECTED: &str = "OXVIM_TEST_MESSAGE_LOCALE";
        const CTYPE_FALLBACK: &str = "<ctype>";
        if let Ok(expected) = std::env::var(EXPECTED) {
            let expected = if expected == CTYPE_FALLBACK {
                super::current_locale(LocaleCategory::CType)
            } else {
                Some(expected)
            };
            assert!(expected.is_some());
            assert_eq!(super::message_locale_from_environment(), expected);
            #[cfg(windows)]
            assert_eq!(super::current_locale(LocaleCategory::Messages), expected);
            return Ok(());
        }

        // Each child owns its environment; no test mutates process-global state.
        for (values, expected) in [
            ([None, None, None], CTYPE_FALLBACK),
            ([Some(""), Some(""), Some("")], CTYPE_FALLBACK),
            ([None, None, Some("1043")], CTYPE_FALLBACK),
            ([None, None, Some("en_US")], "en_US"),
            ([Some(""), Some(""), Some("en_US")], "en_US"),
            ([None, Some("messages"), Some("en_US")], "messages"),
            ([Some("all"), Some("messages"), Some("en_US")], "all"),
            ([Some("1043"), None, None], "1043"),
            ([None, Some("1043"), None], "1043"),
        ] {
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child
                .args([
                    "--exact",
                    "locale::tests::message_locale_environment_precedence_and_ctype_fallback",
                    "--nocapture",
                ])
                .env(EXPECTED, expected);
            for (name, value) in ["LC_ALL", "LC_MESSAGES", "LANG"].into_iter().zip(values) {
                match value {
                    Some(value) => child.env(name, value),
                    None => child.env_remove(name),
                };
            }
            let output = child.output()?;
            assert!(
                output.status.success(),
                "locale environment {values:?}:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn message_catalog_selection_does_not_change_character_locale() {
        let before = super::current_locale(LocaleCategory::CType);
        assert!(before.is_some());
        assert_eq!(
            set_locale(LocaleCategory::Messages, "oxvim-message-catalog"),
            Some(String::new())
        );
        assert_eq!(super::current_locale(LocaleCategory::CType), before);
    }
}
