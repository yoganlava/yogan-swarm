- [ ] **T22b Request status in the TUI**

Plan: TUI › Screens › 1. Compose ("the request shows up under Planning"), 2. Planning.

A request whose lead is planning or failed shows in the list under Planning, with the lead's error in the detail pane; a dead lead (`yogan lead` exited without recording an outcome) is marked failed, like `reap` does for workers.

Done when: snapshot with a planning and a failed request; a failed lead's error is visible without opening `requests/<id>.toml`.
