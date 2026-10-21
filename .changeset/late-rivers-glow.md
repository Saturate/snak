---
default: patch
---

#### Windows and Linux release builds

The release workflow now builds and uploads `Snak-windows-x64.zip` and `Snak-linux-x64.zip` next to the macOS bundles, plain binaries for now. CI builds both (so a missing system library or link error fails there, not mid-release), keeping both compiling between releases so the next version cuts with all three platforms in it.

On Windows and Linux, settings, the token file and logs still live under a macOS-style `~/Library/...` path in the home directory for now.
