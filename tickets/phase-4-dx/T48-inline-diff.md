- [x] **T48 Inline diff with findings at their lines**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Screens › C.

In the Diff tab, `j/k` move between files and `enter` or a click shows or hides a file's hunks from `git diff base...HEAD`, coloured by +/−, with a `+++--` bar per file. Findings whose `location` falls in a shown hunk appear under that line (`◆ critic, fixed  retry.rs:43 …`). Clicking a finding's location on the Findings tab opens Diff with that file unfolded. `d` still pages the full diff.

Done when: snapshot of an expanded file with a finding under its line; unit test that clicking a finding's location switches to Diff with its file open.

## Current code

- `diff_tab` (`src/tui.rs:2686`) draws the diffstat from `diffstat` (~L1751); `d` pages `git diff` with the TUI suspended.
- With T35, `enter` in VS Code opens `code --diff`; this inline view is the non-VS Code behaviour, or both on separate keys if that reads better.
