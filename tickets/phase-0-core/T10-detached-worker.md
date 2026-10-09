- [ ] **T10 Detached worker**

Plan: Architecture (Subcommands, "The worker owns a task's whole lifecycle", Redaction); Running in VS Code › Surviving the terminal.

`yogan worker <id>` spawned with setsid; Claude stdout piped through redactor to `logs/<id>.jsonl`; pid and session recorded; kills whole process group.

Done when: closing the spawning terminal leaves the worker running.
