- [x] **T50 Draggable divider and pane scrollbars**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › How it's built › Divider; Decided 2.

A left `Down` on the border between the panes, then `Drag` events, set `App::split` (list width, 30–70 columns), which replaces the fixed 45%. It lasts for the session; nothing is saved to config. A ratatui `Scrollbar` sits on the right border of any pane whose content overflows; clicking it jumps there.

Done when: unit test that a drag changes the split and clamps at 30 and 70; snapshot of a scrollbar on an overflowing Activity tab.

## Current code

- `draw` uses `Constraint::Percentage(45)` (`src/tui.rs:1794`). The detail pane's scroll offset is `App::scroll`, moved by `scroll_by`.
