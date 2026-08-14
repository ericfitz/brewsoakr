# brewsoakr

A Homebrew wrapper that delays `homebrew/core` and `homebrew/cask` updates
for a soak window so a compromised package can be yanked before you install it.

Third-party taps and every other `brew` subcommand pass through unchanged.
There is no soak bypass flag: run `brew` directly if you need HEAD now.

## Install

```bash
git clone git@github.com:ericfitz/brewsoakr.git
cd brewsoakr
cargo install --path .
```

Or run the debug binary after `cargo build`:

```bash
cargo build
./target/debug/brewsoakr --version
```

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
| `info` | Installed / cutoff / HEAD and the action brewsoakr would take. |
| `--version` / `-V` | Print `brewsoakr <version>`. |
| `--help` / `-h` | brewsoakr help. `help install` is soak-aware; `help services` is `brew help`. |

Other flags (`--formula`, `--cask`, `--debug`, …) are forwarded to `brew`.

`-v` / `--verbose` prints the soak window, cutoff SHAs and times, and a
line for every package evaluated (what happened and why).

## Example

```bash
brewsoakr update
brewsoakr outdated
brewsoakr upgrade
brewsoakr info wget
brewsoakr upgrade -v
```

A typical `upgrade` with nothing to do prints a counts line such as:

```
upgraded 0, already soaked 137, held 0, ahead 0, pinned 0, skipped 0
already soaked: 137 formulae and casks
```

Refusals tell you to run `brew upgrade <name>` (or install/reinstall) to
bypass brewsoakr.
