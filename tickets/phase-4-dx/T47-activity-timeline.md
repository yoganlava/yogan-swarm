- [ ] **T47 Activity timeline**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Screens › B.

- The agent's text under the calls it explains, dim with a `│` gutter.
- `✓`/`✗ exit N` on each tool call from its result's `is_error`.
- `+a −d` on edits, from the input's old and new strings.
- In the verdict (T43) for a running task: a context gauge (latest `Usage::context()` over `[watch] autocompact`) and the last event's age (log mtime).

No time column: the stream has no per-event timestamps.

Done when: snapshot of Activity with text, a failed and a passed call, an edit count and the gauge.

## Current code

- `activity` (`src/tui.rs:1683`) keeps only `ToolUse` and nudge/handoff lines; `activity_tab` (~L2608) already follows the tail.
- `Content::Text` and `Event::User` (tool results) are in `src/stream.rs:11–18`; `Usage::context` is ~L88.
- The worker already compares context to `autocompact` (`src/worker.rs:597`).
