# brewsoak

A Homebrew wrapper that delays `homebrew/core` and `homebrew/cask` updates
for a soak window. This gives security researchers time to discover and yank
a compromised package before you install it.

Third-party taps and every other `brew` subcommand pass through unchanged.
There is no soak bypass flag: run `brew` directly if you need HEAD now.

## Install

### Homebrew (macOS, recommended)

```bash
brew install ericfitz/tap/brewsoak
```

Installs a prebuilt, code-signed and notarized universal (Apple Silicon +
Intel) binary from the [GitHub release](https://github.com/ericfitz/brewsoakr/releases).
Upgrade later with `brew upgrade brewsoak` (brewsoak itself lives in a
third-party tap, so it is never soaked).

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
`N == 24` deletes the config file.

## Commands

| Command | What it does |
|---|---|
| `update` | Refresh cutoff/HEAD snapshots. Does not update Homebrew itself. |
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
