- [x] **T37 Hit map: a click is a key**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › How it's built › One hit map; Mouse map.

`Hits` becomes a list of `(Rect, Target)` that `draw` fills as it renders each clickable thing. `Target` is a key, or one of the few things with no key (select a row, fold a group, drag the divider). `App::mouse` finds the target under a left click and calls `App::key` with its key, so the mouse can't do anything the keyboard can't.

Targets in this ticket: rows (as today), footer keys (T36), detail tabs, and the confirm modal's buttons (` y  Discard `, ` esc  Keep it `). Later tickets register theirs. While a modal is open, only its targets respond.

Done when: unit tests that clicking a footer key, a tab and the confirm modal's `y` button each do what pressing the key does; the existing row-click test still passes.

## Current code

- `Hits` (`src/tui.rs:365`) holds the list and detail rects and row ys; `draw` resets it (~L1771).
- `App::mouse` (~L1435) returns early when any modal is open and handles only left `Down` in the list and the wheel.
- The confirm modal is drawn at ~L1888.
