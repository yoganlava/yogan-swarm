- [ ] **T41 Verdict and Next at the top of Summary**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › A · Main screen (detail pane); problem 4.

Above the tabs: a lifecycle strip (`plan › queue › run › check › review › pr`, current stage bold), a verdict line (gate passed/total, critic open/fixed, `+a −d in N files`, slot, model, spend) and `Next` with the one or two keys that move the task on.

Done when: snapshots for a Review task that's ready, one with a failed gate, and a Proposed task.

## Current code

- `summary` (`src/tui.rs:1680`) joins metadata with " · ". Gate, findings and diffstat are loaded only for tabs 2+ in `load_tab` (~L779); the verdict needs them on Summary too.
