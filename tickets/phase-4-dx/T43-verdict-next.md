- [x] **T43 Pipeline strip, verdict and NEXT buttons**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Try it (detail pane); Screens › B; Visual language.

Above the tabs, in the detail pane:

- **Pipeline strip**: `plan ━ queue ━ run ━ check ━ ◉ review ┄ pr`. Done stages green, the current one bold in the accent colour (`✗` in red on a failure, the spinner while running), later ones dim.
- **Verdict**: gate dots (`●●●●●● 6/6`), critic open/fixed, `+a −d · N files`, slot and model, and spend as a `LineGauge` against the budget. Running tasks show a context gauge (amber past `handoff_at`) and the quiet time instead.
- **NEXT**: one or two buttons for the keys that move the task on, the first green. Each is a hit target (T37) for its key. `Nothing needed yet.` when nothing is.
- **Tabs as pills**: the active tab on an accent background; badges `✓`/`✗` on Gate, a count on Findings, the spinner on Activity while running. Each pill is a hit target, and the wheel over the tab row cycles tabs.

Below 24 rows the verdict condenses to one line and the tabs collapse to the active pill. ASCII: `=`, `-`, `(*)`, `*****x`.

Done when: snapshots for a ready Review task, a failed gate, a Proposed task and a Running task; unit test that clicking a NEXT button does what pressing its key does.

## Current code

- `summary` (`src/tui.rs:2368`) joins metadata with " · ". Gate, findings and diffstat load only for tabs 2+ in `load_tab` (~L1113); the verdict needs them on Summary too.
- The tab line is drawn in `detail` (~L2263).
