- [x] **T36 The footer fits, as key buttons**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Try it (footer); Visual language › Buttons and keys.

Up to six keys per state, in priority order (the key that moves the task on first), then `? more`. Each key is a chip: the key bold in the accent colour on a key background, then its label, dimmed. The palette (T49) lists every key.

Done when: footer snapshots at 80 and 100 columns for Review, Failed, Proposed and a question, none clipped.

## Current code

- `KEYS` (`src/tui.rs:52`) has 22 entries; `draw` filters them per selection (~L1839). At 100 columns the `main_screen` snapshot clips.
- `notice`/`info` replace the keys (~L1870); T38 moves them out.
