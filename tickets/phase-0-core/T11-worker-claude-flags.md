- [ ] **T11 Worker Claude flags**

`--model/--effort`, `--allowedTools`, built-in + project deny list, `--setting-sources project`, `--strict-mcp-config`, strip `CLAUDE_CODE_EFFORT_LEVEL`, cwd = slot, worker rules in appended system prompt. Init check stops the worker on unexpected MCP servers/tools; non-empty `permission_denials` = incomplete.

Done when: a worker on a trivial task commits in its slot; `--resume` from the slot works (plan open question).
