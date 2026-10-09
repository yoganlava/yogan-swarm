- [ ] **T40 Inbox replaces the status groups**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › A · Main screen; problem 3.

Three groups replace the seven in `GROUPS`; the status still shows as the row's glyph.

- **Needs you**: Review, Failed, Proposed, answered questions and failed requests. Right-hand reason: `ready`, `gate failed`, `disputed`, `N to approve`, `answered`, `failed`.
- **Working**: Running, Checking and planning requests.
- **Later**: Queued and PR open.

The header leads with `● N need you`, then the working and queued counts and today's spend.

Done when: `main_screen` snapshot shows the three groups with reasons; proposal trees still read as trees.

## Current code

- `GROUPS` (`src/tui.rs:33`) orders the list; `reload` sorts by it (~L476), and `list` (~L1497) draws requests then tasks, each under its own heading. `header_line` is ~L1470.
- T35's compact layout swaps group headings for glyphs; with three groups that becomes three glyphs.
