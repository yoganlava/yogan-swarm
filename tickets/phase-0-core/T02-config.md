- [x] **T02 Config**

Plan: Configuration (config example, precedence); Workers › Slot scripts and ports (project file example).

Global `~/.config/yogan/config.toml`, `[projects]` map, project found by origin repo name (fallback: checkout path), project file overrides global tables key by key, built-in defaults. Precedence: project > global > default; task and compose overrides are T11.

Done when: unit tests cover project lookup and precedence.
