- [ ] **T28 Stall + loop nudges**

Stall = silent stream and idle process tree (sysinfo), permit wait excluded; loop = same call `loop_repeats` times; SIGTERM → SIGKILL pgroup, resume with a specific nudge; second stall → `Failed`.

Done when: tests for both detectors.
