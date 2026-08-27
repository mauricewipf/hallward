---
name: release
description: >-
  Bumps hallward's Cargo.toml version, commits, tags vX.Y.Z, and pushes
  commit plus tag to origin so GitHub Actions publishes binaries. Use when
  the user says release, release new version, ship, or publish a GitHub
  Release. Do not use when only discussing versioning or CI.
---

# Release hallward

Saying **release** is explicit permission to commit and push for this task only (overrides the usual ask-first git rule). Do not `--amend`, `--force`, or skip hooks. Do not `gh release create`; the tag push starts Actions.

Always **bump**. Never tag the current `Cargo.toml` version as-is. Default **patch** unless the user says `minor`, `major`, or an exact *new* `X.Y.Z`.

## Version bump

Read the current version from the `[package]` block in `Cargo.toml` (`version = "X.Y.Z"`). Compute the next version:

| Spec | From `0.1.0` |
|---|---|
| `patch` (default) | `0.1.1` |
| `minor` | `0.2.0` |
| `major` | `1.0.0` |
| exact `X.Y.Z` | that version, but only if it is **newer** than current |

Abort if the new version equals the current one.

Edit **exactly two places**, both to the same new version:

1. `Cargo.toml`: the package line `version = "…"` (the first `version =` under `[package]`, not a dependency).
2. `Cargo.lock`: the `[[package]]` whose `name = "hallward"` — only that block’s `version = "…"`. Do not change other packages.

Do not run a bump script. Do not rewrite the rest of either file.

## Steps

1. Abort if the working tree is dirty (`git status --porcelain`).
2. Abort unless `HEAD` is `master` and it is in sync with `origin/master` (fast-forward only: `git fetch origin` then `git rev-parse HEAD` equals `origin/master`, or HEAD is an ancestor you can push without rewriting).
3. Abort unless `.github/workflows/release.yml` exists on this commit.
4. Choose spec and bump as above. Abort if `v$new_version` already exists locally or on `origin` (`git rev-parse "v$new_version"` / `git ls-remote --tags origin "v$new_version"`).
5. `cargo test --locked` (must pass; a missed `Cargo.lock` bump fails here). Confirm `cargo run --quiet -- --version` prints `hallward $new_version`.
6. Stage **only** `Cargo.toml` and `Cargo.lock`. Commit:

```bash
git commit -m "$(cat <<EOF
Release $new_version

EOF
)"
```

Do not commit `medialibrary/` or `.album/`.
7. `git tag "v$new_version"` on **that** commit (tag after commit).
8. Dry-run (if asked) stops here. Otherwise:

```bash
git push origin HEAD
git push origin "v$new_version"
```

If push is denied, stop and say so. Do not pretend a Release exists.
9. Print:
   - `https://github.com/mauricewipf/hallward/actions`
   - `https://github.com/mauricewipf/hallward/releases/tag/v$new_version`
   - `https://github.com/mauricewipf/homebrew-hallward` (tap; CI bumps the formula when `HOMEBREW_TAP_TOKEN` is set on the hallward repo)

Do **not** edit `Formula/hallward.rb` by hand on release; CI updates version, URLs, and sha256. If the homebrew job failed, use the manual bump steps in the tap README.
