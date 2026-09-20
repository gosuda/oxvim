# Windows startup and runtime boundaries

This layer follows PR #35. It repairs concrete Windows-specific boundaries;
it does **not** claim that the full Windows editor builds or runs yet.

## Changes

- Preserve native drive, UNC, root-relative, and verbatim prefixes when
  expanding filesystem globs. The old path reconstruction started at `/` and
  treated a drive prefix as an ordinary component. Public `glob()` and
  `packadd` regressions use real files, absolute roots, spaces, and quotes.
- Recognize CRLF script lines on Windows without changing Unix source
  semantics. The first LF-only separator ends CRLF conversion; a final
  unterminated CR remains data. The shared logical-line parser also serves
  startup configuration, rather than adding a second init-file parser.
- Do not pass a nonexistent Windows `LC_MESSAGES` constant to `setlocale`.
  Message catalogs use the environment and leave the character locale
  unchanged, following Neovim's `get_mess_env` and `ex_language` paths.
- Query Windows kernel version, architecture, product description, and
  physical memory through native APIs. All unsafe calls remain inside
  `ox-sys`; `ox-uv` retains `forbid(unsafe_code)`. System-query errors cross
  the Rust boundary explicitly and preserve the Lua error-return convention.
  Memory queries retain libuv's documented zero-on-query-failure convention.
- Repair the Windows `Cow<str>` to `OsStr` path conversion and remove the
  Unix-only conversion from environment-variable NUL tests.

The system information implementation follows libuv's Windows `util.c`,
including its optional registry product description and Windows 11 product
name correction. It does not substitute guessed version numbers or treat
missing process-pipe support as a successful no-op.

## Validation

On Linux, all five `platform_startup` regressions passed, including real
`packadd` discovery. The real-binary CRLF init-file test also passed and
confirmed that the existing Unix E488 behavior was preserved.

The Windows system-boundary library and its tests passed `cargo check` for
`x86_64-pc-windows-msvc`; this checks code without linking or running it.
Windows-targeted Clippy for all `ox-sys` targets completed without warnings.
Native execution of those tests is added to
the Windows CI job, alongside the existing terminal-library tests and a
runtime-library compile check. Linux CI runs the new startup regressions.

```sh
unset CARGO_BUILD_BUILD_DIR RUSTC_WRAPPER
export CARGO_TARGET_DIR="$PWD/target" CARGO_BUILD_JOBS=8
cargo test --locked --release -p ox-editor --test platform_startup
cargo test --locked --release -p oxvim --test cli crlf_init_file_obeys_native_source_rules
cargo clippy --locked --target x86_64-pc-windows-msvc -p ox-sys --all-targets
```

## Review follow-up

The shared source executor now reads the current global `fileformats` value
before joining a script. On Windows, an empty value starts in DOS mode;
any nonempty value enables first-newline detection. An LF-only separator
in DOS mode emits W15 once and switches the remaining input to Unix mode.
The option is sampled once per source invocation, and Unix behavior is
unchanged. Portable regressions exercise both policies, including a
first-line comment, exact message history and `v:errmsg`, pure CRLF input,
and an unterminated final line.

The proposed removal of the Windows message-locale fallback was rejected.
Neovim v0.12.5 `src/nvim/os/lang.c:get_mess_env` explicitly queries
`LC_CTYPE` when `LANG` is absent or numeric. An isolated subprocess matrix
guards that behavior and environment-variable precedence without changing
the test runner's process-global environment. The same test checks the
public message-locale query on native Windows, including the WOW64 CI job.

## Remaining Windows blockers

The editor's job layer unconditionally imports the Unix-only
`ox_uv::process::ProcessPipe`. A correct Windows pipe adapter still needs
ownership, cancellation, callback-order, and native process tests. This layer
does not remove that functionality to make a compiler gate appear green.

The full workspace MSVC cross-build also reaches LuaJIT's native `cl`
discovery requirement in `luajit-src`. The installed xwin SDK is sufficient
for checking the system and runtime boundaries, not that native build step.

The CRLF and rooted-path tests have run on Linux, not in a native Windows
editor. The mixed-separator and empty-`fileformats` regressions exercise
the Windows reader policy on Linux; they do not establish native editor
coverage. Full Windows editor and ConPTY E2E coverage remain open, as does
issue #28.
