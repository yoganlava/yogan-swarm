- [ ] **T40 Hover shows the key**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Try it (hover); How it's built › Hover.

Handle `MouseEventKind::Moved`: remember the hovered target, give it a lighter background, and write its key and label (`m · open PR`) right-aligned in the bottom border of the pane it's in (`Block::title_bottom`). Redraw only when the hovered target changes.

Done when: snapshot with a hovered footer key showing the tint and the hint; unit test that a move over a tab sets the hint and a move over empty space clears it.

## Current code

- `init` (`src/tui.rs:582`) enables `EnableMouseCapture`, which already turns on all-motion tracking; `App::mouse` (~L1435) drops `Moved` in its `_ => {}` arm.
- Targets come from T37's hit map.
