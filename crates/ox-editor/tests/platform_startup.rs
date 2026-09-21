//! Public script and filesystem boundaries used during editor startup.

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use ox_editor::script::{RealFileIO, ScriptCtx, ScriptError};
use ox_editor::{Editor, ExExecutor, TestEditorAccess};
use ox_types::Typval;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "oxvim-platform's paths-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("could not remove fixture {}: {error}", self.0.display());
        }
    }
}

fn vim_string(path: &Path) -> io::Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| io::Error::other("fixture path must be UTF-8"))?;
    Ok(format!("'{}'", value.replace('\'', "''")))
}

fn source_lines(source: &str) -> Result<Vec<String>, ScriptError> {
    ScriptCtx::new(RealFileIO)
        .join_logical_lines(source)
        .map(|lines| lines.into_iter().map(|line| line.text).collect())
}

#[test]
fn crlf_script_lines_follow_the_host_file_format() -> Result<(), ScriptError> {
    let expected = if cfg!(windows) {
        ["echo 1", "echo 2", ""]
    } else {
        ["echo 1\r", "echo 2\r", ""]
    };
    assert_eq!(source_lines("echo 1\r\necho 2\r\n")?, expected);
    Ok(())
}

#[test]
fn a_unix_separator_ends_windows_crlf_conversion() -> Result<(), ScriptError> {
    let expected = if cfg!(windows) {
        ["echo 1", "echo 2", "echo 3\r", ""]
    } else {
        ["echo 1\r", "echo 2", "echo 3\r", ""]
    };
    assert_eq!(source_lines("echo 1\r\necho 2\necho 3\r\n")?, expected);
    Ok(())
}

#[test]
fn an_unterminated_carriage_return_is_script_data() -> Result<(), ScriptError> {
    let expected = if cfg!(windows) {
        ["echo 1", "echo 2\r"]
    } else {
        ["echo 1\r", "echo 2\r"]
    };
    assert_eq!(source_lines("echo 1\r\necho 2\r")?, expected);
    Ok(())
}

#[test]
fn glob_preserves_native_and_canonical_absolute_roots() -> Result<(), Box<dyn Error>> {
    let root = TempRoot::new()?;
    fs::write(root.0.join("fixture.vim"), b"echo 1\n")?;
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    // On Windows canonicalize also exercises a verbatim drive prefix.
    for directory in [&root.0, &fs::canonicalize(&root.0)?] {
        assert!(directory.is_absolute());
        executor.execute_line(
            &editor,
            &format!(
                "let g:matched = glob({}, 0, 1) == [{}]",
                vim_string(&directory.join("*.vim"))?,
                vim_string(&directory.join("fixture.vim"))?
            ),
        )?;
        assert_eq!(
            executor
                .scope()
                .global
                .iter()
                .find(|(name, _)| name.as_bytes() == b"matched")
                .map(|(_, value)| value.clone()),
            Some(Typval::Number(1))
        );
    }
    Ok(())
}

#[test]
fn packadd_sources_a_plugin_from_an_absolute_packpath() -> Result<(), Box<dyn Error>> {
    let root = TempRoot::new()?;
    let plugin = root.0.join("pack/site/opt/matchit/plugin");
    fs::create_dir_all(&plugin)?;
    fs::write(plugin.join("matchit.vim"), b"let g:package_loaded = 47\n")?;
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    executor.execute_line(
        &editor,
        &format!("let &packpath = {}", vim_string(&root.0)?),
    )?;
    executor.execute_line(&editor, "packadd matchit")?;
    assert_eq!(
        executor
            .scope()
            .global
            .iter()
            .find(|(name, _)| name.as_bytes() == b"package_loaded")
            .map(|(_, value)| value.clone()),
        Some(Typval::Number(47))
    );
    Ok(())
}
