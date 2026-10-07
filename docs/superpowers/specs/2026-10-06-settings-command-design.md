# `brewsoak settings`: manage the config from the command line

Date: 2026-10-06
Issue: #1
Extends: [2026-10-05-tap-soak-and-no-soak-design.md](2026-10-05-tap-soak-and-no-soak-design.md)

Today every config change except `SOAK_HOURS` means hand-editing
`~/.config/brewsoak/config.toml`. This adds a `settings` subcommand that shows
the effective config and edits `SOAK_HOURS`, `NO_SOAK`, and `[[TAP]]`
`soak_hours`, keeping comments and unknown keys.

## Human decisions (made by the maintainer, 2026-10-06)

These are architectural decisions made by the human maintainer during
design review. They are not to be changed without the maintainer's approval.

- **Command name is `brewsoak settings`.** `brewsoak config` stays a
  passthrough to `brew config`; config edits are not top-level flags.
- **Command syntax** (approved as proposed):
  `brewsoak settings [show]`,
  `brewsoak settings no-soak add|remove TOKEN...`,
  `brewsoak settings tap-hours USER/REPO N|--clear`,
  `brewsoak settings soak-hours N|--clear`.
- **Atomic write with a backup, no gap.** Write a temp file in the config
  directory, hard-link the current `config.toml` to
  `config.toml.<UTC datetime>.bak`, then rename the temp file over
  `config.toml`. Readers see either the old file or the new one, never none.
- **Keep the 2 most recent backups.** Older `.bak` files are deleted on each
  write.
- The behavior and component sections below were approved section by
  section.

## Behavior

### Commands

| Command | Effect |
|---|---|
| `settings` / `settings show` | Print the effective config (below). Never writes. |
| `settings soak-hours N` | Set `SOAK_HOURS = N`. `N == 24` removes the key (same rule as `--soak-hours`). |
| `settings soak-hours --clear` | Remove `SOAK_HOURS`. |
| `settings no-soak add TOKEN...` | Append each token to `NO_SOAK` that is not already there. |
| `settings no-soak remove TOKEN...` | Remove each matching entry from `NO_SOAK`. |
| `settings tap-hours USER/REPO N` | Set that tap's `soak_hours`, creating its `[[TAP]]` entry if missing. |
| `settings tap-hours USER/REPO --clear` | Remove that tap's `soak_hours`; remove the whole entry if only `name` remains. |
| `settings repair` | Fix what the reader notes or warns about (see the section below). |

`N` is an integer ≥ 1 (`SoakHours::new`). `homebrew/core` and `homebrew/cask`
are valid `USER/REPO` values.

### `show` output

- Global soak hours and their source: `--soak-hours` is rejected with
  `settings` (below), so the source is `BREWSOAK_SOAK_HOURS`, the file, or the
  default 24. The config file path, and `(no config file)` when absent.
- `NO_SOAK` entries as written in the file, one per line.
- Each `[[TAP]]` entry with its effective hours, marked `(own)` or
  `(SOAK_HOURS)`.
- All parse notes and warnings (invalid entries, `NO_SOAK` inside `[[TAP]]`),
  always, not only under `-v`.

### Validation and idempotence

- `NO_SOAK` tokens are validated by `nosoak::parse_entry`, the same parser the
  config reader uses. Any invalid token fails the whole command with
  `Error::Usage` naming every bad token; nothing is written.
- Comparison is case-insensitive. New entries are written lowercased.
- Adding an entry already present (in any case) is a no-op reported as
  `already in NO_SOAK: <token>`.
- Removing an absent entry is a no-op reported as `not in NO_SOAK: <token>`,
  exit 0.
- `tap-hours` validates `USER/REPO` with the same rule as the `[[TAP]]`
  reader (exactly two non-empty segments).
- `soak-hours` warns on stderr when `BREWSOAK_SOAK_HOURS` is set, because the
  environment overrides the file.
- Checks are syntactic only. `settings` never runs `brew` or `git`, never
  checks whether a tap is installed, and writes no per-run log.

### CLI rules

- Unknown verbs, missing arguments, and extra arguments are `Error::Usage`
  (exit 2). No branch accepts unrecognized input.
- `--soak-hours` together with `settings` is `Error::Usage`, pointing at
  `settings soak-hours`.
- `--raw` is accepted and has no effect.
- `brewsoak help settings` and `brewsoak settings --help` / `-h` print the
  settings help. The main `--help` lists `settings`.

## File handling

### Format-preserving edits

Edits go through `toml_edit` (`DocumentMut`), pinned to `0.22` to match the
copy `toml 0.8` already brings in. Comments, key order, and unknown keys are
kept.

- A new top-level key (`SOAK_HOURS`, `NO_SOAK`) is placed before the first
  `[[TAP]]` table, so TOML does not assign it to that table (AGENTS.md config
  rule). The writer never produces `NO_SOAK` inside `[[TAP]]`.
- An existing misplaced `NO_SOAK` inside a `[[TAP]]` is left untouched; the
  existing stderr warning still fires.
- A new `[[TAP]]` entry is appended after the last one.
- `--soak-hours` persistence (`config::apply_persist`) is rebuilt on the same
  editor and writer, so it keeps comments too. Its behavior on invalid TOML
  (warn, don't write, still apply for this run) is unchanged.

### Invalid TOML

`settings` writes against a file that is not valid TOML fail with
`Error::Refusal` (exit 1) naming the path and the parse error. The file is
not touched. `settings show` prints the same refusal.

### `config::write_atomic(path, new_body)`

1. If `new_body` equals the current contents, return without writing; no
   backup is made.
2. Create the config directory if missing.
3. Write `new_body` to `.config.toml.tmp-<pid>` in the same directory with mode
   0600, and fsync it.
4. If `config.toml` exists, hard-link it to `config.toml.<YYYYMMDDTHHMMSSZ>.bak`
   (UTC). If backups for that second exist, use the next suffix above the highest one (`...Z-2.bak`, `...Z-3.bak`, and so on). If
   hard-linking fails, copy instead.
5. Rename the temp file over `config.toml`. When `new_body` is empty or only whitespace (a file holding just comments is kept), remove
   `config.toml` instead of renaming (the backup from step 4 is kept), and
   remove the temp file.
6. Delete all but the 2 newest `config.toml.*.bak` files (newest by name,
   which sorts by timestamp then suffix).
7. Remove any `.config.toml.tmp-*` files older than 10 minutes left by earlier
   failed runs.

On any I/O error the temp file is removed and the error is returned as
`Error::Io`.

### State

- **Second run** of the same edit: no change, so no write and no backup.
- **Two runs overlapping**: there is no lock (hand-run command). Each rename
  is atomic; the last writer wins and both leave backups. Readers never see a
  missing or partial file.
- **Leftovers from a failed run**: a stray `.config.toml.tmp-*` is not read by
  anything and is removed by the next write once it is older than 10 minutes
  (a fresher one may belong to a concurrent run). A stray extra `.bak` is
  pruned by the next write.

## Components

- `src/settings.rs` (new): pure edits on `toml_edit::DocumentMut` —
  `set_soak_hours`, `clear_soak_hours`, `no_soak_add`, `no_soak_remove`,
  `tap_hours_set`, `tap_hours_clear`, and `render_show`. Each edit returns
  whether it changed the document plus messages to print. No I/O.
- `src/config.rs`: `write_atomic`; `apply_persist` rebuilt on
  `settings::set_soak_hours`/`clear_soak_hours` + `write_atomic`.
- `src/cli.rs`: `Command::Settings(SettingsCmd)` with strict parsing and
  `command_help("settings")`.
- `src/lib.rs`: dispatch `Settings` before any brew, git, or snapshot work.

## Testing

- `settings.rs`: add/remove/set/clear round-trips; duplicate add including a
  case-only difference; removing an absent entry; invalid tokens (nothing
  changed); comments and unknown keys preserved; top-level keys inserted
  above `[[TAP]]` in a file with only `[[TAP]]` tables and in an empty file;
  misplaced `NO_SOAK` in `[[TAP]]` left alone; tap clear removing only
  `soak_hours` vs the whole entry; `show` rendering with notes.
- `write_atomic`: unchanged body makes no write and no backup; backup created;
  pruning to 2; same-second suffix; empty body removes the file and keeps a
  backup; missing directory created; stale temp files removed.
- `cli.rs`: parse matrix for every verb and each usage error.
- `lib.rs` (fake world): `settings` never calls brew; `--soak-hours` with
  `settings` is rejected; existing `--soak-hours` persistence tests still
  pass, plus one that a comment survives persistence.

## Docs

README "Configuration" gains a `brewsoak settings` section, and the sentence
"`[[TAP]]` and `NO_SOAK` are file-only" is updated. `--help` lists `settings`.

## `settings repair` (issue #5, maintainer decisions 2026-10-06)

These are human decisions made by the maintainer; do not change them without
the maintainer's approval.

- **Scope.** Invalid and non-string `NO_SOAK` entries are removed. A lone
  valid `NO_SOAK` string is converted to a one-element array. A `NO_SOAK`
  written inside a `[[TAP]]` table has its valid entries merged into the
  top-level list (created above the first `[[TAP]]` if missing) and the
  misplaced key is removed. Invalid `[[TAP]]` entries are removed, a bad
  `soak_hours` is removed (the entry too if only `name` is left), and for
  duplicate `[[TAP]]` names the last one is kept. Valid entries keep their
  text, order, comments, and layout.
- **Refusals.** A `NO_SOAK` that is not an array and not a valid string, and
  a `TAP` that is not an array of tables, are refused and nothing is written.
- **No-op.** Nothing to repair prints `nothing to repair`, writes nothing,
  and makes no backup.
