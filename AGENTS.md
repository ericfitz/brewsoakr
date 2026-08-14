# brewsoakr

## Installing soaked formulae

- Install from a staged `.rb` path under the brewsoakr cache, not `brewsoakr/soaked/<name>`.
- Homebrew rejects path installs unless `HOMEBREW_DEVELOPER=1` is set and `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` is unset. Set those on every brewsoakr `brew` child. Also set `HOMEBREW_NO_AUTO_UPDATE=1` and `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1`.
- Never pass `--ignore-dependencies`. Homebrew treats it as an unsupported developer option and warns even in developer mode. Install the cutoff dep closure first, then install the target and let brew treat already-installed deps as satisfied.
- Cellar receipts omit `bottle`/`rebuild`. Do not treat a missing rebuild as 0 when comparing installed vs cutoff. "Already soaked" is silent unless `-v`/`--verbose`.
- Do not uninstall a `homebrew/core` keg to switch taps.

## Git

- Every git invocation goes through `ProcessGit` / `GitStore` in `src/git.rs`. Do not spawn `git` from other modules.
- Soak refs (`refs/brewsoak/cutoff`, `refs/brewsoak/head`, `refs/brewsoak/window`) are pins, not branches. Updates must force (`+sha:ref` and `git fetch --force`). Never assume a fast-forward.
- Trap every git failure. Surface `Error::Git` with:
  - `action`: what brewsoakr was doing, in our words
  - `detail`: git's stderr (or why git could not start)
- Do not leak a raw git message as the only user-visible text.
