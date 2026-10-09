- [ ] **T39 Keys don't depend on pane focus**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Mouse map; T36 footer labels.

On the Findings tab, `j/k`, `r` and `x` act on findings whichever pane has focus; elsewhere they act on the task. Footer labels name the target: `x discard task`, `x waive finding`, `r reply to worker`, `r reply to lead`.

Done when: test that `x` on the Findings tab with the list focused opens the waive modal, not the discard confirm; footer snapshots show the named labels.

## Current code

- `on_findings` requires `self.detail` (`src/tui.rs:796`); the `x` and `r` branches are in `App::key` (~L824 on).
