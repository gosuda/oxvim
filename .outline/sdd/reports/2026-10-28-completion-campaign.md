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
| 5 | `just oldtest` | produced no results at all | 3654 executed, 2365 failed, 686 skipped, 40 blocked, 5 timed out over 236 files |

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

1. Five oldtest files exceed the runner's five-minute cap before executing a test: `test_plugin_termdebug.vim`,
   `test_tabpage.vim`, `test_functions.vim`, `test_put.vim`, and `test_regexp_latin.vim` (crash). The `:edit`
   hang that used to head this list is fixed: `fnamemodify('foo', ':h')` answered an empty string instead of
   ".", so `vim.fs.parents` walked forever inside a buffer-lifecycle autocmd.
2. `:terminal` and the upstream oldtest Makefile path need terminal output parsed into buffer cells.
3. `call()` and the `uniq()` comparator are implemented, and a Partial's dictionary now binds `self`,
   which also fixed `function('Name', dict)()`. The Ex commands listed below remain unimplemented.
4. `:match`, `:compiler`, `:checktime`, `:doautocmd`, `:badd`, `recover`, `defer`, `win_execute`, and
   `getfperm` still report `not implemented`.
5. Window creation copies the source window's local options; the oracle gives the new window global defaults.
6. `:new`/`:split` aside, functional-suite failures need their own census: the harness aborts early on most
   files, and the group logs now carry per-test lines the counter can read.

## Measurement traps met in this run

- `$TEST_FILTER` is matched with `v:val =~ pattern`, so alternation is Vim's `\|`, not `|`.
  A probe written `^Test_(a|b)$` selects nothing and reports success; every filter in this
  campaign that produced a verdict used one anchored name or a character class.
- A census binary copied outside `target/release` cannot find the runtime tree and reports
  every file as blocked with zero tests. `tests/oldtest/run.sh` uses the in-tree binary.
- `timeout -s TERM` does not stop a hung oxvim cleanly in the shared PTY; harness probes that
  need a hard cap use `start_new_session` and a kill.

## Slices cut for the next wave, 2026-10-28

Six read-only scouts grouped the remaining failures from the logs. Measured causes, with owners:

| Suite | Cause | Tests | Owner |
|---|---|---|---|
| functional | assertion mismatch, no error code | 612 | diffuse; options, keymap, buffer, highlight |
| functional | harness module `test.functional.testnvim.exec_lua` not found | 418 | harness search path, not the editor |
| functional | `expect_msg_seq` got no messages | 187 | `ox-loop` notification path |
| functional | `not implemented: sockconnect` | 96 | channel builtins in `builtins/process.rs` |
| functional | server EOF or RPC failure at startup | 88 | `crates/oxvim/src/main.rs` |
| functional | `not implemented: rpcnotify` / `rpcrequest` / `chanclose` / `jobclose` | 74 + 52 + 38 + 31 | same channel owner |
| functional | `msgpackparse` / `msgpackdump` | 41 | `crates/ox-eval` typval dispatch |
| functional | `Invalid method: nvim__stats` | 29 | `ox-api` registry |
| oldtest | `E492: Not an editor command` | 380-450 | `excmd_exec.rs` dispatch table |
| oldtest | `E117: not implemented: <func>` | 280-340 | builtin families |
| oldtest | value mismatch, no error code | 700-900 | diffuse |

Two more findings matter for the harness. Eight of the sixteen functional groups ended in `Process
terminated due to timeout`, so the 4076-failure figure is a floor and the gate needs a longer or
per-file run before its counts can be trusted.

The terminal surface has the parts but not the wiring. `terminal_screen.rs` holds a VT emulator with
scrollback, `job.rs` pumps pty output, and `editor.rs` projects rows into the buffer, yet a probe of
`:terminal echo hello-from-terminal` followed by `getline(1, '$')` returns blank lines, so the child's
output never reaches the buffer in headless mode. The next terminal slice is to trace that path from
the pty read to the row projection, not to write a new emulator.

## Parity fixes landed after the slice cut, 2026-10-28

| Commit | Surface | What changed | Proof |
|---|---|---|---|
| 8746163 | `package.path` | Relative loader entries are kept, as the oracle keeps them; `cpath` is left alone | `require('test.functional.testnvim.exec_lua')` answers true in a harness-shaped tree; the 418 module-not-found failures disappear from the api and core group logs |
| 3fa6889 | `:terminal` | Buffer gets `buftype=terminal` at attach; `:sleep` pumps the job reactor | `:terminal echo hello-from-terminal` + `:sleep 1` leaves the text and `[Process exited 0]`; with the pump reverted the new test fails on a blank buffer |
| 5e8dbcc | `win_findbuf`, `getcmdwintype` | Both names answered E117 before | Same script on both binaries: `t=` and `ids=[1001]` |
| 56f083b | normal-mode put | Count overflow answers E1240 before allocating | `test_put.vim` moves from a 300 s timeout, 0 executed, to 34 executed, 25 failed; both `assert_fails` cases pass |
| 7c0a007 | `vim.v.exiting` | Reads `vim.NIL` until shutdown, as upstream does | `vim.wait(200, fn, 10)` runs 20 iterations, matching the oracle; it ran 3 before |
| 8ebdd32 | Lua wait path | `loop_poll` flushes terminal jobs through the free executor slot | `crates/oxvim/tests/wait_pty_pump.rs`, 1 test, passes |

`just apidiff` reports `schemas match`, and `oxvim --api-info` is
byte-identical to the oracle's document (32701 bytes each).

## Gaps the parallel wave opened, with owners

- `vim.wait` cannot pump terminal output while both executor slots are busy,
  which is the case for a whole `-l` script; that needs the job reactor to be
  reachable without the executor borrow.
- A `luafile`-hosted wait wakes about 4 times in 2 s where the oracle wakes
  every interval; the `-l` path is correct, so the remaining difference is in
  how the chunk is hosted.
- `:syntax` has no engine: 15 oldtest files and 8 functional files need it,
  plus `synID`/`synIDattr`/`synIDtrans`/`synstack`/`synconcealed`. Scope
  assessment recorded; not attempted.
- Confirmed absent from Neovim and correctly kept absent in oxvim after the
  channel pass: `ch_status`, `chaninfo`, `term_start`, `term_getline`,
  `term_getscrolled`. `termopen` exists upstream and is now implemented.
- Builtin inventory against upstream `eval.lua`: 462 upstream names, 312
  routed, 150 missing, 1 extra (`defer`). The table is in
  `local://compat-builtin-gaps.md`.

## Evidence

All logs live under `.outline/evidence/` (gitignored): `gates/`, `oldtest-final/`, `functional-*.log`,
`nextest-final.log`, `final-apidiff.log`. Census and runner tools are in `.outline/evidence/tools/`:
`gates.sh`, `oldtest-census.sh`, `functional-census.sh`.
