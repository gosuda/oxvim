//! Filesystem builtins: the path-and-file family delegates to
//! [`crate::fs_builtins`]; `swapfilelist` additionally needs the `directory`
//! option its `recover_names` scan walks.

use crate::excmd_exec::ExEditorAccess;
use ox_eval::{Evaluator, Parser as ExprParser, Scope};
use ox_types::{OxStr, Typval};

use crate::options::OptionValue;
use crate::script::FileIO;

use crate::excmd_exec::{EvalHost, VimRegex};

/// `f_readdir`'s optional `{expr}` filter (`fs.c` `readdir_checkitem`
/// 1182-1207): `v:val` and the callback's single argument both carry the
/// entry name, a 0 result drops the entry, -1 ends the scan, and anything
/// else keeps it. Strings evaluate as expressions; Funcrefs and Partials
/// invoke with the name.
fn readdir_filtered<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let list = crate::fs_builtins::call(host.runtime.scripts.io(), "readdir", &args[..1])?;
    let Typval::List(reference) = &list else {
        return Ok(list);
    };
    let entries = reference.borrow().items.clone();
    let regex = VimRegex;
    let mut kept = Vec::with_capacity(entries.len());
    for item in entries {
        let result = match &args[1] {
            callable @ (Typval::Funcref(_) | Typval::Partial(_)) => {
                Evaluator::new(host, &regex).invoke(callable.clone(), vec![item.clone()], scope)?
            }
            other => {
                let source = super::input_string_arg(other)?;
                let expression = ExprParser::new(source.as_bytes()).parse()?;
                // `readdir_checkitem`: set_vim_var_string(VV_VAL) before
                // eval, restore after — the save/restore is unconditional.
                // The slot lives on the live scope's `v:` map: the editor's
                // vvars only reach the scope through a versioned sync this
                // expression never triggers.
                let saved = scope
                    .vim
                    .iter()
                    .position(|(key, _)| key.as_bytes() == b"val")
                    .map(|at| (at, scope.vim[at].1.clone()));
                match saved {
                    Some((at, _)) => scope.vim[at].1 = item.clone(),
                    None => scope.vim.push((OxStr::from("val"), item.clone())),
                }
                let outcome = Evaluator::new(host, &regex).eval(&expression, scope);
                match saved {
                    Some((at, value)) => scope.vim[at].1 = value,
                    None => scope
                        .vim
                        .retain(|(key, _)| key.as_bytes() != b"val"),
                }
                outcome?
            }
        };
        match ox_eval::builtins::number_arg(&result)? {
            0 => continue,
            -1 => break,
            _ => kept.push(item),
        }
    }
    Ok(Typval::list(kept))
}

/// Routes one filesystem builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    if name == "readdir" && args.len() >= 2 {
        return readdir_filtered(host, args, scope);
    }
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

#[cfg(test)]
mod tests {
    use ox_eval::ScopeKind;
    use ox_types::Typval;

    use crate::excmd_exec::ExEditorAccess as _;
    use crate::{Editor, ExExecutor, TestEditorAccess};

    fn global_string(exec: &ExExecutor<crate::script::RealFileIO>, name: &str) -> Option<String> {
        match exec
            .scope()
            .get_scoped(ScopeKind::Global, name.as_bytes(), 0)
            .ok()?
        {
            Typval::String(text) => Some(text.to_string_lossy().into_owned()),
            Typval::Number(number) => Some(number.to_string()),
            _ => None,
        }
    }

    /// `readdir({dir}, {expr})` binds `v:val` to each entry and keeps it
    /// while the expression is non-zero (`readdir_checkitem`,
    /// `fs.c:1182-1207`): 0 drops the entry and -1 also stops the scan.
    #[test]
    fn readdir_evaluates_filter_with_vval() {
        let dir = std::env::temp_dir().join(format!("oxvim-readdir-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["alpha.txt", "beta.md", "gamma.txt"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let path = dir.display().to_string();
        exec.execute_script(
            &editor,
            "<test>",
            &format!("let g:kept = join(sort(readdir('{path}', 'v:val =~ \"txt$\"')), '|')"),
        )
        .unwrap();
        assert_eq!(
            global_string(&exec, "kept"),
            Some("alpha.txt|gamma.txt".to_owned())
        );
        exec.execute_script(
            &editor,
            "<test>",
            &format!("let g:stopped = len(readdir('{path}', '-1'))"),
        )
        .unwrap();
        assert_eq!(global_string(&exec, "stopped"), Some("0".to_owned()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
