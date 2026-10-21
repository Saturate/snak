---
default: patch
---

#### Throttled requests retried

Requests Microsoft throttles with HTTP 429 now wait for the time it asks for and retry, instead of failing the action or leaving the sidebar empty.
