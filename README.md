<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-mark-dark.svg">
    <img src="assets/logo-mark.svg" alt="yogan" width="96" height="96">
  </picture>
</p>

<h1 align="center">yogan</h1>

<p align="center">Parallel Claude Code workers, one PR each</p>

---

yogan is a terminal UI that runs several Claude Code sessions in parallel on one repo. A read-only
lead session turns your request into proposed tasks. You approve them. Each worker then runs
unattended in its own git worktree, passes your gate and an adversarial critic, and waits for you
to review the diff. Nothing is pushed until you approve the pull request.

It is one Rust binary with no daemon, database or web UI. Agents are detached processes that
write to files, so closing the terminal never stops them.

## How it works

```
 request ─► lead ─► proposed tasks ─► you approve ─► worker in slot N ─► gate ─► critic ─► Review ─► you approve PR ─► push + gh pr create
                                                         ▲                           │
                                                         └──── fix round ◄───────────┘
                                                           (gate failure or blocking finding)
```

- **Lead proposes, you approve.** The lead reads the code (no Edit, Write or shell) and files
  tasks with one to five testable acceptance criteria each. Questions get an answer instead.
- **Isolated workers.** Each task runs in a numbered slot: a persistent worktree with its own
  `CARGO_TARGET_DIR`, so incremental builds stay warm between tasks.
- **Your gate, not the agent's word.** After Claude exits, yogan runs your gate steps itself,
  scoped to the crates or paths the diff touched.
- **Adversarial review.** A separate critic session, on a different model by default, tries to
  break the change. Blocker and major findings go back to the worker before you see it.
- **One task, one PR.** On your approval of the drafted title and body, yogan rebases, pushes the
  branch and opens the PR with `gh`. After that the PR is yours.
- **Unattended safety.** Stalls and loops get a nudge, a filling context hands off to a fresh
  session, rate limits park the worker, and a per-task spend ceiling stops runaways. Every failure
  ends in a visible status.

## Requirements

- macOS or Linux (Windows is not supported)
- [Claude Code](https://docs.claude.com/en/docs/claude-code) on `PATH`, signed in
- `git`, and the [GitHub CLI](https://cli.github.com) (`gh`) authenticated for opening PRs
- A Rust toolchain with edition 2024 support (Rust 1.88 or later) to build yogan

## Install

```sh
git clone https://github.com/yoganlava/yogan-swarm.git
cd yogan-swarm
make install    # cargo install --path . --locked --force
```

This installs the `yogan` binary into `~/.cargo/bin`.

## Quick start

```sh
cd path/to/your/repo
yogan
```

1. Press `n`, pick a mode (Auto, Plan or Ask), type a request and submit.
2. Proposals appear under **Needs you**. Press `a` to approve one, `A` for all, `e` to edit or `r`
   to reply to the lead.
3. Approved tasks queue for a free slot and run. Watch them in the Activity tab, or close the
   terminal and come back later.
4. When a task reaches **Review**, read the Summary, Gate, Findings and Diff tabs.
5. Press `m` to draft the PR, then `enter` to push and open it. `esc` backs out without pushing.

Press `?` at any time for the command palette, which lists every key valid for the selection.

## Keys

| Key | Action |
| --- | --- |
| `n` | New request (plan or question) |
| `j` `k` / `↑` `↓` | Move selection |
| `tab`, `1`–`6` | Switch pane, switch detail tab |
| `a` / `A` | Approve a proposal / approve all |
| `e` | Edit a proposal (TOML) or the PR draft |
| `r` | Reply: to the lead, to a task in Review, or a follow-up question |
| `p` | Turn an answer into a plan |
| `m` | Draft the PR (nothing is pushed until `enter`) |
| `M` | Rebase and push the task to main |
| `g` | Regenerate the PR draft with an instruction |
| `d` | Full diff in `$PAGER` |
| `w` | Rewind the slot to one of the worker's commits |
| `t` | Retry a failed task on a model you pick |
| `c` | Continue the session interactively in its slot |
| `R` | Start or stop the slot's run script |
| `o` | Open the selected file, or the task's worktree |
| `x` | Discard the task and free its slot |
| `y` | Copy the answer |
| `z` | Zoom the detail pane |
| `]` | Jump to the next task that needs you |
| `,` | Settings |
| `?` | Command palette |
| `q` | Quit (agents keep running) |

The mouse works too: click selects, double-click zooms, the wheel scrolls and hovering shows the
key a target presses.

## Configuration

yogan writes nothing into your repo. Configuration is layered, highest first:

1. A task's own `model` and `effort`, set with `e` on a proposal
2. The compose screen's model and effort, for that request
3. The project file
4. `~/.config/yogan/config.toml`
5. Built-in defaults ([`src/defaults.toml`](src/defaults.toml))

The project is found by its `origin` remote's repo name, falling back to the checkout's folder
name, in the global file's `[projects]` table. Settings (`,`) edits and saves these files for you;
hand edits work too.

```toml
# ~/.config/yogan/config.toml
branch_prefix = "you/"

[projects]
"my-repo" = "~/.config/yogan/projects/my-repo.toml"
```

A project file can override any table:

```toml
# ~/.config/yogan/projects/my-repo.toml
[worker]
concurrency = 3
slots = 4
budget_usd = 8.0                    # per-task spend ceiling; 0 turns it off
mcp_config = ".mcp.json"            # the only MCP servers workers get
read_tools = ["mcp__graft"]          # extra tools for the lead, Ask and the critic
deny = ["Bash(docker *)"]           # added to the built-in deny list

[pr]
title_format = "type(scope): desc [TICKET]"
types = ["feat", "fix", "refactor", "chore"]

[gate]
steps = [
  { name = "fmt",    run = "cargo fmt {crates} -- --check" },
  { name = "clippy", run = "cargo clippy {crates} --all-targets -- -D warnings" },
  { name = "test",   run = "cargo test {crates}" },
  { name = "sqlx",   run = "cargo sqlx prepare --check", when_changed = ["migrations/**"] },
]

[ports]                             # slot n gets ports base + n * per_slot onwards
base = 20000
per_slot = 10

[scripts]                           # run in the slot; all optional
setup = "./scripts/worktree_up.sh"
teardown = "./scripts/worktree_down.sh"
run = "./scripts/dev.sh"
```

Gate steps take `{crates}` (`-p` flags for touched crates) or `{paths}` (changed top-level
directories), so non-Rust repos work too. A step whose placeholder is empty is skipped. Cargo
features (target dirs, build permits, crate scoping) switch on only when the repo has a
`Cargo.toml`.

Roles that run Claude each have a `model` and `effort`: `[lead]`, `[ask]`, `[critic]`, `[worker]`
and `[pr]`. Thresholds for stalls, loops and handoffs live in `[watch]`; slot cleanup in `[disk]`.
See [`src/defaults.toml`](src/defaults.toml) for every key and its default.

### Slot environment

Slot scripts, Claude, the gate and the critic all run with:

| Variable | Meaning |
| --- | --- |
| `YOGAN_ROOT` | Your main checkout |
| `YOGAN_SLOT`, `YOGAN_SLOT_DIR` | Slot number and worktree path |
| `YOGAN_TASK_ID`, `YOGAN_CRATES` | The current task and its crates |
| `YOGAN_PORT_BASE`, `YOGAN_PORT_COUNT` | This slot's port range, when `[ports]` is set |

### Environment variables

| Variable | Effect |
| --- | --- |
| `YOGAN_ASCII=1` | ASCII glyphs instead of Unicode |
| `YOGAN_DIR` | Override the state directory |
| `COLORTERM=truecolor` | Truecolor theme; otherwise the 16 ANSI colours |

## State

State lives outside the repo, in `~/.local/share/yogan/<repo>/`, so your editor, `rg` and `cargo`
never see the agents' copies:

```
requests/<id>.toml   one per plan or question
tasks/<id>.toml      one per task
logs/<id>.jsonl      Claude's stream, redacted
findings/<id>.toml   the critic's findings
answers/<id>.md      answers to questions
slots/1..N/          persistent worktrees
```

Everything yogan writes passes through a redactor that masks token-like strings. On restart the
TUI rebuilds its view from these files and checks whether each recorded worker is still alive.

## Safety model

Workers run Claude Code headless with an explicit `--allowedTools` list, project settings only
(`--setting-sources project`, so no user hooks or plugins) and `--strict-mcp-config`. A built-in
deny list blocks `git push`, `git checkout`, `git reset`, `git clean`, `git worktree`, `gh` and
similar commands; your project can add more.

This is a rule, not a sandbox. Anything an allowed program runs (`build.rs`, tests, scripts)
executes as you, with your credentials. Run yogan on repos and dependencies you trust.

## VS Code

yogan is built to live in VS Code's integrated terminal. It detects `TERM_PROGRAM=vscode` and opens
files with `code -g`, diffs with `code --diff` and edits with `code --wait`. It uses the 16 ANSI
colours there so it follows your VS Code theme. Below 24 rows it switches to a compact layout that
fits a short panel.

## Development

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

TUI screens are snapshot-tested with ratatui's `TestBackend`. Work is tracked as one file per
ticket under [`tickets/`](tickets), in build order.
