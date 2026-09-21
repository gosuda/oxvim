# Lua 5.1 headers for LPeg

These unmodified headers come from `luajit-src 210.7.2+b925b3e`, the exact
LuaJIT source package selected by `Cargo.lock` through `mlua-sys 0.11.0`.
The source paths are `luajit2/src/{lua.h,luaconf.h,lauxlib.h}`. `COPYRIGHT`
is copied from `luajit2/COPYRIGHT` and retains the upstream license.

LPeg uses only the Lua 5.1 C API. The executable still links the single LuaJIT
runtime built by `mlua-sys`; these files do not build or link another Lua runtime.
Keeping the C headers as explicit source inputs makes fresh, parallel and
cross-target builds independent of Cargo's target-directory layout and build order.

When changing the Lua implementation or its C API, update these headers from
the same locked source package and run a clean workspace build plus the LPeg
tests in `crates/ox-lua/tests/stdlib.rs`.

| File | SHA-256 |
| --- | --- |
| `lua.h` | `ecab8480aaedb648b75258453ab82e71c4d0db234175f455af72962c8578f787` |
| `luaconf.h` | `0368985c56235d9bf8082dee845289500ee91b281e3ae6d8c434ca5348d9a976` |
| `lauxlib.h` | `7093ea70fb38341d10475814b90961cec22f9643c11e3ea4c2ddc2cd4b03164e` |
