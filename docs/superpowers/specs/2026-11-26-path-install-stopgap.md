# Stopgap for issue #17: install by name when brew's current definition is the soaked cutoff

Date: 2026-11-26
Extends: [2026-08-12-brewsoakr-design.md](2026-08-12-brewsoakr-design.md), [2026-10-05-tap-soak-and-no-soak-design.md](2026-10-05-tap-soak-and-no-soak-design.md)
Issue: #17 (stays open; this is not the fix)
Plan: [../plans/2026-11-26-path-install-stopgap.md](../plans/2026-11-26-path-install-stopgap.md)

> **Superseded 2026-11-27** for `homebrew/core` and `homebrew/cask` packages by [2026-11-27-local-tap-install-design.md](2026-11-27-local-tap-install-design.md) (Human decisions 1, 2 and 5). "Tokens: the bare name" is amended for soaked installs, which use full `ericfitz/brewsoak/<name>` tokens, and stands for third-party tap packages, which keep this by-name gate.

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

These are architectural decisions made by the human maintainer. They are not
to be changed without the maintainer's approval.

- **Ship a stopgap now and design the long-term replacement separately**
  (2026-11-26; the maintainer's "option 3" from the options raised for issue
  #17). The long-term mechanism (for example a local tap, or brew's new
  explicit private-source loading) is out of scope here and stays tracked in
  issue #17.
- **The stopgap gate** (reviewed by the maintainer 2026-11-27). A soaked package
  is installed by name, and only when what brew would install by name is the
  soaked cutoff; the same holds for every dependency brew might install or
  upgrade with it. Otherwise the package is held. A hold is never fatal: the
  held package gets a note that says it cannot be soaked until issue #17 is
  fixed and how to bypass brewsoak, and every other package proceeds.
  `upgrade`, `install`, `reinstall`, casks and `--dry-run` are all covered.
- **Tokens: the bare name, never `user/repo/name`** (2026-11-27, confirmed
  after a second look). A package goes to brew by its bare name, and only
  when brew's loader resolves that name to the package's own tap. A name that
  resolves elsewhere (a tap package whose name also exists in core) is held.
  brew records no trust for a bare name, so the 2026-10-05 trust decision
  stands unchanged.
- **Gate strictness: exact file bytes** (2026-11-27). The SHA-256 of the
  cutoff blob must equal brew's `ruby_source_checksum`. Version and revision
  alone are not enough.
- **A changed file at the same version is a soak wait, not issue #17**
  (2026-11-27, amends the hold text above). When the gate holds a member whose
  brew-current version equals the cutoff version while the file bytes differ,
  the hold says `brew's <version> formula changed since the soaked cutoff; it
  installs once the change soaks` (inline, and in the note with `; use ...
  to bypass brewsoak.` and no issue #17 clause). Versions are compared as the
  identity strings brewsoak already has; when the cutoff blob does not parse,
  or the versions differ, the existing issue #17 text stays. Every hold that
  truly waits on #17 (different version, other tap, lookup or deps failure,
  no cutoff) keeps its text. No remaining-time hint: the walk has neither the
  change's commit time nor the soak clock at that point, and the decision
  forbids new git or brew calls for it.
- **A run with soaked work runs `brew update` once, before that work**
  (2026-11-27). This amends the 2026-10-05 decision that `brew update` runs
  only when a no-soak package is involved. See "`brew update` before soaked
  work" for where it runs and what a failure does.

What follows from these, recorded here so the older specs can point at it:
while the stopgap is in place it supersedes the 2026-08-13 path-install
decision (recorded 2026-10-06, issue #4). Nothing is staged and no file path
is passed to brew.

Earlier locks this design leaves alone: the soak clock starts on any file
change (2026-08-12); brewsoak never runs `brew trust` and never trusts a tap
on the user's behalf (2026-10-05, amended 2026-10-06); `--ignore-dependencies`
is never passed (2026-08-12, step 5); the `reinstall` true repair and the
no-soak step keep their full tokens (issue #18).

## Design choices in this spec (by Claude)

Choices 1 and 2 were confirmed by the maintainer on 2026-11-27 and are
recorded above. Choices 3 to 7 are Claude's; the maintainer has seen them in
the session report but has not ruled on them one by one.

1. **The gate compares file bytes, not version and revision** (confirmed).
   The session brief proposed "brew's stable version + revision equals the
   cutoff's". That cannot see a bottle rebuild, a same-version sha change or
   a dependency edit, and the 2026-08-12 lock starts the soak clock on any
   file change. Equal bytes imply equal version, revision, bottle block and
   dependencies.
2. **By-name installs use the bare name** (confirmed). brew records
   item-level trust for every fully qualified name given to `install`,
   `upgrade` or `reinstall` (see Verified facts), which would trust a
   third-party package on the user's behalf.
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
6. **`outdated`, `info` and the plan total are unchanged.** They do not
   predict stopgap holds and do not run `brew update`.
7. **Details of the `brew update` decision** that the maintainer left to the
   design: it also runs on a dry run; a failed update does not stop the run;
   the no-soak step does not repeat an update that succeeded. See
   "`brew update` before soaked work".

## Verified facts (Homebrew 7.0.16-86-gfac0743, 2026-11-26)

- brew knows the SHA-256 of the definition file it would load
  (`Formula#ruby_source_checksum`, `Cask#ruby_source_checksum`; also
  `ruby_source_checksum.sha256` in `brew info --json=v2`). For `ast-grep` it
  equals the SHA-256 of brewsoak's cutoff blob. Of 50 outdated core formulae
  on the test machine, 47 had a cutoff blob identical to brew's current file.
- `brew install <name>`, `brew deps <name>` and `brew ruby` load a bare
  formula name through `Formulary.factory(name)`: core first, then a unique
  installed tap (`terraform` gives `hashicorp/tap`). Casks load through
  `Cask::CaskLoader.load(token)`.
- `brew info --json=v2` is not that loader. For an unqualified name it
  reports the installed formula from the keg's receipt tap when that differs
  from the loader's answer (`installed_resolution` in `cmd/info.rb`), and
  `--installed` rows come from the installed kegs. A tap keg whose name also
  exists in core would be described by its tap's file while
  `brew install <name>` loads core's. So the gate does not read `brew info`.
- One `brew ruby` call that runs those two loaders for all 181 installed
  packages takes 0.7 s and prints tap, version, checksum and
  `latest_version_installed?` for each; an unknown name raises
  `FormulaUnavailableError` or `Cask::CaskUnavailableError`. For the staged
  `cyclonedx/cyclonedx/cyclonedx-gomod` keg the bare name loads
  `homebrew/core`.
- `Tap.with_formula_name("terraform")` and `Tap.with_cask_token("ant")` are
  `nil`: a bare name is not a fully qualified item (see the trust fact
  below).
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
- An explicit `brew update` downloads brew's API data itself
  (`fetch_api_file` in `cmd/update.sh`) and does not consult
  `HOMEBREW_NO_AUTO_UPDATE`. The no-soak step relies on this today.
- `cmd/install.rb`, `cmd/upgrade.rb` and `cmd/reinstall.rb` call
  `Homebrew::Trust.trust_fully_qualified_items!(args.named)`, which adds
  `user/repo/name` to the trust store unless the tap is an official one.
  `brew deps` and `brew info` do not.
- A by-name `brew install` is the command a path install ran; only how the
  definition is loaded differs. `brew install --formula --dry-run ast-grep`
  and `brew install --formula --dry-run terraform` (a tap formula, by bare
  name) both report the upgrade. A dry run prints nothing for an installed
  cask.

Not verified: the exact error the loaders raise for a package in an
untrusted tap when asked through `brew ruby`. The design does not depend on
it (see "Tap trust").

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
use names. `brew::brewsoak_brew_env_pairs(verb)` sets `HOMEBREW_DEVELOPER=1`
and unsets `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` for path installs; neither
enables path loading now and both are left for the long-term design to remove.
`HOMEBREW_DEVELOPER` is not inert, though: with it set, `brew update` follows
`main` instead of the latest stable tag (Homebrew `cmd/update.sh`), so an
`update` child does not get it and inherits the user's own value (2026-11-27
fix); `brew ruby` and the other children keep it.

### The by-name gate

A soaked package may go to brew by name only when brew answers, from its own
data, that:

- the bare name loads from the package's origin tap, and
- its `ruby_source_checksum` equals the SHA-256 of the raw cutoff blob
  from brewsoak's clone (never the staged file, which `sanitize_unofficial`
  rewrites, and never brewsoak's HEAD blob).

brew's answer comes from the loaders `brew install` itself uses, run under
`brew ruby` (as brewsoak already does for version order, version detection
and the host platform): `Formulary.factory(name)` for a formula and
`Cask::CaskLoader.load(token)` for a cask, printing the tap, the version
(`pkg_version`, or the cask `version`), `ruby_source_checksum` and whether the
latest version is installed. The first question of a run asks about every
installed package in one call; a name that is not installed is asked when it
comes up. Answers are kept for the run. If the script fails, or prints
anything brewsoak does not understand, the answer is unknown and the package
is held: a brew change that breaks the script stops installs, it does not
let unsoaked ones through.

A package's by-name dependency list is used only after the package passes the
gate, so the list is the cutoff's list.

### Dependency closure

The walk (`CutoffDepWalk::visit`) asks `brew deps --1 --full-name` by bare
name and now covers the whole declared closure. For each member:

| Member | Rule |
|---|---|
| The target; a missing soaked dependency; an installed soaked dependency this run upgrades first | must pass the gate; installed by name, dependencies first, as today |
| An installed soaked formula dependency this run does not upgrade | passes if brew has its latest version installed (`latest_version_installed?`, the test brew's own dependency check starts with), because brew then leaves it alone; otherwise it must pass the gate, because brew may upgrade it to brew's current version; never installed by brewsoak |
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
data cannot move between the gate and the install (only the one explicit
`brew update` moves it, before the gate).

### `brew update` before soaked work

Decided by the maintainer on 2026-11-27; the details are Choice 7.

- **When.** Once per `upgrade`, `install` or `reinstall` run that has soaked
  work: a bare upgrade whose plan (`plan_size`) is not empty, or a named run
  that names at least one soaked package. It runs after the tokens are
  validated and before the first package is evaluated, so before brewsoak
  asks brew's loaders anything. Its output goes under the existing
  `Updating Homebrew` header, ahead of the soaked section. A run with no
  soaked work is unchanged.
- **Environment.** The `brew update` child gets the same environment as
  every other brew child. `HOMEBREW_NO_AUTO_UPDATE=1` still stops every other
  child from updating, so brew's data does not move again during the run.
- **Order of the data.** brewsoak's own snapshots are refreshed before the
  command starts (`ensure_snapshots`), so after the update brew's data is at
  or past the cutoff. `ProcessBrew` drops its remembered loader and
  `brew deps` answers whenever it runs `brew update`, so nothing read before
  an update is used after it.
- **The no-soak step.** When this update succeeded, the no-soak step does not
  run `brew update` again and prints no second `Updating Homebrew` section:
  still one update per run (2026-10-05). When it did not run, or failed, the
  no-soak step runs its own, exactly as today.
- **Dry run.** A dry run updates too. `brew update` installs nothing, a dry
  run already runs the no-soak update (ruling of 2026-10-06), and without it
  the preview would report holds a real run would not.
- **Failure.** A failed update does not stop the run and holds nothing by
  itself. brewsoak continues with the data brew has, defers the note
  `brew update failed (exit <N>); brew's data may be older than the soak
  cutoff, so more packages may be held`, and the run takes brew's exit code
  (as a failed no-soak update does). This is safe because the gate compares
  exact bytes: older data can only make the gate hold more packages, never
  pass a file that is not the soaked cutoff. `brew` not being found stays
  fatal, as today.
- `brewsoak update`, `outdated` and `info` are unchanged.

### Holds and messages

Never fatal. Every `Error::Brew` from the walk or the loader script becomes a
hold of that target; git and I/O errors stay fatal as today. A held target keeps its
`[i/N]` line, then prints `held <name>: <why>` and defers one note under
`Held:`. The run exits 1 (2026-08-12 mixed-upgrade rule).

    <name>: [dependency <dep>: ]<why>; brewsoak cannot install a soaked copy until issue #17 is fixed; use `brew <verb> <token>` to bypass brewsoak.

`<token>` is `nosoak::brew_token(origin, name)`. `<why>` is one of:

- `brew would install <brew version>, which is not the soaked cutoff`
- `brew's <version> <formula|cask> changed since the soaked cutoff; it
  installs once the change soaks`: brew's current version equals the cutoff's
  version but the file bytes differ (2026-11-27 human decision, below). Its
  note omits the issue #17 clause, because the hold ends when the change
  soaks, not when Homebrew is fixed.
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

A bare name is not a fully qualified item, so `trust_fully_qualified_items!`
skips it and brew's own check applies: it refuses to load a package from an
untrusted tap. For a target
whose origin is a third-party tap the walk asks `brew deps` before the gate,
so the refusal keeps today's note (`<name>: brew does not trust tap <tap>;
run brew trust <tap>, or add it to NO_SOAK`). The same text from the loader
script or from the install maps to the same hold. brewsoak never runs `brew trust`.
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

- A package installs only while brew's current file is the soaked cutoff.
  After the run's `brew update` that means the definition has not changed
  since the cutoff. A package that changed again inside the soak window is
  held until that change has soaked, so one that changes more often than the
  soak time is held until issue #17 is fixed (when the version is unchanged,
  the hold says the change must soak instead).
- When the run's `brew update` fails, brew's data may be older than the
  cutoff and more packages are held; the note says so.
- The closure rule can hold more than brew would change: a target is held
  for an outdated dependency that is still soaking even in the case where
  brew would have left that dependency alone (it meets the bottle minimum).
- A tap package whose name collides with core is held.
- Build dependencies brew adds when it builds from source are not in
  `brew deps --1` and are not gated. This is unchanged from path installs.

## Re-running

No new persistent state in brewsoak. Removing an `origins.toml` record is
idempotent and goes through `atomic::write` under the run lock already held.
brew answers are kept in memory for one process. `brew update` is brew's own
state: a second run updates again, two brewsoak runs cannot overlap (the run
lock), and a `brew update` that failed halfway is retried by the next run. A second run sees an upgraded package as
`NoOpAlreadySoaked`; a held one is evaluated again from scratch. Leftovers
from a failed run are brew's, as today.

## Questions answered by the maintainer (2026-11-27)

1. Third-party tap packages: bare names; name collisions are held.
2. Gate strictness: exact file bytes.
3. `brew update`: a run with soaked work runs it once, first.

All three are recorded under "Human decisions". Still open, and out of scope
here: the no-soak step and the `reinstall` true repair hand brew full tokens,
so brew records item-level trust for those packages (issue #18).

## Testing

Unit tests with `MockBrew` (see the plan): the gate and closure rules, each
never-abort path with a second package that still installs, both trust
paths, origin records, every verb, casks, `--dry-run`, the one `brew update`
(when it runs, its failure, the no-soak step not repeating it), and a sweep
asserting that no recorded brew argument is a path. The final check is the real
`brewsoak upgrade --dry-run ast-grep`, before and after.
