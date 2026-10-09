- [ ] **T05 Stream parser**

`#[serde(tag = "type")]` over system/assistant/user/rate_limit_event/result + `#[serde(other)]`. Extract session_id, model, tools, cost, usage, permission_denials.

Done when: fixture tests from a real `claude -p --output-format stream-json` run.
