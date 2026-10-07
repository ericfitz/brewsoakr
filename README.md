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
`-v`. A `NO_SOAK` key inside a `[[TAP]]` table is ignored, with a warning on
stderr every run.

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
```

`show` prints the effective soak hours and their source (`BREWSOAK_SOAK_HOURS`,
the file, or the default 24), `NO_SOAK` as written, every `[[TAP]]` with its
effective hours, and every parse note and warning. It never writes.

Edits keep comments, key order, and unknown keys. New top-level keys go above
the first `[[TAP]]`; a new `[[TAP]]` is appended after the last one.
`soak-hours 24` removes `SOAK_HOURS` (24 is the default). `no-soak` tokens
are validated like the file's entries, compared case-insensitively, and
written lowercased; adding a present token or removing an absent one is
reported and is not an error. `no-soak remove` validates its tokens like `add`, so an invalid entry
already in the file must be removed by hand. `tap-hours USER/REPO --clear` removes the
entry when only `name` would remain. When nothing is left in the file, the
file is removed.

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
blobless clone under brewsoak's cache. brew's own tap checkouts are never
touched. A tap with no remote, or a non-HTTPS remote, is not soakable:
brewsoak notes its packages and leaves them to `brew`, unless they are in
`NO_SOAK`.

No-soak packages are installed by `brew` itself from their real tap after all
soaked work: one `brew update`, then one `brew upgrade` (or `install` /
`reinstall`) naming every no-soak package. `brew upgrade` also upgrades a
no-soak package's outdated dependencies to brew's latest, so no-soak extends
to its dependencies. Soaked dependents of a no-soak package are not upgraded
by that step.

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

Other flags (`--formula`, `--cask`, `--debug`, …) are forwarded to `brew`.

`-v` / `--verbose` prints the soak window, cutoff SHAs and times, and a
line for every package evaluated (what happened and why).

`--raw` turns off output summarizing and forwards `brew`'s output byte for
byte.

## Output

brewsoak summarizes `brew`'s install output: one line announcing each package
it changes, then a short line per download and install step. Bottle manifests,
plan previews, cleanup, `==>` markers, emoji, and `already installed and
up-to-date` notices are dropped from the terminal.

Nothing is lost. Every byte `brew` writes is appended to a per-run log under
`$TMPDIR`, and the path is printed at the end of the run.

Holds, skips, and deprecation warnings are collected into a `notes:` block
after the counts line so they are not buried in the install scroll, and
`caveats:` follows with the caveats worth reading — brew's "shell completions
have been installed to ..." notices are dropped, anything telling you to run
something is kept.

Deprecations more than a year away are not reported.

## Example

```bash
brewsoak update
brewsoak outdated
brewsoak upgrade
brewsoak info wget
brewsoak upgrade -v
```

A typical `upgrade` with nothing to do prints a counts line such as:

```
upgraded 0, already soaked 137, held 0, ahead 0, pinned 0, skipped 0
already soaked: 137 formulae and casks
```

Casks that update themselves (`auto_updates true`) are left to the app on a
bare `upgrade`, as `brew upgrade` leaves them without `--greedy`; they are
counted as `auto-updates N` and `outdated` lists them under their own
heading. Name one (`brewsoak upgrade alt-tab`) to upgrade it anyway. Since Homebrew 4
an installed cask carries only a version (the Caskroom usually holds no cask
source), so a cask is compared to its cutoff by version alone.

An `upgrade` with work to do looks like:

```
upgrading 26 of 150 packages
[1/26] upgrading aws-c-auth 0.10.5 -> 1.0.0
  downloading aws-c-common 1.0.0
  installing aws-c-common 1.0.0
  installed to /opt/homebrew/Cellar/aws-c-common/1.0.0 (109 files, 1MB)
  ...
upgraded 26, already soaked 124, held 0, ahead 0, pinned 0, skipped 1
installed 989.3MB, freed 1.2GB
notes:
  bash: unparseable identity; skipping
caveats:
  git-lfs:
    Update your git config to finish installation:
      $ git lfs install
full brew log: /var/folders/.../brewsoak-53417.log
```

Refusals tell you to run `brew upgrade <name>` (or install/reinstall) to
bypass brewsoak.

## Releasing

Maintainer notes. `release/` holds the scripts that build a universal binary,
sign and notarize it, attach it to a GitHub release, and render the Homebrew
formula for `ericfitz/homebrew-tap`. See [release/README.md](release/README.md)
for the step-by-step; the crates.io release is a separate `cargo publish`.
