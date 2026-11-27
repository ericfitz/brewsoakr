# Stopgap for issue #17: install by name when brew's current definition is the soaked cutoff

Date: 2026-11-26
Extends: [2026-08-12-brewsoakr-design.md](2026-08-12-brewsoakr-design.md), [2026-10-05-tap-soak-and-no-soak-design.md](2026-10-05-tap-soak-and-no-soak-design.md)
Issue: #17 (stays open; this is not the fix)
Plan: [../plans/2026-11-26-path-install-stopgap.md](../plans/2026-11-26-path-install-stopgap.md)

## Problem

Homebrew 7.0.14+ (commit `ec249da5f7`, 2026-11-09) no longer loads formulae or
casks from file paths, for anyone. `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` is a
no-op and `HOMEBREW_DEVELOPER=1` does not bring path loading back. brewsoak
stages the cutoff `.rb` under `<cache>/staging/` and passes that path to
`brew deps` and `brew install`. brew now reads the path as a name:

    Error: No available formula with the name "ast-grep". Did you mean tgrep?

On a core formula the first `brew deps` failure falls through
`Err(e) => return Err(e)` in `install_cutoff` and aborts the run: a 55-package
`brewsoak upgrade` died at `[1/55]`. Repro: `brewsoak upgrade --dry-run ast-grep`.

## Human decisions (made by the maintainer, 2026-11-26)

This is an architectural decision made by the human maintainer. It is not to
be changed without the maintainer's approval.

- **Ship a stopgap now and design the long-term replacement separately**
  (the maintainer's "option 3", chosen 2026-11-26 from the options raised for
  issue #17). The long-term mechanism (for example a local tap, or brew's new
  explicit private-source loading) is out of scope here and stays tracked in
  issue #17.

What follows from it, recorded here so the older specs can point at it: while
the stopgap is in place it supersedes the 2026-08-13 path-install decision
(recorded 2026-10-06, issue #4). Nothing is staged and no file path is passed
to brew.

Earlier locks this design leaves alone: the soak clock starts on any file
change (2026-08-12); brewsoak never runs `brew trust` and never trusts a tap
on the user's behalf (2026-10-05, amended 2026-10-06); `brew update` runs only
for no-soak packages (2026-10-05); `--ignore-dependencies` is never passed
(2026-08-12, step 5); the `reinstall` true repair and the no-soak step are
untouched.

## Stopgap requirements (from the session brief, 2026-11-26)

Proposed by the orchestrating Claude session and taken as the design's
requirements; the maintainer has not reviewed them one by one.

- A soaked package is installed by name, and only when what brew would
  install by name is the soaked cutoff. The same holds for every dependency
  brew might install or upgrade with it. Otherwise the package is held.
- A hold is never fatal. The held package gets a note that says it cannot be
  soaked until issue #17 is fixed and how to bypass brewsoak; every other
  package in the run proceeds.
- `upgrade`, `install`, `reinstall`, casks and `--dry-run` are all covered.
- brew's tap-trust refusal still holds the package; brewsoak never runs
  `brew trust`.

## Design choices in this spec (by Claude, 2026-11-26; not yet confirmed by the maintainer)

Each differs from, or adds to, the session brief. Choices 1, 2 and 6 are
also listed under Open questions.

1. **The gate compares file bytes, not version and revision.** The brief
   proposed "brew's stable version + revision equals the cutoff's". That
   cannot see a bottle rebuild, a same-version sha change or a dependency
   edit, and the 2026-08-12 lock starts the soak clock on any file change.
   brew reports the SHA-256 of the definition file it would load
   (`ruby_source_checksum.sha256`), so the gate is: SHA-256 of the cutoff
   blob equals that checksum. Equal bytes imply equal version, revision,
   bottle block and dependencies.
2. **By-name installs use the bare name, never `user/repo/name`.** brew
   records item-level trust for every fully qualified name given to
   `install`, `upgrade` or `reinstall` (see Verified facts), which would
   trust a third-party package on the user's behalf. A bare name is used only
   when brew resolves it to the package's own origin; a name that resolves
   elsewhere is held.
3. **`--ignore-dependencies` is not passed.** The dependency-closure gate
   below is the one guarantee. The flag is an unsupported developer option
   (brew warns on every use, and may remove it as it removed path loading),
   it would leave missing no-soak and unsoakable dependencies uninstalled and
   skip upgrades the target's bottle needs, and the AGENTS.md rule and the
   2026-08-12 step 5 stay as they are.
4. **The closure gate covers installed dependencies and walks through them.**
   See "Dependency closure".
5. **SHA-256 comes from `ring`, added as a direct dependency.** It is already
   in `Cargo.lock` through `ureq` and `rustls`, so no new code enters the
   build. The alternative, the `sha2` crate, adds crates to the lockfile.
6. **No `brew update` is added for soaked work**, and `outdated`, `info` and
   the plan total are unchanged. See "Coverage and limits".

## Verified facts (Homebrew 7.0.16-86-gfac0743, 2026-11-26)

- `brew info --json=v2 --installed`, which inventory already runs, carries
  for every formula `tap`, `versions.stable`, `revision`, `installed[].version`
  and `ruby_source_checksum.sha256`, and for every cask `tap`, `version`,
  `installed` and `ruby_source_checksum.sha256`. All 165 formulae and 16 casks
  on the test machine had a checksum.
- The checksum is the SHA-256 of the raw definition file: for `ast-grep` it
  equals the SHA-256 of brewsoak's cutoff blob. Of 50 outdated core formulae,
  47 had a cutoff blob identical to brew's current file.
- A row describes what the bare name resolves to now, not the installed keg:
  the row for a staged `cyclonedx/cyclonedx/cyclonedx-gomod` keg says
  `tap: homebrew/core` with core's checksum.
- `brew info --json=v2 --formula|--cask <bare name>` answers for packages that
  are not installed, resolves a bare tap name (`terraform` gives
  `hashicorp/tap`), and fails with `No available formula` for an unknown name.
- `--1` is `--direct`: `brew deps --1 --full-name <name>` lists the
  dependencies the current definition declares. Without `--1`, an installed
  formula's list comes from its keg. One call takes about 0.4 s.
- `brew install <name>` installs missing dependencies and upgrades outdated
  installed ones, through dependencies that are themselves current:
  `brew install --dry-run harfbuzz` would upgrade `glib` (direct) and
  `fontconfig` (reached through the current `cairo`). In brew's source
  (`FormulaInstaller#expand_dependencies_for_formula_once`) a dependency at
  brew's current version is skipped while its own dependencies are still
  expanded, and an outdated one is left alone only when it meets the minimum
  version recorded in the target's bottle. brewsoak does not read bottle
  minimums, so it cannot predict that case.
- With `HOMEBREW_NO_AUTO_UPDATE=1` and `HOMEBREW_FORCE_API_AUTO_UPDATE`
  unset, brew does not re-download its API data (`API.fetch_api_files!`), so
  what the gate reads is what the install uses.
- `cmd/install.rb`, `cmd/upgrade.rb` and `cmd/reinstall.rb` call
  `Homebrew::Trust.trust_fully_qualified_items!(args.named)`, which adds
  `user/repo/name` to the trust store unless the tap is an official one.
  `brew deps` and `brew info` do not.
- A by-name `brew install` is the command a path install ran; only how the
  definition is loaded differs. `brew install --formula --dry-run ast-grep`
  and `brew install --formula --dry-run terraform` (a tap formula, by bare
  name) both report the upgrade. A dry run prints nothing for an installed
  cask.

Not verified: what `brew info --installed` reports for a keg whose tap is
untrusted. The design does not depend on it (see "Tap trust").

## Design

### Where a staged path reaches brew today

| Site | What it does |
|---|---|
| `cmd::deps::CutoffDepWalk::visit` | writes dependency blobs (`view::write_cutoff_blob`, `view.rs:456`) and calls `brew.deps(kind, <staged path>)` |
| `cmd::apply::install_cutoff` | `tap::write_blob`, then `tap::brew_install_args(.., &path, ..)` |
| `cmd::apply::install_missing_dep` | the same, for each missing or upgrading dependency |
| `tap::brew_install_args` | appends the path as the last argument |
| `brew::ProcessBrew::deps` | passes its `token` through; the walk gives it a path |

Nothing else in the `Brew` trait takes a path. `info`, `outdated`, `update`,
the no-soak step (`nosoak::run_step`) and the `reinstall` true repair already
use names. `brew::brewsoak_brew_env_pairs` sets `HOMEBREW_DEVELOPER=1` and
unsets `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` for path installs; both are now
inert and are left for the long-term design to remove.

### The by-name gate

A soaked package may go to brew by name only when brew answers, from its own
data, that:

- the bare name (with `--formula` or `--cask`) resolves to the package's
  origin tap (the row's `tap`), and
- `ruby_source_checksum.sha256` equals the SHA-256 of the raw cutoff blob
  from brewsoak's clone (never the staged file, which `sanitize_unofficial`
  rewrites, and never brewsoak's HEAD blob).

brew's answer comes from the `info --json=v2 --installed` document inventory
already loads, and from one `brew info --json=v2 --formula|--cask <name>`
call for each name that is not installed. Answers are kept for the run. A
package's by-name dependency list is used only after the package passes the
gate, so the list is the cutoff's list.

### Dependency closure

The walk (`CutoffDepWalk::visit`) asks `brew deps --1 --full-name` by bare
name and now covers the whole declared closure. For each member:

| Member | Rule |
|---|---|
| The target; a missing soaked dependency; an installed soaked dependency this run upgrades first | must pass the gate; installed by name, dependencies first, as today |
| An installed soaked formula dependency this run does not upgrade | passes if brew has it at brew's current version (newest keg equals `versions.stable` plus `_revision`), because brew then leaves it alone; otherwise it must pass the gate, because brew may upgrade it to brew's current version; never installed by brewsoak |
| An installed cask dependency | left alone: brew treats any installed version as satisfied (2026-10-17, section 6) |
| A no-soak or unsoakable dependency | brew's, as today; not walked |
| A dependency in a held tap, or with no cutoff at all | as today (`HeldDep`, or refused from its upstream status) |

The walk recurses through installed dependencies too, because brew expands
the dependencies of a dependency it skips. One failing member holds the
target. `brew deps` answers are kept for the run, so a bare upgrade asks once
per package (an estimated minute at most on a machine with 165 formulae).

### Installing

`brew install --formula|--cask [user flags] <bare name>` for each walked
dependency and then the target, with the flags and environment used today.
`HOMEBREW_FORCE_API_AUTO_UPDATE` is also unset on every brew child so brew's
data cannot move between the gate and the install.

### Holds and messages

Never fatal. Every `Error::Brew` from the walk or the lookup becomes a hold of
that target; git and I/O errors stay fatal as today. A held target keeps its
`[i/N]` line, then prints `held <name>: <why>` and defers one note under
`Held:`. The run exits 1 (2026-08-12 mixed-upgrade rule).

    <name>: [dependency <dep>: ]<why>; brewsoak cannot install a soaked copy until issue #17 is fixed; use `brew <verb> <token>` to bypass brewsoak.

`<token>` is `nosoak::brew_token(origin, name)`. `<why>` is one of:

- `brew would install <brew version>, which is not the soaked cutoff`
- `brew resolves the name to <tap>, not <origin>`
- `brew could not describe it by name (<first line of brew's error>)`
- `brew could not read its dependencies (<first line of brew's error>)`
- `it has no soaked cutoff yet`

The ambiguous-dependency refusal and the held-tap refusal keep their current
text. The "cannot be installed from a staged copy" hold goes away with the
staged copy: a load error is now brew's own failure for that package.

### Origin and receipt bookkeeping

A by-name install writes a receipt that names the real tap, so:

- after a successful by-name install brewsoak removes the package's
  `origins.toml` record (`forget_origin`), where a staged install wrote one
  (`record_origin`, deleted). A dry run and brew's "already installed" answer
  change nothing, as today.
- `inventory.rs` and `origin.rs` do not change. Kegs staged before the
  stopgap keep a null receipt tap and a staging path until they are next
  upgraded, so `origin::staged_tap`, the `origins.toml` fallback and
  `origin::STAGING_TAP` stay. A by-name keg classifies by its receipt tap,
  ahead of any stale record.
- `<cache>/staging/` is no longer written. Existing files are left;
  `prune::prune_cache` still removes the directories of untapped taps.

### Tap trust

Bare names never reach `trust_fully_qualified_items!`, so brew's own check
applies: it refuses to load a package from an untrusted tap. For a target
whose origin is a third-party tap the walk asks `brew deps` before the gate,
so the refusal keeps today's note (`<name>: brew does not trust tap <tap>;
run brew trust <tap>, or add it to NO_SOAK`). The same text from the lookup
or from the install maps to the same hold. brewsoak never runs `brew trust`.
A tap package whose name also exists in core (`cyclonedx-gomod`) is held with
"brew resolves the name to homebrew/core".

### Dry run, `outdated`, `info`, plan total

Every gate decision is made before brew is asked to install, so `--dry-run`
reports the same holds and the same by-name commands (with `--dry-run`
forwarded). `outdated`, `info` and the `[i/N]` plan total keep reporting soak
state and do not predict stopgap holds, like the dependency and trust holds
that are already found only while applying.

### Removed and kept

Removed: every staging write and path argument listed above;
`view::write_cutoff_blob`; `StagedRun::LoadFailure` and its hold helpers; the
`tap_root` field of `ApplySession` and `CutoffDepWalk`. The `tap_root`
parameter of `upgrade`, `install` and `reinstall` stays (unused) so the
long-term fix does not have to re-thread it. Kept for the long-term fix: the
`pub` helpers in `tap.rs` and `taps.rs` (`write_blob`, `sanitize_unofficial`,
`staging_root`, `staged_load_failure`).

## Coverage and limits

- A package installs only while brew's current file is the soaked cutoff:
  the definition has not changed for at least the soak time, and brew's data
  is at or past the cutoff. Packages that change more often than the soak
  time are held until issue #17 is fixed.
- brew's data is as old as the last `brew update`. brewsoak runs one only
  when a no-soak package is involved (2026-10-05). When brew's data is older
  than the cutoff, the package is held; the README tells users to run
  `brew update`.
- The closure rule can hold more than brew would change: a target is held
  for an outdated dependency that is still soaking even in the case where
  brew would have left that dependency alone (it meets the bottle minimum).
- A tap package whose name collides with core is held.
- Build dependencies brew adds when it builds from source are not in
  `brew deps --1` and are not gated. This is unchanged from path installs.

## Re-running

No new persistent state. Removing an `origins.toml` record is idempotent and
goes through `atomic::write` under the run lock already held. brew answers
are kept in memory for one process. A second run sees an upgraded package as
`NoOpAlreadySoaked`; a held one is evaluated again from scratch. Leftovers
from a failed run are brew's, as today.

## Open questions for the maintainer

1. **Third-party tap packages** (Choice 2). This spec uses bare names and
   holds name collisions. Alternatives: (A) hold every soaked tap package
   until #17 is fixed; (C) use `user/repo/name` and accept that brew records
   item-level trust for it, which amends the 2026-10-05 trust decision.
2. **Gate strictness** (Choice 1). This spec uses file bytes. The proposed
   version-and-revision gate would install more, including artifacts that
   changed inside the soak window.
3. **May a soaked run call `brew update` first** to raise coverage? This
   spec says no; yes would amend the 2026-10-05 `brew update` decision.
4. The no-soak step and the `reinstall` true repair already hand brew full
   tokens, so brew already records item-level trust for those packages. Out
   of scope here; needs its own ruling.

## Testing

Unit tests with `MockBrew` (see the plan): the gate and closure rules, each
never-abort path with a second package that still installs, both trust
paths, origin records, every verb, casks, `--dry-run`, and a sweep asserting
that no recorded brew argument is a path. The final check is the real
`brewsoak upgrade --dry-run ast-grep`, before and after.
