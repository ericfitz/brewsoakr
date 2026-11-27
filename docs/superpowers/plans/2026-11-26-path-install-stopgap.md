# Path-Install Stopgap (issue #17) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `brewsoak upgrade|install|reinstall` stop passing staged file paths to brew. A soaked package is installed by bare name only when brew's own current definition is byte-identical to the soaked cutoff, for the package and for every dependency brew might touch; otherwise it is held with a note, and the run continues.

**Architecture:** A new `Brew::by_name` question answers, from `brew info --json=v2`, where a bare name resolves and whether its `ruby_source_checksum.sha256` equals the cutoff blob's SHA-256. `CutoffDepWalk` asks `brew deps` by name, gates every closure member with it, and reports a hold instead of an error. `install_cutoff` and `install_missing_dep` build `brew install --formula|--cask <name>`.

**Tech Stack:** Rust 2024. One new direct dependency, `ring` (already in `Cargo.lock` through `ureq`), for SHA-256.

**Spec:** `docs/superpowers/specs/2026-11-26-path-install-stopgap.md`. Read it first; every message string and rule below comes from it. The spec's "Open questions" are the maintainer's to answer: if an answer changes Choice 1 or Choice 2, stop and re-plan Tasks 2 to 4.

## Global Constraints

- Done gate (run before claiming any task done, show output): `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
- Read `AGENTS.md` first. Its "Installing soaked formulae" section still describes path installs until Task 6; the spec wins where they differ.
- Never pass `--ignore-dependencies`, a file path, or a `user/repo/name` token to a soaked `brew install` or `brew deps`. The no-soak step (`src/nosoak.rs`) and the `reinstall` true repair (`src/cmd/apply.rs:140-166`) keep their full tokens: do not touch them.
- Never run `brew trust`. Never add a `brew update`.
- No `#[allow(dead_code)]`. Delete crate-private code the compiler reports dead. Keep the `pub` staging helpers and their tests: `tap::write_blob`, `tap_formula_path`, `tap_cask_path`, `sanitize_unofficial`, `taps::staging_root`, `taps::staged_load_failure`. Keep `origin::staged_tap`, `origin::STAGING_TAP` and all of `inventory.rs`.
- Keep the `tap_root` parameter of `cmd::upgrade`, `cmd::install`, `cmd::reinstall` and `apply_many` (rename to `_tap_root`), so about 100 test call sites stay as they are. Remove the `tap_root` field from `ApplySession` and `CutoffDepWalk`.
- `MockBrew::by_name` answers "same source, origin tap, not installed-current" for every name it was not scripted for, so existing tests keep installing.
- Existing tests that assert `.rb` paths are migrated to assert the bare name, not deleted.
- American English. Match surrounding comment density. Use `rg PATTERN .` (always give a path).
- End commit messages with the attribution lines from the session's system reminder.

## Review Focus

1. No soaked code path hands brew a path or a full token (Task 5 sweep test).
2. The gate hashes the raw cutoff blob from `resolve_pkg_blobs`, not a staged or sanitized file, and never compares against brewsoak's HEAD blob.
3. A by-name dependency list is used only after that package passed the gate.
4. Every `Error::Brew` from the walk, `brew deps` or `by_name` becomes a hold of one target; the next package still runs; the run exits 1. Git and I/O errors stay fatal.
5. An installed dependency the run does not upgrade is walked through, and holds the target unless brew has it at brew's current version or it passes the gate.
6. Untrusted-tap refusals keep today's note and count the target once.
7. Counts: a held target takes back its `upgraded` count (`hold_target_for_dep`).
8. `origins.toml`: record removed after a real by-name install; untouched on a dry run and on brew's "already installed" answer.

---

### Task 0: Record the failing repro

- [ ] Before changing code, run `cargo build` and then `./target/debug/brewsoak upgrade --dry-run ast-grep > <scratch>/before.txt 2>&1; echo "exit=$?" >> <scratch>/before.txt`. If `ast-grep` is no longer outdated, pick a formula that `./target/debug/brewsoak outdated` lists.
- [ ] **Accept:** the file shows `No available formula with the name` and a non-zero exit. Keep it for Task 7. Nothing is committed.

### Task 1: SHA-256 helper

**Files:** Modify `Cargo.toml` (add `ring = "0.17"` under `[dependencies]`); create `src/digest.rs`; register `pub mod digest;` in `src/lib.rs`.

- [ ] **Failing tests first** (`src/digest.rs`): `sha256_hex_of_empty_input` (`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`), `sha256_hex_of_abc` (`ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`).
- [ ] Implement `pub fn sha256_hex(bytes: &[u8]) -> String` (lowercase hex) with `ring::digest::digest(&ring::digest::SHA256, bytes)`.
- [ ] **Accept:** `Cargo.lock` gains no new package (`git diff --stat Cargo.lock` shows only the `brewsoak` dependency list changing); done gate passes. Commit.

### Task 2: `Brew::by_name`

**Files:** Modify `src/brew.rs` (trait at line 61, `ProcessBrew` at 135 and 377, `brewsoak_brew_env_pairs` at 658, `MockBrew` at 253 and 788, `parse_installed_json` at 1176); create `tests/fixtures/brew-info-v2.json`; modify the delegating test brews at `src/lib.rs:672` and `src/cmd/deps.rs:502`, `:590`.

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
    /// Formula: `versions.stable`, plus `_<revision>` when it is above 0. Cask: `version`.
    pub version: String,
    /// `ruby_source_checksum.sha256` equals the query's `cutoff_sha256`.
    pub same_source: bool,
    /// The newest installed keg (or the installed cask) is at `version`.
    pub installed_current: bool,
}

impl ByName {
    /// A by-name install would load exactly the soaked cutoff of `origin`:
    /// `same_source`, and `tap` is `origin` (case-insensitive).
    pub fn installs_cutoff(&self, origin: &str) -> bool;
}

// trait Brew, no default body:
/// One answer per query, in order. `Err(text)` is brew's own failure for
/// that name (unknown name, untrusted tap). The outer `Err` is a spawn failure.
fn by_name(&self, queries: &[NameQuery]) -> Result<Vec<Result<ByName, String>>, Error>;
```

- [ ] **Failing tests first** (`src/brew.rs` tests):
  - `parse_current_rows_reads_formulae_and_casks`: from the fixture, `ast-grep` (stable `0.51.3`, revision 0, installed `0.51.1`, its checksum), a formula with revision 3 (version `1.5.2_3`), a row with no `ruby_source_checksum` (checksum `None`), a core-collision row (`cyclonedx-gomod`, tap `homebrew/core`), and a cask (`version`, string `installed`, `tap`). Build the fixture by trimming real `brew info --json=v2 --installed` output to those rows and the fields named here; it must contain no local paths.
  - `judge_same_source_needs_equal_checksum`, `judge_no_cutoff_is_never_same_source`, `judge_installed_current_compares_newest_keg_to_stable_plus_revision`, `installs_cutoff_needs_the_origin_tap` (case-insensitive).
  - `process_brew_by_name_asks_info_once_per_uninstalled_name` and `process_brew_by_name_reports_brew_failure_as_err_text`, with the fake `brew.sh` harness of `process_brew_deps_asks_brew_for_full_names` (line 1497): assert the logged args are `info --json=v2 --formula hello` exactly once for two queries, and that a script exiting 1 with stderr yields `Ok(vec![Err(text)])`.
  - `process_brew_deps_asks_once_per_token` (same harness, two `deps` calls, one log line).
  - `brew_children_never_force_api_auto_update`: `brewsoak_brew_env_pairs()` contains `("HOMEBREW_FORCE_API_AUTO_UPDATE", None)`.
  - `mock_by_name_defaults_to_same_source` and `mock_by_name_honors_scripted_answers`.
- [ ] Implement:
  - `pub(crate) struct CurrentRow { kind, name, tap, version, source_sha256: Option<String>, installed: Option<String> }`, `pub(crate) fn parse_current_rows(json: &str) -> Result<Vec<CurrentRow>, Error>` (reuse `json_objects_in_array`, `find_json_key`, `json_string_value`, `installed_version`; add a small integer reader for `revision`), and `pub(crate) fn judge(row: &CurrentRow, q: &NameQuery) -> ByName`.
  - `ProcessBrew`: `current: Mutex<HashMap<(&'static str, String), Result<CurrentRow, String>>>`, filled from the same JSON inside `installed_packages` and by `by_name` misses (`info --json=v2 --formula|--cask <name>`, one call per name, failures remembered). `deps_memo: Mutex<HashMap<(&'static str, String), Vec<String>>>` in `deps` (successes only).
  - `MockBrew`: `pub by_name: BTreeMap<String, Result<ByName, String>>` (keyed by name) and `pub by_name_queries: Mutex<Vec<NameQuery>>`. An unscripted name answers `Ok(ByName { tap: q.origin.clone(), version: String::new(), same_source: true, installed_current: false })`. `ProcessBrew` reads the tap from brew and ignores `q.origin`.
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
3. Ask `by_name` with `sha256_hex` of the cutoff blob from `resolve_pkg_blobs` (`None` when there is none). `Root` and `Install` need `installs_cutoff(origin)`. `Check` passes on `installed_current || installs_cutoff(origin)`. An installed dependency that would be `Install` but is `installed_current` without `installs_cutoff` is treated as `Check`. A failure sets `self.by_name` (`dep: None` for `Root`) and returns. An `Err(text)` answer is `Untrusted` when `taps::untrusted_tap(text)` matches, else `Lookup`.
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
  - `name_resolving_to_another_tap_holds_the_target`: tap formula `cyclonedx-gomod`, answer tap `homebrew/core`; note has `brew resolves the name to homebrew/core, not cyclonedx/cyclonedx` and the bypass token `cyclonedx/cyclonedx/cyclonedx-gomod`.
  - `held_target_takes_back_its_upgraded_count`: Summary shows it under held, not upgraded.
  - `dry_run_reports_the_same_hold` (`--dry-run` flag).
  - `cask_target_is_gated_by_name` (`--cask` deps token is the bare cask token).
  - Migrate, do not delete: `deps_failure_on_staged_tap_formula_holds_with_note` (now the `Deps` hold, keyed by bare name), `deps_untrusted_tap_refusal_holds_with_trust_note`, `untrusted_tap_refusal_on_a_dep_holds_the_dep`; each still asserts no `trust` run.
- [ ] Implement as above. Staged installs are still built by `install_cutoff` in this task; tests that asserted the walk staged dependency files move their assertion to Task 4.
- [ ] **Accept:** done gate passes; `rg -n 'write_cutoff_blob' src` is empty. Commit.

### Task 4: Install by name; origin bookkeeping

**Files:** Modify `src/tap.rs` (`brew_install_args`, line 41, and its tests), `src/cmd/apply.rs` (`install_cutoff`, `install_missing_dep` 798-909, `record_staged_install` 1051-1089, `record_origin` 1032-1046, `StagedRun` 1172-1180, `hold_staged_copy`/`hold_target_staged_copy`, `staged_install_flags`, `staging_dir` 1203, the `tap_root` field), `src/cmd/deps.rs` (drop its `tap_root` field), `src/cmd/test_support.rs` (`run_is_soaked_install`, line 215).

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
- [ ] Implement: `pub fn brew_install_args(pkg: &PkgRef, token: &str, user_flags: &[String]) -> Vec<String>`; rename `record_staged_install` to `record_name_install` and `StagedRun` to `NameRun { Ran { failed }, Untrusted(String) }`; on `installed_now` call `forget_origin(kind, name)`; delete `record_origin`, `LoadFailure`, the staged-copy holds, `staging_dir`, and the `tap_root` fields. Rename `run_is_soaked_install` to `run_is_name_install(runs, name)` (an `install` run whose last argument is `name`, without `--ignore-dependencies`) and update its callers.
- [ ] **Accept:** done gate passes; `rg -n 'write_blob|staging_dir|staged_load_failure' src/cmd` is empty. Commit.

### Task 5: Sweep

**Files:** Modify `src/cmd/test_support.rs`, `src/brew.rs` (`MockBrew::deps`, line 943), remaining tests in `src/cmd/{apply,bare,deps,install,upgrade,view}.rs` and `src/lib.rs`.

- [ ] **Failing test first:** add `pub(super) fn assert_no_path_args(brew: &MockBrew)` (no recorded argument starts with `/`, ends with `.rb`, or is a `user/repo/name` token on an `install` or `deps` run) and `soaked_runs_never_hand_brew_a_path_or_full_token` in `src/cmd/apply.rs`, driving: a named upgrade with a missing and an upgrading dependency, a bare upgrade, a fresh install, a cutoff reinstall, a cask target, and a tap formula.
- [ ] Remove the `file_stem` fallback in `MockBrew::deps` (wrong for `python@3.13`); add `mock_deps_keys_by_exact_token`. Fix every test the removal breaks by keying `deps` on the bare name.
- [ ] Find leftovers: `rg -n '\.rb"\)|ends_with\("|/staging/|staged copy|path install' src/cmd src/lib.rs src/tap.rs`. Each hit is either a kept `pub` helper test in `tap.rs`/`taps.rs`, a git blob path, or gets migrated. List the hits you kept and why in the task report.
- [ ] Update the comment on `apply_brewsoak_brew_env` (`src/brew.rs:640-645`): `HOMEBREW_DEVELOPER` and the `FORBID` removal no longer enable anything; they stay until the long-term fix decides.
- [ ] **Accept:** done gate passes. Commit.

### Task 6: Documentation

**Files:** Modify `AGENTS.md`, `README.md`.

- [ ] `AGENTS.md`, "Installing soaked formulae": replace the two path-install bullets with the stopgap rules (install by bare name only behind `Brew::by_name`; hash the raw cutoff blob; never a path or a full token on a soaked run; holds are never fatal; pointer to the spec and issue #17). Keep the `--ignore-dependencies` bullet. In "Origins and staging", change "write the origin record after every successful staged tap install" to the by-name rule, and note that `<cache>/staging/` is read (old receipts) but no longer written.
- [ ] `README.md`: a short "Homebrew 7.0.14 and later (issue #17)" section under "Commands": what is held and why, the note's wording, the bypass, and "run `brew update` if brew's data is older than the cutoff".
- [ ] Check every claim against the code as merged. No private hostnames, paths or account names (the repo is public).
- [ ] **Accept:** done gate passes. Commit.

### Task 7: Verify against real Homebrew

- [ ] `cargo build`, then `./target/debug/brewsoak upgrade --dry-run ast-grep > <scratch>/after.txt 2>&1; echo "exit=$?" >> <scratch>/after.txt` (same package as Task 0).
- [ ] **Accept:** `after.txt` has no `No available formula`; it shows either brew's `Would upgrade` for the package or a `Held:` note with `issue #17`; exit is 0 or 1, never brew's error status. `tail -5 "$TMPDIR"/brewsoak-*.log` shows `$ brew install --formula --dry-run ast-grep`.
- [ ] Run `./target/debug/brewsoak upgrade --dry-run > <scratch>/bare.txt 2>&1` and report: packages planned, installed by name, held, and the wall time. The run must reach the Summary.
- [ ] Run the done gate and show its output. Report `before.txt`, `after.txt` and the bare-run numbers side by side.
- [ ] Do not push, open a PR, or close issue #17: the PR says `Refs #17`. Wait for the maintainer.
