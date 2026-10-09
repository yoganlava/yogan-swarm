- [ ] **T45 Right-click actions menu**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Screens › D.

Right-clicking a row selects it and opens a popup at the cursor with every key valid for that task (more than the footer's six), then `z zoom` and `] next for you`. Destructive items are red and go through the confirm modal. A click runs an item; `esc` or a click outside closes the menu. Any other key closes it and then acts as usual.

Done when: snapshot of the menu over a Failed task; unit test that right-click then a click on an item does what pressing its key does.

## Current code

- `App::mouse` (`src/tui.rs:1435`) handles only left `Down` and the wheel. Items come from the same per-state key list as T36's footer.
