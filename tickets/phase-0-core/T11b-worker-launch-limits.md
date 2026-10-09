- [ ] **T11b Worker launch limits and stderr**

Plan: Workers (launch command: `pre_exec` setrlimit, `.stderr(err_file)`); Workers › Slot scripts and ports (`[cargo] nofile`).

Before `setsid`, raise `RLIMIT_NOFILE` to `[cargo] nofile` (launchd's default is 256). Claude's stderr goes to a file in `logs/` instead of being dropped.

Done when: a worker's Claude child reports the configured `ulimit -n`; a Claude startup error shows in its stderr log.
