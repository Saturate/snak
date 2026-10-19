---
default: patch
---

#### Windows and Linux release builds

The release workflow now builds and uploads `Snak-windows-x64.zip` and `Snak-linux-x64.zip` next to the macOS bundles, plain binaries for now. CI also runs `cargo check` on both platforms, so Windows and Linux keep compiling between releases and the next version cuts with all three platforms in it.
