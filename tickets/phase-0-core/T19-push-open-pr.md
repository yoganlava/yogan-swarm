- [ ] **T19 Push + open PR**

Plan: Gate, review and PRs › PR drafts (Approval); Gate, review and PRs (Opening the PR step 5, "From there the PR is yours").

`enter` pushes with normal git creds, `gh pr create --draft`, task → `PR open`, slot freed. Runs `[scripts] teardown` via `slot::run_script` before freeing the slot.

Done when: one real PR opened on a scratch repo.
