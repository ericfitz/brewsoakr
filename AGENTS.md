# brewsoak

## Installing soaked formulae

- Install from a staged `.rb` path under the brewsoak cache, not `brewsoakr/soaked/<name>`.
- Homebrew rejects path installs unless `HOMEBREW_DEVELOPER=1` is set and `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` is unset. Set those on every brewsoak `brew` child. Also set `HOMEBREW_NO_AUTO_UPDATE=1` and `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1`.
- Never pass `--ignore-dependencies`. Homebrew treats it as an unsupported developer option and warns even in developer mode. Install the cutoff dep closure first, then install the target and let brew treat already-installed deps as satisfied.
- Cellar receipts omit `bottle`/`rebuild`. Do not treat a missing rebuild as 0 when comparing installed vs cutoff.
- `-v`/`--verbose` prints the soak window, cutoffs, and a line for every package evaluated. Bare `-v` is brewsoak help, not `brew -v`.
- Persist `--soak-hours` only on soaked commands, never on passthrough/`--version`/`--help`.
- Do not uninstall a `homebrew/core` keg to switch taps.

## Origins and staging

- A package's origin is its receipt tap when it is non-empty and not `brewsoakr/soaked`: for a formula, `source.tap` in the keg's `INSTALL_RECEIPT.json` (never `brew info`'s top-level `tap`, which is where brew resolves the name now and says `homebrew/core` for a staged tap keg that shares a core name); for a cask, `tap` from `brew info --json=v2 --installed`; else `<cache>/origins.toml` (`"formula:<name>" = "user/repo"`); else `homebrew/core` / `homebrew/cask`. A staged install leaves the receipt tap null, so write the origin record after every successful staged tap install and remove it after a staged core/cask install.
- Tap packages are staged under `<cache>/staging/taps/<user>/<repo>/{Formula,Casks}/<name>.rb`; core and cask stay directly under `<cache>/staging/`. Never let a tap formula land in the core staging root.
- Tap history lives in `<cache>/taps/<user>/<repo>.git`: a bare clone with a named `origin` remote, fetched `--filter=blob:none` (retry without the filter if the server rejects it). Never fetch into, or move the checkout of, anything under `$(brew --repository)/Library/Taps`. No GitHub API calls for taps.
- `NO_SOAK` packages are never staged. They go to brew as full tokens (`user/repo/name` for tap packages) in one `brew update` + one `brew upgrade|install|reinstall` after all soaked work. A no-soak package whose keg was staged by brewsoak is moved to its real tap with `brew install user/repo/name`.
- `brew tap-info --json --installed` reports `remote: null` for API-mode `homebrew/core` and `homebrew/cask` and for the staging tap; classify those by name before applying the "no HTTPS remote = unsoakable" rule.
- A tap whose fetch fails holds only that tap's packages (`Error::Git` note). Core or cask fetch failure still aborts the run.
- Config: top-level keys (`SOAK_HOURS`, `NO_SOAK`) must precede the first `[[TAP]]` table in every example; a `NO_SOAK` inside `[[TAP]]` is ignored with a stderr warning.

## Output

- Every byte of `brew` output goes to the per-run log under `$TMPDIR`; the terminal gets a summary. Never suppress a line without logging it.
- Visible `brew` runs share one pipe for stdout and stderr so the summarizer sees them in the order brew wrote them. `Output.stderr` is empty for visible runs; callers must read `Output.stdout`.
- Unrecognized brew lines pass through verbatim (minus the `==>` marker). The filter is a summarizer, not an allowlist: never swallow the unknown.
- `--raw` must always be able to turn the summarizer off.
- The installed snapshot is taken once and goes stale as soon as brew upgrades a dependency for us. Compare against what brew reported installing (`quiet::installed_from_output`) before spawning another `brew install`.

## Git

- Every git invocation goes through `ProcessGit` / `GitStore` in `src/git.rs`. Do not spawn `git` from other modules.
- Soak refs (`refs/brewsoak/cutoff`, `refs/brewsoak/head`, `refs/brewsoak/window`) are pins, not branches. Updates must force (`+sha:ref` and `git fetch --force`). Never assume a fast-forward.
- Trap every git failure. Surface `Error::Git` with:
  - `action`: what brewsoak was doing, in our words
  - `detail`: git's stderr (or why git could not start)
- Do not leak a raw git message as the only user-visible text.
