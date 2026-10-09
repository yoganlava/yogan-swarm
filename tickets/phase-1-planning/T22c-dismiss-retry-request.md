- [x] **T22c Dismiss or retry a failed request**

Plan: Lead agent; TUI › Keys (`x` discard after a confirm, Any).

A failed request stays under Planning until something clears it. On a selected failed request, `x` dismisses it after the usual confirm, and a retry key reruns its lead: resumes the session when the request has one, otherwise plans the request afresh.

Done when: a dismissed request leaves the list; a retried request goes back to planning and its lead files proposals.
