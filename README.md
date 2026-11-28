# brewsoak

A Homebrew wrapper that delays `homebrew/core`, `homebrew/cask`, and
third-party tap updates for a soak window. This gives security researchers
time to discover and yank a compromised package before you install it.

Every other `brew` subcommand passes through unchanged. Packages and taps you
list under `NO_SOAK` skip soaking and end in the same state as `brew upgrade`.
There is no soak bypass flag: run `brew` directly if you need HEAD now.

## Install

### Homebrew (macOS, recommended)

```bash
brew install ericfitz/tap/brewsoak
```

Installs a prebuilt, code-signed and notarized universal (Apple Silicon +
Intel) binary from the [GitHub release](https://github.com/ericfitz/brewsoakr/releases).
Upgrade later with `brew upgrade brewsoak`. Add `ericfitz/tap` to `NO_SOAK`
(below) so `brewsoak upgrade` keeps brewsoak itself current without a soak
delay.

Before the first soaked install, trust brewsoak's own tap once, yourself:
`brew trust ericfitz/brewsoak`. brewsoak never runs `brew trust`. The first
run that needs the tap creates it and holds until you have done this (see
"brewsoak's own tap" below).

### Cargo

From [crates.io](https://crates.io/crates/brewsoak):

```bash
cargo install brewsoak
```

From this GitHub repo:

```bash
cargo install --git https://github.com/ericfitz/brewsoakr
```

From a local checkout:

```bash
git clone git@github.com:ericfitz/brewsoakr.git
cd brewsoakr
cargo install --path .
```

Or run `target/debug/brewsoak` after `cargo build`.

## Configuration

Soak duration is hours, integer ≥ 1, default **24**.

| Source | Name |
|---|---|
| CLI | `--soak-hours N` |
| Environment | `BREWSOAK_SOAK_HOURS` |
| File | `~/.config/brewsoak/config.toml` key `SOAK_HOURS` |

Precedence: CLI > environment > file > 24.

`--soak-hours` is persisted only when used with a soaked command
(`update`, `upgrade`, `install`, `reinstall`, `outdated`, `info`).
`N == 24` removes the `SOAK_HOURS` key; the file is deleted only if nothing
else remains.

### Per-tap soak hours and the no-soak list

```toml
# Top-level keys go first. TOML assigns any key after a [[TAP]] header to
# that table.
SOAK_HOURS = 48                  # default for every package, core and cask included
NO_SOAK = ["ericfitz/tap", "wget", "hashicorp/tap/terraform"]

[[TAP]]
name = "hashicorp/tap"
soak_hours = 72                  # optional; applies to every package in this tap

[[TAP]]
name = "cyclonedx/cyclonedx"     # no soak_hours: uses SOAK_HOURS
```

Effective soak hours for a package: a `NO_SOAK` match means no soak at all;
else the origin tap's `[[TAP]]` `soak_hours`; else `SOAK_HOURS`. `[[TAP]]` and
`NO_SOAK` have no flag and no environment variable: edit the file, or use
`brewsoak settings` below. `homebrew/core` and `homebrew/cask` are valid tap
names in both.

`NO_SOAK` entries:

| Entry | Matches |
|---|---|
| `wget` | that formula or cask from any tap |
| `ericfitz/tap` | every package in that tap |
| `hashicorp/tap/terraform` | that one package from that tap |

Matching is case-insensitive. Invalid entries are skipped and reported under
`-v`; `brewsoak settings repair` removes them. A `NO_SOAK` key inside a
`[[TAP]]` table is ignored, with a warning on stderr every run.

`--soak-hours N` edits only the `SOAK_HOURS` key; `[[TAP]]`, `NO_SOAK`, and
comments are kept, and the previous file is backed up as described under
`brewsoak settings`. A config file that is not valid TOML is left alone with
a warning.

### `brewsoak settings`

```text
brewsoak settings [show]
brewsoak settings soak-hours N|--clear
brewsoak settings no-soak add|remove TOKEN...
brewsoak settings tap-hours USER/REPO N|--clear
brewsoak settings repair
```

`show` prints the effective soak hours and their source (`BREWSOAK_SOAK_HOURS`,
the file, or the default 24), `NO_SOAK` as written, every `[[TAP]]` with its
effective hours, and every parse note and warning. It never writes.

Edits keep comments, key order, and unknown keys. New top-level keys go above
the first `[[TAP]]`; a new `[[TAP]]` is appended after the last one.
`soak-hours 24` removes `SOAK_HOURS` (24 is the default). `no-soak` tokens
are validated like the file's entries, compared case-insensitively, and
written lowercased; adding a present token or removing an absent one is
reported and is not an error. `no-soak add` and `remove` refuse a `NO_SOAK`
list that holds a non-string entry; `repair` fixes it. `tap-hours USER/REPO --clear` removes the
entry when only `name` would remain. When nothing is left in the file, the
file is removed.

`repair` makes the file what brewsoak reads, so `show` and `-v` stop noting
problems. A lone `NO_SOAK = "wget"` becomes `["wget"]`. Elements of `NO_SOAK`
that are not valid entries (or not strings) are removed. A `NO_SOAK` written
inside a `[[TAP]]` table has its valid entries appended to the top-level list
(created above the first `[[TAP]]` if missing), and the misplaced key is
deleted. `[[TAP]]` entries with a missing or bad `name` are removed, as is a
`soak_hours` that is not an integer >= 1 (the entry goes too if only `name`
is left) and every earlier duplicate (the last one wins, as when reading).
Valid entries keep their text, order, comments, and layout. Every removal is
printed. A `NO_SOAK` that is neither an array nor a valid entry string (an
invalid string, a number, a table), or a `TAP` that is not an array of
tables, is refused and nothing is written. With nothing to fix it prints
`nothing to repair` and writes nothing.

Every write goes to a temp file first, the previous `config.toml` is kept as
`config.toml.<YYYYMMDDTHHMMSSZ>.bak` next to it (the 2 newest backups are
kept), and the temp file is renamed into place, so another `brewsoak` never sees a
missing or partial file. An unchanged edit writes nothing. A file that is not
valid TOML is refused, with the parse error, and left untouched.

`settings soak-hours` warns on stderr when `BREWSOAK_SOAK_HOURS` is set,
because the environment overrides the file; the edit is still written.

`settings` never runs `brew` or `git` and does not check whether a tap is
installed. `--soak-hours` is not accepted with `settings`.

### Third-party taps

Every installed tap with an HTTPS remote is soaked like core and cask, from a
blobless clone under brewsoak's cache. brewsoak never fetches into or changes a
tap that brew cloned. The only directory it writes under `Library/Taps` is its
own local tap (see "brewsoak's own tap"). A tap with no remote, or a non-HTTPS
remote, is not soakable: brewsoak notes its packages and leaves them to `brew`,
unless they are in `NO_SOAK`.

No-soak packages are installed by `brew` itself from their real tap after all
soaked work: one `brew upgrade` (or `install` / `reinstall`) naming every
no-soak package, preceded by a `brew update` unless one already succeeded
earlier in the run. A dependency of a no-soak package that was installed from
brewsoak's own tap stays at its soaked version when that `brew upgrade` runs:
no-soak does not extend to it, so the no-soak package can run against an older
dependency than its bottle was built with until the dependency's update has
soaked. Soaked dependents of a no-soak package are not upgraded by that step.

When `brewsoak install` has to run `brew upgrade` for part of the no-soak group
(a formula installed from the soak tap, which brew refuses to `install`), the
no-soak section header names what brew runs, for example `Installing no-soak
packages (brew decides; runs brew install and brew upgrade)`.

## Commands

| Command | What it does |
|---|---|
| `update` | Refresh cutoff/HEAD snapshots. Runs `brew update` once when any installed package is no-soak. |
| `outdated` | What `upgrade` would change, plus held / ahead / pinned. |
| `upgrade` | Install soaked cutoff artifacts for eligible installed packages. |
| `install` | Install the soaked cutoff artifact if eligible. |
| `reinstall` | True repair via `brew` when installed == HEAD; otherwise cutoff. |
| `info` | Installed / cutoff / HEAD and the action brewsoak would take. |
| `--version` / `-V` | Print `brewsoak <version>`. |
| `--help` / `-h` | brewsoak help. `help install` is soak-aware; `help services` is `brew help`. |

Other flags (`--debug`, …) are forwarded to `brew`. `--formula`, `--formulae`, `--cask` and `--casks` are not forwarded to the per-package `brew` commands brewsoak runs, because brewsoak adds the flag for the package's own kind: a soaked install or upgrade, and each kind group of the no-soak step when a run has both formulae and casks. They still choose the kind of a name you type (`--cask` or `--formula`).
`upgrade` and `outdated` honor brew's `--greedy` (`-g`),
`--greedy-auto-updates` and `--greedy-latest` themselves (see below); the
no-soak `brew upgrade` also receives them.

`-v` / `--verbose` prints the soak window, cutoff SHAs and times, and a
line for every package evaluated (what happened and why).

`--raw` turns off output summarizing and forwards `brew`'s output byte for
byte.

### brewsoak's own tap

Homebrew 7.0.14 stopped loading formulae from file paths. brewsoak therefore
installs a soaked `homebrew/core` or `homebrew/cask` package from its own local
tap, `ericfitz/brewsoak` (at
`$(brew --repository)/Library/Taps/ericfitz/homebrew-brewsoak`): it writes the
cutoff file there for as long as the install takes and asks brew to install
`ericfitz/brewsoak/<name>`. Packages from third-party taps do not use it.

**Homebrew version.** The minimum is Homebrew 7.0.14. brewsoak does not check
brew's version. The design was verified on one build, 7.0.17-27-g96c963e; below
7.0.14 it is unsupported and untested.

**Trust, once.** The first real run that needs the tap runs
`brew tap-new --no-git ericfitz/brewsoak` and then holds every package it
would install from it until you run

```bash
brew trust ericfitz/brewsoak
```

once. brewsoak never runs `brew trust`. The hold prints
`held <name>: brewsoak's tap is not ready` per package and one note under
`Held:`; the run exits 1. A dry run creates nothing and holds the same way.
`update`, `outdated` and `info` never create the tap.

The note says which of four cases applies:

- Not trusted: `brewsoak installs soaked packages from its own tap,
  ericfitz/brewsoak, and brew does not trust it yet; run brew trust
  ericfitz/brewsoak once, then run brewsoak again.`
- Dry run, no tap yet: `brewsoak's tap, ericfitz/brewsoak, does not exist yet;
  a real run creates it, and it then needs brew trust ericfitz/brewsoak.`
- Tapped from a remote: `ericfitz/brewsoak is tapped from <remote>; brewsoak
  only uses a local tap of that name; remove it with brew untap
  ericfitz/brewsoak.`
- `brew tap-new` failed: `brewsoak could not create its tap (<error>).`

**What brew records.** For every package brewsoak installs, upgrades or
previews with `--dry-run`, brew adds an `ericfitz/brewsoak/<name>` entry to its
trust store; brewsoak never removes them. A dry run also writes the same tap
files a real run does, and puts them back at the end, so the files under the
tap change for the length of the run. On `update` and on a real run, with the
tap local and trusted, brewsoak removes the tap file of a package that was
uninstalled or moved back to core.

**Plain `brew` at the same time.** Do not run brewsoak and a plain `brew
upgrade` or `brew install` at the same time. brewsoak's run lock keeps two
brewsoak runs apart and nothing else; a plain `brew` is not excluded, and a
package can be lost when the two overlap. A real `brewsoak upgrade`, `install`
or `reinstall` starts by removing tap files that no installed package uses, and
`brewsoak update` does the same after its refresh. We saw that start-of-run
step remove a package's tap file while a plain `brew upgrade` was running, and
brew then lost that package's keg. A run whose names are only own-tap tokens
skips that start-of-run prune. Let one finish before you start the other.

**Plain `brew` afterwards.** A package installed from the tap is anchored to it:

- Plain `brew upgrade` and `brew outdated` leave it alone. To move it on
  without brewsoak, use the bypass command from a hold or refusal note
  (below).
- A plain `brew install` of another package leaves its anchored dependencies at
  their soaked versions and installs missing ones from core.
- `brew untap ericfitz/brewsoak` is refused while packages are installed from
  it.
- `brew update` ignores the tap.
- When `HOMEBREW_VERIFY_ATTESTATIONS` is set, brew warns and skips the
  attestation check for bottles poured through the tap.
- While the tap exists and is untrusted, brew prints its untrusted-taps
  warning on your own installs.

| Package installed from the tap | Bypass command |
|---|---|
| Formula, `upgrade` or `install` | `brew upgrade homebrew/core/<name>` |
| Formula, `reinstall` | `brew reinstall homebrew/core/<name>` |
| Cask, `upgrade` or `install` | `brew upgrade --cask homebrew/cask/<token>` |
| Cask, `reinstall` | `brew reinstall --cask homebrew/cask/<token>` |
| Any other package | `brew <verb> <token>` |

A bypass leaves the package's receipt naming core again, and brewsoak treats it
like any other core package. A package is moved to the tap at its next soaked
upgrade; there is no up-front migration.

**If you untrust the tap** (`brew untrust ericfitz/brewsoak`, or a restored or
wiped trust store) while packages are installed from it, brew drops their
per-package trust, hides those formulae from `brew info --installed`, and
refuses plain installs of anything that depends on them. brewsoak still lists
them (formulae from the Cellar) and holds them with the not-ready note. The two ways
out are `brew trust ericfitz/brewsoak`, or moving a package back to core with
`brew upgrade homebrew/core/<name>`.

**No-soak packages.** A no-soak package installed from the tap is handed to
brew as `homebrew/core/<name>` (a cask as `homebrew/cask/<token>`), so brew
upgrades it from core and its receipt then names core. `brewsoak outdated`
takes its no-soak list from `brew outdated`, which does not list such a package
until it has been upgraded once this way. Its dependencies that came from the
tap stay at their soaked versions (see "Third-party taps").

**Third-party tap packages** never go through the tap. They keep the by-name
gate: brewsoak installs one by its bare name, only when brew's own current
definition is byte-for-byte the soaked cutoff, and the same holds for the
members of its dependency closure that brew might install or upgrade. Otherwise
the package is held, nothing is installed for it, the run continues with the
next package and exits 1. The `Notes` section says why, in the form
``<name>: [dependency <dep>: ]<reason>; use `<bypass>` to bypass brewsoak.``
The reasons:

- `brew would install <version>, which is not the soaked cutoff; it installs once that change soaks`
- `brew's <version> formula|cask changed since the soaked cutoff; it installs once the change soaks`
- `brew resolves the name to <tap>, not <origin>; brewsoak cannot install it by name`
- `brew could not describe it by name (<first line>)`, `brew could not read its dependencies (<first line>)`, and `it has no soaked cutoff yet`

A third-party package whose name also exists in `homebrew/core` is held, because
its bare name resolves to core. A third-party package from a tap brew does not
trust is held with `brew does not trust tap <tap>; run brew trust <tap>, or add
it to NO_SOAK`.

For packages from the tap, the hold texts are:

- ``<name>: [dependency <dep>: ]brew could not load its soaked copy (<first line>); use `<bypass>` to bypass brewsoak.``
- ``<name>: dependency <dep>: brew would replace installed <dep> <installed> with <version>, which has not soaked; <name> installs once it has; use `<bypass>` to bypass brewsoak.``
- ``<name>: dependency <dep>: brew cannot replace installed <dep> <installed> as a dependency; run brew upgrade <dep> first; use `<bypass>` to bypass brewsoak.``
- `<name>: held: its dependency <dep> was not installed from brewsoak's tap (<reason>)`.
  brewsoak installs a core or cask dependency from the tap before the target,
  and holds the target when that dependency does not end up installed from the
  tap, because brew would otherwise pull the dependency from core's current
  file. The reason is one of: brew's first error line, when its command
  failed; `brew left the installed version in place`, when brew answered that
  the package is already installed; brew's `Not upgrading <dep>, ...` line
  (without its `Warning: ` or `Error: ` prefix), when brew said it left the dependency
  alone; or `brew ran, but its keg is not from brewsoak's tap at the cutoff
  version`, when brew reported success and the receipt check then failed.
- `<name>: brew left the installed version in place: <reason>`. brew exited 0
  for a target but did not install it, so brewsoak counts it as `skipped`, not
  `upgraded`; it is not a failure and does not change the exit status. The
  reason is brew's own line without its `Warning: `/`Error: ` prefix, either
  `Not upgrading <name>, ...` (for example `no newer app version was detected`
  for a cask that updates itself) or an `already installed` line about the
  target; for a `homebrew/core` or `homebrew/cask` package with no such line,
  it is `its receipt does not name brewsoak's tap`, when no receipt of the
  package names brewsoak's tap.
- `held <name>: an earlier brewsoak run was interrupted`, with the note
  `<name>: an earlier brewsoak run was interrupted; run brewsoak update to
  finish it`. You normally see this on a dry run, because a real run settles
  leftovers from an earlier run before it starts; a settle that failed earlier
  in the same real run leaves the same state. `brewsoak update` or the next
  real run clears it.

**Leftovers.** A `brewsoakr/soaked` tap from an older brewsoak is never used,
fetched or written; remove it with `brew untap brewsoakr/soaked`. An explicit
`ericfitz/brewsoak/<name>` on the brewsoak command line is refused: name the
package without the tap.

**The repository `ericfitz/homebrew-brewsoak` does not exist on GitHub and must
never be created.** `brew install ericfitz/brewsoak/<name>` on a machine where
the tap is not tapped makes brew clone that repository and trust the named
package. If it existed, anyone who typed such a command would install whatever
it held, unsoaked, under brewsoak's tap name, and a machine that tapped it would
make brewsoak hold everything, because brewsoak only uses a local tap of that
name.

`upgrade`, `install` and `reinstall` run `brew update` once before any soaked
work, also on a dry run, so brew's data is fresh when brewsoak compares it. A
failed `brew update` is reported in `Notes` and fails the exit code, but does
not hold anything.

## Output

brewsoak summarizes `brew`'s install output: one line announcing each package
it changes, then a short line per download and install step. Bottle manifests,
plan previews, cleanup, `==>` markers, emoji, and `already installed and
up-to-date` notices are dropped from the terminal.

Nothing is lost. Every byte `brew` writes is appended to a per-run log under
`$TMPDIR`, and the path is printed at the end of the run.

A run prints headed sections (`Fetching Homebrew history`, `Fetching tap
history`, `Upgrading soaked packages`, `Updating Homebrew`, `Upgrading no-soak
packages (brew decides)`) with their lines indented beneath. A section with
nothing in it is omitted, except the soaked section of a bare `upgrade`,
which always prints with `(N of M)`. A dry run reads `Would upgrade …`.
Holds, skips, and deprecation warnings are collected into a `Notes` section
after the `Summary` table, grouped by kind with one package per line, so they
are not buried in the install scroll, and a `Caveats` section follows with the
caveats worth reading — brew's "shell completions
have been installed to ..." notices are dropped, anything telling you to run
something is kept.

Deprecations more than a year away are not reported. A package that is
deprecated or disabled upstream is never installed or upgraded, but one already
at its cutoff (or ahead of soak) is left alone with a warning, not held.

## Example

```bash
brewsoak update
brewsoak outdated
brewsoak upgrade
brewsoak info wget
brewsoak upgrade -v
```

A typical `upgrade` with nothing to do (a dry run here, taken from a real
run and trimmed) prints:

```
Fetching Homebrew history
  Homebrew/homebrew-core
  Homebrew/homebrew-cask
Fetching tap history
  anthropics/tap
  hashicorp/tap
Would upgrade soaked packages (0 of 177)
Updating Homebrew
  Already up-to-date.
Would upgrade no-soak packages (brew decides)
  ericfitz/tap/brewsoak 1.5.2
    Already up-to-date.
  ericfitz/tap/sqdist 0.6.0
    Already up-to-date.
Summary
  would upgrade    0
  already soaked 162
  soaking         10
  held             0
  ahead            1
  pinned           0
  skipped          0
  no-soak          2  (0 would upgrade, 2 already up-to-date)
  auto-updates     2
  installed       0B
  freed           0B
Notes
  Still soaking (left at the cutoff):
    awscli 2.38.3
    cats 15.0.3
  Ahead of soak (installed artifact left unchanged):
    uv
Full brew log: /var/folders/…/T/brewsoak-12345.log
```

A package already at its cutoff while a newer version is still inside the
soak window is counted as `soaking N` instead of `already soaked`. A bare
`upgrade` lists them under `Still soaking (left at the cutoff)` in `Notes`. A
named one (`brewsoak upgrade cats`) says which version is soaking and for how
long. `outdated` lists them under `==> Soaking`, and `info` shows
`up to date (soaked); 15.0.0 soaking`. brew's own "Outdated Formulae" list from
`brew update` is dropped, because it counts versions that are still soaking,
and so are its New and Deleted package lists. All of them stay in `--raw`
output and the brew log.

Casks that update themselves (`auto_updates true`) are left to the app on a
bare `upgrade`, as `brew upgrade` leaves them without `--greedy`; they are
counted as `auto-updates N` and `outdated` lists them under their own
heading. Name one (`brewsoak upgrade alt-tab`) to upgrade it anyway, or pass
`--greedy` (or `--greedy-auto-updates`) to upgrade all of them to their
soaked cutoff like any other cask. A `version :latest` cask is different:
its contents change without the cask changing, so brewsoak reinstalls one
under `--greedy` / `--greedy-latest` only when its cutoff definition differs
from the installed one, and otherwise leaves it with a note that the
contents of a `:latest` cask cannot be soaked (brew would reinstall it every
time). Since Homebrew 4 an installed cask carries only a version (the
Caskroom usually holds no cask source), so a cask is compared to its cutoff
by version alone.

An `upgrade` with work to do looks like:

```
Upgrading soaked packages (26 of 153)
  [1/26] upgrading aws-c-auth 0.10.5 -> 1.0.0
    downloading aws-c-common 1.0.0
    installing aws-c-common 1.0.0
    installed to /opt/homebrew/Cellar/aws-c-common/1.0.0 (109 files, 1MB)
    ...
Updating Homebrew
  Already up-to-date.
Upgrading no-soak packages (brew decides)
  upgrading ericfitz/tap/agentbus 1.15.0 -> 1.16.0
    installing agentbus 1.16.0
  ericfitz/tap/brewsoak 1.5.2
    Already up-to-date.
Summary
  upgraded            26
  already soaked     124
  soaking              0
  held                 0
  ahead                0
  pinned               0
  skipped              1
  no-soak              2  (1 upgraded, 1 already up-to-date)
  auto-updates         0
  installed      989.3MB
  freed            1.2GB
Notes
  bash: unparseable identity; skipping
Caveats
  git-lfs:
    Update your git config to finish installation:
      $ git lfs install
Full brew log: /var/folders/…/T/brewsoak-53417.log
```

Refusals tell you to run `brew upgrade <name>` (or install/reinstall) to
bypass brewsoak.

## Releasing

Maintainer notes. `release/` holds the scripts that build a universal binary,
sign and notarize it, attach it to a GitHub release, and render the Homebrew
formula for `ericfitz/homebrew-tap`. See [release/README.md](release/README.md)
for the step-by-step; the crates.io release is a separate `cargo publish`.
