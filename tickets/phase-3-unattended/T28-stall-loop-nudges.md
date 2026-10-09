- [ ] **T28 Stall + loop nudges**

Plan: Unattended safety (Stall, Loop rows); Unattended safety › Nudges and handoffs; TUI › Screens › 3. Running.

Stall = silent stream and idle process tree (sysinfo), permit wait excluded; loop = same call `loop_repeats` times; SIGTERM → SIGKILL pgroup, resume with a specific nudge; second stall → `Failed`.

Done when: tests for both detectors.
