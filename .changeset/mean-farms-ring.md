---
default: minor
---

#### Trouter frame shapes at debug level, for fast debugging

Running with `SNAK_LOG=snak_teams=debug` now logs every Trouter frame's shape: resourceTypes with their field names (message text never included), unknown resourceTypes flagged as `UNKNOWN push resource type`, acks, the registrar responses and the socket.io handshake. `cargo run -p snak-teams --example push` runs the push channel against the stored login so a debug log can be captured and shared, and `ThreadActivity/...` markers are ignored rather than landing in the message path.
