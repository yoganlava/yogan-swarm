- [ ] **T05 Stream parser**

Plan: Workers ("The stream is parsed loosely with serde"); Unattended safety › Nudges and handoffs › Handoff (usage fields).

`#[serde(tag = "type")]` over system/assistant/user/rate_limit_event/result + `#[serde(other)]`. Extract session_id, model, tools, cost, usage, permission_denials.

Done when: fixture tests from a real `claude -p --output-format stream-json` run.
