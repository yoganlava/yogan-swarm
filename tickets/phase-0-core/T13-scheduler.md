- [ ] **T13 Scheduler**

Plan: Task lifecycle › Scheduling rules; Architecture ("The worker owns a task's whole lifecycle").

`sched.lock`; ready = `Approved` and parent at least `PR open`; concurrency limit; worker calls it on exit.

Done when: unit test of readiness and limit; queue drains with TUI closed.
