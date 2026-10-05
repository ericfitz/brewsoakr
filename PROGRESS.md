# Progress (pushed work)

## 2026-10-05: tap soaking and the NO_SOAK list (unreleased; Cargo version still 1.1.1)

- Third-party taps are soaked from brewsoak's own blobless clones under the
  cache; optional per-tap soak hours via `[[TAP]]` in config.toml.
- `NO_SOAK` (`pkg` | `tap` | `tap/pkg`) hands packages to `brew update` +
  `brew upgrade`, so they end in the same state as a plain `brew upgrade`.
- Staged tap installs record their origin in `origins.toml`; cross-origin
  dependency closure; per-tap failure isolation.
- Fixed a v1 bug: installed casks were invisible (Homebrew >= 4 stores cask
  receipts as JSON). Bare upgrade/outdated now leave `auto_updates` casks alone.
- Spec: docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md.
  Plan: docs/superpowers/plans/2026-10-05-tap-soak-and-no-soak.md.

Not yet done: manual real-brew checks 4-6 (plan Task 13), version bump and
release.
