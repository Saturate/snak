---
default: patch
---

#### Distrusted certificates rejected

TLS certificate checks pick up rustls-webpki 0.103.16 and webpki-root-certs 1.0.10, which stop trusting the distrusted emSign intermediates (RUSTSEC-2026-0382, RUSTSEC-2026-0383).
