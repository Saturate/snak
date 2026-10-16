# Snak

A fast, minimal Microsoft Teams chat client with Slack's layout, built on [GPUI Kit](https://github.com/longbridge/gpui-kit). Teams stays installed for calls and meetings; Snak is where you read and write.

Private experiment. Talks to Teams' internal APIs using Teams' own client ID, the same approach as [Squads](https://github.com/IanTerzo/Squads), so it can break whenever Microsoft changes them.

## Run

```sh
cargo run -p snak-app             # Microsoft Teams
cargo run -p snak-app -- --demo   # fake workspace, no account needed
```

Sign in with the device code shown in the window. Where the login is kept is a setting (⌘, → Account): the macOS Keychain (default), 1Password via the `op` CLI, or a plain 0600 file.

### Keychain prompts during development

Dev builds are ad-hoc signed with a new identity on every build, so the Keychain asks again after each rebuild and "Always Allow" never sticks. Run this once:

```sh
./scripts/dev-cert.sh
```

It creates a self-signed "Snak Development" code-signing identity, and `cargo run` (via `.cargo/config.toml`) then signs each build as `dev.snak.app`, which the Keychain recognises across rebuilds.

## Layout

| Crate | What | License |
|---|---|---|
| `crates/snak-chat` | Backend-neutral chat model and the `ChatBackend` / `Connector` traits | |
| `crates/snak-app` | GPUI Kit desktop app; depends only on `snak-chat` (plus `main.rs` choosing a backend) | |
| `crates/snak-teams` | Teams backend: login, internal API client, Trouter push | MPL-2.0 (derived from Squads) |
| `crates/snak-demo` | In-memory demo workspace for UI work without an account | |

Sidebar settings (stars, sections, hidden, muted) live in `~/Library/Application Support/dev.snak.app/prefs-<backend>.json`.

## Keys

| Key | Action |
|---|---|
| ⌘K | Jump to a channel or chat |
| ⌥↑ / ⌥↓ | Previous / next conversation |
| Enter / ⇧Enter | Send / new line |
| Esc | Close thread |
| Right-click a sidebar item | Star, mute, hide, move to section |

## Checks

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
