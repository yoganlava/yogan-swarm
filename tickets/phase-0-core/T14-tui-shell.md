- [ ] **T14 TUI shell**

Plan: TUI (Visual principles, Widgets, Keys, Rendering and layout, Screens › 12. Help overlay); Architecture (restart PID check).

ratatui two panes, task list grouped by status with step + elapsed, header counts, context footer, `?` help, `Theme` struct, 250 ms file poll, PID liveness on start, one pane below 100 cols, ASCII fallback.

Done when: `TestBackend` snapshot test of the main screen.
