---
default: minor
---

#### Older messages load when you scroll up

Conversations now serve messages in pages with a cursor for the page before them, so scrolling up in the timeline brings in older history instead of stopping at whatever the first fetch loaded. Chats follow the `_metadata.backwardLink` Teams returns with each page (only when it points back at the chat service), channels page through `continuationToken`, and the demo backend pages 20 at a time over seeded backlog, so the behavior is testable offline.
