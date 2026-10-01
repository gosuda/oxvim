//! Byte-oriented parsing of Ex command lines.

use crate::command::{
    AddrType, CommandFlags, NoUserCommands, ResolveError, ResolvedCommand, UserCommandProvider,
    resolve_command,
};
use thiserror::Error;

const MAX_COMMANDS: usize = 1_024;
const MAX_MODIFIERS: usize = 64;
const MAX_OFFSETS: usize = 64;

/// An upstream-compatible error identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// E464: Ambiguous use of user-defined command.
    E464,
    /// E492: Not an editor command.
    E492,
    /// E481: No range allowed.
    E481,
    /// E488: Trailing characters.
    E488,
    /// E471: Argument required.
    E471,
    /// E477: No ! allowed.
    E477,
    /// E939: Positive count required.
    E939,
}

impl ErrorCode {
    /// Returns the traditional Vim error code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::E464 => "E464",
            Self::E492 => "E492",
            Self::E481 => "E481",
            Self::E488 => "E488",
            Self::E471 => "E471",
            Self::E477 => "E477",
            Self::E939 => "E939",
        }
    }
}

/// A command-line parse error with an input byte offset.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{code}: {message} at byte {offset}", code = .code.as_str())]
pub struct ParseError {
    /// Upstream error identifier.
    pub code: ErrorCode,
    /// Zero-based byte offset in the original command line.
    pub offset: usize,
    /// Human-readable detail, which may name the offending text the way
    /// upstream's `%s` messages do.
    pub message: String,
    /// Canonical built-in name when the failure happened after resolution.
    pub command: Option<&'static str>,
}

/// Base of one Ex address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AddressBase {
    /// Current line (`.`), including the implicit address in `,addr`.
    Current,
    /// Last line (`$`).
    Last,
    /// Absolute line number.
    Line(u64),
    /// Mark address (`'x`).
    Mark(char),
    /// Forward search (`/pattern/`).
    ForwardSearch(String),
    /// Backward search (`?pattern?`).
    BackwardSearch(String),
}

/// One address and its ordered signed offsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Address {
    /// Address base.
    pub base: AddressBase,
    /// Signed line offsets; an omitted magnitude is one.
    pub offsets: Vec<i64>,
}

/// Separator between two range addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeSeparator {
    /// Comma: evaluate both addresses from the original current line.
    Comma,
    /// Semicolon: make the first address current before evaluating the second.
    Semicolon,
}

/// Shape and evaluation semantics of a parsed range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeKind {
    /// One explicit address.
    Single,
    /// Whole-buffer shorthand (`%`).
    WholeBuffer,
    /// Two addresses and their separator behavior.
    Pair {
        /// Source separator.
        separator: RangeSeparator,
        /// True only for `;`, which advances current before the second address.
        cursor_advance: bool,
    },
}

/// Parsed Ex range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Range {
    /// First address, absent only for no range.
    pub start: Option<Address>,
    /// Second address for a pair or whole-buffer range.
    pub end: Option<Address>,
    /// Range shape.
    pub kind: RangeKind,
}

/// Forced `'magic'` override recognized while extracting an Ex command's
/// `'incsearch'` preview pattern (`parse_pattern_and_range`,
/// `ex_getln.c:319-323`): only `smagic`/`snomagic` set one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewMagic {
    /// No override: the command's own default magic setting applies.
    Default,
    /// `smagic`: force `'magic'` on for this command's pattern.
    ForceMagic,
    /// `snomagic`: force `'magic'` off for this command's pattern.
    ForceNomagic,
}

/// Syntax-only extraction of the search pattern and address range a
/// preview-eligible Ex command line would use for `'incsearch'`, produced by
/// [`parse_preview_pattern`]. Mirrors `parse_pattern_and_range`
/// (`ex_getln.c:276-398`) without resolving addresses to line numbers or
/// executing anything — evaluation and search stay with the host, which
/// alone has the buffer and cursor this needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewPattern {
    /// Forced magic override; [`PreviewMagic::Default`] otherwise.
    pub magic: PreviewMagic,
    /// Parsed address range, unresolved to line numbers.
    pub range: Option<Range>,
    /// Whether an absent range defaults to the current line (`:s`-family,
    /// `ex_getln.c:391-394`) rather than leaving the whole buffer
    /// unrestricted.
    pub default_current_line: bool,
    /// Search delimiter character that closed (or would close) the pattern.
    pub delimiter: char,
    /// Pattern text between the delimiters, raw and un-escaped.
    pub pattern: String,
    /// Whether `pattern` is empty specifically because of a closed,
    /// back-to-back delimiter pair (`//`), which reuses the last search
    /// pattern (`ex_getln.c:360` `use_last_pat`) rather than meaning no
    /// pattern has been typed yet.
    pub use_last_pattern: bool,
}

/// A recognized command modifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModifierKind {
    /// `aboveleft`.
    AboveLeft,
    /// `belowright`.
    BelowRight,
    /// `botright`.
    BotRight,
    /// `browse`.
    Browse,
    /// `confirm`.
    Confirm,
    /// `filter`.
    Filter,
    /// `hide`.
    Hide,
    /// `horizontal`.
    Horizontal,
    /// `keepalt`.
    KeepAlt,
    /// `keepjumps`.
    KeepJumps,
    /// `keepmarks`.
    KeepMarks,
    /// `keeppatterns`.
    KeepPatterns,
    /// `leftabove`.
    LeftAbove,
    /// `lockmarks`.
    LockMarks,
    /// `noautocmd`.
    NoAutocmd,
    /// `noswapfile`.
    NoSwapfile,
    /// `rightbelow`.
    RightBelow,
    /// `sandbox`.
    Sandbox,
    /// `silent`.
    Silent,
    /// `tab`.
    Tab,
    /// `topleft`.
    TopLeft,
    /// `unsilent`.
    Unsilent,
    /// `verbose`.
    Verbose,
    /// `vertical`.
    Vertical,
}

/// One modifier in source order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandModifier {
    /// Modifier kind.
    pub kind: ModifierKind,
    /// Optional prefix count (`3verbose`, `2tab`).
    pub count: Option<u64>,
    /// Whether the modifier carried `!` (`silent!`, `filter!`).
    pub bang: bool,
    /// Delimited pattern for the `filter` modifier; `None` otherwise.
    pub pattern: Option<String>,
}

/// One parsed command. Execution is intentionally out of scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExCommand {
    /// Resolved built-in or user command.
    pub command: ResolvedCommand,
    /// Ordered command modifiers.
    pub modifiers: Vec<CommandModifier>,
    /// Optional address range.
    pub range: Option<Range>,
    /// Command bang.
    pub bang: bool,
    /// `eap->usefilter` (`ex_docmd.c:2256-2275`): `:read !cmd`, `:read!cmd`,
    /// and `:write !cmd` hand their whole tail to the shell. The `!` that
    /// selected the filter is consumed, so `args` is the shell command and
    /// is never split at `|`.
    pub usefilter: bool,
    /// Post-command count.
    pub count: Option<u64>,
    /// Post-command register.
    pub register: Option<char>,
    /// Uninterpreted argument tail after count/register extraction.
    pub args: String,
    /// Leading whitespace of the command-line chunk this command was read
    /// from — `*eap->cmdlinep`'s own whitespace prefix, the text just past
    /// the previous `|`/`\n` separator including whitespace before `:`s and
    /// modifiers (eval/vars.c:772-776). `=<< trim` requires the terminator
    /// line to carry exactly this indent.
    pub cmdline_ws: String,
    /// Byte range occupied by this command in the original input.
    pub span: std::ops::Range<usize>,
}

struct ParsedCommandTail {
    end: usize,
    bang: bool,
    usefilter: bool,
    count: Option<u64>,
    register: Option<char>,
    args: String,
}

/// Stateless Ex command-line parser.
pub struct Parser<'a, P: UserCommandProvider + ?Sized = NoUserCommands> {
    users: &'a P,
}

impl Default for Parser<'static, NoUserCommands> {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser<'static, NoUserCommands> {
    /// Creates a parser without user-defined commands.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            users: &NoUserCommands,
        }
    }
}

impl<'a, P: UserCommandProvider + ?Sized> Parser<'a, P> {
    /// Creates a parser using the host's user-command registry.
    #[must_use]
    pub const fn with_user_commands(users: &'a P) -> Self {
        Self { users }
    }

    /// Parses all bar-separated commands from one command line.
    ///
    /// # Errors
    ///
    /// Returns an error when any command cannot be parsed, violates the
    /// resolved command's accepted syntax, or exceeds the per-line command
    /// limit.
    pub fn parse(&self, input: &str) -> Result<Vec<ExCommand>, ParseError> {
        let mut commands = Vec::new();
        let mut cursor = 0;
        while cursor < input.len() {
            cursor = skip_space_and_colons(input, cursor);
            if cursor >= input.len() {
                break;
            }
            // `"` at command position is a comment that ends at the newline
            // (ex_docmd.c:2505-2511); a bare `\n` is an empty command that
            // `do_one_cmd` steps over (ex_docmd.c:2513-2516).
            match input.as_bytes()[cursor] {
                b'"' => match input[cursor..].find('\n') {
                    Some(relative) => {
                        cursor += relative + 1;
                        continue;
                    }
                    None => break,
                },
                b'\n' => {
                    cursor += 1;
                    continue;
                }
                _ => {}
            }
            if commands.len() == MAX_COMMANDS {
                return Err(error(ErrorCode::E488, cursor, "too many commands"));
            }
            let (command, next) = self.parse_one(input, cursor)?;
            commands.push(command);
            cursor = next;
            match input.as_bytes().get(cursor) {
                // A comment quote `separate_nextcmd` found in an argument is
                // written over with NUL, so nothing after it is reachable
                // (ex_docmd.c:4170-4178).
                Some(b'"') => break,
                Some(b'|' | b'\n') => cursor += 1,
                _ => {}
            }
        }
        Ok(commands)
    }
    /// Parses only the first bar-separated command from one line, returning
    /// it and the cursor just past it.
    ///
    /// This is `do_one_cmd`'s single-command step: a host that executes a
    /// line one command at a time (`do_cmdline`'s loop) can re-resolve every
    /// later command against whatever the earlier ones changed.
    ///
    /// # Errors
    ///
    /// Returns an error when the first command cannot be parsed or violates
    /// the resolved command's accepted syntax.
    pub fn parse_first(&self, input: &str) -> Result<Option<(ExCommand, usize)>, ParseError> {
        let cursor = skip_space_and_colons(input, 0);
        if cursor >= input.len() || input.as_bytes()[cursor] == b'"' {
            return Ok(None);
        }
        let (command, next) = self.parse_one(input, cursor)?;
        Ok(Some((command, next)))
    }

    fn parse_one(&self, input: &str, start: usize) -> Result<(ExCommand, usize), ParseError> {
        let mut cursor = start;
        let modifiers = parse_modifiers(input, &mut cursor)?;
        cursor = skip_ascii_space(input, cursor);
        let range_offset = cursor;
        let range = parse_range(input, &mut cursor)?;
        cursor = skip_ascii_space(input, cursor);

        let command_offset = cursor;
        let unknown_command = || {
            error(
                ErrorCode::E492,
                command_offset,
                format!("Not an editor command: {}", &input[range_offset..]),
            )
        };
        let (typed, after_name) = parse_command_name(input, cursor);
        if typed.is_empty() {
            // ex_docmd.c:2074-2085: "If we got a line, but no command,
            // then go to the line"; a following `|` prints the range
            // instead (ex_range_without_command, ex_docmd.c:2422-2432).
            let command = match (range.is_some(), input.as_bytes().get(cursor)) {
                (_, Some(b'|')) => {
                    resolve_command("print", self.users).map_err(|_| unknown_command())?
                }
                (true, _) => ResolvedCommand::RangeOnly,
                (false, _) => return Err(unknown_command()),
            };
            return Ok((
                ExCommand {
                    command,
                    modifiers,
                    range,
                    bang: false,
                    usefilter: false,
                    count: None,
                    register: None,
                    args: String::new(),
                    cmdline_ws: chunk_ws(input, start).to_owned(),
                    span: start..cursor,
                },
                cursor,
            ));
        }
        let command =
            resolve_command(typed, self.users).map_err(|resolve_error| match resolve_error {
                ResolveError::NotFound => unknown_command(),
                ResolveError::AmbiguousUserCommand => error(
                    ErrorCode::E464,
                    command_offset,
                    "Ambiguous use of user-defined command",
                ),
            })?;
        let flags = effective_flags(&command);
        if range.is_some() && !flags.contains(CommandFlags::RANGE) {
            return Err(command_error(
                &command,
                ErrorCode::E481,
                range_offset,
                "No range allowed",
            ));
        }

        let ParsedCommandTail {
            end,
            bang,
            usefilter,
            count,
            register,
            args,
        } = Self::parse_command_tail(input, after_name, &command, flags)?;

        Ok((
            ExCommand {
                command,
                modifiers,
                range,
                bang,
                usefilter,
                cmdline_ws: chunk_ws(input, start).to_owned(),
                count,
                register,
                args,
                span: start..end,
            },
            end,
        ))
    }
    /// Parses one resolved command's bang, filter selection, and argument
    /// tail: everything after the command name.
    fn parse_command_tail(
        input: &str,
        mut cursor: usize,
        command: &ResolvedCommand,
        flags: CommandFlags,
    ) -> Result<ParsedCommandTail, ParseError> {
        let mut bang = input.as_bytes().get(cursor) == Some(&b'!');
        if bang && !flags.contains(CommandFlags::BANG) {
            return Err(command_error(
                command,
                ErrorCode::E477,
                cursor,
                "No ! allowed",
            ));
        }
        if bang {
            cursor += 1;
        }
        cursor = skip_ascii_space(input, cursor);
        // ":r!cmd" spends its bang on the filter, and a "!" standing where
        // ":read"/":write" expect a file name selects the filter too
        // (ex_docmd.c:2256-2275). Either way the "!" is consumed here so the
        // remaining line is one shell command.
        let mut usefilter = false;
        if command.name() == "read" && bang {
            usefilter = true;
            bang = false;
        } else if matches!(command.name(), "read" | "write")
            && input.as_bytes().get(cursor) == Some(&b'!')
        {
            usefilter = true;
            cursor += 1;
        }
        let end = command_end(input, cursor, flags, usefilter, &command);
        // `ea.arg = skipwhite(p)` (`ex_docmd.c`): space and tab only, so a CR
        // or a newline that ends the argument stays in it.
        let args_start = skip_ascii_space(input, cursor).min(end);
        // Trailing whitespace is removed by `separate_nextcmd`, which runs
        // only for an `EX_TRLBAR` command that is not a filter, and only calls
        // `del_trailing_spaces` when `EX_NOTRLCOM` is absent
        // (`ex_docmd.c:4162-4164`). So `:normal`, `:let`, `:execute` and
        // `:map` keep every trailing byte, and `:edit` loses only unescaped
        // spaces and tabs.
        let args_end = if flags.contains(CommandFlags::TRLBAR)
            && !flags.contains(CommandFlags::NOTRLCOM)
            && !usefilter
        {
            del_trailing_spaces(input, args_start, end)
        } else {
            end
        };

        let mut args = input[args_start..args_end].to_owned();
        // The shell-family scan removes each `\` that continues the line
        // (`STRMOVE`, ex_docmd.c:2312-2318), leaving a real newline inside
        // the argument that the handler's nested cmdline splits on.
        if shell_arg_family(command.name(), flags, usefilter) {
            args = args.replace("\\\n", "\n");
        }
        let register = if flags.contains(CommandFlags::REGSTR) {
            take_register(&mut args)
        } else {
            None
        };
        let count = if flags.contains(CommandFlags::COUNT) {
            let count = take_count(&mut args, flags.contains(CommandFlags::BUFNAME));
            // "n <= 0" is rejected unless the command accepts zero
            // (ex_docmd.c:1420-1425), so `:sleep 0m` is E939 while `:0read`
            // is fine.
            if count == Some(0) && !flags.contains(CommandFlags::ZEROR) {
                return Err(command_error(
                    command,
                    ErrorCode::E939,
                    args_start,
                    "Positive count required",
                ));
            }
            count
        } else {
            None
        };

        if flags.contains(CommandFlags::NEEDARG) && args.trim().is_empty() {
            return Err(command_error(
                command,
                ErrorCode::E471,
                args_start,
                "Argument required",
            ));
        }
        if !flags.contains(CommandFlags::EXTRA)
            && !matches!(command.name(), "append" | "change" | "insert")
            && !args.trim().is_empty()
        {
            // e_trailing_arg (errors.h:123) names the offending text.
            return Err(command_error(
                command,
                ErrorCode::E488,
                args_start,
                format!("Trailing characters: {}", args.trim()),
            ));
        }

        Ok(ParsedCommandTail {
            end,
            bang,
            usefilter,
            count,
            register,
            args,
        })
    }
}

/// The argument flags that govern one resolved command: a built-in's table
/// entry, or the flags the host recorded for its user command.
#[must_use]
pub const fn effective_flags(command: &ResolvedCommand) -> CommandFlags {
    command.flags()
}

/// The address domain that governs one resolved command.
#[must_use]
pub const fn effective_addr_type(command: &ResolvedCommand) -> AddrType {
    command.addr_type()
}

fn parse_command_name(input: &str, start: usize) -> (&str, usize) {
    let bytes = input.as_bytes();
    let Some(&first) = bytes.get(start) else {
        return ("", start);
    };
    if is_one_letter_command(bytes, start) {
        return (&input[start..=start], start + 1);
    }
    if first.is_ascii_alphabetic() {
        let mut end = start + 1;
        while bytes.get(end).is_some_and(u8::is_ascii_alphabetic) {
            end += 1;
        }
        if first.is_ascii_uppercase() || input[start..end].starts_with("py") {
            while bytes.get(end).is_some_and(u8::is_ascii_alphanumeric) {
                end += 1;
            }
        }
        return (&input[start..end], end);
    }
    if b"@!=><&~#*".contains(&first) {
        return (&input[start..=start], start + 1);
    }
    ("", start)
}

fn is_one_letter_command(bytes: &[u8], start: usize) -> bool {
    let at = |offset: usize| bytes.get(start + offset).copied().unwrap_or_default();
    if at(0) == b'k' && (at(1) != b'e' || at(2) != b'e') {
        return true;
    }
    if at(0) != b's' {
        return false;
    }
    let second = at(1);
    (second == b'c'
        && (at(2) == 0
            || (at(2) != b's'
                && at(2) != b'r'
                && (at(3) == 0 || (at(3) != b'i' && at(4) != b'p')))))
        || second == b'g'
        || (second == b'i' && at(2) != b'm' && at(2) != b'l' && at(2) != b'g')
        || second == b'I'
        || (second == b'r' && at(2) != b'e')
}

/// Parses the modifier stack (`:silent!`, `:verbose`, counts, …) at
/// `*cursor`, advancing it to the first byte after the last modifier.
/// Exposed so a host can inspect modifiers on a line whose command itself
/// fails to resolve (e.g. `silent!` still suppresses that resolution error).
///
/// # Errors
///
/// Returns an error when a modifier is malformed.
pub fn parse_modifiers(input: &str, cursor: &mut usize) -> Result<Vec<CommandModifier>, ParseError> {
    let mut modifiers = Vec::new();
    loop {
        let saved = *cursor;
        let mut probe = skip_ascii_space(input, saved);
        let count_start = probe;
        while input.as_bytes().get(probe).is_some_and(u8::is_ascii_digit) {
            probe += 1;
        }
        let count = if probe > count_start {
            let after_digits = skip_ascii_space(input, probe);
            let parsed = input[count_start..probe].parse::<u64>().ok();
            probe = after_digits;
            parsed
        } else {
            None
        };

        let name_start = probe;
        while input
            .as_bytes()
            .get(probe)
            .is_some_and(u8::is_ascii_alphabetic)
        {
            probe += 1;
        }
        let typed = &input[name_start..probe];
        let Some((kind, allows_count)) = modifier(typed) else {
            *cursor = saved;
            break;
        };
        if count.is_some() && !allows_count {
            *cursor = saved;
            break;
        }
        let mut bang = false;
        if input.as_bytes().get(probe) == Some(&b'!')
            && matches!(kind, ModifierKind::Silent | ModifierKind::Filter)
        {
            bang = true;
            probe += 1;
        }
        let mut pattern = None;
        let is_filter = kind == ModifierKind::Filter;
        if is_filter {
            // ":filter {pat} cmd": the pattern is mandatory and belongs to
            // the modifier, so it is consumed and retained here before the
            // nested command is routed (the 'f' case in parse_command_
            // modifiers: ex_docmd.c:2561-2591). Without a pattern, or when
            // no command follows, "filter" is not a modifier at all.
            let pattern_start = skip_ascii_space(input, probe);
            let at_command_end = matches!(
                input.as_bytes().get(pattern_start).copied(),
                None | Some(b'|' | b'"')
            );
            if at_command_end {
                *cursor = saved;
                break;
            }
            let Ok((parsed_pattern, after_pattern)) = parse_vimgrep_pattern(input, pattern_start)
            else {
                *cursor = saved;
                break;
            };
            pattern = Some(parsed_pattern);
            probe = after_pattern;
            // Without a following nested command, "filter" is not a modifier.
            let after_pattern_space = skip_ascii_space(input, probe);
            if matches!(
                input.as_bytes().get(after_pattern_space).copied(),
                None | Some(b'|' | b'"')
            ) {
                *cursor = saved;
                break;
            }
        } else if kind == ModifierKind::Hide {
            // ":hide" and ":hide | cmd" stay the builtin command; "hide" is
            // a modifier only when another command follows (the 'h' case in
            // parse_command_modifiers: ex_docmd.c:2594-2603).
            let after_word = skip_ascii_space(input, probe);
            if matches!(
                input.as_bytes().get(after_word).copied(),
                None | Some(b'|' | b'"')
            ) {
                *cursor = saved;
                break;
            }
        }
        // A modifier must not be a prefix of a longer identifier. "filter"
        // is exempt because probe has advanced past its pattern, where a
        // following identifier is the nested command, not a word extension
        // (":filter /pat/delete" has no separating space).
        if !is_filter
            && input
                .as_bytes()
                .get(probe)
                .is_some_and(u8::is_ascii_alphabetic)
        {
            *cursor = saved;
            break;
        }
        modifiers.push(CommandModifier {
            kind,
            count,
            bang,
            pattern,
        });
        if modifiers.len() == MAX_MODIFIERS {
            return Err(error(ErrorCode::E488, probe, "too many modifiers"));
        }
        *cursor = skip_ascii_space(input, probe);
    }
    Ok(modifiers)
}

/// Parses one vimgrep-style pattern: a bare identifier word ("pattern fname")
/// or a delimited pattern with optional `g`/`j`/`f` flags ("/pattern/ fname"),
/// returning the pattern text and the cursor just past the pattern.
/// Mirrors `skip_vimgrep_pat`: `ex_cmds.c:4972-5010`.
fn parse_vimgrep_pattern(input: &str, start: usize) -> Result<(String, usize), ParseError> {
    let bytes = input.as_bytes();
    let Some(&first) = bytes.get(start) else {
        return Err(error(ErrorCode::E488, start, "search pattern required"));
    };
    if !first.is_ascii() || first.is_ascii_alphanumeric() || first == b'_' {
        // ":filter foo cmd" / ":vimgrep foo fname": bare pattern up to space.
        let pattern_start = start;
        let mut cursor = start;
        while bytes
            .get(cursor)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            cursor += 1;
        }
        return Ok((input[pattern_start..cursor].to_owned(), cursor));
    }
    // Delimited pattern ":filter /foo/ cmd", optionally followed by flags.
    let (pattern, after) = parse_pattern(input, start, first, true)?;
    let mut cursor = after;
    while matches!(bytes.get(cursor), Some(b'g' | b'j' | b'f')) {
        cursor += 1;
    }
    Ok((pattern, cursor))
}

/// Whether `command_end` must skip a leading grep pattern before scanning
/// for `|` separators and `"` comments.
fn is_grep_command(name: &str) -> bool {
    matches!(name, "vimgrep" | "lvimgrep" | "vimgrepadd" | "lvimgrepadd")
}

/// Returns the cursor just past a vimgrep-family leading pattern, or
/// `args_start` when no pattern can be skipped (`skip_grep_pat`: `ex_docmd.c`
/// 3840-3854; used by `separate_nextcmd` at `ex_docmd.c:4114`).
fn skip_grep_pattern(input: &str, args_start: usize) -> usize {
    match parse_vimgrep_pattern(input, args_start) {
        Ok((_, after)) => after,
        Err(_) => args_start,
    }
}

fn modifier(typed: &str) -> Option<(ModifierKind, bool)> {
    const MODIFIERS: &[(&str, usize, ModifierKind, bool)] = &[
        ("aboveleft", 3, ModifierKind::AboveLeft, false),
        ("belowright", 3, ModifierKind::BelowRight, false),
        ("botright", 2, ModifierKind::BotRight, false),
        ("browse", 3, ModifierKind::Browse, false),
        ("confirm", 4, ModifierKind::Confirm, false),
        ("filter", 4, ModifierKind::Filter, false),
        ("hide", 3, ModifierKind::Hide, false),
        ("horizontal", 3, ModifierKind::Horizontal, false),
        ("keepalt", 5, ModifierKind::KeepAlt, false),
        ("keepjumps", 5, ModifierKind::KeepJumps, false),
        ("keepmarks", 3, ModifierKind::KeepMarks, false),
        ("keeppatterns", 5, ModifierKind::KeepPatterns, false),
        ("leftabove", 5, ModifierKind::LeftAbove, false),
        ("lockmarks", 3, ModifierKind::LockMarks, false),
        ("noautocmd", 3, ModifierKind::NoAutocmd, false),
        ("noswapfile", 3, ModifierKind::NoSwapfile, false),
        ("rightbelow", 6, ModifierKind::RightBelow, false),
        ("sandbox", 3, ModifierKind::Sandbox, false),
        ("silent", 3, ModifierKind::Silent, true),
        ("tab", 3, ModifierKind::Tab, true),
        ("topleft", 2, ModifierKind::TopLeft, false),
        ("unsilent", 3, ModifierKind::Unsilent, false),
        ("verbose", 4, ModifierKind::Verbose, true),
        ("vertical", 4, ModifierKind::Vertical, false),
    ];
    MODIFIERS.iter().find_map(|(name, min_len, kind, count)| {
        (typed.len() >= *min_len && name.starts_with(typed)).then_some((*kind, *count))
    })
}

/// Whether `typed` is a valid abbreviation of `full`, replicating
/// `strncmp(cmd, full, MAX(p - cmd, min_len)) == 0` (`ex_getln.c:315-350`):
/// `typed` must be at least `min_len` bytes and a genuine prefix of `full`.
fn is_abbrev(typed: &str, full: &str, min_len: usize) -> bool {
    typed.len() >= min_len && full.as_bytes().get(..typed.len()) == Some(typed.as_bytes())
}

/// Scans a possibly unterminated delimited pattern for `'incsearch'`
/// preview purposes, mirroring `skip_regexp_ex` as `parse_pattern_and_range`
/// uses it (`ex_getln.c:359-364`): an unescaped closing delimiter ends the
/// pattern; running off the end of `input` takes the rest of the line
/// instead of erroring — this only recognizes a command line, it never
/// requires one to be complete. Returns the pattern text and whether it is
/// empty specifically because of a closed, back-to-back delimiter pair
/// (`//`), which reuses the last search pattern.
fn scan_preview_pattern(input: &str, delim_pos: usize, delimiter: char) -> (String, bool) {
    let mut cursor = delim_pos + delimiter.len_utf8();
    let pattern_start = cursor;
    let mut escaped = false;
    while let Some(ch) = input[cursor..].chars().next() {
        if !escaped && ch == delimiter {
            let pattern = input[pattern_start..cursor].to_owned();
            let use_last_pattern = pattern.is_empty();
            return (pattern, use_last_pattern);
        }
        escaped = !escaped && ch == '\\';
        cursor += ch.len_utf8();
    }
    (input[pattern_start..cursor].to_owned(), false)
}

/// Syntax-only extraction of the search pattern and address range a
/// preview-eligible Ex command line would use, mirroring
/// `parse_pattern_and_range` (`ex_getln.c:276-398`). This NEVER resolves or
/// executes the command — only recognizes whether `'incsearch'` may preview
/// it, and if so, what to search for. Returns `None` for every other
/// command, and for a previewable command with no pattern typed yet
/// (`ex_getln.c:311-313,333-335,344-346,362-364`).
///
/// The previewable command families and their minimum typed abbreviation
/// are exactly upstream's (`ex_getln.c:315-350`): `substitute`/`smagic`/
/// `vglobal` (any prefix), `snomagic` (3), `sort`/`uniq` (3), `vimgrep` (3),
/// `vimgrepadd` (8), `lvimgrep` (2), `lvimgrepadd` (9), `global` (any
/// prefix). A destructive command among these is only ever recognized here,
/// never dispatched: the caller runs a read-only search with the returned
/// pattern and stops.
#[must_use]
pub fn parse_preview_pattern(input: &str) -> Option<PreviewPattern> {
    let mut cursor = 0usize;
    // Skip command modifiers silently (`parse_command_modifiers`,
    // `ex_getln.c:301`).
    parse_modifiers(input, &mut cursor).ok()?;
    cursor = skip_ascii_space(input, cursor);
    // Skip over the range to find the command (`skip_range`,
    // `ex_docmd.c:3313-3361`); `parse_range`/`parse_address` already
    // tolerate an unterminated `/pattern` address the same permissive way.
    let range = parse_range(input, &mut cursor).ok()?;
    cursor = skip_ascii_space(input, cursor);
    let bytes = input.as_bytes();
    let cmd_start = cursor;
    if !matches!(
        bytes.get(cmd_start).copied(),
        Some(b's' | b'g' | b'v' | b'l' | b'u')
    ) {
        return None;
    }
    let mut name_end = cmd_start;
    while bytes.get(name_end).is_some_and(u8::is_ascii_alphabetic) {
        name_end += 1;
    }
    let name = &input[cmd_start..name_end];
    // `if (*skipwhite(p) == NUL) return false;` (`ex_getln.c:311-313`): the
    // command name alone, with nothing after it yet, previews nothing.
    if skip_ascii_space(input, name_end) >= input.len() {
        return None;
    }
    let first = bytes[cmd_start];
    let (magic, default_current_line, delim_optional, mut p) = if is_abbrev(name, "substitute", 1)
        || is_abbrev(name, "smagic", 1)
        || is_abbrev(name, "snomagic", 3)
        || is_abbrev(name, "vglobal", 1)
    {
        let magic = if name.starts_with("sm") {
            PreviewMagic::ForceMagic
        } else if name.starts_with("sn") {
            PreviewMagic::ForceNomagic
        } else {
            PreviewMagic::Default
        };
        // `:s` defaults its range to the current line; `cmd[1] != 'o'`
        // (`ex_getln.c:391`) excludes `:sort`, which shares the `s` prefix.
        let default_current_line = first == b's' && name.as_bytes().get(1) != Some(&b'o');
        (magic, default_current_line, false, name_end)
    } else if is_abbrev(name, "sort", 3) || is_abbrev(name, "uniq", 3) {
        // Skip over `!` and whitespace-separated alpha flags
        // (`ex_getln.c:326-335`).
        let mut p = name_end;
        if bytes.get(p) == Some(&b'!') {
            p = skip_ascii_space(input, p + 1);
        }
        loop {
            p = skip_ascii_space(input, p);
            if bytes.get(p).is_some_and(u8::is_ascii_alphabetic) {
                p += 1;
            } else {
                break;
            }
        }
        if p >= input.len() {
            return None;
        }
        (PreviewMagic::Default, false, false, p)
    } else if is_abbrev(name, "vimgrep", 3)
        || is_abbrev(name, "vimgrepadd", 8)
        || is_abbrev(name, "lvimgrep", 2)
        || is_abbrev(name, "lvimgrepadd", 9)
        || is_abbrev(name, "global", 1)
    {
        let mut p = name_end;
        if bytes.get(p) == Some(&b'!') {
            p += 1;
            if skip_ascii_space(input, p) >= input.len() {
                return None;
            }
        }
        // Only the `g`/`v` global commands require a punctuation delimiter;
        // the `vimgrep` family also accepts a bare space-delimited word
        // (`ex_getln.c:340-350` `delim_optional`).
        (PreviewMagic::Default, false, first != b'g', p)
    } else {
        return None;
    };
    p = skip_ascii_space(input, p);
    let delimiter = input[p..].chars().next()?;
    let (pattern, use_last_pattern) =
        if delim_optional && (delimiter.is_alphanumeric() || delimiter == '_') {
            let end = input[p..]
                .find(char::is_whitespace)
                .map_or(input.len(), |offset| p + offset);
            (input[p..end].to_owned(), false)
        } else {
            scan_preview_pattern(input, p, delimiter)
        };
    if pattern.is_empty() && !use_last_pattern {
        return None;
    }
    Some(PreviewPattern {
        magic,
        range,
        default_current_line,
        delimiter,
        pattern,
        use_last_pattern,
    })
}

fn parse_range(input: &str, cursor: &mut usize) -> Result<Option<Range>, ParseError> {
    let start = *cursor;
    if input.as_bytes().get(start) == Some(&b'%') {
        *cursor += 1;
        return Ok(Some(Range {
            start: Some(Address {
                base: AddressBase::Line(1),
                offsets: Vec::new(),
            }),
            end: Some(Address {
                base: AddressBase::Last,
                offsets: Vec::new(),
            }),
            kind: RangeKind::WholeBuffer,
        }));
    }

    let mut first = parse_address(input, cursor)?;
    let mut last_separator = None;
    let mut cursor_advance = false;
    let mut end = None;
    loop {
        let separator_offset = skip_ascii_space(input, *cursor);
        let separator = match input.as_bytes().get(separator_offset) {
            Some(b',') => RangeSeparator::Comma,
            Some(b';') => RangeSeparator::Semicolon,
            _ => break,
        };
        cursor_advance |= separator == RangeSeparator::Semicolon;
        last_separator = Some(separator);
        *cursor = skip_ascii_space(input, separator_offset + 1);
        let next = parse_address(input, cursor)?.unwrap_or(Address {
            base: AddressBase::Current,
            offsets: Vec::new(),
        });
        if end.is_some() {
            first = end.take();
        } else if first.is_none() {
            first = Some(Address {
                base: AddressBase::Current,
                offsets: Vec::new(),
            });
        }
        end = Some(next);
    }
    if first.is_none() && last_separator.is_none() {
        *cursor = start;
        return Ok(None);
    }
    if let Some(separator) = last_separator {
        return Ok(Some(Range {
            start: first,
            end,
            kind: RangeKind::Pair {
                separator,
                cursor_advance,
            },
        }));
    }
    Ok(Some(Range {
        start: first,
        end: None,
        kind: RangeKind::Single,
    }))
}

fn parse_address(input: &str, cursor: &mut usize) -> Result<Option<Address>, ParseError> {
    let bytes = input.as_bytes();
    let start = *cursor;
    let base = match bytes.get(*cursor).copied() {
        Some(b'.') => {
            *cursor += 1;
            Some(AddressBase::Current)
        }
        Some(b'$') => {
            *cursor += 1;
            Some(AddressBase::Last)
        }
        Some(b'\'') => {
            let mark_offset = *cursor + 1;
            let Some(mark) = input[mark_offset..].chars().next() else {
                return Err(error(ErrorCode::E488, *cursor, "mark name required"));
            };
            *cursor = mark_offset + mark.len_utf8();
            Some(AddressBase::Mark(mark))
        }
        Some(b'/' | b'?') => {
            let delimiter = bytes[*cursor];
            let (pattern, end) = parse_pattern(input, *cursor, delimiter, false)?;
            *cursor = end;
            if delimiter == b'/' {
                Some(AddressBase::ForwardSearch(pattern))
            } else {
                Some(AddressBase::BackwardSearch(pattern))
            }
        }
        Some(digit) if digit.is_ascii_digit() => {
            let number_start = *cursor;
            while bytes.get(*cursor).is_some_and(u8::is_ascii_digit) {
                *cursor += 1;
            }
            let number = input[number_start..*cursor]
                .parse::<u64>()
                .map_err(|_| error(ErrorCode::E488, number_start, "line number is too large"))?;
            Some(AddressBase::Line(number))
        }
        Some(b'+' | b'-') => Some(AddressBase::Current),
        _ => None,
    };
    let Some(base) = base else {
        return Ok(None);
    };
    let mut offsets = Vec::new();
    while matches!(bytes.get(*cursor), Some(b'+' | b'-')) {
        if offsets.len() == MAX_OFFSETS {
            return Err(error(ErrorCode::E488, *cursor, "too many address offsets"));
        }
        let sign = if bytes[*cursor] == b'+' {
            1_i64
        } else {
            -1_i64
        };
        *cursor += 1;
        let magnitude_start = *cursor;
        while bytes.get(*cursor).is_some_and(u8::is_ascii_digit) {
            *cursor += 1;
        }
        let magnitude = if magnitude_start == *cursor {
            1
        } else {
            input[magnitude_start..*cursor]
                .parse::<i64>()
                .map_err(|_| error(ErrorCode::E488, magnitude_start, "offset is too large"))?
        };
        offsets.push(sign * magnitude);
    }
    if *cursor == start {
        return Ok(None);
    }
    Ok(Some(Address { base, offsets }))
}

fn parse_pattern(
    input: &str,
    start: usize,
    delimiter: u8,
    require_close: bool,
) -> Result<(String, usize), ParseError> {
    let bytes = input.as_bytes();
    let mut cursor = start + 1;
    let pattern_start = cursor;
    let mut escaped = false;
    while let Some(&byte) = bytes.get(cursor) {
        if !escaped && byte == delimiter {
            return Ok((input[pattern_start..cursor].to_owned(), cursor + 1));
        }
        escaped = !escaped && byte == b'\\';
        if byte != b'\\' {
            escaped = false;
        }
        cursor += 1;
    }
    if require_close {
        return Err(error(ErrorCode::E488, start, "unterminated search pattern"));
    }
    // Address form: `skip_regexp` takes the rest of the line when the
    // closing delimiter is missing, so `:/#if FOO` is a search.
    Ok((input[pattern_start..cursor].to_owned(), cursor))
}

/// The argument-scan family that ends at a newline (`ex_docmd.c:2295-2324`):
/// `eap->usefilter`, plus the non-`TRLBAR` shell commands whose `CMD_*` entry
/// takes the whole line verbatim.
fn shell_arg_family(name: &str, flags: CommandFlags, usefilter: bool) -> bool {
    usefilter
        || (!flags.contains(CommandFlags::TRLBAR)
            && matches!(name, "!" | "terminal" | "global" | "vglobal"))
}

/// Whitespace prefix of `*eap->cmdlinep` for a command parsed at `before`:
/// the text just past the previous `|`/`\n` separator — or the start of the
/// input — through the first non-blank byte (eval/vars.c:772-776). A `|` or
/// `\n` earlier than the separator cannot precede a parsed command: anything
/// that swallows it (`:normal`, user commands, quoted arguments) also
/// swallows the text this command was read from, so the last separator
/// before `before` is always the real chunk boundary.
fn chunk_ws(input: &str, before: usize) -> &str {
    let chunk_start = input[..before]
        .rfind(['|', '\n'])
        .map_or(0, |pos| pos + 1);
    let indent_len = input[chunk_start..]
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    &input[chunk_start..chunk_start + indent_len]
}

fn command_end(
    input: &str,
    args_start: usize,
    flags: CommandFlags,
    usefilter: bool,
    command: &ResolvedCommand,
) -> usize {
    let name = command.name();
    // `do_one_cmd` ends a shell-family argument at a newline (`ex_docmd.c`
    // 2295-2324): `:read !cmd`, `:write !cmd`, `:!`, `:terminal`, `:global`
    // and `:vglobal`. A backslash-newline inside is a line continuation that
    // survives the scan.
    if shell_arg_family(name, flags, usefilter) {
        return newline_command_end(input, args_start, true);
    }
    // `separate_nextcmd` scans a `TRLBAR` argument for `|`/comment/newline
    // (ex_docmd.c:4130-4178). The substitute family shares the scan so a
    // separator after its flags still splits the command.
    if flags.contains(CommandFlags::TRLBAR)
        || matches!(name, "substitute" | "smagic" | "snomagic")
    {
        return trlbar_command_end(input, args_start, flags, name);
    }
    // `cmd_has_expr_args` (ex_docmd.c:1464-1473): `do_one_cmd` bounds the
    // argument with a `skip_expr` loop, so the command ends at the first
    // `|` or `\n` the expression scan cannot consume.
    if matches!(name, "execute" | "echo" | "echon" | "echomsg" | "echoerr") {
        return expression_command_end(input, args_start, NewlineEnd::AnyDepth);
    }
    // Handlers that evaluate their own argument and then split the tail with
    // `check_nextcmd`: a newline separates only at the expression's top
    // level — inside `()`/`[]`/`{}` it stays in the argument.
    if matches!(
        name,
        "let"
            | "const"
            | "call"
            | "for"
            | "if"
            | "elseif"
            | "while"
            | "return"
            | "throw"
            | "eval"
            | "catch"
            | "cexpr"
            | "cgetexpr"
            | "lexpr"
            | "lgetexpr"
            | "caddexpr"
            | "laddexpr"
    ) {
        return expression_command_end(input, args_start, NewlineEnd::Toplevel);
    }
    // `ex_wincmd` (ex_docmd.c:6523-6549) consumes the window-command key
    // itself and then splits the tail with `check_nextcmd`
    // (ex_docmd.c:4630-4637), so the command ends after its key form instead
    // of after the whole line.
    if name == "wincmd" {
        return wincmd_command_end(input, args_start);
    }
    // Commands whose argument is the whole remainder of the cmdline: script
    // source a language handler evaluates, a `:command`/`:autocmd` body that
    // keeps following lines as its definition, `:normal` keys, a nested
    // cmdline the handler re-executes (`:windo let a\nlet b` runs both per
    // window), and user commands (`:Mc a\nlet x` puts the newline in
    // `<args>`). A newline is content here, never a separator.
    if matches!(command, ResolvedCommand::User(_))
        || matches!(
            name,
            "normal"
                | "command"
                | "autocmd"
                | "debug"
                | "lua"
                | "luado"
                | "mzscheme"
                | "perl"
                | "perldo"
                | "python"
                | "python3"
                | "pythonx"
                | "pyx"
                | "pyxdo"
                | "ruby"
                | "rubydo"
                | "tcl"
                | "tcldo"
                | "argdo"
                | "bufdo"
                | "tabdo"
                | "windo"
                | "confirm"
                | "browse"
                | "unsilent"
                | "filter"
        )
    {
        return input.len();
    }
    // Every other non-`TRLBAR` handler parses its own argument and then
    // splits the tail with `check_nextcmd`, so the command ends at the
    // first newline (verified against `nvim_command`: `:edit`,
    // `:delfunction`, `:syntax` and friends all resume after `\n`).
    newline_command_end(input, args_start, false)
}

/// Ends a command at its first newline separator. `continuation` marks the
/// shell-family scan where a backslash-newline pair is a line continuation
/// inside the argument rather than a separator (ex_docmd.c:2312-2318).
fn newline_command_end(input: &str, args_start: usize, continuation: bool) -> usize {
    let bytes = input.as_bytes();
    let mut cursor = args_start;
    while let Some(&byte) = bytes.get(cursor) {
        if byte == b'\n'
            && !(continuation && cursor > args_start && bytes[cursor - 1] == b'\\')
        {
            return cursor;
        }
        cursor += 1;
    }
    input.len()
}

/// The `separate_nextcmd` scan for a `TRLBAR` argument: the command ends at
/// `|` (except :append/:change/:insert), a `"` comment (except `EX_NOTRLCOM`),
/// or a newline (ex_docmd.c:4130-4178).
fn trlbar_command_end(
    input: &str,
    args_start: usize,
    flags: CommandFlags,
    name: &str,
) -> usize {
    let bar_breaks = !matches!(name, "append" | "change" | "insert");
    let bytes = input.as_bytes();
    let mut escaped = false;
    let mut cursor = args_start;
    // vimgrep family patterns are regexes that may contain `|`; skip the
    // leading pattern (plus g/j/f flags) before scanning for bar separators
    // and quote comments, so ":vimgrep /foo|bar/ f | copen" splits after the
    // file argument (separate_nextcmd: ex_docmd.c:4112-4165).
    if is_grep_command(name) {
        cursor = skip_grep_pattern(input, args_start);
    }
    while let Some(&byte) = bytes.get(cursor) {
        if escaped {
            escaped = false;
            cursor += 1;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            cursor += 1;
            continue;
        }
        if byte == b'|' && bar_breaks {
            return cursor;
        }
        if byte == b'"'
            && !flags.contains(CommandFlags::NOTRLCOM)
            && !is_comment_quote_exception(bytes, args_start, cursor, flags, name)
        {
            return cursor;
        }
        // `separate_nextcmd` ends a TRLBAR argument at a newline too
        // (ex_docmd.c:4170-4178).
        if byte == b'\n' {
            return cursor;
        }
        cursor += 1;
    }
    input.len()
}

/// The `wincmd` command boundary (`ex_wincmd`, ex_docmd.c:6522-6551).
///
/// The generated `wincmd` metadata deliberately has no `EX_TRLBAR`, so
/// `separate_nextcmd` never scans its argument and the handler computes
/// `eap->nextcmd = check_nextcmd(p)` itself from just past the
/// window-command key. The command therefore ends after one key — two for
/// the `g`/Ctrl-G forms (`ex_docmd.c:6527-6534`) — followed only by a bar
/// after optional spaces or tabs (`check_nextcmd`, `ex_docmd.c:4630-4637`).
/// Anything else stays in the argument so the handler can reject it, and a
/// literal `|` is consumed as the key itself, which is exactly why
/// `EX_TRLBAR` can never be added to `wincmd`.
fn wincmd_command_end(input: &str, args_start: usize) -> usize {
    let bytes = input.as_bytes();
    // Digits leading the argument are the command's count (`wincmd 10<`),
    // the same way a `:10wincmd` prefix range is: the window key is the
    // first non-digit byte after them.
    let mut key_start = args_start;
    while matches!(bytes.get(key_start), Some(b'0'..=b'9')) {
        key_start += 1;
    }
    key_start = skip_ascii_space(input, key_start);
    let Some(&key) = bytes.get(key_start) else {
        // NEEDARG rejects an empty argument before the handler ever runs.
        return input.len();
    };
    let mut cursor = key_start + 1;
    if key == b'g' || key == 0x07 {
        // The `g`/Ctrl-G forms consume a second command character; a
        // missing one stays missing so the handler reports E474 instead of
        // the parser splitting the line here (ex_docmd.c:6529-6532).
        cursor += usize::from(bytes.get(cursor).is_some());
    }
    cursor = skip_ascii_space(input, cursor);
    // `check_nextcmd` accepts `|` or `\n` as the separator (ex_docmd.c:4648).
    if matches!(bytes.get(cursor), Some(b'|' | b'\n')) {
        return cursor;
    }
    input.len()
}

/// Where a newline ends an expression-scanned argument. Mirrors the two ways
/// upstream's `do_one_cmd` bounds the text a command sees (`ex_docmd.c`):
/// the `skip_expr` loop used by `cmd_has_expr_args` aborts wherever the
/// expression parser stops — including inside `()`, `[]`, `{}` — while the
/// `let`/`if`/... handlers evaluate the argument themselves and then run
/// `check_nextcmd`, so a newline inside nested forms stays in the argument
/// for the evaluator to reject (E15 "Invalid expression").
#[derive(Clone, Copy)]
enum NewlineEnd {
    AnyDepth,
    Toplevel,
}

fn expression_command_end(input: &str, start: usize, newline: NewlineEnd) -> usize {
    let bytes = input.as_bytes();
    let mut cursor = start;
    let mut quote = None;
    let mut escaped = false;
    let mut nesting = 0_usize;
    while let Some(&byte) = bytes.get(cursor) {
        if let Some(delimiter) = quote {
            if delimiter == b'\'' && byte == b'\'' && bytes.get(cursor + 1) == Some(&b'\'') {
                cursor += 2;
                continue;
            }
            if escaped {
                escaped = false;
            } else if delimiter == b'"' && byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
            cursor += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' | b'[' | b'{' => nesting = nesting.saturating_add(1),
            b')' | b']' | b'}' => nesting = nesting.saturating_sub(1),
            b'|' if nesting == 0
                && bytes.get(cursor + 1) != Some(&b'|')
                && (cursor == start || bytes.get(cursor - 1) != Some(&b'|')) =>
            {
                return cursor;
            }
            b'=' if matches!(newline, NewlineEnd::Toplevel)
                && nesting == 0
                && !matches!(
                    cursor.checked_sub(1).and_then(|prev| bytes.get(prev)),
                    Some(b'=' | b'+' | b'-' | b'*' | b'/' | b'%' | b'.')
                )
                && bytes.get(cursor + 1) == Some(&b'<')
                && bytes.get(cursor + 2) == Some(&b'<') =>
            {
                return heredoc_command_end(input, cursor + 3, start);
            }
            b'\n' if matches!(newline, NewlineEnd::AnyDepth) || nesting == 0 => {
                return cursor;
            }
            _ => {}
        }
        cursor += 1;
    }
    input.len()
}

/// The command boundary for `let`/`const` heredocs (`=<<`).
///
/// `ex_let` hands the text after `<<` to `heredoc_get` (eval/vars.c:739-912),
/// which reads body lines out of the cmdline string itself until the
/// terminator line — so the command's argument runs through the terminator,
/// not to the first newline. `after_shift` is the index just past `<<` and
/// `args_start` the argument's start (used to find the command line's own
/// indentation, which `trim` allows on the terminator line, vars.c:853-857).
fn heredoc_command_end(input: &str, after_shift: usize, args_start: usize) -> usize {
    let header_end = input[after_shift..]
        .find('\n')
        .map_or(input.len(), |rel| after_shift + rel);
    // Words after `<<`: optional `trim`/`eval` modifiers, then the marker.
    let mut words = input[after_shift..header_end].trim_start_matches([' ', '\t']);
    let mut trim = false;
    loop {
        let end = words
            .find(|character: char| character.is_ascii_whitespace())
            .unwrap_or(words.len());
        match &words[..end] {
            "trim" => trim = true,
            "eval" => {}
            _ => break,
        }
        words = words[end..].trim_start_matches([' ', '\t']);
    }
    let marker_end = words
        .find(|character: char| character.is_ascii_whitespace())
        .unwrap_or(words.len());
    let marker = &words[..marker_end];
    // A missing or invalid marker fails inside `heredoc_get` before any body
    // line is consumed: the command owns the rest of the string and the
    // handler reports E172/E221/E991.
    if marker.is_empty()
        || marker.starts_with('"')
        || marker.as_bytes()[0].is_ascii_lowercase()
    {
        return input.len();
    }
    // `trim` allows the terminator to repeat the whitespace prefix of
    // `*eap->cmdlinep` — the command chunk's own indent, which for a command
    // after `|` is the whitespace that follows the bar (eval/vars.c:853-857).
    let indent = chunk_ws(input, args_start);
    let mut body = header_end + 1;
    while body < input.len() {
        let line_end = input[body..]
            .find('\n')
            .map_or(input.len(), |rel| body + rel);
        let line = &input[body..line_end];
        let marker_line = if trim {
            line.strip_prefix(indent).unwrap_or(line)
        } else {
            line
        };
        if marker_line == marker {
            // The argument ends at the newline terminating the marker line
            // (or at end of input when the marker is last).
            return line_end;
        }
        body = line_end + 1;
    }
    input.len()
}

fn is_comment_quote_exception(
    bytes: &[u8],
    args_start: usize,
    cursor: usize,
    flags: CommandFlags,
    command_name: &str,
) -> bool {
    is_initial_quoted_register(bytes, args_start, cursor, flags)
        || (command_name == "@" && cursor == args_start)
        || (command_name == "redir"
            && cursor == args_start + 1
            && bytes.get(args_start) == Some(&b'@'))
}

fn is_initial_quoted_register(
    bytes: &[u8],
    args_start: usize,
    cursor: usize,
    flags: CommandFlags,
) -> bool {
    flags.contains(CommandFlags::REGSTR)
        && cursor == args_start
        && bytes
            .get(cursor + 1)
            .copied()
            .is_some_and(|byte| is_register(char::from(byte)))
}

fn take_register(args: &mut String) -> Option<char> {
    let trimmed = args.trim_start();
    let skipped = args.len() - trimmed.len();
    let mut chars = trimmed.char_indices();
    let (first_offset, first) = chars.next()?;
    let (register, consumed) = if first == '"' {
        let (offset, register) = chars.next()?;
        (register, offset + register.len_utf8())
    } else {
        let next = chars.next();
        if next.is_some_and(|(_, character)| !character.is_ascii_whitespace()) {
            return None;
        }
        (first, first_offset + first.len_utf8())
    };
    if !is_register(register) {
        return None;
    }
    args.drain(..skipped + consumed);
    *args = args.trim_start().to_owned();
    Some(register)
}

fn is_register(character: char) -> bool {
    character.is_ascii_alphanumeric() || "\"-:.%#=*+_/@".contains(character)
}

/// `parse_count` (`ex_docmd.c:1395-1430`): a leading digit run on a `COUNT`
/// command is the count.
///
/// The digits are taken greedily and whatever follows stays in the argument,
/// which is what `:sleep 100m` needs. Only a `BUFNAME` command insists the
/// digits end at whitespace or end-of-argument, so that `:buffer 123foo`
/// stays a buffer name rather than becoming count 123 plus "foo".
fn take_count(args: &mut String, buffer_name: bool) -> Option<u64> {
    let trimmed = args.trim_start();
    let skipped = args.len() - trimmed.len();
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    if buffer_name
        && trimmed
            .as_bytes()
            .get(digits)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        return None;
    }
    let count = trimmed[..digits].parse::<u64>().ok()?;
    args.drain(..skipped + digits);
    *args = args.trim_start().to_owned();
    Some(count)
}

fn skip_space_and_colons(input: &str, mut cursor: usize) -> usize {
    loop {
        cursor = skip_ascii_space(input, cursor);
        if input.as_bytes().get(cursor) == Some(&b':') {
            cursor += 1;
        } else {
            return cursor;
        }
    }
}

/// `skipwhite` (`charset.c`): `ascii_iswhite` is space and tab only
/// (`ascii_defs.h:84-87`), so CR, NL and every other control byte stop it.
fn skip_ascii_space(input: &str, mut cursor: usize) -> usize {
    while matches!(input.as_bytes().get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    cursor
}

/// `del_trailing_spaces` (`strings.c:429-436`): removes trailing spaces and
/// tabs, stops at one escaped with `\` or CTRL-V, and never removes the
/// first byte of the argument.
fn del_trailing_spaces(input: &str, start: usize, mut end: usize) -> usize {
    let bytes = input.as_bytes();
    while end > start + 1
        && matches!(bytes[end - 1], b' ' | b'\t')
        && !matches!(bytes[end - 2], b'\\' | 0x16)
    {
        end -= 1;
    }
    end
}

fn error(code: ErrorCode, offset: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        code,
        offset,
        message: message.into(),
        command: None,
    }
}

fn command_error(
    command: &ResolvedCommand,
    code: ErrorCode,
    offset: usize,
    message: impl Into<String>,
) -> ParseError {
    let mut error = error(code, offset, message);
    if let ResolvedCommand::Builtin(spec) = command {
        error.command = Some(spec.name);
    }
    error
}
