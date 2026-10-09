- [ ] **T39 Keys don't depend on pane focus**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › Keys (`r`, `x`); problem 5.

On the Findings tab, `j/k`, `r` and `x` act on findings whichever pane has focus; elsewhere they act on the task. Footer labels name the target: `x discard task`, `x waive finding`, `r reply to worker`, `r reply to lead`.

Done when: test that `x` on the Findings tab with the list focused opens the waive modal, not the discard confirm; footer snapshots show the named labels.

## Current code

- `on_findings` requires `self.detail` (`src/tui.rs:525`); `x` falls through to discard at ~L698. `r` branches at ~L666 and ~L737–741.
