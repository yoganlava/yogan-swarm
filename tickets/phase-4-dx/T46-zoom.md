- [x] **T46 Zoom**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Screens › B, C; Mouse map.

`z` gives the detail pane the full width; `z` or `esc` goes back. `←/→` cycle the detail tabs; `1–6` still work. Double-clicking a row zooms it (two `Down`s on the same cell within 400 ms, since crossterm doesn't report double-clicks). The pane's top border shows `z zoom` / `esc unzoom` as a hit target.

Done when: snapshot of a zoomed detail pane at 100 columns; unit test that a double-click on a row zooms.

## Current code

- `draw` (`src/tui.rs:1770`) splits 45/55 at 100+ columns (~L1794) and shows one pane below that.
