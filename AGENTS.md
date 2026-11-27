# brewsoak

## Installing soaked formulae

- Soaked installs are a stopgap for issue #17 (Homebrew 7.0.14+ no longer loads formulae from paths); spec: `docs/superpowers/specs/2026-11-26-path-install-stopgap.md`. brewsoak has no tap of its own; `brewsoakr/soaked` survives only as `origin::STAGING_TAP` so pre-1.0 receipts and `tap-info` entries classify as staged, never as an origin (issue #4).
- Install a soaked package by bare name, and only behind `Brew::by_name`: brew's own current definition must be byte-identical to the soaked cutoff.
- Hash the raw cutoff blob from `resolve_pkg_blobs`, never a staged or sanitized file and never brewsoak's HEAD blob.
- The gate covers the dependency closure. A member passes when brew's current definition is the cutoff, or when it is an installed dependency that brew already has at brew's current version (`Role::Check` in `cmd/deps.rs`). Use a by-name dependency list only after that package passed.
- Never hand a soaked `brew install` or `brew deps` a path or a `user/repo/name` token.
- A gate failure or any `Error::Brew` from the walk, `brew deps` or `by_name` holds that one target with a note naming issue #17 and a bypass command. Holds are never fatal: the next package still runs and the run exits 1. Git and I/O errors stay fatal.
- Run one `brew update` per run, before soaked work, only when the run has soaked work (also on a dry run). Its failure is a note and an exit code, never a hold. The no-soak step repeats it only when it did not succeed.
- Set `HOMEBREW_NO_AUTO_UPDATE=1`, `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1` and `HOMEBREW_NO_COLOR=1` on every brewsoak `brew` child (brew colors some `==>` lines even into a pipe; the summarizer's ANSI strip is only a fallback).
- Also remove `HOMEBREW_FORCE_API_AUTO_UPDATE` from brew children, so brew's loaders read the taps as they are.
- `HOMEBREW_DEVELOPER=1` and the removal of `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` are still set, but enable nothing now.
- Never pass `--ignore-dependencies`. Homebrew treats it as an unsupported developer option and warns even in developer mode. Install the cutoff dep closure first, then install the target and let brew treat already-installed deps as satisfied.
- Cellar receipts omit `bottle`/`rebuild`. Do not treat a missing rebuild as 0 when comparing installed vs cutoff.
- `-v`/`--verbose` prints the soak window, cutoffs, and a line for every package evaluated. Bare `-v` is brewsoak help, not `brew -v`.
- Persist `--soak-hours` only on soaked commands, never on passthrough/`--version`/`--help`.
- Do not uninstall a `homebrew/core` keg to switch taps.

## Origins and staging

- A package's origin is its receipt tap when it is non-empty and not `brewsoakr/soaked`: for a formula, `source.tap` in the keg's `INSTALL_RECEIPT.json` (never `brew info`'s top-level `tap`, which is where brew resolves the name now and says `homebrew/core` for a staged tap keg that shares a core name); when a formula's `source.tap` is null and `source.path` is under `<cache>/staging/taps/<user>/<repo>/`, the origin is `<user>/<repo>`; for a cask, `tap` from `brew info --json=v2 --installed`; else `<cache>/origins.toml` (`"formula:<name>" = "user/repo"`); else `homebrew/core` / `homebrew/cask`. A by-name install of a tap package records its tap in the receipt, so remove the origin record after every real by-name install (not on a dry run, not on brew's "already installed" answer). `<cache>/staging/` is still read (old receipts with a null tap and a staged `source.path`) but no longer written.
- The dependency walk runs `brew deps --1 --full-name`: brew resolves bare dep names (core first, then a unique installed tap) and prints `user/repo/name` for tap packages, so a full name is that tap and a bare name is core or cask. An ambiguous name (`TapFormulaAmbiguityError`) holds the target.
- `<cache>/staging/` (`taps/<user>/<repo>/{Formula,Casks}/<name>.rb` for tap packages, directly under it for core and cask) is historical: brewsoak no longer writes it, but old receipts may still point into it.
- Tap history lives in `<cache>/taps/<user>/<repo>.git`: a bare clone with a named `origin` remote, fetched `--filter=blob:none` (retry without the filter if the server rejects it). Never fetch into, or move the checkout of, anything under `$(brew --repository)/Library/Taps`. No GitHub API calls for taps.
- `NO_SOAK` packages go to brew as full tokens (`user/repo/name` for tap packages) in one `brew upgrade|install|reinstall` after all soaked work (preceded by `brew update` unless one already succeeded). A no-soak package whose keg was staged by brewsoak is moved to its real tap with `brew install user/repo/name`.
- `brew tap-info --json --installed` reports `remote: null` for API-mode `homebrew/core` and `homebrew/cask` and for a leftover `brewsoakr/soaked` tap; classify those by name before applying the "no HTTPS remote = unsoakable" rule.
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

## Cache state

- Commands that write the cache (`update`, `upgrade`, `install`, `reinstall`) hold the `<cache>/run.lock` flock from the start of `dispatch` (`lock::LockSlot`). `outdated` and `info` start unlocked; `cmd::ensure_snapshots` takes the lock itself when it finds the snapshot missing, unreadable or stale, then re-reads `state.toml` and decides again. Code that writes `state.toml`, the clones, gc or prune takes a `&RunLock`; never write the cache without one. The holder's pid and command live in `run.lock.holder` (temp + rename). `settings` never touches the cache.
- After a refresh, `prune::prune_cache` (under the lock) removes clone and staging dirs of taps no longer tapped and `origins.toml` records of packages no longer installed. Never prune a record because its tap is gone while the package is installed; never touch `$(brew --repository)/Library/Taps`.
- Write `state.toml` and `origins.toml` only through `atomic::write` (temp + rename, then fsync the directory).
- `gc_prune` runs only after `state.toml` is written.
