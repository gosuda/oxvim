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

## Integration result

The parallel wave landed in coherent commits: the five history tables
(`a76714b`), the put path plus the missing Ex command cluster (`aaa779d`), the
channel and terminal functions (`89f062b`), `vim._core.keyparse` with the Tab
termcode fix (`8ee39b8`), the abandon gate and bang modifiers (`eeae23f`), and
`cpfile`/`lpfile` with the forced-abandon unload (`68f8481`), then timers and
the seven missing queries (`6a0b5e2`), the listdo family with `:defer`,
`++once`/`++nested` and mid-sleep delivery (`c58b2ac`), closures running
against every live editor map (`f6c8a2e`), the census builtins (`cb4a034`),
and funcref delivery with `:checkhealth` and directory views (`6f62361`).
`cargo nextest run --workspace` was green at every landing; the last measured
count is 3769 passed, 1 skipped.

Oldtest census, five files, before and after:

| File | Before | After |
|---|---|---|
| test_put.vim | 25 failed | 7 failed |
| test_cdo.vim | 3 failed | 1 failed |
| test_winfixbuf.vim | 86 failed | 79 failed |
| test_user_func.vim | 23 failed | 19 failed |

## Full functional gate

`just functional` was run end to end and failed: the recipe exits 123 and group
logs end in `Process terminated due to timeout`. No group reported a clean
pass. The lua group was then re-measured alone on the binary that carries every
commit up to `42beb24`: 1460 tests ran in 876 s, 590 marked FAIL and 870
pass-or-skip. The earlier census counted 611 failures for the same group with a
different parser, so treat the delta as approximate rather than exact.

The raw log is `.outline/evidence/functional-lua-post.log`, and it carries ANSI
colour escapes, so a `^RUN` match sees nothing until they are stripped:

```sh
sed -E 's/\x1b\[[0-9;]*m//g' .outline/evidence/functional-lua-post.log \
  | tr -d '\r' | awk '/^RUN /{ if ($0 ~ /FAIL[[:space:]]*$/) f++; else p++ } END { print f, p }'
```

This group result is a broad failure census — `luaeval` API errors, async
`_normalize_error`, syntax gaps and a timeout — and says nothing about
`vim._core.keyparse`, whose verification is the 19 oracle-identical chord
probes in `8ee39b8`. `oxvim --api-info` is byte-identical to the oracle and `just apidiff`
reports schemas match, so the remaining work is behavioural.

## Defects the waves found and fixed

- A Vimscript lambda body evaluated in its creation snapshot, so `let g:x`
  inside it vanished and `g:`/`b:`/`w:`/`t:`/`v:`/`s:` reads were stale.
- A lambda timer or job callback redispatched by name where `<lambda>N` is not
  a user function, so it fired nothing and said nothing.
- The Lua callback shape (`vim.fn.jobstart` produces an empty name plus a
  registry slot) that a prefix-only discriminator would have misrouted.
- A stopped timer removed synchronously, vanishing from `timer_info` before its
  own callback ran (`42beb24`): it now stays visible until the delivery ends,
  and `timer_stopall` from inside a callback follows the same rule (`79d9213`).
- `<Tab>`/`<NL>` encoded as `K_SPECIAL` keys instead of their control bytes.
- A control byte naming itself `C-X`, which rendered `<C-S-f>` as `<S-C-F>`.

## Runtime plugin loading, 2026-10-29

`:runtime` and `:packadd` sourced every plugin file as Vimscript, so
`runtime/pack/dist/opt/nvim.difftool/plugin/difftool.lua` was parsed as Ex
commands and failed on line 1 with `E488: Trailing characters: ~= nil then` —
six functional plugin specs. Both call sites now split on the extension like
upstream `source_callback` (`runtime.c:975-990`), and `v:vim_did_enter` is
seeded so the plugin's own `vim.v.vim_did_enter > 0` check compares a number.

Re-running `test/functional/plugin/difftool_spec.lua`: the E488 failures are
gone (0 occurrences), and the six tests now fail deeper, at
`vim/_core/system.lua:240: attempt to call field 'pipe' (a nil value)`.

`uv.pipe` and `pipe:open` now exist (`b58ba06`): the pair form answers
`{read = fd, write = fd}`, the no-argument form answers a pipe handle, and
`open` adopts a descriptor as a `ProcessPipe` with direction taken from its
access mode. The same commit makes `v:vim_did_enter` a transition — 0 through
startup, 1 from `VimEnter` — so a `-S` script sees 0 like the oracle while a
run-time `:packadd` sees 1.

The difftool spec then stops at
`stdio entries must be pipe handles, false, nil, or descriptors 0, 1, or 2`,
raised in the `spawn` field and reached from
`runtime/pack/dist/opt/nvim.difftool/lua/difftool.lua:121`: `uv.spawn` still
refuses an arbitrary descriptor number, which is what `system.lua` passes for
the child's stdout, so all seven difftool tests still fail (six of them on that
line). Adding the variant is not local: `StdioConfig`
is `Copy` and lives in `[StdioConfig; 3]`, and `SpawnOptions` derives `Clone`
and is destructured in two spawn paths, so an `OwnedFd` field breaks the
ownership model. A partial attempt was reverted rather than left half-wired.
The design is written up in `local://uv-spawn-stdio-brief.md`: a descriptor
registry in ox-lua (`uv.pipe` inserts both halves, `pipe:open` and `uv.spawn`
take them out, `uv.fs_close` retires them) feeding one new ox-uv entry point,
`spawn_with_owned_stdio`, that owns descriptors without touching the copyable
`StdioConfig` or the `Clone` spawn options.

Update after `c9ebeca` and `fb3e2c4`: the registry exists and the stdio
rejection is gone. `TEST_FILE=test/functional/plugin/difftool_spec.lua` now runs
all seven tests in about 270 ms each instead of hanging for 240 s, and they fail
later:

- T1 to T6: `difftool.lua:386` calls `cfirst`, which raises `E553: No more
  items`, so the quickfix list is empty where upstream has entries.
- T7: `oxvim: Option not supported: "-d": requires a diff engine`, so the
  diff-mode command line is not implemented.

Evidence: `.outline/evidence/difftool-after-pipes.log`.

Diagnosis of the remaining difftool failure, measured outside the test harness:

- `vim.system({ 'diff', '-qrN', left, right }, { text = true, env = { LC_ALL = 'C' } }):wait()`
  answers the right code and stdout, both with absolute and with relative
  directory arguments, from a `-l` script.
- `vim.fn.executable('diff')` answers 1, so the plugin takes the `diffr` path.
- `vim.split`, the `Files 'a' and 'b' differ` pattern, `vim.fs.relpath`,
  `vim.fs.abspath` and `setqflist` with `quickfixtextfunc` all answer what the
  plugin expects, and a hand-built list of four entries survives `setqflist`.
- Resolved by `fix(quickfix): jump through a list whose entries are all
  invalid` (`f0797e8`): the list was never empty. `set_items` held the entries,
  but the port had no `qf_nonevalid` flag, so `:cfirst` searched for a valid
  entry, found none (difftool items carry a filename but no line or pattern) and
  answered `E553`. Upstream marks such a list and then ignores validity. With
  that flag, `difftool_spec` answers T1, T3, T4 and T6 as passing, and T2, T5,
  T7 fail on their own causes: split layout, autocmd cleanup, and `vim -d`.
  Evidence: `.outline/evidence/difftool-nonevalid.log`.

Update: the whole spec is green. `fix(layout): split the outermost frame for
botright windows` (`47625dd`) gives `:copen` the `WSP_BOT` split it needs, so
the list window spans the tabpage width and `getwininfo()` answers the columns
before the bottom row; T2 passes. `feat(startup): support -d and report
v:argf` (`3ca8241`) implements `-d`, seeds `v:argf`, and loads a directory
argument as the listing view `:edit <dir>` shows; T7 passes.

`test/functional/plugin/difftool_spec.lua`: seven of seven pass, in about
280 ms each. Evidence: `.outline/evidence/difftool-dashd.log`.
- Superseded hypothesis, not measured: the quickfix list is empty only when the plugin
  runs from an Ex command. No probe yet records `vim.system` output or the
  quickfix count inside a real Ex host, so executor starvation during
  `:wait()` remains a candidate, in the same class as the `vim.wait`
  terminal-pump gap, rather than a confirmed cause.
- `fix(uv): keep pipe descriptor reuse safe` (`f8da21d`) removes a stale-marker
  hazard: registering a pair or taking a half now drops an awaiting-close
  marker for the same number, and `close` clears both states. Two tests pin it,
  `stale_marker_never_reports_a_second_close` and
  `taking_a_number_clears_its_marker`, and both fail when the previous `close`
  and `take` bodies are restored, so the tests bite on the defect rather than
  passing by construction.
- `fix(uv): mark spawned descriptors only after the spawn starts` (`17a6c24`)
  keeps a rejected `stdio` table from stranding a marker for a descriptor that
  was released.

Two defects surfaced while wiring the registry. `pipe:open` adopted the
descriptor without installing the reactor callback, so reads never reached Lua
and the child's output vanished. The readiness path then held the callback
state across the Lua call, so a callback that stopped its own pipe panicked with
`RefCell already borrowed`; the delivery path clones the callback and drops the
borrow first. The second has a regression test,
`pipe_read_callback_can_stop_its_own_pipe`.

That is the next gap for this spec
and for anything else spawning with pipes through `vim.system()`.

Plugin group re-measured on the current binary: the run did not finish inside
900 s (exit 124), and before it was killed 103 tests failed and 27 passed or
skipped. The earlier 117-failure figure came from a different parser on a
stale log, so neither number is a group total; treat this as a partial census.
The E488 parse cluster is gone (0 occurrences) and the failures now sit at
`uv.spawn` stdio, `E484` on directory edits outside the edit path, and
provider/host messages.

## Still open

- `just functional` fails (exit 123); no final full `just oldtest` run is
  green. A 236-file baseline measured earlier stands at 3654 executed, 2365
  failed, 686 skipped, 40 blocked and 5 timed out, and the five-file table above
  is a post-wave census, not that gate.
- Dict-watcher fire hooks are not attached to the lvalue and mutation sites,
  so a watcher registers and validates but does not observe.
- `:checkhealth` cannot run a real provider re-entrantly and uses a native
  fallback; `:menu`, `:syntax` and `:mkspell` remain absent.
- `vim.wait` cannot pump terminal jobs while both executor slots are busy.
- `cargo fmt --all --check` reports 199 diffs. A workspace-wide rustfmt sweep
  was prepared and then dropped rather than published: its diff could not be
  proven formatting-only, and the gate sweep belongs in its own reviewed
  change.

## Evidence

All logs live under `.outline/evidence/` (gitignored): `gates/`, `oldtest-final/`, `functional-*.log`,
`nextest-final.log`, `final-apidiff.log`. Census and runner tools are in `.outline/evidence/tools/`:
`gates.sh`, `oldtest-census.sh`, `functional-census.sh`.


## Functional lua group re-measure (2026-10-29)

Direct per-group run of `test/functional/lua` (the recipe's `TEST_PARALLEL_GROUP`
is only read by cmake, not by the justfile guard):

- Result: `Process terminated due to timeout` at T840,
  `vim.system (sync) handle input`. Partial counts before the stop: 412 FAIL,
  485 pass. The group has no summary until that test stops hanging.
  Evidence: `.outline/evidence/functional-lua-post2.log`.
- Cause, found by probe: `vim.system({'cat'}, { stdin = 'hellocat' })` hung.
  Three defects: `pipe_pair` created both ends without `O_CLOEXEC`, so the child
  inherited a writer to its own stdin; a zero-length write never reported
  completion, so the flush before shutdown stalled; and a pipe shutdown never
  answered its callback, so the handle that ends the stream never closed.
  Fixed in `b425af0`, with `pipe_pair_ends_are_close_on_exec` pinning the
  descriptor flag. `/bin/cat` now exits, and the child's output arrives.
- `test/functional/lua/system_spec.lua` after the fixes: 13 of 16 pass, and the
  file runs to completion instead of timing out. `44ba18e` gave `kill` signal
  names, made a closed process's kill quiet, and matched upstream's
  `cmd: expected non-empty list of strings` and `ENOENT ... (cmd|cwd)` text.
  Remaining: `supports timeout` expects `wait(100)` to return the finished
  result after `is_closing()` turns true, but the port reports `is_closing()`
  at child exit, before the child's stdout pipe drains, so the short wait kills
  a process that was still writing. Evidence:
  `.outline/evidence/system-spec-post5.log`.


### Re-measure after the stdin fix

`TEST_FILE=test/functional/lua timeout 1500 just functional` (the recipe only
honours the group switch through `TEST_FILE`), run to completion:

| Result | Count |
|---|---|
| Tests run | 1460 |
| PASS | 852 |
| SKIP | 21 |
| FAIL | 574 |

Before the fix the same group reported 590 FAIL and 870 pass-or-skip, so
sixteen tests moved to passing and the run no longer stops at a timeout. The
largest remaining clusters are `lua:` (99), `async` (73), plain `lua` (67),
`vim.snippet` (28), `luaeval()` (17), `vim.fs` (11), `vim.diagnostic` (11) and
`vim._watch` (11). Evidence:
`.outline/evidence/functional-lua-post3.log`.


### Control: `vim.system (sync) supports timeout` fails on the reference binary

The one remaining failure in `test/functional/lua/system_spec.lua` is not a
port regression. Running the same test through the same harness, with
`NVIM_PRG` pointing at `.references/neovim/build/bin/nvim`:

```
RUN T1 vim.system (sync) supports timeout  FAIL
Expected: code = 0
Actual:   code = 124
RUN T2 vim.system (async) supports timeout OK
```

The `-l` probe agrees between the two binaries as well: both answer
`code = 124, stdout = "late"` for
`vim.system({'sh','-c','(sleep 0.3; printf late) & exit 0'})` when `wait(100)`
times out, so the port matches the reference here rather than diverging from
it. The sync expectation depends on the harness environment, not on the port.


### Correction: system_spec counts, and two remaining timeout failures

The earlier `13 of 16` was wrong. `system_spec.lua` has 17 tests. Exact
outcomes after `ec0b16f` (`test/functional/lua/system_spec.lua`,
`.outline/evidence/system-spec-post6.log`):

| Outcome | Count |
|---|---|
| PASS | 13 |
| SKIP | 1 (`always captures all content of stdout/stderr #30846`) |
| FAIL | 2 |

The two failures, with reference-binary controls through the same harness:

| Test | oxvim | reference `nvim` | Reading |
|---|---|---|---|
| `vim.system (sync) supports timeout` | FAIL, `code 124` where the test wants `0` | FAIL, same mismatch | harness expectation, not a port defect |
| `vim.system (async) supports timeout` | FAIL after 10.2 s | PASS in 1.15 s | open port defect |

`ec0b16f` fixed a third one: `SystemObj:wait() does not process non-fast
events #27292` failed at 1261 ms before it and passes after, and the reference
passes it too. `vim._core.loop_poll` was draining the `vim.schedule` queue even
when `vim.wait` asked for fast events only.

For the async timeout the cause is visible in `-l`: with
`vim.system({'sleep','10'}, {timeout=1000})`, `obj:wait()` returns at 1009 ms
with `code 124, signal 9`, while the expectation is `signal 15`. The object's
own timeout timer (`timer_oneshot`, `system.lua:352-360`) should fire first and
kill with SIGTERM; instead `wait` reaches its own deadline and kills with
SIGKILL. An extra zero-length poll after the deadline did not change that
(still `signal 9`), so it was reverted rather than left in the tree.


### Async timeout: timer precision at the deadline

The remaining difference is sub-millisecond timer behaviour, measured on both
binaries with a timer due exactly at the wait deadline:

```lua
local t = vim.uv.new_timer()
t:start(500, 0, function() fired = true end)
local ok = vim.wait(500, function() return fired end, nil, true)
```

| Binary | Timer fired | `vim.wait` |
|---|---|---|
| reference `nvim` | 499.95 ms | true |
| oxvim | 500.33 ms | false |

The reference fires fractionally early, so `remaining` is still positive when
its poll returns and the predicate is checked again. The port fires about
0.3 ms late, so `vim.wait` reaches its post-poll deadline check first and
answers false even though the event has arrived. The same shift explains
`vim.system({'sleep','10'}, {timeout=1000})`: the object's own timeout timer
loses the race to `wait`'s deadline, and the kill therefore comes from
`wait`'s SIGKILL path (`signal 9`) instead of the object timer's SIGTERM
(`signal 15`).

Two attempts were measured and reverted rather than left in the tree: an extra
zero-length poll after the deadline, and a two-millisecond margin on the wake
timer. Neither changed the outcome, and both touched the hot wait path without
proof. The fix belongs in timer expiry precision, not in `vim.wait`.


### keyparse coverage

`vim._core.keyparse` (the Lua table `vim.keycode(keys, true)` builds on) now has
its own test, `keyparse_reports_modifiers_and_both_spellings`
(`crates/ox-lua/tests/uv_core.rs`), pinning the shape for a lowercase key, an
uppercase letter (which the oracle reports as `S`), and a control key. The
Rust-side oracle comparison is `keyparse_reports_the_oracle_chords`
(`crates/ox-eval/src/builtins_tests.rs`) with seven notations. There is no
upstream functional spec for `keyparse`; the reference test tree has none, so
the Lua group cannot carry this coverage.


### Dict watchers: why firing is not a two-line hook

The registry is complete and already models upstream's in-flight rules
(`crates/ox-editor/src/userfunc.rs`: `busy`, `needs_free`, `WatchRound`,
`dictwatcher_begin`/`is_live`/`arguments`/`finish`). What is missing is the
notification calls. Address every upstream site, not just the obvious one:

| Upstream site | Mutation |
|---|---|
| `eval/typval.c:2831`, `:2838`, `:2864` | `extend()` adds, replaces, and overwrites keys |
| `eval/typval.c:3447` | `remove()` deletes a key |
| `eval/vars.c:1694` | `unlet` and scope-dict removal |
| `buffer.c:4304` | a `b:` variable write |
| `lua/stdlib.c:395`, `:438` | the Lua table path that deletes and updates a key |

`tv_dict_watcher_notify` (`eval/typval.c:2054`) passes three arguments: the
dictionary, the key, and a dictionary that carries `new` and/or `old`. A busy
watcher is skipped, so a callback that mutates the same dictionary cannot
re-enter that watcher, but other watchers in the same round still run.

The port cannot call the hook from the mutation leaves as they stand. Firing a
Vimscript callback needs `&mut ExRuntime` plus `access`, `scope`, and `lua`
(`call_user_function_with_self`, `crates/ox-editor/src/excmd_exec.rs:5054`),
while the delete path is threaded as `&ExRuntime` from `remove_subscript_chain`
down to `dict_remove_key` (`crates/ox-editor/src/lvalue.rs:815-850`, `:911`),
and the set path (`dict_set_key`, `:880`) has `&mut` but none of the call
context.

Two designs follow, and the faithful one costs more than it looks:

1. Record events into a queue owned by `ExRuntime` and drain them after the Ex
   command finishes. Small, keeps the borrow rule, but batches. One command
   that writes two keys then fires both callbacks, where upstream fires each
   callback at its own mutation and the callback can observe the intermediate
   dictionary.
2. Thread the call context into the lvalue leaves and fire per mutation. This
   matches upstream ordering, and it changes four signatures plus their
   callers, in one coherent change with tests.

Design 2 is the one to build; design 1 is cheaper and observably different, so
it is not a shortcut worth taking. The Lua-side `lua/stdlib.c` sites also need
the Lua-to-dict conversion path, which is separate work again.


### Plugin group: what the measurement now says

Three specs were run alone, with the rebuilt binary:

| Spec | Before | After |
|---|---|---|
| `plugin/cfilter_spec.lua` | 16 of 18 failed | 18 of 18 pass |
| `plugin/dir_spec.lua` | 32 of 38 failed | 31 of 38 failed |
| `plugin/editorconfig_spec.lua` | 11 of 12 failed | 11 of 12 failed |

Two causes were found and fixed:

- `<q-bang>` was left literal. `uc_check_code` (usercmd.c:1563) expands it to
  `"!"` or `""`, and `pack/dist/opt/cfilter/plugin/cfilter.vim:61` builds a
  `:call` expression from it, so every cfilter test died on E15.
- A URL was treated as a relative name. `vim_isAbsName` (path.c:1750) counts
  one as absolute, so `nvim_buf_set_name` and `fnamemodify(':p')` must not
  prepend the working directory. `dir_spec` T2 passes now.

The remaining `dir_spec` cluster has one structural cause: `:edit <dir>` is
implemented in Rust (`crates/ox-editor/src/excmd_exec.rs:13625-13740`, a port of
`runtime/lua/nvim/dir.lua`), while upstream calls the Lua module. So
`package.loaded['nvim.dir']` stays nil ("does not load the module until opening
a directory"), and the module's own nested autocmd behaviour around listings
never runs. Patching the Rust copy to imitate those three cases would add a
second owner for the same behaviour. The move that removes the divergence is to
delete the Rust listing and call `nvim.dir.try_open` (and
`nvim.dir.handle_startup_dirs` for a startup directory) through the existing Lua
host, which is what upstream does.

`editorconfig_spec` fails for a different reason and is unmeasured beyond its
first assertions: `indent_size = 3` leaves `'shiftwidth'` at 8, so
`runtime/lua/editorconfig.lua` never applies. These specs run under the
functional harness, not `-u NONE`, so the probe has to go through the harness.


### Buffer names: API regression

`buf_set_name_keeps_a_url_out_of_the_working_directory` (`crates/ox-api/src/tests.rs`)
now pins the boundary the `dir_spec` URI failure crosses: a URL survives
`nvim_buf_set_name` unchanged, and a relative name still becomes absolute. The
test fails with the guard removed and passes with it.

### OptionSet: the event is declared but never fired

`dir_spec` "triggers nested autocmds when opening directory buffers" and
"handles nested autocmds deleting the directory buffer" register
`autocmd OptionSet buftype` and read `v:option_new`. In this port the event name
exists (`crates/ox-editor/src/autocmd.rs:146`) but no code path fires it, so
those tests cannot pass however the listing is implemented.

The contract to port is `apply_optionset_autocmd_now` (`option.c:2099-2134`):

- it fires only when not starting up and not already inside an OptionSet
  dispatch (`*get_vim_var_str(VV_OPTION_TYPE) != NUL` gates recursion);
- the pattern is the option's full name;
- it sets `v:option_new`, `v:option_old`, `v:option_oldlocal`,
  `v:option_oldglobal`, `v:option_type` (`local` or `global`), and
  `v:option_command` (`set`, `setlocal`, `setglobal`, or `modeline`), then
  resets those variables afterwards.

The port has three option setters (`Options::set_global`, `set_buffer`,
`set_window`, `crates/ox-editor/src/options.rs:333-420`) and no single place that
knows a change happened, which scope it took, and the old value. Firing needs
that knowledge, so the hook belongs above the setters, where the editor also
holds the autocmd executor.


### OptionSet: fired from the option API, still short of the spec

`4f56421` adds the dispatch: `OptionSet` fires from `nvim_set_option_value`
with the option's full name as the pattern, the six `v:option_*` values
published for the callbacks and restored afterwards, a startup gate, and the
no-nesting gate from `option.c:2102`. The test
`set_option_value_fires_optionset_with_the_new_value` reads the values while the
callback runs and fails when the dispatch is removed.

It does not move `dir_spec`: still 7 of 38 pass, and T3/T4 report no events.
The reason is the earlier finding, not this gap. Our `:edit <dir>` renders the
listing in Rust, so `nvim.dir` never runs, `buftype` is never written through
the option API, and there is nothing to announce. Once the plugin drives the
listing, this dispatch is what makes its `buftype` write visible.

`:set` and its relatives are not wired yet: `OptionAssignment`
(`excmd_exec.rs:5702-5712`) carries the committed value and whether it changed,
but not the old effective, local, and global values the event publishes. That
change has to widen the record first.


### OptionSet: all three setters now publish it

| Path | Where | Test |
|---|---|---|
| `nvim_set_option_value` | `ox-api/src/global.rs` | `set_option_value_fires_optionset_with_the_new_value` |
| `nvim_set_option` (deprecated) | `ox-api/src/global.rs` | covered by the same seam |
| `:set`, `:setlocal`, `:setglobal` | `excmd_exec.rs` `fire_optionset_autocmd` | `set_fires_optionset_with_the_new_value` |

Two details the implementation had to get right, both found by the tests:

- the handlers read `v:` through the scope mirror, so the published values are
  synced into the scope before the dispatch runs. Without that sync the handler
  saw the previous, empty values;
- the assignment record had to carry the layer and both old values, because
  `v:option_command`, `v:option_type`, `v:option_oldlocal`, and
  `v:option_oldglobal` differ per scope (`option.c:2115-2129`).

`dir_spec` is unchanged by this: 7 of 38 pass. Its T3 and T4 tests wait for
`buftype` to be written through the option API by `nvim.dir`, and this port
still renders the listing in Rust, so that write never happens. The listing
cutover is the change that makes those two tests reachable.


### The three open gaps, with the constraint that blocks each

**Terminal output during `vim.wait` when both executors are busy.** The pump
already drains PTY output and deferred job events from whichever executor slot
it can borrow (`crates/oxvim/src/server.rs:6318-6360`). When both slots are
held by the enclosing frames, nothing can be drained, because the bytes live in
`ExRuntime.jobs` and `flush_pty_output` (`excmd_exec.rs:1733`) needs `&mut
ExExecutor`. The editor itself is reachable from the session at that point, so
the blocker is ownership, not access. The bytes live in a private
`Job.pty_output: Vec<u8>` (`job.rs:309`) and leave it only through
`take_all_pty_output`, which needs `&mut JobManager` (`job.rs:1230`). A second
drain would fork the queue, so this state has to move out of the executors into
session-owned state before the pump can serve a fully busy editor, and that move
needs its own regression test.

**Dict watchers.** The registry and the firing round are complete
(`userfunc.rs`), and nothing calls them. The delete path is threaded as
`&ExRuntime` from `remove_subscript_chain` down to `dict_remove_key`
(`lvalue.rs:815-850`, `:911`), while firing needs `&mut ExRuntime` plus
`access`, `scope`, and `lua` (`excmd_exec.rs:5054`). The Ex-side `:set`
dispatch added for `OptionSet` is the shape that fits here: a command-level
hook that reads the record and dispatches with the full context.

**`:checkhealth` providers.** `:checkhealth` routes to
`require('vim.health')._check` when a Lua host answers
(`excmd_exec.rs:17071-17076`) and falls back to the native report when the host
cannot run the module re-entrantly. The functional `health_spec` failures report
`E5108: Lua callbacks are not installed` at `testnvim.lua:144`, which is a
client-side call, so the provider call itself is not proven to be the failing
one. Two mechanisms fit and a measurement separates them: run the same
`:checkhealth` through the embedded server with only one executor slot occupied,
and see whether the providers run. Until then the cause stays unconfirmed.

Host availability is ruled out for one evidence set only, and the chain was
checked end to end: the oldtest runner invokes `oxvim ... --headless`
(`tests/oldtest/run.sh:75`), which reaches `server::run_stdio`
(`main.rs:141-143`), and that path installs a Lua host (`server.rs:636`). So the
99 E5108 hits in that census are real Lua errors. `run_batch` (`main.rs:134`,
selected by `-e`/`-s`) and `run_lua` (`main.rs:119-121`) do lack a host, and
those are the paths where a probe answers "no Ex-command host is installed".
The two evidence sets stay separate.


### Full `just oldtest`, measured

One run over all 236 files, 340 seconds, exit 1:

| Outcome | Count |
|---|---|
| executed | 3764 |
| failed | 2364 |
| skipped | 693 |
| clean files | 11 |
| blocked | 39 |
| timeout or crash | 3 |

The gate is measured, not closed. The failures cluster by error code rather
than by file, so one cause carries many files:

| Error | Count | Sample |
|---|---|---|
| E605 (wrapped inner error) | 243 | `Vim(argdelete):E480: No match: x` |
| E492 not an editor command | 221 | `:syntax`, `:diffsplit`, `:match`, `:recover`, `:compiler`, `:helptags`, `:doautocmd`, `:filter`, `:tabs`, `:saveas`, `:checkpath`, `:breakadd` |
| E117 unknown function | 157 | `s:LoadFTPlugin` from `runtime/ftplugin.vim`, `g:foo#bar.echo` |
| E488 trailing characters | 114 | command forms that take arguments we reject |
| E474 invalid argument | 107 | option values |
| E5108 Lua call failed | 99 | see below; three different causes in one code |
| E806 variable scope errors | 97 | |
| E518 unknown option | 95 | options the metadata lacks |
| E121 undefined variable | 90 | |

Worst files by failed count: `test_cmdline.vim` (364), `test_winfixbuf.vim`
(79), `test_ins_complete.vim` (104 executed, 22 failed of 141),
`test_normal.vim` (91), `test_options.vim` (63), `test_edit.vim` (59),
`test_diffmode.vim` (45), `test_search.vim` (52), `test_filetype.vim` (70),
`test_window_cmd.vim` (49).

E5108 is one code with several causes, so it is not a single cluster. Three
samples from the same run:

- `luaeval()` calling a nil value (`Test_cmdmod_completion`);
- `s:LoadFTPlugin` unknown, raised through `runtime/filetype.lua:27`
  (`Test_compiler_spotbugs_properties`), so this one shares the E117 cause;
- `test_disable_char_avail` undefined symbol in the binary
  (`Test_cmdcomplete_info`), which the test harness itself overrides.

The E492 count also over-reports missing built-ins: its sample includes
user-defined names such as `xxx`, `one`, and `foo2`, which tests register
themselves and which are expected to fail. Any repair batch has to compare each
name against the command registry and the reference test first.

A minimal reproduction of one E117 sample (`s:Hello()` called from an `au
FileType` defined in the same script) answers `yes` on both binaries, so that
cluster comes from a different script-context path than the simple case and
needs its own reproducer before any fix.


### `s:` in autocmd bodies: measured effect

`a20c5e0` makes an autocmd body run under the script that defined it. The
`runtime/ftplugin.vim` failure (`au FileType * call s:LoadFTPlugin()`) no longer
appears in the oldtest messages at all, and one file moved a long way.

| Measure | Before | After |
|---|---|---|
| executed | 3764 | 3285 |
| failed | 2364 | 2055 |
| skipped | 693 | 571 |
| blocked | 39 | 45 |
| clean files | 11 | 11 |
| `test_cmdline.vim` failed | 364 | 121 |

Both runs are full `just oldtest` passes over the same 236 files. The failure
count drops by 309; the block count rises by 6, so part of the change is tests
that now reach a later, unsupported feature instead of failing at the handler.
`test_cmdline.vim` accounts for 243 of the drop.


### `:doautocmd`

`49a2ebc` adds `:doautocmd [group] {event}[,{event}] [fname]`
(`do_doautocmd`, autocmd.c:1141), which the port lacked entirely. It rejects
`*` with E217, fails before firing on an unknown name (E216), accepts
`<nomodeline>` without running modelines, and reports
"No matching autocommands: {arg}" when nothing matched. The regression covers a
firing event and a miss. Red proof, run in both states: with the dispatch
replaced by `Flow::Normal`, the regression fails; with the command restored it
passes.

`45d2c27` corrected three details against the reference after review:
`<nomodeline>` is a prefix (`autocmd.c:1244`), a run that matches nothing is
silent because `ex_doautocmd` passes `do_msg = false` (`ex_docmd.c:4527`), and
E216 names the group only when one was given (`autocmd.c:2467`). The regression
now covers an explicit file name, a name the pattern rejects, the
`<nomodeline>` prefix, group restriction, E217, and both E216 texts. Modeline
execution remains out of scope: this port does not run modelines.

This is the first item from the E492 census. The census itself over-counts
missing built-ins because it also contains user-defined names such as `xxx`,
`one`, and `foo2`, so each remaining candidate still needs a registry check
before it is implemented.


### `ex_cmds` functional group, partial measurement

This is a timed-out partial measurement, not a completed group. One run,
1200 seconds, exit 1: the runner killed the group while `:write appends FIFO
file` (T435, `write_spec.lua`) was still running, so no group summary exists
and both counts below are lower bounds.

| Outcome | Count |
|---|---|
| run | 405 |
| OK | 54 |
| FAIL | 343 |

Worst specs, failures over tests:

| Spec | Failing |
|---|---|
| `ex_cmds/dict_notifications_spec.lua` | 64 of 70 |
| `ex_cmds/echo_spec.lua` | 39 of 41 |
| `ex_cmds/cd_spec.lua` | 32 of 36 |
| `ex_cmds/mksession_spec.lua` | 18 of 18 |
| `ex_cmds/swapfile_preserve_recover_spec.lua` | 17 of 17 |
| `ex_cmds/help_spec.lua` | 15 of 16 |
| `ex_cmds/pack_spec.lua` | 15 of 15 |
| `ex_cmds/cmd_map_spec.lua` | 15 of 15 |

The last test reached is T435 `:write appends FIFO file`; the log has no
per-group total line because the process died at the timeout.

`dict_notifications_spec` is the largest single cluster here, and it is exactly
the dict-watcher gap: the registry and its firing round exist, and nothing calls
them. That makes the watcher hook the highest-value next change in this group,
not a small one: a watcher callback dispatches through four live forms
(`Name`, `Funcref`, `Partial`, and a removed watcher, `userfunc.rs:82-92`), and
the round must mark, check, and finish around user code that can reenter.


### Dict watchers: the design that fits the port

More is known now than when the gap was first recorded:

- `WatchCallback` has no Lua form (`userfunc.rs:82-92`): a name, a `Funcref`, a
  `Partial`, or `None`. So firing needs no Lua host for the callback itself.
- The dispatch helpers still take `Option<&Rc<dyn LuaExec>>` as a parameter
  (`call_user_function_with_self`, `excmd_exec.rs:5054`; the timer path's
  `EvalHost`, `excmd_exec.rs:1385-1400`), and lvalue has no access to it.
- `dict_write_key` holds `try_borrow_mut` until it returns
  (`lvalue.rs:886-908`), and upstream passes the replaced value as `old` while
  omitting `old` for a newly added key (`tv_dict_watcher_notify`,
  typval.c:2054).

That fixes the shape: the leaf records `(dict, key, old, new)` into a queue on
`ExRuntime`, and `ExExecutor` drains it at the end of the command, where
`self.lua`, `access`, and `scope` all exist. The round then runs
`dictwatcher_begin` → `is_live` → invoke → `dictwatcher_arguments` →
`dictwatcher_finish` exactly as `userfunc.rs:475-535` expects, so a callback
that removes a watcher or mutates the dictionary mid-round stays consistent.

Ordering is per command rather than per mutation. For `:let d.a = 1 | let
d.b = 2` that is still one fire per write, because the `|` separator runs two
commands; the divergence appears only if a single command writes one dictionary
twice, and that case needs a test before this lands.


### Scope-dict subscripts: the first failure in `dict_notifications_spec`

Reproduced outside the harness:

```vim
let g:["probe"] = 42
call writefile([string(get(g:, "probe", "missing"))], "/tmp/scope.txt")
```

The reference writes `42`. This port aborts line 1 with
`E121: Undefined variable: g:`, so none of the spec's 70 tests can reach its
assertions, and the watcher hook is not what fixes that cluster first.

One attempt was made and reverted rather than left half-wired: teaching
`bind_lvalue` (`lvalue.rs:531`) to accept `g:`/`b:`/`w:`/`t:` with subscripts by
resolving the scope dictionary. The binary still reported E121, so the binder at
`lvalue.rs:531` is not the one the `:let` path uses for this spelling. The
remaining candidates are the `:let` target resolution in `command_let`
(`excmd_exec.rs:5402`) and its `split_assignment` (`excmd_exec.rs:22412`), and
the E121 text renders the name as `g` rather than `g:`, which points at the
name-handling step before either.


### Scope-dict subscripts: fixed and verified

`g:['k'] = v` and `unlet g:['k']` address the scope dictionary itself
(`get_lval`), and only a bare prefix is E461. Both failed: the lvalue bound as a
plain variable called `g:` (E121), and the removal went through the snapshot
`scope_dict` returns.

| Probe | Before | After | Reference |
|---|---|---|---|
| `let g:["probe"] = 42`, then `get(g:, 'probe')` | no value | `42` | `42` |
| `unlet g:["probe"]`, then `exists("g:probe")` | `1` | `0` | `0` |

| Test | Red proof |
|---|---|
| `a_scope_dict_subscript_assignment_reaches_the_scope` | failed with E121 before `f359610` |
| `a_scope_dict_subscript_unlet_removes_the_variable` | failed with the `remove_pair` arm disabled, passed after restoring it |

Both fixes are single-key forms. A multi-key path such as
`let g:['a']['b'] = 1` still resolves through the snapshot and is untested;
`dict_notifications_spec.lua` writes only single keys.


### `dict_notifications_spec` after the scope-dict fixes

Partial run, 300 seconds, exit 124 (killed during T34):

| Outcome | Count |
|---|---|
| run | 34 |
| OK | 0 |
| FAIL | 33 |

Setup now succeeds where it used to abort on the first line, so the spec reaches
its assertions. The first failure is not the watcher: T1 reports
`Vim(call):E475: Invalid argument: Channel doesn't exist` from the spec's own
`rpcnotify`, and its `after_each` then reports
`Couldn't find a watcher matching key and callback`. T2 and later compare the
expected notification payload and get nothing, which is the watcher gap.

So two causes remain, in this order:

1. `rpcnotify` cannot reach the client channel. `process.rs:919` resolves ids
   through the job list, while the client channel is registered in the session
   (`session.rs:113`), so the two id spaces never meet. Every `rpcnotify` from
   Vimscript fails with E475 until one lookup covers both.
2. The watchers never fire, which is the round described above.

The run is a lower bound for the same timeout reason as the group measurement.


### `rpcnotify` to the client channel: the seam it needs

`JobManager::rpc_notify` sends to a job, else to a socket, else errors
(`job.rs:982-988`); the client channel is neither, so `rpcnotify(1, ...)` from
Vimscript always reports E475. `ServerHost` is not the answer either: it
exposes only `start`, `stop`, and `list` (`server.rs:21-41`) and knows nothing
about transports.

The shape that fits is the one `set_lua_exec` already uses: the manager gains an
embedder-installed sink, and the server installs it with a handle that writes a
`[2, event, params]` frame into its `ChannelOutput` (`server.rs:51`). No
`JobManager` fallback should pretend a transport channel is a job.


### `:checkhealth`: cause identified, not a missing host

Measured from the captured plugin-group run plus code citations, without edits:

- The group's first failure is **T76**, an assertion (`:checkhealth detects invalid
  $VIMRUNTIME`: expected false, actual true), not an error.
- The first `E5108` is **T82** `:checkhealth vim.g.health`, raised at
  `testnvim.lua:144` from `Vim(checkhealth)` in `health_spec.lua:82-88`, and it
  repeats through T97.
- That string comes only from a `lua: None` path
  (`excmd_exec.rs:18861-18865`, `run_lua_autocmd_callback`; same shape at
  `20477-20479`). A busy executor pair prints a different text
  (`"no free Ex executor for …"`, `server.rs:5189-5192`), so re-entrancy is not
  this failure. Both executors are configured with hosts
  (`server.rs:636-637`, `4756-4770`).
- `require('vim.health')` loads and its table works: T80 passes
  (`health_spec.lua:68-76`). The gap is callback delivery into `:checkhealth`,
  not a missing health API.
- Confirmation step, if wanted: at the `None` branch record `lua.is_some()`
  with both `try_borrow` results; `None` confirms this caller, and only a
  both-borrows-failed result plus the `"no free Ex executor…"` string would
  revive the re-entrancy theory.


### Ex command inventory: upstream against the dispatch table

Method: every name in `.references/neovim/src/nvim/ex_cmds.lua` (44-3459) against
the dispatch in `excmd_exec.rs:3734-4139`, exact-string rows, alias families kept
separate and multi-name arms expanded.

| Set | Count |
|---|---|
| upstream names this port does not dispatch | 201 |
| names both accept | 363 |
| names this port accepts that upstream does not list as rows | 7 |

The 7 are `b`, `bprev`, `cprev`, `lcexpr`, `lprev`, `tabc`, `tabn`. They read as
an artifact of the exact-row method rather than a defect: upstream reaches those
spellings through prefix matching, not as table rows, so this line is a
method note, not a finding.

The 201 missing names carry oldtest evidence where the census has it:
`recover` 8, `diffsplit` 6, `helptags` 6, `match` 5, `checkpath` 4, `tabs` 4,
`>`, `~`, and `@` 3 each, `tabprevious` 3, then `breakadd`, `earlier`, `goto`,
`pedit`, `psearch`, `rundo`, `wundo`, `arglocal`, `ascii`, `ball`, `balt`,
`changes`, `diffpatch`, `folddoopen`, `grep`, `left`, `make`, `packloadall`,
`pbuffer`, `saveas`, `sargument`, `sball`, `sfind`, `tabmove`, `tags`,
`undolist`, `=` with 1 or 2 each.

Cheapest five, by behaviour rather than popularity: `pwd`, `list`, `number`,
`uptime`, `tabs` — each reuses an existing path (`command_cd`, `command_print`,
`command_version`, the tab iteration) and adds no state.


### Round summary after the watcher, command, and sink work

Landed commits in this round: `cef2725` (dict watchers, dict targets),
`bb7df47` (scope targets + registration through the evaluator identity),
`127e70a` and `d898b6b` (two mutation-proved test deletions), `72425a2`
(`:pwd`, `:list`, `:number`, `:uptime`, `:tabs`), `acbc643` (client channel
sink for `rpcnotify`).

| Measure | Before round | After |
|---|---|---|
| workspace suite | 3793 | 3811 |
| `plugin/cfilter_spec` | 18 of 18 | 18 of 18 |
| `plugin/dir_spec` | 7 of 38 | 8 of 38 |
| `ex_cmds` group (partial, 1200 s timeout) | 54 OK / 343 FAIL of 405 | 58 OK / 339 FAIL of 405 |
| `dict_notifications_spec` (partial) | 0 OK / 33 FAIL of 34 | not yet re-measured with the sink built in |

The `ex_cmds` numbers predate the client sink, because the run used the binary
built before `acbc643`; the `dict_notifications_spec` re-measure with the sink
is the next gate.


### `dict_notifications_spec` after the sink and the watcher round

| Measure | Before round | After |
|---|---|---|
| tests run | 34 (killed at 300 s) | 70 (finished, exit 1) |
| OK | 0 | 26 |
| FAIL | 33 | 44 |

The spec now runs to completion instead of dying at the harness timeout, and 26
of its 70 tests pass. Two causes carried the earlier zero: `rpcnotify(1, ...)`
failed with E475 until the client sink landed (`acbc643`), and no watcher fired
until `cef2725`/`bb7df47`.

The remaining 44 cluster on mutation routes this round did not cover:

- `remove()` on a dictionary (T2, and the same test through `nvim_*_var` and
  `vim.g.` in T3 and T4): `ox-eval`'s `remove()` builtin mutates a dictionary
  outside the lvalue paths the watcher producer watches.
- `extend()` (T5): same reason, different builtin.
- key patterns, empty keys, and the change-sequence tests: pattern matching and
  ordering once the above routes notify.
- `b:` remove: the scope variant of the first item.

Each is a producer gap in `builtins.rs` (dictionary-builtin mutations), not a
round defect: the round itself is exercised by the tests that pass.


### Builtin producers: measured, and what remains

`36779d3` made `remove()`, `extend()`, and `filter()` notify dictionary watchers.

| Measure | Before | After |
|---|---|---|
| `dict_notifications_spec` OK | 26 | 28 |
| `ex_cmds` group OK (of 405) | 58 | 76 |

The spec moves by only two tests, and the remaining failures still name
`remove()` and `extend()` on `g:`. That is the diagnosis: the spec mutates the
scope mapping itself (`remove(g:, 'k')`, `extend(g:, {...})`), not a dictionary
variable, so the new producers record a `WatchTarget::Dict` event while the
watcher is registered as `WatchTarget::Scope(Global)`. The builtin routes need
the same scope-identity mapping that `dictwatcheradd` already uses.


### `:checkhealth` after the host seam

`a32f365` removed the E5108 failure mode: the health spec no longer contains a
single `E5108` line (it had 16 before), and the Lua callbacks registered on
`FileType/checkhealth` now run.

Before, from the plugin-group run that covered this file: 41 failures and 16
`E5108` lines. After, measured alone: 44 run, 4 OK, 40 FAIL, zero `E5108`
lines. The OK count before was not measured alone, so it is not claimed here.

The remaining failures are assertions about behaviour the port has not matched,
starting with T1 `:checkhealth detects invalid $VIMRUNTIME` (expected false,
actual true) and T2 for `'runtimepath'`, then `getcompletion()` and cmdline
completion. Those are separate defects from the callback path.


### Why the remaining 42 stay red: the dict argument's identity

The spec's callback refuses to notify unless the dictionary it receives is the
scope mapping itself:

```vim
function! g:Changed(dict, key, value)
  if a:dict isnot g: | throw 'invalid dict' | endif
  call rpcnotify(g:channel, 'values', a:key, a:value)
endfunction
```

Probe on the real binary: a watcher on `g:` with a Vimscript callback that only
appends to a list fires correctly for `let g:['watched'] = 'test'` on both
oxvim and the reference, so the producer works. What fails is the identity: the
drain builds the dict argument with `scope.scope_dict(kind)`, which is a fresh
per-read snapshot, so `a:dict isnot g:` is true inside the callback and it
throws before notifying.

The fix is structural: a scope namespace needs one stable dictionary identity
per session (upstream's `vimvardict`), with `set_scoped` keeping it current,
instead of a snapshot rebuilt on every evaluation.
