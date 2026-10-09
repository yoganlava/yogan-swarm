- [x] **T34 Settings**

Plan: TUI › Screens › 10. Settings; Configuration.

`,` opens model/effort per role and watch thresholds; writes project file, `g` global.

Done when: saved file reloads identically.

## From the plan

- Screen 10, Settings: *`,` opens Settings, so configuration never needs a text editor. The Models tab cycles each role's model and effort with the arrow keys, with the watch thresholds underneath. Saving writes this project's file under `~/.config/yogan/projects/`, and `g` switches to the global file. The files remain the source of truth, so hand edits still work.*
- Keys: `,`: *Open Settings: model and effort per role, watch thresholds* (any screen). `g`: *in Settings, switch to the global file.*
- Configuration: *Each role that runs Claude (lead, questions, critic, workers, PR drafts) has its own model and effort.* *Nothing is written into the repo. yogan identifies the project by the repo name of its `origin` remote, falling back to the checkout's folder, and looks it up in `[projects]`. A project file can override any global table.*
- Precedence, highest first: task fields, then the compose screen's fields, the project file, the global `config.toml`, and built-in defaults.
- *Model accepts an alias or a full model name, such as `claude-opus-5-5` or `claude-fable-5-1`; effort accepts whatever levels the installed Claude Code supports, up to `max`. An invalid value fails at launch rather than silently.*
- Global config shape (roles and watch):

```toml
[lead]
model  = "claude-opus-5-5"
effort = "xhigh"

[ask]                    # questions
model  = "claude-opus-5-5"
effort = "high"

[pr]                     # PR drafts
model  = "claude-opus-5-5"
effort = "medium"

[critic]
model  = "claude-fable-5-1"
effort = "max"

[worker]
model  = "claude-opus-5-5"
effort = "high"

[watch]
stall_after  = "15m"
nudges       = 1
loop_repeats = 4
autocompact  = 200000
handoff_at   = 0.8
max_handoffs = 2

[projects]        # repo name or checkout folder -> project file
"fuse-os"       = "~/.config/yogan/projects/fuse-os.toml"
```

## Current code

- `src/config.rs`:
  - `load` reads the defaults (`src/defaults.toml`), then `~/.config/yogan/config.toml`, then the project file, merged key by key (`layered`, `merge`).
  - `find_project` resolves the project file from `[projects]` by the origin repo name (`origin_name`) or the checkout path; `expand` handles `~`; `read_toml` reads a file into a `toml::Table`.
  - Role structs are `Role` (lead), `Ask`, `Pr`, `Critic` and `Worker`; `Watch` holds the thresholds, and `stall_after` is a string like `"15m"` parsed by `duration`.
- Valid efforts: `worker::EFFORTS` and `check_effort` (`src/worker.rs`).
- The TUI loads config on demand with `config::load(&self.repo)` (`src/tui.rs`); `App::sessions` is read once at startup.
- Overlays and modals in `src/tui.rs`: `help`, `preview` and `centered` show how a full-screen or centered view is drawn; key handling for overlays is at the top of `App::key`.

## Notes

- Write only the tables and keys Settings changes into the target file, leaving its other tables (`[scripts]`, `[gate]`, `[ports]` and so on) intact. Read it as a `toml::Table`, set the keys, and write it back.
- If the project has no `[projects]` entry yet, saving has to create the file under `~/.config/yogan/projects/` and add the mapping to the global file. The plan doesn't say this explicitly; it follows from "saving writes this project's file".
- "Reloads identically": after saving, `config::load` gives the same values as shown in Settings.
