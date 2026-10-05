# Tap Soaking and the No-Soak List Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Soak every installed third-party tap (with optional per-tap soak hours) from brewsoak's own blobless clones, and let a `NO_SOAK` list hand named packages and whole taps straight to `brew` so they end in the same state as `brew upgrade`.

**Architecture:** Config gains `[[TAP]]` and `NO_SOAK`; a new `inventory` module classifies every installed package (origin tap + soaked / no-soak / unsoakable) from `brew info` + `brew tap-info` + `origins.toml`. Tap snapshots are bare blobless clones under `<cache>/taps/<user>/<repo>.git` with `refs/brewsoak/{cutoff,head}` pins found by `rev-list --before`; `state.toml` gains a `[taps."user/repo"]` table per tap. Soaked tap packages reuse the v1 desired-state table and path-install flow from a per-tap staging directory; no-soak packages are batched into one `brew update` + one `brew upgrade|install|reinstall` after all soaked work.

**Tech Stack:** Rust 2024, serde (+derive), toml 0.8, time, ureq (unchanged). `std::process::Command` for git/brew behind the existing `GitStore` / `Brew` traits. `tempfile` dev-dependency. Fakes: `MockBrew`, `InMemoryGit`, `StaticGithub`.

**Spec:** `docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md` (extends `docs/superpowers/specs/2026-08-12-brewsoakr-design.md`). The spec's "Human decisions" section is locked.

## Spec issues

None of these change a Human decision. Each is planned around the recommendation below.

1. **`git fetch --filter=blob:none` needs a named promisor remote.** Verified on git 2.56: fetching with a filter from a raw URL works but registers the URL itself as a promisor "remote name" (garbage `remote.<url>.promisor` config plus warnings), and on-demand blob fetches for `git show` only work through a registered promisor remote. Resolution: `git remote add origin <url>` (or `set-url` when it exists) in the bare clone, then `fetch --force --filter=blob:none origin +HEAD:refs/brewsoak/head`. The cutoff pin is set with `git update-ref refs/brewsoak/cutoff <sha>` (the commit is already in the full history; a depth-1 fetch into a full-history clone would write a `shallow` file and corrupt later walks). Both refs are still force-updated pins.
2. **`homebrew/core` and `homebrew/cask` show `remote: null` in `brew tap-info --json --installed`** on API-mode brew (verified on this machine). Classification matches core, cask, and the staging tap by name *before* applying the "remote missing → unsoakable" rule.
3. **Per-tap hours for `homebrew/core` / `homebrew/cask`** need `core_hours` / `cask_hours` in `state.toml` (both default to `hours` when loading a v1 file). The spec's "refresh when stored hours differ" rule is stated for taps; v1 `outdated`/`info` reuse the core/cask snapshot regardless of hours, and this plan keeps that v1 behavior for core/cask (changing it is a separate decision).
4. **`brew deps --1 --formula <staged path>` on a tap formula with bare same-tap deps** can fail ("No available formula") because a path-loaded formula has no tap context. This is the same failure class as the spec's staged-load hold, one step earlier. Resolution: an `Error::Brew` from `deps()` for a tap-origin staged path holds that package with the spec's note (`<name>: cannot be installed from a staged copy; use brew, or add it to NO_SOAK`) instead of aborting the run.
5. **Tap switch via `brew install user/repo/name` over an installed keg** may only print "already installed" and exit nonzero, and `merge_status` currently rewrites that to success. Resolution: the switch run does not apply the already-installed masking; if brew did not report installing the package, add a note `<name>: brew did not replace the staged keg; run brew reinstall user/repo/name` and keep brew's status. The real-brew check is a named step in Task 13; if it fails, `brew reinstall user/repo/name` is the fallback (same spec intent: land on the real tap).
6. **Pinned no-soak packages.** `brew upgrade <pinned>` errors out the whole invocation. Resolution: a bare `upgrade` excludes pinned no-soak packages from the no-soak token list (counted as pinned, like v1); an explicitly named pinned package is passed to brew, which errors as it would by hand.
7. **Explicit `user/tap/pkg` whose tap is not in `tap-info --installed`** (and a receipt tap that is no longer tapped) has no known remote. Resolution: treat as unsoakable with the hint `tap user/tap is not installed; run brew tap user/tap first, or add it to NO_SOAK`; a no-soak entry still goes to brew, which auto-taps.
8. **The summary golden cannot live under `tests/`**: `InMemoryGit` is `#[cfg(test)]`. It is a unit test in `src/cmd.rs` reading `tests/fixtures/upgrade_summary.txt` with the same `BREWSOAK_BLESS=1` convention as `tests/quiet_golden.rs`.

## Global Constraints

Project rules (from `AGENTS.md` and the specs):

- Install soaked packages from a staged `.rb` path under the brewsoak cache, never via a `brewsoakr/soaked/<name>` token.
- Every brewsoak-driven `brew` child gets `HOMEBREW_DEVELOPER=1`, `HOMEBREW_NO_AUTO_UPDATE=1`, `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1`, and `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` unset (`brew::apply_brewsoak_brew_env`). This includes the no-soak `brew update` / `brew upgrade` runs.
- Never pass `--ignore-dependencies`. Install the cutoff dep closure first; let brew see those deps as satisfied.
- Cellar receipts omit `bottle`/`rebuild`; a missing rebuild is unknown, not `0` (`PkgIdentity::same_artifact` already handles this; do not bypass it).
- `-v`/`--verbose` prints the soak window, cutoffs, and a line for every package evaluated. Bare `-v` is brewsoak help.
- Persist `--soak-hours` only on soaked commands, never on passthrough/`--version`/`--help`. Persistence is now a key-level edit that keeps `[[TAP]]` and `NO_SOAK`.
- Do not uninstall a `homebrew/core` keg to switch taps.
- Every byte of `brew` output goes to the per-run log; the terminal gets a summary. Never suppress a line without logging it. Unrecognized lines pass through verbatim. `--raw` always turns the summarizer off. Visible runs put both streams in `Output.stdout`; `Output.stderr` is empty for visible runs.
- The installed snapshot goes stale as soon as brew upgrades a dependency; compare against `quiet::installed_from_output` (`ApplySession::done`) before spawning another `brew install`.
- Every git invocation goes through `ProcessGit` / `GitStore` in `src/git.rs`. Every git failure is `Error::Git { action, detail }` with `action` in brewsoak's words and `detail` = git's stderr. Never leak a raw git message as the only user-visible text.
- Soak refs (`refs/brewsoak/cutoff`, `refs/brewsoak/head`, `refs/brewsoak/window`) are force-updated pins, never fast-forwards.
- brewsoak never fetches into, or moves the checkout of, a repo under `$(brew --repository)/Library/Taps`. Tap history comes only from `<cache>/taps/<user>/<repo>.git`.
- No GitHub API calls for third-party taps.
- `NO_SOAK` overrides every soak time. A no-soak package is installed by brew from its real tap, never from a staged file.
- `brew update` runs at most once per run, and only when a no-soak package is involved (or, for `brewsoak update`, when any installed package is no-soak).
- Exit status table is unchanged: `0` nothing to do / all soaked actions ok, `1` any refusal or hold, brew's code when brew failed for an eligible package (keep `1` unless brew's code is `> 1`), `2` usage.
- Unit tests never touch the network or a real Homebrew.

Environment and hygiene (macOS host):

- No GNU `timeout`/`coreutils`. `rg -E` is the *encoding* flag; use plain `rg 'a|b'`.
- Use `rg`, not `grep`. Stage only the files named in each commit step; never `git add -A`. No `git stash`.
- After every task: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. All three must be clean before the commit step.
- Commit messages in this plan end with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F
  ```

## Review Focus

Inputs the spec implies but does not spell out; each has a test added to the owning task.

1. **Pinned no-soak package in a bare `upgrade`** (`brew upgrade <pinned>` fails the whole batch): it must be counted as pinned and left out of the no-soak tokens. Test in Task 9 (`upgrade_bare_leaves_pinned_no_soak_out_of_brew_tokens`).
2. **Explicit `user/tap/pkg` for a tap that is not installed**: must be refused with the "run brew tap" hint, not crash on a missing clone, and must still go to brew when it is no-soak. Test in Task 9 (`explicit_token_for_untapped_tap_is_refused_with_tap_hint`).
3. **`brew deps` failing on a staged tap formula**: hold that package with the staged-copy note, keep going with the next package. Test in Task 10 (`deps_failure_on_staged_tap_formula_holds_with_note`).
4. **Tap-switch `brew install` that only says "already installed"**: must not be reported as success. Test in Task 8 (`switch_install_that_did_not_replace_keg_keeps_status_and_notes`).
5. **Tap younger than its soak window (`cutoff = None`)**: every package in it reads as "too new" through the existing table; nothing panics on the missing cutoff tree. Test in Task 9 (`tap_without_cutoff_holds_its_packages_as_too_new`).

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/nosoak.rs` | `NO_SOAK` entry grammar + matcher (`NoSoakEntry`, `NoSoakList`); the no-soak brew step (`Target`, `run_step`, `brew_token`) | Create |
| `src/config.rs` | `Config` (hours, persist, `[[TAP]]`, `NO_SOAK`, notes), `parse_file`, `resolve_config`, `effective_hours`, key-level `apply_persist` | Modify |
| `src/origin.rs` | Origin constants, `split_tap`, `OriginRecords` (`origins.toml`), `resolve_origin` | Create |
| `src/git.rs` | `GitStore` additions: `set_remote`, `fetch_history` (blobless + fallback), `rev_list_before`, `update_ref`, `ls_tree`; `ProcessGit` tree cache; `InMemoryGit` keyed by `(dir, ref)` with commits/trees/failing remotes | Modify |
| `src/taps.rs` | `TapInfo` + `tap-info` JSON parsing, `TapClass` + `classify`, `clone_dir`, `staging_root`, `resolve_path` (Homebrew search order), `staged_load_failure` | Create |
| `src/brew.rs` | `InstalledPkg.tap`, `Brew::tap_info`, `Brew::outdated_names`, `MockBrew.taps` / `.outdated`, stop dropping third-party in `parse_installed_json` (Task 9), `pub(crate)` JSON helpers | Modify |
| `src/snapshot.rs` | `TapState`, `Snapshots` gains `core_hours`/`cask_hours`/`taps`/`held_taps`, `RefreshPlan`, `refresh_with`, `refresh_taps`, `tap_needs_refresh`, serde state file with `[taps]` | Modify |
| `src/inventory.rs` | `PkgClass`, `Pkg`, `Inventory` (`build`, `load`, `find`, `needed_taps`, `any_no_soak`), `class_for`, `Token` + `parse_token` | Create |
| `src/cmd.rs` | Origin-aware resolution, inventory/config plumbing, unsoakable refusals, held-tap notes, no-soak collection + step, per-tap staging, origin records, cross-origin dep closure, `update` runs `brew update`, `outdated`/`info` additions, summary golden | Modify |
| `src/report.rs` | `Counts.no_soak`, `counts_line`, `origin_line` | Modify |
| `src/lib.rs` | Build `Config` + `Inventory`, pass to `ensure_snapshots` and commands, drop `third_party_only_names` passthrough, print config notes under `-v` | Modify |
| `src/cli.rs` | Help text updates (Task 12) | Modify |
| `tests/fixtures/upgrade_summary.txt` | Golden summary fixture (Task 11) | Create |
| `README.md`, `AGENTS.md` | Docs (Task 12) | Modify |

Existing test doubles that implement traits by hand and must keep compiling: `FailShowGit` (`src/cmd.rs` tests) implements `GitStore`; `RecordingGithub` (`src/lib.rs` tests) implements `GithubApi`. Every new `GitStore` method therefore gets a default impl.

---

### Task 1: NO_SOAK entry grammar and matcher

**Files:**
- Create: `src/nosoak.rs`
- Modify: `src/lib.rs:1-16` (add `pub mod nosoak;`)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  ```rust
  pub enum NoSoakEntry { Name(String), Tap(String), TapPkg { tap: String, name: String } }
  pub fn parse_entry(raw: &str) -> Result<NoSoakEntry, String>;   // Err = reason text for a -v note
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct NoSoakList { entries: Vec<NoSoakEntry> }
  impl NoSoakList {
      pub fn new(entries: Vec<NoSoakEntry>) -> Self;
      pub fn matches(&self, origin: &str, name: &str) -> bool;  // case-insensitive
      pub fn is_empty(&self) -> bool;
  }
  ```

- [ ] **Step 1: Write the failing tests**

Create `src/nosoak.rs` with only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_matches_any_origin() {
        let list = NoSoakList::new(vec![parse_entry("wget").unwrap()]);
        assert!(list.matches("homebrew/core", "wget"));
        assert!(list.matches("acme/tools", "WGET"));
        assert!(!list.matches("homebrew/core", "wget2"));
    }

    #[test]
    fn one_slash_is_a_whole_tap() {
        let list = NoSoakList::new(vec![parse_entry("EricFitz/Tap").unwrap()]);
        assert!(list.matches("ericfitz/tap", "brewsoak"));
        assert!(list.matches("ericfitz/tap", "anything"));
        assert!(!list.matches("homebrew/core", "brewsoak"));
    }

    #[test]
    fn two_slashes_is_one_package_from_one_tap() {
        let list = NoSoakList::new(vec![parse_entry("hashicorp/tap/terraform").unwrap()]);
        assert!(list.matches("hashicorp/tap", "terraform"));
        assert!(!list.matches("hashicorp/tap", "vault"));
        assert!(!list.matches("homebrew/core", "terraform"));
    }

    #[test]
    fn core_and_cask_are_valid_tap_entries() {
        let list = NoSoakList::new(vec![parse_entry("homebrew/cask").unwrap()]);
        assert!(list.matches("homebrew/cask", "firefox"));
        assert!(!list.matches("homebrew/core", "firefox"));
    }

    #[test]
    fn malformed_entries_are_rejected_with_a_reason() {
        for raw in ["", "  ", "a/b/c/d", "/tap", "tap/", "a//c", "a/b/"] {
            let err = parse_entry(raw).expect_err(raw);
            assert!(err.contains(&format!("{raw:?}")) || raw.trim().is_empty(), "{raw:?}: {err}");
        }
    }

    #[test]
    fn empty_list_matches_nothing() {
        assert!(NoSoakList::default().is_empty());
        assert!(!NoSoakList::default().matches("homebrew/core", "wget"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test nosoak`
Expected: compile error (`parse_entry`, `NoSoakList` not found) after adding `pub mod nosoak;` to `src/lib.rs`.

- [ ] **Step 3: Write the implementation** (above the test module in `src/nosoak.rs`)

```rust
//! The `NO_SOAK` list: packages and taps brewsoak hands straight to brew.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoSoakEntry {
    /// `wget`: that formula or cask from any origin.
    Name(String),
    /// `ericfitz/tap`: every package whose origin is that tap.
    Tap(String),
    /// `hashicorp/tap/terraform`: that package from that tap only.
    TapPkg { tap: String, name: String },
}

/// Parse one `NO_SOAK` entry. `Err` carries the reason, worded for a `-v` note.
/// Matching is case-insensitive, so entries are lowercased here.
pub fn parse_entry(raw: &str) -> Result<NoSoakEntry, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("NO_SOAK entry is empty; skipped".to_string());
    }
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("NO_SOAK entry {raw:?} has an empty path segment; skipped"));
    }
    let lower = raw.to_ascii_lowercase();
    match parts.len() {
        1 => Ok(NoSoakEntry::Name(lower)),
        2 => Ok(NoSoakEntry::Tap(lower)),
        3 => {
            let (tap, name) = lower.rsplit_once('/').expect("three segments");
            Ok(NoSoakEntry::TapPkg {
                tap: tap.to_string(),
                name: name.to_string(),
            })
        }
        _ => Err(format!("NO_SOAK entry {raw:?} has more than two slashes; skipped")),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoSoakList {
    entries: Vec<NoSoakEntry>,
}

impl NoSoakList {
    pub fn new(entries: Vec<NoSoakEntry>) -> Self {
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `origin` is `user/repo` (core and cask included); `name` is the formula
    /// name or cask token. Both compare case-insensitively.
    pub fn matches(&self, origin: &str, name: &str) -> bool {
        let origin = origin.to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        self.entries.iter().any(|e| match e {
            NoSoakEntry::Name(n) => *n == name,
            NoSoakEntry::Tap(t) => *t == origin,
            NoSoakEntry::TapPkg { tap, name: n } => *tap == origin && *n == name,
        })
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test nosoak`
Expected: 6 passed.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/nosoak.rs src/lib.rs
git commit -m "feat: parse and match NO_SOAK entries

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 2: Config: `[[TAP]]`, `NO_SOAK`, effective hours, key-level persist

**Files:**
- Modify: `src/config.rs` (whole file)
- Modify: `src/lib.rs:120-130` (`dispatch` uses `resolve_config`; prints notes under `-v`; prints persist warning)
- Test: `src/config.rs` tests, `src/lib.rs` test `soak_hours_persists_on_soaked_command` (unchanged expectation `SOAK_HOURS = 48\n`)

**Interfaces:**
- Consumes: `nosoak::{parse_entry, NoSoakList}`, `SoakHours`, existing `resolve_hours` / `PersistAction` / `ResolvedHours`.
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct TapEntry { pub name: String /* lowercase user/repo */, pub soak_hours: Option<SoakHours> }
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct ParsedFile { pub soak_hours: Option<SoakHours>, pub taps: Vec<TapEntry>, pub no_soak: NoSoakList, pub notes: Vec<String> }
  pub fn parse_file(contents: &str) -> ParsedFile;      // bad TOML => ParsedFile::default()
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Config { pub hours: SoakHours, pub persist: PersistAction, pub taps: Vec<TapEntry>, pub no_soak: NoSoakList, pub notes: Vec<String> }
  impl Config {
      pub fn effective_hours(&self, origin: &str) -> SoakHours;   // [[TAP]] soak_hours else self.hours
      pub fn is_no_soak(&self, origin: &str, name: &str) -> bool;
      pub fn uniform(hours: SoakHours) -> Self;                   // tests: no taps, no NO_SOAK
  }
  pub fn resolve_config(cli: Option<u32>, env: Option<&str>, file_contents: Option<&str>) -> Result<Config, Error>;
  pub fn apply_persist(action: PersistAction, path: &Path) -> Result<Option<String>, Error>;  // Some(warning) when file left alone
  ```
  `resolve_hours` and `ResolvedHours` stay as they are; `resolve_config` wraps them.

- [ ] **Step 1: Write the failing tests** (append to the `tests` module in `src/config.rs`)

```rust
    const FULL: &str = r#"
SOAK_HOURS = 48

[[TAP]]
name = "HashiCorp/tap"
soak_hours = 72

[[TAP]]
name = "cyclonedx/cyclonedx"

NO_SOAK = ["ericfitz/tap", "wget", "hashicorp/tap/terraform"]
"#;

    #[test]
    fn parse_file_reads_taps_and_no_soak() {
        let p = parse_file(FULL);
        assert_eq!(p.soak_hours.map(|h| h.get()), Some(48));
        assert_eq!(p.taps.len(), 2);
        assert_eq!(p.taps[0].name, "hashicorp/tap");
        assert_eq!(p.taps[0].soak_hours.map(|h| h.get()), Some(72));
        assert_eq!(p.taps[1].name, "cyclonedx/cyclonedx");
        assert_eq!(p.taps[1].soak_hours, None);
        assert!(p.no_soak.matches("ericfitz/tap", "brewsoak"));
        assert!(p.no_soak.matches("homebrew/core", "wget"));
        assert!(p.no_soak.matches("hashicorp/tap", "terraform"));
        assert!(!p.no_soak.matches("hashicorp/tap", "vault"));
        assert!(p.notes.is_empty(), "{:?}", p.notes);
    }

    #[test]
    fn effective_hours_prefers_tap_entry_then_global() {
        let cfg = resolve_config(None, None, Some(FULL)).unwrap();
        assert_eq!(cfg.effective_hours("hashicorp/tap").get(), 72);
        assert_eq!(cfg.effective_hours("HASHICORP/TAP").get(), 72);
        assert_eq!(cfg.effective_hours("cyclonedx/cyclonedx").get(), 48);
        assert_eq!(cfg.effective_hours("homebrew/core").get(), 48);
        assert!(cfg.is_no_soak("ericfitz/tap", "brewsoak"));
    }

    #[test]
    fn core_and_cask_can_have_their_own_hours() {
        let cfg = resolve_config(
            None,
            None,
            Some("[[TAP]]\nname = \"homebrew/core\"\nsoak_hours = 96\n"),
        )
        .unwrap();
        assert_eq!(cfg.effective_hours("homebrew/core").get(), 96);
        assert_eq!(cfg.effective_hours("homebrew/cask").get(), 24);
    }

    #[test]
    fn cli_hours_still_win_over_file_but_not_over_tap_entry() {
        let cfg = resolve_config(Some(12), None, Some(FULL)).unwrap();
        assert_eq!(cfg.hours.get(), 12);
        assert_eq!(cfg.effective_hours("homebrew/core").get(), 12);
        assert_eq!(cfg.effective_hours("hashicorp/tap").get(), 72);
    }

    #[test]
    fn invalid_tap_entries_fall_back_and_are_noted() {
        let p = parse_file(
            "[[TAP]]\nname = \"a/b\"\nsoak_hours = 0\n\n[[TAP]]\nsoak_hours = 5\n\n[[TAP]]\nname = \"bad\"\n\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 7\n",
        );
        assert_eq!(p.taps.len(), 1, "{:?}", p.taps);
        assert_eq!(p.taps[0].name, "a/b");
        assert_eq!(p.taps[0].soak_hours.map(|h| h.get()), Some(7), "last duplicate wins");
        assert_eq!(p.notes.len(), 4, "{:?}", p.notes);
        assert!(p.notes.iter().any(|n| n.contains("soak_hours") && n.contains("a/b")), "{:?}", p.notes);
        assert!(p.notes.iter().any(|n| n.contains("missing") && n.contains("name")), "{:?}", p.notes);
        assert!(p.notes.iter().any(|n| n.contains("\"bad\"")), "{:?}", p.notes);
        assert!(p.notes.iter().any(|n| n.contains("duplicate")), "{:?}", p.notes);
    }

    #[test]
    fn no_soak_not_an_array_of_strings_is_ignored_and_noted() {
        for contents in ["NO_SOAK = \"wget\"\n", "NO_SOAK = [1, 2]\n", "NO_SOAK = [\"wget\", 3]\n"] {
            let p = parse_file(contents);
            assert!(p.no_soak.is_empty(), "{contents:?}");
            assert!(p.notes.iter().any(|n| n.contains("NO_SOAK")), "{contents:?}: {:?}", p.notes);
        }
    }

    #[test]
    fn no_soak_malformed_entry_is_skipped_and_noted() {
        let p = parse_file("NO_SOAK = [\"wget\", \"a/b/c/d\", \"\"]\n");
        assert!(p.no_soak.matches("homebrew/core", "wget"));
        assert!(!p.no_soak.matches("a/b", "c"));
        assert_eq!(p.notes.len(), 2, "{:?}", p.notes);
    }

    #[test]
    fn bad_toml_is_all_defaults() {
        let p = parse_file("[[[");
        assert_eq!(p, ParsedFile::default());
    }

    #[test]
    fn persist_write_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        let warn = apply_persist(PersistAction::Write(SoakHours::new(12).unwrap()), &path).unwrap();
        assert_eq!(warn, None);
        let p = parse_file(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(p.soak_hours.map(|h| h.get()), Some(12));
        assert_eq!(p.taps.len(), 2);
        assert!(p.no_soak.matches("ericfitz/tap", "x"));
    }

    #[test]
    fn persist_delete_removes_only_soak_hours_and_file_only_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        apply_persist(PersistAction::Delete, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("SOAK_HOURS"), "{text}");
        assert!(text.contains("NO_SOAK"), "{text}");
        std::fs::write(&path, "SOAK_HOURS = 48\n").unwrap();
        apply_persist(PersistAction::Delete, &path).unwrap();
        assert!(!path.exists(), "file with no keys left must be deleted");
    }

    #[test]
    fn persist_leaves_invalid_toml_alone_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[[").unwrap();
        let warn = apply_persist(PersistAction::Write(SoakHours::new(12).unwrap()), &path).unwrap();
        assert!(warn.is_some_and(|w| w.contains("not valid TOML")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[[");
        let warn = apply_persist(PersistAction::Delete, &path).unwrap();
        assert!(warn.is_some());
        assert!(path.exists());
    }
```

Also update the existing `apply_write_and_delete` test: `apply_persist(...)` now returns `Result<Option<String>, Error>`, so `.unwrap()` still works; the first assertion changes to `assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "SOAK_HOURS = 48");` (toml's serializer formats the line the same way, but do not depend on the trailing newline).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test config::`
Expected: compile errors for `parse_file` signature, `resolve_config`, `apply_persist` return type.

- [ ] **Step 3: Write the implementation**

Replace the top of `src/config.rs` (keep `PersistAction`, `ResolvedHours`, `resolve_hours`, `read_file` as they are; replace `parse_file` and `apply_persist`):

```rust
use crate::nosoak::{self, NoSoakList};
use crate::{Error, SoakHours};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapEntry {
    /// `user/repo`, lowercased.
    pub name: String,
    pub soak_hours: Option<SoakHours>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedFile {
    pub soak_hours: Option<SoakHours>,
    pub taps: Vec<TapEntry>,
    pub no_soak: NoSoakList,
    /// Invalid entries, worded for `-v`.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Resolved `SOAK_HOURS`: CLI > env > file > 24.
    pub hours: SoakHours,
    pub persist: PersistAction,
    pub taps: Vec<TapEntry>,
    pub no_soak: NoSoakList,
    pub notes: Vec<String>,
}

impl Config {
    pub fn uniform(hours: SoakHours) -> Self {
        Self {
            hours,
            persist: PersistAction::None,
            taps: Vec::new(),
            no_soak: NoSoakList::default(),
            notes: Vec::new(),
        }
    }

    /// `NO_SOAK` is checked by the caller first; this is steps 2 and 3 of the
    /// spec's "effective soak hours".
    pub fn effective_hours(&self, origin: &str) -> SoakHours {
        let origin = origin.to_ascii_lowercase();
        self.taps
            .iter()
            .find(|t| t.name == origin)
            .and_then(|t| t.soak_hours)
            .unwrap_or(self.hours)
    }

    pub fn is_no_soak(&self, origin: &str, name: &str) -> bool {
        self.no_soak.matches(origin, name)
    }
}

pub fn resolve_config(
    cli: Option<u32>,
    env: Option<&str>,
    file_contents: Option<&str>,
) -> Result<Config, Error> {
    let resolved = resolve_hours(cli, env, file_contents)?;
    let parsed = file_contents.map(parse_file).unwrap_or_default();
    Ok(Config {
        hours: resolved.hours,
        persist: resolved.persist,
        taps: parsed.taps,
        no_soak: parsed.no_soak,
        notes: parsed.notes,
    })
}

/// Bad TOML is silently all-defaults (v1 rule). Bad keys or entries fall back
/// individually and leave a note.
pub fn parse_file(contents: &str) -> ParsedFile {
    let Ok(v) = toml::from_str::<toml::Value>(contents) else {
        return ParsedFile::default();
    };
    let mut out = ParsedFile {
        soak_hours: v
            .get("SOAK_HOURS")
            .and_then(toml::Value::as_integer)
            .and_then(|n| u32::try_from(n).ok())
            .and_then(SoakHours::new),
        ..ParsedFile::default()
    };
    parse_taps(&v, &mut out);
    parse_no_soak(&v, &mut out);
    out
}

fn parse_taps(v: &toml::Value, out: &mut ParsedFile) {
    let Some(entries) = v.get("TAP") else {
        return;
    };
    let Some(entries) = entries.as_array() else {
        out.notes.push("config: TAP is not an array of tables; ignored".into());
        return;
    };
    for entry in entries {
        let Some(name) = entry.get("name").and_then(toml::Value::as_str) else {
            out.notes.push("config: [[TAP]] entry is missing name; skipped".into());
            continue;
        };
        let lower = name.trim().to_ascii_lowercase();
        if lower.split('/').count() != 2 || lower.split('/').any(str::is_empty) {
            out.notes.push(format!("config: [[TAP]] name {name:?} is not user/repo; skipped"));
            continue;
        }
        let soak_hours = match entry.get("soak_hours") {
            None => None,
            Some(h) => match h
                .as_integer()
                .and_then(|n| u32::try_from(n).ok())
                .and_then(SoakHours::new)
            {
                Some(h) => Some(h),
                None => {
                    out.notes.push(format!(
                        "config: [[TAP]] {lower} soak_hours {h} is not an integer >= 1; using SOAK_HOURS"
                    ));
                    None
                }
            },
        };
        if let Some(pos) = out.taps.iter().position(|t| t.name == lower) {
            out.notes.push(format!("config: duplicate [[TAP]] {lower}; last entry wins"));
            out.taps.remove(pos);
        }
        out.taps.push(TapEntry { name: lower, soak_hours });
    }
}

fn parse_no_soak(v: &toml::Value, out: &mut ParsedFile) {
    let Some(raw) = v.get("NO_SOAK") else {
        return;
    };
    let strings: Option<Vec<&str>> = raw
        .as_array()
        .and_then(|a| a.iter().map(toml::Value::as_str).collect());
    let Some(strings) = strings else {
        out.notes.push("config: NO_SOAK is not an array of strings; ignored".into());
        return;
    };
    let mut entries = Vec::new();
    for s in strings {
        match nosoak::parse_entry(s) {
            Ok(e) => entries.push(e),
            Err(reason) => out.notes.push(format!("config: {reason}")),
        }
    }
    out.no_soak = NoSoakList::new(entries);
}

pub fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Key-level edit of `SOAK_HOURS`. Returns a warning instead of touching a
/// file that is not valid TOML. Comments are not preserved.
pub fn apply_persist(action: PersistAction, path: &Path) -> Result<Option<String>, Error> {
    if action == PersistAction::None {
        return Ok(None);
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut table = match existing.as_deref() {
        None => toml::Table::new(),
        Some(s) => match toml::from_str::<toml::Table>(s) {
            Ok(t) => t,
            Err(_) => {
                return Ok(Some(format!(
                    "{} is not valid TOML; --soak-hours was not persisted (it still applies to this run)",
                    path.display()
                )));
            }
        },
    };
    match action {
        PersistAction::Write(hours) => {
            table.insert(
                "SOAK_HOURS".into(),
                toml::Value::Integer(i64::from(hours.get())),
            );
        }
        PersistAction::Delete => {
            table.remove("SOAK_HOURS");
        }
        PersistAction::None => unreachable!(),
    }
    if table.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = toml::to_string(&table).map_err(|e| Error::Other(format!("config: {e}")))?;
    std::fs::write(path, body)?;
    Ok(None)
}
```

In `src/lib.rs` `dispatch`, replace the config lines:

```rust
    let env = world.env_soak();
    let file = config::read_file(&world.config_path());
    let cfg = config::resolve_config(inv.soak_hours, env.as_deref(), file.as_deref())?;
    if inv.command.is_soaked() {
        if let Some(warning) = config::apply_persist(cfg.persist, &world.config_path())? {
            eprintln!("brewsoak: warning: {warning}");
        }
        if cmd::is_verbose(&inv.brew_args) {
            for note in &cfg.notes {
                println!("{note}");
            }
        }
    }
```

and replace every later `resolved.hours` with `cfg.hours`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test config:: && cargo test lib::tests::soak_hours`
Expected: all pass, including `soak_hours_persists_on_soaked_command` (the file still reads `SOAK_HOURS = 48` on its one line; if `toml::to_string` emits exactly `SOAK_HOURS = 48\n` the existing `assert_eq!` holds; otherwise loosen that test to `text.trim() == "SOAK_HOURS = 48"`).

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/config.rs src/lib.rs
git commit -m "feat: read [[TAP]] and NO_SOAK from config; persist SOAK_HOURS as a key-level edit

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 3: Origin records (`origins.toml`) and receipt tap on `InstalledPkg`

**Files:**
- Create: `src/origin.rs`
- Modify: `src/lib.rs:1-16` (add `pub mod origin;`)
- Modify: `src/brew.rs:12-18` (`InstalledPkg.tap`), `src/brew.rs:502-542` (`parse_installed_json` records the tap; still drops third-party for now), brew.rs tests `pkg()` helper and `parse_installed_json_*` expectations
- Modify: `src/cmd.rs` tests `formula_pkg` (add `tap: None`)
- Test: `src/origin.rs` tests, `src/brew.rs` tests

**Interfaces:**
- Consumes: `resolve::PkgKind`.
- Produces:
  ```rust
  pub const CORE: &str = "homebrew/core";
  pub const CASK: &str = "homebrew/cask";
  pub const STAGING_TAP: &str = "brewsoakr/soaked";
  pub fn default_origin(kind: PkgKind) -> &'static str;
  pub fn is_core_or_cask(origin: &str) -> bool;
  pub fn split_tap(origin: &str) -> Option<(&str, &str)>;      // ("hashicorp", "tap")
  pub fn record_key(kind: PkgKind, name: &str) -> String;      // "formula:terraform"
  #[derive(Debug, Default, Clone, PartialEq, Eq)]
  pub struct OriginRecords { map: BTreeMap<String, String> }
  impl OriginRecords {
      pub fn load(cache: &Path) -> Self;                        // missing/unreadable/bad TOML => empty
      pub fn save(&self, cache: &Path) -> Result<(), Error>;
      pub fn get(&self, kind: PkgKind, name: &str) -> Option<&str>;
      pub fn set(&mut self, kind: PkgKind, name: &str, origin: &str);
      pub fn remove(&mut self, kind: PkgKind, name: &str);
  }
  /// Spec "Package origin" order: receipt tap (non-empty, not staging) > record > core/cask.
  pub fn resolve_origin(receipt_tap: Option<&str>, records: &OriginRecords, kind: PkgKind, name: &str) -> String;
  ```
  `brew::InstalledPkg` gains `pub tap: Option<String>` (`None` for JSON `null`, `""`, or the staging tap).

- [ ] **Step 1: Write the failing tests**

`src/origin.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tap_user_repo() {
        assert_eq!(split_tap("hashicorp/tap"), Some(("hashicorp", "tap")));
        assert_eq!(split_tap("wget"), None);
        assert_eq!(split_tap("a/b/c"), None);
    }

    #[test]
    fn records_round_trip_through_origins_toml() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = OriginRecords::default();
        r.set(PkgKind::Formula, "terraform", "hashicorp/tap");
        r.set(PkgKind::Cask, "foo", "acme/casks");
        r.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join("origins.toml")).unwrap();
        assert!(text.contains("\"formula:terraform\" = \"hashicorp/tap\""), "{text}");
        let loaded = OriginRecords::load(dir.path());
        assert_eq!(loaded.get(PkgKind::Formula, "terraform"), Some("hashicorp/tap"));
        assert_eq!(loaded.get(PkgKind::Cask, "foo"), Some("acme/casks"));
        assert_eq!(loaded.get(PkgKind::Formula, "foo"), None, "kind is part of the key");
    }

    #[test]
    fn remove_drops_the_entry() {
        let mut r = OriginRecords::default();
        r.set(PkgKind::Formula, "terraform", "hashicorp/tap");
        r.remove(PkgKind::Formula, "terraform");
        assert_eq!(r.get(PkgKind::Formula, "terraform"), None);
    }

    #[test]
    fn load_missing_or_corrupt_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(OriginRecords::load(dir.path()), OriginRecords::default());
        std::fs::write(dir.path().join("origins.toml"), "[[[").unwrap();
        assert_eq!(OriginRecords::load(dir.path()), OriginRecords::default());
    }

    #[test]
    fn resolve_origin_prefers_receipt_then_record_then_default() {
        let mut r = OriginRecords::default();
        r.set(PkgKind::Formula, "terraform", "hashicorp/tap");
        assert_eq!(resolve_origin(Some("Acme/Tools"), &r, PkgKind::Formula, "terraform"), "acme/tools");
        assert_eq!(resolve_origin(None, &r, PkgKind::Formula, "terraform"), "hashicorp/tap");
        assert_eq!(resolve_origin(Some(""), &r, PkgKind::Formula, "terraform"), "hashicorp/tap");
        assert_eq!(resolve_origin(Some(STAGING_TAP), &r, PkgKind::Formula, "terraform"), "hashicorp/tap");
        assert_eq!(resolve_origin(None, &r, PkgKind::Formula, "wget"), CORE);
        assert_eq!(resolve_origin(None, &r, PkgKind::Cask, "firefox"), CASK);
    }
}
```

`src/brew.rs` tests: in `parse_installed_json_keeps_core_and_cask_drops_third_party`, change the expected vector so `wget` has `tap: Some("homebrew/core".into())` and `firefox` has `tap: Some("homebrew/cask".into())` (update the `pkg()` helper to take a `tap: Option<&str>` argument, and every other `pkg(...)` call to pass `None` or the tap the fixture declares). Add:

```rust
    #[test]
    fn parse_installed_json_maps_null_empty_and_staging_tap_to_none() {
        let json = r#"{
          "formulae": [
            {"name": "a", "tap": null, "installed": [{"version": "1"}]},
            {"name": "b", "tap": "", "installed": [{"version": "1"}]},
            {"name": "c", "tap": "brewsoakr/soaked", "installed": [{"version": "1"}]}
          ],
          "casks": []
        }"#;
        let got = parse_installed_json(json, |_, _, _| Some("rb".into())).expect("parse");
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|p| p.tap.is_none()), "{got:?}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test origin:: ; cargo test brew::tests::parse_installed_json`
Expected: compile errors (`origin` module missing, `InstalledPkg` has no field `tap`).

- [ ] **Step 3: Write the implementation**

`src/origin.rs`:

```rust
//! Where a package comes from (`user/repo`), and the `origins.toml` record
//! brewsoak keeps because a staged install leaves the receipt tap empty.

use crate::Error;
use crate::resolve::PkgKind;
use std::collections::BTreeMap;
use std::path::Path;

pub const CORE: &str = "homebrew/core";
pub const CASK: &str = "homebrew/cask";
/// brewsoak's own local tap. A receipt that names it says nothing about origin.
pub const STAGING_TAP: &str = "brewsoakr/soaked";

pub fn default_origin(kind: PkgKind) -> &'static str {
    match kind {
        PkgKind::Formula => CORE,
        PkgKind::Cask => CASK,
    }
}

pub fn is_core_or_cask(origin: &str) -> bool {
    origin.eq_ignore_ascii_case(CORE) || origin.eq_ignore_ascii_case(CASK)
}

pub fn split_tap(origin: &str) -> Option<(&str, &str)> {
    let (user, repo) = origin.split_once('/')?;
    if user.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((user, repo))
}

pub fn record_key(kind: PkgKind, name: &str) -> String {
    let kind = match kind {
        PkgKind::Formula => "formula",
        PkgKind::Cask => "cask",
    };
    format!("{kind}:{}", name.to_ascii_lowercase())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OriginRecords {
    map: BTreeMap<String, String>,
}

impl OriginRecords {
    fn path(cache: &Path) -> std::path::PathBuf {
        cache.join("origins.toml")
    }

    /// A missing, unreadable, or corrupt file is an empty record set: the
    /// package then falls back to core/cask (spec "Package origin", step 3).
    pub fn load(cache: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(Self::path(cache)) else {
            return Self::default();
        };
        let Ok(table) = toml::from_str::<toml::Table>(&raw) else {
            return Self::default();
        };
        let map = table
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_ascii_lowercase())))
            .collect();
        Self { map }
    }

    pub fn save(&self, cache: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(cache)?;
        let mut body = String::new();
        for (k, v) in &self.map {
            body.push_str(&format!("{k:?} = {v:?}\n"));
        }
        std::fs::write(Self::path(cache), body)?;
        Ok(())
    }

    pub fn get(&self, kind: PkgKind, name: &str) -> Option<&str> {
        self.map.get(&record_key(kind, name)).map(String::as_str)
    }

    pub fn set(&mut self, kind: PkgKind, name: &str, origin: &str) {
        self.map
            .insert(record_key(kind, name), origin.to_ascii_lowercase());
    }

    pub fn remove(&mut self, kind: PkgKind, name: &str) {
        self.map.remove(&record_key(kind, name));
    }
}

/// Spec order: the receipt tap when non-empty and not the staging tap; else
/// brewsoak's record; else `homebrew/core` / `homebrew/cask`.
pub fn resolve_origin(
    receipt_tap: Option<&str>,
    records: &OriginRecords,
    kind: PkgKind,
    name: &str,
) -> String {
    if let Some(tap) = receipt_tap
        && !tap.is_empty()
        && !tap.eq_ignore_ascii_case(STAGING_TAP)
    {
        return tap.to_ascii_lowercase();
    }
    records
        .get(kind, name)
        .map(str::to_string)
        .unwrap_or_else(|| default_origin(kind).to_string())
}
```

`src/brew.rs`: add `pub tap: Option<String>,` to `InstalledPkg` (document: "Receipt tap from `brew info`; `None` for null/empty/staging"). In `parse_installed_json`, for both loops:

```rust
        let tap = json_string_value(obj, "tap");
        if !keep_tap(tap.as_deref()) {
            continue;
        }
        let tap = tap.filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case(crate::origin::STAGING_TAP));
```

and set `tap: tap.clone()` (formula loop) / `tap` in the `InstalledPkg` literal. Also extend `keep_tap` to accept the staging tap: `None | Some("") | Some("homebrew/core") | Some("homebrew/cask") | Some("brewsoakr/soaked")`. Task 9 removes `keep_tap`.

`src/cmd.rs` test helper `formula_pkg`: add `tap: None,`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test origin:: ; cargo test brew::`
Expected: all pass.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/origin.rs src/lib.rs src/brew.rs src/cmd.rs
git commit -m "feat: origin records and receipt tap on installed packages

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 4: GitStore additions for tap clones

**Files:**
- Modify: `src/git.rs` (trait, `ProcessGit`, `InMemoryGit`, tests)
- Modify: `src/lib.rs:51-61` (`RealWorld::new` uses `git::ProcessGit::default()`)
- Modify: `src/git.rs` test `process_git_*` helpers (reuse `git_ok`, `git_commit_at`)
- Test: `src/git.rs` tests

**Interfaces:**
- Consumes: existing `GitStore`, `Error::Git`, `git_date_arg`.
- Produces (all with default impls that return `Error::Git { detail: "this git backend cannot ..." }` so `FailShowGit` keeps compiling; `InMemoryGit` and `ProcessGit` override all):
  ```rust
  pub trait GitStore {
      // existing methods unchanged, plus:
      /// `git remote add <name> <url>`, or `set-url` when it exists.
      fn set_remote(&self, dir: &Path, name: &str, url: &str) -> Result<(), Error>;
      /// Full commit history of the remote's HEAD into `ref_name` (force), blobless
      /// (`--filter=blob:none`); retried without the filter if the server rejects it.
      fn fetch_history(&self, dir: &Path, remote_name: &str, ref_name: &str) -> Result<(), Error>;
      /// `(sha, committer_unix)` of the newest commit reachable from `rev` with
      /// committer time <= `until_unix`; `None` when the history is younger.
      fn rev_list_before(&self, dir: &Path, rev: &str, until_unix: i64) -> Result<Option<(String, i64)>, Error>;
      /// Force-set a pin ref to `sha`.
      fn update_ref(&self, dir: &Path, ref_name: &str, sha: &str) -> Result<(), Error>;
      /// `git ls-tree -r --name-only <sha>`; cached per (dir, sha) in ProcessGit.
      fn ls_tree(&self, dir: &Path, sha: &str) -> Result<Vec<String>, Error>;
  }
  pub struct ProcessGit { trees: RefCell<HashMap<(PathBuf, String), Rc<Vec<String>>>> }  // impl Default
  pub fn filter_rejected(stderr: &str) -> bool;   // pure predicate for the fallback
  ```
  `InMemoryGit` additions:
  ```rust
  pub fn insert_commits(&self, remote_url: &str, newest_first: &[(&str, i64)]);  // head = first
  pub fn insert_tree(&self, sha: &str, paths: &[&str]);
  pub fn fail_remote(&self, remote_url: &str);     // fetch_history for it => Error::Git
  pub fn remote_url(&self, dir: &Path) -> Option<String>;
  ```
  `InMemoryGit.refs` becomes keyed by `(dir string, ref)`. `show` stays keyed by `(sha, path)`, so fixtures use distinct SHAs per tap.

- [ ] **Step 1: Write the failing tests** (append to `src/git.rs` tests)

```rust
    #[test]
    fn in_memory_fetch_history_pins_head_and_rev_list_before_finds_cutoff() {
        let git = InMemoryGit::new();
        let dir = Path::new("/cache/taps/hashicorp/tap.git");
        git.insert_commits(
            "https://github.com/hashicorp/homebrew-tap",
            &[("h3", 1_700_000_000), ("h2", 1_699_990_000), ("h1", 1_699_900_000)],
        );
        git.set_remote(dir, "origin", "https://github.com/hashicorp/homebrew-tap").unwrap();
        git.fetch_history(dir, "origin", REF_HEAD).unwrap();
        assert_eq!(git.rev_parse(dir, REF_HEAD).unwrap(), Some("h3".into()));
        assert_eq!(
            git.rev_list_before(dir, REF_HEAD, 1_699_995_000).unwrap(),
            Some(("h2".into(), 1_699_990_000))
        );
        assert_eq!(git.rev_list_before(dir, REF_HEAD, 1_000).unwrap(), None);
        git.update_ref(dir, REF_CUTOFF, "h2").unwrap();
        assert_eq!(git.rev_parse(dir, REF_CUTOFF).unwrap(), Some("h2".into()));
    }

    #[test]
    fn in_memory_refs_are_per_dir() {
        let git = InMemoryGit::new();
        let a = Path::new("/cache/taps/a/tap.git");
        let b = Path::new("/cache/taps/b/tap.git");
        git.update_ref(a, REF_HEAD, "aaa").unwrap();
        git.update_ref(b, REF_HEAD, "bbb").unwrap();
        assert_eq!(git.rev_parse(a, REF_HEAD).unwrap(), Some("aaa".into()));
        assert_eq!(git.rev_parse(b, REF_HEAD).unwrap(), Some("bbb".into()));
    }

    #[test]
    fn in_memory_failing_remote_is_error_git() {
        let git = InMemoryGit::new();
        let dir = Path::new("/cache/taps/x/y.git");
        git.fail_remote("https://example.com/x/homebrew-y");
        git.set_remote(dir, "origin", "https://example.com/x/homebrew-y").unwrap();
        let err = git.fetch_history(dir, "origin", REF_HEAD).unwrap_err();
        assert!(matches!(err, Error::Git { .. }), "{err}");
        assert!(err.to_string().contains("fetching history"), "{err}");
    }

    #[test]
    fn in_memory_ls_tree_returns_inserted_paths() {
        let git = InMemoryGit::new();
        git.insert_tree("h2", &["Formula/terraform.rb", "README.md"]);
        assert_eq!(
            git.ls_tree(unused_dir(), "h2").unwrap(),
            vec!["Formula/terraform.rb".to_string(), "README.md".to_string()]
        );
        assert!(git.ls_tree(unused_dir(), "nosuch").unwrap().is_empty());
    }

    #[test]
    fn filter_rejected_recognizes_server_refusals() {
        assert!(filter_rejected("fatal: filtering not recognized by server, ignoring"));
        assert!(filter_rejected("warning: filtering not recognized by server"));
        assert!(filter_rejected("fatal: invalid filter-spec 'blob:none'"));
        assert!(filter_rejected("fatal: the remote end hung up unexpectedly\nerror: server does not support filter"));
        assert!(!filter_rejected("fatal: could not read from remote repository"));
    }

    #[test]
    fn process_git_fetch_history_then_cutoff_pin_and_ls_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bare = tmp.path().join("tap.git");
        std::fs::create_dir(&src).unwrap();
        git_ok(&src, &["init", "-b", "main"]);
        git_ok(&src, &["config", "user.email", "test@example.com"]);
        git_ok(&src, &["config", "user.name", "Test"]);
        std::fs::create_dir(src.join("Formula")).unwrap();
        std::fs::write(src.join("Formula/foo.rb"), "one\n").unwrap();
        git_ok(&src, &["add", "."]);
        git_commit_at(&src, "one", COMMIT_UNIX);
        let older = git_ok(&src, &["rev-parse", "HEAD"]);
        std::fs::write(src.join("Formula/foo.rb"), "two\n").unwrap();
        git_ok(&src, &["add", "."]);
        git_commit_at(&src, "two", COMMIT_UNIX + 3600);
        let newer = git_ok(&src, &["rev-parse", "HEAD"]);

        let git = ProcessGit::default();
        git.init_bare(&bare).expect("init");
        git.set_remote(&bare, "origin", src.to_str().unwrap()).expect("remote add");
        git.set_remote(&bare, "origin", src.to_str().unwrap()).expect("remote set-url is idempotent");
        git.fetch_history(&bare, "origin", REF_HEAD).expect("fetch history");
        assert_eq!(git.rev_parse(&bare, REF_HEAD).unwrap(), Some(newer.clone()));
        let (sha, when) = git
            .rev_list_before(&bare, REF_HEAD, COMMIT_UNIX + 60)
            .unwrap()
            .expect("older commit is before the cutoff");
        assert_eq!(sha, older);
        assert_eq!(when, COMMIT_UNIX);
        assert_eq!(git.rev_list_before(&bare, REF_HEAD, COMMIT_UNIX - 1).unwrap(), None);
        git.update_ref(&bare, REF_CUTOFF, &older).expect("pin cutoff");
        assert_eq!(git.rev_parse(&bare, REF_CUTOFF).unwrap(), Some(older.clone()));
        assert_eq!(git.ls_tree(&bare, &older).unwrap(), vec!["Formula/foo.rb".to_string()]);
        assert_eq!(
            git.show(&bare, &older, "Formula/foo.rb").unwrap().as_deref(),
            Some(b"one\n".as_slice())
        );
        // Force-pin: moving HEAD back to the older commit must not be rejected.
        git_ok(&src, &["reset", "--hard", &older]);
        git.fetch_history(&bare, "origin", REF_HEAD).expect("non-fast-forward head");
        assert_eq!(git.rev_parse(&bare, REF_HEAD).unwrap(), Some(older));
    }

    #[test]
    fn process_git_fetch_history_error_names_the_action() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("tap.git");
        let git = ProcessGit::default();
        git.init_bare(&bare).unwrap();
        git.set_remote(&bare, "origin", "/no/such/brewsoak-tap").unwrap();
        let err = git.fetch_history(&bare, "origin", REF_HEAD).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("fetching history"), "{text}");
        assert!(text.contains("git failed"), "{text}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test git::`
Expected: compile errors (`set_remote`, `fetch_history`, `insert_commits`, `ProcessGit::default`...).

- [ ] **Step 3: Write the implementation**

Trait additions (after `log_sha_before`):

```rust
    fn set_remote(&self, _dir: &Path, name: &str, url: &str) -> Result<(), Error> {
        Err(Error::Git {
            action: format!("registering tap remote {name} ({url})"),
            detail: "this git backend cannot add remotes".into(),
        })
    }
    fn fetch_history(&self, _dir: &Path, remote_name: &str, _ref_name: &str) -> Result<(), Error> {
        Err(Error::Git {
            action: format!("fetching history from {remote_name}"),
            detail: "this git backend cannot fetch history".into(),
        })
    }
    fn rev_list_before(&self, _dir: &Path, _rev: &str, _until_unix: i64) -> Result<Option<(String, i64)>, Error> {
        Err(Error::Git {
            action: "looking up the tap commit at or before the soak cutoff".into(),
            detail: "this git backend cannot walk history".into(),
        })
    }
    fn update_ref(&self, _dir: &Path, ref_name: &str, _sha: &str) -> Result<(), Error> {
        Err(Error::Git {
            action: format!("pinning {ref_name}"),
            detail: "this git backend cannot update refs".into(),
        })
    }
    fn ls_tree(&self, _dir: &Path, _sha: &str) -> Result<Vec<String>, Error> {
        Err(Error::Git {
            action: "listing a tap commit's files".into(),
            detail: "this git backend cannot list trees".into(),
        })
    }
```

`ProcessGit` becomes a struct with a tree cache:

```rust
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Default)]
pub struct ProcessGit {
    /// `ls-tree` output per (clone dir, commit). A commit's tree never changes.
    trees: RefCell<HashMap<(PathBuf, String), Rc<Vec<String>>>>,
}

/// Servers that do not support partial clone answer the filter with one of
/// these; the caller retries without it.
pub fn filter_rejected(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("filter") && (s.contains("not recognized") || s.contains("not support") || s.contains("invalid filter-spec"))
}
```

(Move the existing `#[cfg(test)] use std::cell::RefCell; use std::collections::HashMap;` lines to plain `use` at the top since `ProcessGit` now needs them.) The `impl GitStore for ProcessGit` additions:

```rust
    fn set_remote(&self, dir: &Path, name: &str, url: &str) -> Result<(), Error> {
        let action = format!("registering tap remote {name} ({url})");
        let dir = dir.to_string_lossy();
        let add = run_git(&action, &["--git-dir", dir.as_ref(), "remote", "add", name, url])?;
        if add.status.success() {
            return Ok(());
        }
        if String::from_utf8_lossy(&add.stderr).contains("already exists") {
            let set = run_git(&action, &["--git-dir", dir.as_ref(), "remote", "set-url", name, url])?;
            if set.status.success() {
                return Ok(());
            }
            return Err(git_fail(&action, &set));
        }
        Err(git_fail(&action, &add))
    }

    fn fetch_history(&self, dir: &Path, remote_name: &str, ref_name: &str) -> Result<(), Error> {
        let action = format!("fetching history from {remote_name} to find the tap's soak cutoff");
        let dir = dir.to_string_lossy();
        let spec = format!("+HEAD:{ref_name}");
        let with_filter = run_git(
            &action,
            &["--git-dir", dir.as_ref(), "fetch", "--force", "--filter=blob:none", remote_name, &spec],
        )?;
        if with_filter.status.success() {
            return Ok(());
        }
        if !filter_rejected(&String::from_utf8_lossy(&with_filter.stderr)) {
            return Err(git_fail(&action, &with_filter));
        }
        let plain = run_git(
            &action,
            &["--git-dir", dir.as_ref(), "fetch", "--force", remote_name, &spec],
        )?;
        if plain.status.success() {
            Ok(())
        } else {
            Err(git_fail(&action, &plain))
        }
    }

    fn rev_list_before(&self, dir: &Path, rev: &str, until_unix: i64) -> Result<Option<(String, i64)>, Error> {
        let action = "looking up the tap commit at or before the soak cutoff";
        let dir = dir.to_string_lossy();
        let before = format!("--before={}", git_date_arg(until_unix));
        // `log -1 --before` is the `rev-list -1 --before` walk with formatting
        // and no `commit <sha>` header line to strip.
        let output = run_git(
            action,
            &["--git-dir", dir.as_ref(), "log", "-1", &before, "--format=%H %ct", rev],
        )?;
        if !output.status.success() {
            return if missing_object(&output) { Ok(None) } else { Err(git_fail(action, &output)) };
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.split_whitespace();
        match (parts.next(), parts.next().and_then(|t| t.parse::<i64>().ok())) {
            (Some(sha), Some(when)) => Ok(Some((sha.to_string(), when))),
            _ => Ok(None),
        }
    }

    fn update_ref(&self, dir: &Path, ref_name: &str, sha: &str) -> Result<(), Error> {
        let action = format!("pinning {ref_name} to {sha}");
        let dir = dir.to_string_lossy();
        let output = run_git(&action, &["--git-dir", dir.as_ref(), "update-ref", ref_name, sha])?;
        if output.status.success() { Ok(()) } else { Err(git_fail(&action, &output)) }
    }

    fn ls_tree(&self, dir: &Path, sha: &str) -> Result<Vec<String>, Error> {
        let key = (dir.to_path_buf(), sha.to_string());
        if let Some(cached) = self.trees.borrow().get(&key) {
            return Ok(cached.as_ref().clone());
        }
        let action = format!("listing the files of tap commit {sha}");
        let dir_s = dir.to_string_lossy();
        let output = run_git(&action, &["--git-dir", dir_s.as_ref(), "ls-tree", "-r", "--name-only", sha])?;
        if !output.status.success() {
            return Err(git_fail(&action, &output));
        }
        let paths: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        self.trees.borrow_mut().insert(key, Rc::new(paths.clone()));
        Ok(paths)
    }
```

`InMemoryGit`:

```rust
#[cfg(test)]
#[derive(Default)]
pub struct InMemoryGit {
    blobs: RefCell<HashMap<(String, String), Vec<u8>>>,
    /// (dir, ref) -> sha. Tap clones each pin the same ref names.
    refs: RefCell<HashMap<(String, String), String>>,
    fetched: RefCell<Vec<(String, String)>>,
    /// remote url -> commits newest first (sha, committer unix)
    commits: RefCell<HashMap<String, Vec<(String, i64)>>>,
    trees: RefCell<HashMap<String, Vec<String>>>,
    remotes: RefCell<HashMap<(String, String), String>>, // (dir, name) -> url
    failing: RefCell<std::collections::HashSet<String>>,
}

#[cfg(test)]
impl InMemoryGit {
    fn key(dir: &Path, name: &str) -> (String, String) {
        (dir.to_string_lossy().into_owned(), name.to_string())
    }
    pub fn insert_commits(&self, remote_url: &str, newest_first: &[(&str, i64)]) {
        self.commits.borrow_mut().insert(
            remote_url.to_string(),
            newest_first.iter().map(|(s, t)| ((*s).to_string(), *t)).collect(),
        );
    }
    pub fn insert_tree(&self, sha: &str, paths: &[&str]) {
        self.trees.borrow_mut().insert(sha.to_string(), paths.iter().map(|p| (*p).to_string()).collect());
    }
    pub fn fail_remote(&self, remote_url: &str) {
        self.failing.borrow_mut().insert(remote_url.to_string());
    }
    pub fn remote_url(&self, dir: &Path) -> Option<String> {
        self.remotes.borrow().get(&Self::key(dir, "origin")).cloned()
    }
}
```

In `impl GitStore for InMemoryGit`: `fetch_depth1` inserts into `refs` with `Self::key(dir, ref_name)`; `rev_parse` looks up `Self::key(dir, rev)`; add:

```rust
    fn set_remote(&self, dir: &Path, name: &str, url: &str) -> Result<(), Error> {
        self.remotes.borrow_mut().insert(Self::key(dir, name), url.to_string());
        Ok(())
    }

    fn fetch_history(&self, dir: &Path, remote_name: &str, ref_name: &str) -> Result<(), Error> {
        let url = self.remotes.borrow().get(&Self::key(dir, remote_name)).cloned();
        let action = format!("fetching history from {remote_name} to find the tap's soak cutoff");
        let Some(url) = url else {
            return Err(Error::Git { action, detail: "no such remote".into() });
        };
        if self.failing.borrow().contains(&url) {
            return Err(Error::Git { action, detail: format!("fatal: could not read from remote repository {url}") });
        }
        let head = self.commits.borrow().get(&url).and_then(|c| c.first().map(|(s, _)| s.clone()));
        let Some(head) = head else {
            return Err(Error::Git { action, detail: "remote has no commits".into() });
        };
        self.fetched.borrow_mut().push((head.clone(), ref_name.to_string()));
        self.refs.borrow_mut().insert(Self::key(dir, ref_name), head);
        Ok(())
    }

    fn rev_list_before(&self, dir: &Path, rev: &str, until_unix: i64) -> Result<Option<(String, i64)>, Error> {
        let start = self.refs.borrow().get(&Self::key(dir, rev)).cloned().unwrap_or_else(|| rev.to_string());
        let url = self.remotes.borrow().get(&Self::key(dir, "origin")).cloned();
        let commits = url.and_then(|u| self.commits.borrow().get(&u).cloned()).unwrap_or_default();
        let from = commits.iter().position(|(s, _)| *s == start).unwrap_or(0);
        Ok(commits[from..].iter().find(|(_, t)| *t <= until_unix).cloned())
    }

    fn update_ref(&self, dir: &Path, ref_name: &str, sha: &str) -> Result<(), Error> {
        self.refs.borrow_mut().insert(Self::key(dir, ref_name), sha.to_string());
        Ok(())
    }

    fn ls_tree(&self, _dir: &Path, sha: &str) -> Result<Vec<String>, Error> {
        Ok(self.trees.borrow().get(sha).cloned().unwrap_or_default())
    }
```

`src/lib.rs`: `git: git::ProcessGit::default(),` in `RealWorld::new`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test git::`
Expected: all pass (the existing `fetch_records_ref_and_rev_parse` still passes because it uses the same `unused_dir()` for fetch and rev-parse).

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/git.rs src/lib.rs
git commit -m "feat: blobless history fetch, rev-list cutoff, ls-tree, and ref pins in GitStore

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 5: Tap classification, `brew tap-info`, name resolution, staging paths

**Files:**
- Create: `src/taps.rs`
- Modify: `src/lib.rs:1-16` (add `pub mod taps;`)
- Modify: `src/brew.rs:30-44` (`Brew::tap_info`), `src/brew.rs:96-146` (`MockBrew.taps`), `src/brew.rs:148-216` (`ProcessBrew::tap_info`), `src/brew.rs:571,664` (`json_objects_in_array`, `json_string_value`, `scan_array_objects`, `find_json_key` become `pub(crate)`)
- Test: `src/taps.rs` tests, `src/brew.rs` test `mock_tap_info_returns_the_vec`

**Interfaces:**
- Consumes: `origin::{CORE, CASK, STAGING_TAP, split_tap}`, `resolve::PkgKind`, brew JSON helpers.
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct TapInfo { pub name: String /* lowercase */, pub remote: Option<String> }
  pub fn parse_tap_info_json(json: &str) -> Result<Vec<TapInfo>, Error>;   // top-level JSON array
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum TapClass { Core, Cask, Staging, Unsoakable, Soakable }
  pub fn classify(tap: &TapInfo) -> TapClass;
  pub fn clone_dir(cache: &Path, tap: &str) -> PathBuf;              // <cache>/taps/<user>/<repo>.git
  pub fn staging_root(tap_root: &Path, origin: &str) -> PathBuf;     // core/cask => tap_root; else tap_root/taps/<user>/<repo>
  pub fn resolve_path(tree: &[String], kind: PkgKind, name: &str) -> Option<String>;
  pub fn staged_load_failure(brew_output: &str) -> bool;
  ```
  `Brew` trait: `fn tap_info(&self) -> Result<Vec<TapInfo>, Error>;` `MockBrew` gains `pub taps: Vec<TapInfo>` (default empty).

- [ ] **Step 1: Write the failing tests**

`src/taps.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const TAP_INFO: &str = r#"[
      {"name": "homebrew/core", "remote": null, "path": "/x", "private": false, "formula_names": [], "cask_tokens": []},
      {"name": "homebrew/cask", "remote": null},
      {"name": "brewsoakr/soaked", "remote": null, "path": "/opt/homebrew/Library/Taps/brewsoakr/homebrew-soaked"},
      {"name": "HashiCorp/tap", "remote": "https://github.com/hashicorp/homebrew-tap", "private": false},
      {"name": "acme/private", "remote": "git@github.com:acme/homebrew-private.git"},
      {"name": "local/tap", "remote": null}
    ]"#;

    #[test]
    fn parse_tap_info_reads_name_and_remote() {
        let taps = parse_tap_info_json(TAP_INFO).unwrap();
        assert_eq!(taps.len(), 6);
        assert_eq!(taps[3].name, "hashicorp/tap", "names are lowercased");
        assert_eq!(taps[3].remote.as_deref(), Some("https://github.com/hashicorp/homebrew-tap"));
        assert_eq!(taps[0].remote, None);
    }

    #[test]
    fn classify_follows_the_spec_table() {
        let taps = parse_tap_info_json(TAP_INFO).unwrap();
        let classes: Vec<TapClass> = taps.iter().map(classify).collect();
        assert_eq!(
            classes,
            vec![
                TapClass::Core,
                TapClass::Cask,
                TapClass::Staging,
                TapClass::Soakable,
                TapClass::Unsoakable,
                TapClass::Unsoakable,
            ]
        );
    }

    #[test]
    fn http_remote_is_unsoakable() {
        let tap = TapInfo { name: "a/b".into(), remote: Some("http://example.com/a/homebrew-b".into()) };
        assert_eq!(classify(&tap), TapClass::Unsoakable);
    }

    #[test]
    fn clone_and_staging_dirs_are_per_tap() {
        assert_eq!(
            clone_dir(Path::new("/cache"), "hashicorp/tap"),
            PathBuf::from("/cache/taps/hashicorp/tap.git")
        );
        assert_eq!(
            staging_root(Path::new("/cache/staging"), "hashicorp/tap"),
            PathBuf::from("/cache/staging/taps/hashicorp/tap")
        );
        assert_eq!(staging_root(Path::new("/cache/staging"), "homebrew/core"), PathBuf::from("/cache/staging"));
        assert_eq!(staging_root(Path::new("/cache/staging"), "homebrew/cask"), PathBuf::from("/cache/staging"));
    }

    fn tree(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn resolve_path_formula_search_order() {
        let t = tree(&[
            "Formula/terraform.rb",
            "Formula/t/vault.rb",
            "HomebrewFormula/consul.rb",
            "nomad.rb",
            "README.md",
        ]);
        assert_eq!(resolve_path(&t, PkgKind::Formula, "terraform").as_deref(), Some("Formula/terraform.rb"));
        assert_eq!(resolve_path(&t, PkgKind::Formula, "vault").as_deref(), Some("Formula/t/vault.rb"));
        assert_eq!(resolve_path(&t, PkgKind::Formula, "consul").as_deref(), Some("HomebrewFormula/consul.rb"));
        assert_eq!(resolve_path(&t, PkgKind::Formula, "nomad").as_deref(), Some("nomad.rb"));
        assert_eq!(resolve_path(&t, PkgKind::Formula, "readme"), None);
        assert_eq!(resolve_path(&t, PkgKind::Formula, "missing"), None);
    }

    #[test]
    fn resolve_path_prefers_flat_formula_over_sharded() {
        let t = tree(&["Formula/x/foo.rb", "Formula/foo.rb"]);
        assert_eq!(resolve_path(&t, PkgKind::Formula, "foo").as_deref(), Some("Formula/foo.rb"));
    }

    #[test]
    fn resolve_path_cask_search_order() {
        let t = tree(&["Casks/foo.rb", "Casks/b/bar.rb", "Formula/bar.rb"]);
        assert_eq!(resolve_path(&t, PkgKind::Cask, "foo").as_deref(), Some("Casks/foo.rb"));
        assert_eq!(resolve_path(&t, PkgKind::Cask, "bar").as_deref(), Some("Casks/b/bar.rb"));
        assert_eq!(resolve_path(&t, PkgKind::Cask, "baz"), None);
        assert_eq!(resolve_path(&t, PkgKind::Formula, "foo"), None, "casks are not formulae");
    }

    #[test]
    fn resolve_path_does_not_match_version_suffix_collisions() {
        let t = tree(&["Formula/python@3.12.rb", "Formula/python.rb"]);
        assert_eq!(resolve_path(&t, PkgKind::Formula, "python").as_deref(), Some("Formula/python.rb"));
        assert_eq!(resolve_path(&t, PkgKind::Formula, "python@3.12").as_deref(), Some("Formula/python@3.12.rb"));
    }

    #[test]
    fn staged_load_failure_matches_ruby_load_errors() {
        assert!(staged_load_failure("Error: cannot load such file -- /x/staging/taps/a/b/Formula/../lib/helper"));
        assert!(staged_load_failure("Error: Invalid formula: /x/Formula/foo.rb\nfoo: uninitialized constant Helper"));
        assert!(staged_load_failure("Error: foo: undefined method `helper' for"));
        assert!(!staged_load_failure("Error: No bottle available for foo"));
        assert!(!staged_load_failure("Warning: foo 1.0 is already installed and up-to-date."));
    }
}
```

`src/brew.rs` test:

```rust
    #[test]
    fn mock_tap_info_returns_the_vec() {
        let brew = MockBrew {
            taps: vec![crate::taps::TapInfo { name: "a/b".into(), remote: None }],
            ..MockBrew::new()
        };
        assert_eq!(brew.tap_info().unwrap().len(), 1);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test taps:: ; cargo test brew::tests::mock_tap_info`
Expected: compile errors (module missing, `MockBrew` has no field `taps`).

- [ ] **Step 3: Write the implementation**

`src/taps.rs`:

```rust
//! Third-party taps: what brew has tapped, which of them brewsoak can soak,
//! where their clones and staged files live, and how a name maps to a file.

use crate::Error;
use crate::brew::{json_string_value, scan_array_objects};
use crate::origin::{self, CASK, CORE, STAGING_TAP};
use crate::resolve::PkgKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapInfo {
    /// `user/repo`, lowercased.
    pub name: String,
    /// `null` for local taps and for API-mode core/cask.
    pub remote: Option<String>,
}

/// `brew tap-info --json --installed` prints a top-level array of tap objects.
pub fn parse_tap_info_json(json: &str) -> Result<Vec<TapInfo>, Error> {
    let after = json.trim_start();
    let Some(rest) = after.strip_prefix('[') else {
        return Err(Error::Other("tap-info json is not an array".into()));
    };
    let objects = scan_array_objects(rest)
        .ok_or_else(|| Error::Other("tap-info json array is malformed".into()))?;
    let mut out = Vec::new();
    for obj in objects {
        let Some(name) = json_string_value(obj, "name").filter(|n| !n.is_empty()) else {
            continue;
        };
        out.push(TapInfo {
            name: name.to_ascii_lowercase(),
            remote: json_string_value(obj, "remote").filter(|r| !r.is_empty()),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapClass {
    Core,
    Cask,
    Staging,
    /// No remote, or not `https://`: brewsoak never installs from it.
    Unsoakable,
    Soakable,
}

/// Name matches come first: API-mode `homebrew/core` and `homebrew/cask`
/// report `remote: null` too (spec issue 2).
pub fn classify(tap: &TapInfo) -> TapClass {
    if tap.name == CORE {
        return TapClass::Core;
    }
    if tap.name == CASK {
        return TapClass::Cask;
    }
    if tap.name == STAGING_TAP {
        return TapClass::Staging;
    }
    match tap.remote.as_deref() {
        Some(r) if r.starts_with("https://") => TapClass::Soakable,
        _ => TapClass::Unsoakable,
    }
}

/// Bare clone for a tap's soak history. Never a repo under brew's `Library/Taps`.
pub fn clone_dir(cache: &Path, tap: &str) -> PathBuf {
    let (user, repo) = origin::split_tap(tap).unwrap_or((tap, "tap"));
    cache.join("taps").join(user).join(format!("{repo}.git"))
}

/// Per-tap staging directory so a tap formula never collides with a core one.
/// Core and cask keep the v1 layout directly under `tap_root`.
pub fn staging_root(tap_root: &Path, origin_tap: &str) -> PathBuf {
    if origin::is_core_or_cask(origin_tap) {
        return tap_root.to_path_buf();
    }
    let (user, repo) = origin::split_tap(origin_tap).unwrap_or((origin_tap, "tap"));
    tap_root.join("taps").join(user).join(repo)
}

/// Homebrew's lookup order for a tap. Tap aliases and renames are not followed.
pub fn resolve_path(tree: &[String], kind: PkgKind, name: &str) -> Option<String> {
    let file = format!("{name}.rb");
    let has = |p: &str| tree.iter().any(|t| t == p).then(|| p.to_string());
    let sharded = |dir: &str| {
        let prefix = format!("{dir}/");
        let suffix = format!("/{file}");
        tree.iter()
            .find(|t| t.starts_with(&prefix) && t.ends_with(&suffix))
            .cloned()
    };
    match kind {
        PkgKind::Formula => has(&format!("Formula/{file}"))
            .or_else(|| sharded("Formula"))
            .or_else(|| has(&format!("HomebrewFormula/{file}")))
            .or_else(|| has(&file)),
        PkgKind::Cask => has(&format!("Casks/{file}")).or_else(|| sharded("Casks")),
    }
}

/// brew could not load the staged `.rb` at all (as opposed to failing to
/// build or download it): the formula reaches outside its own file.
pub fn staged_load_failure(brew_output: &str) -> bool {
    let s = brew_output.to_ascii_lowercase();
    s.contains("cannot load such file")
        || s.contains("invalid formula")
        || s.contains("invalid cask")
        || s.contains("uninitialized constant")
        || s.contains("undefined method")
}
```

`src/brew.rs`:
- `use crate::taps::TapInfo;`
- trait: `fn tap_info(&self) -> Result<Vec<TapInfo>, Error>;`
- `ProcessBrew`:
  ```rust
    fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
        let output = self.run(&["tap-info".into(), "--json".into(), "--installed".into()])?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        crate::taps::parse_tap_info_json(&String::from_utf8_lossy(&output.stdout))
    }
  ```
- `MockBrew`: field `pub taps: Vec<TapInfo>`, `taps: Vec::new()` in `Default`, and `fn tap_info(&self) -> Result<Vec<TapInfo>, Error> { Ok(self.taps.clone()) }`.
- Make `json_objects_in_array`, `find_json_key`, `scan_array_objects`, `json_string_value`, `json_bool_value` `pub(crate)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test taps:: ; cargo test brew::`
Expected: all pass.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/taps.rs src/lib.rs src/brew.rs
git commit -m "feat: classify installed taps and resolve tap formula paths

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 6: Tap snapshots in `state.toml` with per-tap hours and failure isolation

**Files:**
- Modify: `src/snapshot.rs` (whole file)
- Modify: `src/cmd.rs` tests `core_snaps()` and `view_world()` (use `Snapshots::core_only`)
- Modify: `src/report.rs:29-35` (`soak_banner` unchanged signature; no change needed)
- Test: `src/snapshot.rs` tests

**Interfaces:**
- Consumes: `git::{GitStore, REF_CUTOFF, REF_HEAD}`, `taps::clone_dir`, `github::cutoff_instant`.
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct TapState { pub hours: SoakHours, pub cutoff_sha: Option<String>, pub head_sha: String, pub cutoff_time: Option<OffsetDateTime> }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Snapshots {
      pub core: TapSnapshot, pub cask: TapSnapshot,
      pub hours: SoakHours,            // global SOAK_HOURS (banner)
      pub core_hours: SoakHours, pub cask_hours: SoakHours,
      pub taps: BTreeMap<String, TapState>,
      /// tap -> Error::Git text from this run's refresh; not persisted.
      pub held_taps: BTreeMap<String, String>,
  }
  impl Snapshots {
      pub fn core_only(core: TapSnapshot, cask: TapSnapshot, hours: SoakHours) -> Self;
      pub fn tap(&self, name: &str) -> Option<&TapState>;
  }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct TapPlan { pub name: String, pub remote: String, pub hours: SoakHours }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct RefreshPlan { pub hours: SoakHours, pub core_hours: SoakHours, pub cask_hours: SoakHours, pub taps: Vec<TapPlan> }
  impl RefreshPlan { pub fn uniform(hours: SoakHours) -> Self; }
  pub fn refresh(git, gh, cache, hours, now, progress) -> Result<Snapshots, Error>;          // unchanged; = refresh_with(uniform)
  pub fn refresh_with(git: &impl GitStore, gh: &impl GithubApi, cache: &Path, plan: &RefreshPlan, now: OffsetDateTime, progress: &mut impl Write) -> Result<Snapshots, Error>;
  /// Refresh only the given taps into an existing snapshot set (outdated/info); writes state.
  pub fn refresh_taps(git: &impl GitStore, cache: &Path, snaps: &mut Snapshots, taps: &[TapPlan], now: OffsetDateTime, progress: &mut impl Write) -> Result<(), Error>;
  pub fn refresh_tap(git: &impl GitStore, cache: &Path, tap: &TapPlan, now: OffsetDateTime) -> Result<TapState, Error>;
  pub fn tap_needs_refresh(snaps: &Snapshots, name: &str, hours: SoakHours) -> bool;   // no state, or stored hours differ
  pub fn load_state(cache: &Path) -> Result<Option<Snapshots>, Error>;                 // v1 files load with core_hours = cask_hours = hours, no taps
  ```

- [ ] **Step 1: Write the failing tests** (append to `src/snapshot.rs` tests; update the existing `fixture_gh` tests only where the struct changed)

```rust
    fn tap_plan(name: &str, remote: &str, hours: u32) -> TapPlan {
        TapPlan { name: name.into(), remote: remote.into(), hours: SoakHours::new(hours).unwrap() }
    }

    fn tap_git(now: OffsetDateTime) -> InMemoryGit {
        let git = InMemoryGit::new();
        let n = now.unix_timestamp();
        git.insert_commits(
            "https://github.com/hashicorp/homebrew-tap",
            &[("hc3", n - 3600), ("hc2", n - 50 * 3600), ("hc1", n - 100 * 3600)],
        );
        git.insert_commits(
            "https://github.com/cyclonedx/homebrew-cyclonedx",
            &[("cy2", n - 3600), ("cy1", n - 30 * 3600)],
        );
        git.insert_commits("https://github.com/young/homebrew-tap", &[("y1", n - 3600)]);
        git
    }

    #[test]
    fn refresh_tap_pins_head_and_rev_list_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let state = refresh_tap(
            &git,
            dir.path(),
            &tap_plan("hashicorp/tap", "https://github.com/hashicorp/homebrew-tap", 72),
            now(),
        )
        .expect("refresh tap");
        assert_eq!(state.head_sha, "hc3");
        assert_eq!(state.cutoff_sha.as_deref(), Some("hc1"), "72h cutoff skips the 50h commit");
        assert_eq!(state.hours.get(), 72);
        assert_eq!(state.cutoff_time, Some(now() - Duration::hours(100)));
        let clone = crate::taps::clone_dir(dir.path(), "hashicorp/tap");
        assert_eq!(git.remote_url(&clone).as_deref(), Some("https://github.com/hashicorp/homebrew-tap"));
        assert_eq!(git.rev_parse(&clone, REF_HEAD).unwrap(), Some("hc3".into()));
        assert_eq!(git.rev_parse(&clone, REF_CUTOFF).unwrap(), Some("hc1".into()));
    }

    #[test]
    fn per_tap_hours_give_different_cutoffs() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let a = refresh_tap(&git, dir.path(), &tap_plan("hashicorp/tap", "https://github.com/hashicorp/homebrew-tap", 24), now()).unwrap();
        let b = refresh_tap(&git, dir.path(), &tap_plan("hashicorp/tap", "https://github.com/hashicorp/homebrew-tap", 72), now()).unwrap();
        assert_eq!(a.cutoff_sha.as_deref(), Some("hc2"));
        assert_eq!(b.cutoff_sha.as_deref(), Some("hc1"));
    }

    #[test]
    fn tap_younger_than_window_has_no_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let state = refresh_tap(&git, dir.path(), &tap_plan("young/tap", "https://github.com/young/homebrew-tap", 24), now()).unwrap();
        assert_eq!(state.head_sha, "y1");
        assert_eq!(state.cutoff_sha, None);
        assert_eq!(state.cutoff_time, None);
    }

    #[test]
    fn one_failing_tap_is_held_and_others_continue() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        git.fail_remote("https://github.com/hashicorp/homebrew-tap");
        let plan = RefreshPlan {
            taps: vec![
                tap_plan("hashicorp/tap", "https://github.com/hashicorp/homebrew-tap", 72),
                tap_plan("cyclonedx/cyclonedx", "https://github.com/cyclonedx/homebrew-cyclonedx", 24),
            ],
            ..RefreshPlan::uniform(SoakHours::new(24).unwrap())
        };
        let mut progress = Vec::new();
        let snaps = refresh_with(&git, &fixture_gh(), dir.path(), &plan, now(), &mut progress).expect("core/cask ok");
        assert!(snaps.taps.get("cyclonedx/cyclonedx").is_some_and(|t| t.cutoff_sha.as_deref() == Some("cy1")));
        assert!(snaps.taps.get("hashicorp/tap").is_none());
        let held = snaps.held_taps.get("hashicorp/tap").expect("held");
        assert!(held.contains("fetching history"), "{held}");
        let text = String::from_utf8(progress).unwrap();
        assert!(text.contains("fetching hashicorp/tap"), "{text}");
        assert!(text.contains("fetching cyclonedx/cyclonedx"), "{text}");
    }

    #[test]
    fn state_round_trips_taps_and_per_origin_hours() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let plan = RefreshPlan {
            hours: SoakHours::new(24).unwrap(),
            core_hours: SoakHours::new(8).unwrap(),
            cask_hours: SoakHours::new(24).unwrap(),
            taps: vec![
                tap_plan("hashicorp/tap", "https://github.com/hashicorp/homebrew-tap", 72),
                tap_plan("young/tap", "https://github.com/young/homebrew-tap", 24),
            ],
        };
        let snaps = refresh_with(&git, &fixture_gh(), dir.path(), &plan, now(), &mut std::io::sink()).unwrap();
        assert_eq!(snaps.core.cutoff_sha, "tenh", "core uses its own 8h window");
        assert_eq!(snaps.cask.cutoff_sha, "thirtyh");
        let raw = std::fs::read_to_string(dir.path().join("state.toml")).unwrap();
        assert!(raw.contains("[taps.\"hashicorp/tap\"]"), "{raw}");
        assert!(raw.contains("core_hours = 8"), "{raw}");
        let loaded = load_state(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, snaps);
        assert_eq!(loaded.tap("young/tap").unwrap().cutoff_sha, None);
        assert_eq!(loaded.tap("hashicorp/tap").unwrap().hours.get(), 72);
    }

    #[test]
    fn v1_state_file_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("state.toml"),
            "hours = 24\ncore_cutoff = \"a\"\ncore_head = \"b\"\ncask_cutoff = \"c\"\ncask_head = \"d\"\ncore_cutoff_time = \"2023-11-13T22:13:20Z\"\n",
        )
        .unwrap();
        let s = load_state(dir.path()).unwrap().unwrap();
        assert_eq!(s.core_hours.get(), 24);
        assert_eq!(s.cask_hours.get(), 24);
        assert!(s.taps.is_empty());
        assert!(s.held_taps.is_empty());
        assert!(s.core.cutoff_time.is_some());
    }

    #[test]
    fn tap_needs_refresh_when_missing_or_hours_differ() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let plan = RefreshPlan {
            taps: vec![tap_plan("hashicorp/tap", "https://github.com/hashicorp/homebrew-tap", 72)],
            ..RefreshPlan::uniform(SoakHours::new(24).unwrap())
        };
        let mut snaps = refresh_with(&git, &fixture_gh(), dir.path(), &plan, now(), &mut std::io::sink()).unwrap();
        assert!(!tap_needs_refresh(&snaps, "hashicorp/tap", SoakHours::new(72).unwrap()));
        assert!(tap_needs_refresh(&snaps, "hashicorp/tap", SoakHours::new(24).unwrap()));
        assert!(tap_needs_refresh(&snaps, "cyclonedx/cyclonedx", SoakHours::new(24).unwrap()));
        refresh_taps(
            &git,
            dir.path(),
            &mut snaps,
            &[tap_plan("cyclonedx/cyclonedx", "https://github.com/cyclonedx/homebrew-cyclonedx", 24)],
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert!(snaps.tap("cyclonedx/cyclonedx").is_some());
        assert!(snaps.tap("hashicorp/tap").is_some(), "existing tap state kept");
        let loaded = load_state(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.taps.len(), 2, "refresh_taps persists");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test snapshot::`
Expected: compile errors (`TapPlan`, `RefreshPlan`, `refresh_tap`, new fields).

- [ ] **Step 3: Write the implementation**

Replace `src/snapshot.rs` non-test code:

```rust
use crate::git::{GitStore, REF_CUTOFF, REF_HEAD};
use crate::github::{GithubApi, cutoff_instant};
use crate::taps;
use crate::{Error, SoakHours};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const CORE_REMOTE: &str = "https://github.com/Homebrew/homebrew-core";
pub const CASK_REMOTE: &str = "https://github.com/Homebrew/homebrew-cask";
pub const CORE_REPO: &str = "Homebrew/homebrew-core";
pub const CASK_REPO: &str = "Homebrew/homebrew-cask";
/// Name of the remote inside every tap clone (spec issue 1).
pub const TAP_REMOTE_NAME: &str = "origin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapSnapshot {
    pub cutoff_sha: String,
    pub head_sha: String,
    pub cutoff_time: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapState {
    pub hours: SoakHours,
    /// `None` when no commit is older than the tap's cutoff.
    pub cutoff_sha: Option<String>,
    pub head_sha: String,
    pub cutoff_time: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshots {
    pub core: TapSnapshot,
    pub cask: TapSnapshot,
    pub hours: SoakHours,
    pub core_hours: SoakHours,
    pub cask_hours: SoakHours,
    pub taps: BTreeMap<String, TapState>,
    /// Taps whose refresh failed this run (`Error::Git` text). Not persisted,
    /// so the next run tries again.
    pub held_taps: BTreeMap<String, String>,
}

impl Snapshots {
    pub fn core_only(core: TapSnapshot, cask: TapSnapshot, hours: SoakHours) -> Self {
        Self {
            core,
            cask,
            hours,
            core_hours: hours,
            cask_hours: hours,
            taps: BTreeMap::new(),
            held_taps: BTreeMap::new(),
        }
    }

    pub fn tap(&self, name: &str) -> Option<&TapState> {
        self.taps.get(&name.to_ascii_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapPlan {
    pub name: String,
    pub remote: String,
    pub hours: SoakHours,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshPlan {
    pub hours: SoakHours,
    pub core_hours: SoakHours,
    pub cask_hours: SoakHours,
    pub taps: Vec<TapPlan>,
}

impl RefreshPlan {
    pub fn uniform(hours: SoakHours) -> Self {
        Self { hours, core_hours: hours, cask_hours: hours, taps: Vec::new() }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TapStateFile {
    hours: u32,
    cutoff: String,
    head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cutoff_time: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StateFile {
    hours: u32,
    #[serde(default)]
    core_hours: Option<u32>,
    #[serde(default)]
    cask_hours: Option<u32>,
    core_cutoff: String,
    core_head: String,
    cask_cutoff: String,
    cask_head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    core_cutoff_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cask_cutoff_time: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    taps: BTreeMap<String, TapStateFile>,
}

pub fn refresh(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    hours: SoakHours,
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    refresh_with(git, gh, cache, &RefreshPlan::uniform(hours), now, progress)
}

/// Core and cask failures abort (v1). A tap failure holds only that tap.
pub fn refresh_with(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    plan: &RefreshPlan,
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    std::fs::create_dir_all(cache)?;
    writeln!(progress, "fetching {CORE_REPO}…")?;
    let core = refresh_core_tap(git, gh, &cache.join("core.git"), CORE_REMOTE, CORE_REPO, cutoff_instant(now, plan.core_hours))?;
    writeln!(progress, "fetching {CASK_REPO}…")?;
    let cask = refresh_core_tap(git, gh, &cache.join("cask.git"), CASK_REMOTE, CASK_REPO, cutoff_instant(now, plan.cask_hours))?;
    let mut snaps = Snapshots {
        core,
        cask,
        hours: plan.hours,
        core_hours: plan.core_hours,
        cask_hours: plan.cask_hours,
        taps: BTreeMap::new(),
        held_taps: BTreeMap::new(),
    };
    refresh_taps(git, cache, &mut snaps, &plan.taps, now, progress)?;
    Ok(snaps)
}

pub fn refresh_taps(
    git: &impl GitStore,
    cache: &Path,
    snaps: &mut Snapshots,
    taps: &[TapPlan],
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<(), Error> {
    for tap in taps {
        writeln!(progress, "fetching {}…", tap.name)?;
        match refresh_tap(git, cache, tap, now) {
            Ok(state) => {
                snaps.held_taps.remove(&tap.name);
                snaps.taps.insert(tap.name.clone(), state);
            }
            Err(e @ Error::Git { .. }) => {
                snaps.taps.remove(&tap.name);
                snaps.held_taps.insert(tap.name.clone(), e.to_string());
            }
            Err(e) => return Err(e),
        }
    }
    write_state(cache, snaps)
}

pub fn refresh_tap(
    git: &impl GitStore,
    cache: &Path,
    tap: &TapPlan,
    now: OffsetDateTime,
) -> Result<TapState, Error> {
    let dir = taps::clone_dir(cache, &tap.name);
    std::fs::create_dir_all(dir.parent().unwrap_or(cache))?;
    git.init_bare(&dir)?;
    git.set_remote(&dir, TAP_REMOTE_NAME, &tap.remote)?;
    git.fetch_history(&dir, TAP_REMOTE_NAME, REF_HEAD)?;
    let head_sha = git.rev_parse(&dir, REF_HEAD)?.ok_or_else(|| Error::Git {
        action: format!("resolving the fetched head of {}", tap.name),
        detail: format!("{REF_HEAD} is missing after fetch"),
    })?;
    let until = cutoff_instant(now, tap.hours).unix_timestamp();
    let cutoff = git.rev_list_before(&dir, REF_HEAD, until)?;
    let (cutoff_sha, cutoff_time) = match cutoff {
        Some((sha, when)) => {
            git.update_ref(&dir, REF_CUTOFF, &sha)?;
            (Some(sha), OffsetDateTime::from_unix_timestamp(when).ok())
        }
        None => (None, None),
    };
    Ok(TapState { hours: tap.hours, cutoff_sha, head_sha, cutoff_time })
}

pub fn tap_needs_refresh(snaps: &Snapshots, name: &str, hours: SoakHours) -> bool {
    match snaps.tap(name) {
        Some(state) => state.hours != hours,
        None => true,
    }
}

pub fn load_state(cache: &Path) -> Result<Option<Snapshots>, Error> {
    let path = cache.join("state.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let parsed: StateFile = toml::from_str(&raw).map_err(|e| Error::Other(format!("state.toml: {e}")))?;
    let hours_of = |n: u32| SoakHours::new(n).ok_or_else(|| Error::Other(format!("invalid hours in state.toml: {n}")));
    let hours = hours_of(parsed.hours)?;
    let mut taps = BTreeMap::new();
    for (name, t) in parsed.taps {
        taps.insert(
            name.to_ascii_lowercase(),
            TapState {
                hours: hours_of(t.hours)?,
                cutoff_sha: (!t.cutoff.is_empty()).then_some(t.cutoff),
                head_sha: t.head,
                cutoff_time: parse_state_time(t.cutoff_time.as_deref()),
            },
        );
    }
    Ok(Some(Snapshots {
        core: TapSnapshot { cutoff_sha: parsed.core_cutoff, head_sha: parsed.core_head, cutoff_time: parse_state_time(parsed.core_cutoff_time.as_deref()) },
        cask: TapSnapshot { cutoff_sha: parsed.cask_cutoff, head_sha: parsed.cask_head, cutoff_time: parse_state_time(parsed.cask_cutoff_time.as_deref()) },
        hours,
        core_hours: parsed.core_hours.map(hours_of).transpose()?.unwrap_or(hours),
        cask_hours: parsed.cask_hours.map(hours_of).transpose()?.unwrap_or(hours),
        taps,
        held_taps: BTreeMap::new(),
    }))
}

fn parse_state_time(raw: Option<&str>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw?, &Rfc3339).ok()
}

fn fmt_time(t: Option<OffsetDateTime>) -> Option<String> {
    t.and_then(|t| t.format(&Rfc3339).ok())
}

fn write_state(cache: &Path, snaps: &Snapshots) -> Result<(), Error> {
    let file = StateFile {
        hours: snaps.hours.get(),
        core_hours: Some(snaps.core_hours.get()),
        cask_hours: Some(snaps.cask_hours.get()),
        core_cutoff: snaps.core.cutoff_sha.clone(),
        core_head: snaps.core.head_sha.clone(),
        cask_cutoff: snaps.cask.cutoff_sha.clone(),
        cask_head: snaps.cask.head_sha.clone(),
        core_cutoff_time: fmt_time(snaps.core.cutoff_time),
        cask_cutoff_time: fmt_time(snaps.cask.cutoff_time),
        taps: snaps
            .taps
            .iter()
            .map(|(name, t)| {
                (
                    name.clone(),
                    TapStateFile {
                        hours: t.hours.get(),
                        cutoff: t.cutoff_sha.clone().unwrap_or_default(),
                        head: t.head_sha.clone(),
                        cutoff_time: fmt_time(t.cutoff_time),
                    },
                )
            })
            .collect(),
    };
    let body = toml::to_string(&file).map_err(|e| Error::Other(format!("state.toml: {e}")))?;
    std::fs::write(cache.join("state.toml"), body)?;
    Ok(())
}
```

Rename the existing private `refresh_tap` (core/cask, GitHub-backed) to `refresh_core_tap`; keep `cutoff_via_shallow` as is. In `src/cmd.rs` tests, change `core_snaps()` and `view_world()` to build with `Snapshots::core_only(TapSnapshot {...}, TapSnapshot {...}, SoakHours::new(24).expect("hours >= 1"))`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test snapshot:: && cargo test`
Expected: all pass. The existing `load_state_round_trips` assertion `raw.contains("hours = 24")` still holds with serde output.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/snapshot.rs src/cmd.rs
git commit -m "feat: per-tap soak snapshots in state.toml with failure isolation

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 7: Inventory: origin and class for every installed package; explicit tokens

**Files:**
- Create: `src/inventory.rs`
- Modify: `src/lib.rs:1-16` (add `pub mod inventory;`)
- Test: `src/inventory.rs` tests

**Interfaces:**
- Consumes: `brew::{Brew, InstalledPkg}`, `taps::{TapInfo, TapClass, classify}`, `origin::{OriginRecords, resolve_origin, is_core_or_cask, split_tap, default_origin}`, `config::Config`, `resolve::PkgKind`.
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum PkgClass { Soaked, NoSoak, Unsoakable }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Pkg { pub name: String, pub kind: PkgKind, pub origin: String, pub class: PkgClass, pub receipt_rb: String, pub receipt_tap: Option<String>, pub pinned: bool }
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct Inventory { pub pkgs: Vec<Pkg>, pub taps: BTreeMap<String, TapClass>, pub tap_remotes: BTreeMap<String, String> }
  impl Inventory {
      pub fn build(installed: Vec<InstalledPkg>, taps: &[TapInfo], origins: &OriginRecords, cfg: &Config) -> Self;
      pub fn load(brew: &impl Brew, cache: &Path, cfg: &Config) -> Result<Self, Error>;  // installed_packages + tap_info + OriginRecords::load
      pub fn find(&self, name: &str) -> Option<&Pkg>;
      pub fn tap_class(&self, tap: &str) -> Option<TapClass>;
      pub fn class_for(&self, origin: &str, name: &str, cfg: &Config) -> PkgClass;    // for names not installed
      /// Soakable taps with >= 1 installed soaked package, plus `extra` origins
      /// (explicit tokens) that are soakable: (name, remote).
      pub fn needed_taps(&self, extra: &[String]) -> Vec<(String, String)>;
      pub fn any_no_soak(&self) -> bool;
  }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Token { pub origin: Option<String>, pub name: String }
  pub fn parse_token(raw: &str) -> Result<Token, Error>;   // "user/repo/name" | "name"; one slash => Error::Usage
  ```
  Note: this task does not change `brew.rs` (`installed_core` still drops third-party); Task 9 flips that in the same commit that makes `cmd.rs` origin-aware.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::SoakHours;
    use crate::config::parse_file;

    fn cfg(no_soak: &str) -> Config {
        let parsed = parse_file(&format!("NO_SOAK = {no_soak}\n"));
        Config { no_soak: parsed.no_soak, ..Config::uniform(SoakHours::new(24).unwrap()) }
    }

    fn installed(name: &str, kind: PkgKind, tap: Option<&str>) -> InstalledPkg {
        InstalledPkg { name: name.into(), kind, receipt_rb: "rb".into(), pinned: false, tap: tap.map(str::to_string) }
    }

    fn taps() -> Vec<TapInfo> {
        vec![
            TapInfo { name: "homebrew/core".into(), remote: None },
            TapInfo { name: "homebrew/cask".into(), remote: None },
            TapInfo { name: "brewsoakr/soaked".into(), remote: None },
            TapInfo { name: "hashicorp/tap".into(), remote: Some("https://github.com/hashicorp/homebrew-tap".into()) },
            TapInfo { name: "ericfitz/tap".into(), remote: Some("https://github.com/ericfitz/homebrew-tap".into()) },
            TapInfo { name: "local/tap".into(), remote: None },
        ]
    }

    #[test]
    fn build_classifies_every_installed_package() {
        let mut origins = OriginRecords::default();
        origins.set(PkgKind::Formula, "vault", "hashicorp/tap");
        let inv = Inventory::build(
            vec![
                installed("wget", PkgKind::Formula, Some("homebrew/core")),
                installed("ca-certificates", PkgKind::Formula, None),
                installed("terraform", PkgKind::Formula, Some("hashicorp/tap")),
                installed("vault", PkgKind::Formula, None),
                installed("brewsoak", PkgKind::Formula, Some("ericfitz/tap")),
                installed("thing", PkgKind::Formula, Some("local/tap")),
                installed("firefox", PkgKind::Cask, Some("homebrew/cask")),
            ],
            &taps(),
            &origins,
            &cfg("[\"ericfitz/tap\", \"wget\"]"),
        );
        let by = |n: &str| inv.find(n).unwrap();
        assert_eq!((by("wget").origin.as_str(), by("wget").class), ("homebrew/core", PkgClass::NoSoak));
        assert_eq!((by("ca-certificates").origin.as_str(), by("ca-certificates").class), ("homebrew/core", PkgClass::Soaked));
        assert_eq!((by("terraform").origin.as_str(), by("terraform").class), ("hashicorp/tap", PkgClass::Soaked));
        assert_eq!((by("vault").origin.as_str(), by("vault").class), ("hashicorp/tap", PkgClass::Soaked), "staged install: origin from record");
        assert_eq!(by("vault").receipt_tap, None);
        assert_eq!((by("brewsoak").origin.as_str(), by("brewsoak").class), ("ericfitz/tap", PkgClass::NoSoak));
        assert_eq!((by("thing").origin.as_str(), by("thing").class), ("local/tap", PkgClass::Unsoakable));
        assert_eq!(by("firefox").class, PkgClass::Soaked);
        assert_eq!(inv.tap_class("hashicorp/tap"), Some(TapClass::Soakable));
        assert_eq!(inv.tap_class("brewsoakr/soaked"), Some(TapClass::Staging));
        assert!(inv.any_no_soak());
    }

    #[test]
    fn no_soak_wins_over_unsoakable() {
        let inv = Inventory::build(
            vec![installed("thing", PkgKind::Formula, Some("local/tap"))],
            &taps(),
            &OriginRecords::default(),
            &cfg("[\"local/tap\"]"),
        );
        assert_eq!(inv.find("thing").unwrap().class, PkgClass::NoSoak);
    }

    #[test]
    fn receipt_tap_not_installed_anymore_is_unsoakable() {
        let inv = Inventory::build(
            vec![installed("gone", PkgKind::Formula, Some("old/tap"))],
            &taps(),
            &OriginRecords::default(),
            &cfg("[]"),
        );
        assert_eq!(inv.find("gone").unwrap().class, PkgClass::Unsoakable);
        assert_eq!(inv.class_for("old/tap", "gone", &cfg("[]")), PkgClass::Unsoakable);
        assert_eq!(inv.class_for("old/tap", "gone", &cfg("[\"old/tap\"]")), PkgClass::NoSoak);
    }

    #[test]
    fn needed_taps_are_soakable_with_a_soaked_package_plus_extras() {
        let inv = Inventory::build(
            vec![
                installed("terraform", PkgKind::Formula, Some("hashicorp/tap")),
                installed("brewsoak", PkgKind::Formula, Some("ericfitz/tap")),
                installed("thing", PkgKind::Formula, Some("local/tap")),
            ],
            &taps(),
            &OriginRecords::default(),
            &cfg("[\"ericfitz/tap\"]"),
        );
        assert_eq!(
            inv.needed_taps(&[]),
            vec![("hashicorp/tap".to_string(), "https://github.com/hashicorp/homebrew-tap".to_string())]
        );
        let with_extra = inv.needed_taps(&["ericfitz/tap".into(), "local/tap".into(), "homebrew/core".into()]);
        assert_eq!(with_extra.len(), 2, "{with_extra:?}");
        assert!(with_extra.iter().any(|(n, _)| n == "ericfitz/tap"));
    }

    #[test]
    fn class_for_not_installed_names() {
        let inv = Inventory::build(Vec::new(), &taps(), &OriginRecords::default(), &cfg("[\"wget\"]"));
        let c = cfg("[\"wget\"]");
        assert_eq!(inv.class_for("homebrew/core", "wget", &c), PkgClass::NoSoak);
        assert_eq!(inv.class_for("homebrew/core", "curl", &c), PkgClass::Soaked);
        assert_eq!(inv.class_for("hashicorp/tap", "vault", &c), PkgClass::Soaked);
        assert_eq!(inv.class_for("local/tap", "x", &c), PkgClass::Unsoakable);
        assert_eq!(inv.class_for("nobody/tap", "x", &c), PkgClass::Unsoakable, "untapped tap");
    }

    #[test]
    fn parse_token_forms() {
        assert_eq!(parse_token("wget").unwrap(), Token { origin: None, name: "wget".into() });
        assert_eq!(
            parse_token("HashiCorp/tap/terraform").unwrap(),
            Token { origin: Some("hashicorp/tap".into()), name: "terraform".into() }
        );
        assert!(matches!(parse_token("user/foo"), Err(Error::Usage(_))));
        assert!(matches!(parse_token("a/b/c/d"), Err(Error::Usage(_))));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test inventory::`
Expected: compile error (module missing).

- [ ] **Step 3: Write the implementation**

```rust
//! Every installed package with its origin tap and soak class.

use crate::Error;
use crate::brew::{Brew, InstalledPkg};
use crate::config::Config;
use crate::origin::{self, OriginRecords};
use crate::resolve::PkgKind;
use crate::taps::{self, TapClass, TapInfo};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgClass {
    Soaked,
    NoSoak,
    Unsoakable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkg {
    pub name: String,
    pub kind: PkgKind,
    /// `user/repo`, lowercase; core/cask included.
    pub origin: String,
    pub class: PkgClass,
    pub receipt_rb: String,
    /// What brew's receipt says; `None` when it was staged by brewsoak.
    pub receipt_tap: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub pkgs: Vec<Pkg>,
    pub taps: BTreeMap<String, TapClass>,
    pub tap_remotes: BTreeMap<String, String>,
}

impl Inventory {
    pub fn build(
        installed: Vec<InstalledPkg>,
        taps: &[TapInfo],
        origins: &OriginRecords,
        cfg: &Config,
    ) -> Self {
        let mut inv = Inventory::default();
        for tap in taps {
            inv.taps.insert(tap.name.clone(), taps::classify(tap));
            if let Some(remote) = &tap.remote {
                inv.tap_remotes.insert(tap.name.clone(), remote.clone());
            }
        }
        for p in installed {
            let origin = origin::resolve_origin(p.tap.as_deref(), origins, p.kind, &p.name);
            let class = inv.class_for(&origin, &p.name, cfg);
            inv.pkgs.push(Pkg {
                name: p.name,
                kind: p.kind,
                origin,
                class,
                receipt_rb: p.receipt_rb,
                receipt_tap: p.tap,
                pinned: p.pinned,
            });
        }
        inv
    }

    pub fn load(brew: &impl Brew, cache: &Path, cfg: &Config) -> Result<Self, Error> {
        let installed = brew.installed_packages()?;
        let taps = brew.tap_info()?;
        let origins = OriginRecords::load(cache);
        Ok(Self::build(installed, &taps, &origins, cfg))
    }

    pub fn find(&self, name: &str) -> Option<&Pkg> {
        self.pkgs.iter().find(|p| p.name == name)
    }

    pub fn tap_class(&self, tap: &str) -> Option<TapClass> {
        self.taps.get(&tap.to_ascii_lowercase()).copied()
    }

    /// NO_SOAK first; then core/cask and soakable taps are soaked; anything
    /// else (no remote, non-HTTPS, staging, or not tapped at all) is unsoakable.
    pub fn class_for(&self, origin_tap: &str, name: &str, cfg: &Config) -> PkgClass {
        if cfg.is_no_soak(origin_tap, name) {
            return PkgClass::NoSoak;
        }
        if origin::is_core_or_cask(origin_tap) {
            return PkgClass::Soaked;
        }
        match self.tap_class(origin_tap) {
            Some(TapClass::Soakable) => PkgClass::Soaked,
            _ => PkgClass::Unsoakable,
        }
    }

    pub fn needed_taps(&self, extra: &[String]) -> Vec<(String, String)> {
        let mut names: Vec<String> = self
            .pkgs
            .iter()
            .filter(|p| p.class == PkgClass::Soaked && !origin::is_core_or_cask(&p.origin))
            .map(|p| p.origin.clone())
            .chain(extra.iter().map(|e| e.to_ascii_lowercase()))
            .filter(|t| self.tap_class(t) == Some(TapClass::Soakable))
            .collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter_map(|n| self.tap_remotes.get(&n).map(|r| (n.clone(), r.clone())))
            .collect()
    }

    pub fn any_no_soak(&self) -> bool {
        self.pkgs.iter().any(|p| p.class == PkgClass::NoSoak)
    }
}

/// An explicit command-line name: `name` or `user/repo/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub origin: Option<String>,
    pub name: String,
}

pub fn parse_token(raw: &str) -> Result<Token, Error> {
    match raw.matches('/').count() {
        0 => Ok(Token { origin: None, name: raw.to_string() }),
        2 => {
            let (tap, name) = raw.rsplit_once('/').expect("two slashes");
            if origin::split_tap(tap).is_none() || name.is_empty() {
                return Err(Error::Usage(format!("{raw}: expected user/repo/name")));
            }
            Ok(Token { origin: Some(tap.to_ascii_lowercase()), name: name.to_string() })
        }
        _ => Err(Error::Usage(format!("{raw}: expected name or user/repo/name"))),
    }
}
```

`installed_packages` does not exist yet: for this task only, call `brew.installed_core()` in `Inventory::load` (Task 9 renames it). The `default_origin` import is unused here; omit it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test inventory::`
Expected: 6 passed.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/inventory.rs src/lib.rs
git commit -m "feat: inventory classifies installed packages by origin and soak class

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 8: The no-soak brew step

**Files:**
- Modify: `src/nosoak.rs` (add the step below the matcher)
- Modify: `src/brew.rs:96-146` (`MockBrew.next_outputs: Mutex<VecDeque<(i32, Vec<u8>)>>`, consumed by `run_visible` before falling back to `next_status`/`next_stdout`)
- Test: `src/nosoak.rs` tests, `src/brew.rs` test `mock_run_visible_pops_queued_outputs`

**Interfaces:**
- Consumes: `brew::Brew`, `quiet::installed_from_output`, `origin::is_core_or_cask`.
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Target { pub origin: String, pub name: String, pub switch_tap: bool }
  #[derive(Debug, Default, Clone, PartialEq, Eq)]
  pub struct StepResult { pub status: Option<i32>, pub notes: Vec<String>, pub count: usize, pub installed: BTreeMap<String, String> }
  pub fn brew_token(origin: &str, name: &str) -> String;   // "name" for core/cask, else "origin/name"
  pub fn run_step(brew: &impl Brew, verb: &str, user_flags: &[String], targets: &[Target], out: &mut impl Write) -> Result<StepResult, Error>;
  ```
  `MockBrew` gains `pub next_outputs: Mutex<VecDeque<(i32, Vec<u8>)>>`; `run_visible` pops the front if any, else uses `next_status`/`next_stdout`.

Behavior of `run_step`:
1. No targets: `StepResult::default()`, no brew calls.
2. Print `no-soak: updating brew` then `brew update` (visible). Non-zero: note `no-soak: brew update failed (exit N); not {verb}d: a, b` (tokens), `status = Some(N)`, `count = targets.len()`, return. Nothing else runs.
3. Tokens whose `switch_tap` is true **and** `verb == "upgrade"` go to one `brew install [flags] tokens...`; the rest to one `brew <verb> [flags] tokens...` (each only if non-empty). Flags: `user_flags` minus any brew subcommand word (`tap::is_brew_subcommand`, make it `pub(crate)`).
4. The plain run uses `cmd::merge_status` semantics (already-installed nonzero → 0); the switch run does not: if its status is nonzero, or `quiet::installed_from_output` lacks a switch target's name, note `<name>: brew did not replace the staged keg; run brew reinstall <token>` and keep brew's status (at least 1).
5. `count` = number of targets handed to brew; `installed` = union of `installed_from_output` over the runs.

- [ ] **Step 1: Write the failing tests** (append to `src/nosoak.rs` tests)

```rust
    use crate::brew::MockBrew;
    use std::collections::VecDeque;

    fn t(origin: &str, name: &str) -> Target {
        Target { origin: origin.into(), name: name.into(), switch_tap: false }
    }

    fn runs(brew: &MockBrew) -> Vec<Vec<String>> {
        brew.visible_runs.lock().unwrap().clone()
    }

    #[test]
    fn brew_token_is_bare_for_core_and_cask() {
        assert_eq!(brew_token("homebrew/core", "wget"), "wget");
        assert_eq!(brew_token("homebrew/cask", "firefox"), "firefox");
        assert_eq!(brew_token("ericfitz/tap", "brewsoak"), "ericfitz/tap/brewsoak");
    }

    #[test]
    fn no_targets_runs_nothing() {
        let brew = MockBrew::new();
        let r = run_step(&brew, "upgrade", &[], &[], &mut Vec::new()).unwrap();
        assert_eq!(r, StepResult::default());
        assert!(runs(&brew).is_empty());
    }

    #[test]
    fn one_update_then_one_upgrade_with_full_tokens() {
        let brew = MockBrew::new();
        let mut out = Vec::new();
        let r = run_step(
            &brew,
            "upgrade",
            &["--verbose".into()],
            &[t("ericfitz/tap", "brewsoak"), t("homebrew/core", "wget")],
            &mut out,
        )
        .unwrap();
        let got = runs(&brew);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0], vec!["update".to_string()]);
        assert_eq!(got[1], vec!["upgrade", "--verbose", "ericfitz/tap/brewsoak", "wget"]);
        assert_eq!(r.count, 2);
        assert_eq!(r.status, Some(0));
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert!(String::from_utf8(out).unwrap().contains("no-soak: updating brew"));
    }

    #[test]
    fn install_verb_uses_install_and_strips_subcommand_words_from_flags() {
        let brew = MockBrew::new();
        run_step(&brew, "install", &["install".into(), "--cask".into()], &[t("homebrew/cask", "firefox")], &mut Vec::new()).unwrap();
        let got = runs(&brew);
        assert_eq!(got[1], vec!["install", "--cask", "firefox"]);
    }

    #[test]
    fn tap_switch_targets_use_install_in_a_separate_run() {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![
                (0, Vec::new()),
                (0, Vec::new()),
                (0, b"\xf0\x9f\x8d\xba  /opt/homebrew/Cellar/vault/1.2.0: 5 files, 1MB\n".to_vec()),
            ])),
            ..MockBrew::new()
        };
        let targets = [
            t("homebrew/core", "wget"),
            Target { origin: "hashicorp/tap".into(), name: "vault".into(), switch_tap: true },
        ];
        let r = run_step(&brew, "upgrade", &[], &targets, &mut Vec::new()).unwrap();
        let got = runs(&brew);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[1], vec!["upgrade", "wget"]);
        assert_eq!(got[2], vec!["install", "hashicorp/tap/vault"]);
        assert_eq!(r.status, Some(0));
        assert_eq!(r.installed.get("vault").map(String::as_str), Some("1.2.0"));
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    #[test]
    fn switch_install_that_did_not_replace_keg_keeps_status_and_notes() {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![
                (0, Vec::new()),
                (1, b"Warning: hashicorp/tap/vault 1.2.0 is already installed and up-to-date.\n".to_vec()),
            ])),
            ..MockBrew::new()
        };
        let targets = [Target { origin: "hashicorp/tap".into(), name: "vault".into(), switch_tap: true }];
        let r = run_step(&brew, "upgrade", &[], &targets, &mut Vec::new()).unwrap();
        assert_eq!(r.status, Some(1), "already-installed masking must not apply to the switch run");
        assert!(r.notes.iter().any(|n| n.contains("did not replace") && n.contains("brew reinstall hashicorp/tap/vault")), "{:?}", r.notes);
    }

    #[test]
    fn switch_only_applies_to_upgrade() {
        let brew = MockBrew::new();
        let targets = [Target { origin: "hashicorp/tap".into(), name: "vault".into(), switch_tap: true }];
        run_step(&brew, "reinstall", &[], &targets, &mut Vec::new()).unwrap();
        assert_eq!(runs(&brew)[1], vec!["reinstall", "hashicorp/tap/vault"]);
    }

    #[test]
    fn failed_brew_update_fails_the_step_only() {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![(3, b"Error: no network\n".to_vec())])),
            ..MockBrew::new()
        };
        let r = run_step(&brew, "upgrade", &[], &[t("ericfitz/tap", "brewsoak"), t("homebrew/core", "wget")], &mut Vec::new()).unwrap();
        assert_eq!(runs(&brew).len(), 1, "no upgrade after a failed update");
        assert_eq!(r.status, Some(3));
        assert_eq!(r.count, 2);
        assert!(r.notes.iter().any(|n| n.contains("brew update failed (exit 3)") && n.contains("ericfitz/tap/brewsoak, wget")), "{:?}", r.notes);
    }

    #[test]
    fn plain_run_already_installed_nonzero_is_success() {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![
                (0, Vec::new()),
                (1, b"Warning: wget 1.0 is already installed and up-to-date.\n".to_vec()),
            ])),
            ..MockBrew::new()
        };
        let r = run_step(&brew, "upgrade", &[], &[t("homebrew/core", "wget")], &mut Vec::new()).unwrap();
        assert_eq!(r.status, Some(0));
    }
```

`src/brew.rs` test:

```rust
    #[test]
    fn mock_run_visible_pops_queued_outputs() {
        let brew = MockBrew {
            next_outputs: Mutex::new(std::collections::VecDeque::from(vec![(2, b"first\n".to_vec())])),
            next_status: 0,
            ..MockBrew::new()
        };
        let a = brew.run_visible(&["x".into()]).unwrap();
        assert_eq!(a.status.code(), Some(2));
        assert_eq!(a.stdout, b"first\n");
        let b = brew.run_visible(&["y".into()]).unwrap();
        assert_eq!(b.status.code(), Some(0), "queue empty: falls back to next_status");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test nosoak:: ; cargo test brew::tests::mock_run_visible_pops`
Expected: compile errors (`Target`, `run_step`, `next_outputs`).

- [ ] **Step 3: Write the implementation**

`src/brew.rs` `MockBrew`: add `pub next_outputs: Mutex<VecDeque<(i32, Vec<u8>)>>` (`use std::collections::VecDeque;`), default `Mutex::new(VecDeque::new())`, and change `run_visible`:

```rust
    fn run_visible(&self, args: &[String]) -> Result<Output, Error> {
        self.record(args);
        self.record_visible(args);
        let queued = self.next_outputs.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        Ok(match queued {
            Some((status, stdout)) => Output {
                status: std::process::ExitStatus::from_raw(exit_status_raw(status)),
                stdout,
                stderr: Vec::new(),
            },
            None => self.mock_output(),
        })
    }
```

`src/tap.rs`: `pub(crate) fn is_brew_subcommand`.

`src/nosoak.rs` additions:

```rust
use crate::Error;
use crate::brew::Brew;
use crate::origin;
use crate::quiet;
use crate::tap::is_brew_subcommand;
use std::collections::BTreeMap;
use std::io::Write;

/// One no-soak package handed to brew. `switch_tap`: the installed keg was
/// staged by brewsoak (receipt tap empty) and the origin is a third-party tap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub origin: String,
    pub name: String,
    pub switch_tap: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub status: Option<i32>,
    pub notes: Vec<String>,
    /// Targets handed to brew (the `no-soak N` count).
    pub count: usize,
    /// Cellar versions brew reported installing, for the staleness check.
    pub installed: BTreeMap<String, String>,
}

pub fn brew_token(origin_tap: &str, name: &str) -> String {
    if origin::is_core_or_cask(origin_tap) {
        name.to_string()
    } else {
        format!("{origin_tap}/{name}")
    }
}

fn merge(slot: &mut Option<i32>, code: i32) {
    *slot = Some(slot.map_or(code, |prev| prev.max(code)));
}

fn already_installed(text: &str) -> bool {
    text.to_ascii_lowercase().contains("already installed")
}

/// Spec "No-soak packages": one `brew update`, then one `brew <verb>` with
/// full tokens; tap-switch targets go through `brew install` on `upgrade`.
pub fn run_step(
    brew: &impl Brew,
    verb: &str,
    user_flags: &[String],
    targets: &[Target],
    out: &mut impl Write,
) -> Result<StepResult, Error> {
    let mut result = StepResult::default();
    if targets.is_empty() {
        return Ok(result);
    }
    result.count = targets.len();
    let tokens: Vec<String> = targets.iter().map(|t| brew_token(&t.origin, &t.name)).collect();

    writeln!(out, "no-soak: updating brew")?;
    let update = brew.run_visible(&["update".to_string()])?;
    if !update.status.success() {
        let code = update.status.code().unwrap_or(1);
        result.notes.push(format!(
            "no-soak: brew update failed (exit {code}); not {verb}d: {}",
            tokens.join(", ")
        ));
        result.status = Some(code);
        return Ok(result);
    }

    let flags: Vec<String> = user_flags.iter().filter(|f| !is_brew_subcommand(f)).cloned().collect();
    let (switch, plain): (Vec<&Target>, Vec<&Target>) =
        targets.iter().partition(|t| t.switch_tap && verb == "upgrade");

    if !plain.is_empty() {
        let mut args = vec![verb.to_string()];
        args.extend(flags.iter().cloned());
        args.extend(plain.iter().map(|t| brew_token(&t.origin, &t.name)));
        writeln!(out, "no-soak: brew {}", args.join(" "))?;
        let output = brew.run_visible(&args)?;
        result.installed.extend(quiet::installed_from_output(&output.stdout));
        let mut code = output.status.code().unwrap_or(1);
        if code != 0 && already_installed(&String::from_utf8_lossy(&output.stdout)) {
            code = 0;
        }
        merge(&mut result.status, code);
    }

    if !switch.is_empty() {
        let mut args = vec!["install".to_string()];
        args.extend(flags.iter().cloned());
        args.extend(switch.iter().map(|t| brew_token(&t.origin, &t.name)));
        writeln!(out, "no-soak: brew {} (moving staged kegs to their tap)", args.join(" "))?;
        let output = brew.run_visible(&args)?;
        let installed = quiet::installed_from_output(&output.stdout);
        let mut code = output.status.code().unwrap_or(1);
        for t in &switch {
            if code != 0 || !installed.contains_key(&t.name) {
                result.notes.push(format!(
                    "{}: brew did not replace the staged keg; run brew reinstall {}",
                    t.name,
                    brew_token(&t.origin, &t.name)
                ));
                code = code.max(1);
            }
        }
        result.installed.extend(installed);
        merge(&mut result.status, code);
    }
    Ok(result)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test nosoak:: ; cargo test brew::`
Expected: all pass.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/nosoak.rs src/brew.rs src/tap.rs
git commit -m "feat: no-soak step runs brew update once and one brew upgrade with full tokens

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 9: Origin-aware commands: inventory plumbing, tap resolution, explicit tokens, no-soak collection

This is the plumbing task. It makes `cmd.rs` resolve every package against its origin's snapshot, stops `brew.rs` from dropping third-party packages (same commit, so behavior never regresses mid-sequence), replaces the third-party passthrough with class-based handling, and runs the Task 8 no-soak step after the soaked work. Staged installs still go to the v1 staging root; Task 10 moves tap packages to per-tap directories and adds the cross-origin dep walk.

**Files:**
- Modify: `src/brew.rs:36` (`installed_core` → `installed_packages`), `src/brew.rs:502-569` (remove `keep_tap`), brew tests (`parse_installed_json_keeps_core_and_cask_drops_third_party` → `parse_installed_json_keeps_third_party`)
- Modify: `src/report.rs:121-153` (`Counts.no_soak`, `counts_line`, `origin_line`)
- Modify: `src/cmd.rs` (signatures of `ensure_snapshots`, `classify_installed`, `write_update_summary`, `prefetch_installed`, `update`, `outdated`, `info`, `upgrade`, `plan_size`, `install`, `reinstall`, `apply_many`, `ApplySession`, `apply_one`, `install_cutoff`, `install_missing_dep`, `resolve_kind`, `CutoffDepWalk`, `resolve_named`, `resolve_view`, `resolve_pkg_blobs`, `cutoff_blob_exists`, `cutoff_in_either_tree`, `natural_kind`, `write_cutoff_blob`; remove `passthrough_third_party`)
- Modify: `src/lib.rs:120-312` (`dispatch`; remove `third_party_only_names`), lib tests
- Modify: `src/inventory.rs:66` (`installed_core` → `installed_packages`)
- Test: `src/cmd.rs` tests, `src/lib.rs` tests, `src/report.rs` tests

**Interfaces:**
- Consumes: `inventory::{Inventory, Pkg, PkgClass, Token, parse_token}`, `config::Config`, `snapshot::{RefreshPlan, TapPlan, refresh_with, refresh_taps, tap_needs_refresh, TapState}`, `taps::{resolve_path, clone_dir, TapClass}`, `nosoak::{Target, run_step, brew_token}`, `origin::*`.
- Produces (new public signatures in `cmd.rs`):
  ```rust
  pub fn ensure_snapshots(git: &impl GitStore, gh: &impl GithubApi, brew: &impl Brew, cache: &Path, cfg: &Config, inv: &Inventory, extra_taps: &[String], now: OffsetDateTime, force: bool, progress: &mut impl Write) -> Result<Snapshots, Error>;
  pub fn refresh_plan(cfg: &Config, inv: &Inventory, extra_taps: &[String]) -> RefreshPlan;
  pub fn update(brew, git, gh, cache, cfg: &Config, inv: &Inventory, now, verbose, out) -> Result<(), Error>;
  pub fn outdated(brew, git, snaps, cache, inv: &Inventory, cfg: &Config, extra_args, out) -> Result<RunResult, Error>;
  pub fn info(brew, git, snaps, cache, inv: &Inventory, cfg: &Config, names, user_flags, out) -> Result<RunResult, Error>;
  pub fn upgrade(brew, git, snaps, cache, tap_root, inv: &Inventory, cfg: &Config, names, user_flags, out) -> Result<RunResult, Error>;
  pub fn install(brew, git, snaps, cache, tap_root, inv: &Inventory, cfg: &Config, names, force_cask, force_formula, user_flags, out) -> Result<RunResult, Error>;
  pub fn reinstall(brew, git, snaps, cache, tap_root, inv: &Inventory, cfg: &Config, names, user_flags, out) -> Result<RunResult, Error>;
  pub fn explicit_tap_origins(names: &[String]) -> Vec<String>;   // origins of user/repo/name tokens (for extra_taps)
  ```
  `report.rs`: `Counts { ..., pub no_soak: usize }`, `counts_line` ends with `, no-soak N`, and
  `pub fn origin_line(name: &str, origin: &str, hours: SoakHours, class: PkgClass) -> String` → `"{name}: origin {origin}; soak {hours}h"` or `"{name}: origin {origin}; no-soak"` or `"{name}: origin {origin}; unsoakable"`.
  `brew.rs`: `fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error>` (every installed formula and cask from every tap).

Internal `cmd.rs` shapes the later tasks rely on:

```rust
/// Which snapshot a package resolves against.
fn resolve_pkg_blobs(git: &impl GitStore, snaps: &Snapshots, cache: &Path, origin: &str, name: &str, kind: PkgKind) -> Result<resolve::ResolvedBlobs, Error>;
fn resolve_view(git: &impl GitStore, snaps: &Snapshots, cache: &Path, origin: &str, name: &str, kind: PkgKind, receipt_rb: Option<&str>) -> Result<Option<ResolvedView>, Error>;
fn natural_kind(git: &impl GitStore, snaps: &Snapshots, cache: &Path, inv: &Inventory, origin: &str, name: &str) -> Result<PkgKind, Error>;
/// origin + name + class for a command-line token, installed or not.
struct Resolved { origin: String, name: String, kind_hint: Option<PkgKind>, class: PkgClass, receipt_tap: Option<String>, tapped: bool }
fn resolve_token(raw: &str, inv: &Inventory, cfg: &Config) -> Result<Resolved, Error>;
struct ApplySession<'a, B, G, W> { /* existing fields, plus */ inv: &'a Inventory, cfg: &'a Config, nosoak: Vec<nosoak::Target> }
impl ApplySession { fn run_nosoak(&mut self) -> Result<(), Error>; }
```

- [ ] **Step 1: Write the failing tests**

`src/report.rs`:

```rust
    #[test]
    fn counts_line_ends_with_no_soak() {
        let c = Counts { upgraded: 1, no_soak: 2, ..Counts::default() };
        assert_eq!(counts_line(&c), "upgraded 1, already soaked 0, held 0, ahead 0, pinned 0, skipped 0, no-soak 2");
    }

    #[test]
    fn origin_line_shows_hours_or_class() {
        use crate::inventory::PkgClass;
        let h = crate::SoakHours::new(72).unwrap();
        assert_eq!(origin_line("terraform", "hashicorp/tap", h, PkgClass::Soaked), "terraform: origin hashicorp/tap; soak 72h");
        assert_eq!(origin_line("brewsoak", "ericfitz/tap", h, PkgClass::NoSoak), "brewsoak: origin ericfitz/tap; no-soak");
        assert_eq!(origin_line("x", "local/tap", h, PkgClass::Unsoakable), "x: origin local/tap; unsoakable");
    }
```

`src/brew.rs`: rename the test to `parse_installed_json_keeps_third_party` and expect three packages, `foo` with `tap: Some("acme/tools".into())`; the closure returns `Some("class Foo; end".into())` for `foo`.

`src/cmd.rs` test helpers (add near `core_snaps`):

```rust
    use crate::config::Config;
    use crate::inventory::Inventory;
    use crate::origin::OriginRecords;
    use crate::snapshot::TapState;
    use crate::taps::TapInfo;

    fn cfg24() -> Config {
        Config::uniform(SoakHours::new(24).expect("hours >= 1"))
    }

    fn cfg_with(file: &str) -> Config {
        let p = crate::config::parse_file(file);
        Config { taps: p.taps, no_soak: p.no_soak, ..cfg24() }
    }

    fn inv_from(brew: &MockBrew, cfg: &Config) -> Inventory {
        Inventory::build(brew.installed.clone(), &brew.taps, &OriginRecords::default(), cfg)
    }

    fn tapped(name: &str, remote: Option<&str>) -> TapInfo {
        TapInfo { name: name.into(), remote: remote.map(str::to_string) }
    }

    fn formula_pkg_from(name: &str, tap: &str, receipt_rb: String) -> InstalledPkg {
        InstalledPkg { tap: Some(tap.into()), ..formula_pkg(name, receipt_rb) }
    }

    /// Snapshots with core/cask plus one soaked tap whose cutoff/head trees
    /// hold `Formula/<name>.rb` blobs.
    fn tap_snaps(git: &InMemoryGit, tap: &str, cutoff: Option<&str>, head: &str) -> Snapshots {
        let mut snaps = core_snaps();
        snaps.taps.insert(
            tap.into(),
            TapState {
                hours: SoakHours::new(24).unwrap(),
                cutoff_sha: cutoff.map(str::to_string),
                head_sha: head.into(),
                cutoff_time: None,
            },
        );
        if let Some(c) = cutoff {
            git.insert_tree(c, &["Formula/terraform.rb", "README.md"]);
        }
        git.insert_tree(head, &["Formula/terraform.rb", "README.md"]);
        snaps
    }
```

Every existing test call to `outdated`, `info`, `upgrade`, `install`, `reinstall`, `update` gains the `inv`/`cfg` arguments in the positions shown in Interfaces; use `let cfg = cfg24(); let inv = inv_from(&brew, &cfg);`. `call_reinstall` and `upgrade_both` helpers do the same once.

New tests:

```rust
    #[test]
    fn upgrade_bare_soaks_tap_package_from_its_tap_snapshot() {
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = MockBrew {
            installed: vec![formula_pkg_from("terraform", "hashicorp/tap", formula_rb("terraform", "1.0.0", "oldsha"))],
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(&brew, &git, &snaps, unused_cache(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("upgrading terraform 1.0.0 -> 1.1.0"), "{text}");
        assert!(run_is_soaked_install(&lock_runs(&brew), "terraform"), "{:?}", lock_runs(&brew));
        assert!(!r.refused);
        assert!(!run_has_token(&lock_runs(&brew), "hashicorp/tap/terraform"), "soaked tap packages are never brew tokens");
    }

    #[test]
    fn upgrade_explicit_tap_token_is_soaked_not_passed_through() {
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = MockBrew {
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        install(&brew, &git, &snaps, unused_cache(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut out).unwrap();
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "terraform"), "{runs:?}");
        assert!(!run_has_token(&runs, "hashicorp/tap/terraform"), "{runs:?}");
    }

    #[test]
    fn tap_without_cutoff_holds_its_packages_as_too_new() {
        let git = InMemoryGit::new();
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = MockBrew {
            installed: vec![formula_pkg_from("terraform", "hashicorp/tap", formula_rb("terraform", "1.0.0", "oldsha"))],
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", None, "taphead");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(&brew, &git, &snaps, unused_cache(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert!(text.contains("terraform is too new"), "{text}");
        assert!(lock_runs(&brew).iter().all(|a| a.first().map(String::as_str) != Some("install")), "nothing installed");
    }

    #[test]
    fn held_tap_holds_only_its_packages_with_git_note() {
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", formula_rb("alpha", "1.1.0", "midsha"));
        git.insert_blob("headsha", "Formula/a/alpha.rb", formula_rb("alpha", "1.2.0", "newsha"));
        let brew = MockBrew {
            installed: vec![
                formula_pkg("alpha", formula_rb("alpha", "1.0.0", "oldsha")),
                formula_pkg_from("terraform", "hashicorp/tap", formula_rb("terraform", "1.0.0", "oldsha")),
            ],
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = core_snaps();
        snaps.held_taps.insert("hashicorp/tap".into(), "while fetching history from origin, git failed:\nfatal: boom".into());
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(&brew, &git, &snaps, unused_cache(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(run_is_soaked_install(&lock_runs(&brew), "alpha"), "core package still upgraded");
        assert!(r.refused);
        assert!(text.contains("terraform: tap hashicorp/tap could not be refreshed") && text.contains("fatal: boom"), "{text}");
    }

    #[test]
    fn unsoakable_tap_package_is_noted_in_bare_upgrade_and_refused_when_named() {
        let brew = MockBrew {
            installed: vec![formula_pkg_from("thing", "local/tap", formula_rb("thing", "1.0.0", "oldsha"))],
            taps: vec![tapped("local/tap", None)],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = core_snaps();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(&brew, &git_empty(), &snaps, unused_cache(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!r.refused, "bare upgrade only notes unsoakable packages");
        assert!(text.contains("thing: tap local/tap has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK"), "{text}");
        let mut out = Vec::new();
        let r = upgrade(&brew, &git_empty(), &snaps, unused_cache(), tap.path(), &inv, &cfg, &["local/tap/thing".into()], &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused, "explicit unsoakable is a refusal");
        assert!(text.contains("use `brew upgrade local/tap/thing`"), "{text}");
        assert!(lock_runs(&brew).is_empty());
    }

    fn git_empty() -> InMemoryGit {
        InMemoryGit::new()
    }

    #[test]
    fn explicit_token_for_untapped_tap_is_refused_with_tap_hint() {
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(&brew, &git_empty(), &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &["nobody/tap/x".into()], false, false, &[], &mut out).unwrap();
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("tap nobody/tap is not installed; run brew tap nobody/tap first, or add it to NO_SOAK"), "{text}");
        assert!(lock_runs(&brew).is_empty());
        // No-soak entry: brew handles it (brew auto-taps).
        let cfg = cfg_with("NO_SOAK = [\"nobody/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let r = install(&brew, &git_empty(), &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &["nobody/tap/x".into()], false, false, &[], &mut out).unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        assert_eq!(runs[0], vec!["update".to_string()]);
        assert_eq!(runs[1], vec!["install", "nobody/tap/x"]);
    }

    #[test]
    fn upgrade_runs_soaked_work_first_then_one_update_and_one_upgrade_for_no_soak() {
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", formula_rb("alpha", "1.1.0", "midsha"));
        git.insert_blob("headsha", "Formula/a/alpha.rb", formula_rb("alpha", "1.2.0", "newsha"));
        let brew = MockBrew {
            installed: vec![
                formula_pkg("alpha", formula_rb("alpha", "1.0.0", "oldsha")),
                formula_pkg_from("brewsoak", "ericfitz/tap", formula_rb("brewsoak", "1.0.0", "oldsha")),
                formula_pkg("wget", formula_rb("wget", "1.0.0", "oldsha")),
            ],
            taps: vec![tapped("ericfitz/tap", Some("https://github.com/ericfitz/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\", \"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(&brew, &git, &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs[..1], "alpha"), "soaked first: {runs:?}");
        assert_eq!(runs[1], vec!["update".to_string()]);
        assert_eq!(runs[2], vec!["upgrade", "ericfitz/tap/brewsoak", "wget"]);
        assert_eq!(runs.len(), 3);
        assert!(!r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no-soak 2"), "{text}");
    }

    #[test]
    fn upgrade_without_no_soak_packages_never_runs_brew_update() {
        let (brew, git) = two_outdated_world("");
        let _ = upgrade_both(&brew, &git);
        assert!(!run_has_token(&lock_runs(&brew), "update"));
    }

    #[test]
    fn upgrade_bare_leaves_pinned_no_soak_out_of_brew_tokens() {
        let brew = MockBrew {
            installed: vec![
                formula_pkg_pinned("wget", formula_rb("wget", "1.0.0", "oldsha")),
                formula_pkg("curl", formula_rb("curl", "1.0.0", "oldsha")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"wget\", \"curl\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(&brew, &git_empty(), &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(runs[1], vec!["upgrade", "curl"], "{runs:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("pinned 1") && text.contains("no-soak 1"), "{text}");
        // Explicitly named: handed to brew, which errors as it would by hand.
        let mut out = Vec::new();
        upgrade(&brew, &git_empty(), &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &["wget".into()], &[], &mut out).unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(runs.last().unwrap(), &vec!["upgrade".to_string(), "wget".to_string()]);
    }

    #[test]
    fn no_soak_staged_keg_switches_tap_with_install() {
        let brew = MockBrew {
            installed: vec![InstalledPkg { tap: None, ..formula_pkg("vault", formula_rb("vault", "1.0.0", "oldsha")) }],
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"hashicorp/tap\"]\n");
        let mut origins = OriginRecords::default();
        origins.set(PkgKind::Formula, "vault", "hashicorp/tap");
        let inv = Inventory::build(brew.installed.clone(), &brew.taps, &origins, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(&brew, &git_empty(), &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(runs[1], vec!["install", "hashicorp/tap/vault"], "{runs:?}");
    }

    #[test]
    fn staged_package_with_lost_origin_record_notes_unknown_origin() {
        // vault was staged from a tap (receipt tap null); origins.toml was lost;
        // the name resolves nowhere in core or cask.
        let brew = MockBrew {
            installed: vec![InstalledPkg { tap: None, ..formula_pkg("vault", formula_rb("vault", "1.0.0", "v")) }],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(&brew, &git_empty(), &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert!(text.contains("vault: origin unknown; reinstall it from its tap with brew"), "{text}");
        assert!(!text.contains("vault is missing at HEAD"), "{text}");
        assert!(text.contains("held 1"), "{text}");
    }

    #[test]
    fn reinstall_no_soak_goes_to_brew_reinstall_after_update() {
        let brew = MockBrew {
            installed: vec![formula_pkg_from("brewsoak", "ericfitz/tap", formula_rb("brewsoak", "1.0.0", "b"))],
            taps: vec![tapped("ericfitz/tap", Some("https://github.com/ericfitz/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let r = reinstall(&brew, &git_empty(), &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &["brewsoak".into()], &[], &mut Vec::new()).unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        assert_eq!(runs, vec![vec!["update".to_string()], vec!["reinstall".to_string(), "ericfitz/tap/brewsoak".to_string()]]);
    }

    #[test]
    fn reinstall_true_repair_of_tap_package_uses_full_token() {
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = MockBrew {
            installed: vec![formula_pkg_from("terraform", "hashicorp/tap", formula_rb("terraform", "1.2.0", "newsha"))],
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let tap = tempfile::tempdir().unwrap();
        reinstall(&brew, &git, &snaps, unused_cache(), tap.path(), &inv, &cfg, &["terraform".into()], &[], &mut Vec::new()).unwrap();
        assert_eq!(lock_runs(&brew), vec![vec!["reinstall".to_string(), "hashicorp/tap/terraform".to_string()]]);
    }

    #[test]
    fn explicit_tap_token_over_a_core_keg_is_refused_not_switched() {
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = MockBrew {
            installed: vec![formula_pkg_from("terraform", "homebrew/core", formula_rb("terraform", "1.0.0", "oldsha"))],
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let tap = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(&brew, &git, &snaps, cache.path(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut out).unwrap();
        assert!(r.refused);
        assert!(String::from_utf8(out).unwrap().contains("terraform is installed from homebrew/core; brewsoak does not switch taps; use brew"));
        assert!(lock_runs(&brew).is_empty());
        assert_eq!(OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"), None);
    }

    #[test]
    fn fresh_cask_install_honors_no_soak_for_homebrew_cask() {
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/f/firefox.rb", cask_rb("firefox", "1.0"));
        git.insert_blob("caskhead", "Casks/f/firefox.rb", cask_rb("firefox", "1.0"));
        let brew = MockBrew::new();
        let cfg = cfg_with("NO_SOAK = [\"homebrew/cask\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        install(&brew, &git, &core_snaps(), unused_cache(), tap.path(), &inv, &cfg, &["firefox".into()], false, false, &[], &mut Vec::new()).unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(runs, vec![vec!["update".to_string()], vec!["install".to_string(), "firefox".to_string()]], "{runs:?}");
    }

    #[test]
    fn tap_package_refusal_hint_uses_full_token() {
        let git = InMemoryGit::new();
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = MockBrew { taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))], ..MockBrew::new() };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", None, "taphead");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        install(&brew, &git, &snaps, unused_cache(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("use `brew install hashicorp/tap/terraform` to bypass"));
    }

    #[test]
    fn verbose_prints_origin_and_hours_per_package() {
        let (brew, git, snaps) = view_world();
        let cfg = cfg_with("[[TAP]]\nname = \"homebrew/core\"\nsoak_hours = 48\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(&brew, &git, &snaps, unused_cache(), &inv, &cfg, &["-v".into()], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("alpha: origin homebrew/core; soak 48h"), "{text}");
    }
```

`src/cmd.rs` tests to delete or rewrite: `upgrade_third_party_passthrough` (delete; replaced above), `info_mixed_third_party_passthrough` and `outdated_mixed_third_party_passthrough` (rewrite as: a named `local/tap/thing` in `info` prints `origin: local/tap` and `unsoakable`; `outdated` prints the unsoakable note; neither calls `brew.run_visible`).

`src/lib.rs` tests: `info_third_party_is_exec_without_refresh` becomes `info_untapped_token_is_soak_aware_not_exec`: dispatch `info acme/tools/foo` → `Dispatch::Exit(1)` (refused: tap not installed) and no `Exec`. `outdated_third_party_is_exec_without_refresh` becomes `outdated_ignores_names_and_does_not_exec`: `outdated acme/tools/foo` → `Dispatch::Exit(0)`. Add:

```rust
    #[test]
    fn ensure_snapshots_refreshes_only_needed_taps() {
        let world = TestWorld::new();
        world.git.insert_commits("https://github.com/hashicorp/homebrew-tap", &[("hc2", now().unix_timestamp() - 3600), ("hc1", now().unix_timestamp() - 200 * 3600)]);
        let brew = MockBrew {
            installed: vec![InstalledPkg { name: "terraform".into(), kind: PkgKind::Formula, receipt_rb: "class X < Formula\n  url \"https://e.com/terraform-1.0.0.tar.gz\"\n  sha256 \"a\"\nend\n".into(), pinned: false, tap: Some("hashicorp/tap".into()) }],
            taps: vec![
                TapInfo { name: "hashicorp/tap".into(), remote: Some("https://github.com/hashicorp/homebrew-tap".into()) },
                TapInfo { name: "ericfitz/tap".into(), remote: Some("https://github.com/ericfitz/homebrew-tap".into()) },
            ],
            ..MockBrew::new()
        };
        let world = TestWorld { brew, ..world };
        std::fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
        std::fs::write(world.config_path(), "[[TAP]]\nname = \"hashicorp/tap\"\nsoak_hours = 100\n").unwrap();
        match dispatch(&s(&["update"]), &world).expect("update") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        let state = std::fs::read_to_string(world.cache_path().join("state.toml")).unwrap();
        assert!(state.contains("[taps.\"hashicorp/tap\"]"), "{state}");
        assert!(state.contains("hours = 100"), "{state}");
        assert!(!state.contains("ericfitz/tap"), "no installed soaked package: not refreshed\n{state}");
    }
```

(`TestWorld` fields are private to the test module, so the struct-update syntax works there; add `use crate::brew::InstalledPkg; use crate::resolve::PkgKind; use crate::taps::TapInfo;` to the lib test imports.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test 2>&1 | head -40`
Expected: compile errors across `cmd.rs`, `lib.rs`, `report.rs` (new params, missing fields).

- [ ] **Step 3: Write the implementation**

`src/brew.rs`: rename `installed_core` → `installed_packages` in the trait, both impls, `inventory.rs`, and every `cmd.rs` call (`rg -n installed_core src`). Delete `keep_tap` and its two `if !keep_tap(...) { continue; }` guards; keep the `tap` normalization from Task 3.

`src/report.rs`:

```rust
pub fn counts_line(c: &Counts) -> String {
    format!(
        "upgraded {}, already soaked {}, held {}, ahead {}, pinned {}, skipped {}, no-soak {}",
        c.upgraded, c.soaked, c.held, c.ahead, c.pinned, c.skipped, c.no_soak
    )
}

pub fn origin_line(name: &str, origin: &str, hours: SoakHours, class: PkgClass) -> String {
    match class {
        PkgClass::Soaked => format!("{name}: origin {origin}; soak {}h", hours.get()),
        PkgClass::NoSoak => format!("{name}: origin {origin}; no-soak"),
        PkgClass::Unsoakable => format!("{name}: origin {origin}; unsoakable"),
    }
}
```

(add `pub no_soak: usize` to `Counts`; `nothing_to_do` also requires `self.no_soak == 0`).

`src/cmd.rs` core pieces:

```rust
use crate::config::Config;
use crate::inventory::{self, Inventory, PkgClass};
use crate::nosoak;
use crate::origin;
use crate::snapshot::{self, RefreshPlan, Snapshots, TapPlan};
use crate::taps;

pub fn explicit_tap_origins(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter_map(|n| inventory::parse_token(n).ok().and_then(|t| t.origin))
        .collect()
}

pub fn refresh_plan(cfg: &Config, inv: &Inventory, extra_taps: &[String]) -> RefreshPlan {
    RefreshPlan {
        hours: cfg.hours,
        core_hours: cfg.effective_hours(origin::CORE),
        cask_hours: cfg.effective_hours(origin::CASK),
        taps: inv
            .needed_taps(extra_taps)
            .into_iter()
            .map(|(name, remote)| TapPlan { hours: cfg.effective_hours(&name), name, remote })
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn ensure_snapshots(
    git: &impl GitStore,
    gh: &impl GithubApi,
    brew: &impl Brew,
    cache: &Path,
    cfg: &Config,
    inv: &Inventory,
    extra_taps: &[String],
    now: time::OffsetDateTime,
    force: bool,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    let plan = refresh_plan(cfg, inv, extra_taps);
    let snaps = if force {
        snapshot::refresh_with(git, gh, cache, &plan, now, progress)?
    } else {
        match snapshot::load_state(cache)? {
            Some(mut s) => {
                // outdated/info: reuse, but refresh taps with no state or other hours.
                let stale: Vec<TapPlan> = plan
                    .taps
                    .iter()
                    .filter(|t| snapshot::tap_needs_refresh(&s, &t.name, t.hours))
                    .cloned()
                    .collect();
                if !stale.is_empty() {
                    snapshot::refresh_taps(git, cache, &mut s, &stale, now, progress)?;
                }
                return Ok(s);
            }
            None => snapshot::refresh_with(git, gh, cache, &plan, now, progress)?,
        }
    };
    prefetch_installed(brew, git, cache, inv, &snaps);
    Ok(snaps)
}

fn resolve_pkg_blobs(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    origin_tap: &str,
    name: &str,
    kind: PkgKind,
) -> Result<resolve::ResolvedBlobs, Error> {
    if origin::is_core_or_cask(origin_tap) {
        let repo = tap_repo(cache, kind);
        let tap = match kind {
            PkgKind::Formula => &snaps.core,
            PkgKind::Cask => &snaps.cask,
        };
        let pkg = PkgRef { name: name.to_string(), kind };
        return resolve::resolve_blobs(git, &repo, &tap.cutoff_sha, &tap.head_sha, &pkg);
    }
    let Some(state) = snaps.tap(origin_tap) else {
        return Ok(resolve::ResolvedBlobs { cutoff: None, head: None });
    };
    let dir = taps::clone_dir(cache, origin_tap);
    let show_at = |sha: &str| -> Result<Option<Vec<u8>>, Error> {
        let tree = git.ls_tree(&dir, sha)?;
        match taps::resolve_path(&tree, kind, name) {
            Some(path) => git.show(&dir, sha, &path),
            None => Ok(None),
        }
    };
    let cutoff = match state.cutoff_sha.as_deref() {
        Some(sha) => show_at(sha)?,
        None => None,
    };
    let head = show_at(&state.head_sha)?;
    Ok(resolve::ResolvedBlobs { cutoff, head })
}
```

`resolve_view`, `cutoff_blob_exists`, `write_cutoff_blob` gain `origin_tap: &str` before `name` and forward it. `natural_kind(git, snaps, cache, inv, origin_tap, name)`: `inv.find(name)` with matching origin → its kind; else formula if either blob resolves at that origin, else cask. `resolve_named(git, snaps, cache, inv, cfg, raw)` parses the token via `resolve_token` and uses the installed receipt when present. `prefetch_installed(brew, git, cache, inv, snaps)` iterates `inv.pkgs` with `class == Soaked`. `classify_installed`, `write_update_summary`, `plan_size` iterate `inv.pkgs` filtered to `Soaked` and resolve with `p.origin`.

Token resolution:

```rust
struct Resolved {
    origin: String,
    name: String,
    class: PkgClass,
    receipt_tap: Option<String>,
    /// origin is core/cask or appears in `brew tap-info --installed`.
    tapped: bool,
}

fn resolve_token(raw: &str, inv: &Inventory, cfg: &Config) -> Result<Resolved, Error> {
    let token = inventory::parse_token(raw)?;
    let installed = inv.find(&token.name).filter(|p| token.origin.as_deref().is_none_or(|o| o == p.origin));
    let origin_tap = match (&token.origin, installed) {
        (Some(o), _) => o.clone(),
        (None, Some(p)) => p.origin.clone(),
        (None, None) => origin::CORE.to_string(), // not installed, bare name: core; natural_kind may flip to cask
    };
    let tapped = origin::is_core_or_cask(&origin_tap) || inv.tap_class(&origin_tap).is_some();
    let class = match installed {
        Some(p) if token.origin.is_none() => p.class,
        _ => inv.class_for(&origin_tap, &token.name, cfg),
    };
    Ok(Resolved {
        origin: origin_tap,
        name: token.name,
        class,
        receipt_tap: installed.and_then(|p| p.receipt_tap.clone()),
        tapped,
    })
}
```

(For a bare name that is not installed and resolves as a cask, `resolve_kind`/`natural_kind` pick `PkgKind::Cask` and the origin becomes `homebrew/cask`; apply that swap in `apply_one` right after `resolve_kind`: `if origin::is_core_or_cask(&r.origin) { r.origin = origin::default_origin(kind).to_string(); }`.)

`apply_one` head (replaces the `is_third_party` branch):

```rust
    fn apply_one(&mut self, raw: &str) -> Result<(), Error> {
        let mut r = resolve_token(raw, self.inv, self.cfg)?;
        let name = r.name.clone();
        let pinned = self.inv.find(&name).is_some_and(|p| p.pinned);
        let bare_run = self.names_were_empty;
        if pinned && self.brew_verb == "upgrade" && (bare_run || r.class != PkgClass::NoSoak) {
            self.counts.pinned += 1;
            if is_verbose(self.user_flags) {
                writeln!(self.out, "{name}: pinned; skipped")?;
            }
            return Ok(());
        }
        if is_verbose(self.user_flags) {
            writeln!(self.out, "{}", report::origin_line(&name, &r.origin, self.cfg.effective_hours(&r.origin), r.class))?;
        }
        match r.class {
            PkgClass::NoSoak => {
                self.nosoak.push(nosoak::Target {
                    switch_tap: r.receipt_tap.is_none() && !origin::is_core_or_cask(&r.origin) && self.inv.find(&name).is_some(),
                    origin: r.origin,
                    name,
                });
                return Ok(());
            }
            PkgClass::Unsoakable => {
                let token = nosoak::brew_token(&r.origin, &name);
                let msg = if r.tapped {
                    format!("{name}: tap {} has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK", r.origin)
                } else {
                    format!("{name}: tap {} is not installed; run brew tap {} first, or add it to NO_SOAK", r.origin, r.origin)
                };
                if bare_run {
                    self.defer(msg);
                } else {
                    self.defer(format!("{msg}; use `brew {} {token}` to bypass brewsoak.", self.brew_verb));
                    self.counts.held += 1;
                    self.refused = true;
                }
                return Ok(());
            }
            PkgClass::Soaked => {}
        }
        if let Some(err) = self.snaps.held_taps.get(&r.origin) {
            self.defer(format!("{name}: tap {} could not be refreshed; held. {err}", r.origin));
            self.counts.held += 1;
            self.refused = true;
            return Ok(());
        }
        let kind = self.resolve_kind(&r.origin, &name)?;
        if origin::is_core_or_cask(&r.origin) {
            r.origin = origin::default_origin(kind).to_string();
        }
        let receipt = self.inv.find(&name).filter(|p| p.origin == r.origin).map(|p| p.receipt_rb.as_str());
        let Some(view) = resolve_view(self.git, self.snaps, self.cache, &r.origin, &name, kind, receipt)? else {
            // unchanged from here on: unparseable identity, warnings, desired action ...
```

`ApplySession` gains `names_were_empty: bool`, `inv`, `cfg`, `nosoak: Vec<nosoak::Target>`. `apply_many` gains a `bare_run: bool` parameter right after `names` and stores it in `names_were_empty`; `upgrade` passes `names.is_empty()`, `install` passes `false`, and `reinstall` sets the field to `false` directly.

Origin-unknown note (spec "Package origin", last bullet): right after `resolve_view` returns a view, add

```rust
        if view.action == DesiredAction::RefuseYanked
            && origin::is_core_or_cask(&r.origin)
            && self.inv.find(&name).is_some_and(|p| p.receipt_tap.is_none())
        {
            self.defer(format!("{name}: origin unknown; reinstall it from its tap with brew"));
            self.counts.held += 1;
            self.refused = true;
            return Ok(());
        }
```

(This fires only for an installed package whose receipt tap is `None` and whose name resolves at neither core nor cask: the staged-then-lost-record case. API-mode core receipts also report `None`, but a real core formula resolves at HEAD, so it never reaches `RefuseYanked` with both blobs missing. A package whose receipt tap names `homebrew/core` keeps the plain yanked refusal.) The rest of `apply_one` is unchanged except `install_cutoff(name, &r.origin, kind, &view)` (Task 10 uses the origin; in this task it still writes to `self.tap_root` and installs as before).

`reinstall` loop body: resolve the token first, then branch on class before the v1 true-repair check:

```rust
    for raw in names {
        let r = resolve_token(raw, &inv, cfg)?;
        match r.class {
            PkgClass::NoSoak => {
                session.nosoak.push(nosoak::Target { origin: r.origin, name: r.name, switch_tap: false });
                continue;
            }
            PkgClass::Unsoakable => {
                session.apply_one(raw)?; // produces the refusal
                continue;
            }
            PkgClass::Soaked => {}
        }
        let Some(pkg) = inv.find(&r.name).filter(|p| p.origin == r.origin) else {
            return Err(Error::Refusal(format!("reinstall: no installed keg: {}", r.name)));
        };
        let Some(view) = resolve_view(git, snaps, cache, &r.origin, &r.name, pkg.kind, Some(&pkg.receipt_rb))? else {
            writeln!(session.out, "{}: unparseable identity; skipping", r.name)?;
            continue;
        };
        if view.installed.as_ref() == view.head.as_ref() {
            // true repair: brew reinstall with the full token so a tap formula resolves in its tap
            let token = nosoak::brew_token(&r.origin, &r.name);
            writeln!(session.out, "reinstalling {}", r.name)?;
            let mut args = vec!["reinstall".to_string()];
            args.extend(user_flags.iter().cloned());
            args.push(token);
            session.record_run(&args)?;
            session.counts.upgraded += 1;
            continue;
        }
        session.apply_one(raw)?;
    }
    session.run_nosoak()?;
```

Explicit tap token over a keg from another origin: in `apply_one`, right after `resolve_token`, if the token named an origin and `self.inv.find(&name)` exists with a different `origin`, defer `"{name} is installed from {other}; brewsoak does not switch taps; use brew"`, `counts.held += 1`, `refused = true`, and return (never uninstall a core keg to switch taps).

Class after the kind swap: a bare not-installed name starts as `homebrew/core`; after `resolve_kind` flips it to `homebrew/cask`, recompute `r.class = self.inv.class_for(&r.origin, &name, self.cfg)` and, if it is now `NoSoak`, push the `nosoak::Target` and return (so `NO_SOAK = ["homebrew/cask"]` covers a fresh cask install).

Bypass hints: `refusal_message(view.action, &nosoak::brew_token(&r.origin, &name), self.brew_verb)` and, in `refuse_ineligible_dep`, print the target as its full token, so a tap package's hint is `brew install hashicorp/tap/terraform`, not a bare name brew would resolve against core.

After the loop in `apply_many` and `reinstall` (before `write_tail`):

```rust
    fn run_nosoak(&mut self) -> Result<(), Error> {
        let targets = std::mem::take(&mut self.nosoak);
        let step = nosoak::run_step(self.brew, self.brew_verb, self.user_flags, &targets, self.out)?;
        self.counts.no_soak += step.count;
        self.done.extend(step.installed);
        for note in step.notes {
            self.defer(note);
        }
        if let Some(code) = step.status {
            self.brew_status = Some(self.brew_status.map_or(code, |p| p.max(code)));
        }
        Ok(())
    }
```

`outdated` and `info`: iterate `inv.pkgs` (or the named tokens via `resolve_token`); `NoSoak` → under `-v` print the origin line and `"{name}: no-soak; brew decides"`, count separately (Task 11 adds the `brew outdated` listing); `Unsoakable` → push the "has no HTTPS remote" note into `held` for `outdated` and print `origin: <tap>` + `action: unsoakable` for `info`; held tap → `held.push(format!("{name}: tap {origin} could not be refreshed; {err}"))`. `info` long form prints `origin: {origin}` and `soak hours: {N}` (or `no-soak`) after the `head:` line. Remove `passthrough_third_party` and the `resolve::is_third_party` branches in `info`, `reinstall`, `apply_one`.

`src/lib.rs` `dispatch`: for every soaked command build

```rust
            let cache = world.cache_path();
            let inv = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
```

then `cmd::ensure_snapshots(world.git(), world.github(), world.brew(), &cache, &cfg, &inv, &cmd::explicit_tap_origins(&names), world.now(), force, &mut out)` (for `Outdated`, `extra_taps` is `&[]`; for `Info`/`Upgrade`/`Install`/`Reinstall` it is the explicit tokens' origins), and pass `&inv, &cfg` to each `cmd::*` call. `Update` calls `cmd::update(world.brew(), world.git(), world.github(), &cache, &cfg, &inv, world.now(), verbose, &mut out)`, which builds its plan with `refresh_plan(cfg, inv, &[])`. Delete `third_party_only_names` and the two `is_third_party` early-returns.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: all pass, including every pre-existing `cmd.rs` test with the new arguments.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/brew.rs src/report.rs src/cmd.rs src/lib.rs src/inventory.rs
git commit -m "feat: soak third-party taps, refuse unsoakable ones, and batch no-soak packages to brew

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 10: Soaked tap install flow: per-tap staging, origin records, cross-origin dep closure, staged-load hold

**Files:**
- Modify: `src/cmd.rs` (`ApplySession::install_cutoff`, `install_missing_dep`, `collect_cutoff_deps`, `CutoffDepWalk`, `record_run`, new `record_origin`, new `dep_origin`)
- Modify: `src/tap.rs:41-88` (`dep_closure` is unused after this task: delete it and its tests; keep `write_blob`, `sanitize_unofficial`, `brew_install_args`)
- Test: `src/cmd.rs` tests

**Interfaces:**
- Consumes: `taps::{staging_root, staged_load_failure}`, `origin::OriginRecords`, `inventory::PkgClass`, `nosoak::brew_token`.
- Produces (internal to `cmd.rs`):
  ```rust
  /// (origin, name) a `brew deps` token resolves to, or None when brew keeps it.
  fn dep_origin(&self, dep: &str, dependent_origin: &str) -> Result<Option<(String, String)>, Error>;
  struct CutoffDepWalk { /* + */ inv: &Inventory, cfg: &Config, out: Vec<(String /*origin*/, String, PkgKind)> }
  impl ApplySession { fn record_origin(&mut self, origin: &str, name: &str, kind: PkgKind) -> Result<(), Error>; }
  ```
  `ApplySession` gains `origins: OriginRecords` (loaded in `apply_many`/`reinstall` from `cache`).

Rules (spec "Soaked tap packages"):
- Stage at `taps::staging_root(tap_root, origin)` + `Formula/<name>.rb` or `Casks/<name>.rb` (core/cask unchanged).
- After a successful target or dep install (status 0, or already-installed → 0): tap origin → `origins.set`; core/cask → `origins.remove`; then `origins.save(cache)`.
- Dep tokens from `brew deps --1 <staged path>`: `user/tap/dep` → that tap; bare `dep` → the dependent's own tap if it resolves at that tap's cutoff tree, else core, else cask, else `None` (left to brew). A dep whose class is `NoSoak` or `Unsoakable` is left to brew. A dep in a held tap refuses the target (same path as an ineligible dep, with the git note).
- Tap-origin install whose brew output matches `taps::staged_load_failure` (and nonzero exit): hold with `"{name}: cannot be installed from a staged copy; use brew, or add it to NO_SOAK"`, `counts.upgraded -= 1; counts.held += 1; refused = true`, and do not merge brew's status.
- `brew.deps` returning `Error::Brew` for a tap-origin staged path: same hold (spec issue 4). Core/cask `deps` errors propagate as before.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn soaked_tap_install_stages_under_tap_dir_and_writes_origin_record() {
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = MockBrew {
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        install(&brew, &git, &snaps, cache.path(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut out).unwrap();
        let staged = tap.path().join("taps/hashicorp/tap/Formula/terraform.rb");
        assert!(staged.exists(), "staged under the per-tap directory");
        assert!(!tap.path().join("Formula/terraform.rb").exists(), "never under the core staging root");
        let runs = lock_runs(&brew);
        assert!(runs.iter().any(|a| a.iter().any(|x| x == staged.to_str().unwrap())), "{runs:?}");
        let records = OriginRecords::load(cache.path());
        assert_eq!(records.get(PkgKind::Formula, "terraform"), Some("hashicorp/tap"));
    }

    #[test]
    fn core_install_removes_a_stale_origin_record() {
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", formula_rb("alpha", "1.1.0", "midsha"));
        git.insert_blob("headsha", "Formula/a/alpha.rb", formula_rb("alpha", "1.2.0", "newsha"));
        let cache = tempfile::tempdir().unwrap();
        let mut records = OriginRecords::default();
        records.set(PkgKind::Formula, "alpha", "old/tap");
        records.save(cache.path()).unwrap();
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        install(&brew, &git, &core_snaps(), cache.path(), tap.path(), &inv, &cfg, &["alpha".into()], false, false, &[], &mut Vec::new()).unwrap();
        assert_eq!(OriginRecords::load(cache.path()).get(PkgKind::Formula, "alpha"), None);
    }

    #[test]
    fn dep_closure_crosses_taps_and_core() {
        // terraform (hashicorp/tap) depends on bare `helper` (same tap) and `zlib` (core);
        // helper depends on `acme/tools/widget` (another soakable tap).
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        git.insert_blob("tapcut", "Formula/helper.rb", formula_rb("helper", "0.1.0", "h1"));
        git.insert_blob("taphead", "Formula/helper.rb", formula_rb("helper", "0.1.0", "h1"));
        git.insert_blob("acmecut", "Formula/widget.rb", formula_rb("widget", "2.0.0", "w1"));
        git.insert_blob("acmehead", "Formula/widget.rb", formula_rb("widget", "2.0.0", "w1"));
        git.insert_tree("acmecut", &["Formula/widget.rb"]);
        git.insert_tree("acmehead", &["Formula/widget.rb"]);
        git.insert_blob("cutoffsha", "Formula/z/zlib.rb", formula_rb("zlib", "1.3", "z1"));
        git.insert_blob("headsha", "Formula/z/zlib.rb", formula_rb("zlib", "1.3", "z1"));
        let tap = tempfile::tempdir().unwrap();
        let staged = |p: &str| tap.path().join(p).to_string_lossy().into_owned();
        let mut deps = BTreeMap::new();
        deps.insert(staged("taps/hashicorp/tap/Formula/terraform.rb"), vec!["helper".to_string(), "zlib".to_string()]);
        deps.insert(staged("taps/hashicorp/tap/Formula/helper.rb"), vec!["acme/tools/widget".to_string()]);
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap")),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/terraform.rb", "Formula/helper.rb"]);
        git.insert_tree("taphead", &["Formula/terraform.rb", "Formula/helper.rb"]);
        snaps.taps.insert("acme/tools".into(), TapState { hours: SoakHours::new(24).unwrap(), cutoff_sha: Some("acmecut".into()), head_sha: "acmehead".into(), cutoff_time: None });
        let cache = tempfile::tempdir().unwrap();
        install(&brew, &git, &snaps, cache.path(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut Vec::new()).unwrap();
        let runs = lock_runs(&brew);
        let installs: Vec<&str> = runs
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .map(|a| a.last().unwrap().as_str())
            .collect();
        assert_eq!(
            installs,
            vec![
                staged("taps/acme/tools/Formula/widget.rb"),
                staged("taps/hashicorp/tap/Formula/helper.rb"),
                staged("Formula/zlib.rb"),
                staged("taps/hashicorp/tap/Formula/terraform.rb"),
            ],
            "deps first, each from its own origin's staging dir: {runs:?}"
        );
        let records = OriginRecords::load(cache.path());
        assert_eq!(records.get(PkgKind::Formula, "widget"), Some("acme/tools"));
        assert_eq!(records.get(PkgKind::Formula, "helper"), Some("hashicorp/tap"));
        assert_eq!(records.get(PkgKind::Formula, "zlib"), None);
    }

    #[test]
    fn no_soak_and_unsoakable_deps_are_left_to_brew() {
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path().join("taps/hashicorp/tap/Formula/terraform.rb").to_string_lossy().into_owned(),
            vec!["ericfitz/tap/brewsoak".to_string(), "local/tap/thing".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap")),
                tapped("ericfitz/tap", Some("https://github.com/ericfitz/homebrew-tap")),
                tapped("local/tap", None),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let r = install(&brew, &git, &snaps, cache.path(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut Vec::new()).unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        let installs: Vec<&Vec<String>> = runs.iter().filter(|a| a.first().map(String::as_str) == Some("install")).collect();
        assert_eq!(installs.len(), 1, "only the target: {runs:?}");
        assert!(!run_has_token(&runs, "ericfitz/tap/brewsoak"));
    }

    #[test]
    fn staged_load_failure_holds_with_note_and_does_not_fail_the_run() {
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = MockBrew {
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            next_status: 1,
            next_stdout: b"Error: cannot load such file -- ../lib/helper\n".to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(&brew, &git, &snaps, cache.path(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert_eq!(r.brew_status, None, "a staged-load hold is not a brew failure");
        assert!(text.contains("terraform: cannot be installed from a staged copy; use brew, or add it to NO_SOAK"), "{text}");
        assert!(text.contains("held 1"), "{text}");
        assert_eq!(OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"), None);
    }

    #[test]
    fn deps_failure_on_staged_tap_formula_holds_with_note() {
        struct FailDepsBrew(MockBrew);
        impl Brew for FailDepsBrew {
            fn brew_bin(&self) -> &Path { self.0.brew_bin() }
            fn run(&self, a: &[String]) -> Result<std::process::Output, Error> { self.0.run(a) }
            fn run_visible(&self, a: &[String]) -> Result<std::process::Output, Error> { self.0.run_visible(a) }
            fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> { self.0.installed_packages() }
            fn tap_new_soaked(&self) -> Result<(), Error> { self.0.tap_new_soaked() }
            fn tap_info(&self) -> Result<Vec<TapInfo>, Error> { self.0.tap_info() }
            fn deps(&self, _k: PkgKind, token: &str) -> Result<Vec<String>, Error> {
                Err(Error::Brew { status: 1, message: format!("Error: No available formula with the name \"helper\" ({token})") })
            }
        }
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let brew = FailDepsBrew(MockBrew {
            taps: vec![tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap"))],
            ..MockBrew::new()
        });
        let cfg = cfg24();
        let inv = inv_from(&brew.0, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(&brew, &git, &snaps, cache.path(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut out).unwrap();
        assert!(r.refused);
        assert!(String::from_utf8(out).unwrap().contains("cannot be installed from a staged copy"));
        assert!(lock_runs(&brew.0).iter().all(|a| a.first().map(String::as_str) != Some("install")));
    }

    #[test]
    fn dep_in_held_tap_refuses_the_target() {
        let git = InMemoryGit::new();
        git.insert_blob("tapcut", "Formula/terraform.rb", formula_rb("terraform", "1.1.0", "midsha"));
        git.insert_blob("taphead", "Formula/terraform.rb", formula_rb("terraform", "1.2.0", "newsha"));
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path().join("taps/hashicorp/tap/Formula/terraform.rb").to_string_lossy().into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap")),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        snaps.held_taps.insert("acme/tools".into(), "while fetching history from origin, git failed:\nfatal: boom".into());
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(&brew, &git, &snaps, cache.path(), tap.path(), &inv, &cfg, &["hashicorp/tap/terraform".into()], false, false, &[], &mut out).unwrap();
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("cannot install terraform: dependency widget") && text.contains("acme/tools could not be refreshed"), "{text}");
    }
```

Also update the existing `install_uses_cutoff_tap_deps_not_head_graph` and `install_cask_installs_formula_dep` expectations only if they assert on the staged path; core staging paths are unchanged, so they should pass as-is.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test cmd::tests::soaked_tap_install ; cargo test cmd::tests::dep_closure_crosses`
Expected: FAIL (staged at the core root; no origin record; deps resolved only against core).

- [ ] **Step 3: Write the implementation**

`ApplySession` additions and the rewritten install path:

```rust
    fn staging_root(&self, origin_tap: &str) -> PathBuf {
        taps::staging_root(self.tap_root, origin_tap)
    }

    fn record_origin(&mut self, origin_tap: &str, name: &str, kind: PkgKind) -> Result<(), Error> {
        if origin::is_core_or_cask(origin_tap) {
            self.origins.remove(kind, name);
        } else {
            self.origins.set(kind, name, origin_tap);
        }
        self.origins.save(self.cache)
    }

    /// Runs one staged install. Returns false when the package was held
    /// because brew could not load the staged copy.
    fn record_staged_install(&mut self, origin_tap: &str, name: &str, kind: PkgKind, args: &[String]) -> Result<bool, Error> {
        let output = self.brew.run_visible(args)?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success()
            && !origin::is_core_or_cask(origin_tap)
            && taps::staged_load_failure(&text)
        {
            self.hold_staged_copy(name);
            return Ok(false);
        }
        self.done.extend(quiet::installed_from_output(&output.stdout));
        // Only a real install proves the keg came from this origin; brew's
        // "already installed" answer says nothing about where the keg came from.
        let installed_now = output.status.success();
        merge_status(&mut self.brew_status, output);
        if installed_now {
            self.record_origin(origin_tap, name, kind)?;
        }
        Ok(true)
    }

    fn hold_staged_copy(&mut self, name: &str) {
        self.defer(format!("{name}: cannot be installed from a staged copy; use brew, or add it to NO_SOAK"));
        self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
        self.counts.held += 1;
        self.refused = true;
    }

    fn install_cutoff(&mut self, name: &str, origin_tap: &str, kind: PkgKind, view: &ResolvedView) -> Result<(), Error> {
        let pkg = PkgRef { name: name.to_string(), kind };
        let blob = view.cutoff_blob.as_deref().ok_or_else(|| Error::Other(format!("{name} is eligible but the cutoff blob is missing")))?;
        let root = self.staging_root(origin_tap);
        let path = tap::write_blob(&root, &pkg, blob)?;

        let deps = match self.collect_cutoff_deps(origin_tap, name, kind) {
            Ok(d) => d,
            Err(Error::Brew { .. }) if !origin::is_core_or_cask(origin_tap) => {
                self.hold_staged_copy(name);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for (dep_origin, dep, dep_kind) in deps {
            if self.inv.find(&dep).is_some_and(|p| p.origin == dep_origin) {
                continue;
            }
            if !self.install_missing_dep(name, &dep_origin, dep_kind, &dep)? {
                return Ok(());
            }
        }
        let args = tap::brew_install_args(&pkg, &path, self.user_flags);
        self.record_staged_install(origin_tap, name, kind, &args)?;
        Ok(())
    }
```

`install_missing_dep(target, dep_origin, kind, dep)`: resolve with `dep_origin`; if `self.snaps.held_taps.get(dep_origin)` is `Some(err)`, write `cannot install {target}: dependency {dep} is in tap {dep_origin}, which could not be refreshed; {err}; use brew ...` with `refused = true` and return `Ok(false)`; else as before but stage at `self.staging_root(dep_origin)` and run through `record_staged_install(dep_origin, dep, kind, &args)` (ignore its bool: a dep load failure holds the dep by name and the target install is then attempted; brew reports the missing dep). `refuse_ineligible_dep` is unchanged.

`CutoffDepWalk` gains `inv`, `cfg`, `out: Vec<(String, String, PkgKind)>`; `visit(origin_tap, name, kind, include_self)` stages at `taps::staging_root(self.tap_root, origin_tap)` via `write_cutoff_blob(..., origin_tap, ...)`, then for each `dep` from `brew.deps(kind, staged_path)`:

```rust
            let Some((dep_origin, dep_name)) = self.dep_origin(&dep, origin_tap)? else { continue };
            if self.inv.class_for(&dep_origin, &dep_name, self.cfg) != PkgClass::Soaked {
                continue; // no-soak or unsoakable deps are brew's
            }
            let dep_kind = natural_kind(self.git, self.snaps, self.cache, self.inv, &dep_origin, &dep_name)?;
            self.visit(&dep_origin, &dep_name, dep_kind, true)?;
```

with

```rust
    fn dep_origin(&self, dep: &str, dependent_origin: &str) -> Result<Option<(String, String)>, Error> {
        if let Ok(tok) = inventory::parse_token(dep)
            && let Some(o) = tok.origin
        {
            return Ok(Some((o, tok.name)));
        }
        if !origin::is_core_or_cask(dependent_origin)
            && self.snaps.held_taps.get(dependent_origin).is_none()
            && cutoff_blob_exists(self.git, self.snaps, self.cache, dependent_origin, dep, PkgKind::Formula)?
        {
            return Ok(Some((dependent_origin.to_string(), dep.to_string())));
        }
        if cutoff_blob_exists(self.git, self.snaps, self.cache, origin::CORE, dep, PkgKind::Formula)? {
            return Ok(Some((origin::CORE.to_string(), dep.to_string())));
        }
        if cutoff_blob_exists(self.git, self.snaps, self.cache, origin::CASK, dep, PkgKind::Cask)? {
            return Ok(Some((origin::CASK.to_string(), dep.to_string())));
        }
        Ok(None)
    }
```

A dep whose origin tap appears in `held_taps` is returned as-is; `install_missing_dep` turns it into the refusal (`dep_in_held_tap_refuses_the_target`). Delete `cutoff_in_either_tree` (unused). Delete `tap::dep_closure`, `ClosureWalk`, and their four tests.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/cmd.rs src/tap.rs
git commit -m "feat: stage soaked tap packages per tap, record origins, and walk deps across origins

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 11: `update`, `outdated`, `info` for no-soak packages; summary golden

**Files:**
- Modify: `src/brew.rs:30-44` (`Brew::outdated_names`), `MockBrew.outdated: Vec<String>`
- Modify: `src/cmd.rs` (`update`, `outdated`, `info`)
- Create: `tests/fixtures/upgrade_summary.txt` (blessed by the golden test)
- Test: `src/cmd.rs` tests, `src/brew.rs` test `mock_outdated_names`

**Interfaces:**
- Consumes: `inventory::Inventory::any_no_soak`, `nosoak::brew_token`, brew JSON helpers.
- Produces: `Brew::outdated_names(&self) -> Result<Vec<String>, Error>` — names (formula `name`, cask `name`) from `brew outdated --json=v2` with `HOMEBREW_NO_AUTO_UPDATE=1` (no `brew update`). `MockBrew` returns `self.outdated.clone()`.

Behavior:
- `brewsoak update`: after `refresh_with` and the summary, if `inv.any_no_soak()`: print `no-soak packages installed; updating brew`, run `brew update` (visible, once); nonzero → `brew update failed (exit N)` on `out` and return `Err(Error::Brew { status, message })` after the snapshot work is done (snapshots are already written).
- `outdated`: new section `==> No-soak (brew)` listing `name (no-soak, brew)` for each installed no-soak package whose name is in `brew.outdated_names()`; always printed (`write_section_always`). `held` gains unsoakable notes and held-tap lines (from Task 9). "nothing outdated" also requires the no-soak section empty.
- `info`: long form prints `origin: <tap>` then `soak hours: N` or `soak: no-soak (brew decides)`; `action:` for a no-soak package is `no-soak`; compact line for no-soak is `"{name}  {inst}  no-soak (brew)"`.
- Golden: `upgrade_summary_golden` in `cmd.rs` tests runs a bare `upgrade` over a fixed world (one soaked core upgrade, one no-soak tap package, one unsoakable, one held tap, one pinned) with `MockBrew` output queued for the staged install, and compares the full `out` text to `tests/fixtures/upgrade_summary.txt`; `BREWSOAK_BLESS=1` rewrites the fixture.

- [ ] **Step 1: Write the failing tests**

`src/brew.rs`:

```rust
    #[test]
    fn mock_outdated_names() {
        let brew = MockBrew { outdated: vec!["brewsoak".into()], ..MockBrew::new() };
        assert_eq!(brew.outdated_names().unwrap(), vec!["brewsoak".to_string()]);
    }

    #[test]
    fn parse_outdated_json_reads_formula_and_cask_names() {
        let json = r#"{"formulae": [{"name": "brewsoak", "installed_versions": ["1.0"], "current_version": "1.1"}], "casks": [{"name": "firefox"}]}"#;
        assert_eq!(parse_outdated_json(json).unwrap(), vec!["brewsoak".to_string(), "firefox".to_string()]);
    }
```

`src/cmd.rs`:

```rust
    #[test]
    fn update_runs_brew_update_once_only_when_a_no_soak_package_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", formula_rb("wget", "1.0.0", "a"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        update(&brew, &git, &fixture_gh(), dir.path(), &cfg, &inv, now(), false, &mut Vec::new()).unwrap();
        assert!(!run_has_token(&lock_runs(&brew), "update"), "no no-soak package: no brew update");
        let cfg = cfg_with("NO_SOAK = [\"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        update(&brew, &git, &fixture_gh(), dir.path(), &cfg, &inv, now(), false, &mut out).unwrap();
        let updates = lock_runs(&brew).iter().filter(|a| a == &&vec!["update".to_string()]).count();
        assert_eq!(updates, 1);
        assert!(String::from_utf8(out).unwrap().contains("updating brew"));
    }

    #[test]
    fn outdated_lists_no_soak_from_brews_view_without_updating() {
        let (mut brew, git, snaps) = view_world();
        brew.installed.push(formula_pkg_from("brewsoak", "ericfitz/tap", formula_rb("brewsoak", "1.0.0", "b")));
        brew.taps.push(tapped("ericfitz/tap", Some("https://github.com/ericfitz/homebrew-tap")));
        brew.outdated = vec!["brewsoak".into(), "alpha".into()];
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(&brew, &git, &snaps, unused_cache(), &inv, &cfg, &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("==> No-soak (brew)\nbrewsoak (no-soak, brew)"), "{text}");
        assert!(!text.contains("alpha (no-soak"), "soaked packages are not listed from brew's view: {text}");
        assert!(!run_has_token(&lock_runs(&brew), "update"));
    }

    #[test]
    fn info_shows_origin_hours_and_no_soak() {
        let (mut brew, git, snaps) = view_world();
        brew.installed.push(formula_pkg_from("brewsoak", "ericfitz/tap", formula_rb("brewsoak", "1.0.0", "b")));
        brew.taps.push(tapped("ericfitz/tap", Some("https://github.com/ericfitz/homebrew-tap")));
        let cfg = cfg_with("[[TAP]]\nname = \"homebrew/core\"\nsoak_hours = 48\nNO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        info(&brew, &git, &snaps, unused_cache(), &inv, &cfg, &["alpha".into(), "brewsoak".into()], &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("alpha\ninstalled: 1.0.0\ncutoff: 1.1.0\nhead: 1.2.0\norigin: homebrew/core\nsoak hours: 48\naction: would upgrade"), "{text}");
        assert!(text.contains("brewsoak\ninstalled: 1.0.0\n") && text.contains("origin: ericfitz/tap\nsoak: no-soak (brew decides)\naction: no-soak"), "{text}");
    }

    #[test]
    fn upgrade_summary_golden() {
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", formula_rb("alpha", "1.1.0", "midsha"));
        git.insert_blob("headsha", "Formula/a/alpha.rb", formula_rb("alpha", "1.2.0", "newsha"));
        let brew = MockBrew {
            installed: vec![
                formula_pkg("alpha", formula_rb("alpha", "1.0.0", "oldsha")),
                formula_pkg_from("brewsoak", "ericfitz/tap", formula_rb("brewsoak", "1.0.0", "b")),
                formula_pkg_from("thing", "local/tap", formula_rb("thing", "1.0.0", "t")),
                formula_pkg_from("terraform", "hashicorp/tap", formula_rb("terraform", "1.0.0", "t")),
                formula_pkg_pinned("curl", formula_rb("curl", "1.0.0", "c")),
            ],
            taps: vec![
                tapped("ericfitz/tap", Some("https://github.com/ericfitz/homebrew-tap")),
                tapped("local/tap", None),
                tapped("hashicorp/tap", Some("https://github.com/hashicorp/homebrew-tap")),
            ],
            next_stdout: b"\xf0\x9f\x8d\xba  /opt/homebrew/Cellar/alpha/1.1.0: 5 files, 1MB\n".to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut snaps = core_snaps();
        snaps.held_taps.insert("hashicorp/tap".into(), "while fetching history from origin to find the tap's soak cutoff, git failed:\nfatal: unable to access".into());
        let tap = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(&brew, &git, &snaps, cache.path(), tap.path(), &inv, &cfg, &[], &[], &mut out).unwrap();
        let got = String::from_utf8(out).unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upgrade_summary.txt");
        if std::env::var_os("BREWSOAK_BLESS").is_some() {
            std::fs::write(&path, &got).unwrap();
        }
        let want = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(got, want, "upgrade summary changed; rerun with BREWSOAK_BLESS=1 if intended");
        assert!(got.contains("no-soak 1"), "{got}");
        assert!(got.contains("notes:"), "{got}");
        assert!(got.contains("thing: tap local/tap has no HTTPS remote"), "{got}");
        assert!(got.contains("terraform: tap hashicorp/tap could not be refreshed"), "{got}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test outdated_names ; cargo test cmd::tests::update_runs_brew_update ; cargo test cmd::tests::upgrade_summary_golden`
Expected: compile errors (`outdated`, `outdated_names` missing) / golden mismatch.

- [ ] **Step 3: Write the implementation**

`src/brew.rs`:

```rust
    /// Names brew itself considers outdated right now (no `brew update`).
    fn outdated_names(&self) -> Result<Vec<String>, Error>;
```

```rust
    // ProcessBrew
    fn outdated_names(&self) -> Result<Vec<String>, Error> {
        let output = self.run(&["outdated".into(), "--json=v2".into()])?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        parse_outdated_json(&String::from_utf8_lossy(&output.stdout))
    }

pub(crate) fn parse_outdated_json(json: &str) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    for key in ["formulae", "casks"] {
        for obj in json_objects_in_array(json, key)? {
            if let Some(name) = json_string_value(obj, "name").filter(|n| !n.is_empty()) {
                out.push(name);
            }
        }
    }
    Ok(out)
}
```

`MockBrew`: `pub outdated: Vec<String>` (default empty), `fn outdated_names(&self) -> Result<Vec<String>, Error> { Ok(self.outdated.clone()) }`. The `FailDepsBrew` test double from Task 10 forwards `outdated_names` to its inner mock.

`src/cmd.rs` `update` (after `write_update_summary`, before `snapshots refreshed`):

```rust
    if inv.any_no_soak() {
        writeln!(out, "no-soak packages installed; updating brew")?;
        let output = brew.run_visible(&["update".to_string()])?;
        if !output.status.success() {
            let status = output.status.code().unwrap_or(1);
            writeln!(out, "brew update failed (exit {status})")?;
            writeln!(out, "snapshots refreshed")?;
            return Err(Error::Brew { status, message: String::new() });
        }
    }
```

(`lib.rs` already maps `Error::Brew` from a soaked command via `soaked_exit`; route `Update` through `soaked_exit(cmd::update(...).map(|()| cmd::RunResult { refused: false, brew_status: None }))`.)

`outdated`: after the installed loop,

```rust
    let brew_outdated = if no_soak_names.is_empty() { Vec::new() } else { brew.outdated_names()? };
    let no_soak: Vec<String> = no_soak_names
        .iter()
        .filter(|n| brew_outdated.iter().any(|o| o == *n))
        .map(|n| format!("{n} (no-soak, brew)"))
        .collect();
    write_section_always(out, "==> No-soak (brew)", &no_soak)?;
```

where `no_soak_names` collects `p.name` for `p.class == PkgClass::NoSoak && !p.pinned`.

`info` long form, after `head:`:

```rust
            writeln!(out, "origin: {}", r.origin)?;
            match r.class {
                PkgClass::NoSoak => writeln!(out, "soak: no-soak (brew decides)")?,
                _ => writeln!(out, "soak hours: {}", cfg.effective_hours(&r.origin).get())?,
            }
            writeln!(out, "action: {}", match r.class {
                PkgClass::NoSoak => "no-soak",
                PkgClass::Unsoakable => "unsoakable",
                PkgClass::Soaked => report::human_action(view.action),
            })?;
```

Run the golden once with `BREWSOAK_BLESS=1 cargo test cmd::tests::upgrade_summary_golden`, read `tests/fixtures/upgrade_summary.txt`, and check by eye that it shows: `upgrading 1 of 5 packages`, `[1/1] upgrading alpha 1.0.0 -> 1.1.0`, `no-soak: updating brew`, `no-soak: brew upgrade ericfitz/tap/brewsoak`, a counts line ending `pinned 1, skipped 0, no-soak 1`, and a `notes:` block with the unsoakable and held-tap lines. Then run it again without the env var.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/brew.rs src/cmd.rs src/lib.rs tests/fixtures/upgrade_summary.txt
git commit -m "feat: update/outdated/info know about no-soak packages; golden upgrade summary

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 12: Docs and help text

**Files:**
- Modify: `README.md:1-60` (intro, install note, configuration), plus a new "Third-party taps" section after Configuration
- Modify: `AGENTS.md` (new "Origins and staging" section)
- Modify: `src/cli.rs:59-84` (`help_text`), `src/cli.rs:257-338` (`command_help` for `update`, `upgrade`, `install`, `outdated`, `info`)
- Test: `src/cli.rs` tests (add `help_mentions_no_soak`)

**Interfaces:**
- Consumes: nothing new. Produces: documentation only.

- [ ] **Step 1: Write the failing test** (append to `src/cli.rs` tests)

```rust
    #[test]
    fn help_mentions_no_soak_and_taps() {
        assert!(help_text().contains("NO_SOAK"));
        assert!(command_help("upgrade").unwrap().contains("no-soak"));
        assert!(!command_help("upgrade").unwrap().contains("passed through to brew"));
        assert!(command_help("update").unwrap().contains("brew update"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test cli::tests::help_mentions_no_soak_and_taps`
Expected: FAIL.

- [ ] **Step 3: Write the docs**

`src/cli.rs` `help_text` line 3: `A Homebrew wrapper that delays core, cask, and third-party tap updates for a soak window.` Replace the `Other brew commands are passed through unchanged.` line with:

```
Other brew commands are passed through unchanged. Packages and taps listed
under NO_SOAK in ~/.config/brewsoak/config.toml skip soaking and go to brew.
```

`command_help("update")`: replace `Does not update the Homebrew tool itself.` with `Runs brew update once when any installed package is no-soak.` and `Refresh soak snapshots for homebrew-core and homebrew-cask.` with `Refresh soak snapshots for homebrew-core, homebrew-cask, and every soaked tap.`. `command_help("upgrade")`: replace `Third-party tap tokens are passed through to brew.` with:

```
Third-party tap packages are soaked from brewsoak's own tap clones.
NO_SOAK packages are handed to brew after the soaked work (one brew update,
then one brew upgrade); their outdated dependencies go with them.
Taps without an HTTPS remote are not soakable and are noted, not upgraded.
```

`install`/`outdated`/`info`: add one line each: `user/repo/name tokens are soaked (or no-soak) like any other package.` For `info`: `Shows origin tap and effective soak hours; no-soak packages are marked.`

`README.md`:
- Line 3-5: `A Homebrew wrapper that delays \`homebrew/core\`, \`homebrew/cask\`, and third-party tap updates for a soak window. ...`
- Line 7: `Every other \`brew\` subcommand passes through unchanged. Packages and taps you list under \`NO_SOAK\` skip soaking and end in the same state as \`brew upgrade\`.`
- Lines 20-21: replace `(brewsoak itself lives in a third-party tap, so it is never soaked)` with `. Add \`ericfitz/tap\` to \`NO_SOAK\` (below) so \`brewsoak upgrade\` keeps brewsoak itself current without a soak delay.`
- After the Configuration table and precedence paragraph, add:

````markdown
### Per-tap soak hours and the no-soak list

```toml
SOAK_HOURS = 48                  # default for every package, core and cask included

[[TAP]]
name = "hashicorp/tap"
soak_hours = 72                  # optional; applies to every package in this tap

[[TAP]]
name = "cyclonedx/cyclonedx"     # no soak_hours: uses SOAK_HOURS

NO_SOAK = ["ericfitz/tap", "wget", "hashicorp/tap/terraform"]
```

Effective soak hours for a package: a `NO_SOAK` match means no soak at all;
else the origin tap's `[[TAP]]` `soak_hours`; else `SOAK_HOURS`. `[[TAP]]` and
`NO_SOAK` are file-only (no flag, no environment variable). `homebrew/core`
and `homebrew/cask` are valid tap names in both.

`NO_SOAK` entries: `wget` (that formula or cask from any tap), `ericfitz/tap`
(every package in that tap), `hashicorp/tap/terraform` (one package from one
tap). Matching is case-insensitive. Invalid entries are skipped and reported
under `-v`.

`--soak-hours N` edits only the `SOAK_HOURS` key; `[[TAP]]` and `NO_SOAK`
are kept. A config file that is not valid TOML is left alone with a warning.

### Third-party taps

Every installed tap with an HTTPS remote is soaked like core and cask, from a
blobless clone under brewsoak's cache (brew's own tap checkouts are never
touched). A tap with no remote, or a non-HTTPS remote, is not soakable:
brewsoak notes its packages and leaves them to `brew`, unless they are in
`NO_SOAK`.

No-soak packages are installed by `brew` itself from their real tap after all
soaked work: one `brew update`, then one `brew upgrade` (or `install` /
`reinstall`) with every no-soak package named. `brew upgrade` also upgrades a
no-soak package's outdated dependencies to brew's latest, so no-soak extends
to its dependencies. Soaked dependents of a no-soak package are not upgraded
by that step.
````

`AGENTS.md`, new section after "Installing soaked formulae":

```markdown
## Origins and staging

- A package's origin is the receipt `tap` from `brew info --json=v2 --installed` when it is non-empty and not `brewsoakr/soaked`; else `<cache>/origins.toml` (`"formula:<name>" = "user/repo"`); else `homebrew/core` / `homebrew/cask`. A staged install leaves the receipt tap null, so write the origin record after every successful staged tap install and remove it after a staged core/cask install.
- Tap packages are staged under `<cache>/staging/taps/<user>/<repo>/{Formula,Casks}/<name>.rb`; core and cask stay directly under `<cache>/staging/`. Never let a tap formula land in the core staging root.
- Tap history lives in `<cache>/taps/<user>/<repo>.git`: a bare clone with a named `origin` remote, fetched `--filter=blob:none` (retry without the filter if the server rejects it). Never fetch into, or move the checkout of, anything under `$(brew --repository)/Library/Taps`. No GitHub API calls for taps.
- `NO_SOAK` packages are never staged. They go to brew as full tokens (`user/repo/name` for tap packages) in one `brew update` + one `brew upgrade|install|reinstall` after all soaked work. A no-soak package whose keg was staged by brewsoak is moved to its real tap with `brew install user/repo/name`.
- `brew tap-info --json --installed` reports `remote: null` for API-mode `homebrew/core` and `homebrew/cask` and for the staging tap; classify those by name before applying the "no HTTPS remote = unsoakable" rule.
- A tap whose fetch fails holds only that tap's packages (`Error::Git` note). Core or cask fetch failure still aborts the run.
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test cli::`
Expected: all pass.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add README.md AGENTS.md src/cli.rs
git commit -m "docs: tap soaking, per-tap hours, and the NO_SOAK list

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Rcuu4euuYuG1X6LhFf8b4F"
```

---

### Task 13: Manual verification against real brew (human / orchestrator)

This task needs the maintainer's machine and real Homebrew. It is not a subagent task; the orchestrator runs it with the human, non-destructive steps first. Nothing here is committed except a fix-up commit if a step exposes a bug (then go back through the owning task's TDD cycle).

**Files:**
- None created. Read-only on the repo unless a bug is found.

**Interfaces:**
- Consumes: the release binary `target/release/brewsoak` (`cargo build --release`).

Preparation (non-destructive):

- [ ] **Step 1: Back up config and cache**

```bash
cp ~/.config/brewsoak/config.toml /private/tmp/claude-501/brewsoak-config.bak 2>/dev/null || true
cp ~/Library/Caches/brewsoak/state.toml /private/tmp/claude-501/brewsoak-state.bak 2>/dev/null || true
cargo build --release
```

- [ ] **Step 2: Inventory and classification (read-only)**

```bash
cat > ~/.config/brewsoak/config.toml <<'EOF'
SOAK_HOURS = 24
[[TAP]]
name = "hashicorp/tap"
soak_hours = 72
NO_SOAK = ["ericfitz/tap"]
EOF
target/release/brewsoak update -v
```
Expected: `fetching hashicorp/tap…` (and every other installed HTTPS tap with a soaked package: anthropics/tap, cyclonedx/cyclonedx, daveshanley/vacuum, endava/tap, heroku/brew as applicable), no fetch for `ericfitz/tap` (no-soak), `brewsoakr/soaked` ignored, `no-soak packages installed; updating brew` followed by brew's update output, and `state.toml` contains `[taps."hashicorp/tap"]` with `hours = 72`. Verify no clone was created or fetched under `$(brew --repository)/Library/Taps` (`ls -la $(brew --repository)/Library/Taps/hashicorp/homebrew-tap/.git/FETCH_HEAD` timestamp unchanged). Confirm `endava/tap` packages resolve (root-level formula) with `target/release/brewsoak info endava/tap/<name>` showing `origin: endava/tap`.

- [ ] **Step 3: `outdated` and `info` (read-only)**

```bash
target/release/brewsoak outdated -v
target/release/brewsoak info brewsoak
target/release/brewsoak info hashicorp/tap/terraform
```
Expected: `brewsoak` under `==> No-soak (brew)` only if brew says it is outdated; `-v` prints `<name>: origin <tap>; soak Nh` for every package; `info brewsoak` shows `origin: ericfitz/tap` and `soak: no-soak (brew decides)`; `info hashicorp/tap/terraform` shows `soak hours: 72`.

Mutating steps (each one reversible with plain `brew`):

- [ ] **Step 4: NO_SOAK keeps brewsoak current (spec manual check 1)**

Pick a brewsoak version that is not the latest if one is installed (`brew list --versions brewsoak`), else skip the downgrade. Run `target/release/brewsoak upgrade -v`. Expected: soaked work first, then `no-soak: updating brew`, then `no-soak: brew upgrade ericfitz/tap/brewsoak`, counts line ends with `no-soak 1`, and `brew info brewsoak` reports the latest tap version. Exit code 0.

- [ ] **Step 5: A soaked `hashicorp/tap` package installs from its staged copy (spec manual check 2)**

Choose a small hashicorp formula not yet installed (e.g. `hashicorp/tap/consul` or `hashicorp/tap/packer`). Run `target/release/brewsoak install hashicorp/tap/packer -v`. Expected: `packer: origin hashicorp/tap; soak 72h`, a `brew install --formula ~/Library/Caches/brewsoak/staging/taps/hashicorp/tap/Formula/packer.rb` run (visible in the `full brew log` path printed at the end), `origins.toml` gains `"formula:packer" = "hashicorp/tap"`, and `brew info --json=v2 packer | jq '.formulae[0].tap'` is `null`. If brew refuses to load the staged copy, expect the hold note `packer: cannot be installed from a staged copy; use brew, or add it to NO_SOAK` and exit 1, and record which formula and why in the task report.

- [ ] **Step 6: Tap switch replaces the keg cleanly (spec manual check 3; spec issue 5)**

With `packer` installed from Step 5, add `hashicorp/tap/packer` to `NO_SOAK` and run `target/release/brewsoak upgrade -v`. Expected: `no-soak: brew install hashicorp/tap/packer (moving staged kegs to their tap)`; afterwards `brew info --json=v2 packer | jq '.formulae[0].tap'` is `"hashicorp/tap"` and `brew list --versions packer` shows one keg. If brew instead printed "already installed" and the note `packer: brew did not replace the staged keg; run brew reinstall hashicorp/tap/packer` appeared with exit 1, run `brew reinstall hashicorp/tap/packer` and record the outcome: the spec-issue-5 fallback then becomes a code change (switch via `reinstall`) through Task 8's TDD cycle.

- [ ] **Step 7: Failure isolation (read-only)**

Temporarily point one tap clone at a dead remote: `git --git-dir ~/Library/Caches/brewsoak/taps/hashicorp/tap.git remote set-url origin https://127.0.0.1:9/nope` and run `target/release/brewsoak update`. Expected: `hashicorp/tap` packages held with the `Error::Git` note (`while fetching history from origin ...`), other taps and core/cask refreshed, exit 1 on `upgrade` (a hold is a refusal), not an abort. brewsoak restores the remote on the next run (it calls `set-url` from tap-info), so just run `target/release/brewsoak update` again.

- [ ] **Step 8: Clean up**

```bash
brew uninstall packer   # if installed in Step 5/6 and not wanted
cp /private/tmp/claude-501/brewsoak-config.bak ~/.config/brewsoak/config.toml 2>/dev/null || true
```

Report: which steps passed, the full brew log paths, and any bug found (with the task that owns the fix).

---

## Self-review notes

- **Spec coverage.** Configuration (Task 2), effective hours (Task 2), `[[TAP]]`/`NO_SOAK` rules (Tasks 1–2), persisting `--soak-hours` (Task 2), package origin + `origins.toml` (Tasks 3, 10), tap classification (Task 5), which taps refresh (Tasks 7, 9), clone/cutoff/pins/no-GitHub (Tasks 4, 6), no cutoff commit (Tasks 6, 9), name resolution (Task 5), state (Task 6), failure isolation (Tasks 6, 9, 10), inventory (Tasks 7, 9), soaked tap packages incl. staging dir / origin record / dep closure / staged-load hold (Task 10), no-soak packages 1–6 (Tasks 8, 9, 11), explicit names (Task 9), `info`/`outdated` (Task 11), output (Tasks 9, 11), exit status (unchanged; Tasks 8–9 merge brew status), testing list (each bullet has a named test above), manual verification (Task 13), docs (Task 12). The "origin unknown; reinstall it from its tap with brew" note is the origin-unknown branch in Task 9 (`staged_package_with_lost_origin_record_notes_unknown_origin`).
- **Type consistency.** `Config::effective_hours(&str) -> SoakHours`, `Inventory::class_for(&str, &str, &Config) -> PkgClass`, `nosoak::Target { origin, name, switch_tap }`, `snapshot::TapPlan { name, remote, hours }`, `Snapshots::tap(&str) -> Option<&TapState>`, `taps::staging_root(&Path, &str) -> PathBuf`, `GitStore::rev_list_before -> Result<Option<(String, i64)>, Error>` are used with the same shapes in every task.
- **Review Focus.** All five items have named tests: Task 9 (`upgrade_bare_leaves_pinned_no_soak_out_of_brew_tokens`, `explicit_token_for_untapped_tap_is_refused_with_tap_hint`, `tap_without_cutoff_holds_its_packages_as_too_new`), Task 10 (`deps_failure_on_staged_tap_formula_holds_with_note`), Task 8 (`switch_install_that_did_not_replace_keg_keeps_status_and_notes`).
