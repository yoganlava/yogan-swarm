- [x] **T42 Inbox with reason chips**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Try it (Inbox, header); Visual language › Reason chip, Slot meter.

Three groups replace the seven in `GROUPS`; the status still shows as the row's glyph.

- **Needs you**: Review, Failed, Proposed, answered questions and failed requests. The reason is a coloured chip on the right: green `ready`, red `gate failed` / `failed`, amber `disputed`, `N to approve`, `answered`.
- **Working**: Running, Checking and planning requests.
- **Later**: Queued and PR open, with a dim reason (`needs a slot`, `PR #412`).

Group headings read `▾ NEEDS YOU 3 ───`; clicking one folds or unfolds it for the session.

The header leads with a `● N need you` chip (click sends `]`, T44), then the working and queued counts, a slot meter (`slots ▰▰▱`, one cell per `[worker] slots`, coloured by its task's status; click selects that task, hover names it) and the spend on the right. ASCII: `[ready]`, `#`/`.`.

Done when: `main_screen` snapshot shows the three groups with chips, the header chip and the slot meter; proposal trees still read as trees; unit tests that clicking a heading folds it and clicking a slot selects its task.

## Current code

- `GROUPS` (`src/tui.rs:35`) orders the list; `reload` (~L743) sorts by it, and `list` (~L2103) draws requests then tasks. `header_line` is ~L2076.
- T35's compact layout swaps group headings for glyphs; with three groups that becomes three glyphs.
