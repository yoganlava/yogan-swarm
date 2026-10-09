- [ ] **T30 Spend, rate limits, API errors**

Plan: Unattended safety (API error, Spend, Usage rows); Workers (`rate_limit_event`); Task lifecycle › Scheduling rules (spend ceiling).

`--max-budget-usd`; usage summed across sessions; park on non-`allowed` rate limit until reset; one auto-resume on API error. Adds `Task.usage`.

Done when: tests for each transition.
