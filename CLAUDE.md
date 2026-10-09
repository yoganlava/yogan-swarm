# yogan-swarm

Plan: https://claude.ai/code/artifact/4743097c-83ef-4f61-8694-7eb9cbd8b866 (read it with the docs tools).

## Tickets

One file per ticket in `tickets/phase-<n>-<name>/T<nn>-<slug>.md`, in build order. Each starts with a `- [ ]` checkbox.

- Unless told otherwise, work the next open ticket: `grep -L '^- \[x\]' tickets/*/*.md | sort | head -1`. Read its section of the plan first.
- A ticket is done when its "Done when" holds and `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass.
- Then tick it (`- [ ]` → `- [x]`) in the same commit as the work. Never tick a ticket you didn't verify; if "Done when" needs a manual check you can't run, say so and leave it unticked.
- Work that doesn't fit the ticket becomes a new ticket file in the same phase, not extra code.
