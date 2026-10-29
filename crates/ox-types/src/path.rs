//! Path predicates that more than one crate must answer the same way.

/// `path_has_drive_letter` (`path.c:1684`): a Windows drive letter, which
/// upstream rejects before the scheme rules run. The check is not Windows-only
/// in the reference implementation, so `C:/x` is not a URL on any platform.
#[must_use]
pub fn has_drive_letter(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && matches!(bytes[1], b':' | b'|')
        && (bytes.len() == 2 || matches!(bytes[2], b'/' | b'\\' | b'?' | b'#'))
}

/// `path_with_url` (`path.c:1713`): a scheme followed by `:/` or `:\`.
///
/// A name that looks like a URL is an absolute name, not a relative one
/// (`vim_isAbsName`, `path.c:1750`), so every caller that expands a relative
/// name against the working directory asks this first.
#[must_use]
pub fn path_with_url(name: &str) -> bool {
    let bytes = name.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_alphabetic) || has_drive_letter(name) {
        return false;
    }
    let mut index = 1;
    while index < bytes.len()
        && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'+' | b'-' | b'.'))
    {
        index = index.saturating_add(1);
    }
    // A scheme may not end with '+', '-', or '.'.
    if matches!(bytes[index.saturating_sub(1)], b'+' | b'-' | b'.') {
        return false;
    }
    let rest = &name[index.min(name.len())..];
    rest.starts_with(":/") || rest.starts_with(":\\")
}

#[cfg(test)]
mod tests {
    use super::{has_drive_letter, path_with_url};

    #[test]
    fn urls_are_scheme_prefixed_names() {
        assert!(path_with_url("example:///tmp/x"));
        assert!(path_with_url("term://./1:bash"));
        assert!(path_with_url("health:/"));
        // A scheme may not end with '+', '-', or '.', and must start with a
        // letter.
        assert!(!path_with_url("a+:/x"));
        assert!(!path_with_url("1abc:///x"));
        // Relative and absolute plain paths are not URLs.
        assert!(!path_with_url("src/lib.rs"));
        assert!(!path_with_url("/tmp/x"));
        assert!(!path_with_url(""));
    }

    #[test]
    fn a_windows_drive_letter_is_not_a_url() {
        assert!(has_drive_letter("C:/x"));
        assert!(has_drive_letter("C|"));
        assert!(has_drive_letter("c:?q"));
        // A bare `C:` is a drive-relative path (`path_len == 2`).
        assert!(has_drive_letter("C:"));
        assert!(!has_drive_letter("CC"));
        assert!(!has_drive_letter("CC:/x"));
        // Upstream rejects the drive letter before the scheme rules, so this
        // is a drive path on every platform, not a one-letter URL.
        assert!(!path_with_url("C:/x"));
        assert!(!path_with_url("C|:/x"));
    }
}
