//! Syntax registry and `:syntax` command support.
//!
//! Upstream reference: `src/nvim/syntax.c` (`ex_syntax`, `syn_cmd_list`, etc.).

use thiserror::Error;

/// Errors arising during syntax parsing and listing.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum SyntaxError {
    /// Highlight group not found in registry.
    #[error("E28: No such highlight group name: {0}")]
    NoSuchGroup(String),
    /// Group argument was missing in command.
    #[error("E400: No such syntax group")]
    MissingGroup,
    /// Pattern argument was missing in `:syntax match`.
    #[error("E400: Missing pattern: {0}")]
    MissingPattern(String),
    /// Unrecognized `:syntax` subcommand.
    #[error("E410: Invalid :syntax subcommand: {0}")]
    InvalidSubcommand(String),
}

/// A single syntax definition item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyntaxItem {
    /// A regex pattern match.
    Match {
        /// Regular expression pattern.
        pattern: String,
    },
    /// A list of keywords.
    Keyword {
        /// Keywords matching this group.
        words: Vec<String>,
    },
    /// A region defined by start/end/skip.
    Region {
        /// Raw region definition text.
        definition: String,
    },
}

/// Per-buffer registry of defined syntax groups.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyntaxRegistry {
    /// Groups in definition order: (group_name, items).
    groups: Vec<(String, Vec<SyntaxItem>)>,
}

impl SyntaxRegistry {
    /// Creates an empty syntax registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears either a single group or all groups.
    pub fn clear(&mut self, group: Option<&str>) {
        if let Some(target) = group {
            self.groups
                .retain(|(name, _)| !name.eq_ignore_ascii_case(target));
        } else {
            self.groups.clear();
        }
    }

    /// Adds a regex pattern match to the given group.
    pub fn add_match(&mut self, group: &str, pattern: String) {
        if let Some((_, items)) = self
            .groups
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(group))
        {
            items.push(SyntaxItem::Match { pattern });
        } else {
            self.groups
                .push((group.to_string(), vec![SyntaxItem::Match { pattern }]));
        }
    }

    /// Adds keywords to the given group.
    pub fn add_keyword(&mut self, group: &str, words: Vec<String>) {
        if let Some((_, items)) = self
            .groups
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(group))
        {
            items.push(SyntaxItem::Keyword { words });
        } else {
            self.groups
                .push((group.to_string(), vec![SyntaxItem::Keyword { words }]));
        }
    }

    /// Adds a region definition to the given group.
    pub fn add_region(&mut self, group: &str, definition: String) {
        if let Some((_, items)) = self
            .groups
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(group))
        {
            items.push(SyntaxItem::Region { definition });
        } else {
            self.groups
                .push((group.to_string(), vec![SyntaxItem::Region { definition }]));
        }
    }

    /// Returns whether the group is defined in this registry.
    #[must_use]
    pub fn has_group(&self, group: &str) -> bool {
        self.groups
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(group))
    }

    /// Returns true if no groups or items are defined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() || self.groups.iter().all(|(_, items)| items.is_empty())
    }

    /// Formats the group(s) according to upstream `syntax.c` (`syn_cmd_list`).
    ///
    /// # Errors
    /// Returns [`SyntaxError::NoSuchGroup`] when a requested group is missing.
    pub fn format_list(&self, group: Option<&str>) -> Result<String, SyntaxError> {
        if self.is_empty() {
            return Ok("No Syntax items defined for this buffer\n".to_string());
        }
        if let Some(target) = group {
            if !self.has_group(target) {
                return Err(SyntaxError::NoSuchGroup(target.to_string()));
            }
        }

        let mut out = String::from("\n--- Syntax items ---\n");
        for (name, items) in &self.groups {
            if let Some(target) = group {
                if !name.eq_ignore_ascii_case(target) {
                    continue;
                }
            }
            if items.is_empty() {
                continue;
            }
            for (idx, item) in items.iter().enumerate() {
                if idx == 0 {
                    let pad = if name.len() < 15 { 15 - name.len() } else { 1 };
                    out.push_str(name);
                    for _ in 0..pad {
                        out.push(' ');
                    }
                    out.push_str("xxx ");
                } else {
                    out.push_str(&" ".repeat(19));
                }

                match item {
                    SyntaxItem::Match { pattern } => {
                        let delim = pick_delimiter(pattern);
                        out.push_str(&format!("match {delim}{pattern}{delim}  \n"));
                    }
                    SyntaxItem::Keyword { words } => {
                        out.push_str(&format!("{}\n", words.join(" ")));
                    }
                    SyntaxItem::Region { definition } => {
                        out.push_str(&format!("{definition}  \n"));
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Upstream `put_pattern` delimiter selection (`syntax.c:3521-3548`).
#[must_use]
pub fn pick_delimiter(pattern: &str) -> char {
    const SEPCHARS: &[char] = &['/', '+', '=', '-', '#', '@', '"', '|', '\'', '^', '&'];
    for &ch in SEPCHARS {
        if !pattern.contains(ch) {
            return ch;
        }
    }
    '/'
}

/// Parses arguments for `:syntax match {group-name} {pattern}`.
///
/// # Errors
/// Returns error when group or pattern is missing.
pub fn parse_match_args(args: &str) -> Result<(String, String), SyntaxError> {
    let trimmed = args.trim_start();
    let end_group = trimmed
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(trimmed.len());
    let group = &trimmed[..end_group];
    if group.is_empty() {
        return Err(SyntaxError::MissingGroup);
    }
    let rest = trimmed[end_group..].trim_start();
    let Some(delim) = rest.chars().next() else {
        return Err(SyntaxError::MissingPattern(args.to_string()));
    };
    let pat_str = &rest[delim.len_utf8()..];
    let mut end = None;
    let mut escaped = false;
    for (i, c) in pat_str.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == delim {
            end = Some(i);
            break;
        }
    }
    let pattern = match end {
        Some(i) => &pat_str[..i],
        None => pat_str,
    };
    Ok((group.to_string(), pattern.to_string()))
}

/// Parses arguments for `:syntax keyword {group-name} {keyword}..`.
///
/// # Errors
/// Returns error when group is missing.
pub fn parse_keyword_args(args: &str) -> Result<(String, Vec<String>), SyntaxError> {
    let trimmed = args.trim_start();
    let end_group = trimmed
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(trimmed.len());
    let group = &trimmed[..end_group];
    if group.is_empty() {
        return Err(SyntaxError::MissingGroup);
    }
    let rest = trimmed[end_group..].trim_start();
    let words = rest.split_ascii_whitespace().map(String::from).collect();
    Ok((group.to_string(), words))
}

/// Parses arguments for `:syntax region {group-name} {options}..`.
///
/// # Errors
/// Returns error when group is missing.
pub fn parse_region_args(args: &str) -> Result<(String, String), SyntaxError> {
    let trimmed = args.trim_start();
    let end_group = trimmed
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(trimmed.len());
    let group = &trimmed[..end_group];
    if group.is_empty() {
        return Err(SyntaxError::MissingGroup);
    }
    let rest = trimmed[end_group..].trim_start();
    Ok((group.to_string(), rest.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_empty_list() {
        let registry = SyntaxRegistry::new();
        assert_eq!(
            registry.format_list(None).unwrap(),
            "No Syntax items defined for this buffer\n"
        );
    }

    #[test]
    fn registry_missing_group_errors() {
        let mut registry = SyntaxRegistry::new();
        registry.add_match("foo", "bar".to_string());
        assert_eq!(
            registry.format_list(Some("nonexistent")).unwrap_err(),
            SyntaxError::NoSuchGroup("nonexistent".to_string())
        );
    }

    #[test]
    fn registry_match_formatting() {
        let mut registry = SyntaxRegistry::new();
        registry.add_match("directoryDirectory", ".*/$".to_string());
        let output = registry.format_list(Some("directoryDirectory")).unwrap();
        assert_eq!(
            output,
            "\n--- Syntax items ---\ndirectoryDirectory xxx match +.*/$+  \n"
        );
    }

    #[test]
    fn registry_short_name_padding() {
        let mut registry = SyntaxRegistry::new();
        registry.add_match("foo", "bar".to_string());
        let output = registry.format_list(Some("foo")).unwrap();
        assert_eq!(
            output,
            "\n--- Syntax items ---\nfoo            xxx match /bar/  \n"
        );
    }

    #[test]
    fn registry_multiple_items_formatting() {
        let mut registry = SyntaxRegistry::new();
        registry.add_match("foo", "bar".to_string());
        registry.add_match("foo", "baz".to_string());
        let output = registry.format_list(Some("foo")).unwrap();
        assert_eq!(
            output,
            "\n--- Syntax items ---\nfoo            xxx match /bar/  \n                   match /baz/  \n"
        );
    }

    #[test]
    fn registry_clear() {
        let mut registry = SyntaxRegistry::new();
        registry.add_match("foo", "bar".to_string());
        registry.add_match("baz", "qux".to_string());
        registry.clear(Some("foo"));
        assert!(!registry.has_group("foo"));
        assert!(registry.has_group("baz"));
        registry.clear(None);
        assert!(registry.is_empty());
    }

    #[test]
    fn parse_match_with_quotes() {
        let (group, pattern) = parse_match_args(r#"directoryDirectory ".*/$""#).unwrap();
        assert_eq!(group, "directoryDirectory");
        assert_eq!(pattern, ".*/$");
    }

    #[test]
    fn parse_match_with_slashes() {
        let (group, pattern) = parse_match_args("myGroup /abc/").unwrap();
        assert_eq!(group, "myGroup");
        assert_eq!(pattern, "abc");
    }
}
