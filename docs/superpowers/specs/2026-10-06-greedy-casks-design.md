# Honor `--greedy` for self-updating casks

Date: 2026-10-06
Issue: #3

A bare `brewsoak upgrade` leaves casks with `auto_updates true` to the app
(`LeaveAutoUpdates`), as brew does without `--greedy`. brewsoak forwards the
greedy flags to brew where brew accepts them but never acts on them itself.

## Human decisions (made by the maintainer, 2026-10-06)

These are architectural decisions made by the human maintainer during
design review. They are not to be changed without the maintainer's approval.

- **`version :latest` casks keep the soak promise.** Under `--greedy` or
  `--greedy-latest`, a `:latest` cask is reinstalled only when its cutoff cask
  definition differs from the installed one (the existing identity
  comparison). When they match, brewsoak leaves it and says the contents of a
  `:latest` cask cannot be soaked. This departs from brew, which reinstalls
  every `:latest` cask on each greedy upgrade.

## Behavior

| Flag | Self-updating casks (`auto_updates true`) | `version :latest` casks |
|---|---|---|
| none | left to the app (unchanged) | unchanged |
| `--greedy` | upgraded to the cutoff like any cask | rule above |
| `--greedy-auto-updates` | upgraded to the cutoff | unchanged |
| `--greedy-latest` | left to the app | rule above |

- A `:latest` cask that also has `auto_updates true` is governed by both rows:
  `--greedy-auto-updates` lifts the auto-updates leave; the identity rule still
  decides whether anything changes.
- `brewsoak outdated` with the same flags lists greedy-eligible casks under
  "Outdated (will upgrade)" instead of "Auto-updates", and with `-v` notes
  each `:latest` cask left because its contents cannot be soaked.
- `bare_action` takes the greedy mode; `plan_size`, `apply_resolved`, and the
  outdated report all call it, so the count, the upgrade, and the report
  agree.
- Flags are read from the brew args already forwarded (`src/flags.rs`); the
  install-path filter that drops `--greedy*` from `brew install <file>.rb`
  is unchanged.

## Testing

Unit tests for each flag and for the bare run (unchanged), covering
`bare_action`, `plan_size`, and the outdated grouping; a `:latest` cask with a
matching cutoff is left with the note, and one with a changed cutoff
definition is reinstalled.
