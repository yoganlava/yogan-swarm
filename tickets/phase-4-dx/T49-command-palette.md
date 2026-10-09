- [x] **T49 Command palette replaces the help overlay**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Screens › D; Decided 1.

`?` (and `:`) opens a centred palette: a query line, then rows for the selected task's actions, the global keys and `Go to <task>` for every task, each with its key chip and a dim context column. Typing filters by subsequence; `↑/↓` or hover selects; `enter` or a click runs; `esc` or a click outside closes. Rows come from `KEYS` and the per-state key list, so the palette can't drift from the real keys. The help overlay goes.

Done when: snapshot with the query `appr` on a Proposed task; unit test that typing filters and `enter` runs the selected action; the help overlay code is gone.

## Current code

- `KEYS` (`src/tui.rs:52`); `help` (~L2735) draws the overlay; `?` sets `self.help` (~L1000) and `App::key` clears it (~L941).
