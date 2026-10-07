# `brewsoak settings` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `brewsoak settings` (show / `soak-hours` / `no-soak add|remove` / `tap-hours`) that edits `~/.config/brewsoak/config.toml` in place while keeping comments, key order and unknown keys, writing atomically with timestamped backups, and rebuild `--soak-hours` persistence on the same editor and writer.

**Architecture:** A new pure module `src/settings.rs` applies each edit to a `toml_edit::DocumentMut` and reports whether the document changed plus the lines to print; it does no I/O. `src/config.rs` gains `write_atomic` (temp file, hard-link backup, rename, prune, stale-temp sweep) with an injected clock, and `apply_persist` is rebuilt on the editor plus `write_atomic`. `src/cli.rs` parses `settings` strictly into `Command::Settings(SettingsCmd)`; `src/lib.rs` dispatches it right after argument parsing, before config resolution, brew, git or snapshot work.

**Tech Stack:** Rust 2024, `toml_edit = "0.22"` (new direct dependency; `toml 0.8` already pulls 0.22.27), `toml 0.8` (unchanged reader), `time 0.3` (`format_description::parse_borrowed::<2>`, no new feature), `tempfile` (dev). Fakes: `MockBrew`, `InMemoryGit`, `StaticGithub` through the existing `World` trait.

**Spec:** `docs/superpowers/specs/2026-10-06-settings-command-design.md` (extends `docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md`). The spec's "Human decisions" section is frozen: command name `settings`, the four verb syntaxes, atomic write with hard-link backup and no gap, keep the 2 most recent backups.

## Verified facts (probed against toml_edit 0.22.27 and time 0.3.55 before writing this plan)

These drive the exact strings the tests pin. Do not "fix" them from memory.

- toml_edit always renders root key-values before any `[[TAP]]` table, even when a key is inserted into a document that was parsed from a file holding only `[[TAP]]` tables. It does not add a blank line between them, so the editor adds one itself (`separate_taps`).
- `doc["SOAK_HOURS"] = value(12)` drops a trailing `# comment` on that line. Replacing the `Value` in place while copying the old value's `Decor` keeps it.
- `Array::push` on a multi-line array renders `"curl", "x",` on one line; the editor copies the last element's indentation prefix (not its suffix, which may hold a comment) and uses `push_formatted`. A comment that sits after the last comma is the array's `trailing` and moves after the new last element; do not write a test claiming it stays.
- `Array::retain` keeps a trailing `# comment` on the `NO_SOAK` line; removing the last element leaves `NO_SOAK = []`.
- Removing one `[[TAP]]` entry takes its own leading comment with it and leaves its neighbors' comments. Removing the `TAP` key after the last entry renders an empty document.
- `"[[["` fails `str::parse::<DocumentMut>()` with a `Display` that starts `TOML parse error at line 1, column 3`. A comment-only file parses to a root table with `len() == 0` and renders back unchanged.
- `ArrayOfTablesIter` is `Box<dyn Iterator>` (not double-ended): find the last match with `enumerate().filter().last()`, not `rposition`.
- `OffsetDateTime::from_unix_timestamp(1_700_000_000)` formats as `20231114T221320Z` with `parse_borrowed::<2>("[year][month][day]T[hour][minute][second]Z")`. `format_description::parse` is deprecated; the `macros` feature is not enabled, so no `format_description!` macro.
- As plain strings, `config.toml.20231114T221320Z-2.bak` sorts **before** `config.toml.20231114T221320Z.bak` (`-` < `.`), so "newest by name" must parse `(stamp, n)`; a raw sort would prune the wrong file on a same-second write.

## Global Constraints

- Command name is `brewsoak settings`; `brewsoak config` stays a passthrough to `brew config`.
- Syntax, exactly: `settings [show]`, `settings no-soak add|remove TOKEN...`, `settings tap-hours USER/REPO N|--clear`, `settings soak-hours N|--clear`. `N` is an integer ≥ 1 (`SoakHours::new`); `N == 24` removes `SOAK_HOURS`.
- Unknown verbs, missing arguments and extra arguments are `Error::Usage` (exit 2). No branch accepts unrecognized input. `--soak-hours` with `settings` is `Error::Usage` pointing at `settings soak-hours`. `--raw` is accepted and has no effect.
- `settings` never runs `brew` or `git`, never checks whether a tap is installed, and writes no per-run log. Checks are syntactic.
- Edits go through `toml_edit::DocumentMut`, dependency pinned `toml_edit = "0.22"`. Comments, key order and unknown keys are kept. A new top-level key lands before the first `[[TAP]]`; the writer never produces `NO_SOAK` inside `[[TAP]]`; a misplaced one is left untouched and the existing stderr warning still fires. A new `[[TAP]]` is appended after the last one.
- Invalid TOML: `settings` (show and edits) fails with `Error::Refusal` (exit 1) naming the path and the parse error; the file is not touched. `apply_persist` keeps its warn-don't-write behavior.
- `write_atomic`: no write and no backup when the body is unchanged; temp file `.config.toml.tmp-<pid>` in the config directory, mode 0600, fsynced; existing file hard-linked to `config.toml.<YYYYMMDDTHHMMSSZ>.bak` (UTC), `-2`, `-3` suffixes on collision, copy if linking fails; rename over; a whitespace-only body removes `config.toml` (a comment-only file is kept); keep the 2 newest backups; remove stale `.config.toml.tmp-*`; any I/O error removes the temp file and returns `Error::Io`.
- `NO_SOAK` tokens are validated by `nosoak::parse_entry`; any bad token fails the whole command with `Error::Usage` naming every bad token. Comparison is case-insensitive; new entries are written lowercased. `USER/REPO` uses the `[[TAP]]` reader's rule (exactly two non-empty segments); `homebrew/core` and `homebrew/cask` are valid.
- Done gate after every task and at the end: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`. Baseline: all pass before Task 1.
- Branch `issue-1-settings-command`; each task commits there; the orchestrator squashes the issue into one commit on `main`. Commit messages end with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy
  ```
- American English. Rust edition 2024 (`if let ... && let ...` chains are used in this codebase). Match surrounding style: short doc comments that say why, `Error::Usage`/`Refusal`/`Io` as in `src/error.rs`, tests in a `#[cfg(test)] mod tests` at the bottom of each file. Use `rg -n PATTERN <path>`, never bare `rg`. Stage only the files named in each commit step.
- Issue #4 (removing dead `tap_new_soaked` staging-tap code in `brew.rs`/`cmd.rs`) lands first and does not touch these files; ignore it.

## Review Focus

Inputs the spec implies but does not spell out. Each has its pinning test in the owning task.

1. `BREWSOAK_SOAK_HOURS` set to something that is not an integer ≥ 1 (`nope`, `0`): `show` must report the file or default as the source (mirroring `resolve_hours`) and print a note that the environment value is ignored, not silently claim the environment as the source. Test: `render_show_notes_an_invalid_env_value` (Task 1).
2. `NO_SOAK` present but not an array of strings (`NO_SOAK = "wget"`, `NO_SOAK = [1]`): `no-soak add|remove` must refuse with `Error::Refusal` naming the key, not clobber or append to it. Test: `no_soak_not_an_array_of_strings_is_refused` (Task 1).
3. `TAP` present as a plain `[TAP]` table or a scalar: `tap-hours` must refuse rather than turn it into an array of tables. Test: `tap_not_an_array_of_tables_is_refused` (Task 1).
4. Two backups in the same second (`Z.bak`, `Z-2.bak`, `Z-3.bak`): pruning must delete `Z.bak` (the oldest), which a plain name sort would keep. Test: `same_second_backups_get_suffixes_and_prune_oldest_first` (Task 2).
5. Duplicate `[[TAP]]` entries for one tap: `tap-hours` must edit the last one, the one `parse_taps` honors ("last entry wins"), or the edit would have no effect. Test: `tap_hours_set_edits_the_last_duplicate` (Task 1).

## File Structure

- `Cargo.toml` / `Cargo.lock` (modify): add `toml_edit = "0.22"` as a direct dependency. No `cargo update`.
- `src/settings.rs` (create): pure edits on `DocumentMut` (`Edit`, `set_soak_hours`, `clear_soak_hours`, `no_soak_add`, `no_soak_remove`, `tap_hours_set`, `tap_hours_clear`, `normalize_tap`, `render_show`). No I/O.
- `src/config.rs` (modify): `read_existing`, `write_atomic` (+ `backup_stamp`, `backup_key`, helpers), `apply_persist(action, path, now)` rebuilt on `settings::*` + `write_atomic`.
- `src/cli.rs` (modify): `Command::Settings(SettingsCmd)`, `SettingsCmd`, `SettingsEdit`, `parse_settings`, `help_text`, `command_help("settings")`.
- `src/lib.rs` (modify): `pub mod settings;`, `run_settings`, `edit_settings`, dispatch branch, `apply_persist` call gains `world.now()`, fake-world tests.
- `README.md` (modify): "Configuration" section.

---

### Task 1: `settings.rs` edit engine and the `toml_edit` dependency

**Files:**
- Modify: `Cargo.toml` (`[dependencies]`), `Cargo.lock` (regenerated by `cargo build`)
- Create: `src/settings.rs`
- Modify: `src/lib.rs:1-21` (add `pub mod settings;` in alphabetical order, after `pub mod resolve;`)
- Test: `src/settings.rs` (`mod tests`)

**Interfaces:**
- Consumes: `crate::nosoak::parse_entry(&str) -> Result<NoSoakEntry, String>`, `crate::config::parse_file(&str) -> ParsedFile` (fields `soak_hours: Option<SoakHours>`, `taps: Vec<TapEntry { name, soak_hours }>`, `notes: Vec<String>`, `warnings: Vec<String>`), `crate::SoakHours` (`new`, `get`, `DEFAULT`), `crate::Error`.
- Produces (Tasks 2, 3, 4 rely on these exact names):
  - `pub struct Edit { pub changed: bool, pub messages: Vec<String> }` (derives `Debug, Clone, Default, PartialEq, Eq`)
  - `pub fn set_soak_hours(doc: &mut DocumentMut, hours: SoakHours) -> Edit`
  - `pub fn clear_soak_hours(doc: &mut DocumentMut) -> Edit`
  - `pub fn no_soak_add(doc: &mut DocumentMut, tokens: &[String]) -> Result<Edit, Error>`
  - `pub fn no_soak_remove(doc: &mut DocumentMut, tokens: &[String]) -> Result<Edit, Error>`
  - `pub fn tap_hours_set(doc: &mut DocumentMut, tap: &str, hours: SoakHours) -> Result<Edit, Error>`
  - `pub fn tap_hours_clear(doc: &mut DocumentMut, tap: &str) -> Result<Edit, Error>`
  - `pub fn normalize_tap(raw: &str) -> Result<String, Error>` (lowercased `user/repo`, else `Error::Usage`)
  - `pub fn render_show(path: &Path, env: Option<&str>, doc: Option<&DocumentMut>) -> String` (`doc == None` means no config file)

- [ ] **Step 1: Create the branch and confirm the baseline gate**

```bash
cd /Users/efitz/Projects/brewsoakr
git checkout -b issue-1-settings-command 2>/dev/null || git checkout issue-1-settings-command
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test
```
Expected: every command exits 0.

- [ ] **Step 2: Add the dependency and the module**

In `Cargo.toml`, under `[dependencies]`, after the `toml = "0.8"` line:

```toml
toml_edit = "0.22"
```

In `src/lib.rs`, after `pub mod resolve;`:

```rust
pub mod settings;
```

Create `src/settings.rs` with only the header and the test module skeleton so the crate still builds:

```rust
//! `brewsoak settings`: edits of the config file as a `toml_edit::DocumentMut`.
//! No I/O here: the caller reads the file, applies one edit, and writes the
//! document back only when `Edit::changed`. Comments, key order, and unknown
//! keys survive because the document is edited in place, never regenerated.

use crate::config;
use crate::nosoak;
use crate::{Error, SoakHours};
use std::path::Path;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value, value};

/// What one edit did. `messages` is one line per token or key, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    pub changed: bool,
    pub messages: Vec<String>,
}

impl Edit {
    fn unchanged(message: impl Into<String>) -> Self {
        Self {
            changed: false,
            messages: vec![message.into()],
        }
    }

    fn changed_with(message: impl Into<String>) -> Self {
        Self {
            changed: true,
            messages: vec![message.into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> DocumentMut {
        s.parse().expect("test fixture is valid TOML")
    }

    const FULL: &str = "# header\nSOAK_HOURS = 48 # trailing\nNO_SOAK = [\"ericfitz/tap\", \"wget\"] # list\nunknown = true\n\n# taps\n[[TAP]]\nname = \"HashiCorp/tap\"\nsoak_hours = 72\n\n[[TAP]]\nname = \"cyclonedx/cyclonedx\"\n";
}
```

Run: `cargo build`
Expected: builds; `Cargo.lock` now lists `toml_edit` under the `brewsoak` package's dependencies (check with `rg -n 'toml_edit' /Users/efitz/Projects/brewsoakr/Cargo.lock`; the `toml_edit` version stays `0.22.27`). Unused-import warnings are fine until Step 5 (clippy runs at the end of the task).

- [ ] **Step 3: Write the failing tests for `SOAK_HOURS`, `NO_SOAK` and the tap-only / empty-file placement**

Append inside `mod tests` in `src/settings.rs`:

```rust
    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| (*i).to_string()).collect()
    }

    #[test]
    fn set_soak_hours_keeps_trailing_comment_and_other_keys() {
        let mut d = doc(FULL);
        let edit = set_soak_hours(&mut d, SoakHours::new(12).unwrap());
        assert_eq!(edit, Edit::changed_with("SOAK_HOURS = 12"));
        assert_eq!(
            d.to_string(),
            FULL.replace("SOAK_HOURS = 48 # trailing", "SOAK_HOURS = 12 # trailing")
        );
    }

    #[test]
    fn set_soak_hours_same_value_is_unchanged() {
        let mut d = doc(FULL);
        let edit = set_soak_hours(&mut d, SoakHours::new(48).unwrap());
        assert!(!edit.changed, "{edit:?}");
        assert_eq!(edit.messages, ["SOAK_HOURS is already 48"]);
        assert_eq!(d.to_string(), FULL);
    }

    #[test]
    fn set_soak_hours_24_removes_the_key_like_clear() {
        let mut d = doc(FULL);
        let edit = set_soak_hours(&mut d, SoakHours::DEFAULT);
        assert!(edit.changed);
        let text = d.to_string();
        assert!(!text.contains("SOAK_HOURS"), "{text}");
        assert!(text.starts_with("# header\n"), "{text}");
        assert!(text.contains("unknown = true"), "{text}");
        let again = clear_soak_hours(&mut d);
        assert!(!again.changed);
        assert!(again.messages[0].contains("not set"), "{again:?}");
    }

    #[test]
    fn set_soak_hours_overwrites_a_wrong_typed_value() {
        let mut d = doc("SOAK_HOURS = \"x\" # c\n");
        set_soak_hours(&mut d, SoakHours::new(9).unwrap());
        assert_eq!(d.to_string(), "SOAK_HOURS = 9 # c\n");
    }

    #[test]
    fn empty_file_gets_exactly_the_key() {
        let mut d = doc("");
        set_soak_hours(&mut d, SoakHours::new(48).unwrap());
        assert_eq!(d.to_string(), "SOAK_HOURS = 48\n");
        let mut d = doc("");
        no_soak_add(&mut d, &s(&["wget"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = [\"wget\"]\n");
    }

    #[test]
    fn top_level_keys_land_above_the_first_tap_in_a_tap_only_file() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nsoak_hours = 72\n");
        set_soak_hours(&mut d, SoakHours::new(48).unwrap());
        no_soak_add(&mut d, &s(&["wget"])).unwrap();
        assert_eq!(
            d.to_string(),
            "SOAK_HOURS = 48\nNO_SOAK = [\"wget\"]\n\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 72\n"
        );
        let parsed = config::parse_file(&d.to_string());
        assert_eq!(parsed.soak_hours.map(|h| h.get()), Some(48));
        assert!(parsed.no_soak.matches("homebrew/core", "wget"));
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    }

    #[test]
    fn tap_only_file_with_a_leading_comment_keeps_it_below_the_new_keys() {
        let mut d = doc("# taps\n[[TAP]]\nname = \"a/b\"\n");
        set_soak_hours(&mut d, SoakHours::new(48).unwrap());
        assert_eq!(d.to_string(), "SOAK_HOURS = 48\n\n# taps\n[[TAP]]\nname = \"a/b\"\n");
    }

    #[test]
    fn no_soak_add_appends_lowercased_and_skips_present_in_any_case() {
        let mut d = doc(FULL);
        let edit = no_soak_add(&mut d, &s(&["WGET", "Curl", "curl"])).unwrap();
        assert!(edit.changed);
        assert_eq!(
            edit.messages,
            [
                "already in NO_SOAK: wget",
                "added to NO_SOAK: curl",
                "already in NO_SOAK: curl"
            ]
        );
        assert_eq!(
            d.to_string(),
            FULL.replace(
                "NO_SOAK = [\"ericfitz/tap\", \"wget\"] # list",
                "NO_SOAK = [\"ericfitz/tap\", \"wget\", \"curl\"] # list"
            )
        );
    }

    #[test]
    fn no_soak_add_all_present_is_unchanged() {
        let mut d = doc(FULL);
        let edit = no_soak_add(&mut d, &s(&["wget"])).unwrap();
        assert!(!edit.changed);
        assert_eq!(d.to_string(), FULL);
    }

    #[test]
    fn no_soak_add_keeps_multiline_indentation() {
        let mut d = doc("NO_SOAK = [\n  \"wget\",\n  \"curl\",\n]\n");
        no_soak_add(&mut d, &s(&["x"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = [\n  \"wget\",\n  \"curl\",\n  \"x\",\n]\n");
        let mut d = doc("NO_SOAK = [\"wget\"]\n");
        no_soak_add(&mut d, &s(&["x"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = [\"wget\", \"x\"]\n");
        let mut d = doc("NO_SOAK = []\n");
        no_soak_add(&mut d, &s(&["x"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = [\"x\"]\n");
    }

    #[test]
    fn no_soak_invalid_tokens_change_nothing_and_name_every_bad_token() {
        for op in [no_soak_add, no_soak_remove] {
            let mut d = doc(FULL);
            let err = op(&mut d, &s(&["curl", "a/b/c/d", "", "x//y"])).unwrap_err();
            match err {
                Error::Usage(m) => {
                    assert!(m.contains("\"a/b/c/d\""), "{m}");
                    assert!(m.contains("empty"), "{m}");
                    assert!(m.contains("\"x//y\""), "{m}");
                    assert!(!m.contains("skipped"), "{m}");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(d.to_string(), FULL);
        }
    }

    #[test]
    fn no_soak_remove_matches_case_insensitively_and_reports_absent() {
        let mut d = doc(FULL);
        let edit = no_soak_remove(&mut d, &s(&["WGET", "nope"])).unwrap();
        assert!(edit.changed);
        assert_eq!(
            edit.messages,
            ["removed from NO_SOAK: wget", "not in NO_SOAK: nope"]
        );
        assert_eq!(
            d.to_string(),
            FULL.replace(
                "NO_SOAK = [\"ericfitz/tap\", \"wget\"] # list",
                "NO_SOAK = [\"ericfitz/tap\"] # list"
            )
        );
    }

    #[test]
    fn no_soak_remove_without_key_or_last_entry() {
        let mut d = doc("");
        let edit = no_soak_remove(&mut d, &s(&["wget"])).unwrap();
        assert_eq!(edit, Edit::unchanged("not in NO_SOAK: wget"));
        assert_eq!(d.to_string(), "");
        let mut d = doc("NO_SOAK = [\"wget\"]\n");
        no_soak_remove(&mut d, &s(&["wget"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = []\n");
    }

    #[test]
    fn no_soak_not_an_array_of_strings_is_refused() {
        for contents in ["NO_SOAK = \"wget\"\n", "NO_SOAK = [1]\n", "NO_SOAK = [\"a\", 2]\n"] {
            for op in [no_soak_add, no_soak_remove] {
                let mut d = doc(contents);
                match op(&mut d, &s(&["curl"])) {
                    Err(Error::Refusal(m)) => assert!(m.contains("NO_SOAK"), "{m}"),
                    other => panic!("{contents:?}: {other:?}"),
                }
                assert_eq!(d.to_string(), contents);
            }
        }
    }

    #[test]
    fn misplaced_no_soak_inside_tap_is_left_alone() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"wget\"]\n");
        no_soak_add(&mut d, &s(&["curl"])).unwrap();
        assert_eq!(
            d.to_string(),
            "NO_SOAK = [\"curl\"]\n\n[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"wget\"]\n"
        );
        assert_eq!(config::parse_file(&d.to_string()).warnings.len(), 1);
    }
```

Run: `cargo test --lib settings::`
Expected: compile error (`set_soak_hours` and friends not found).

- [ ] **Step 4: Implement `SOAK_HOURS` and `NO_SOAK` edits**

Insert between `impl Edit` and `#[cfg(test)]` in `src/settings.rs`:

```rust
/// `SOAK_HOURS = N`. The default (24) removes the key instead, the same rule
/// `--soak-hours` follows.
pub fn set_soak_hours(doc: &mut DocumentMut, hours: SoakHours) -> Edit {
    if hours == SoakHours::DEFAULT {
        return clear_soak_hours(doc);
    }
    let n = i64::from(hours.get());
    if doc.get("SOAK_HOURS").and_then(Item::as_integer) == Some(n) {
        return Edit::unchanged(format!("SOAK_HOURS is already {n}"));
    }
    let had_values = root_has_values(doc);
    set_integer(doc.as_table_mut(), "SOAK_HOURS", n);
    separate_taps(doc, had_values);
    Edit::changed_with(format!("SOAK_HOURS = {n}"))
}

pub fn clear_soak_hours(doc: &mut DocumentMut) -> Edit {
    if doc.as_table_mut().remove("SOAK_HOURS").is_none() {
        return Edit::unchanged("SOAK_HOURS is not set; the default is 24");
    }
    Edit::changed_with("SOAK_HOURS removed; the default is 24")
}

/// Replace the value in place so a trailing comment on the line survives.
/// `table[key] = value(n)` would drop it.
fn set_integer(table: &mut Table, key: &str, n: i64) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            let mut new = Value::from(n);
            *new.decor_mut() = decor;
            *existing = new;
        }
        None => table[key] = value(n),
    }
}

fn root_has_values(doc: &DocumentMut) -> bool {
    doc.iter().any(|(_, item)| item.is_value())
}

/// toml_edit renders root keys above every `[[TAP]]` but adds no blank line
/// when the file held only tables. Add one, once, so the result reads like
/// the README example.
fn separate_taps(doc: &mut DocumentMut, had_values: bool) {
    if had_values {
        return;
    }
    let Some(first) = doc
        .get_mut("TAP")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|tables| tables.get_mut(0))
    else {
        return;
    };
    let prefix = first
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    if !prefix.starts_with('\n') {
        first.decor_mut().set_prefix(format!("\n{prefix}"));
    }
}

/// Every token through `nosoak::parse_entry`, the config reader's parser.
/// All bad tokens go in one usage error; the result is lowercased.
fn validate_tokens(tokens: &[String]) -> Result<Vec<String>, Error> {
    let bad: Vec<String> = tokens
        .iter()
        .filter_map(|t| nosoak::parse_entry(t).err())
        .map(|reason| reason.trim_end_matches("; skipped").to_string())
        .collect();
    if !bad.is_empty() {
        return Err(Error::Usage(format!(
            "invalid NO_SOAK token(s):\n  {}",
            bad.join("\n  ")
        )));
    }
    Ok(tokens
        .iter()
        .map(|t| t.trim().to_ascii_lowercase())
        .collect())
}

/// `Ok(None)`: no top-level key. `Err`: the key exists but is not an array
/// of strings, which the reader ignores and this editor will not clobber.
fn no_soak_array(doc: &mut DocumentMut) -> Result<Option<&mut Array>, Error> {
    let Some(item) = doc.get_mut("NO_SOAK") else {
        return Ok(None);
    };
    match item.as_array_mut() {
        Some(arr) if arr.iter().all(|v| v.as_str().is_some()) => Ok(Some(arr)),
        _ => Err(Error::Refusal(
            "config: NO_SOAK is not an array of strings; fix it by hand".into(),
        )),
    }
}

fn no_soak_contains(arr: &Array, token: &str) -> bool {
    arr.iter()
        .any(|v| v.as_str().is_some_and(|s| s.trim().eq_ignore_ascii_case(token)))
}

/// Append with the indentation of the last element when the array is written
/// one entry per line; a plain push would put the new entry on that line.
fn push_entry(arr: &mut Array, token: &str) {
    let last_prefix = arr
        .iter()
        .last()
        .and_then(|v| v.decor().prefix())
        .and_then(|p| p.as_str())
        .map(str::to_string);
    match last_prefix {
        Some(prefix) if prefix.contains('\n') => {
            let indent = prefix.rsplit('\n').next().unwrap_or("");
            let mut new = Value::from(token);
            new.decor_mut().set_prefix(format!("\n{indent}"));
            arr.push_formatted(new);
        }
        _ => arr.push(token),
    }
}

/// Append each token not already present (case-insensitive), lowercased.
pub fn no_soak_add(doc: &mut DocumentMut, tokens: &[String]) -> Result<Edit, Error> {
    let tokens = validate_tokens(tokens)?;
    let had_values = root_has_values(doc);
    if no_soak_array(doc)?.is_none() {
        doc["NO_SOAK"] = value(Array::new());
    }
    let arr = no_soak_array(doc)?.expect("NO_SOAK was just created");
    let mut edit = Edit::default();
    for token in tokens {
        if no_soak_contains(arr, &token) {
            edit.messages.push(format!("already in NO_SOAK: {token}"));
            continue;
        }
        push_entry(arr, &token);
        edit.changed = true;
        edit.messages.push(format!("added to NO_SOAK: {token}"));
    }
    separate_taps(doc, had_values);
    Ok(edit)
}

/// Remove each matching entry (case-insensitive). An absent token is
/// reported, not an error. An emptied list stays as `NO_SOAK = []`.
pub fn no_soak_remove(doc: &mut DocumentMut, tokens: &[String]) -> Result<Edit, Error> {
    let tokens = validate_tokens(tokens)?;
    let mut edit = Edit::default();
    let Some(arr) = no_soak_array(doc)? else {
        edit.messages = tokens
            .iter()
            .map(|t| format!("not in NO_SOAK: {t}"))
            .collect();
        return Ok(edit);
    };
    for token in tokens {
        let before = arr.len();
        arr.retain(|v| !v.as_str().is_some_and(|s| s.trim().eq_ignore_ascii_case(&token)));
        if arr.len() == before {
            edit.messages.push(format!("not in NO_SOAK: {token}"));
        } else {
            edit.changed = true;
            edit.messages.push(format!("removed from NO_SOAK: {token}"));
        }
    }
    Ok(edit)
}
```

Note on `no_soak_add` when the key was absent: the array is created empty, so at least one token is appended (a token can only be "already in" after an earlier token in the same call added it); the key is never left as a new empty `NO_SOAK = []`.

Run: `cargo test --lib settings::`
Expected: all tests from Step 3 pass; the tap and show tests do not exist yet.

- [ ] **Step 5: Write the failing tests for `tap-hours` and `render_show`**

Append inside `mod tests`:

```rust
    #[test]
    fn normalize_tap_accepts_two_segments_and_lowercases() {
        assert_eq!(normalize_tap(" HashiCorp/Tap ").unwrap(), "hashicorp/tap");
        assert_eq!(normalize_tap("homebrew/core").unwrap(), "homebrew/core");
        for bad in ["bad", "a/b/c", "/b", "a/", "", "a//b"] {
            match normalize_tap(bad) {
                Err(Error::Usage(m)) => assert!(m.contains("user/repo"), "{bad:?}: {m}"),
                other => panic!("{bad:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn tap_hours_set_updates_the_matching_entry_case_insensitively() {
        let mut d = doc(FULL);
        let edit = tap_hours_set(&mut d, "hashicorp/TAP", SoakHours::new(10).unwrap()).unwrap();
        assert_eq!(edit, Edit::changed_with("hashicorp/tap soak_hours = 10"));
        assert_eq!(
            d.to_string(),
            FULL.replace("soak_hours = 72", "soak_hours = 10"),
            "name keeps the user's casing"
        );
        let again = tap_hours_set(&mut d, "hashicorp/tap", SoakHours::new(10).unwrap()).unwrap();
        assert!(!again.changed, "{again:?}");
        assert!(again.messages[0].contains("already 10"), "{again:?}");
    }

    #[test]
    fn tap_hours_set_adds_soak_hours_to_an_entry_without_one() {
        let mut d = doc(FULL);
        tap_hours_set(&mut d, "cyclonedx/cyclonedx", SoakHours::new(5).unwrap()).unwrap();
        assert!(
            d.to_string()
                .ends_with("[[TAP]]\nname = \"cyclonedx/cyclonedx\"\nsoak_hours = 5\n"),
            "{d}"
        );
    }

    #[test]
    fn tap_hours_set_edits_the_last_duplicate() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nsoak_hours = 1\n\n[[TAP]]\nname = \"a/b\"\n");
        tap_hours_set(&mut d, "a/b", SoakHours::new(5).unwrap()).unwrap();
        assert_eq!(
            d.to_string(),
            "[[TAP]]\nname = \"a/b\"\nsoak_hours = 1\n\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n"
        );
        assert_eq!(
            config::parse_file(&d.to_string()).taps[0].soak_hours.map(|h| h.get()),
            Some(5),
            "the reader honors the last duplicate, so that is the one edited"
        );
    }

    #[test]
    fn tap_hours_set_appends_a_new_entry_after_the_last() {
        let mut d = doc(FULL);
        let edit = tap_hours_set(&mut d, "New/Tap", SoakHours::new(10).unwrap()).unwrap();
        assert_eq!(edit, Edit::changed_with("new/tap added with soak_hours = 10"));
        assert_eq!(
            d.to_string(),
            format!("{FULL}\n[[TAP]]\nname = \"new/tap\"\nsoak_hours = 10\n")
        );
        let mut d = doc("");
        tap_hours_set(&mut d, "new/tap", SoakHours::new(10).unwrap()).unwrap();
        assert_eq!(d.to_string(), "[[TAP]]\nname = \"new/tap\"\nsoak_hours = 10\n");
        let mut d = doc("SOAK_HOURS = 48\n");
        tap_hours_set(&mut d, "new/tap", SoakHours::new(10).unwrap()).unwrap();
        assert_eq!(
            d.to_string(),
            "SOAK_HOURS = 48\n\n[[TAP]]\nname = \"new/tap\"\nsoak_hours = 10\n"
        );
    }

    #[test]
    fn tap_entry_without_a_string_name_never_matches() {
        let mut d = doc("[[TAP]]\nname = 5\n\n[[TAP]]\nsoak_hours = 3\n");
        tap_hours_set(&mut d, "a/b", SoakHours::new(5).unwrap()).unwrap();
        assert!(d.to_string().ends_with("[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n"), "{d}");
        assert_eq!(d["TAP"].as_array_of_tables().unwrap().len(), 3);
    }

    #[test]
    fn tap_hours_set_rejects_bad_names_without_touching_the_doc() {
        let mut d = doc(FULL);
        for bad in ["bad", "a/b/c", "/b"] {
            assert!(matches!(
                tap_hours_set(&mut d, bad, SoakHours::new(5).unwrap()),
                Err(Error::Usage(_))
            ));
            assert!(matches!(tap_hours_clear(&mut d, bad), Err(Error::Usage(_))));
        }
        assert_eq!(d.to_string(), FULL);
    }

    #[test]
    fn tap_hours_clear_removes_only_soak_hours_when_other_keys_remain() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\nextra = 1\n");
        let edit = tap_hours_clear(&mut d, "A/B").unwrap();
        assert_eq!(edit, Edit::changed_with("a/b soak_hours removed; it uses SOAK_HOURS"));
        assert_eq!(d.to_string(), "[[TAP]]\nname = \"a/b\"\nextra = 1\n");
    }

    #[test]
    fn tap_hours_clear_removes_the_whole_entry_when_only_name_is_left() {
        let mut d = doc(FULL);
        let edit = tap_hours_clear(&mut d, "hashicorp/tap").unwrap();
        assert!(edit.changed);
        assert_eq!(edit.messages.len(), 2, "{edit:?}");
        assert!(edit.messages[1].contains("entry removed"), "{edit:?}");
        let text = d.to_string();
        assert!(!text.contains("HashiCorp"), "{text}");
        assert!(text.contains("[[TAP]]\nname = \"cyclonedx/cyclonedx\"\n"), "{text}");
        assert!(text.starts_with("# header\n"), "{text}");
        assert!(text.contains("unknown = true"), "{text}");
    }

    #[test]
    fn tap_hours_clear_last_entry_leaves_an_empty_document() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n");
        tap_hours_clear(&mut d, "a/b").unwrap();
        assert!(d.get("TAP").is_none(), "{d}");
        assert!(d.to_string().trim().is_empty(), "{d:?}");
    }

    #[test]
    fn tap_hours_clear_absent_tap_or_absent_hours_is_unchanged() {
        let mut d = doc(FULL);
        let edit = tap_hours_clear(&mut d, "nope/tap").unwrap();
        assert!(!edit.changed);
        assert!(edit.messages[0].contains("no [[TAP]] entry"), "{edit:?}");
        let edit = tap_hours_clear(&mut d, "cyclonedx/cyclonedx").unwrap();
        assert!(!edit.changed);
        assert!(edit.messages[0].contains("no soak_hours"), "{edit:?}");
        assert_eq!(d.to_string(), FULL);
        let mut d = doc("");
        assert!(!tap_hours_clear(&mut d, "a/b").unwrap().changed);
    }

    #[test]
    fn tap_not_an_array_of_tables_is_refused() {
        for contents in ["TAP = 5\n", "[TAP]\nname = \"a/b\"\n"] {
            let mut d = doc(contents);
            match tap_hours_set(&mut d, "a/b", SoakHours::new(5).unwrap()) {
                Err(Error::Refusal(m)) => assert!(m.contains("TAP"), "{m}"),
                other => panic!("{contents:?}: {other:?}"),
            }
            assert!(matches!(tap_hours_clear(&mut d, "a/b"), Err(Error::Refusal(_))));
            assert_eq!(d.to_string(), contents);
        }
    }

    fn show(contents: Option<&str>, env: Option<&str>) -> String {
        let d = contents.map(doc);
        render_show(Path::new("/home/x/.config/brewsoak/config.toml"), env, d.as_ref())
    }

    #[test]
    fn render_show_lists_everything_from_the_file() {
        let text = show(Some(FULL), None);
        assert_eq!(
            text,
            "config: /home/x/.config/brewsoak/config.toml\n\
             soak hours: 48 (SOAK_HOURS in the file)\n\
             NO_SOAK:\n  ericfitz/tap\n  wget\n\
             taps:\n  hashicorp/tap: 72 (own)\n  cyclonedx/cyclonedx: 48 (SOAK_HOURS)\n"
        );
    }

    #[test]
    fn render_show_env_overrides_file_and_tap_defaults() {
        let text = show(Some(FULL), Some("36"));
        assert!(text.contains("soak hours: 36 (BREWSOAK_SOAK_HOURS)\n"), "{text}");
        assert!(text.contains("  cyclonedx/cyclonedx: 36 (SOAK_HOURS)\n"), "{text}");
        assert!(text.contains("  hashicorp/tap: 72 (own)\n"), "{text}");
    }

    #[test]
    fn render_show_without_a_file() {
        let text = show(None, None);
        assert_eq!(
            text,
            "config: /home/x/.config/brewsoak/config.toml (no config file)\n\
             soak hours: 24 (default)\nNO_SOAK: (none)\ntaps: (none)\n"
        );
    }

    #[test]
    fn render_show_notes_an_invalid_env_value() {
        for bad in ["nope", "0"] {
            let text = show(Some("SOAK_HOURS = 8\n"), Some(bad));
            assert!(text.contains("soak hours: 8 (SOAK_HOURS in the file)\n"), "{text}");
            assert!(
                text.contains(&format!("note: BREWSOAK_SOAK_HOURS={bad:?} is not an integer >= 1; ignored\n")),
                "{text}"
            );
        }
    }

    #[test]
    fn render_show_prints_entries_as_written_plus_notes_and_warnings() {
        let text = show(
            Some("NO_SOAK = [\"WGet\", \"a/b/c/d\"]\n\n[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"x\"]\n\n[[TAP]]\nname = \"bad\"\n"),
            None,
        );
        assert!(text.contains("NO_SOAK:\n  WGet\n  a/b/c/d\n"), "as written: {text}");
        assert!(text.contains("note: config: NO_SOAK entry \"a/b/c/d\""), "{text}");
        assert!(text.contains("note: config: [[TAP]] name \"bad\""), "{text}");
        assert!(text.contains("warning: config: NO_SOAK inside [[TAP]] a/b"), "{text}");
        assert!(text.contains("  a/b: 24 (SOAK_HOURS)\n"), "{text}");
    }
```

Run: `cargo test --lib settings::`
Expected: compile error (`normalize_tap`, `tap_hours_set`, `tap_hours_clear`, `render_show` not found).

- [ ] **Step 6: Implement the tap edits and `render_show`**

Append before `#[cfg(test)]` in `src/settings.rs`:

```rust
/// Same rule as the `[[TAP]]` reader: exactly two non-empty segments.
pub fn normalize_tap(raw: &str) -> Result<String, Error> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.split('/').count() != 2 || lower.split('/').any(str::is_empty) {
        return Err(Error::Usage(format!("tap must be user/repo, got {raw:?}")));
    }
    Ok(lower)
}

/// `Ok(None)`: no `TAP` key. `Err`: it exists but is not `[[TAP]]` tables.
fn tap_tables(doc: &mut DocumentMut) -> Result<Option<&mut ArrayOfTables>, Error> {
    let Some(item) = doc.get_mut("TAP") else {
        return Ok(None);
    };
    item.as_array_of_tables_mut().map(Some).ok_or_else(|| {
        Error::Refusal("config: TAP is not an array of tables; fix it by hand".into())
    })
}

/// Index of the last `[[TAP]]` whose `name` matches: the reader lets the
/// last duplicate win, so that is the entry whose hours take effect.
/// Entries without a string `name` never match.
fn tap_index(tables: &ArrayOfTables, tap: &str) -> Option<usize> {
    tables
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.get("name")
                .and_then(Item::as_str)
                .is_some_and(|n| n.trim().eq_ignore_ascii_case(tap))
        })
        .last()
        .map(|(i, _)| i)
}

/// Set that tap's `soak_hours`, appending a `[[TAP]]` after the last one
/// when the tap has none.
pub fn tap_hours_set(doc: &mut DocumentMut, tap: &str, hours: SoakHours) -> Result<Edit, Error> {
    let tap = normalize_tap(tap)?;
    let n = i64::from(hours.get());
    if let Some(tables) = tap_tables(doc)?
        && let Some(i) = tap_index(tables, &tap)
    {
        let table = tables.get_mut(i).expect("index from iter");
        if table.get("soak_hours").and_then(Item::as_integer) == Some(n) {
            return Ok(Edit::unchanged(format!("{tap} soak_hours is already {n}")));
        }
        set_integer(table, "soak_hours", n);
        return Ok(Edit::changed_with(format!("{tap} soak_hours = {n}")));
    }
    let mut entry = Table::new();
    entry["name"] = value(tap.as_str());
    entry["soak_hours"] = value(n);
    match tap_tables(doc)? {
        Some(tables) => tables.push(entry),
        None => {
            let mut tables = ArrayOfTables::new();
            tables.push(entry);
            doc["TAP"] = Item::ArrayOfTables(tables);
        }
    }
    Ok(Edit::changed_with(format!("{tap} added with soak_hours = {n}")))
}

/// Remove that tap's `soak_hours`; an entry left with only `name` goes too,
/// and the `TAP` key goes when no entry remains.
pub fn tap_hours_clear(doc: &mut DocumentMut, tap: &str) -> Result<Edit, Error> {
    let tap = normalize_tap(tap)?;
    let Some(tables) = tap_tables(doc)? else {
        return Ok(Edit::unchanged(format!("{tap} has no [[TAP]] entry")));
    };
    let Some(i) = tap_index(tables, &tap) else {
        return Ok(Edit::unchanged(format!("{tap} has no [[TAP]] entry")));
    };
    let table = tables.get_mut(i).expect("index from iter");
    if table.remove("soak_hours").is_none() {
        return Ok(Edit::unchanged(format!(
            "{tap} has no soak_hours; it uses SOAK_HOURS"
        )));
    }
    let mut edit = Edit::changed_with(format!("{tap} soak_hours removed; it uses SOAK_HOURS"));
    if table.len() == 1 && table.contains_key("name") {
        tables.remove(i);
        edit.messages
            .push(format!("{tap} [[TAP]] entry removed; only name was left"));
        if tables.is_empty() {
            doc.as_table_mut().remove("TAP");
        }
    }
    Ok(edit)
}

/// The effective config, for `settings show`. `doc` is `None` when there is
/// no config file. Every parse note and warning is included, not only
/// under `-v`.
pub fn render_show(path: &Path, env: Option<&str>, doc: Option<&DocumentMut>) -> String {
    let contents = doc.map(ToString::to_string);
    let parsed = contents.as_deref().map(config::parse_file).unwrap_or_default();
    let mut out = String::new();
    match contents {
        Some(_) => out.push_str(&format!("config: {}\n", path.display())),
        None => out.push_str(&format!("config: {} (no config file)\n", path.display())),
    }
    // Same precedence as `config::resolve_hours`: env > file > 24, where an
    // env value that is not an integer >= 1 is ignored.
    let env_hours = env
        .and_then(|raw| raw.parse::<u32>().ok())
        .and_then(SoakHours::new);
    let (hours, source) = match (env_hours, parsed.soak_hours) {
        (Some(h), _) => (h, "BREWSOAK_SOAK_HOURS"),
        (None, Some(h)) => (h, "SOAK_HOURS in the file"),
        (None, None) => (SoakHours::DEFAULT, "default"),
    };
    out.push_str(&format!("soak hours: {} ({source})\n", hours.get()));
    let as_written: Vec<&str> = doc
        .and_then(|d| d.get("NO_SOAK"))
        .and_then(Item::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if as_written.is_empty() {
        out.push_str("NO_SOAK: (none)\n");
    } else {
        out.push_str("NO_SOAK:\n");
        for entry in as_written {
            out.push_str(&format!("  {entry}\n"));
        }
    }
    if parsed.taps.is_empty() {
        out.push_str("taps: (none)\n");
    } else {
        out.push_str("taps:\n");
        for tap in &parsed.taps {
            match tap.soak_hours {
                Some(h) => out.push_str(&format!("  {}: {} (own)\n", tap.name, h.get())),
                None => out.push_str(&format!("  {}: {} (SOAK_HOURS)\n", tap.name, hours.get())),
            }
        }
    }
    for note in &parsed.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    if let Some(raw) = env
        && env_hours.is_none()
    {
        out.push_str(&format!(
            "note: BREWSOAK_SOAK_HOURS={raw:?} is not an integer >= 1; ignored\n"
        ));
    }
    for warning in &parsed.warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    out
}
```

Run: `cargo test --lib settings::`
Expected: all `settings::` tests pass. If `render_show_prints_entries_as_written_plus_notes_and_warnings` fails on the note wording, copy the exact note text from `config::parse_taps` / `parse_no_soak` (`src/config.rs:149-229`) into the assertion; the notes are theirs, not this module's.

- [ ] **Step 7: Done gate and commit**

```bash
cd /Users/efitz/Projects/brewsoakr
cargo fmt && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test
git add Cargo.toml Cargo.lock src/settings.rs src/lib.rs
git commit -m "feat(settings): format-preserving config edits on toml_edit

Pure edits of SOAK_HOURS, NO_SOAK and [[TAP]] soak_hours on a DocumentMut,
plus the show renderer. Top-level keys land above the first [[TAP]];
comments, order and unknown keys survive.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```
Expected: gate exits 0 at every stage; commit succeeds.

---

### Task 2: `config::write_atomic` with an injected clock; `apply_persist` rebuilt on it

**Files:**
- Modify: `src/config.rs` (`use` block at top; replace `apply_persist` at lines 235-283; add `read_existing`, `write_atomic` and helpers; update the three `apply_persist` tests at lines 346-357, 498-535 and add new ones)
- Modify: `src/lib.rs:143` (`apply_persist` call gains `world.now()`)
- Test: `src/config.rs` (`mod tests`)

**Interfaces:**
- Consumes: `settings::set_soak_hours`, `settings::clear_soak_hours`, `settings::Edit` (Task 1); `time::OffsetDateTime`.
- Produces (Task 4 relies on these):
  - `pub fn read_existing(path: &Path) -> Result<Option<String>, Error>` (`None` when the file does not exist; other I/O errors are returned)
  - `pub fn write_atomic(path: &Path, new_body: &str, now: time::OffsetDateTime) -> Result<(), Error>`
  - `pub fn apply_persist(action: PersistAction, path: &Path, now: time::OffsetDateTime) -> Result<Option<String>, Error>` (same return meaning as before: `Some(warning)` for invalid TOML)
  - `pub(crate) fn backup_stamp(now: OffsetDateTime) -> String` and `pub(crate) fn backup_key(name: &str, file: &str) -> Option<(String, u32)>` (tested directly)

- [ ] **Step 1: Write the failing `write_atomic` tests**

Append inside `mod tests` in `src/config.rs`:

```rust
    // `OffsetDateTime`, `Path` and `PermissionsExt` come in through
    // `use super::*` (Step 2 adds them to the module's imports).
    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("fixed now")
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn backup_stamp_is_utc_compact_iso() {
        assert_eq!(backup_stamp(at(0)), "20231114T221320Z");
    }

    #[test]
    fn backup_key_orders_same_second_suffixes_after_the_bare_name() {
        let k = |f| backup_key("config.toml", f);
        assert_eq!(
            k("config.toml.20231114T221320Z.bak"),
            Some(("20231114T221320Z".into(), 1))
        );
        assert_eq!(
            k("config.toml.20231114T221320Z-2.bak"),
            Some(("20231114T221320Z".into(), 2))
        );
        assert!(k("config.toml.20231114T221320Z-2.bak") > k("config.toml.20231114T221320Z.bak"));
        assert!(k("config.toml.20231114T221320Z-10.bak") > k("config.toml.20231114T221320Z-2.bak"));
        assert!(k("config.toml.20231114T221321Z.bak") > k("config.toml.20231114T221320Z-10.bak"));
        assert_eq!(k("config.toml"), None);
        assert_eq!(k(".config.toml.tmp-1"), None);
        assert_eq!(k("config.toml.20231114T221320Z-x.bak"), None);
        assert_eq!(k("other.toml.20231114T221320Z.bak"), None);
    }

    #[test]
    fn unchanged_body_makes_no_write_and_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_atomic(&path, "a\n", at(0)).unwrap();
        assert_eq!(names(dir.path()), ["config.toml"], "first write of a new file has nothing to back up");
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        write_atomic(&path, "a\n", at(1)).unwrap();
        assert_eq!(names(dir.path()), ["config.toml"]);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        write_atomic(&path, "b\n", at(2)).unwrap();
        assert_eq!(
            names(dir.path()),
            ["config.toml", "config.toml.20231114T221322Z.bak"]
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("config.toml.20231114T221322Z.bak")).unwrap(), "a\n");
        write_atomic(&path, "b\n", at(3)).unwrap();
        assert_eq!(names(dir.path()).len(), 2, "second identical edit: no backup");
    }

    #[test]
    fn backups_keep_the_two_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (i, body) in ["a\n", "b\n", "c\n", "d\n"].iter().enumerate() {
            write_atomic(&path, body, at(i as i64)).unwrap();
        }
        assert_eq!(
            names(dir.path()),
            [
                "config.toml",
                "config.toml.20231114T221322Z.bak",
                "config.toml.20231114T221323Z.bak"
            ]
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("config.toml.20231114T221322Z.bak")).unwrap(), "b\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("config.toml.20231114T221323Z.bak")).unwrap(), "c\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "d\n");
    }

    #[test]
    fn same_second_backups_get_suffixes_and_prune_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for body in ["a\n", "b\n", "c\n", "d\n"] {
            write_atomic(&path, body, at(0)).unwrap();
        }
        assert_eq!(
            names(dir.path()),
            [
                "config.toml",
                "config.toml.20231114T221320Z-2.bak",
                "config.toml.20231114T221320Z-3.bak"
            ],
            "Z.bak (the oldest) is the one pruned, not Z-2 which sorts first as a string"
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("config.toml.20231114T221320Z-3.bak")).unwrap(), "c\n");
    }

    #[test]
    fn empty_body_removes_the_file_and_keeps_a_backup() {
        for empty in ["", "  \n"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            write_atomic(&path, "a\n", at(0)).unwrap();
            write_atomic(&path, empty, at(1)).unwrap();
            assert!(!path.exists(), "{empty:?}");
            assert_eq!(names(dir.path()), ["config.toml.20231114T221321Z.bak"], "{empty:?}");
            write_atomic(&path, "", at(2)).unwrap();
            assert_eq!(names(dir.path()).len(), 1, "empty onto missing is a no-op");
        }
    }

    #[test]
    fn comment_only_body_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_atomic(&path, "# keep\n", at(0)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# keep\n");
    }

    #[test]
    fn missing_directory_is_created_and_mode_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x/y/config.toml");
        write_atomic(&path, "a\n", at(0)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn stale_temp_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(dir.path().join(".config.toml.tmp-1"), "junk").unwrap();
        std::fs::write(dir.path().join(format!(".config.toml.tmp-{}", std::process::id())), "junk").unwrap();
        write_atomic(&path, "a\n", at(0)).unwrap();
        assert_eq!(names(dir.path()), ["config.toml"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
    }

    #[test]
    fn read_existing_distinguishes_missing_from_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(read_existing(&path).unwrap(), None);
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_existing(&path).unwrap(), Some(String::new()));
    }
```

Run: `cargo test --lib config::`
Expected: compile error (`write_atomic`, `backup_stamp`, `backup_key`, `read_existing` not found).

- [ ] **Step 2: Implement `read_existing`, `write_atomic` and helpers**

In `src/config.rs`, change the `use` block at the top to:

```rust
use crate::nosoak::{self, NoSoakList};
use crate::settings;
use crate::{Error, SoakHours};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use toml_edit::DocumentMut;
```

Insert after `pub fn read_file` (line 233):

```rust
/// `None` when the file does not exist; any other I/O error is returned.
pub fn read_existing(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Backups kept after every write; older `.bak` files are deleted.
const BACKUPS_KEPT: usize = 2;

/// `20231114T221320Z`: UTC, sortable, safe in a file name.
pub(crate) fn backup_stamp(now: OffsetDateTime) -> String {
    let format = time::format_description::parse_borrowed::<2>(
        "[year][month][day]T[hour][minute][second]Z",
    )
    .expect("static format description");
    now.to_offset(time::UtcOffset::UTC)
        .format(&format)
        .expect("a UTC datetime formats")
}

/// `config.toml.20231114T221320Z-2.bak` -> `("20231114T221320Z", 2)`; the
/// bare name is suffix 1. Anything else is not a backup of `name`.
pub(crate) fn backup_key(name: &str, file: &str) -> Option<(String, u32)> {
    let middle = file
        .strip_prefix(name)?
        .strip_prefix('.')?
        .strip_suffix(".bak")?;
    match middle.split_once('-') {
        None => Some((middle.to_string(), 1)),
        Some((stamp, n)) => Some((stamp.to_string(), n.parse().ok()?)),
    }
}

fn backup_name(name: &str, stamp: &str, n: u32) -> String {
    if n == 1 {
        format!("{name}.{stamp}.bak")
    } else {
        format!("{name}.{stamp}-{n}.bak")
    }
}

/// Spec "config::write_atomic". Readers see the old file or the new one,
/// never none: the body goes to a temp file, the current file is hard-linked
/// to a `.bak`, and the temp file is renamed over it.
///
/// There is no lock. Two overlapping runs each rename atomically and the
/// last one wins; the stale-temp sweep at the end can delete the other run's
/// in-flight temp file, in which case that run fails with an I/O error and
/// the config is still whole.
pub fn write_atomic(path: &Path, new_body: &str, now: OffsetDateTime) -> Result<(), Error> {
    let current = read_existing(path)?;
    if current.as_deref().unwrap_or("") == new_body {
        return Ok(());
    }
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).ok_or_else(|| {
        Error::Other(format!("config: {} has no parent directory", path.display()))
    })?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Other(format!("config: {} has no file name", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let tmp_prefix = format!(".{name}.tmp-");
    let tmp = dir.join(format!("{tmp_prefix}{}", std::process::id()));
    let result = write_temp(&tmp, new_body)
        .map_err(Error::from)
        .and_then(|()| replace_with_temp(path, &tmp, dir, name, new_body, current.is_some(), now));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    prune_backups(dir, name)?;
    remove_stale_temps(dir, &tmp_prefix, &tmp)
}

/// Mode 0600 and fsync. `create` + `truncate` (not `create_new`) so a
/// leftover from an earlier run with the same pid does not block the write;
/// `set_permissions` because `mode()` applies only when the file is created.
fn write_temp(tmp: &Path, body: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(body.as_bytes())?;
    file.sync_all()
}

/// Steps 4 and 5: back up the current file, then rename the temp file over
/// it, or remove both when the new body is only whitespace.
fn replace_with_temp(
    path: &Path,
    tmp: &Path,
    dir: &Path,
    name: &str,
    new_body: &str,
    exists: bool,
    now: OffsetDateTime,
) -> Result<(), Error> {
    if exists {
        make_backup(path, dir, name, now)?;
    }
    if new_body.trim().is_empty() {
        std::fs::remove_file(tmp)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    } else {
        std::fs::rename(tmp, path).map_err(Error::from)
    }
}

/// Hard link to `<name>.<stamp>.bak`, then `-2`, `-3`, ... while the name is
/// taken (a link onto an existing name fails atomically, so two runs in the
/// same second cannot share a backup). Copy when linking is not possible.
fn make_backup(path: &Path, dir: &Path, name: &str, now: OffsetDateTime) -> Result<(), Error> {
    let stamp = backup_stamp(now);
    for n in 1..=999 {
        let backup = dir.join(backup_name(name, &stamp, n));
        match std::fs::hard_link(path, &backup) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                std::fs::copy(path, &backup)?;
                return Ok(());
            }
        }
    }
    Err(Error::Other(format!(
        "config: more than 999 backups of {name} in one second; not writing"
    )))
}

/// Step 6: keep the newest `BACKUPS_KEPT` by `(stamp, suffix)`.
fn prune_backups(dir: &Path, name: &str) -> Result<(), Error> {
    let mut backups: Vec<((String, u32), PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        if let Some(key) = backup_key(name, file) {
            backups.push((key, entry.path()));
        }
    }
    backups.sort();
    let excess = backups.len().saturating_sub(BACKUPS_KEPT);
    for (_, old) in backups.into_iter().take(excess) {
        std::fs::remove_file(old)?;
    }
    Ok(())
}

/// Step 7: `.config.toml.tmp-*` left by earlier failed runs. Our own temp
/// file is already renamed or removed by now; skip it anyway.
fn remove_stale_temps(dir: &Path, prefix: &str, own: &Path) -> Result<(), Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let is_temp = entry.file_name().to_str().is_some_and(|f| f.starts_with(prefix));
        if is_temp && entry.path() != own {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}
```

Run: `cargo test --lib config::`
Expected: the Step 1 tests pass; the old `apply_persist` tests still pass (they call the two-argument form, which still exists until Step 4).

- [ ] **Step 3: Write the failing `apply_persist` tests (new signature)**

Replace `apply_write_and_delete` (lines 346-357) with:

```rust
    #[test]
    fn apply_write_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        apply_persist(PersistAction::Write(SoakHours::new(48).unwrap()), &path, at(0)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SOAK_HOURS = 48\n");
        apply_persist(PersistAction::Delete, &path, at(1)).unwrap();
        assert!(!path.exists());
        assert_eq!(
            names(dir.path()),
            ["config.toml.20231114T221321Z.bak"],
            "the delete left a backup"
        );
        apply_persist(PersistAction::Delete, &path, at(2)).unwrap(); // missing is ok
        assert_eq!(names(dir.path()).len(), 1, "nothing to do: no write, no backup");
    }
```

Replace the three later tests `persist_write_keeps_other_keys`, `persist_delete_removes_only_soak_hours_and_file_only_when_empty`, `persist_leaves_invalid_toml_alone_with_warning` (lines 497-535) with:

```rust
    const COMMENTED: &str = "# keep me\nSOAK_HOURS = 6 # why\nNO_SOAK = [\"wget\"]\n";

    #[test]
    fn persist_write_keeps_other_keys_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        let warn = apply_persist(PersistAction::Write(SoakHours::new(12).unwrap()), &path, at(0)).unwrap();
        assert_eq!(warn, None);
        let p = parse_file(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(p.soak_hours.map(|h| h.get()), Some(12));
        assert_eq!(p.taps.len(), 2);
        assert!(p.no_soak.matches("ericfitz/tap", "x"));
        std::fs::write(&path, COMMENTED).unwrap();
        apply_persist(PersistAction::Write(SoakHours::new(12).unwrap()), &path, at(1)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# keep me\nSOAK_HOURS = 12 # why\nNO_SOAK = [\"wget\"]\n"
        );
    }

    #[test]
    fn persist_same_value_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, COMMENTED).unwrap();
        apply_persist(PersistAction::Write(SoakHours::new(6).unwrap()), &path, at(0)).unwrap();
        assert_eq!(names(dir.path()), ["config.toml"], "no backup for a no-op");
    }

    #[test]
    fn persist_delete_removes_only_soak_hours_and_file_only_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        apply_persist(PersistAction::Delete, &path, at(0)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("SOAK_HOURS"), "{text}");
        assert!(text.contains("NO_SOAK"), "{text}");
        std::fs::write(&path, "SOAK_HOURS = 48\n").unwrap();
        apply_persist(PersistAction::Delete, &path, at(1)).unwrap();
        assert!(!path.exists(), "file with no keys left must be deleted");
        std::fs::write(&path, "# only a comment\nSOAK_HOURS = 48\n").unwrap();
        apply_persist(PersistAction::Delete, &path, at(2)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# only a comment\n",
            "a file holding just comments is kept"
        );
    }

    #[test]
    fn persist_leaves_invalid_toml_alone_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[[").unwrap();
        let warn = apply_persist(PersistAction::Write(SoakHours::new(12).unwrap()), &path, at(0)).unwrap();
        assert!(warn.is_some_and(|w| w.contains("not valid TOML")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[[");
        let warn = apply_persist(PersistAction::Delete, &path, at(1)).unwrap();
        assert!(warn.is_some());
        assert!(path.exists());
        assert_eq!(names(dir.path()), ["config.toml"], "no backup of a file we did not write");
    }
```

Run: `cargo test --lib config::`
Expected: compile error (`apply_persist` takes 2 arguments).

- [ ] **Step 4: Rebuild `apply_persist`**

Replace the whole `apply_persist` function (the old lines 235-283) with:

```rust
/// Key-level edit of `SOAK_HOURS` through the settings editor, so comments
/// and unknown keys survive, written with `write_atomic`. Returns a warning
/// instead of touching a file that is not valid TOML.
pub fn apply_persist(
    action: PersistAction,
    path: &Path,
    now: OffsetDateTime,
) -> Result<Option<String>, Error> {
    if action == PersistAction::None {
        return Ok(None);
    }
    let existing = read_existing(path)?;
    let mut doc = match existing.as_deref() {
        None => DocumentMut::new(),
        Some(s) => match s.parse::<DocumentMut>() {
            Ok(doc) => doc,
            Err(_) => {
                return Ok(Some(format!(
                    "{} is not valid TOML; --soak-hours was not persisted (it still applies to this run)",
                    path.display()
                )));
            }
        },
    };
    let edit = match action {
        PersistAction::None => return Ok(None),
        PersistAction::Write(hours) => settings::set_soak_hours(&mut doc, hours),
        PersistAction::Delete => settings::clear_soak_hours(&mut doc),
    };
    if edit.changed {
        write_atomic(path, &doc.to_string(), now)?;
    }
    Ok(None)
}
```

In `src/lib.rs` line 143, change the call to:

```rust
            && let Some(warning) =
                config::apply_persist(cfg.persist, &world.config_path(), world.now())?
```

Run: `cargo test`
Expected: all pass, including `lib::tests::soak_hours_persists_on_soaked_command` (`"SOAK_HOURS = 48\n"` is exactly what toml_edit renders for a new document).

- [ ] **Step 5: Done gate and commit**

```bash
cd /Users/efitz/Projects/brewsoakr
cargo fmt && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test
git add src/config.rs src/lib.rs
git commit -m "feat(config): atomic config writes with backups; persist keeps comments

write_atomic: temp file (0600, fsync), hard-link backup named by UTC time
with -2/-3 suffixes in the same second, rename, keep 2 backups, sweep stale
temps. apply_persist now edits through settings:: and writes through it.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```
Expected: gate exits 0; commit succeeds.

---

### Task 3: CLI parsing of `settings` and its help text

**Files:**
- Modify: `src/cli.rs` (`Command` enum at lines 12-37; `help_text` at 59-86; `parse_argv` subcommand match at 120-135; `command_help` at 278-371; tests)
- Test: `src/cli.rs` (`mod tests`)

**Interfaces:**
- Consumes: `crate::settings::normalize_tap` (Task 1), `crate::SoakHours`.
- Produces (Task 4 relies on these exact shapes):
  ```rust
  pub enum Command { ..., Settings(SettingsCmd), ... }
  pub enum SettingsCmd { Show, Edit(SettingsEdit) }
  pub enum SettingsEdit {
      SoakHours(Option<SoakHours>),                      // None = --clear
      NoSoakAdd(Vec<String>),                            // raw tokens, validated by the editor
      NoSoakRemove(Vec<String>),
      TapHours { tap: String, hours: Option<SoakHours> } // tap normalized; None = --clear
  }
  ```
  `command_help("settings") -> Some(&'static str)`; `Command::Settings(_).is_soaked() == false`; `Invocation.brew_args` is empty and `soak_hours` is `None` for settings.

- [ ] **Step 1: Write the failing parse tests**

Append inside `mod tests` in `src/cli.rs`:

```rust
    fn settings(args: &[&str]) -> SettingsCmd {
        let i = parse_argv(&s(args)).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        assert!(i.brew_args.is_empty(), "{args:?}: {:?}", i.brew_args);
        assert_eq!(i.soak_hours, None, "{args:?}");
        match i.command {
            Command::Settings(cmd) => cmd,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    fn hours(n: u32) -> Option<SoakHours> {
        Some(SoakHours::new(n).unwrap())
    }

    #[test]
    fn settings_bare_and_show() {
        assert_eq!(settings(&["settings"]), SettingsCmd::Show);
        assert_eq!(settings(&["settings", "show"]), SettingsCmd::Show);
        let i = parse_argv(&s(&["settings", "--raw", "show"])).unwrap();
        assert!(i.raw, "--raw is accepted and ignored");
        assert_eq!(i.command, Command::Settings(SettingsCmd::Show));
        assert!(!Command::Settings(SettingsCmd::Show).is_soaked());
    }

    #[test]
    fn settings_soak_hours_value_and_clear() {
        assert_eq!(
            settings(&["settings", "soak-hours", "48"]),
            SettingsCmd::Edit(SettingsEdit::SoakHours(hours(48)))
        );
        assert_eq!(
            settings(&["settings", "soak-hours", "24"]),
            SettingsCmd::Edit(SettingsEdit::SoakHours(hours(24))),
            "24 is the editor's business (it removes the key)"
        );
        assert_eq!(
            settings(&["settings", "soak-hours", "--clear"]),
            SettingsCmd::Edit(SettingsEdit::SoakHours(None))
        );
    }

    #[test]
    fn settings_no_soak_add_and_remove_keep_tokens_verbatim() {
        assert_eq!(
            settings(&["settings", "no-soak", "add", "WGet", "hashicorp/tap/terraform"]),
            SettingsCmd::Edit(SettingsEdit::NoSoakAdd(s(&["WGet", "hashicorp/tap/terraform"])))
        );
        assert_eq!(
            settings(&["settings", "no-soak", "remove", "wget"]),
            SettingsCmd::Edit(SettingsEdit::NoSoakRemove(s(&["wget"])))
        );
    }

    #[test]
    fn settings_tap_hours_normalizes_the_tap() {
        assert_eq!(
            settings(&["settings", "tap-hours", "HashiCorp/tap", "72"]),
            SettingsCmd::Edit(SettingsEdit::TapHours {
                tap: "hashicorp/tap".into(),
                hours: hours(72)
            })
        );
        assert_eq!(
            settings(&["settings", "tap-hours", "homebrew/core", "--clear"]),
            SettingsCmd::Edit(SettingsEdit::TapHours {
                tap: "homebrew/core".into(),
                hours: None
            })
        );
    }

    #[test]
    fn settings_help_forms() {
        for args in [
            s(&["settings", "--help"]),
            s(&["settings", "-h"]),
            s(&["settings", "no-soak", "-h"]),
            s(&["help", "settings"]),
        ] {
            match parse_argv(&args).unwrap().command {
                Command::Help { topic: Some(t) } => assert_eq!(t, "settings", "{args:?}"),
                other => panic!("{args:?}: {other:?}"),
            }
        }
        let text = command_help("settings").unwrap();
        for word in ["show", "soak-hours", "no-soak add", "no-soak remove", "tap-hours", "--clear", ".bak"] {
            assert!(text.contains(word), "{word}: {text}");
        }
        assert!(help_text().contains("settings"), "{}", help_text());
    }

    #[test]
    fn settings_usage_matrix() {
        for args in [
            s(&["settings", "show", "x"]),
            s(&["settings", "soak-hours"]),
            s(&["settings", "soak-hours", "0"]),
            s(&["settings", "soak-hours", "abc"]),
            s(&["settings", "soak-hours", "48", "x"]),
            s(&["settings", "soak-hours", "--clear", "x"]),
            s(&["settings", "no-soak"]),
            s(&["settings", "no-soak", "add"]),
            s(&["settings", "no-soak", "frob", "x"]),
            s(&["settings", "no-soak", "add", "--flag"]),
            s(&["settings", "tap-hours"]),
            s(&["settings", "tap-hours", "a/b"]),
            s(&["settings", "tap-hours", "bad", "5"]),
            s(&["settings", "tap-hours", "a/b/c", "5"]),
            s(&["settings", "tap-hours", "a/b", "0"]),
            s(&["settings", "tap-hours", "a/b", "5", "x"]),
            s(&["settings", "bogus"]),
            s(&["settings", "-v"]),
            s(&["-v", "settings"]),
            s(&["--verbose", "settings", "show"]),
        ] {
            match parse_argv(&args) {
                Err(Error::Usage(_)) => {}
                other => panic!("{args:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn settings_rejects_soak_hours_flag_in_every_position() {
        for args in [
            s(&["--soak-hours", "1", "settings", "show"]),
            s(&["settings", "--soak-hours=1"]),
            s(&["settings", "soak-hours", "48", "--soak-hours", "1"]),
        ] {
            match parse_argv(&args) {
                Err(Error::Usage(m)) => assert!(m.contains("settings soak-hours"), "{args:?}: {m}"),
                other => panic!("{args:?}: {other:?}"),
            }
        }
    }
```

Run: `cargo test --lib cli::settings`
Expected: compile error (`SettingsCmd`, `SettingsEdit`, `Command::Settings` not found).

- [ ] **Step 2: Add the enums, the parser and the help**

In `src/cli.rs`, change the `use` line to:

```rust
use crate::{Error, SoakHours};
```

Add to `Command` after `Version,`:

```rust
    Settings(SettingsCmd),
```

Add after the `impl Command` block:

```rust
/// `brewsoak settings`: show, or one edit of the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsCmd {
    Show,
    Edit(SettingsEdit),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEdit {
    /// `None` is `--clear`. 24 is passed through; the editor removes the key.
    SoakHours(Option<SoakHours>),
    /// Raw tokens; `settings::no_soak_add` validates and lowercases them.
    NoSoakAdd(Vec<String>),
    NoSoakRemove(Vec<String>),
    /// `tap` is normalized `user/repo`; `hours` `None` is `--clear`.
    TapHours {
        tap: String,
        hours: Option<SoakHours>,
    },
}
```

In `parse_argv`, right after the `if subcommand == "help" { ... }` block (before `let before = ...`):

```rust
    if subcommand == "settings" {
        // Config edits are not a flag: `--soak-hours` is for soaked commands.
        if soak_hours.is_some() {
            return Err(Error::Usage(
                "--soak-hours cannot be combined with settings; use: brewsoak settings soak-hours N"
                    .into(),
            ));
        }
        if sub_idx != 0 {
            return Err(Error::Usage(format!(
                "settings takes no options before it, got: {}",
                remaining[..sub_idx].join(" ")
            )));
        }
        let rest = &remaining[sub_idx + 1..];
        if rest.iter().any(|a| a == "--help" || a == "-h") {
            return Ok(Invocation {
                soak_hours: None,
                command: Command::Help {
                    topic: Some("settings".into()),
                },
                brew_args: Vec::new(),
                raw,
            });
        }
        return Ok(Invocation {
            soak_hours: None,
            command: Command::Settings(parse_settings(rest)?),
            brew_args: Vec::new(),
            raw,
        });
    }
```

Add after `fn passthrough`:

```rust
/// Strict: every verb takes a fixed shape, and anything else is usage.
fn parse_settings(args: &[String]) -> Result<SettingsCmd, Error> {
    let hint = "see: brewsoak help settings";
    let Some((verb, rest)) = args.split_first() else {
        return Ok(SettingsCmd::Show);
    };
    let edit = match verb.as_str() {
        "show" => {
            if !rest.is_empty() {
                return Err(Error::Usage(format!(
                    "settings show takes no arguments, got: {}; {hint}",
                    rest.join(" ")
                )));
            }
            return Ok(SettingsCmd::Show);
        }
        "soak-hours" => {
            let [arg] = rest else {
                return Err(Error::Usage(format!(
                    "settings soak-hours takes exactly one argument: N or --clear; {hint}"
                )));
            };
            SettingsEdit::SoakHours(parse_hours_or_clear(arg, "soak-hours")?)
        }
        "no-soak" => {
            let Some((op, tokens)) = rest.split_first() else {
                return Err(Error::Usage(format!(
                    "settings no-soak needs add or remove and at least one token; {hint}"
                )));
            };
            if tokens.is_empty() {
                return Err(Error::Usage(format!(
                    "settings no-soak {op} needs at least one token; {hint}"
                )));
            }
            if let Some(flag) = tokens.iter().find(|t| t.starts_with('-')) {
                return Err(Error::Usage(format!(
                    "settings no-soak {op} takes tokens, not options, got {flag:?}; {hint}"
                )));
            }
            match op.as_str() {
                "add" => SettingsEdit::NoSoakAdd(tokens.to_vec()),
                "remove" => SettingsEdit::NoSoakRemove(tokens.to_vec()),
                other => {
                    return Err(Error::Usage(format!(
                        "unknown settings no-soak verb {other:?}; expected add or remove; {hint}"
                    )));
                }
            }
        }
        "tap-hours" => {
            let [tap, arg] = rest else {
                return Err(Error::Usage(format!(
                    "settings tap-hours takes exactly two arguments: USER/REPO and N or --clear; {hint}"
                )));
            };
            SettingsEdit::TapHours {
                tap: crate::settings::normalize_tap(tap)?,
                hours: parse_hours_or_clear(arg, "tap-hours")?,
            }
        }
        other => {
            return Err(Error::Usage(format!(
                "unknown settings verb {other:?}; expected show, soak-hours, no-soak, or tap-hours; {hint}"
            )));
        }
    };
    Ok(SettingsCmd::Edit(edit))
}

fn parse_hours_or_clear(arg: &str, verb: &str) -> Result<Option<SoakHours>, Error> {
    if arg == "--clear" {
        return Ok(None);
    }
    arg.parse::<u32>()
        .ok()
        .and_then(SoakHours::new)
        .map(Some)
        .ok_or_else(|| {
            Error::Usage(format!(
                "settings {verb} needs an integer >= 1 or --clear, got {arg:?}"
            ))
        })
}
```

In `help_text()`, replace the block from `Soaked commands:` through the `under NO_SOAK ...` line with:

```text
Soaked commands:
  update, upgrade, install, reinstall, outdated, info
Other brew commands are passed through unchanged. Packages and taps listed
under NO_SOAK in ~/.config/brewsoak/config.toml skip soaking and go to brew.

Settings (edit ~/.config/brewsoak/config.toml; never runs brew):
  settings [show]                         print the effective config
  settings soak-hours N|--clear           set or remove SOAK_HOURS
  settings no-soak add|remove TOKEN...    edit the NO_SOAK list
  settings tap-hours USER/REPO N|--clear  set or remove a tap's soak_hours
```

and add `  brewsoak settings no-soak add wget` to the `Examples:` list.

In `command_help`, add an arm before `_ => return None,`:

```rust
        "settings" => {
            "\
Usage: brewsoak settings [show]
       brewsoak settings soak-hours N|--clear
       brewsoak settings no-soak add|remove TOKEN...
       brewsoak settings tap-hours USER/REPO N|--clear

Show or edit ~/.config/brewsoak/config.toml. Edits keep comments, key order,
and unknown keys. Before each write the previous file is kept as
config.toml.<UTC time>.bak next to it; the 2 newest backups are kept.

  show                  effective soak hours and their source, NO_SOAK as
                        written, every [[TAP]] with its effective hours, and
                        every parse note and warning
  soak-hours N          set SOAK_HOURS (24, the default, removes the key)
  soak-hours --clear    remove SOAK_HOURS
  no-soak add TOKEN...  append tokens not already listed (case-insensitive)
  no-soak remove TOKEN...
                        remove matching tokens; an absent token is reported
  tap-hours USER/REPO N
                        set that tap's soak_hours, adding its [[TAP]] entry
  tap-hours USER/REPO --clear
                        remove soak_hours; an entry left with only name goes

TOKEN is wget, user/repo, or user/repo/name. N is an integer >= 1.
homebrew/core and homebrew/cask are valid USER/REPO values.
BREWSOAK_SOAK_HOURS overrides SOAK_HOURS in the file; soak-hours warns when
it is set. --soak-hours is not accepted with settings. A file that is not
valid TOML is refused, not rewritten.
"
        }
```

Run: `cargo test --lib cli::`
Expected: all pass. `settings_usage_matrix` line `["--verbose", "settings", "show"]`: `--verbose` is not stripped (only `--raw` and `--soak-hours` are), so `sub_idx == 1` and the "no options before it" error fires.

- [ ] **Step 3: Done gate and commit**

```bash
cd /Users/efitz/Projects/brewsoakr
cargo fmt && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test
git add src/cli.rs
git commit -m "feat(cli): parse brewsoak settings strictly; help text

Command::Settings with Show / soak-hours / no-soak add|remove / tap-hours.
Unknown verbs, missing or extra arguments, options before the word, and
--soak-hours with settings are usage errors.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```
Expected: gate exits 0; commit succeeds.

Note for Step 2: adding the `Settings` variant makes the `match inv.command` in `src/lib.rs::dispatch` non-exhaustive, so `cargo build` fails until an arm exists. Add this temporary arm at the end of that match (Task 4 replaces it):

```rust
        // Replaced in Task 4.
        cli::Command::Settings(_) => Ok(Dispatch::Exit(0)),
```

---

### Task 4: Dispatch, `run_settings`, env warning, fake-world tests, README

**Files:**
- Modify: `src/lib.rs` (`use` block; `dispatch` at lines 125-153; new `run_settings` / `edit_settings`; tests)
- Modify: `README.md:49-100` ("Configuration" section)
- Test: `src/lib.rs` (`mod tests`)

**Interfaces:**
- Consumes: `cli::{Command::Settings, SettingsCmd, SettingsEdit}` (Task 3); `settings::{set_soak_hours, clear_soak_hours, no_soak_add, no_soak_remove, tap_hours_set, tap_hours_clear, render_show, Edit}` (Task 1); `config::{read_existing, write_atomic, parse_file}` (Task 2); `World::{config_path, env_soak, now}`.
- Produces: `pub fn run_settings(cmd: &cli::SettingsCmd, path: &Path, env: Option<&str>, now: OffsetDateTime, out: &mut impl std::io::Write) -> Result<(), Error>`; `dispatch` returns `Ok(Dispatch::Exit(0))` for every settings verb that does not error.

- [ ] **Step 1: Write the failing fake-world tests**

Append inside `mod tests` in `src/lib.rs`:

```rust
    fn assert_nothing_external_ran(world: &TestWorld, what: &str) {
        assert!(world.brew.runs.lock().unwrap().is_empty(), "{what}: brew ran");
        assert!(
            world.brew.visible_runs.lock().unwrap().is_empty(),
            "{what}: brew ran visibly"
        );
        assert!(!world.github.refreshed.get(), "{what}: github was queried");
        assert!(
            !world.cache_path().exists(),
            "{what}: settings must not create the cache"
        );
    }

    fn config_dir_names(world: &TestWorld) -> Vec<String> {
        let dir = world.config_path();
        let dir = dir.parent().unwrap();
        let mut v: Vec<String> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        v.sort();
        v
    }

    #[test]
    fn settings_never_runs_brew_git_or_snapshots() {
        for args in [
            s(&["settings"]),
            s(&["settings", "show"]),
            s(&["settings", "soak-hours", "48"]),
            s(&["settings", "no-soak", "add", "wget"]),
            s(&["settings", "no-soak", "remove", "nope"]),
            s(&["settings", "tap-hours", "a/b", "5"]),
            s(&["settings", "tap-hours", "a/b", "--clear"]),
            s(&["settings", "soak-hours", "--clear"]),
        ] {
            let world = TestWorld::new();
            match dispatch(&args, &world) {
                Ok(Dispatch::Exit(0)) => {}
                other => panic!("{args:?}: {other:?}"),
            }
            assert_nothing_external_ran(&world, &args.join(" "));
        }
    }

    #[test]
    fn settings_with_soak_hours_flag_is_usage_and_writes_nothing() {
        let world = TestWorld::new();
        match dispatch(&s(&["--soak-hours", "1", "settings", "show"]), &world) {
            Err(Error::Usage(m)) => assert!(m.contains("settings soak-hours"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(!world.config_path().exists());
    }

    #[test]
    fn settings_edits_round_trip_and_show_reports_them() {
        let world = TestWorld::new();
        let path = world.config_path();
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::NoSoakAdd(s(&["WGet", "ericfitz/tap"]))),
            &path,
            None,
            now(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            format!("added to NO_SOAK: wget\nadded to NO_SOAK: ericfitz/tap\nwrote {}\n", path.display())
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "NO_SOAK = [\"wget\", \"ericfitz/tap\"]\n"
        );
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::TapHours {
                tap: "hashicorp/tap".into(),
                hours: Some(SoakHours::new(72).unwrap()),
            }),
            &path,
            None,
            now(),
            &mut out,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "NO_SOAK = [\"wget\", \"ericfitz/tap\"]\n\n[[TAP]]\nname = \"hashicorp/tap\"\nsoak_hours = 72\n"
        );
        let mut out = Vec::new();
        run_settings(&cli::SettingsCmd::Show, &path, Some("36"), now(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("soak hours: 36 (BREWSOAK_SOAK_HOURS)\n"), "{text}");
        assert!(text.contains("  wget\n  ericfitz/tap\n"), "{text}");
        assert!(text.contains("  hashicorp/tap: 72 (own)\n"), "{text}");
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::NoSoakRemove(s(&["wget", "nope"]))),
            &path,
            None,
            now(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("removed from NO_SOAK: wget\nnot in NO_SOAK: nope\n"), "{text}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("NO_SOAK = [\"ericfitz/tap\"]\n"),
            "comment-free edit keeps the rest"
        );
    }

    #[test]
    fn settings_show_without_a_file_prints_the_defaults() {
        let world = TestWorld::new();
        let mut out = Vec::new();
        run_settings(&cli::SettingsCmd::Show, &world.config_path(), None, now(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("(no config file)\n"), "{text}");
        assert!(text.contains("soak hours: 24 (default)\n"), "{text}");
        assert!(!world.config_path().exists(), "show never writes");
    }

    #[test]
    fn settings_refuses_invalid_toml_and_leaves_it_alone() {
        let world = TestWorld::new();
        let path = world.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[[[").unwrap();
        for cmd in [
            cli::SettingsCmd::Show,
            cli::SettingsCmd::Edit(cli::SettingsEdit::NoSoakAdd(s(&["wget"]))),
            cli::SettingsCmd::Edit(cli::SettingsEdit::SoakHours(None)),
        ] {
            let mut out = Vec::new();
            match run_settings(&cmd, &path, None, now(), &mut out) {
                Err(Error::Refusal(m)) => {
                    assert!(m.contains(&path.display().to_string()), "{cmd:?}: {m}");
                    assert!(m.contains("TOML parse error"), "{cmd:?}: {m}");
                }
                other => panic!("{cmd:?}: {other:?}"),
            }
            assert!(out.is_empty(), "{cmd:?}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[[");
        assert_eq!(config_dir_names(&world), ["config.toml"], "no backup, no temp file");
    }

    #[test]
    fn settings_second_identical_edit_writes_nothing() {
        let world = TestWorld::new();
        let add = s(&["settings", "no-soak", "add", "wget"]);
        dispatch(&add, &world).unwrap();
        assert_eq!(config_dir_names(&world), ["config.toml"]);
        dispatch(&add, &world).unwrap();
        assert_eq!(config_dir_names(&world), ["config.toml"], "no-op edit: no write, no backup");
        dispatch(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert_eq!(config_dir_names(&world).len(), 2, "a real edit leaves one backup");
        dispatch(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert_eq!(config_dir_names(&world).len(), 2, "repeating it adds nothing");
        let text = std::fs::read_to_string(world.config_path()).unwrap();
        assert_eq!(text, "NO_SOAK = [\"wget\"]\nSOAK_HOURS = 48\n");
    }

    #[test]
    fn settings_invalid_token_is_usage_and_writes_nothing() {
        let world = TestWorld::new();
        match dispatch(&s(&["settings", "no-soak", "add", "a/b/c/d"]), &world) {
            Err(Error::Usage(m)) => assert!(m.contains("a/b/c/d"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(!world.config_path().exists());
    }

    #[test]
    fn settings_clear_last_key_removes_the_file() {
        let world = TestWorld::new();
        dispatch(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert!(world.config_path().exists());
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::SoakHours(None)),
            &world.config_path(),
            None,
            now(),
            &mut out,
        )
        .unwrap();
        assert!(!world.config_path().exists(), "nothing left: file removed");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(&format!("removed {} (nothing left)", world.config_path().display())), "{text}");
        assert_eq!(config_dir_names(&world).len(), 1, "the backup remains");
    }

    #[test]
    fn soak_hours_persistence_keeps_comments() {
        let world = TestWorld::new();
        std::fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
        std::fs::write(world.config_path(), "# keep me\nSOAK_HOURS = 6 # why\n").unwrap();
        match dispatch(&s(&["--soak-hours", "48", "outdated"]), &world).expect("outdated") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(world.config_path()).unwrap(),
            "# keep me\nSOAK_HOURS = 48 # why\n"
        );
    }

    #[test]
    fn settings_soak_hours_still_writes_when_env_is_set() {
        let world = TestWorld {
            env_soak: Some("36".into()),
            ..TestWorld::new()
        };
        dispatch(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert_eq!(
            std::fs::read_to_string(world.config_path()).unwrap(),
            "SOAK_HOURS = 48\n",
            "the warning (stderr) does not stop the edit"
        );
    }
```

Run: `cargo test --lib tests::settings`
Expected: compile error (`run_settings` not found).

- [ ] **Step 2: Implement `run_settings` and the dispatch branch**

In `src/lib.rs`, change the `use` lines near the top to:

```rust
use crate::brew::Brew;
use std::io::Write;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use toml_edit::DocumentMut;
```

In `dispatch`, right after `world.brew().set_raw(inv.raw);` and before `let env = world.env_soak();`:

```rust
    if let cli::Command::Settings(cmd) = &inv.command {
        // Before config resolution, brew, git, or snapshots: settings reads
        // and writes the file and nothing else, and refuses invalid TOML
        // where resolve_config would silently default it.
        let env = world.env_soak();
        run_settings(
            cmd,
            &world.config_path(),
            env.as_deref(),
            world.now(),
            &mut std::io::stdout(),
        )?;
        return Ok(Dispatch::Exit(0));
    }
```

Remove the temporary `cli::Command::Settings(_) => Ok(Dispatch::Exit(0)),` arm from Task 3 and replace it with:

```rust
        cli::Command::Settings(_) => unreachable!("settings returns before config resolution"),
```

Add after `dispatch` (before `fn soaked_exit`):

```rust
/// Spec "brewsoak settings". Reads the file once, applies one edit, and
/// writes the whole document back only when the edit changed it.
pub fn run_settings(
    cmd: &cli::SettingsCmd,
    path: &Path,
    env: Option<&str>,
    now: OffsetDateTime,
    out: &mut impl Write,
) -> Result<(), Error> {
    let existing = config::read_existing(path)?;
    let doc = match existing.as_deref() {
        None => None,
        Some(text) => Some(text.parse::<DocumentMut>().map_err(|e| {
            Error::Refusal(format!(
                "{} is not valid TOML; fix it by hand before using settings:\n{e}",
                path.display()
            ))
        })?),
    };
    match cmd {
        cli::SettingsCmd::Show => {
            write!(out, "{}", settings::render_show(path, env, doc.as_ref()))?;
            Ok(())
        }
        cli::SettingsCmd::Edit(edit) => {
            edit_settings(edit, doc.unwrap_or_default(), path, env, now, out)
        }
    }
}

fn edit_settings(
    cmd: &cli::SettingsEdit,
    mut doc: DocumentMut,
    path: &Path,
    env: Option<&str>,
    now: OffsetDateTime,
    out: &mut impl Write,
) -> Result<(), Error> {
    // The reader's warnings (a NO_SOAK inside [[TAP]]) fire on edits too;
    // dispatch skipped its own warning loop for settings.
    for warning in &config::parse_file(&doc.to_string()).warnings {
        eprintln!("brewsoak: warning: {warning}");
    }
    let edit = match cmd {
        cli::SettingsEdit::SoakHours(hours) => {
            if let Some(raw) = env {
                eprintln!(
                    "brewsoak: warning: BREWSOAK_SOAK_HOURS={raw} is set and overrides SOAK_HOURS in the file"
                );
            }
            match hours {
                Some(h) => settings::set_soak_hours(&mut doc, *h),
                None => settings::clear_soak_hours(&mut doc),
            }
        }
        cli::SettingsEdit::NoSoakAdd(tokens) => settings::no_soak_add(&mut doc, tokens)?,
        cli::SettingsEdit::NoSoakRemove(tokens) => settings::no_soak_remove(&mut doc, tokens)?,
        cli::SettingsEdit::TapHours {
            tap,
            hours: Some(h),
        } => settings::tap_hours_set(&mut doc, tap, *h)?,
        cli::SettingsEdit::TapHours { tap, hours: None } => {
            settings::tap_hours_clear(&mut doc, tap)?
        }
    };
    for message in &edit.messages {
        writeln!(out, "{message}")?;
    }
    if !edit.changed {
        return Ok(());
    }
    let body = doc.to_string();
    config::write_atomic(path, &body, now)?;
    if body.trim().is_empty() {
        writeln!(out, "removed {} (nothing left)", path.display())?;
    } else {
        writeln!(out, "wrote {}", path.display())?;
    }
    Ok(())
}
```

Run: `cargo test`
Expected: all pass. If `settings_second_identical_edit_writes_nothing` fails on the final text, note that `no-soak add` ran first so `NO_SOAK` precedes `SOAK_HOURS`; the assertion already expects that order. Do not reorder keys.

- [ ] **Step 3: Update the README "Configuration" section**

In `README.md`, replace the paragraph

```
Effective soak hours for a package: a `NO_SOAK` match means no soak at all;
else the origin tap's `[[TAP]]` `soak_hours`; else `SOAK_HOURS`. `[[TAP]]` and
`NO_SOAK` are file-only (no flag, no environment variable). `homebrew/core`
and `homebrew/cask` are valid tap names in both.
```

with

```
Effective soak hours for a package: a `NO_SOAK` match means no soak at all;
else the origin tap's `[[TAP]]` `soak_hours`; else `SOAK_HOURS`. `[[TAP]]` and
`NO_SOAK` have no flag and no environment variable: edit the file, or use
`brewsoak settings` below. `homebrew/core` and `homebrew/cask` are valid tap
names in both.
```

Replace the paragraph

```
`--soak-hours N` edits only the `SOAK_HOURS` key; `[[TAP]]` and `NO_SOAK` are
kept. A config file that is not valid TOML is left alone with a warning.
```

with (the outer fence here is four backticks only because the README text itself contains a fenced block; copy what is inside it)

````
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
reported and is not an error. `tap-hours USER/REPO --clear` removes the
entry when only `name` would remain. When nothing is left in the file, the
file is removed.

Every write goes to a temp file first, the previous `config.toml` is kept as
`config.toml.<UTC time>.bak` next to it (the 2 newest backups are kept), and
the temp file is renamed into place, so another `brewsoak` never sees a
missing or partial file. An unchanged edit writes nothing. A file that is not
valid TOML is refused, with the parse error, and left untouched.

`settings` never runs `brew` or `git` and does not check whether a tap is
installed. `--soak-hours` is not accepted with `settings`.
````

Check with `rg -n 'file-only|brewsoak settings' /Users/efitz/Projects/brewsoakr/README.md`: `file-only` has no matches; `brewsoak settings` appears in the new section.

- [ ] **Step 4: Done gate, manual smoke run, commit**

```bash
cd /Users/efitz/Projects/brewsoakr
cargo fmt && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test
```
Expected: exit 0 at every stage.

Smoke run against a scratch `HOME` so the real config is untouched (`paths::home_dir` reads `$HOME`, `src/paths.rs:3-7`, and `config_file` is `$HOME/.config/brewsoak/config.toml`):

```bash
cd /Users/efitz/Projects/brewsoakr
SCRATCH=$(mktemp -d)
HOME="$SCRATCH" target/debug/brewsoak settings
HOME="$SCRATCH" target/debug/brewsoak settings no-soak add WGet ericfitz/tap
HOME="$SCRATCH" target/debug/brewsoak settings tap-hours HashiCorp/tap 72
HOME="$SCRATCH" target/debug/brewsoak settings soak-hours 48
HOME="$SCRATCH" target/debug/brewsoak settings show
HOME="$SCRATCH" target/debug/brewsoak settings bogus; echo "exit $?"
ls -la "$SCRATCH/.config/brewsoak/"
cat "$SCRATCH/.config/brewsoak/config.toml"
rm -rf "$SCRATCH"
```
Expected: `show` lists `48 (SOAK_HOURS in the file)`, `wget`, `ericfitz/tap`, `hashicorp/tap: 72 (own)`; `bogus` prints a usage error and `exit 2`; the directory holds `config.toml` (mode `-rw-------`) and two `config.toml.*.bak` files; the file reads `NO_SOAK = ["wget", "ericfitz/tap"]` then `SOAK_HOURS = 48` (keys in the order they were added, both above the tables), a blank line, then the `[[TAP]]` entry. Never run this against the real `~/.config/brewsoak`.

```bash
git add src/lib.rs README.md
git commit -m "feat(settings): dispatch brewsoak settings; README

settings runs before config resolution and any brew, git, or snapshot work,
refuses invalid TOML, prints the editor's messages, and writes through
config::write_atomic. soak-hours warns when BREWSOAK_SOAK_HOURS is set.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```
Expected: commit succeeds.

- [ ] **Step 5: Final gate on the whole branch**

```bash
cd /Users/efitz/Projects/brewsoakr
git status --short
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test
git log --oneline main..issue-1-settings-command
```
Expected: clean tree; gate exits 0; four commits on the branch. Report the gate output verbatim to the orchestrator, who squashes the branch into one commit on `main`.

---

## State (spec "State", where it is tested)

- **Second run of the same edit:** the editor returns `changed == false`, nothing is written and no backup is made (`settings_second_identical_edit_writes_nothing`, `unchanged_body_makes_no_write_and_no_backup`, `persist_same_value_writes_nothing`).
- **Two overlapping runs:** no lock. Each rename is atomic and the last writer wins; both make a backup (a same-second collision gets `-2`, tested in `same_second_backups_get_suffixes_and_prune_oldest_first`). The stale-temp sweep can delete the other run's in-flight temp file; that run then fails with `Error::Io` and the config is still whole. Not unit-testable without threads against a real directory; documented in the `write_atomic` doc comment.
- **Leftovers from a failed run:** `.config.toml.tmp-*` (any pid, including our own) is removed by the next write (`stale_temp_files_are_removed`); an extra `.bak` is pruned by the next write (`backups_keep_the_two_newest`).

## Self-review notes

- Spec coverage: commands table (Tasks 1, 3, 4); `show` output (Task 1 `render_show`, Task 4 dispatch); validation and idempotence (Task 1); CLI rules (Task 3, `--raw` in `settings_bare_and_show`, help forms); format-preserving edits (Task 1, verified facts); invalid TOML (Task 4 `settings_refuses_invalid_toml_and_leaves_it_alone`, Task 2 `persist_leaves_invalid_toml_alone_with_warning`); `write_atomic` steps 1-7 (Task 2, one test per step); state (above); components (one task each); docs (Task 4 Step 3 and Task 3 help).
- Decisions the spec left open, resolved here: an emptied `NO_SOAK` stays as `NO_SOAK = []` (the spec removes a `[[TAP]]` entry only for the tap case); `no-soak remove` validates its tokens like `add` (the spec's validation sentence is not add-only), so an invalid entry already in the file must be removed by hand; `NO_SOAK` or `TAP` of the wrong type is refused rather than overwritten; a wrong-typed `SOAK_HOURS` is overwritten; duplicate `[[TAP]]` entries edit the last one; `settings` edits print the reader's warnings to stderr and `show` puts them in its stdout listing; the `soak-hours` env warning also fires on `--clear`.
- Type consistency: `Edit`, `SettingsCmd::Edit(SettingsEdit)`, `run_settings(cmd, path, env, now, out)`, `write_atomic(path, body, now)`, `apply_persist(action, path, now)` are spelled the same in every task.
