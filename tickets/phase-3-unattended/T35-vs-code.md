- [ ] **T35 VS Code**

Plan: Running in VS Code; TUI › Screens › 11. Compact layout in VS Code.

`TERM_PROGRAM=vscode`: `code -g`, `code --diff`, `code --wait`, `code -n`; compact layout below 24 rows; mouse; bell + `on_review_ready`.

Done when: snapshot of compact layout.

## From the plan

*yogan's normal home is VS Code's integrated terminal. It detects it through `TERM_PROGRAM=vscode` and adapts how it opens files, shows diffs, edits tasks and handles keys.*

| Concern | In VS Code | Elsewhere |
|---|---|---|
| Open a file (`o`) | `code -g path:line` in the current window. Paths are printed relative to the repo, so VS Code's link detection also makes them clickable | `$EDITOR +line path` |
| Diffs | Enter on a file in the Diff tab opens VS Code's diff editor via `code --diff`, with the base version written to a temp file | `git diff` in `$PAGER` |
| Edit a task (`e`) | `code --wait` on the task TOML; the TUI stays up showing "editing in VS Code" until the tab closes | `$EDITOR`, TUI suspended |
| Whole worktree (`o` with no path selected) | `code -n` opens the slot in a new window | Prints the path |
| Submit in compose | Tab to Submit, then enter. ctrl+enter works only where the terminal reports it | ctrl+enter where the terminal reports it |
| Notifications | Terminal bell, shown on VS Code's terminal tab, plus `on_review_ready` | `on_review_ready` |
| Copy (`y`) | OSC 52, falling back to the native clipboard | Same |

- Keys: `o`: *Open the selected path:line in the editor (VS Code: code -g); with no path selected, open the task's worktree (VS Code: new window).* Any task.
- *Your keybindings.json already sends shift+enter as ESC followed by Enter, which yogan reads as Alt+Enter; in the compose screen both mean newline.*
- **Short panels.** *The terminal panel is often 15–20 rows. Below 24 rows yogan switches to a compact layout: header and footer merge into one line and the detail tabs collapse to the active one.*
- Screen 11: *Below 24 rows, the header and footer merge into one line, status group headings give way to glyphs, and the detail tabs collapse to the active one (`Activity ▾`; `1–6` still switch). Both panes stay visible, so a short panel under your editor still shows what every agent is doing.*
- **Theme.** *In VS Code yogan uses the 16 ANSI colours, which follow the active VS Code theme. Truecolor is used only for the selection tint.*
- **Contrast.** *VS Code's minimumContrastRatio brightens dim text, so status never relies on dimness alone; glyphs and position carry it too.*
- **Mouse.** *Click selects a task and the wheel scrolls panes. While yogan captures the mouse, use VS Code's force-selection modifier to select text, or set `mouse = false` in config.*
- Unattended safety, "Review ready" row: *The `on_review_ready` command if set, after a terminal bell.*
- Config:

```toml
[tui]
mouse = true

[notify]
on_review_ready = ""     # command to run; empty means terminal bell only
```

## Current code

- `[tui]` and `[notify]` are in `src/defaults.toml`, but `config::Config` (`src/config.rs`) has no structs for them yet.
- `src/tui.rs`:
  - `Theme::detect` (~L86) reads `COLORTERM` and `YOGAN_ASCII`.
  - `draw` (~L1073) splits header, body and footer, and goes to one pane below 100 columns. `header_line` (~L1261), `list` (~L1288) and `detail` (~L1426) draw the parts.
  - `editor` (~L386) runs `$EDITOR` with the TUI suspended (`app.edit` in the `run` loop, ~L300), and `d` pages `git diff` (`app.pager`).
  - Copy is `copy` (~L1580, OSC 52).
  - Nothing handles the mouse yet; `run` reads only `Event::Key`.
- Tasks reach `Review` in `worker::lifecycle` (`src/worker.rs`), a detached process, so `on_review_ready` can run there even with the TUI closed. The bell needs the TUI: `App::reload` (~L447) can tell when a task newly entered Review.
- There's no `o` key yet. The Findings tab's `location` (`path:line`) and the Questions answer's cited paths (`cites`, ~L1565) are the selectable paths today.
- Snapshot tests: see `main_screen` and `narrow_ascii_one_pane` in `src/tui.rs` for a whole-screen TestBackend snapshot at a given size.
