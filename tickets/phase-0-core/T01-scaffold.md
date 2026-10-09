- [x] **T01 Scaffold**

Plan: Dependencies and milestones (crate list); Architecture › Subcommands; Open questions and assumptions › Assumptions (binary name).

Crate `yogan-swarm`, `[[bin]] yogan`, clap + anyhow only; every other crate is added by the ticket that first uses it (cargo warns on unused deps). clap: no args → TUI, `worker <id>`, `task propose`.

Done when: `yogan --help` lists both subcommands.
