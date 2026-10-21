---
default: patch
---

#### Older chats show up again

Chats Teams marks as hidden, which on some tenants is every DM without recent activity, are no longer dropped. The sidebar still shows the most recent ones up to the DM limit, and search finds the rest. Requests Microsoft throttles with HTTP 429 are retried after the wait it asks for.
