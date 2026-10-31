//! Functions for creating session, vimrc, exrc, and view files.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use ox_eval::{Scope, ScopeKind};
use ox_excmd::ExCommand;
use ox_types::{BufHandle, Object, OxStr, Typval, WinHandle};

use crate::autocmd::Event;
use crate::editor::Editor;
use crate::excmd_exec::{
    ExEditorAccess, ExRuntime, Flow, LuaExec, fire_buffer_lifecycle,
};
use crate::layout::{Frame, Layout, TabpageState};
use crate::options::OptionValue;
use crate::script::{FileIO, FileKind};

/// Default file name for `:mksession`.
pub const SESSION_FILE: &str = "Session.vim";
/// Default file name for `:mkvimrc`.
pub const VIMRC_FILE: &str = ".nvimrc";
/// Default file name for `:mkexrc`.
pub const EXRC_FILE: &str = ".exrc";

const PATH_ESC_CHARS: &[u8] = b" \t\n*?[{`$\\%#'\"|!<";

/// Escapes a file or directory name for use in Ex commands.
#[must_use]
pub fn escape_fname(name: &str) -> String {
    let mut escaped = Vec::with_capacity(name.len());
    for &byte in name.as_bytes() {
        if PATH_ESC_CHARS.contains(&byte) {
            escaped.push(b'\\');
        }
        escaped.push(byte);
    }
    if matches!(escaped.first(), Some(b'>' | b'+')) || escaped == b"-" {
        escaped.insert(0, b'\\');
    }
    String::from_utf8(escaped).unwrap_or_else(|_| name.to_owned())
}

/// Normalizes backslashes to forward slashes and escapes special characters.
#[must_use]
pub fn normalize_and_escape(path: &str) -> String {
    escape_fname(&path.replace('\\', "/"))
}
fn resolve_absolute_cwd(cwd_path: &Path) -> PathBuf {
    let cwd_str = cwd_path.to_string_lossy();
    let mut resolved = if let Some(rest) = cwd_str.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(rest)
        } else {
            cwd_path.to_path_buf()
        }
    } else if cwd_str == "~" {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home)
        } else {
            cwd_path.to_path_buf()
        }
    } else {
        cwd_path.to_path_buf()
    };
    if !resolved.is_absolute() {
        if let Ok(cur) = std::env::current_dir() {
            resolved = cur.join(&resolved);
        }
    }
    resolved
}

fn error_flow<F: FileIO>(
    runtime: &ExRuntime<F>,
    code: &'static str,
    message: impl Into<String>,
) -> Flow {
    Flow::Exception(runtime.exception(code, message))
}

/// Parsed flags from the `'sessionoptions'` option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    /// Save empty windows.
    pub blank: bool,
    /// Save hidden and unloaded buffers.
    pub buffers: bool,
    /// Save current and local directories.
    pub curdir: bool,
    /// Save manual and local folds.
    pub folds: bool,
    /// Save uppercase global variables.
    pub globals: bool,
    /// Save help windows.
    pub help: bool,
    /// Save local options and mappings.
    pub localoptions: bool,
    /// Save options and mappings.
    pub options: bool,
    /// Save screen resize.
    pub resize: bool,
    /// Save session directory.
    pub sesdir: bool,
    /// Save all tabpages.
    pub tabpages: bool,
    /// Save terminal windows.
    pub terminal: bool,
    /// Save window position.
    pub winpos: bool,
    /// Save window sizes.
    pub winsize: bool,
    /// Skip 'runtimepath' when saving options.
    pub skiprtp: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self::from_str("blank,buffers,curdir,folds,help,tabpages,winsize,terminal")
    }
}

impl SessionOptions {
    /// Parses flags from a comma-separated string representation.
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        let mut opts = Self {
            blank: false,
            buffers: false,
            curdir: false,
            folds: false,
            globals: false,
            help: false,
            localoptions: false,
            options: false,
            resize: false,
            sesdir: false,
            tabpages: false,
            terminal: false,
            winpos: false,
            winsize: false,
            skiprtp: false,
        };
        for item in s.split(',') {
            match item.trim() {
                "blank" => opts.blank = true,
                "buffers" => opts.buffers = true,
                "curdir" => opts.curdir = true,
                "folds" => opts.folds = true,
                "globals" => opts.globals = true,
                "help" => opts.help = true,
                "localoptions" => opts.localoptions = true,
                "options" => opts.options = true,
                "resize" => opts.resize = true,
                "sesdir" => opts.sesdir = true,
                "tabpages" => opts.tabpages = true,
                "terminal" => opts.terminal = true,
                "winpos" => opts.winpos = true,
                "winsize" => opts.winsize = true,
                "skiprtp" => opts.skiprtp = true,
                _ => {}
            }
        }
        opts
    }
}

struct SessionWriter<'a> {
    editor: &'a Editor,
    options: &'a SessionOptions,
    options_raw: &'a str,
    cwd: &'a str,
    did_lcd: bool,
    output: String,
}

fn tabpage_has_buffer_in_tiled_window(tabpage: &TabpageState, buf: BufHandle) -> bool {
    let layout = tabpage.layout();
    layout
        .windows()
        .iter()
        .any(|&win| layout.window(win).is_ok_and(|st| st.buffer == buf))
}

impl<'a> SessionWriter<'a> {
    fn new(
        editor: &'a Editor,
        options: &'a SessionOptions,
        options_raw: &'a str,
        cwd: &'a str,
    ) -> Self {
        Self {
            editor,
            options,
            options_raw,
            cwd,
            did_lcd: false,
            output: String::new(),
        }
    }

    fn buffer_buftype(&self, buf: BufHandle) -> String {
        self.editor
            .options()
            .get_buffer(buf, "buftype")
            .ok()
            .and_then(|v| match v {
                OptionValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn ses_do_win(&self, tabpage: &TabpageState, win: WinHandle) -> bool {
        if tabpage.window_config(win).ok().flatten().is_some()
            || tabpage.floating_windows().any(|f| f.window == win)
        {
            return false;
        }
        let Ok(win_state) = tabpage.layout().window(win) else {
            return false;
        };
        let buf = win_state.buffer;
        let Ok(buf_state) = self.editor.buffer(buf) else {
            return false;
        };
        let buftype = self.buffer_buftype(buf);
        let is_terminal = buftype == "terminal" || self.editor.is_terminal_buffer(buf);
        let is_nofilename = matches!(buftype.as_str(), "nofile" | "quickfix" | "prompt");
        let has_no_name = buf_state.name().as_bytes().is_empty();

        if has_no_name || (!is_terminal && is_nofilename) {
            return self.options.blank;
        }
        if buftype == "help" {
            return self.options.help;
        }
        if is_terminal {
            return self.options.terminal;
        }
        true
    }

    fn is_buffer_in_tiled_window(&self, buf: BufHandle) -> bool {
        self.editor
            .tabpages()
            .iter()
            .filter_map(|&tab| self.editor.tabpage(tab).ok())
            .any(|tabpage| tabpage_has_buffer_in_tiled_window(tabpage, buf))
    }

    fn is_window_editable(&self, tabpage: &TabpageState, win: WinHandle) -> bool {
        if !self.ses_do_win(tabpage, win) {
            return false;
        }
        let Ok(win_state) = tabpage.layout().window(win) else {
            return false;
        };
        let Ok(buf_state) = self.editor.buffer(win_state.buffer) else {
            return false;
        };
        if buf_state.name().as_bytes().is_empty() {
            return false;
        }
        let buftype = self.buffer_buftype(win_state.buffer);
        let is_terminal = buftype == "terminal" || self.editor.is_terminal_buffer(win_state.buffer);
        if buftype == "help"
            || is_terminal
            || matches!(buftype.as_str(), "nofile" | "quickfix" | "prompt")
        {
            return false;
        }
        true
    }

    fn should_save_buffer(&self, buf: BufHandle, state: &crate::BufferState) -> bool {
        if state.name().as_bytes().is_empty() {
            return false;
        }
        if !self.options.buffers && !self.is_buffer_in_tiled_window(buf) {
            return false;
        }
        let buftype = self.buffer_buftype(buf);
        if buftype == "help" && !self.options.help {
            return false;
        }
        if (buftype == "terminal" || self.editor.is_terminal_buffer(buf))
            && !self.options.terminal
        {
            return false;
        }
        state.flags.contains(crate::BufferFlags::LISTED)
    }

    fn write_header(&mut self) {
        self.output
            .push_str("\" Session file created by Oxvim :mksession\n");
        let raw = self.options_raw;
        self.output.push_str(&format!("set sessionoptions={raw}\n"));
        self.output.push_str("let SessionLoad = 1\n");
        self.output.push_str(
            "let s:so_save = &g:so | let s:siso_save = &g:siso | setg so=0 siso=0 | setl so=-1 siso=-1\n",
        );
        self.output
            .push_str("let v:this_session=expand(\"<sfile>:p\")\n");
        self.output.push_str("silent only\n");
        if self.options.tabpages {
            self.output.push_str("silent tabonly\n");
        }
        if self.options.sesdir {
            self.output
                .push_str("exe \"cd \" . escape(expand(\"<sfile>:p:h\"), ' ')\n");
        } else if self.options.curdir {
            let cwd_path = resolve_absolute_cwd(Path::new(self.cwd));
            let cwd = normalize_and_escape(&cwd_path.to_string_lossy());
            self.output.push_str(&format!("cd {cwd}\n"));
        }
        self.output.push_str(
            "if expand('%') == '' && !&modified && line('$') <= 1 && getline(1) == ''\n  let s:wipebuf = bufnr('%')\nendif\n",
        );

        if !self.options.options {
            self.output.push_str("let s:shortmess_save = &shortmess\n");
        }
        let cur_shm = self
            .editor
            .options()
            .get_global("shortmess")
            .ok()
            .and_then(|v| match v {
                OptionValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let to_add: String = ['a', 'o', 'O']
            .into_iter()
            .filter(|&c| !cur_shm.contains(c))
            .collect();
        if !to_add.is_empty() {
            self.output.push_str(&format!("set shortmess+={to_add}\n"));
        }
    }

    fn write_buffers(&mut self) {
        for buf in self.editor.buffers() {
            let Ok(state) = self.editor.buffer(buf) else {
                continue;
            };
            if !self.should_save_buffer(buf, state) {
                continue;
            }
            let name = state.name().to_string_lossy();
            let escaped = normalize_and_escape(&name);
            self.output.push_str(&format!("badd +0 {escaped}\n"));
        }
        self.output.push_str("argglobal\n%argdel\n");

        if self.options.curdir {
            self.write_buffer_local_dirs();
        }
    }

    fn write_buffer_local_dirs(&mut self) {
        for buf in self.editor.buffers() {
            let Ok(state) = self.editor.buffer(buf) else {
                continue;
            };
            let Some(local_dir) = &state.local_directory else {
                continue;
            };
            if !self.should_save_buffer(buf, state) {
                continue;
            }
            let name = state.name().to_string_lossy();
            let abs_local_dir = if local_dir.is_absolute() {
                local_dir.clone()
            } else {
                Path::new(self.cwd).join(local_dir)
            };
            let abs_local_dir_str = abs_local_dir.to_string_lossy().replace('\\', "/");
            self.output.push_str(&format!(
                "lua vim.api.nvim_create_autocmd('BufEnter', {{ once = true, buffer = vim.fn.bufadd([==[{name}]==]), callback = function() vim.cmd.bcd({{ [==[{abs_local_dir_str}]==], magic = {{ file = false, bar = false }} }}) end }})\n"
            ));
            self.did_lcd = true;
        }
    }

    fn write_frame(&mut self, frame: &Frame) {
        match frame {
            Frame::Leaf(_) => {}
            Frame::Column { children, .. } => {
                let count = children.len().saturating_sub(1);
                for _ in 0..count {
                    self.output.push_str("wincmd _ | wincmd |\nsplit\n");
                }
                if count > 0 {
                    self.output.push_str(&format!("{count}wincmd k\n"));
                }
                for (idx, child) in children.iter().enumerate() {
                    self.write_frame(child);
                    if idx + 1 < children.len() {
                        self.output.push_str("wincmd w\n");
                    }
                }
            }
            Frame::Row { children, .. } => {
                let count = children.len().saturating_sub(1);
                for _ in 0..count {
                    self.output.push_str("wincmd _ | wincmd |\nvsplit\n");
                }
                if count > 0 {
                    self.output.push_str(&format!("{count}wincmd h\n"));
                }
                for (idx, child) in children.iter().enumerate() {
                    self.write_frame(child);
                    if idx + 1 < children.len() {
                        self.output.push_str("wincmd w\n");
                    }
                }
            }
        }
    }

    fn write_window_resizes(&mut self, layout: &Layout, root: &Frame, win: WinHandle, index: usize) {
        if !self.options.winsize {
            return;
        }
        let Ok(geom) = layout.window_geometry(win) else {
            return;
        };
        let total_h = root.geometry().height;
        let total_w = root.geometry().width;
        if total_h > 0 {
            let half = total_h / 2;
            let h = geom.height;
            self.output.push_str(&format!(
                "exe '{index}resize ' . ((&lines * {h} + {half}) / {total_h})\n"
            ));
        }
        if total_w > 0 {
            let half = total_w / 2;
            let w = geom.width;
            self.output.push_str(&format!(
                "exe 'vertical {index}resize ' . ((&columns * {w} + {half}) / {total_w})\n"
            ));
        }
    }

    fn write_window_buffer_load(&mut self, layout: &Layout, win: WinHandle) {
        let Ok(win_state) = layout.window(win) else {
            return;
        };
        let buf = win_state.buffer;
        let buftype = self.buffer_buftype(buf);
        let is_terminal = buftype == "terminal" || self.editor.is_terminal_buffer(buf);
        let name = self
            .editor
            .buffer(buf)
            .map(|st| st.name().to_string_lossy().into_owned())
            .unwrap_or_default();

        if !name.is_empty()
            && (!matches!(buftype.as_str(), "nofile" | "quickfix" | "prompt") || is_terminal)
        {
            let edit_name = if self.did_lcd && !Path::new(&name).is_absolute() {
                Path::new(self.cwd)
                    .join(&name)
                    .to_string_lossy()
                    .into_owned()
            } else {
                name
            };
            let escaped = normalize_and_escape(&edit_name);
            self.output.push_str(&format!(
                "if bufexists(fnamemodify(\"{escaped}\", \":p\")) | buffer {escaped} | else | edit {escaped} | endif\n"
            ));
            if is_terminal {
                self.output
                    .push_str(&format!("if &buftype ==# 'terminal'\n  silent file {escaped}\nendif\n"));
            }
        } else {
            self.output.push_str("enew\n");
        }
    }

    fn write_window_leaf(&mut self, layout: &Layout, win: WinHandle) {
        let Ok(win_state) = layout.window(win) else {
            return;
        };
        if self.options.curdir && let Some(dir) = &win_state.local_directory {
            let abs_dir = if dir.is_absolute() {
                dir.clone()
            } else {
                Path::new(self.cwd).join(dir)
            };
            let escaped = normalize_and_escape(&abs_dir.to_string_lossy());
            self.output.push_str(&format!("lcd {escaped}\n"));
        }
    }

    fn write_window_cursor(&mut self, layout: &Layout, win: WinHandle) {
        let Ok(win_state) = layout.window(win) else {
            return;
        };
        let line = win_state.cursor.lnum.max(1);
        self.output
            .push_str(&format!("let s:l = {line}\nkeepjumps exe s:l\nnormal! 0\n"));
    }

    fn write_tabpage(&mut self, tabpage: &TabpageState, tab_index: usize) {
        if self.options.tabpages && tab_index > 0 {
            self.output.push_str("tabnew\n");
        }

        if self.options.curdir && let Some(dir) = &tabpage.local_directory {
            let abs_dir = if dir.is_absolute() {
                dir.clone()
            } else {
                Path::new(self.cwd).join(dir)
            };
            let escaped = normalize_and_escape(&abs_dir.to_string_lossy());
            self.output.push_str(&format!("tcd {escaped}\n"));
        }

        let layout = tabpage.layout();
        let root = layout.root();

        let first_editable_win = layout
            .windows()
            .iter()
            .copied()
            .find(|&win| self.is_window_editable(tabpage, win));

        if let Some(first_win) = first_editable_win
            && let Ok(first_state) = layout.window(first_win)
            && let Ok(buf_state) = self.editor.buffer(first_state.buffer)
        {
            let raw_name = buf_state.name().to_string_lossy();
            let edit_name = if self.did_lcd && !Path::new(&*raw_name).is_absolute() {
                Path::new(self.cwd)
                    .join(&*raw_name)
                    .to_string_lossy()
                    .into_owned()
            } else {
                raw_name.into_owned()
            };
            let escaped = normalize_and_escape(&edit_name);
            self.output.push_str(&format!("edit {escaped}\n"));
        }

        let windows: Vec<WinHandle> = layout
            .windows()
            .into_iter()
            .filter(|&win| self.ses_do_win(tabpage, win))
            .collect();

        let has_splits = windows.len() > 1 && !matches!(root, Frame::Leaf(_));
        if has_splits {
            self.output.push_str(
                "let s:save_splitbelow = &splitbelow\nlet s:save_splitright = &splitright\nset splitbelow splitright\n",
            );
            self.write_frame(root);
            self.output.push_str(
                "let &splitbelow = s:save_splitbelow\nlet &splitright = s:save_splitright\nwincmd t\n",
            );
            if self.options.winsize {
                self.output.push_str(
                    "let s:save_winminheight = &winminheight\nlet s:save_winminwidth = &winminwidth\nset winminheight=0\nset winheight=1\nset winminwidth=0\nset winwidth=1\n",
                );
            }
        }

        let curwin = tabpage.current_window();
        let mut curwin_nr = 1;

        for (idx, &win) in windows.iter().enumerate() {
            if win == curwin {
                curwin_nr = idx + 1;
            }
            if idx > 0 {
                self.output.push_str("wincmd w\n");
            }
            if Some(win) != first_editable_win {
                self.write_window_buffer_load(layout, win);
            }
            self.write_window_leaf(layout, win);
            self.write_window_resizes(layout, root, win, idx + 1);
            self.write_window_cursor(layout, win);
        }

        if has_splits && self.options.winsize {
            self.output.push_str(
                "let &winminheight = s:save_winminheight\nlet &winminwidth = s:save_winminwidth\n",
            );
        }

        if curwin_nr > 1 {
            self.output.push_str(&format!("{curwin_nr}wincmd w\n"));
        }
    }

    fn write_tabs(&mut self) {
        let tabs = if self.options.tabpages {
            self.editor.tabpages()
        } else {
            self.editor.current_tabpage().into_iter().collect()
        };
        let curtab = self.editor.current_tabpage();

        for (tab_index, &tab) in tabs.iter().enumerate() {
            if let Ok(tabpage) = self.editor.tabpage(tab) {
                self.write_tabpage(tabpage, tab_index);
            }
        }

        if self.options.tabpages && let Some(curtab) = curtab {
            if let Some(pos) = tabs.iter().position(|t| *t == curtab) {
                self.output.push_str(&format!("tabnext {}\n", pos + 1));
            }
        }
    }

    fn write_footer(&mut self) {
        self.output.push_str(
            "if exists('s:wipebuf') && len(win_findbuf(s:wipebuf)) == 0 && getbufvar(s:wipebuf, '&buftype') isnot# 'terminal'\n  silent exe 'bwipe ' . s:wipebuf\nendif\n",
        );
        self.output.push_str("unlet! s:wipebuf\n");
        if self.options.options {
            let cur_shm = self
                .editor
                .options()
                .get_global("shortmess")
                .ok()
                .and_then(|v| match v {
                    OptionValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            self.output.push_str(&format!("set shortmess={cur_shm}\n"));
        } else {
            self.output.push_str("let &shortmess = s:shortmess_save\n");
        }
        self.output.push_str(
            "let &g:so = s:so_save | let &g:siso = s:siso_save\nset hlsearch\ndoautoall SessionLoadPost\nunlet SessionLoad\n\" vim: set ft=vim :\n",
        );
    }

    fn build(mut self) -> String {
        self.write_header();
        self.write_buffers();
        self.write_tabs();
        self.write_footer();
        self.output
    }
}

/// Generates the body of a session file.
#[must_use]
pub fn generate_session(
    editor: &Editor,
    ssop: &SessionOptions,
    ssop_str: &str,
    cwd: &str,
) -> String {
    SessionWriter::new(editor, ssop, ssop_str, cwd).build()
}

fn resolve_view_target<E: ExEditorAccess>(access: &E) -> Result<String, ()> {
    access.with_ex_editor(|editor| {
        let buf = editor.current_buffer().ok_or(())?;
        let state = editor.buffer(buf).map_err(|_| ())?;
        if state.name().as_bytes().is_empty() {
            return Err(());
        }
        Ok(format!("{}.view", state.name().to_string_lossy()))
    })
}

fn resolve_target_file<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    command: &ExCommand,
) -> Result<String, Flow> {
    let raw_arg = command.args.trim();
    if !raw_arg.is_empty() {
        return Ok(raw_arg.to_owned());
    }
    match command.command.name() {
        "mksession" => Ok(SESSION_FILE.to_owned()),
        "mkvimrc" => Ok(VIMRC_FILE.to_owned()),
        "mkexrc" => Ok(EXRC_FILE.to_owned()),
        "mkview" => resolve_view_target(access)
            .map_err(|()| error_flow(runtime, "E32", "No file name")),
        _ => Ok(SESSION_FILE.to_owned()),
    }
}

fn generate_file_content<E: ExEditorAccess>(access: &E, cmd_name: &str) -> String {
    if cmd_name == "mksession" {
        return access.with_ex_editor(|editor| {
            let ssop_str = editor
                .options()
                .get_global("sessionoptions")
                .ok()
                .and_then(|v| match v {
                    OptionValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    "blank,buffers,curdir,folds,help,tabpages,winsize,terminal".to_owned()
                });
            let ssop = SessionOptions::from_str(&ssop_str);
            let raw_cwd = editor
                .global_directory()
                .map(Path::to_path_buf)
                .or_else(|| editor.effective_directory())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let cwd = resolve_absolute_cwd(&raw_cwd);
            let cwd_str = cwd.to_string_lossy().into_owned();
            generate_session(editor, &ssop, &ssop_str, &cwd_str)
        });
    }
    match cmd_name {
        "mkvimrc" => "\" ~/.nvimrc\nversion 6.0\n".to_owned(),
        "mkexrc" => "\" ~/.exrc\n".to_owned(),
        _ => "\" View file\n".to_owned(),
    }
}

fn update_this_session<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    path: &Path,
) {
    let cwd = access.with_ex_editor(|editor| {
        editor
            .effective_directory()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    });
    let full_path = if path.is_absolute() {
        runtime.scripts.io().canonicalize(path)
    } else {
        let joined = cwd.join(path);
        let canon = runtime.scripts.io().canonicalize(&joined);
        if canon.is_absolute() {
            canon
        } else {
            joined
        }
    };
    let full_path_str = full_path.to_string_lossy().into_owned();

    access.with_ex_editor(|editor| {
        editor.vvars_mut().insert(
            OxStr::from("this_session"),
            Object::String(OxStr::from(full_path_str.as_str())),
        );
    });
    scope.replace_pair(
        ScopeKind::Vim,
        "this_session",
        Typval::String(OxStr::from(full_path_str.as_str())),
    );
}

/// Executes `:mksession`, `:mkvimrc`, `:mkexrc`, or `:mkview`.
pub(crate) fn command_session<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<dyn LuaExec>>,
    command: &ExCommand,
) -> Flow {
    let cmd_name = command.command.name();
    let is_session = cmd_name == "mksession";

    let fname = match resolve_target_file(runtime, access, command) {
        Ok(fname) => fname,
        Err(flow) => return flow,
    };
    let path = PathBuf::from(&fname);

    if runtime
        .scripts
        .io()
        .metadata(&path, true)
        .is_ok_and(|m| m.kind == FileKind::Directory)
    {
        return error_flow(runtime, "E17", format!("\"{fname}\" is a directory"));
    }

    if !command.bang && runtime.scripts.io().exists(&path) {
        return error_flow(
            runtime,
            "E189",
            format!("\"{fname}\" exists (add ! to override)"),
        );
    }

    if is_session {
        let current_buf = access
            .with_ex_editor(|editor| editor.current_buffer().unwrap_or(BufHandle::CURRENT));
        let pre_flow = fire_buffer_lifecycle(
            runtime,
            access,
            scope,
            lua,
            &[Event::SessionWritePre],
            current_buf,
        );
        if !matches!(pre_flow, Flow::Normal) {
            return pre_flow;
        }
    }

    let content = generate_file_content(access, cmd_name);

    if let Err(err) = runtime.scripts.io().write_string(&path, &content) {
        return error_flow(
            runtime,
            "E190",
            format!("Cannot open \"{fname}\" for writing: {err}"),
        );
    }

    if is_session {
        update_this_session(runtime, access, scope, &path);
        let current_buf = access
            .with_ex_editor(|editor| editor.current_buffer().unwrap_or(BufHandle::CURRENT));
        let post_flow = fire_buffer_lifecycle(
            runtime,
            access,
            scope,
            lua,
            &[Event::SessionWritePost],
            current_buf,
        );
        if !matches!(post_flow, Flow::Normal) {
            return post_flow;
        }
    }

    Flow::Normal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Editor;
    use crate::excmd_exec::{ExExecutor, TestEditorAccess};
    use crate::layout::{Anchor, RelativeTo, WinConfig};

    #[test]
    fn test_class_a_shortmess_no_duplicate_flags() {
        let mut editor = Editor::new();
        let buf = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buf, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let cur_dir = std::env::temp_dir();
        let cur_dir_str = cur_dir.to_str().unwrap();
        let ssop = SessionOptions::default();
        let session = generate_session(&editor, &ssop, "curdir,buffers", cur_dir_str);

        assert!(session.contains("let s:shortmess_save = &shortmess\n"));
        assert!(session.contains("set shortmess+=a\n"));
        assert!(!session.contains("set shortmess+=aoO\n"));
        assert!(session.contains("let &shortmess = s:shortmess_save\n"));

        editor
            .options_mut()
            .set_global("shortmess", OptionValue::String("ltToOCFa".to_owned()))
            .unwrap();
        let session2 = generate_session(&editor, &ssop, "curdir,buffers", cur_dir_str);
        assert!(!session2.contains("set shortmess+="));

        let session_file = cur_dir.join(format!("oxvim_session_test_{}.vim", std::process::id()));
        std::fs::write(&session_file, &session2).unwrap();
        let access = TestEditorAccess::new(editor);
        let mut executor = ExExecutor::new();
        let res = executor.execute_line(&access, &format!("source {}", session_file.display()));
        let _ = std::fs::remove_file(&session_file);
        if let Err(crate::excmd_exec::ExecError::Vim(err)) = &res {
            assert_ne!(
                err.kind,
                crate::excmd_exec::VimExceptionKind::Error("E474".to_owned()),
                "sourcing session file raised E474: {err:?}"
            );
        }

        let mut editor2 = Editor::new();
        let buf2 = editor2.create_buffer(true).unwrap();
        editor2
            .create_tabpage(buf2, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        editor2
            .options_mut()
            .set_global("shortmess", OptionValue::String("ltTOCFa".to_owned()))
            .unwrap();
        let session3 = generate_session(&editor2, &ssop, "curdir,buffers", cur_dir_str);
        assert!(session3.contains("set shortmess+=o\n"));
        assert!(!session3.contains("set shortmess+=a"));
        assert!(!session3.contains("set shortmess+=O"));

        let session_file3 = cur_dir.join(format!("oxvim_session_test3_{}.vim", std::process::id()));
        std::fs::write(&session_file3, &session3).unwrap();
        let access2 = TestEditorAccess::new(editor2);
        let mut executor2 = ExExecutor::new();
        let res3 = executor2.execute_line(&access2, &format!("source {}", session_file3.display()));
        let _ = std::fs::remove_file(&session_file3);
        if let Err(crate::excmd_exec::ExecError::Vim(err)) = &res3 {
            assert_ne!(
                err.kind,
                crate::excmd_exec::VimExceptionKind::Error("E474".to_owned()),
                "sourcing session3 file raised E474: {err:?}"
            );
        }
        let duplicate_err = executor2.execute_line(&access2, "set shortmess+=a").unwrap_err();
        match duplicate_err {
            crate::excmd_exec::ExecError::Vim(err) => {
                assert_eq!(err.kind, crate::excmd_exec::VimExceptionKind::Error("E474".to_owned()));
            }
            other => panic!("expected E474, got {other:?}"),
        }
    }

    #[test]
    fn test_class_a_shortmess_options_flag() {
        let editor = Editor::new();
        let mut ssop = SessionOptions::default();
        ssop.options = true;
        let session = generate_session(&editor, &ssop, "options,curdir,buffers", "/test/dir");

        assert!(!session.contains("let s:shortmess_save = &shortmess\n"));
        assert!(!session.contains("let &shortmess = s:shortmess_save\n"));
        assert!(session.contains("set shortmess=ltToOCF\n"));
    }

    #[test]
    fn test_class_b_terminal_skipped_when_disabled() {
        let mut editor = Editor::new();
        let b1 = editor.create_buffer(true).unwrap();
        editor
            .buffer_mut(b1)
            .unwrap()
            .set_name(ox_types::OxStr::from("doc.txt"));
        let tab = editor
            .create_tabpage(b1, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();

        let channel = editor.allocate_channel_id();
        let b_term = editor
            .allocate_terminal_buffer_rows(
                channel,
                None,
                crate::terminal_screen::ScreenSize::new(24, 80),
            )
            .unwrap();
        editor
            .buffer_mut(b_term)
            .unwrap()
            .flags
            .set(crate::BufferFlags::LISTED, true);
        editor
            .options_mut()
            .set_buffer(b_term, "buftype", OptionValue::String("terminal".to_owned()))
            .unwrap();
        editor
            .buffer_mut(b_term)
            .unwrap()
            .set_name(ox_types::OxStr::from("term://bash"));

        let current_win = editor.tabpage(tab).unwrap().current_window();
        editor
            .set_window_buffer(current_win, b_term, crate::editor::BufferRelease::KeepLoaded)
            .unwrap();

        let mut ssop = SessionOptions::default();
        ssop.terminal = false;
        let session = generate_session(&editor, &ssop, "buffers,curdir", "/test/dir");
        assert!(session.contains("badd +0 doc.txt\n"));
        assert!(!session.contains("term://bash"));
        assert!(!session.contains("edit term://bash"));

        ssop.terminal = true;
        let session2 = generate_session(&editor, &ssop, "buffers,curdir,terminal", "/test/dir");
        assert!(session2.contains("badd +0 doc.txt\n"));
        assert!(session2.contains("badd +0 term://bash\n"));
    }

    #[test]
    fn test_class_b_floating_windows_skipped() {
        let mut editor = Editor::new();
        let b_main = editor.create_buffer(true).unwrap();
        editor
            .buffer_mut(b_main)
            .unwrap()
            .set_name(ox_types::OxStr::from("main.txt"));
        let b_float = editor.create_buffer(true).unwrap();
        editor
            .buffer_mut(b_float)
            .unwrap()
            .set_name(ox_types::OxStr::from("float.txt"));

        let tab = editor
            .create_tabpage(b_main, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();

        let config = WinConfig::new(RelativeTo::Editor, Anchor::NorthWest, 0.0, 0.0, 10, 5).unwrap();
        let _float_win = editor.open_float(tab, b_float, config).unwrap();

        let mut ssop = SessionOptions::default();
        ssop.buffers = false;
        let session = generate_session(&editor, &ssop, "curdir", "/test/dir");

        assert!(session.contains("badd +0 main.txt\n"));
        assert!(!session.contains("float.txt"));
        let float_buf_cmd = format!("buffer {}\n", i64::from(b_float));
        assert!(!session.contains(&float_buf_cmd));
    }
    #[test]
    fn test_class_c_absolute_escaped_cd_no_raw_tilde() {
        let editor = Editor::new();
        let ssop = SessionOptions::default();
        let session = generate_session(&editor, &ssop, "curdir", "/home/user/my path/b'uf d");

        assert!(session.contains("cd /home/user/my\\ path/b\\'uf\\ d\n"));
        assert!(!session.lines().any(|l| l.starts_with("cd ~")));
    }

    #[test]
    fn test_class_d_fname_escaping_and_buffer_local_cd() {
        assert_eq!(escape_fname("b'uf d"), "b\\'uf\\ d");
        assert_eq!(escape_fname("foo$bar#baz%qux.txt"), "foo\\$bar\\#baz\\%qux.txt");
        assert_eq!(escape_fname(">leading_gt.txt"), "\\>leading_gt.txt");
        assert_eq!(escape_fname("+leading_plus.txt"), "\\+leading_plus.txt");
        assert_eq!(escape_fname("-"), "\\-");

        let mut editor = Editor::new();
        let b = editor.create_buffer(true).unwrap();
        editor
            .buffer_mut(b)
            .unwrap()
            .set_name(ox_types::OxStr::from("test'file.txt"));
        editor
            .buffer_mut(b)
            .unwrap()
            .local_directory = Some(PathBuf::from("/home/user/b'uf d"));

        let ssop = SessionOptions::default();
        let session = generate_session(&editor, &ssop, "curdir,buffers", "/home/user");

        assert!(session.contains(
            "lua vim.api.nvim_create_autocmd('BufEnter', { once = true, buffer = vim.fn.bufadd([==[test'file.txt]==]), callback = function() vim.cmd.bcd({ [==[/home/user/b'uf d]==], magic = { file = false, bar = false } }) end })\n"
        ));
    }
}
