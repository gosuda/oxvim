# Terminal stabilization, 2026-09-20

This layer follows `stabilize/01-reproducible-lpeg-build` (PR #33).
It fixes observed terminal behavior rather than replacing the editor model.

## Reproductions and repairs

| Public behavior | Observed failure | Repair |
| --- | --- | --- |
| Edit, move, save, quit | The terminal cursor stayed hidden after painting. | Position and show the composed editor or command-line cursor after every frame. |
| Unicode input | Queued UTF-8 bytes became separate characters. | Decode complete scalars after byte-oriented mapping, including quoted `K_SPECIAL` continuations; retain incomplete input. |
| Left/right in Insert and command-line modes | Cursor movement was ignored; subsequent editing and undo were wrong. | Track UTF-8 insertion boundaries and split Insert undo blocks after movement. |
| Command-line cursor-only updates | The RPC cursor position always described the end of the command. | Send the current byte position, accounting for displayed control characters. |
| Resize with immediate keyboard input | The smaller grid appeared but keyboard input stopped. | Select crossterm's level-triggered `use-dev-tty` backend. Its poll loop needs a positive timeout; use one millisecond. |
| Resize while the embedded server polls | `SIGWINCH` interrupted `epoll_wait`; the server exited with an I/O error. | Treat `Interrupted` as an empty reactor wakeup, preserving timer and signal processing. Other I/O errors still propagate. |
| Read screen dimensions after resize | `&columns` and `&lines` retained startup values. | Update geometry and screen options through the existing session-owned resize API. |
| Windows capability negotiation | Unix-only polling could not consume emitted probe replies. | Keep the Windows path on environment-derived capabilities without sending queries or consuming input. |

The resize trace showed both keyboard and signal readiness in one epoll batch.
The old terminal backend returned the resize event before consuming keyboard
readiness; a later edge-triggered poll did not report those unread bytes again.
No delay or retry was added to the test to hide that failure.

## Tests

`crates/oxvim/tests/tui_e2e.rs` launches the actual binary with an embedded
server in a pseudoterminal. A VT parser checks the current screen and cursor,
and file checks compare saved bytes. Each session uses a private home,
configuration, working directory, and document. Deadlines bound waits.
Failures retain an ANSI transcript and screen snapshot; children are killed
and reaped during failed-session cleanup.

Four scenarios cover editing and saving; Unicode, arrows, undo and redo;
command-line insertion and deletion; and shrink/grow with immediate input.
The two existing `interactive_pty` scenarios also pass. The four new scenarios
passed 20 consecutive runs, totaling 80 successful scenario executions.
A separate traced resize run passed with observed `EINTR` deliveries.

Focused commands, from the repository root:

```sh
unset CARGO_BUILD_BUILD_DIR RUSTC_WRAPPER
export CARGO_TARGET_DIR="$PWD/target" CARGO_BUILD_JOBS=8
cargo test --locked --release -p oxvim --test tui_e2e --test interactive_pty
cargo test --locked --release -p ox-editor --test unicode_input
cargo test --locked --release -p ox-tui
cargo check --locked --target x86_64-pc-windows-msvc -p ox-tui --all-targets
```

The Windows command was a cross-check on Linux, not a native editor run.
The workflow runs actual Linux PTY tests and native Windows terminal-library
tests, pins action revisions and Rust 1.98.0, and uploads failed PTY artifacts.
It does not label the terminal-library job as full Windows editor coverage.

## Broader validation and limits

The first fresh workspace nextest run completed 3,470 tests: 3,461 passed,
nine failed, and one additional test was skipped. Eight failures require a
dynamic tree-sitter parser fixture; the remaining performance-contract test
requires `.references/neovim/build/bin/nvim`. These failures were not skipped,
marked ignored, or changed into passing tests.

For the parser rerun, the C grammar fixture is tree-sitter-c v0.24.2, archive
SHA-256 `2eeb4db31f8fa0865e45488503d13403923bcb485a1bdb637abff8c42dd97364`.
Build its `src/parser.c` as a shared library and set
`OXVIM_TREE_SITTER_PARSER` to its absolute path and
`OXVIM_TREE_SITTER_LANGUAGE=c`. Fixture sources belong under `target`, not
in the product source or the read-only reference checkout.

Workspace all-target Clippy also finds existing denied `unwrap_used` calls
in `ox-text/src/swapfile.rs` tests. Focused Clippy completes, with existing
warnings in other library code. No workspace-wide lint allowance was added.

This layer does not establish full Windows editor support, upstream functional
or oldtest parity, or the comparative performance contract. It makes no
throughput or latency-improvement claim. Those gates remain distinct from
passing real-terminal regressions.
