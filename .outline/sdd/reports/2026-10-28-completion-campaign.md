# Completion and stabilization campaign, 2026-10-28

Work landed on `main` after `c4520b0`. The .agent-tasks goal table is the contract:
build, workspace tests, api-info diff, functional suite, oldtest suite.

## Gate results

| # | Criterion | Before | After |
|---|---|---|---|
| 1 | `cargo build --workspace --release` | pass | pass |
| 2 | `cargo nextest run --workspace` | 3563/3564 | 3645/3645, one skipped |
| 3 | `just apidiff` | fail, one function | pass, zero semantic differences |
| 4 | `just functional` | never measured | 1830 pass, 4076 fail, 58 skip, 1 error over 484 files |
| 5 | `just oldtest` | produced no results at all | 3591 executed, 2316 failed, 685 skipped, 40 blocked, 6 timed out over 236 files |

Criteria 1 to 3 are green. Criteria 4 and 5 are measured and still red.

## What was wrong, and what fixed it

1. **`nvim_mcursor` was invented.** Commit fc59163 added the API function and edited the canonical metadata
   blob so `nvim_get_api_info` advertised it; upstream at the reference commit has no such function. The blob
   was restored from the oracle's own `--api-info` output (32701 bytes), the inventory regenerated, and the
   handler, registration, tests, and dead editor methods removed.
2. **`:scriptnames` was missing.** Upstream `runtime/plugin/rplugin.vim` calls it on every start, so each
   session printed four error lines: `E117: Unknown function: scriptnames` twice and a bare
   `not implemented: scriptnames`. It is implemented now, and an unimplemented Ex command reports
   `E492: Not an editor command` instead of a function error.
3. **The PTY differential test hedged on encoding.** `edits_quits_and_restores_terminal_palette` waited for
   `editx` in the printable byte stream, which only appears when a client skips blank cells; this client paints
   them. It now parses the stream with `vt100` and asserts the rendered screen, and honours `OXVIM_BIN` so a
   build in a private `CARGO_TARGET_DIR` is the binary under test.
4. **The oldtest gate could not run.** The upstream Makefile starts every file through
   `jobstart(..., {'term': v:true})` and reads terminal-buffer lines back, and oxvim paints no terminal buffer,
   so the gate reported an empty screen and no results. `tests/oldtest/run.sh` drives `runtest.vim` directly
   with one scratch tree per file, and a full sweep takes about six minutes.

## Defect clusters closed by the fix waves

| Cluster | Effect |
|---|---|
| Ex modifier loop, `:put =`, `:tabdo`, `:vimgrep`, `:wNext`, function reload | `:unsilent :norm! n` parses; six `:put =` shapes match the oracle; `test_normal` E488 4 to 0 |
| Substitute text lock | a recursive `\=` replacement raised E565 instead of hanging forever |
| Script-local scope | `s:has_ffi` undefined 41 sites to 0; `g:Name()` calls 3 to 0 |
| screen queries | E5070 from `nr2char(screenchar(...))` 15 sites to 0 |
| Command-line builtins | `getcmdline getcmdpos setcmdpos getcmdtype getcmdprompt pumvisible wildmenumode`, plus the deferred `new_cmdpos` slot |
| Filetype | `:setfiletype` and the `did_filetype` transitions; the destructed-callback class at zero |
| Registers and key notation | E523 invalid-special-key class gone; `test_registers` 32 to 29 failed |
| Abbreviations | `:abbreviate` family on the mapping engine; `test_mapping` 46 to 36 failed |
| Fold and diff | model fixes; 24 of 24 diff tests pass |
| Highlight | startup table built from the oracle set: 388 visible, 11 hidden |
| Regex | a lone `[` is literal; E944, E769, and E54 arrive through `CompileError::code()` |

## Defects found and not yet fixed

1. A Lua to Vimscript builtin call costs about 0.5 ms against a few microseconds upstream. Filetype detection
   over one file name makes about 13 500 `vim.fn.fnamemodify` calls, so `:edit` on an unknown name takes tens
   of seconds. This is why `test_substitute.vim` times out: its child process runs `:next 0`.
2. `:terminal` and the upstream oldtest Makefile path need terminal output parsed into buffer cells.
3. `call()` is unimplemented although `exists('*call')` answers 1, and `uniq()` ignores its comparator.
4. `:match`, `:compiler`, `:checktime`, `:doautocmd`, `:badd`, `recover`, `defer`, `win_execute`, and
   `getfperm` still report `not implemented`.
5. Window creation copies the source window's local options; the oracle gives the new window global defaults.
6. `:new`/`:split` aside, functional-suite failures need their own census: the harness aborts early on most
   files, and the group logs now carry per-test lines the counter can read.

## Evidence

All logs live under `.outline/evidence/` (gitignored): `gates/`, `oldtest-final/`, `functional-*.log`,
`nextest-final.log`, `final-apidiff.log`. Census and runner tools are in `.outline/evidence/tools/`:
`gates.sh`, `oldtest-census.sh`, `functional-census.sh`.
