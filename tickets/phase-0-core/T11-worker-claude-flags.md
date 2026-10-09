- [ ] **T11 Worker Claude flags**

Plan: Workers (launch command, deny list, MCP/settings/permissions bullets, worker rules, "What is enforced"); Configuration (`CLAUDE_CODE_EFFORT_LEVEL`); Open questions and assumptions (`--resume` from the slot).

`--model/--effort` resolved task > compose > config (`task.model.or(compose.model).unwrap_or(cfg.model)`), `--allowedTools`, built-in + project deny list, `--setting-sources project`, `--strict-mcp-config`, strip `CLAUDE_CODE_EFFORT_LEVEL`, cwd = slot, `CARGO_TARGET_DIR=<slot>/target` (seeded by T07), worker rules in appended system prompt. Init check stops the worker on unexpected MCP servers/tools; non-empty `permission_denials` = incomplete.

Read-only Bash such as `echo hi` ran with only `--allowedTools Read` (2.1.295), so the allowlist does not gate read-only commands; the deny list does.

Done when: a worker on a trivial task commits in its slot; `--resume` from the slot works (plan open question).
