- [ ] **T37 Toasts**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › Keys (messages); problem 6. The plan's *Notices* widget: bottom-right Block, fades after 4 s.

Info messages show bottom-right and fade after 4 s. Errors stay until `esc`. Neither replaces the key bar.

Done when: snapshot with a toast and the key bar both visible; unit test that an info toast is gone after 4 s and an error is not.

## Current code

- `App::key` clears `notice` and `info` on every key (`src/tui.rs:551`); `draw` shows them in place of the keys (~L1323).
