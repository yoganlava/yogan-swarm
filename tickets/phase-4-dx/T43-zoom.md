- [ ] **T43 Zoom the detail pane**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › B · Running (`z`).

`z` gives the detail pane the full width; `z` or `esc` goes back. `←/→` cycle the detail tabs; `1–5` still work.

Done when: snapshot of a zoomed detail pane at 100 columns.

## Current code

- `draw` (`src/tui.rs:1249`) splits 45/55 at 100+ columns and shows one pane below that.
