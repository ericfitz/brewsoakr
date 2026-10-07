# Greedy Casks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A bare `brewsoak upgrade` / `brewsoak outdated` honors `--greedy`, `--greedy-auto-updates` and `--greedy-latest` (and brew's `-g`) with brew's meanings, while `version :latest` casks keep the soak promise.

**Architecture:** `src/flags.rs` gains a `Greedy { auto_updates, latest }` mode read from the brew args brewsoak already forwards. `bare_action` in `src/cmd.rs` takes that mode: the auto-updates leave is lifted when `greedy.auto_updates` is set. A `:latest` cask changes nothing in the action table (its cutoff-vs-installed identity already decides); under `greedy.latest` a `:latest` cask whose identity matches gets a note that its contents cannot be soaked. `plan_size`, `apply_resolved` and the `outdated` report all go through the same two functions (`bare_action`, `latest_left`), so the `[i/N]` total, the upgrade and the report agree, and a test pins that agreement under every flag.

**Tech Stack:** Rust edition 2024, no new dependencies. Fakes: `MockBrew`, `InMemoryGit` (unit tests in `src/cmd.rs`).

**Spec:** `docs/superpowers/specs/2026-10-06-greedy-casks-design.md` (issue #3). The spec's "Human decisions" section is frozen: a `:latest` cask is reinstalled only when its cutoff cask definition differs from the installed one; when they match, brewsoak leaves it and says the contents of a `:latest` cask cannot be soaked.

## Decisions the spec leaves open (made in this plan)

1. **No new `DesiredAction` variant.** A `:latest` cask whose identity matches stays `NoOpAlreadySoaked` (it is at its cutoff definition) and counts as "already soaked". The note is carried by a `ResolvedView.latest` flag plus `latest_left(view, greedy)`, not by the action enum.
2. **Where the note surfaces.** `upgrade`: a deferred line in the `notes:` block (visible without `-v`, like `ahead_message`), and the `-v` evaluate line's `did` text. `outdated`: with `-v` only, one line after the evaluate line (the spec says so for `outdated`).
3. **`-g` means greedy** on `upgrade` and `outdated` (brew's own short there). On an upgrade run the greedy flags are brewsoak's to read: they are removed from what reaches `brew install <file>.rb` (previously `-g` was forwarded, where brew install reads it as `--git`) and no longer appear in the "brew install does not accept ...; dropped from staged installs" note. The `flags::filter_for_verb` table itself is unchanged.
4. **`brewsoak update`'s summary** (`classify_installed`) keeps bare semantics (`Greedy::default()`): `brew update` takes no greedy flags and `cmd::update` receives only `is_verbose`.
5. **Named upgrades** (`brewsoak upgrade alt-tab --greedy`) never go through `bare_action`; greedy flags change nothing there (today's behavior), and one assertion pins it.
6. **`latest` is read from the cutoff identity**, not HEAD or the installed one: the cutoff is what brewsoak would install, so a cask whose cutoff moved from `:latest` to a numbered version soaks and upgrades normally. (`auto_updates` keeps reading HEAD, as today.)

Out of scope, listed for the orchestrator: `brew.outdated_names()` takes no flags, so a no-soak self-updating cask does not appear under `brewsoak outdated --greedy` (no-soak packages are brew's; the no-soak `brew upgrade` already forwards `--greedy*` through the UPGRADE table in `src/nosoak.rs`).

## Global Constraints

- Done gate after every task and at the end: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`. All four must be clean before the commit step.
- `plan_size`, `apply_resolved` and the `outdated` report must agree (the comment in `plan_size` says so); Task 5 pins that agreement under each flag.
- The install-path filter that drops `--greedy*` from `brew install <file>.rb` (`flags::filter_for_verb("install", ...)`, `tap::brew_install_args`) is unchanged.
- The three existing bare-run tests (`bare_upgrade_total_leaves_auto_updates_casks_out`, `auto_updates_cask_is_left_alone_on_bare_upgrade_but_upgraded_when_named`, `outdated_lists_auto_updates_cask_in_its_own_section`) must pass unmodified.
- Every byte of `brew` output still goes to the per-run log; nothing here touches the summarizer.
- `-v`/`--verbose` prints a line for every package evaluated; new notes follow the existing `report::evaluate_line` / `ApplySession::defer` paths.
- American English in code, comments and docs ("honor", not "honour"). Rust edition 2024; match surrounding style; `rg` with an explicit path.
- Work on branch `issue-3-greedy-casks`; stage only the files named in each commit step; never `git add -A`; no `git stash`. The orchestrator squashes into one commit on main.
- Assume issues #4 (dead `tap_new_soaked` code) and #1 (settings command) landed first: anchor every edit on a function name, never a line number in `src/cmd.rs`.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy
  ```

## Review Focus

1. `-g` inside a short cluster (`brewsoak upgrade -vg`): expected to mean `--greedy` on upgrade/outdated and never reach `brew install` as `--git`. Pinned by `greedy_mode_reads_g_in_a_cluster` and `without_greedy_strips_the_flags_and_the_letter` (Task 1) and `staged_install_drops_flags_brew_install_rejects_and_says_so_once` (Task 4).
2. A cask that is both `version :latest` and `auto_updates true`, under `--greedy-latest` alone: expected to stay left to the app (the auto-updates leave is not lifted). Pinned by `greedy_latest_leaves_self_updating_casks_and_notes_latest` (Task 4).
3. `--greedy` on a named upgrade (`brewsoak upgrade nightly --greedy`): expected to be a no-op for a `:latest` cask at its cutoff, with no note. Pinned by `named_upgrade_ignores_greedy_and_prints_no_latest_note` (Task 4).
4. A cutoff that moved from `:latest` to a numbered version: expected to upgrade like any cask and never carry the `:latest` note. Pinned by `resolve_view_reads_latest_from_the_cutoff_cask` and `latest_left_only_under_greedy_latest_with_a_matching_cutoff` (both Task 3).
5. The dropped-flags note after `brewsoak upgrade --greedy`: expected not to claim `--greedy` was dropped (it was honored). Pinned by `staged_install_drops_flags_brew_install_rejects_and_says_so_once` (Task 4).

## File map

- `src/flags.rs`: `Greedy`, `greedy_mode`, `without_greedy` (new, with tests).
- `src/identity.rs`: `PkgIdentity::is_latest_cask` (new, with test).
- `src/cmd.rs`: `ResolvedView.latest`; `bare_action(view, greedy)`; `latest_left`, `latest_note`; `plan_size(.., greedy)`; `note_dropped_flags(verb, flags, context)`; `apply_resolved`, `outdated`, `classify_installed`, `upgrade` callers; tests near `auto_updates_world`.
- `src/cli.rs`: `command_help` text for `upgrade` and `outdated`; `help_mentions_no_soak_and_taps` test.
- `README.md`: the auto-updates paragraph under "Example" and the flags paragraph under "Commands".

---

### Task 1: Greedy mode in `src/flags.rs`

**Files:**
- Modify: `src/flags.rs` (add after `pub fn is_dry_run`; tests at the end of `mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `pub struct Greedy { pub auto_updates: bool, pub latest: bool }` (`Debug, Clone, Copy, Default, PartialEq, Eq`)
  - `pub fn greedy_mode(flags: &[String]) -> Greedy`
  - `pub fn without_greedy(flags: &[String]) -> Vec<String>`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/flags.rs`:

```rust
    #[test]
    fn greedy_mode_reads_each_flag_with_brew_meaning() {
        assert_eq!(greedy_mode(&[]), Greedy::default());
        assert_eq!(
            greedy_mode(&f(&["--greedy"])),
            Greedy {
                auto_updates: true,
                latest: true
            }
        );
        assert_eq!(
            greedy_mode(&f(&["--greedy-auto-updates"])),
            Greedy {
                auto_updates: true,
                latest: false
            }
        );
        assert_eq!(
            greedy_mode(&f(&["--greedy-latest"])),
            Greedy {
                auto_updates: false,
                latest: true
            }
        );
        assert_eq!(
            greedy_mode(&f(&["--greedy-latest", "--greedy-auto-updates"])),
            Greedy {
                auto_updates: true,
                latest: true
            }
        );
    }

    #[test]
    fn greedy_mode_reads_g_in_a_cluster() {
        let both = Greedy {
            auto_updates: true,
            latest: true,
        };
        assert_eq!(greedy_mode(&f(&["-g"])), both);
        assert_eq!(greedy_mode(&f(&["-vg"])), both);
        assert_eq!(greedy_mode(&f(&["-v"])), Greedy::default());
    }

    #[test]
    fn greedy_mode_ignores_lookalikes() {
        // `filter_for_verb` drops a boolean flag given a value; so do we.
        assert_eq!(greedy_mode(&f(&["--greedy=yes"])), Greedy::default());
        assert_eq!(greedy_mode(&f(&["--greedy-foo"])), Greedy::default());
        assert_eq!(greedy_mode(&f(&["--g"])), Greedy::default());
        assert_eq!(greedy_mode(&f(&["greedy"])), Greedy::default());
    }

    #[test]
    fn without_greedy_strips_the_flags_and_the_letter() {
        let got = without_greedy(&f(&[
            "--greedy",
            "--verbose",
            "--greedy-latest",
            "-vg",
            "-g",
            "--greedy-auto-updates",
            "--greedy=yes",
        ]));
        assert_eq!(got, f(&["--verbose", "-v", "--greedy=yes"]));
        assert_eq!(without_greedy(&f(&["-v", "--force"])), f(&["-v", "--force"]));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib flags::tests 2>&1 | tail -20`
Expected: compile error, `cannot find function greedy_mode` / `cannot find type Greedy`.

- [ ] **Step 3: Implement**

Add to `src/flags.rs` directly after `pub fn is_dry_run`:

```rust
/// Which self-updating casks a bare `upgrade`/`outdated` may touch. brew's
/// `--greedy` covers both kinds; the two long forms cover one each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Greedy {
    /// `--greedy` / `--greedy-auto-updates`: casks with `auto_updates true`.
    pub auto_updates: bool,
    /// `--greedy` / `--greedy-latest`: casks with `version :latest`.
    pub latest: bool,
}

/// Read the greedy flags brew's `upgrade` and `outdated` take. `-g`, alone
/// or in a short cluster (`-vg`), is `--greedy` on those verbs (brew's own
/// short; on `install` and `reinstall` the same letter is `--git`, so only
/// upgrade and outdated call this).
pub fn greedy_mode(flags: &[String]) -> Greedy {
    let mut mode = Greedy::default();
    for f in flags {
        match f.as_str() {
            "--greedy" => {
                mode.auto_updates = true;
                mode.latest = true;
            }
            "--greedy-auto-updates" => mode.auto_updates = true,
            "--greedy-latest" => mode.latest = true,
            s if is_short_cluster(s) && s[1..].contains('g') => {
                mode.auto_updates = true;
                mode.latest = true;
            }
            _ => {}
        }
    }
    mode
}

/// `flags` without the greedy flags brewsoak consumed on an upgrade run:
/// the three long forms, and `g` removed from short clusters (a cluster
/// that was only `-g` disappears). What is left is what `brew install
/// <file>.rb` may see; the `install` table filter still applies after.
pub fn without_greedy(flags: &[String]) -> Vec<String> {
    flags
        .iter()
        .filter_map(|f| match f.as_str() {
            "--greedy" | "--greedy-auto-updates" | "--greedy-latest" => None,
            s if is_short_cluster(s) => {
                let kept: String = s[1..].chars().filter(|c| *c != 'g').collect();
                (!kept.is_empty()).then(|| format!("-{kept}"))
            }
            _ => Some(f.clone()),
        })
        .collect()
}

/// `-v`, `-vn`: a single dash followed by one or more letters.
fn is_short_cluster(s: &str) -> bool {
    s.len() > 1 && s.starts_with('-') && !s.starts_with("--")
}
```

Then make `is_dry_run` use the same helper (behavior unchanged):

```rust
/// `--dry-run`, or `-n` alone or inside a short cluster (`-vn`).
pub fn is_dry_run(flags: &[String]) -> bool {
    flags
        .iter()
        .any(|f| f == "--dry-run" || (is_short_cluster(f) && f[1..].contains('n')))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib flags::tests 2>&1 | tail -20`
Expected: all `flags::tests` pass, including the four new ones.

- [ ] **Step 5: Done gate and commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
Expected: all clean.

```bash
git add src/flags.rs
git commit -m "feat(flags): read brew's greedy mode from the forwarded args

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```

---

### Task 2: `PkgIdentity::is_latest_cask` in `src/identity.rs`

**Files:**
- Modify: `src/identity.rs` (`impl PkgIdentity`, after `same_artifact`; test in `mod tests`)

**Interfaces:**
- Consumes: `CaskIdentity.version` (`":latest"` for `version :latest`, parsed by `first_quoted_or_symbol`).
- Produces: `pub fn is_latest_cask(&self) -> bool` on `PkgIdentity`.

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `src/identity.rs`:

```rust
    #[test]
    fn is_latest_cask_reads_only_the_cutoff_side() {
        let latest = PkgIdentity::Cask(
            parse_cask("cask \"nightly\" do\n  version :latest\n  sha256 :no_check\nend\n").unwrap(),
        );
        let numbered = PkgIdentity::Cask(
            parse_cask("cask \"nightly\" do\n  version \"2.0\"\n  sha256 \"abc\"\nend\n").unwrap(),
        );
        let formula = PkgIdentity::Formula(FormulaIdentity {
            version: ":latest".into(),
            revision: 0,
            rebuild: None,
            sha256: "s".into(),
        });
        assert!(latest.is_latest_cask());
        assert!(!numbered.is_latest_cask(), "a cutoff that moved to a number soaks");
        assert!(!formula.is_latest_cask(), "only casks have :latest");
        // brew's version-only receipt for an installed :latest cask.
        let receipt = PkgIdentity::Cask(
            parse_cask(&crate::brew::version_only_cask_receipt("nightly", "latest")).unwrap(),
        );
        assert!(receipt.is_latest_cask());
        assert!(receipt.same_artifact(&latest), "same definition: already soaked");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib identity::tests::is_latest_cask_reads_only_the_cutoff_side 2>&1 | tail -10`
Expected: compile error, `no method named is_latest_cask`.

- [ ] **Step 3: Implement**

In `src/identity.rs`, inside `impl PkgIdentity`, after `same_artifact`:

```rust
    /// A cask whose definition says `version :latest`: the artifact behind
    /// it changes without the cask changing, so its contents cannot be soaked.
    pub fn is_latest_cask(&self) -> bool {
        matches!(self, Self::Cask(c) if c.version == ":latest")
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib identity::tests 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Done gate and commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
Expected: all clean.

```bash
git add src/identity.rs
git commit -m "feat(identity): name a :latest cask identity

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```

---

### Task 3: `bare_action` takes the greedy mode

**Files:**
- Modify: `src/cmd.rs`: `struct ResolvedView`, `fn bare_action`, `fn resolve_view`, `fn plan_size`, `fn classify_installed`, `fn outdated` (the `view.action = bare_action(&view)` line), `fn apply_resolved` (the `if self.bare_run` block), `fn upgrade` (the `plan_size` call); new `fn latest_left`, `fn latest_note` next to `bare_action`; tests after `fn auto_updates_world` in `mod tests`.

**Interfaces:**
- Consumes: `flags::Greedy` (Task 1), `PkgIdentity::is_latest_cask` (Task 2).
- Produces (all private to `src/cmd.rs`):
  - `struct ResolvedView { ..., auto_updates: bool, latest: bool }`
  - `fn bare_action(view: &ResolvedView, greedy: flags::Greedy) -> DesiredAction`
  - `fn latest_left(view: &ResolvedView, greedy: flags::Greedy) -> bool`
  - `fn latest_note(name: &str) -> String`
  - `fn plan_size(git, snaps, cache, inv, greedy: flags::Greedy) -> usize`
  - In this task every caller passes `flags::Greedy::default()`, so behavior is unchanged; Tasks 4 and 5 wire the real mode.

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` in `src/cmd.rs`, directly after `fn auto_updates_world`:

```rust
    /// A hand-built view for the pure functions. `latest` makes the cutoff
    /// `version :latest`; the installed side is always a numbered version.
    fn greedy_view(action: DesiredAction, auto_updates: bool, latest: bool) -> ResolvedView {
        let id = |v: &str| {
            PkgIdentity::Cask(
                crate::identity::parse_cask(&format!("cask \"x\" do\n  version {v}\nend\n"))
                    .unwrap(),
            )
        };
        ResolvedView {
            installed: Some(id("\"1.0\"")),
            cutoff: Some(id(if latest { ":latest" } else { "\"2.0\"" })),
            head: None,
            action,
            cutoff_blob: None,
            warnings: Vec::new(),
            auto_updates,
            latest,
        }
    }

    fn greedy(argv: &[&str]) -> flags::Greedy {
        flags::greedy_mode(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn bare_action_leaves_self_updating_cask_unless_greedy_auto_updates() {
        let view = greedy_view(DesiredAction::InstallCutoff, true, false);
        assert_eq!(bare_action(&view, greedy(&[])), DesiredAction::LeaveAutoUpdates);
        assert_eq!(
            bare_action(&view, greedy(&["--greedy-latest"])),
            DesiredAction::LeaveAutoUpdates
        );
        assert_eq!(
            bare_action(&view, greedy(&["--greedy-auto-updates"])),
            DesiredAction::InstallCutoff
        );
        assert_eq!(
            bare_action(&view, greedy(&["--greedy"])),
            DesiredAction::InstallCutoff
        );
    }

    #[test]
    fn bare_action_only_changes_a_self_updating_install() {
        for argv in [
            &[][..],
            &["--greedy"][..],
            &["--greedy-latest"][..],
            &["--greedy-auto-updates"][..],
        ] {
            let plain = greedy_view(DesiredAction::InstallCutoff, false, false);
            assert_eq!(bare_action(&plain, greedy(argv)), DesiredAction::InstallCutoff, "{argv:?}");
            let soaked = greedy_view(DesiredAction::NoOpAlreadySoaked, true, false);
            assert_eq!(bare_action(&soaked, greedy(argv)), DesiredAction::NoOpAlreadySoaked, "{argv:?}");
            let ahead = greedy_view(DesiredAction::LeaveAheadOfSoak, true, false);
            assert_eq!(bare_action(&ahead, greedy(argv)), DesiredAction::LeaveAheadOfSoak, "{argv:?}");
            let mut fresh = greedy_view(DesiredAction::InstallCutoff, true, false);
            fresh.installed = None;
            assert_eq!(bare_action(&fresh, greedy(argv)), DesiredAction::InstallCutoff, "{argv:?}");
        }
    }

    #[test]
    fn latest_left_only_under_greedy_latest_with_a_matching_cutoff() {
        let same = greedy_view(DesiredAction::NoOpAlreadySoaked, false, true);
        assert!(!latest_left(&same, greedy(&[])), "bare run: silent, as today");
        assert!(latest_left(&same, greedy(&["--greedy-latest"])));
        assert!(latest_left(&same, greedy(&["--greedy"])));
        assert!(!latest_left(&same, greedy(&["--greedy-auto-updates"])));
        let moved = greedy_view(DesiredAction::InstallCutoff, false, true);
        assert!(!latest_left(&moved, greedy(&["--greedy"])), "a changed definition reinstalls");
        let numbered = greedy_view(DesiredAction::NoOpAlreadySoaked, false, false);
        assert!(!latest_left(&numbered, greedy(&["--greedy"])));
        let mut fresh = greedy_view(DesiredAction::NoOpAlreadySoaked, false, true);
        fresh.installed = None;
        assert!(!latest_left(&fresh, greedy(&["--greedy"])));
        assert_eq!(
            latest_note("nightly"),
            "nightly: version :latest left unchanged; the contents of a :latest cask cannot be soaked"
        );
    }

    #[test]
    fn resolve_view_reads_latest_from_the_cutoff_cask() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "caskcut",
            "Casks/n/nightly.rb",
            "cask \"nightly\" do\n  version :latest\n  sha256 :no_check\n  url \"https://example.com/nightly.dmg\"\nend\n",
        );
        git.insert_blob("caskhead", "Casks/n/nightly.rb", cask_rb("nightly", "2.0"));
        let receipt = crate::brew::version_only_cask_receipt("nightly", "latest");
        let view = resolve_view(
            &git,
            &core_snaps(),
            unused_cache(),
            "homebrew/cask",
            "nightly",
            PkgKind::Cask,
            Some(&receipt),
        )
        .unwrap()
        .expect("parseable");
        assert!(view.latest, "cutoff is :latest");
        assert_eq!(view.action, DesiredAction::NoOpAlreadySoaked);
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/n/nightly.rb", cask_rb("nightly", "2.0"));
        git.insert_blob("caskhead", "Casks/n/nightly.rb", cask_rb("nightly", "2.0"));
        let view = resolve_view(
            &git,
            &core_snaps(),
            unused_cache(),
            "homebrew/cask",
            "nightly",
            PkgKind::Cask,
            Some(&receipt),
        )
        .unwrap()
        .expect("parseable");
        assert!(!view.latest, "a cutoff that moved to a number is soakable");
        assert_eq!(view.action, DesiredAction::InstallCutoff);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cmd::tests::bare_action 2>&1 | tail -20`
Expected: compile errors (`no field latest`, wrong number of arguments to `bare_action`, `latest_left` not found).

- [ ] **Step 3: Implement the pure functions and the view field**

In `src/cmd.rs`, replace `struct ResolvedView` and `fn bare_action` with:

```rust
struct ResolvedView {
    installed: Option<PkgIdentity>,
    cutoff: Option<PkgIdentity>,
    head: Option<PkgIdentity>,
    action: DesiredAction,
    cutoff_blob: Option<Vec<u8>>,
    warnings: Vec<String>,
    /// The HEAD cask says `auto_updates true`.
    auto_updates: bool,
    /// The cutoff cask says `version :latest`. Read from the cutoff, not
    /// HEAD: the cutoff is what brewsoak would install, and a cask whose
    /// cutoff moved from `:latest` to a number soaks like any other.
    latest: bool,
}

/// What a bare `upgrade`/`outdated` does: an installed self-updating cask
/// that is behind the cutoff is left to the app, as brew leaves it without
/// `--greedy` / `--greedy-auto-updates`. Everything else keeps its action.
/// `greedy.latest` changes no action: a `:latest` cask is reinstalled only
/// when its cutoff definition differs from the installed one, which
/// `desired_action` already decides; see `latest_left` for the note.
fn bare_action(view: &ResolvedView, greedy: flags::Greedy) -> DesiredAction {
    if view.auto_updates
        && !greedy.auto_updates
        && view.installed.is_some()
        && view.action == DesiredAction::InstallCutoff
    {
        DesiredAction::LeaveAutoUpdates
    } else {
        view.action
    }
}

/// Under `--greedy` / `--greedy-latest`, brew reinstalls every `:latest`
/// cask. brewsoak leaves one whose cutoff definition matches the installed
/// one and says why. Evaluate after `bare_action` has settled the action.
fn latest_left(view: &ResolvedView, greedy: flags::Greedy) -> bool {
    greedy.latest
        && view.latest
        && view.installed.is_some()
        && view.action == DesiredAction::NoOpAlreadySoaked
}

fn latest_note(name: &str) -> String {
    format!(
        "{name}: version :latest left unchanged; the contents of a :latest cask cannot be soaked"
    )
}
```

In `fn resolve_view`, replace the `let auto_updates = ...` line and the `Ok(Some(ResolvedView { ... }))` with:

```rust
    let auto_updates = kind == PkgKind::Cask && head_rb.is_some_and(identity::cask_auto_updates);
    let latest = cutoff.as_ref().is_some_and(PkgIdentity::is_latest_cask);
    Ok(Some(ResolvedView {
        installed,
        cutoff,
        head,
        action,
        cutoff_blob: blobs.cutoff,
        warnings,
        auto_updates,
        latest,
    }))
```

- [ ] **Step 4: Thread the parameter through every caller (behavior unchanged)**

`fn plan_size`: add the parameter and name all three agreeing sites in the comment.

```rust
/// How many installed packages the soak window says to change. Resolution is
/// cheap (cached blobs) and errors just mean "no total to show".
fn plan_size(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
    greedy: flags::Greedy,
) -> usize {
    // Keep this in step with `apply_resolved` and the `outdated` report:
    // every package counted here must reach `announce` there and land under
    // "Outdated (will upgrade)", and none other. All three go through
    // `bare_action` with the same greedy mode.
    snapshotted_pkgs(inv, snaps)
        .filter(|pkg| bare_skip(pkg, snaps).is_none())
        .filter(|pkg| {
            matches!(
                resolve_view(
                    git,
                    snaps,
                    cache,
                    &pkg.origin,
                    &pkg.name,
                    pkg.kind,
                    Some(&pkg.receipt_rb)
                ),
                Ok(Some(view)) if bare_action(&view, greedy) == DesiredAction::InstallCutoff
            )
        })
        .count()
}
```

`fn upgrade`: `let plan_total = (names.is_empty()).then(|| plan_size(git, snaps, cache, inv, flags::Greedy::default()));`

`fn classify_installed` (the `update` summary; `brew update` takes no greedy flags, so this stays bare for good):

```rust
        // `brew update` has no greedy mode; the summary reads like a bare run.
        view.action = bare_action(&view, flags::Greedy::default());
```

`fn outdated`: `view.action = bare_action(&view, flags::Greedy::default());`

`fn apply_resolved`, the bare block:

```rust
        if self.bare_run {
            view.action = bare_action(&view, flags::Greedy::default());
        }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib cmd::tests 2>&1 | tail -5`
Expected: all `cmd::tests` pass, including the four new ones and the three existing `auto_updates` tests.

- [ ] **Step 6: Done gate and commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
Expected: all clean. (`plan_size` has five parameters, under clippy's `too_many_arguments` limit of seven, so no `#[allow]` is needed.)

```bash
git add src/cmd.rs
git commit -m "refactor(cmd): bare_action takes the greedy mode; views know :latest cutoffs

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```

---

### Task 4: `upgrade` honors the greedy flags

**Files:**
- Modify: `src/cmd.rs`: `fn upgrade` (`plan_size` call), `fn apply_resolved` (bare block, `did`, the `NoOpAlreadySoaked` arm), `fn note_dropped_flags`, its two callers (the `"reinstall"` site inside the repair-reinstall path and the `"install"` site next to `tap::brew_install_args`), test `staged_install_drops_flags_brew_install_rejects_and_says_so_once`, new tests after `fn greedy_view`.

**Interfaces:**
- Consumes: `flags::greedy_mode`, `flags::without_greedy` (Task 1); `bare_action`, `latest_left`, `latest_note`, `plan_size(.., greedy)` (Task 3); test helpers `cask_rb`, `cask_pkg_from`, `formula_pkg`, `formula_rb`, `core_snaps`, `cfg24`, `inv_from`, `lock_runs`, `run_is_soaked_install`, `upgrade_names_flags`, `two_outdated_world`.
- Produces: test helpers `fn latest_cask_rb(name: &str, url: &str, auto_updates: bool) -> String`, `fn greedy_world() -> (MockBrew, InMemoryGit, Snapshots)` and `fn greedy_upgrade(brew: &MockBrew, git: &InMemoryGit, snaps: &Snapshots, argv: &[&str]) -> String` (used by Task 5); `ApplySession::note_dropped_flags(&mut self, verb: &str, flags: &[String], context: &str)` and `ApplySession::staged_install_flags(&self) -> Vec<String>`.

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` in `src/cmd.rs`, directly after `fn greedy` (Task 3):

```rust
    /// A `version :latest` cask with `sha256 :no_check`; `url` is the only
    /// thing that can differ between two of them.
    fn latest_cask_rb(name: &str, url: &str, auto_updates: bool) -> String {
        let auto = if auto_updates { "  auto_updates true\n" } else { "" };
        format!(
            "cask \"{name}\" do\n  version :latest\n  sha256 :no_check\n  url \"{url}\"\n{auto}end\n"
        )
    }

    /// Five installed packages, one per greedy row: `wget` (formula behind
    /// cutoff), `alt-tab` (self-updating, behind cutoff), `nightly`
    /// (`:latest`, version-only receipt, cutoff matches), `moved` (`:latest`,
    /// full receipt, cutoff url moved) and `self-latest` (`:latest` and
    /// self-updating, cutoff url moved).
    ///
    /// Expected plan sizes: bare 2 (wget, moved); `--greedy-latest` 2;
    /// `--greedy-auto-updates` 4 (+ alt-tab, self-latest); `--greedy` 4.
    fn greedy_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "headsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.2.0", "newsha"),
        );
        let casks = [
            (
                "alt-tab",
                format!("{}  auto_updates true\n", cask_rb("alt-tab", "11.8.0")),
            ),
            (
                "nightly",
                latest_cask_rb("nightly", "https://example.com/nightly.dmg", false),
            ),
            (
                "moved",
                latest_cask_rb("moved", "https://example.com/moved-new.dmg", false),
            ),
            (
                "self-latest",
                latest_cask_rb("self-latest", "https://example.com/self-new.dmg", true),
            ),
        ];
        for (name, rb) in &casks {
            let path = format!("Casks/{}/{name}.rb", &name[..1]);
            git.insert_blob("caskcut", &path, rb.clone());
            git.insert_blob("caskhead", &path, rb.clone());
        }
        let brew = MockBrew {
            installed: vec![
                formula_pkg("wget", formula_rb("wget", "1.0.0", "oldsha")),
                cask_pkg_from(
                    "alt-tab",
                    "homebrew/cask",
                    crate::brew::version_only_cask_receipt("alt-tab", "7.38.1"),
                ),
                cask_pkg_from(
                    "nightly",
                    "homebrew/cask",
                    crate::brew::version_only_cask_receipt("nightly", "latest"),
                ),
                cask_pkg_from(
                    "moved",
                    "homebrew/cask",
                    latest_cask_rb("moved", "https://example.com/moved-old.dmg", false),
                ),
                cask_pkg_from(
                    "self-latest",
                    "homebrew/cask",
                    latest_cask_rb("self-latest", "https://example.com/self-old.dmg", true),
                ),
            ],
            ..MockBrew::new()
        };
        (brew, git, core_snaps())
    }

    /// Bare `upgrade` over `greedy_world` with `flags`; returns the output.
    fn greedy_upgrade(brew: &MockBrew, git: &InMemoryGit, snaps: &Snapshots, argv: &[&str]) -> String {
        let cfg = cfg24();
        let inv = inv_from(brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let flags: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        upgrade(
            brew,
            git,
            snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &flags,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn bare_upgrade_of_greedy_world_is_unchanged() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["-v"]);
        assert!(text.contains("upgrading 2 of 5 packages"), "{text}");
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "wget"), "{text}");
        assert!(run_is_soaked_install(&runs, "moved"), "a changed :latest definition reinstalls: {text}");
        assert!(!run_is_soaked_install(&runs, "alt-tab"), "{text}");
        assert!(!run_is_soaked_install(&runs, "self-latest"), "{text}");
        assert!(!run_is_soaked_install(&runs, "nightly"), "{text}");
        assert!(!text.contains("cannot be soaked"), "bare run says nothing about :latest: {text}");
        assert!(text.contains("auto-updates 2"), "{text}");
    }

    #[test]
    fn greedy_upgrades_self_updating_casks_and_notes_latest_casks() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy", "-v"]);
        assert!(text.contains("upgrading 4 of 5 packages"), "{text}");
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "alt-tab"), "{text}");
        assert!(run_is_soaked_install(&runs, "self-latest"), "{text}");
        assert!(run_is_soaked_install(&runs, "moved"), "{text}");
        assert!(!run_is_soaked_install(&runs, "nightly"), "{text}");
        assert!(
            text.contains("alt-tab: installing cutoff 11.8.0; installed 7.38.1 is behind soak; installing cutoff"),
            "{text}"
        );
        assert!(
            text.contains("nightly: up to date (soaked); installed :latest matches cutoff; left unchanged; its contents cannot be soaked"),
            "{text}"
        );
        assert!(text.contains("notes:\n"), "{text}");
        assert!(
            text.contains(&format!("\n  {}\n", latest_note("nightly"))),
            "the note is a line of the notes block: {text}"
        );
        assert!(!text.contains("auto-updates"), "no cask was left to the app: {text}");
        assert!(text.contains("upgraded 4, already soaked 1,"), "{text}");
        for args in &runs {
            assert!(
                !args.iter().any(|a| a.starts_with("--greedy")),
                "brew install never sees the greedy flags: {args:?}"
            );
        }
    }

    #[test]
    fn greedy_auto_updates_upgrades_self_updating_casks_only() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy-auto-updates"]);
        assert!(text.contains("upgrading 4 of 5 packages"), "{text}");
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "alt-tab"), "{text}");
        assert!(run_is_soaked_install(&runs, "self-latest"), "{text}");
        assert!(!run_is_soaked_install(&runs, "nightly"), "{text}");
        assert!(!text.contains("cannot be soaked"), "{text}");
    }

    #[test]
    fn greedy_latest_leaves_self_updating_casks_and_notes_latest() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy-latest", "-v"]);
        assert!(text.contains("upgrading 2 of 5 packages"), "{text}");
        let runs = lock_runs(&brew);
        assert!(!run_is_soaked_install(&runs, "alt-tab"), "{text}");
        assert!(
            !run_is_soaked_install(&runs, "self-latest"),
            "both :latest and self-updating: the auto-updates leave stands: {text}"
        );
        assert!(run_is_soaked_install(&runs, "moved"), "{text}");
        assert!(text.contains("self-latest: auto-updates; installed :latest is left to the app"), "{text}");
        assert!(text.contains(&latest_note("nightly")), "{text}");
        assert!(text.contains("auto-updates 2"), "{text}");
    }

    #[test]
    fn short_g_is_greedy_on_upgrade_and_never_reaches_brew_install() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["-vg"]);
        assert!(text.contains("upgrading 4 of 5 packages"), "{text}");
        let installs: Vec<Vec<String>> = lock_runs(&brew)
            .into_iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .collect();
        assert_eq!(installs.len(), 4, "{text}");
        for args in &installs {
            assert!(
                !args.iter().any(|a| a == "-g" || a == "-vg" || a == "--git"),
                "`g` is --git to brew install: {args:?}"
            );
            assert!(args.iter().any(|a| a == "-v"), "the rest of the cluster stays: {args:?}");
        }
    }

    #[test]
    fn named_upgrade_ignores_greedy_and_prints_no_latest_note() {
        let (brew, git, snaps) = greedy_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["nightly".into(), "alt-tab".into()],
            &["--greedy".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let runs = lock_runs(&brew);
        assert!(!run_is_soaked_install(&runs, "nightly"), "{text}");
        assert!(run_is_soaked_install(&runs, "alt-tab"), "named: upgraded regardless of greedy: {text}");
        assert!(!text.contains("cannot be soaked"), "{text}");
        assert!(!text.contains("does not accept --greedy"), "{text}");
    }
```

Then change the existing test `staged_install_drops_flags_brew_install_rejects_and_says_so_once` so the note no longer names `--greedy` (brewsoak consumed it) while `--greedy` still never reaches `brew install`:

```rust
    #[test]
    fn staged_install_drops_flags_brew_install_rejects_and_says_so_once() {
        let (brew, git) = two_outdated_world("");
        let flags = ["--greedy", "--ignore-pinned", "--verbose"].map(String::from);
        let text = upgrade_names_flags(&brew, &git, &[], &flags);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 2, "{visible:?}");
        for args in &visible {
            assert!(
                !args
                    .iter()
                    .any(|a| a == "--greedy" || a == "--ignore-pinned"),
                "{args:?}"
            );
            assert!(args.iter().any(|a| a == "--verbose"), "{args:?}");
        }
        // `--greedy` is brewsoak's on an upgrade, so it is not "dropped".
        let note = "brew install does not accept --ignore-pinned; dropped from staged installs";
        assert_eq!(text.matches(note).count(), 1, "{text}");
        assert!(!text.contains("does not accept --greedy"), "{text}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cmd::tests::greedy 2>&1 | tail -30 && cargo test --lib cmd::tests::staged_install_drops_flags 2>&1 | tail -10`
Expected: `greedy_upgrades_self_updating_casks_and_notes_latest_casks`, `greedy_auto_updates_upgrades_self_updating_casks_only`, `short_g_is_greedy_on_upgrade_and_never_reaches_brew_install` and `staged_install_drops_flags_brew_install_rejects_and_says_so_once` FAIL (plan still says 2 of 5; note still names `--greedy`; `-vg` forwarded). `named_upgrade_ignores_greedy_and_prints_no_latest_note` FAILS on the "does not accept --greedy" note. `greedy_latest_leaves_self_updating_casks_and_notes_latest` FAILS on the missing note. Only `bare_upgrade_of_greedy_world_is_unchanged` passes already (bare behavior is unchanged by design).

- [ ] **Step 3: Implement**

`fn upgrade`: read the mode once.

```rust
    let plan_total =
        (names.is_empty()).then(|| plan_size(git, snaps, cache, inv, flags::greedy_mode(user_flags)));
```

`fn apply_resolved`: replace the bare block and the `did` match, and add the note in the `NoOpAlreadySoaked` arm.

```rust
        let greedy = flags::greedy_mode(self.user_flags);
        if self.bare_run {
            view.action = bare_action(&view, greedy);
        }
        let leaves_latest = self.bare_run && latest_left(&view, greedy);
        let did = match view.action {
            DesiredAction::InstallCutoff => "installing cutoff",
            DesiredAction::NoOpAlreadySoaked if leaves_latest => {
                "left unchanged; its contents cannot be soaked"
            }
            DesiredAction::NoOpAlreadySoaked => "left unchanged",
            DesiredAction::LeaveAheadOfSoak => "left unchanged",
            DesiredAction::LeaveAutoUpdates => "left to the app",
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => "refused",
        };
```

and, in the later `match view.action`:

```rust
            DesiredAction::NoOpAlreadySoaked => {
                if self.brew_verb == "install" {
                    writeln!(self.out, "{name} is already installed")?;
                }
                if leaves_latest {
                    // brew would reinstall it under --greedy; say why we did not.
                    self.defer(latest_note(&name));
                }
            }
```

`fn note_dropped_flags`: take the flags it judges.

```rust
    /// Say once which of `flags` `brew <verb>` would reject and we dropped.
    fn note_dropped_flags(&mut self, verb: &str, flags: &[String], context: &str) {
        let dropped = crate::flags::filter_for_verb(verb, flags).dropped;
        if let Some(note) = crate::flags::dropped_note(verb, &dropped, context)
            && !self.deferred.contains(&note)
        {
            self.defer(note);
        }
    }

    /// What a staged `brew install` may see. On an upgrade the greedy flags
    /// are brewsoak's (read by `bare_action`), so they are neither forwarded
    /// nor reported as dropped; `-g` would otherwise reach brew install as
    /// `--git`. The install table filter still applies in `brew_install_args`.
    fn staged_install_flags(&self) -> Vec<String> {
        if self.brew_verb == "upgrade" {
            crate::flags::without_greedy(self.user_flags)
        } else {
            self.user_flags.to_vec()
        }
    }
```

At the `"install"` call site (next to `tap::brew_install_args`; this is the only `brew_install_args` call that forwards user flags, the dependency install in `install_missing_dep` passes `&[]`):

```rust
        let install_flags = self.staged_install_flags();
        let args = tap::brew_install_args(&pkg, &path, &install_flags);
        self.note_dropped_flags("install", &install_flags, "staged installs");
```

At the `"reinstall"` call site (repair reinstall; `user_flags` is the enclosing function's parameter there):

```rust
            args.extend(crate::flags::filter_for_verb("reinstall", user_flags).kept);
            session.note_dropped_flags("reinstall", user_flags, "repair reinstalls");
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cmd::tests 2>&1 | tail -5`
Expected: all `cmd::tests` pass, the three pre-existing `auto_updates` tests untouched and green.

- [ ] **Step 5: Done gate and commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
Expected: all clean.

```bash
git add src/cmd.rs
git commit -m "feat(upgrade): honor --greedy, --greedy-auto-updates and --greedy-latest on a bare upgrade

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```

---

### Task 5: `outdated` honors the greedy flags, and the three agree

**Files:**
- Modify: `src/cmd.rs`: `fn outdated` (greedy mode, the `bare_action` call, the `-v` note); new tests after `fn named_upgrade_ignores_greedy_and_prints_no_latest_note`.

**Interfaces:**
- Consumes: `flags::greedy_mode` (Task 1); `bare_action`, `latest_left`, `latest_note`, `plan_size` (Task 3); `greedy_world`, `greedy_upgrade` (Task 4); `outdated`, `unused_cache`.
- Produces: nothing new outside tests.

- [ ] **Step 1: Write the failing tests**

```rust
    /// Bare `outdated` over `greedy_world` with `flags`; returns the output.
    fn greedy_outdated(brew: &MockBrew, git: &InMemoryGit, snaps: &Snapshots, argv: &[&str]) -> String {
        let cfg = cfg24();
        let inv = inv_from(brew, &cfg);
        let flags: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        outdated(brew, git, snaps, unused_cache(), &inv, &cfg, &flags, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Lines under `header` up to the next `==> ` header, `(none)` excluded.
    fn section_lines(text: &str, header: &str) -> Vec<String> {
        text.lines()
            .skip_while(|l| *l != header)
            .skip(1)
            .take_while(|l| !l.starts_with("==> "))
            .filter(|l| *l != "(none)")
            .map(str::to_string)
            .collect()
    }

    const OUTDATED_HEADER: &str = "==> Outdated (will upgrade)";
    const AUTO_UPDATES_HEADER: &str = "==> Auto-updates (left to the app; name it to upgrade)";

    #[test]
    fn outdated_greedy_lists_self_updating_casks_as_outdated() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["--greedy"]);
        let upgrades = section_lines(&text, OUTDATED_HEADER);
        assert!(upgrades.contains(&"alt-tab (7.38.1) < 11.8.0".to_string()), "{text}");
        assert!(upgrades.contains(&"self-latest (:latest) < :latest".to_string()), "{text}");
        assert!(upgrades.contains(&"moved (:latest) < :latest".to_string()), "{text}");
        assert!(upgrades.contains(&"wget (1.0.0) < 1.1.0".to_string()), "{text}");
        assert_eq!(upgrades.len(), 4, "{text}");
        assert!(section_lines(&text, AUTO_UPDATES_HEADER).is_empty(), "{text}");
        assert!(!text.contains("cannot be soaked"), "the note needs -v: {text}");
    }

    #[test]
    fn outdated_greedy_latest_notes_latest_casks_with_verbose() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["--greedy-latest", "-v"]);
        assert!(text.contains(&latest_note("nightly")), "{text}");
        assert_eq!(text.matches("cannot be soaked").count(), 1, "only nightly: {text}");
        let auto = section_lines(&text, AUTO_UPDATES_HEADER);
        assert_eq!(auto.len(), 2, "alt-tab and self-latest stay left to the app: {text}");
        assert_eq!(section_lines(&text, OUTDATED_HEADER).len(), 2, "{text}");
    }

    #[test]
    fn outdated_greedy_auto_updates_lists_self_updating_and_skips_the_note() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["--greedy-auto-updates", "-v"]);
        assert_eq!(section_lines(&text, OUTDATED_HEADER).len(), 4, "{text}");
        assert!(section_lines(&text, AUTO_UPDATES_HEADER).is_empty(), "{text}");
        assert!(!text.contains("cannot be soaked"), "{text}");
    }

    #[test]
    fn outdated_short_g_is_greedy() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["-g"]);
        assert_eq!(section_lines(&text, OUTDATED_HEADER).len(), 4, "{text}");
    }

    #[test]
    fn plan_size_upgrade_and_outdated_agree_under_every_greedy_mode() {
        for (argv, want) in [
            (&[][..], 2usize),
            (&["--greedy-latest"][..], 2),
            (&["--greedy-auto-updates"][..], 4),
            (&["--greedy"][..], 4),
        ] {
            let (brew, git, snaps) = greedy_world();
            let cfg = cfg24();
            let inv = inv_from(&brew, &cfg);
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let size = plan_size(&git, &snaps, unused_cache(), &inv, flags::greedy_mode(&owned));
            assert_eq!(size, want, "plan_size {argv:?}");

            let text = greedy_upgrade(&brew, &git, &snaps, argv);
            assert!(text.contains(&format!("upgrading {want} of 5 packages")), "{argv:?}: {text}");
            let announced = text
                .lines()
                .filter(|l| l.starts_with('[') && l.contains(&format!("/{want}] ")))
                .count();
            assert_eq!(announced, want, "announced {argv:?}: {text}");

            let text = greedy_outdated(&brew, &git, &snaps, argv);
            assert_eq!(
                section_lines(&text, OUTDATED_HEADER).len(),
                want,
                "outdated {argv:?}: {text}"
            );
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cmd::tests::outdated_greedy 2>&1 | tail -20 && cargo test --lib cmd::tests::plan_size_upgrade_and_outdated_agree 2>&1 | tail -10`
Expected: `outdated_greedy_lists_self_updating_casks_as_outdated`, `outdated_greedy_auto_updates_lists_self_updating_and_skips_the_note`, `outdated_short_g_is_greedy` and the agreement test FAIL (outdated still files alt-tab under Auto-updates); `outdated_greedy_latest_notes_latest_casks_with_verbose` fails on the missing note.

- [ ] **Step 3: Implement**

In `fn outdated`, after `let verbose = is_verbose(extra_args);`:

```rust
    let greedy = flags::greedy_mode(extra_args);
```

Replace `view.action = bare_action(&view, flags::Greedy::default());` and the verbose evaluate block with:

```rust
        view.action = bare_action(&view, greedy);
        for warn in &view.warnings {
            writeln!(out, "warning: {warn}")?;
        }
        if verbose {
            writeln!(
                out,
                "{}",
                report::evaluate_line(
                    &pkg.name,
                    view.action,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.head.as_ref(),
                    "classified for outdated",
                )
            )?;
            if latest_left(&view, greedy) {
                writeln!(out, "{}", latest_note(&pkg.name))?;
            }
        }
```

(The `match view.action` that follows is unchanged: with `greedy.auto_updates` the self-updating casks now arrive as `InstallCutoff` and land in `upgrades`; the Auto-updates section prints `(none)`.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cmd::tests 2>&1 | tail -5`
Expected: all pass, `outdated_lists_auto_updates_cask_in_its_own_section` (bare) untouched and green.

- [ ] **Step 5: Done gate and commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
Expected: all clean.

```bash
git add src/cmd.rs
git commit -m "feat(outdated): list greedy-eligible casks as outdated; note :latest casks with -v

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```

---

### Task 6: Help text and README

**Files:**
- Modify: `src/cli.rs`: `command_help` arms `"upgrade"` and `"outdated"`; test `help_mentions_no_soak_and_taps`.
- Modify: `README.md`: the "Other flags (...) are forwarded to `brew`." paragraph under "## Commands" and the "Casks that update themselves" paragraph under "## Example".

**Interfaces:**
- Consumes: the behavior of Tasks 4 and 5.
- Produces: nothing in code.

- [ ] **Step 1: Write the failing test**

In `src/cli.rs` `mod tests`, extend `help_mentions_no_soak_and_taps`:

```rust
    #[test]
    fn help_mentions_no_soak_and_taps() {
        assert!(help_text().contains("NO_SOAK"));
        assert!(command_help("upgrade").unwrap().contains("no-soak"));
        assert!(
            !command_help("upgrade")
                .unwrap()
                .contains("passed through to brew")
        );
        assert!(command_help("update").unwrap().contains("brew update"));
        for verb in ["upgrade", "outdated"] {
            let text = command_help(verb).unwrap();
            assert!(text.contains("--greedy-auto-updates"), "{verb}: {text}");
            assert!(text.contains("--greedy-latest"), "{verb}: {text}");
            assert!(text.contains("cannot be soaked"), "{verb}: {text}");
        }
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib cli::tests::help_mentions_no_soak_and_taps 2>&1 | tail -10`
Expected: FAIL on the `--greedy-auto-updates` assertion for `upgrade`.

- [ ] **Step 3: Update the help text**

In `src/cli.rs` `command_help`, replace the two lines beginning `Casks that update themselves` in the `"upgrade"` arm with:

```text
Casks that update themselves (auto_updates true) are left to the app on a
bare upgrade, as brew leaves them without --greedy; name one to upgrade it.
  -g, --greedy              also upgrade self-updating casks to their cutoff,
                            and consider version :latest casks
      --greedy-auto-updates self-updating casks only
      --greedy-latest       version :latest casks only
A version :latest cask is reinstalled only when its cutoff definition
differs from the installed one; otherwise it is left with a note that the
contents of a :latest cask cannot be soaked.
```

In the `"outdated"` arm, replace the body with:

```text
Usage: brewsoak outdated

List installed packages that upgrade would change, plus held,
ahead-of-soak, auto-updates, and pinned sections.

  -g, --greedy              list self-updating casks behind their cutoff
                            under Outdated instead of Auto-updates, and
                            consider version :latest casks
      --greedy-auto-updates self-updating casks only
      --greedy-latest       version :latest casks only (with -v, notes each
                            one left because its contents cannot be soaked)
  -v, --verbose   print soak window and a line for every package evaluated
      --raw       print brew's output unfiltered (a full log is always
                  written under $TMPDIR; its path is printed at the end)
```

- [ ] **Step 4: Update the README**

Under "## Commands", replace the line

```
Other flags (`--formula`, `--cask`, `--debug`, …) are forwarded to `brew`.
```

with

```
Other flags (`--formula`, `--cask`, `--debug`, …) are forwarded to `brew`.
`upgrade` and `outdated` honor brew's `--greedy` (`-g`),
`--greedy-auto-updates` and `--greedy-latest` themselves (see below); the
no-soak `brew upgrade` also receives them.
```

Under "## Example", replace the paragraph beginning `Casks that update themselves` with:

```
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
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --lib cli::tests 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 6: Done gate and commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
Expected: all clean. Also confirm the README claims against the code: `rg -n 'greedy' /Users/efitz/Projects/brewsoakr/src/cli.rs /Users/efitz/Projects/brewsoakr/README.md`.

```bash
git add src/cli.rs README.md
git commit -m "docs: describe the greedy flags for upgrade and outdated

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy"
```

---

## Final step

- [ ] Run the done gate once more on the branch head and paste its tail in the completion report:

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test 2>&1 | rg 'test result|FAILED|panicked|warning|error'`
Expected: only `test result: ok.` lines.

- [ ] Confirm the three pre-existing bare-run tests are byte-for-byte unchanged: `git diff -U0 main -- src/cmd.rs | rg '^[-+].*(auto_updates_cask_is_left_alone|bare_upgrade_total_leaves|outdated_lists_auto_updates_cask)'` prints nothing (`-U0` so the new tests placed next to `auto_updates_world` do not drag those names in as context).

- [ ] Report to the orchestrator: branch `issue-3-greedy-casks`, the six commits, the done-gate tail, and the decisions list above (items 2 and 3 are the ones the maintainer may want to veto: the `notes:` line in `upgrade`, and `-g` consumption).
