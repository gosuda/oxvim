# Sanctioned differential divergences

Each entry must name one replay session and give a one-line behavioral justification. Add entries only through `replay --bless --reason` after inspecting the printed semantic diff.
- replay/sessions/core.yaml [sha256:03626d929d05c75b066d1e2c46ad1c4ff5349d68323e8a57cf5ee26f3b326c1d] — Oxvim's API metadata surface is incomplete (upstream nvim_mcursor still unimplemented); all subsequent core smoke responses match upstream.
- replay/sessions/ui_attach.yaml [sha256:50f177d5de114536c96ca6316c367d34db8146ce3226c75bc64b07b67989faca] — Oxvim emits the required upstream-ordered initial redraw metadata before attach response, while its deterministic compositor/highlight snapshot intentionally omits Neovim runtime-specific title, cwd, full default highlight corpus, and secondary mode/mouse frame.
