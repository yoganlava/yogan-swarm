- [x] **T12b Gate `when_changed` and SHAs**

Plan: Workers › Slot scripts and ports (the `[gate]` example's `migration` step).

A step's `when_changed = ["db-sys/migrations/**"]` runs it only when a changed path matches one of the globs; every step gets `BASE_SHA` (merge base) and `HEAD_SHA`. Until then a `when_changed` step runs on every gate, since unknown keys are ignored.

Done when: test that a `when_changed` step runs only for matching paths and sees both SHAs.
