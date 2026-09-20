# Parser fixture contract

This layer follows PR #34. It changes a test fixture, not the tree-sitter
implementation or its expected highlighting behavior.

The test helper accepts either a Lua or C parser. The highlight regression
nevertheless always parsed Lua source and queried Lua's `number` and `chunk`
nodes. With a real C parser, it failed before reaching any highlight assertion:
`Invalid node type "number"`.

The fixture now selects valid source and node names for the selected grammar.
Both sources place `value` at columns 6 through 11 and span two rows. All
assertions remain: capture count, hidden-group filtering, buffer and namespace
IDs, exact coordinates, default priority, explicit priority, and root range.
An additional assertion rejects a syntax-error tree.

Validation used tree-sitter-c v0.24.2, built from the archive whose SHA-256 is
`2eeb4db31f8fa0865e45488503d13403923bcb485a1bdb637abff8c42dd97364`.
The complete workspace command was:

```sh
unset CARGO_BUILD_BUILD_DIR RUSTC_WRAPPER
export CARGO_TARGET_DIR="$PWD/target" CARGO_BUILD_JOBS=8
export OXVIM_TREE_SITTER_PARSER="$PWD/target/test-fixtures/c.so"
export OXVIM_TREE_SITTER_LANGUAGE=c
cargo nextest run --locked --release --workspace --no-fail-fast
```

Result: 3,470 tests ran; 3,469 passed and one failed. One further test was
skipped by the existing configuration. The failure is
`differential::perf_contract::steady_state_workloads_are_state_neutral`, whose
required `.references/neovim/build/bin/nvim` binary is absent. The test was
not bypassed or changed. This is not a passing full-workspace gate or a
completed performance comparison.
