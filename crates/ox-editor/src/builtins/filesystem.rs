//! Filesystem builtins: the path-and-file family delegates to
//! [`crate::fs_builtins`]; `swapfilelist` additionally needs the `directory`
//! option its `recover_names` scan walks.

use crate::excmd_exec::ExEditorAccess;
use ox_types::Typval;

use crate::options::OptionValue;
use crate::script::FileIO;

use crate::excmd_exec::EvalHost;

/// Routes one filesystem builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if name == "swapfilelist" {
        // f_swapfilelist iterates the 'directory' option (recover_names); the
        // option has no static default upstream (it is computed at startup),
        // so an unset store reads as "." — the first entry of every platform
        // default.
        let directory =
            host.access
                .with_ex_editor(|editor| match editor.options().get_global("directory") {
                    Ok(OptionValue::String(value)) => value.clone(),
                    _ => ".".to_owned(),
                });
        return crate::fs_builtins::swapfilelist(host.runtime.scripts.io(), args.len(), &directory);
    }
    if name == "writefile" {
        // The `D` flag is `add_defer("delete", ...)` against the enclosing
        // function frame, so this one needs the runtime and not just the
        // `FileIO` seam.
        crate::fs_builtins::check_writefile_arity(args.len())?;
        let mut deferred = None;
        let in_function = host.runtime.can_add_defer();
        let result = crate::fs_builtins::writefile(
            host.runtime.scripts.io(),
            args,
            in_function,
            &mut deferred,
        );
        if let Some(path) = deferred {
            host.runtime
                .push_deferred_delete(path, crate::fs_builtins::DeleteMode::File);
        }
        return result;
    }
    if name == "mkdir" {
        // The `D`/`R` flags are `add_defer("delete", dir, 'd'/'rf')` against
        // the enclosing function frame, so `mkdir` needs the runtime like
        // `writefile` does.
        crate::fs_builtins::check_arity("mkdir", args.len())?;
        let mut deferred = None;
        let result = crate::fs_builtins::mkdir(
            host.runtime.scripts.io(),
            args,
            host.runtime.can_add_defer(),
            &mut deferred,
        );
        if let Some((path, mode)) = deferred {
            host.runtime.push_deferred_delete(path, mode);
        }
        return result;
    }
    if name == "glob" && !args.is_empty() {
        if let Some(result) = glob_with_brace_expansion(host.runtime.scripts.io(), args)? {
            return Ok(result);
        }
    }
    crate::fs_builtins::call(host.runtime.scripts.io(), name, args)
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let Some(open) = bytes.iter().position(|b| *b == b'{') else {
        return vec![pattern.to_string()];
    };
    let Some(close) = bytes[open..].iter().position(|b| *b == b'}').map(|p| open + p) else {
        return vec![pattern.to_string()];
    };
    let inner = &pattern[open + 1..close];
    if !inner.contains(',') {
        return vec![pattern.to_string()];
    }
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let mut results = Vec::new();
    for alt in inner.split(',') {
        let expanded = format!("{prefix}{alt}{suffix}");
        results.extend(expand_braces(&expanded));
    }
    results
}

fn glob_with_brace_expansion<F: FileIO>(
    io: &F,
    args: &[Typval],
) -> ox_eval::Result<Option<Typval>> {
    let Ok(pattern_str) = crate::builtins::input_string_arg(&args[0]) else {
        return Ok(None);
    };
    let pattern = pattern_str.to_string_lossy();
    if !pattern.contains('{') || !pattern.contains('}') {
        return Ok(None);
    }
    let patterns = expand_braces(&pattern);
    if patterns.len() <= 1 {
        return Ok(None);
    }
    let list = args.get(2).is_some_and(Typval::is_truthy);
    let mut all_matches = Vec::new();
    for pat in patterns {
        collect_glob_matches(io, args, &pat, &mut all_matches);
    }
    all_matches.sort();
    all_matches.dedup();
    if list {
        let items = all_matches
            .into_iter()
            .map(|s| Typval::String(ox_types::OxStr::from(s.as_str())))
            .collect();
        Ok(Some(Typval::list(items)))
    } else {
        Ok(Some(Typval::String(ox_types::OxStr::from(
            all_matches.join("\n").as_str(),
        ))))
    }
}

fn collect_glob_matches<F: FileIO>(
    io: &F,
    args: &[Typval],
    pat: &str,
    all_matches: &mut Vec<String>,
) {
    let mut call_args = args.to_vec();
    call_args[0] = Typval::String(ox_types::OxStr::from(pat));
    call_args.truncate(4);
    if call_args.len() < 3 {
        call_args.resize(3, Typval::Number(0));
    }
    call_args[2] = Typval::Number(1);
    let Ok(Typval::List(matches)) = crate::fs_builtins::call(io, "glob", &call_args) else {
        return;
    };
    let Ok(matches) = matches.try_borrow() else {
        return;
    };
    for item in &matches.items {
        if let Typval::String(s) = item {
            all_matches.push(s.to_string_lossy().into_owned());
        }
    }
}
