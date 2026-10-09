- [ ] **T38 Toasts**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Visual language › Toast.

Info messages show in the bottom right as a rounded card (`Clear` + `Block`, accent border) and fade after 4 s. Errors have a red border and stay until `esc`. Clicking a toast dismisses it. Neither replaces the key bar.

Done when: snapshot with a toast and the key bar both visible; unit test that an info toast is gone after 4 s and an error is not; a click on a toast dismisses it.

## Current code

- `App::key` clears `notice` and `info` on every key (`src/tui.rs:827`); `draw` shows them in place of the keys (~L1870).
