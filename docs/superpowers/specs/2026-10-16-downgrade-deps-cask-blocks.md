# Fixes from the 2026-10-16 run: downgrades, dependency order, cask platform blocks

## Problems (from a real run's log)

1. `openssl@3` went from 3.6.5 to 3.5.9. Homebrew re-pointed `openssl@3` at the 3.5
   line and moved 3.6 to the new `openssl@3.6`. The installed version matched neither
   cutoff nor HEAD, so `eligibility::desired_action` returned `InstallCutoff`.
   brewsoak has no notion of version order, so it downgrades any package installed
   newer than the cutoff but not at HEAD.
2. ada-url, merve and node were upgraded before simdutf, so their new bottles
   briefly linked against a `libsimdutf.37.dylib` that didn't exist yet. When
   `cmd.rs` walks the dependency closure, it skips every installed dependency, even
   one this run is about to upgrade, and the plan runs in alphabetical order.
3. ngrok and wireshark-app: "unparseable identity". Their `version` (and `sha256`)
   live inside `on_arm do` / `on_monterey :or_newer do` blocks. `identity::depth1_lines`
   treats only `stable do` as transparent.
4. The summarizer's `brew reinstall` drop rule (`quiet.rs`) also removed
   `brew reinstall --build-from-source X` after "Rebuild this from source with:",
   which left that heading with nothing under it.

## Decisions (made by Eric, 2026-10-16)

- **Version ordering, option B:** brewsoak doesn't port Homebrew's version
  ordering. It asks brew: `brew ruby -e` compares versions with
  `PkgVersion.parse` (formulae, to handle `_N` revisions) or `Version.new` (casks).
  One call compares a whole batch, and the results are memoized for the run.
  Only pairs whose version strings differ are asked about.
- **macOS block resolution, option A:** brew is asked once per run for the host
  (`MacOS.version.to_sym`, `Hardware::CPU.arch`) and the ordered
  `MacOSVersion::SYMBOLS` list. Rust then resolves `on_<codename>`
  (`:or_newer`/`:or_older`), `on_arm`/`on_intel`, `on_macos`/`on_linux` and
  `sha256 arm: …, intel: …` statically. There is no hardcoded codename table.
- Unused alternatives: a Rust port of Homebrew `Version` (1A), and per-cask
  `brew info --json=v2` evaluation (3B).

## Behavior

- New `DesiredAction::LeaveNewerThanCutoff`: the installed version is strictly
  newer than the cutoff by brew's ordering. It's never installed, it's counted
  under "ahead", and the run prints a note:
  `<name>: installed <v> is newer than cutoff <c>; leaving it (upstream may have re-pointed the formula)`.
  Named runs refuse it the same way `LeaveAheadOfSoak` does. `outdated`, `plan_size`
  and the upgrade path must agree.
- A version comparison that fails (brew error or unparseable output) holds that
  package with a note. brewsoak never falls back to installing the cutoff.
- An installed, soaked dependency whose own action is `InstallCutoff` is walked
  and upgraded before the package that depends on it. Its later plan entry prints
  "already upgraded as a dependency", and it's counted once.
- If host info can't be read, platform blocks stay opaque, as they are today.

## Re-running

These changes add no persistent state; the version memo and host info last for
one run. A second run sees an already-upgraded dependency as `NoOpAlreadySoaked`.
