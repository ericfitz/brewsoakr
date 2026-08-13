# brewsoakr

## Installing soaked formulae

- Install from a staged `.rb` path under the brewsoakr cache, not `brewsoakr/soaked/<name>`.
- Homebrew rejects path installs unless `HOMEBREW_DEVELOPER=1` is set and `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` is unset. Set those on every brewsoakr `brew` child. Also set `HOMEBREW_NO_AUTO_UPDATE=1`.
- Do not uninstall a `homebrew/core` keg to switch taps.

## Git

- Every git invocation goes through `ProcessGit` / `GitStore` in `src/git.rs`. Do not spawn `git` from other modules.
- Soak refs (`refs/brewsoak/cutoff`, `refs/brewsoak/head`, `refs/brewsoak/window`) are pins, not branches. Updates must force (`+sha:ref` and `git fetch --force`). Never assume a fast-forward.
- Trap every git failure. Surface `Error::Git` with:
  - `action`: what brewsoakr was doing, in our words
  - `detail`: git's stderr (or why git could not start)
- Do not leak a raw git message as the only user-visible text.
