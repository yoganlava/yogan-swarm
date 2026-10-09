- [x] **T11c Validate effort at launch**

Plan: Configuration ("An invalid value fails at launch rather than silently").

Claude 2.1.295 only warns on an unknown `--effort` ("ignoring it and using the default effort") and runs anyway, so the worker checks the resolved effort against `low|medium|high|xhigh|max` before spawning Claude; anything else fails the task naming the value.

Done when: a task with `effort = "bogus"` is `Failed` without Claude starting.
