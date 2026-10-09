- [ ] **T10 Detached worker**

`yogan worker <id>` spawned with setsid; Claude stdout piped through redactor to `logs/<id>.jsonl`; pid and session recorded; kills whole process group.

Done when: closing the spawning terminal leaves the worker running.
