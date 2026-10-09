- [ ] **T44 Activity timeline**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › B · Running; problem 6. The plan's screen 3 gauge and last-event age.

- The agent's text under the calls it explains, dim with a `│` gutter.
- `✓`/`✗` on each tool call from its result's `is_error`.
- `+a −d` on edits, from the input's old and new strings.
- In the tab's header: a context gauge (latest `Usage::context()` over `[watch] autocompact`) and the last event's age (log mtime).

No time column: the stream has no per-event timestamps.

Done when: snapshot of Activity with text, a failed and a passed call, an edit count and the gauge.

## Current code

- `activity` (`src/tui.rs:1168`) keeps only `ToolUse` and nudge/handoff lines. `Content::Text` and `Event::User` (tool results) are in `src/stream.rs:11–18`; `Usage::context` is ~L88.
- The worker already compares context to `autocompact` (`src/worker.rs:597`).
