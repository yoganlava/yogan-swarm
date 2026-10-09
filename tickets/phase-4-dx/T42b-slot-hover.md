- [ ] **T42b Hovering a slot names its task**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Mouse map (Slot meter: "Hover names it").

Split out of T42, which landed the slot meter before T40's hover existed. Hovering a cell of the header's slot meter shows `slot 2 · Retry webhook sends` (or `slot 3 · free`) where T40 shows hover hints.

Done when: unit test that a move over a held slot names its task and a move over a free one says `free`.

## Current code

- `header_line` (`src/tui.rs`) registers each held slot as a `Target::Row`; free slots register nothing.
- Needs T40's hover state and hint drawing.
