# Path-Install Stopgap (issue #17) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `brewsoak upgrade|install|reinstall` stop passing staged file paths to brew. A soaked package is installed by bare name only when brew's own current definition is byte-identical to the soaked cutoff, for the package and for every dependency brew might touch; otherwise it is held with a note, and the run continues. A run with soaked work runs `brew update` once, first.

**Architecture:** A new `Brew::by_name` question runs the loaders `brew install` uses (`Formulary.factory`, `Cask::CaskLoader.load`) under `brew ruby` and answers where a bare name loads from and whether its `ruby_source_checksum` equals the cutoff blob's SHA-256. `brew info` is not used for this: it follows the installed keg, not the loader (spec, "Verified facts"). `CutoffDepWalk` asks `brew deps` by name, gates every closure member with it, and reports a hold instead of an error. `install_cutoff` and `install_missing_dep` build `brew install --formula|--cask <name>`.

**Tech Stack:** Rust 2024. One new direct dependency, `ring` (already in `Cargo.lock` through `ureq`), for SHA-256.

**Spec:** `docs/superpowers/specs/2026-11-26-path-install-stopgap.md`. Read it first; every message string and rule below comes from it. The maintainer answered the spec's questions on 2026-11-27 (bare names, exact bytes, one `brew update` before soaked work), so the design is frozen: surface any gap as a design change, do not guess.

## Global Constraints

- Done gate (run before claiming any task done, show output): `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
- Read `AGENTS.md` first. Its "Installing soaked formulae" section still describes path installs until Task 7; the spec wins where they differ.
- Never pass `--ignore-dependencies`, a file path, or a `user/repo/name` token to a soaked `brew install` or `brew deps`. The no-soak step (`src/nosoak.rs`) and the `reinstall` true repair (`src/cmd/apply.rs:140-166`) keep their full tokens: do not touch them.
- Never run `brew trust`. A run makes at most one successful `brew update`: Task 5 adds the one before soaked work; add no other.
- No `#[allow(dead_code)]`. Delete crate-private code the compiler reports dead. Keep the `pub` staging helpers and their tests: `tap::write_blob`, `tap_formula_path`, `tap_cask_path`, `sanitize_unofficial`, `taps::staging_root`, `taps::staged_load_failure`. Keep `origin::staged_tap`, `origin::STAGING_TAP` and all of `inventory.rs`.
- Keep the `tap_root` parameter of `cmd::upgrade`, `cmd::install`, `cmd::reinstall` and `apply_many` (rename to `_tap_root` in Task 4), so about 100 test call sites stay as they are. Remove the `tap_root` field from `CutoffDepWalk` (Task 3) and `ApplySession` (Task 4).
- `MockBrew::by_name` answers "same source, origin tap, not installed-current" for every name it was not scripted for, so existing tests keep installing.
- Existing tests that assert `.rb` paths are migrated to assert the bare name, not deleted.
- American English. Match surrounding comment density. Use `rg PATTERN .` (always give a path).
- End commit messages with the attribution lines from the session's system reminder.

## Review Focus

1. No soaked code path hands brew a path or a full token (Task 6 sweep test).
2. The gate hashes the raw cutoff blob from `resolve_pkg_blobs`, not a staged or sanitized file, and never compares against brewsoak's HEAD blob.
3. A by-name dependency list is used only after that package passed the gate.
4. Every `Error::Brew` from the walk, `brew deps` or `by_name` becomes a hold of one target; the next package still runs; the run exits 1. Git and I/O errors stay fatal.
5. An installed dependency the run does not upgrade is walked through, and holds the target unless brew has it at brew's current version or it passes the gate.
6. Untrusted-tap refusals keep today's note and count the target once.
7. Counts: a held target takes back its `upgraded` count (`hold_target_for_dep`).
8. `origins.toml`: record removed after a real by-name install; untouched on a dry run and on brew's "already installed" answer.
9. `brew update`: runs once, before any loader or `brew deps` question, only when the run has soaked work; also on a dry run; its failure is a note and an exit code, never a hold or an abort; the no-soak step repeats it only when it did not succeed.

---

### Task 0: Record the failing repro

- [ ] Before changing code, run `cargo build` and then `./target/debug/brewsoak upgrade --dry-run ast-grep > <scratch>/before.txt 2>&1; echo "exit=$?" >> <scratch>/before.txt`. If `ast-grep` is no longer outdated, pick a formula that `./target/debug/brewsoak outdated` lists.
- [ ] **Accept:** the file shows `No available formula with the name` and a non-zero exit. Keep it for Task 8. Nothing is committed.

### Task 1: SHA-256 helper

**Files:** Modify `Cargo.toml` (add `ring = "0.17"` under `[dependencies]`); create `src/digest.rs`; register `pub mod digest;` in `src/lib.rs`.

- [ ] **Failing tests first** (`src/digest.rs`): `sha256_hex_of_empty_input` (`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`), `sha256_hex_of_abc` (`ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`).
- [ ] Implement `pub fn sha256_hex(bytes: &[u8]) -> String` (lowercase hex) with `ring::digest::digest(&ring::digest::SHA256, bytes)`.
- [ ] **Accept:** `Cargo.lock` gains no new package (`git diff --stat Cargo.lock` shows only the `brewsoak` dependency list changing); done gate passes. Commit.

### Task 2: `Brew::by_name`

**Files:** Modify `src/brew.rs` (the `*_RUBY` constants at lines 43-60, trait at 61, `ProcessBrew` at 135 and 377, `brewsoak_brew_env_pairs` at 658, `MockBrew` at 253 and 788); modify the delegating test brews at `src/lib.rs:672` and `src/cmd/deps.rs:502`, `:590`.

Types (all `pub`, `Debug, Clone, PartialEq, Eq`):

```rust
/// One question for `Brew::by_name`.
pub struct NameQuery {
    pub kind: PkgKind,
    /// Bare name or cask token: what brewsoak would hand to `brew install`.
    pub name: String,
    /// Lowercase `user/repo` brewsoak expects the name to resolve to.
    pub origin: String,
    /// SHA-256 of the raw cutoff blob; `None` when there is no cutoff.
    pub cutoff_sha256: Option<String>,
}

/// What brew's own data says a bare name is right now.
pub struct ByName {
    /// Lowercase `user/repo` the name resolves to.
    pub tap: String,
    /// Formula: `pkg_version` (`1.5.2_3`). Cask: `version`.
    pub version: String,
    /// brew's `ruby_source_checksum` equals the query's `cutoff_sha256`.
    pub same_source: bool,
    /// brew's `latest_version_installed?` (cask: installed at `version`).
    pub installed_current: bool,
}

impl ByName {
    /// A by-name install would load exactly the soaked cutoff of `origin`:
    /// `same_source`, and `tap` is `origin` (case-insensitive).
    pub fn installs_cutoff(&self, origin: &str) -> bool;
}

// trait Brew, no default body:
/// One answer per query, in order. `Err(text)` is brew's own failure for
/// that name (unknown name, untrusted tap, a script brewsoak could not
/// read). The outer `Err` is only "brew not found".
fn by_name(&self, queries: &[NameQuery]) -> Result<Vec<Result<ByName, String>>, Error>;
```

The script, a new constant beside `COMPARE_VERSIONS_RUBY` (names arrive as ARGV, never as Ruby source; every row leads with `kind name` so the parser matches rows to queries by key, never by position, and a missing, mismatched or duplicated row is that query's `Err`):

```ruby
ARGV.each_slice(2) do |kind, name|
  begin
    if kind == "cask"
      c = Cask::CaskLoader.load(name)
      sum = c.ruby_source_checksum
      puts ["OK", kind, name, c.tap&.name, c.version, sum && sum[:sha256], c.installed_version.to_s == c.version.to_s].join("\t")
    else
      f = Formulary.factory(name)
      puts ["OK", kind, name, f.tap&.name, f.pkg_version, f.ruby_source_checksum&.hexdigest, f.latest_version_installed?].join("\t")
    end
  rescue StandardError, ScriptError => e
    puts ["ERR", kind, name, e.class, e.message.lines.first.to_s.strip].join("\t")
  end
end
```

Verified output on Homebrew 7.0.16: `OK	formula	ast-grep	homebrew/core	0.51.3	b3c32fcf…	false` for `formula ast-grep`; `ERR	formula	no-such-xyz	FormulaUnavailableError	No available formula with the name "no-such-xyz".`.

- [ ] **Failing tests first** (`src/brew.rs` tests):
  - `parse_by_name_lines_reads_ok_and_err_rows`: an `OK` formula row, an `OK` cask row, an `OK` row with an empty checksum (never `same_source`), an `OK` row with an empty tap (`Err`), an `ERR` row (`Err` carrying the message), and a malformed row (`Err`).
  - `parse_by_name_lines_short_output_is_err_for_the_missing_names` (fewer lines than queries).
  - `judge_same_source_needs_equal_checksum`, `judge_no_cutoff_is_never_same_source`, `installs_cutoff_needs_the_origin_tap` (case-insensitive; `cyclonedx-gomod` loading `homebrew/core` with origin `cyclonedx/cyclonedx` is false).
  - With the fake `brew.sh` harness of `process_brew_deps_asks_brew_for_full_names` (line 1497): `process_brew_by_name_asks_about_every_installed_package_in_its_first_call` (after `installed_packages`, one `ruby` spawn whose ARGV holds the installed names and the queried one; a second query for an installed name spawns nothing), `process_brew_by_name_asks_once_per_uninstalled_name`, and `process_brew_by_name_script_failure_is_err_for_every_query` (script exits 1: `Ok(vec![Err(_), Err(_)])`, never an outer `Err`).
  - `process_brew_by_name_never_runs_brew_info`: no logged spawn starts with `info` besides the inventory's `info --json=v2 --installed`.
  - `process_brew_deps_asks_once_per_token` (two `deps` calls, one log line).
  - `process_brew_forgets_loader_and_deps_answers_after_brew_update`: `by_name`, `deps`, then `run_visible(["update"])`, then the same `by_name` and `deps` again: the fake brew logs two `ruby` and two `deps` spawns.
  - `brew_children_never_force_api_auto_update`: `brewsoak_brew_env_pairs()` contains `("HOMEBREW_FORCE_API_AUTO_UPDATE", None)`.
  - `mock_by_name_defaults_to_same_source` and `mock_by_name_honors_scripted_answers`.
- [ ] Implement:
  - `pub(crate) struct Loaded { tap: String, version: String, source_sha256: Option<String>, installed_current: bool }`, `pub(crate) fn parse_by_name_lines(stdout: &str, count: usize) -> Vec<Result<Loaded, String>>`, and `pub(crate) fn judge(loaded: &Loaded, q: &NameQuery) -> ByName`.
  - `ProcessBrew`: `loaded: Mutex<HashMap<(&'static str, String), Result<Loaded, String>>>` and `installed_names: Mutex<Vec<(PkgKind, String)>>` (filled by `installed_packages`). `by_name` asks, in one `brew ruby -e BY_NAME_RUBY -- kind name …` spawn, for every queried name not yet in `loaded`, plus (the first time) every installed name; failures are remembered like successes. `deps_memo: Mutex<HashMap<(&'static str, String), Vec<String>>>` in `deps` (successes only). `run_visible_in` clears `loaded` and `deps_memo` when `args[0] == "update"`, whatever the exit status.
  - `MockBrew`: `pub by_name: BTreeMap<String, Result<ByName, String>>` (keyed by name) and `pub by_name_queries: Mutex<Vec<NameQuery>>`. An unscripted name answers `Ok(ByName { tap: q.origin.clone(), version: String::new(), same_source: true, installed_current: false })`.
  - Delegate `by_name` in `EchoVisibleBrew` and both `FailDepsBrew` impls.
- [ ] **Accept:** done gate passes; no caller yet. Commit.

### Task 3: The walk asks by name and gates the closure

**Files:** Modify `src/cmd/deps.rs` (`CutoffDeps`, `CutoffDepWalk`, `visit`, lines 26-150), `src/cmd/apply.rs` (`install_cutoff` 709-766, `collect_cutoff_deps` 770-795, hold helpers 982-1030), `src/cmd/view.rs` (delete `write_cutoff_blob`, 439-458), `src/cmd/notes.rs` (one note builder).

Shapes:

```rust
// deps.rs
pub(super) enum NameWhy {
    NotCutoff { brew_version: String },      // "brew would install <v>, which is not the soaked cutoff"
    OtherTap { tap: String, origin: String }, // "brew resolves the name to <tap>, not <origin>"
    Untrusted(String),                        // existing trust hold
    Lookup(String),                           // "brew could not describe it by name (<first line>)"
    Deps(String),                             // "brew could not read its dependencies (<first line>)"
    NoCutoff,                                 // "it has no soaked cutoff yet"
}
pub(super) struct NameHold { pub(super) dep: Option<String>, pub(super) why: NameWhy }
// CutoffDeps gains: pub(super) by_name: Option<NameHold>
enum Role { Root, Install, Check }

// notes.rs
pub(super) fn by_name_note(name: &str, dep: Option<&str>, why: &str, brew_verb: &str, token: &str) -> String
// "<name>: [dependency <dep>: ]<why>; brewsoak cannot install a soaked copy until issue #17 is fixed; use `brew <verb> <token>` to bypass brewsoak."
```

`visit(origin, name, kind, role)` rules (spec "Dependency closure"):

1. Token for `brew.deps` is the bare `name`. No staging write.
2. A `Root` whose origin is a third-party tap calls `brew.deps` first, so an untrusted-tap refusal keeps its note; the list is used only after step 3 passes.
3. Ask `by_name` (with the member's origin and `sha256_hex` of the cutoff blob from `resolve_pkg_blobs`, `None` when there is none). `Root` and `Install` need `installs_cutoff(origin)`. `Check` passes on `installed_current || installs_cutoff(origin)`. An installed dependency that would be `Install` but is `installed_current` without `installs_cutoff` is treated as `Check`. A failure sets `self.by_name` (`dep: None` for `Root`) and returns. An `Err(text)` answer is `Untrusted` when `taps::untrusted_tap(text)` matches, else `Lookup`.
4. For each dependency: an installed cask, a no-soak or unsoakable package, and an installed package in a held or snapshot-less tap `continue` as today. An installed soaked formula is visited as `Install` when `upgrades_this_run`, else as `Check`. A missing dependency is handled as today (`HeldDep`, the no-cutoff push, or `visit(.., Install)`).
5. A `brew.deps` error below the root sets `NameHold { dep, why: Deps | Untrusted }`; at the root it propagates as today.
6. Only `Install` members are pushed to `out`.

`install_cutoff`: every `Error::Brew` from `collect_cutoff_deps` is a hold (ambiguous as today; `untrusted_tap` as today, for any origin; anything else `hold_target_by_name` with `Deps`). `Err(e) => return Err(e)` remains only for non-brew errors. Then `walked.by_name` before `walked.held`. New `hold_target_by_name(name, token, &NameHold)`: `hold_target_for_dep()`, one inline `held <name>: [dependency <dep>: ]<why>` line, one `defer_held(by_name_note(..))`; `Untrusted` goes to `hold_target_untrusted` or `hold_target_untrusted_dep` instead.

- [ ] **Failing tests first** (in `src/cmd/deps.rs` tests unless noted; each uses `MockBrew` and asserts on `lock_runs`, the output text and `RunResult`):
  - `walk_asks_deps_by_bare_name`: a wrapper brew records every `deps` token; none contains `/` or ends with `.rb`.
  - `target_not_at_brew_current_is_held_and_the_next_package_installs` (`src/cmd/upgrade.rs`): `alpha` scripted `same_source: false, version: "1.2.0"`, `gamma`-like second package unscripted; output has `held alpha: brew would install 1.2.0, which is not the soaked cutoff`, the note ends ``use `brew upgrade alpha` to bypass brewsoak.`` and contains `issue #17`; the second package's install ran; `refused` is true; the call returned `Ok`.
  - `deps_failure_on_a_core_formula_holds_it_and_the_run_continues`: `deps_errors` for `alpha` with `Error: No available formula with the name "alpha".`; before this task the call returns `Err`.
  - `by_name_lookup_failure_holds_the_target_and_the_run_continues`.
  - `missing_dep_not_at_brew_current_holds_the_target`: note reads `alpha: dependency libx: brew would install …`.
  - `outdated_installed_dep_not_at_cutoff_holds_the_target`: `libx` installed, not upgrading this run, scripted `same_source: false, installed_current: false`.
  - `installed_current_dep_is_left_alone_and_walked_through`: `libx` scripted `installed_current: true, same_source: false` passes; its missing dependency `liby` scripted `same_source: false` holds the target naming `liby`.
  - `name_resolving_to_another_tap_holds_the_target`: installed tap formula `cyclonedx-gomod` with receipt tap `cyclonedx/cyclonedx` (the keg the note's own bypass command creates), answer tap `homebrew/core` with `same_source: true`; note has `brew resolves the name to homebrew/core, not cyclonedx/cyclonedx` and the bypass token `cyclonedx/cyclonedx/cyclonedx-gomod`.
  - `held_target_takes_back_its_upgraded_count`: Summary shows it under held, not upgraded.
  - `dry_run_reports_the_same_hold` (`--dry-run` flag).
  - `cask_target_is_gated_by_name` (`--cask` deps token is the bare cask token).
  - Migrate, do not delete: `deps_failure_on_staged_tap_formula_holds_with_note` (now the `Deps` hold, keyed by bare name), `deps_untrusted_tap_refusal_holds_with_trust_note`, `untrusted_tap_refusal_on_a_dep_holds_the_dep`; each still asserts no `trust` run.
- [ ] Implement as above. Remove the `tap_root` field of `CutoffDepWalk` here (nothing reads it once the walk stops staging; clippy would fail the gate). `install_cutoff` and `install_missing_dep` still stage and install by path in this task, so `ApplySession.tap_root` stays until Task 4; tests that asserted the walk staged dependency files move their assertion to Task 4.
- [ ] **Accept:** done gate passes; `rg -n 'write_cutoff_blob' src` is empty. Commit.

### Task 4: Install by name; origin bookkeeping

**Files:** Modify `src/tap.rs` (`brew_install_args`, line 41, and its tests), `src/cmd/apply.rs` (`install_cutoff`, `install_missing_dep` 798-909, `record_staged_install` 1051-1089, `record_origin` 1032-1046, `StagedRun` 1172-1180, `hold_staged_copy`/`hold_target_staged_copy`, `staged_install_flags`, `staging_dir` 1203, the `tap_root` field), `src/cmd/test_support.rs` (`run_is_soaked_install`, line 215).

- [ ] **Failing tests first:**
  - `src/tap.rs`: `brew_install_args_ends_with_the_bare_name` (`["install", "--formula", "wget"]`); migrate the four flag tests to a token argument.
  - `upgrade_installs_the_cutoff_by_name` (`src/cmd/upgrade.rs`): the visible run is exactly `install --formula alpha`.
  - `missing_dep_is_installed_by_name_before_the_target` (`src/cmd/deps.rs`).
  - `install_and_reinstall_install_by_name` (`src/cmd/install.rs`), `cask_installs_by_name_with_cask_flag`.
  - `tap_formula_installs_by_bare_name_and_its_origin_record_is_removed` (`src/cmd/apply.rs`): real cache dir with `origins.toml` holding `"formula:packer" = "hashicorp/tap"`; after the run the visible args end with `packer` (not `hashicorp/tap/packer`) and the record is gone.
  - `dry_run_and_already_installed_keep_the_origin_record`.
  - Migrate `untrusted_tap_refusal_holds_the_target_and_never_runs_brew_trust` (`src/cmd/install.rs:353`) to the by-name args.
  - Replace `staged_load_failure_holds_with_note_and_does_not_fail_the_run` (`src/cmd/install.rs:263`) with `brew_load_error_by_name_is_brews_failure_and_the_run_continues`: brew's exit code is kept, no "staged copy" text, the next package runs.
  - `src/inventory.rs`: `by_name_keg_classifies_by_its_receipt_tap_ahead_of_a_stale_record` (receipt tap `hashicorp/tap`, record naming another tap); skip if an existing test already asserts exactly this, and say which.
- [ ] Implement: `pub fn brew_install_args(pkg: &PkgRef, token: &str, user_flags: &[String]) -> Vec<String>`; rename `record_staged_install` to `record_name_install` and `StagedRun` to `NameRun { Ran { failed }, Untrusted(String) }`; on `installed_now` call `forget_origin(kind, name)`; delete `record_origin`, `LoadFailure`, the staged-copy holds, `staging_dir`, and the `tap_root` field of `ApplySession`. Rename `run_is_soaked_install` to `run_is_name_install(runs, name)` (an `install` run whose last argument is `name`, without `--ignore-dependencies`) and update its callers.
- [ ] **Accept:** done gate passes; `rg -n 'write_blob|staging_dir|staged_load_failure' src/cmd` is empty. Commit.

### Task 5: One `brew update` before soaked work

**Files:** Modify `src/nosoak.rs` (`run_step`, line 143; its inline update at 163-176), `src/cmd/apply.rs` (`reinstall` at 35, `apply_many` at 177, `ApplySession` at 268, `run_nosoak` at 1103), and tests in `src/cmd/{apply,install,upgrade}.rs`, `src/nosoak.rs`, `src/lib.rs`.

Shapes (spec "`brew update` before soaked work"):

```rust
// nosoak.rs
/// Runs `brew update` under the `Updating Homebrew` header.
/// `None` when it succeeded, else brew's exit code.
pub fn brew_update(brew: &impl Brew, out: &mut impl Write) -> Result<Option<i32>, Error>;
/// `run_step`, skipping its own update (and header) when `updated`.
pub fn run_step_after(brew: &impl Brew, verb: &str, user_flags: &[String],
    targets: &[Target], updated: bool, out: &mut impl Write) -> Result<StepResult, Error>;
// `run_step` keeps its signature and calls `run_step_after(.., false, ..)`.

// apply.rs, ApplySession
/// `Some(true)`: this run's `brew update` succeeded. `Some(false)`: it failed. `None`: it did not run.
brew_updated: Option<bool>,
fn update_before_soaked_work(&mut self, names: &[String]) -> Result<(), Error>;
```

Rules:

- Soaked work means: a bare run with `plan_total` above 0, or a named run where `resolve_token` gives `PkgClass::Soaked` for at least one name.
- `apply_many` calls it after `validate_tokens` and the verbose banner, before `session.out.open()` and the package loop. `reinstall` calls it after its banner, before its loop. It writes through `self.out.get_mut()`, so the header is at column 0 ahead of the soaked section.
- On failure: `defer("brew update failed (exit <N>); brew's data may be older than the soak cutoff, so more packages may be held")`, `max_status(&mut self.brew_status, N)`, and continue. Nothing is held and `refused` is not set. The update's output is not fed to `self.done`.
- `run_nosoak` calls `run_step_after` with `self.brew_updated == Some(true)`.
- `cmd::update`, `outdated` and `info` do not change.

- [ ] **Failing tests first:**
  - `soaked_upgrade_runs_brew_update_before_asking_brew_anything` (`src/cmd/apply.rs`): a wrapper brew (the `FailDepsBrew` pattern) logs `run_visible_in`, `deps` and `by_name` calls in one list; the first entry is `update`, and it appears once.
  - Rewrite `upgrade_without_no_soak_packages_never_runs_brew_update` (`src/cmd/apply.rs:1975`) as `bare_upgrade_with_an_empty_plan_runs_no_brew_update`, and add `named_run_of_only_no_soak_packages_updates_in_the_no_soak_step` (one `update`, after no soaked run, as today).
  - `run_with_soaked_and_no_soak_work_updates_exactly_once`: one `["update"]` run, and the output has one `Updating Homebrew` header, above the soaked header.
  - `failed_brew_update_notes_it_and_the_package_still_installs`: queue `(1, b"Error: offline")` first; the package's `install` run still happens, the Notes hold the exact failure note, nothing is under `Held:`, `brew_status` is `Some(1)`, `refused` is false.
  - `failed_brew_update_is_retried_by_the_no_soak_step`: with a no-soak target, two `update` runs.
  - `dry_run_still_runs_brew_update` (`--dry-run`).
  - `install_and_reinstall_update_before_soaked_work` (`src/cmd/install.rs`): for `install` of a new soaked package and for a cutoff `reinstall`.
  - `src/nosoak.rs`: `brew_update_returns_the_exit_code_on_failure`, `run_step_after_an_update_runs_no_brew_update_and_prints_no_header`, and the existing `run_step` tests unchanged.
- [ ] Implement as above.
- [ ] Migrate: a soaked run now makes `update` its first visible run, so it takes the first queued `MockBrew::next_outputs` entry and sees `next_status`. Find the sites with `rg -n 'next_outputs|next_status' src/cmd src/lib.rs` (about a dozen) and `rg -n '"update"' src/cmd src/lib.rs`; prepend `(0, Vec::new())` to a queue, and extend exact run-list assertions with the leading `update`. Do not special-case `update` in `MockBrew`.
- [ ] **Accept:** done gate passes. Commit.

### Task 6: Sweep

**Files:** Modify `src/cmd/test_support.rs`, `src/brew.rs` (`MockBrew::deps`, line 943), remaining tests in `src/cmd/{apply,bare,deps,install,upgrade,view}.rs` and `src/lib.rs`.

- [ ] **Failing test first:** add `pub(super) fn assert_no_path_args(brew: &MockBrew)` (no recorded argument starts with `/`, ends with `.rb`, or is a `user/repo/name` token on an `install` or `deps` run) and `soaked_runs_never_hand_brew_a_path_or_full_token` in `src/cmd/apply.rs`, driving: a named upgrade with a missing and an upgrading dependency, a bare upgrade, a fresh install, a cutoff reinstall, a cask target, and a tap formula.
- [ ] Remove the `file_stem` fallback in `MockBrew::deps` (wrong for `python@3.13`); add `mock_deps_keys_by_exact_token`. Fix every test the removal breaks by keying `deps` on the bare name.
- [ ] Find leftovers: `rg -n '\.rb"\)|ends_with\("|/staging/|staged copy|path install' src/cmd src/lib.rs src/tap.rs`. Each hit is either a kept `pub` helper test in `tap.rs`/`taps.rs`, a git blob path, or gets migrated. List the hits you kept and why in the task report.
- [ ] Update the comment on `apply_brewsoak_brew_env` (`src/brew.rs:640-645`): `HOMEBREW_DEVELOPER` and the `FORBID` removal no longer enable anything; they stay until the long-term fix decides.
- [ ] **Accept:** done gate passes. Commit.

### Task 7: Documentation

**Files:** Modify `AGENTS.md`, `README.md`.

- [ ] `AGENTS.md`, "Installing soaked formulae": replace the two path-install bullets with the stopgap rules (install by bare name only behind `Brew::by_name`; hash the raw cutoff blob; never a path or a full token on a soaked run; holds are never fatal; pointer to the spec and issue #17). Keep the `--ignore-dependencies` bullet. Add the one-`brew update`-per-run rule (before soaked work; the no-soak step does not repeat a successful one). In "Origins and staging", change "write the origin record after every successful staged tap install" to the by-name rule, and note that `<cache>/staging/` is read (old receipts) but no longer written.
- [ ] `README.md`: a short "Homebrew 7.0.14 and later (issue #17)" section under "Commands": what is held and why, the note's wording, the bypass, and that `upgrade`, `install` and `reinstall` now run `brew update` once before soaked work (also on a dry run). Correct any sentence that says `brew update` runs only for no-soak packages.
- [ ] Check every claim against the code as merged. No private hostnames, paths or account names (the repo is public).
- [ ] **Accept:** done gate passes. Commit.

### Task 8: Verify against real Homebrew

- [ ] `cargo build`, then `./target/debug/brewsoak upgrade --dry-run ast-grep > <scratch>/after.txt 2>&1; echo "exit=$?" >> <scratch>/after.txt` (same package as Task 0).
- [ ] **Accept:** `after.txt` has no `No available formula`; it shows either brew's `Would upgrade` for the package or a `Held:` note with `issue #17`; exit is 0 or 1, never brew's error status. The newest `"$TMPDIR"/brewsoak-*.log` shows `$ brew update` before any `$ brew ruby` or `$ brew deps` line, and `$ brew install --formula --dry-run ast-grep` if the package was not held.
- [ ] Run `./target/debug/brewsoak upgrade --dry-run > <scratch>/bare.txt 2>&1` and report: packages planned, installed by name, held, and the wall time. The run must reach the Summary.
- [ ] Run the done gate and show its output. Report `before.txt`, `after.txt` and the bare-run numbers side by side.
- [ ] Do not push, open a PR, or close issue #17: the PR says `Refs #17`. Wait for the maintainer.
