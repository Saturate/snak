# Snak

A fast, minimal Microsoft Teams chat client with Slack's layout, built on [GPUI Kit](https://github.com/longbridge/gpui-kit). Teams stays installed for calls and meetings; Snak is where you read and write.

Snak talks to Teams' internal APIs using Teams' own client ID, the same approach as [Squads](https://github.com/IanTerzo/Squads). Those APIs are undocumented, so Snak can break whenever Microsoft changes them. It's early and moves fast; expect rough edges.

## Run

```sh
cargo run -p snak-app             # Microsoft Teams
cargo run -p snak-app -- --demo   # fake workspace, no account needed
```

To get a proper app with the Dock icon, build the bundle:

```sh
./scripts/bundle.sh --open            # target/Snak.app, debug build
./scripts/bundle.sh --release --open  # optimised
```

The icon is `assets/icon.svg`; the bundle script renders it with `tools/make-icon`. Snak needs Rust 1.88 or newer and macOS 13 or newer.

Sign in with the device code shown in the window. Where the login is kept is a setting (⌘, → Account): the macOS Keychain (default), 1Password via the `op` CLI, or a plain 0600 file. Logs go to `~/Library/Logs/Snak/snak.log`; set `SNAK_LOG=debug` for more.

### Keychain prompts during development

Dev builds are ad-hoc signed with a new identity on every build, so the Keychain asks again after each rebuild and "Always Allow" never sticks. Run this once:

```sh
./scripts/dev-cert.sh
```

It creates a self-signed "Snak Development" code-signing identity, and `cargo run` (via the runner in `.cargo/config.toml`) then signs each build as `dev.snak.app`, which the Keychain recognises across rebuilds. Test binaries are left unsigned. The script's header explains the trade-off: other programs running as you could use the same identity.

## Layout

| Crate | What | License |
|---|---|---|
| `crates/snak-chat` | Backend-neutral chat model and the `ChatBackend` / `Connector` traits | GPL-3.0-or-later |
| `crates/snak-app` | GPUI Kit desktop app; depends only on `snak-chat` (plus `main.rs` choosing a backend) | GPL-3.0-or-later |
| `crates/snak-teams` | Teams backend: login, internal API client, Trouter push | MPL-2.0 (derived from Squads) |
| `crates/snak-demo` | In-memory demo workspace for UI work without an account | GPL-3.0-or-later |
| `tools/make-icon` | Renders the app icon; dev tool, not shipped | GPL-3.0-or-later |

Settings live in `~/Library/Application Support/dev.snak.app/settings.json`; sidebar layout (stars, sections, hidden, muted, collapsed) in `prefs-<backend>.json` next to it.

## Keys

| Key | Action |
|---|---|
| ⌘K | Jump to a channel or chat |
| ⌘⇧P | Command palette |
| ⌘, | Settings |
| ⌘⇧A | All unreads |
| ⌥↑ / ⌥↓ | Previous / next conversation |
| Enter / ⇧Enter | Send / new line |
| Esc | Close thread |
| Right-click a sidebar item or team | Star, mute, hide, move to section |

## Checks

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Credits

- [Squads](https://github.com/IanTerzo/Squads) by IanTerzo, the minimalist Teams client Snak grew out of. The login flow, the Teams API calls and the Trouter push connection are derived from it (MPL-2.0), and it's where to look for how the Teams internals work.
- [ost](https://github.com/eisbaw/ost) (MIT), a reference for Trouter and Teams calling.
- [Zed](https://zed.dev) for the look, the theme format and the One themes; [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) and [GPUI Kit](https://github.com/longbridge/gpui-kit) for the UI.
- Bundled themes and their licenses: `crates/snak-app/assets/themes/LICENSES.md`.

## License

GPL-3.0-or-later (see `LICENSE`), except `crates/snak-teams`, which is MPL-2.0.
