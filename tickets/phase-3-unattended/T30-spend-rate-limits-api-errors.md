- [ ] **T30 Spend, rate limits, API errors**

Plan: Unattended safety (API error, Spend, Usage rows); Workers (`rate_limit_event`); Task lifecycle › Scheduling rules (spend ceiling).

`--max-budget-usd`; usage summed across sessions; park on non-`allowed` rate limit until reset; one auto-resume on API error. Adds `Task.usage`.

Rate-limit `status` can be `allowed_warning` (seen at 92% five-hour utilization on 2.1.295); treat it as allowed, not as a reason to park.

Done when: tests for each transition.
