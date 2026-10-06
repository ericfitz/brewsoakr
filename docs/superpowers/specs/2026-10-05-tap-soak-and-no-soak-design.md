# Tap soaking and the no-soak list

Date: 2026-10-05
Extends: [2026-08-12-brewsoakr-design.md](2026-08-12-brewsoakr-design.md)

Two features:

1. Third-party taps are soaked, with an optional per-tap soak time.
2. A `NO_SOAK` list of packages and taps that skip soaking and end in the
   same state as `brew upgrade`.

Motivating case: brewsoak itself ships from `ericfitz/tap`. With
`NO_SOAK = ["ericfitz/tap"]`, `brewsoak upgrade` keeps brewsoak current with
no soak delay.

## Human decisions (made by the maintainer, 2026-10-05)

These are architectural decisions made by the human maintainer during
design review. They are not to be changed without the maintainer's approval.

- **Supersedes the 2026-08-12 locks** "Third-party taps pass through to
  `brew`" and "Scope is homebrew-core formulae and homebrew-cask casks":
  every installed third-party tap is soaked by default. `NO_SOAK` is the
  exemption.
- **Per-tap soak time.** A `[[TAP]]` array in the config file. Each entry
  names a tap and optionally sets `soak_hours`. A tap with no entry, or an
  entry without `soak_hours`, uses `SOAK_HOURS`. There is no global
  tap-hours override.
- **`NO_SOAK` overrides every soak time**, however configured.
- **Tap history comes from brewsoak's own clones** in its cache, not from
  brew's tap repositories. brewsoak never fetches into, or moves the
  checkout of, a repo under `$(brew --repository)/Library/Taps`.
- **A no-soak package must end in the same state as `brew upgrade <pkg>`.**
  It is installed by `brew` itself from its real tap, not from a staged
  file.
- **Supersedes the 2026-08-12 lock** "Updating the Homebrew tool itself is
  out of scope", narrowly: brewsoak runs `brew update` once per run, and only
  when a no-soak package is involved, so no-soak packages get the true
  latest.
- **brewsoak never trusts a third-party tap on the user's behalf**
  (decided 2026-10-05, after Homebrew 7.0.8 refused a staged
  `hashicorp/tap/packer`). When brew refuses a staged copy because its tap is
  untrusted, brewsoak holds the package, exits 1, and tells the user to run
  `brew trust <tap>`. brewsoak keeps trusting its own `brewsoakr/soaked`
  staging tap only.
- **A formula's origin comes from its keg receipt, not `brew info`**
  (decided 2026-10-05, bug 6). `brew info`'s top-level `tap` reported
  `homebrew/core` for a staged `cyclonedx/cyclonedx` keg because core has a
  formula of the same name, so the origin record was never consulted.
  brewsoak reads `source.tap` from the keg's `INSTALL_RECEIPT.json` instead;
  null falls back to `origins.toml`, then core. Casks keep `brew info`.
- **A staged formula's origin can be read from its receipt path**
  (decided 2026-10-05). When `source.tap` is null and `source.path` is under
  `<cache>/staging/taps/<user>/<repo>/`, that tap is the origin, ahead of
  `origins.toml`. This recovers kegs whose record was never written (bug 3);
  `origins.toml` stays as the fallback.
- **Accepted consequence:** `brew upgrade` also upgrades a no-soak package's
  outdated dependencies to brew's latest. No-soak therefore extends to its
  dependencies. This is documented, not prevented.

## Configuration

File `~/.config/brewsoak/config.toml`:

```toml
SOAK_HOURS = 48                  # default for every package, core and cask included
NO_SOAK = ["ericfitz/tap", "wget", "hashicorp/tap/terraform"]

# Top-level keys must come before the first [[TAP]] table: TOML assigns any
# key written after a [[TAP]] header to that table.

[[TAP]]
name = "hashicorp/tap"
soak_hours = 72                  # optional; applies to every package in this tap

[[TAP]]
name = "cyclonedx/cyclonedx"     # no soak_hours: uses SOAK_HOURS
```

A `NO_SOAK` key found inside a `[[TAP]]` table is a misplaced top-level key.
brewsoak warns on stderr on every run (not only under `-v`), naming the fix,
and does not apply it, since a silently ignored no-soak list is the failure a
user would least expect.

### Effective soak hours for a package

1. Matches any `NO_SOAK` entry → not soaked (no-soak path).
2. Else its origin tap has a `[[TAP]]` entry with a valid `soak_hours` →
   that value.
3. Else the resolved `SOAK_HOURS` (CLI > `BREWSOAK_SOAK_HOURS` > file >
   24, unchanged).

### `[[TAP]]` rules

- File only. No CLI flag, no environment variable.
- `name` is `user/repo`, matched case-insensitively. `homebrew/core` and
  `homebrew/cask` are allowed, so core and cask can have their own times.
- `soak_hours` must be an integer ≥ 1. Missing or invalid falls back to
  `SOAK_HOURS`; invalid values are reported under `-v`.
- An entry with a missing or malformed `name` is skipped and reported under
  `-v`.
- Duplicate names: the last entry wins; reported under `-v`.

### `NO_SOAK` rules

| Entry | Matches |
|---|---|
| `wget` (no slash) | A formula or cask named `wget` from any origin |
| `ericfitz/tap` (one slash) | Every package whose origin is that tap |
| `hashicorp/tap/terraform` (two slashes) | That package from that tap only |

- A one-slash entry is always a tap. Homebrew package names never contain
  `/` (`@` is used for versions, e.g. `python@3.12`).
- Matching is case-insensitive.
- `homebrew/core` and `homebrew/cask` are valid tap entries.
- Empty strings, more than two slashes, or empty path segments are
  malformed: skipped and reported under `-v`.
- Not a TOML array of strings → the whole key is ignored, reported under
  `-v`.

### Invalid config overall

Unchanged from v1: unreadable file or bad TOML is silently ignored (defaults
apply). Individual bad keys or entries fall back as above.

### Persisting `--soak-hours`

`apply_persist` becomes a key-level edit.

- `--soak-hours N`, N ≠ 24: set `SOAK_HOURS = N`, leaving every other key,
  `[[TAP]]` and `NO_SOAK` as they were.
- `--soak-hours 24`: remove the `SOAK_HOURS` key. Delete the file only if no
  keys remain.
- If the existing file is not valid TOML, the write does not overwrite it:
  report a warning on stderr and leave the file alone (the CLI value still
  applies to this run).
- Comments are not preserved. Key order may change.

## Package origin

A package's origin is the tap it belongs to:

1. The tap the installed package came from, if non-empty and not
   brewsoak's staging tap. For a formula this is `source.tap` in the keg's
   own `INSTALL_RECEIPT.json` (null after a staged install). When it is
   null and the receipt's `source.path` lies under brewsoak's own
   `<cache>/staging/taps/<user>/<repo>/`, the origin is `<user>/<repo>`. The top-level
   `tap` in `brew info --json=v2 --installed` is the tap brew resolves the
   name to now, not the keg's receipt, so it is not used for formulae. For a
   cask it is `tap` in `brew info --json=v2 --installed`.
2. Else brewsoak's origin record, `<cache>/origins.toml`.
3. Else `homebrew/core` for formulae, `homebrew/cask` for casks (v1
   behavior).

`origins.toml` maps `"<kind>:<name>"` to `"user/repo"`, kind being
`formula` or `cask`:

```toml
"formula:terraform" = "hashicorp/tap"
```

- Written after a successful staged install of a tap package.
- A staged install of a core or cask package removes any entry for that
  kind and name.
- If clearing the cache loses the record, the package falls back to
  core/cask. If its name does not resolve there, the run notes
  `origin unknown; reinstall it from its tap with brew`.

## Tap classification

From `brew tap-info --json --installed`:

| Tap | Treatment |
|---|---|
| `homebrew/core`, `homebrew/cask` | Existing v1 snapshots |
| brewsoak's staging tap | Ignored |
| Remote missing (`null`) or not `https://` | Unsoakable |
| Everything else | Soakable |

Unsoakable tap packages are never installed by brewsoak. A bare `upgrade` or
`outdated` adds a note:
`<name>: tap <tap> has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK`.
An explicitly named unsoakable package is refused with the usual bypass
hint. A no-soak package from an unsoakable tap is fine: brew handles it.

## Tap snapshots

### Which taps refresh

Only taps that are needed: soakable taps with at least one installed package
that is not no-soak, plus the tap of any explicitly named `user/tap/pkg` that
is not no-soak.

### Clone

- Bare clone at `<cache>/taps/<user>/<repo>.git`.
- Fetch the remote's default branch with full commit history and
  `--filter=blob:none` (blobs are fetched on demand). If the server rejects
  the filter, retry without it.
- HEAD = the fetched tip. Cutoff =
  `git rev-list -1 --before=<now − tap hours> <head>` (committer time).
- `refs/brewsoak/cutoff` and `refs/brewsoak/head` are force-updated pins.
- No GitHub API calls for taps (avoids the 60/hour unauthenticated limit;
  works for any HTTPS host).
- Every git call goes through `GitStore` / `ProcessGit`. Every failure is
  `Error::Git { action, detail }`.

### No cutoff commit

If no commit is older than the tap's cutoff (the tap is younger than its
soak window), the tap has no cutoff tree. Every package in it is "too new"
and refused or held.

### Name resolution

List the commit's tree once per refresh (`git ls-tree -r --name-only`) and
resolve in Homebrew's order:

- Formula: `Formula/<name>.rb`, then `Formula/**/<name>.rb`, then
  `HomebrewFormula/<name>.rb`, then `<name>.rb` at the repo root.
- Cask: `Casks/<name>.rb`, then `Casks/**/<name>.rb`.

Tap aliases are not resolved. Renames are not followed (same as core).

### State

`state.toml` gains a table per refreshed tap:

```toml
[taps."hashicorp/tap"]
hours = 72
cutoff = "<sha or empty when no cutoff>"
head = "<sha>"
cutoff_time = "<rfc3339>"
```

State files without `taps` still load. `outdated` and `info` reuse the
stored snapshot unless the tap has none or its stored `hours` differs from
its effective hours; then they refresh that tap.

### Failure isolation

- A tap's fetch failure holds only that tap's packages, with an
  `Error::Git` note. Other taps continue.
- Core or cask refresh failure aborts the run (unchanged).

## Commands

### Inventory

`brew info --json=v2 --installed` no longer drops third-party packages.
Every package gets an origin and a class:

- **no-soak**: matches `NO_SOAK`.
- **soaked**: everything else in core, cask, or a soakable tap.
- **unsoakable**: in an unsoakable tap and not no-soak.

### Soaked tap packages

Same desired-state table and identity rules as core/cask (cutoff vs HEAD by
raw blob; installed vs cutoff/HEAD by parsed identity; survival = resolves
at HEAD with no `deprecate!`/`disable!`).

- Stage the cutoff blob at
  `<cache>/staging/taps/<user>/<repo>/{Formula,Casks}/<name>.rb` (per-tap
  directory, so a tap formula never collides with a core one). Core and cask
  staging is unchanged.
- Install with the existing path-install invocation and env.
- On success, write the origin record.
- Dependency closure: walk deps across origins. A dep reported as
  `user/tap/dep` resolves to that tap. A bare dep name resolves to the
  dependent's own tap first, then core. Each dep is staged from its own
  origin's cutoff snapshot. Deps that are no-soak or unsoakable are left to
  brew.
- If brew cannot load a staged tap formula (e.g. `require_relative` to a
  file elsewhere in the tap), hold the package with a note:
  `<name>: cannot be installed from a staged copy; use brew, or add it to NO_SOAK`.
- If brew refuses the staged copy because the tap is untrusted (Homebrew
  7.0.8+: `Refusing to load formula <tap>/<name> from untrusted tap <tap>`),
  hold the package with a note:
  `<name>: brew does not trust tap <tap>; run brew trust <tap>, or add it to NO_SOAK`.
  See the 2026-10-05 human decision on tap trust.

### No-soak packages

For mutating commands (`upgrade`, `install`, `reinstall`):

1. Run all soaked work first.
2. If any no-soak package is involved, run `brew update` once (output to the
   run log via the summarizer). `brewsoak update` also runs it when any
   installed package is no-soak.
3. Run one `brew upgrade` (or `install` / `reinstall`) with every no-soak
   package as a full token: `name` for core and cask, `user/repo/name` for
   tap packages. `HOMEBREW_NO_AUTO_UPDATE=1` stays set (brew was just
   updated).
4. Tap switch: when a no-soak package's installed receipt has an empty tap
   (it was staged by brewsoak) and its origin is a third-party tap, use
   `brew reinstall <user/repo/name>` instead of `upgrade`, so it lands on its
   real tap. `brew install` was tried first and answers "already installed"
   without replacing the keg; `reinstall` does replace it, and the receipt's
   `source.tap` becomes the tap. After a successful switch the package's
   `origins.toml` record is removed. If brew does not replace the keg, the
   run exits 1 with a note telling the user to uninstall and install it.
5. `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1` stays set. This is the one
   deliberate difference from a hand-run `brew upgrade`: soaked dependents of
   a no-soak package are not upgraded by it.
6. A failed `brew update` fails the no-soak step only: no-soak packages are
   reported as failed with brew's exit code; soaked results stand.

### Explicit names

- `user/tap/pkg` is no longer passed through. It is soaked, no-soak, or
  refused (unsoakable), per its class.
- `info` shows origin, effective soak hours, and `no-soak` where it applies.
- `outdated` lists no-soak packages from brew's current view
  (`brew outdated --json=v2`), without running `brew update`, labeled
  `(no-soak, brew)`.

### Output

- The counts line gains `no-soak N`.
- Notes cover: unsoakable taps, held taps (fetch failures), invalid config
  entries (`-v`), staged-load failures, unknown origin.
- `-v` prints each package's origin and effective soak hours.

## Exit status

Unchanged table. No-soak brew failures follow the existing "brew failed for
an eligible package" row.

## Testing

Unit tests with the existing fakes (`MockBrew`, `InMemoryGit`,
`StaticGithub`):

- Config: `[[TAP]]` and `NO_SOAK` parsing; invalid and duplicate entries;
  `--soak-hours` persistence keeps `[[TAP]]` and `NO_SOAK`; file deleted
  only when empty; invalid TOML not overwritten.
- Matching: three entry forms, case-insensitivity, origin resolution order.
- Tap snapshots: cutoff via `rev-list --before`; no cutoff commit; per-tap
  hours give different cutoffs; one failing tap does not block others; skip
  rules (null remote, SSH remote, staging tap); state round-trip including
  the v1 format.
- Name resolution: search order, root-level formula, sharded `Formula/`.
- Flows: soaked tap install stages under the tap directory and writes
  `origins.toml`; no-soak runs `brew update` exactly once and only when
  needed, then one `brew upgrade` with full tokens; tap-switch uses
  `reinstall`; dep closure crosses taps; explicit unsoakable is refused.
- Golden: an `upgrade` summary with `no-soak N` and the new notes.

Manual verification against real brew (plan steps):

- `NO_SOAK = ["ericfitz/tap"]`: `brewsoak upgrade` brings brewsoak to latest
  from `ericfitz/tap`.
- A soaked `hashicorp/tap` package installs from its staged copy.
- The tap-switch case replaces the keg cleanly.

## Docs

- README: new config keys, taps are soaked, no-soak covers dependencies,
  and replace "brewsoak itself lives in a third-party tap, so it is never
  soaked" with advice to add `ericfitz/tap` to `NO_SOAK`.
- AGENTS.md: origin and staging rules.

## Out of scope

- Private taps over SSH (unsoakable unless the remote is HTTPS and fetches
  without prompting).
- Tap aliases and renames.
- CLI or env control of `[[TAP]]` or `NO_SOAK`.
