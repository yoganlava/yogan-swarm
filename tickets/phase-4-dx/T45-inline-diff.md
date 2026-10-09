- [ ] **T45 Inline diff with findings at their lines**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › C · Review; problem 7.

In the Diff tab, `j/k` move between files and `enter` shows or hides a file's hunks from `git diff base...HEAD`, coloured by +/−. Findings whose `location` falls in a shown hunk appear under that line. `d` still pages the full diff.

Done when: snapshot of an expanded file with a finding under its line.

## Current code

- `diff_tab` (`src/tui.rs:1997`) draws the diffstat from `diffstat` (~L1234); `d` pages `git diff` with the TUI suspended (~L310).
- With T35, `enter` in VS Code opens `code --diff`; this ticket's inline view is the non-VS Code behaviour, or both on separate keys if that reads better.
