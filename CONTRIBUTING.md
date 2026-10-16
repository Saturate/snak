# Contributing

1. Fork and branch from `main`.
2. `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. Add a changeset (see below)

You can work on the UI without a Teams account: `cargo run -p snak-app -- --demo` uses an in-memory workspace. Keep Teams-specific code in `crates/snak-teams`; the app only talks to the `snak-chat` traits.

## Changesets

Every PR that changes behavior needs a changeset file in `.changeset/`. Create a markdown file with this format:

```markdown
---
default: patch
---

#### Short title

What changed and why.
```

Use `patch` for fixes, `minor` for new features. The package name is always `default`. [changesetter](https://github.com/Saturate/changesetter) consumes these at release time to bump the version and generate the changelog; `changesetter add` writes one for you.

For internal changes that don't affect users (CI, tooling, docs), use `none` instead of `patch`/`minor`.

CI will block the PR if the changeset is missing.

## Releases

Merging to `main` updates a "Next Release" PR. Merging that PR bumps the version, writes `CHANGELOG.md`, tags `vX.Y.Z` and attaches `Snak.app` builds for Apple Silicon and Intel to the GitHub release.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org): `feat(scope):`, `fix(scope):`, `docs:`, `chore:`, etc. Scope is optional but appreciated.

## Security

Message HTML is written by other people. Anything that fetches, opens or renders from it goes through an allowlist: see `classify_media_url` and `is_safe_link`. Never log tokens or message text.
