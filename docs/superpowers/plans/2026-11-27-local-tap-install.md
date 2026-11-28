# Local-Tap Installs for Soaked Packages (issue #17) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A soaked `homebrew/core` formula or `homebrew/cask` cask installs at its cutoff whether or not brew's current file is the cutoff: brewsoak writes the cutoff file into its own local tap, `ericfitz/brewsoak`, and installs the package from that tap by full token.

**Architecture:** A new module, `src/soaktap.rs`, owns the soak tap: its state (from `brew tap-info`), the tap files, an undo directory in the cache that makes every tap write reversible, the settle step that keeps or puts back each file according to the keg brew left behind, recovery after an interrupted run, and pruning. `CutoffDepWalk` (`src/cmd/deps.rs`) writes a member's cutoff file before it asks `brew deps` about it and applies the spec's dependency table; `ApplySession` (`src/cmd/apply.rs`) installs dependencies and the target with `brew install|upgrade ... ericfitz/brewsoak/<name>` and settles after each target. Third-party tap packages keep the 1.6.1 by-name gate unchanged.

**Tech Stack:** Rust 2024, no new dependencies. Tests use `MockBrew`, `InMemoryGit` and temp directories.

**Spec:** `docs/superpowers/specs/2026-11-27-local-tap-install-design.md` (commit `4fb9cbf`). Read it first. Its "Human decisions" 1 to 6 are frozen. Every message string below is copied from it. Any gap found while building is raised as a design change, not filled by a guess.

## Global Constraints

- Done gate (run before claiming any task done, show its output): `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
- Read `AGENTS.md` first. Its "Installing soaked formulae" section describes the 1.6.1 stopgap until Task 12; this plan and the spec win where they differ.
- The soak tap is `ericfitz/brewsoak` (`origin::SOAK_TAP`). The legacy tap is `brewsoakr/soaked` (`origin::STAGING_TAP`): recognized by name, never written, pruned or installed from. The GitHub repository `ericfitz/homebrew-brewsoak` must never be created.
- Never run `brew trust` or `brew untrust`, never set `HOMEBREW_NO_REQUIRE_TAP_TRUST`, never edit Homebrew's trust store.
- Never pass `--ignore-dependencies`, never edit a keg receipt, never uninstall a keg to move it between taps, never hand brew a file path.
- Only soaked packages whose origin is `homebrew/core` or `homebrew/cask` use the soak tap. A third-party file is never written into it, and no third-party command carries its name. Third-party packages keep the by-name gate and bare names.
- No command that carries `ericfitz/brewsoak/<name>` is issued unless `SoakTap::ready` returned `Ok` in this run (tap local and trusted).
- Under `$(brew --repository)/Library/Taps` brewsoak writes and deletes only `.rb` files below `Formula/` and `Casks/` of its own tap, and only while holding a `&RunLock`. `prune::prune_cache` keeps its signature and contract.
- Undo entries live in `<cache>/soak-tap-undo/{Formula,Casks}/`: `<name>.rb` (previous bytes) or an empty `<name>.absent`.
- A missing tap directory or undo directory reads as empty: never an error, never created by a read.
- Holds are never fatal: the next package still runs and the run exits 1. Git and I/O errors stay fatal.
- Minimum Homebrew is 7.0.14, documented only. No run-time version check.
- Every unit test of a command or of the soak-tap module uses `MockBrew`. The only other brew stand-in is the existing fake `brew` shell script for `ProcessBrew`'s own argument plumbing (`fake_loader_brew`, `src/brew.rs:3143`). No test runs real brew or touches the network.
- No `#[allow(dead_code)]`. Existing tests are migrated, not deleted, unless a step names the test to remove.
- American English. Match the surrounding comment density. Use `rg PATTERN .` (always give a path).
- End commit messages with the attribution lines from your own session's system reminder.

Decisions this plan makes where the spec left the choice to the builder (do not change them in a task):

- **P1, tap directory.** The directory is the `path` that tap-info reports for `ericfitz/brewsoak`; when the entry has no `path`, it is the `tap_root` parameter (`World::tap_root`). One function decides: `soaktap::tap_dir`.
- **P2, anchored.** Not stored. `soaktap::anchored(dir, pkg)` reads the tap file when asked. The stored fact is the receipt: `Pkg.own_tap`.
- **P3, MockBrew defaults.** An unscripted `MockBrew` reports a local, trusted soak tap with no `path` (`soak_tap`), answers `receipt_tap` with `None` (so every tap file is put back), `satisfied: false`, and answers `deps` for `ericfitz/brewsoak/<name>` from the entry for `<name>`. Existing install tests rely on all four.
- **P4, own class.** `TapClass::SoakTap` is its own variant; `TapClass::Staging` stays for the legacy tap.
- **P5, lock.** `cmd::upgrade`, `cmd::install`, `cmd::reinstall` and `apply_many` take `lock: &RunLock` as their first parameter.

## Review Focus

1. Cutoff bytes that are not tidy text: CRLF line endings, no final newline, non-UTF-8 bytes. With no `no_autobump!` line the tap file must equal the blob byte for byte (Task 4 pins it).
2. Names that are not plain words: `python@3.12`, `c++filt`, and a formula and a cask that share a name. Tap file, undo entry, token and prune must treat each as its own package (Tasks 4, 5, 6).
3. An undo directory in a state brewsoak never wrote: both `<name>.rb` and `<name>.absent`, a stale temp file, an unknown file. Recovery must restore the `.rb` bytes, leave unknown files alone and stay idempotent (Task 6).
4. A symlink or a nested directory under `Formula/` at prune time. Prune removes a stale `.rb` link itself, never its target, and never follows a linked directory (Task 6).
5. `brewsoak install -g <installed formula>`: `-g` means `--git` to `brew install` and `--greedy` to `brew upgrade`. The soak-tap command for an installed formula is `brew upgrade`, and must not receive `-g` (Task 4).

## File Structure

| File | Responsibility after this plan |
|---|---|
| `src/origin.rs` | `SOAK_TAP`, `OwnTap`, `own_tap`; origin ignores both of brewsoak's tap names |
| `src/taps.rs` | `TapInfo` with `path` and `trusted`; `TapClass::SoakTap`; `staging_root`, `staged_load_failure` removed |
| `src/brew.rs` | `tap_new`, `receipt_tap`, the `satisfied` loader column, receipt facts, the Cellar scan, MockBrew additions |
| `src/tap.rs` | Byte-exact tap files, atomic `write_blob`, `soak_token`, `soak_tap_args` |
| `src/atomic.rs` | `write_bytes` |
| `src/soaktap.rs` (new) | State, `NotReady` notes, `SoakTap` (ready, stage, settle), `anchored`, `installed_dep_rule`, `recover`, `prune`, `maintain` |
| `src/inventory.rs` | `Pkg.own_tap`, `Inventory.soak_tap` |
| `src/cmd/deps.rs`, `src/cmd/apply.rs`, `src/cmd/notes.rs` | Per-origin member rules, installs through the tap, settle, holds, bypass hints |
| `src/nosoak.rs` | Official token for a package installed from the soak tap |
| `src/quiet.rs` | Bare names on the terminal; `Trusted` lines to the log only |
| `src/lib.rs`, `src/cmd/{upgrade,install,update,outdated,info}.rs` | Lock and tap directory threading; maintenance points; the tap-not-ready note |
| `README.md`, `AGENTS.md`, three older specs | Rules, user documentation, supersession notes |

---

### Task 1: Names, tap-info fields and receipt facts

No behavior changes for any package on a machine without the soak tap.

**Files:**
- Modify: `src/origin.rs:11-12` (constants), `src/origin.rs:140-160` (`resolve_origin`)
- Modify: `src/taps.rs:11-66` (`TapInfo`, `parse_tap_info_json`, `TapClass`, `classify`); delete `src/taps.rs:74-82` (`staging_root`) and `src/taps.rs:104-113` (`staged_load_failure`) with their tests (`clone_and_staging_dirs_are_per_tap` keeps its clone half; `staged_load_failure_matches_ruby_load_errors` goes)
- Modify: `src/brew.rs:15-27` (`InstalledPkg`), `src/brew.rs:1328-1368` (`ReceiptSource`, `read_formula_receipt_source_opt`), `src/brew.rs:1481-1530` (`parse_installed_json`)
- Modify: `src/inventory.rs:19-37` (`Pkg`, `Inventory`), `src/inventory.rs:51-89` (`build_in`)
- Test: the `tests` modules of those four files

**Interfaces:**
- Consumes: nothing from this plan.
- Produces:

```rust
// src/origin.rs
pub const SOAK_TAP: &str = "ericfitz/brewsoak";
/// Which of brewsoak's own tap names a receipt carries. Neither says anything about origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnTap { Soak, Legacy }
/// ASCII case-insensitive exact match of `SOAK_TAP` or `STAGING_TAP`.
pub fn own_tap(tap: &str) -> Option<OwnTap>;

// src/taps.rs
pub struct TapInfo {
    pub name: String,
    pub remote: Option<String>,
    /// `path` from tap-info; `None` when absent or empty.
    pub path: Option<PathBuf>,
    /// `trusted` from tap-info: absent is `true`, `true` is `true`, anything else is `false`.
    pub trusted: bool,
}
impl TapInfo { pub fn new(name: &str, remote: Option<&str>) -> Self } // lowercases name; path None; trusted true
pub enum TapClass { Core, Cask, Staging, SoakTap, Unsoakable, Soakable }

// src/brew.rs
pub struct InstalledPkg { /* existing fields */ pub own_tap: Option<OwnTap> }
pub struct ReceiptSource { pub tap: Option<String>, pub path: Option<String>, pub own_tap: Option<OwnTap> }

// src/inventory.rs
pub struct Pkg { /* existing fields */ pub own_tap: Option<OwnTap> }
pub struct Inventory { /* existing fields */ pub soak_tap: Option<TapInfo> } // the tap-info entry named SOAK_TAP
```

- [ ] **Step 1: Write the failing tests**

- `origin::tests::own_tap_matches_both_names_whatever_the_case`: `own_tap("EricFitz/Brewsoak") == Some(OwnTap::Soak)`, `own_tap("brewsoakr/soaked") == Some(OwnTap::Legacy)`, `own_tap("ericfitz/tap") == None`.
- `origin::tests::resolve_origin_ignores_a_soak_tap_receipt`: receipt `Some("ericfitz/brewsoak")`, no record, formula gives `"homebrew/core"`; with a record `acme/tools` gives `"acme/tools"`.
- `taps::tests::parse_tap_info_reads_path_and_trusted`: entries with `"trusted": true`, `false`, no key, `"yes"`, `1` and `null` parse to `true, false, true, false, false, false`; `"path": "/x/Taps/ericfitz/homebrew-brewsoak"` parses to that `PathBuf`; a missing `path` to `None`.
- `taps::tests::classify_names_the_soak_tap_before_the_remote_rule`: `ericfitz/brewsoak` is `SoakTap` with `remote: None` and with an `https://` remote; `brewsoakr/soaked` is still `Staging`.
- `taps::tests::cloned_ericfitz_tap_stays_soakable`: `ericfitz/tap` with `https://github.com/ericfitz/homebrew-tap` is `Soakable`.
- `brew::tests::receipt_naming_an_own_tap_has_no_tap_and_keeps_the_fact`: a keg receipt with `"source": {"tap": "ericfitz/brewsoak"}` gives `tap: None, own_tap: Some(Soak)`; `brewsoakr/soaked` gives `Some(Legacy)`; `homebrew/core` gives `tap: Some("homebrew/core"), own_tap: None`.
- `brew::tests::receipt_tap_that_is_not_a_string_is_not_from_the_soak_tap`: `"tap": 7`, `"tap": {}`, `"tap": true` each give `tap: None, own_tap: None`.
- `brew::tests::parse_installed_json_cask_keeps_the_own_tap_fact`: a cask object with `"tap": "ericfitz/brewsoak"` gives `tap: None, own_tap: Some(Soak)`.
- `inventory::tests::soak_tap_receipt_defaults_to_core_and_is_soaked`: a formula with `own_tap: Some(Soak)` has origin `homebrew/core`, class `Soaked`, `Pkg.own_tap == Some(Soak)`; a cask has origin `homebrew/cask`.
- `inventory::tests::a_no_soak_entry_naming_the_soak_tap_matches_no_package`: with `NO_SOAK = ["ericfitz/brewsoak"]` that formula is still `Soaked`.
- `inventory::tests::ericfitz_tap_packages_keep_their_origin_beside_the_soak_tap`: taps `ericfitz/tap` (https) and `ericfitz/brewsoak` (local); a formula with receipt `ericfitz/tap` has origin `ericfitz/tap`, class `Soaked`; `inv.tap_class("ericfitz/brewsoak") == Some(TapClass::SoakTap)`; `inv.soak_tap` is the soak-tap entry; `class_for("ericfitz/brewsoak", "x", &cfg) == PkgClass::Unsoakable`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test own_tap soak_tap trusted 2>&1 | tail -20`
Expected: compile errors (`SOAK_TAP`, `own_tap`, missing fields).

- [ ] **Step 3: Implement**

Add the items above. `resolve_origin` skips a receipt tap for which `own_tap` is `Some` (today it skips only `STAGING_TAP`, `src/origin.rs:151`). Both receipt readers (`src/brew.rs:1359-1360` and `1508-1510`) set `own_tap` from the raw string and erase the tap for both names. `trusted` needs "absent" told apart from "not a boolean": use `brew::find_json_key`. Fix the doc comment at `src/origin.rs:11` (it calls the legacy tap "brewsoak's own local tap").

Every struct literal of the changed types must be updated. Find them with `rg -n 'TapInfo \{|InstalledPkg \{|Pkg \{' src`. Tap literals become `TapInfo::new(..)`; package literals get `own_tap: None`.

- [ ] **Step 4: Run the tests, then the done gate**

Run: `cargo test 2>&1 | rg '^test result'` then the done gate.
Expected: all pass; no test other than the two named above was removed.

- [ ] **Step 5: Commit**

```bash
git add src/origin.rs src/taps.rs src/brew.rs src/inventory.rs src/lib.rs src/prune.rs src/nosoak.rs src/cmd
git commit -m "feat: recognize brewsoak's own tap in tap-info, receipts and the inventory (#17)"
```

---

### Task 2: Brew questions and MockBrew groundwork

**Files:**
- Modify: `src/brew.rs:67-80` (`BY_NAME_RUBY`), `src/brew.rs:96-125` (`ByName`, `Loaded`), `src/brew.rs:157-203` (`parse_by_name_row`, `judge`), `src/brew.rs:205-279` (trait `Brew`), `src/brew.rs:415-547` (`MockBrew`), `src/brew.rs:1077-1267` (`impl Brew for MockBrew`), `impl Brew for ProcessBrew` (`src/brew.rs:549`)
- Modify: `src/cmd/test_support.rs:44-58` (`inv_from`, `tapped`), `src/cmd/test_support.rs:96-98` (`unused_cache`)
- Test: `src/brew.rs` tests (`fake_loader_brew` at `:3143` gains the column)

**Interfaces:**
- Consumes: `origin::SOAK_TAP`, `TapInfo::new` (Task 1).
- Produces:

```rust
// trait Brew, both required
/// `brew tap-new --no-git <tap>`. `Err` is the first non-empty line of brew's
/// error, or why brew could not start.
fn tap_new(&self, tap: &str) -> Result<(), String>;
/// Lowercase `source.tap` of the receipt of `name` installed at exactly
/// `version`. Formula: `<cellar>/<name>/<version>/INSTALL_RECEIPT.json`.
/// Cask: `<caskroom>/<name>/.metadata/INSTALL_RECEIPT.json`, only when its
/// `source.version` is `version`. `None` when there is no such keg or
/// receipt or the value is not a string. The soak tap's name is not erased.
fn receipt_tap(&self, kind: PkgKind, name: &str, version: &str) -> Option<String>;

pub struct ByName { /* existing fields */
    /// Formula: brew's `Dependency.new(name).installed?`. Cask: same as `installed_current`.
    pub satisfied: bool }

// MockBrew, new pub fields
pub soak_tap: Option<TapInfo>,        // default Some(TapInfo::new(SOAK_TAP, None)); None means not tapped
pub tap_new_error: Option<String>,    // makes tap_new fail with this text
pub tap_created: Mutex<bool>,         // set by a successful tap_new
pub receipt_taps: BTreeMap<(String, String), String>, // (name, version) -> tap
pub on_call: Mutex<Option<Box<dyn FnMut(&[String]) + Send>>>,

// src/cmd/test_support.rs
pub(super) fn test_cache() -> &'static Path;                  // replaces unused_cache()
pub(super) fn soak_tap_at(path: &Path, trusted: bool) -> TapInfo; // local entry with that path
```

MockBrew behavior to implement exactly:
- `tap_info()` returns `taps`, then the soak-tap entry unless `taps` already has one named `SOAK_TAP`: `soak_tap` when `Some`; otherwise, once `tap_created` is set, a local entry with `trusted: false`.
- `tap_new` pushes `["tap-new", "--no-git", tap]` to `runs`, then fails with `tap_new_error` or sets `tap_created`.
- `deps` and `deps_errors` look up the exact token first; for a token `ericfitz/brewsoak/<name>` they then use the entry for `<name>`.
- `on_call` runs before the answer of `run`, `run_visible_in` and `tap_new` (their args) and of `deps` (`["deps", "--formula"|"--cask", token]`).
- The unscripted `by_name` answer has `satisfied: false`.

Loader row: `OK kind name tap version sum installed_current satisfied` (8 tab-separated fields). A 7-field `OK` row is `Err("unparseable brew loader row: ...")`.

- [ ] **Step 1: Write the failing tests** (all in `src/brew.rs`)

- `by_name_script_asks_brews_own_dependency_test`: `BY_NAME_RUBY.contains("Dependency.new(name).installed?")`.
- `parse_by_name_lines_reads_the_satisfied_column`: an 8-field row ending `false\ttrue` gives `installed_current == false`, `satisfied == true`.
- `parse_by_name_row_rejects_a_row_without_the_satisfied_column`: a 7-field `OK` row is `Err`.
- `process_brew_tap_new_passes_no_git_and_reports_brews_first_line`: a fake brew logs its args and, when a marker file exists, prints `Error: Tap is already installed!\nmore` to stderr and exits 1. Assert the logged args are `tap-new --no-git ericfitz/brewsoak`, `Ok(())` without the marker, `Err("Error: Tap is already installed!")` with it.
- `tap_new_child_sets_homebrew_developer`: `env_of("tap-new")` contains `("HOMEBREW_DEVELOPER", Some("1"))` and no key `HOMEBREW_NO_REQUIRE_TAP_TRUST`.
- `receipt_tap_reads_the_exact_keg_and_keeps_the_soak_tap_name`: temp Cellar with `wget/1.0_1/INSTALL_RECEIPT.json` naming `ericfitz/brewsoak`: `Some("ericfitz/brewsoak")` for `1.0_1`, `None` for `1.0`.
- `cask_receipt_tap_needs_the_same_source_version`: `.metadata/INSTALL_RECEIPT.json` with `source: {tap: "ericfitz/brewsoak", version: "3.003"}`: `Some` for `3.003`, `None` for `3.002`.
- `receipt_tap_is_none_for_a_non_string_tap`: `"tap": null` and `"tap": 3` give `None`.
- `mock_tap_info_appends_the_default_soak_tap`: `MockBrew::new().tap_info()` has one entry: `ericfitz/brewsoak`, `remote: None`, `trusted: true`, `path: None`. With `soak_tap: None` it is empty.
- `mock_tap_new_records_then_reports_an_untrusted_local_tap`: with `soak_tap: None`, after `tap_new` the `runs` hold `["tap-new","--no-git","ericfitz/brewsoak"]` and `tap_info()` has the entry with `trusted: false`. With `tap_new_error: Some("Error: nope".into())` it returns that `Err` and `tap_info()` stays empty.
- `mock_deps_answers_a_soak_tap_token_from_the_bare_entry`: `deps["alpha"] = ["beta"]`; `deps(Formula, "ericfitz/brewsoak/alpha") == ["beta"]`; an exact entry for the full token wins (update `mock_deps_keys_by_exact_token` to match).
- `mock_on_call_sees_runs_and_deps`: the hook records `["install","x"]` from `run_visible` and `["deps","--formula","x"]` from `deps`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib brew:: 2>&1 | tail -20`
Expected: compile errors for the missing methods and fields.

- [ ] **Step 3: Implement**

`ProcessBrew::tap_new` uses `self.run`. `ProcessBrew::receipt_tap` reads from `self.cellar()` and `self.caskroom()`. In `BY_NAME_RUBY` the formula row appends `Dependency.new(name).installed?`; when brew raises there (an untrusted tap), the existing `rescue` turns the whole row into `ERR`, which is intended.

`test_cache()` returns a writable directory that is the same for one test and removed when the test's thread ends: a `thread_local!` `tempfile::TempDir` whose path is leaked as `&'static Path`. Rename `unused_cache` to `test_cache` in every file (`rg -l unused_cache src`). `inv_from` builds from `brew.tap_info()`; so do the direct `Inventory::build(.., &brew.taps, ..)` calls in `src/cmd/apply.rs` and `src/cmd/install.rs` tests.

- [ ] **Step 4: Run the tests, then the done gate.** Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/brew.rs src/cmd
git commit -m "feat: add tap-new, receipt and dependency-test questions to Brew (#17)"
```

---

### Task 3: Formulae brew leaves out of its JSON

Spec "An untrusted tap that already has packages", "Reading the inventory".

**Files:**
- Modify: `src/brew.rs:750-781` (`ProcessBrew::installed_packages`); new function beside `parse_installed_json` (`src/brew.rs:1481`)
- Test: `src/brew.rs` tests

**Interfaces:**
- Consumes: `InstalledPkg.own_tap`, `ReceiptSource` (Task 1); `Brew::compare_versions` (existing, `src/brew.rs:248`).
- Produces:

```rust
/// Formulae with a directory in `cellar` that `listed` lacks and whose newest
/// keg has a receipt naming the soak tap. `newest` picks the newest of several
/// keg versions; `None` leaves the formula out.
pub(crate) fn soak_tap_kegs_brew_left_out(
    cellar: &Path,
    listed: &[InstalledPkg],
    newest: impl FnMut(&[String]) -> Option<String>,
) -> Vec<InstalledPkg>;
```

Each added package: `kind: Formula`, the keg's `.brew/<name>.rb` as `receipt_rb`, `pinned: false`, `tap: None`, `staged_path: None`, `own_tap: Some(OwnTap::Soak)`. A formula with one keg does not call `newest`.

- [ ] **Step 1: Write the failing tests** (temp Cellar; helper `write_receipt` exists at `src/brew.rs:2262`)

- `left_out_keg_from_the_soak_tap_is_added_from_disk`: `hello/2.12.2` with a soak-tap receipt and `.brew/hello.rb`, `listed` empty: one package with the fields above.
- `listed_formula_is_not_added_twice`.
- `left_out_keg_from_another_tap_is_ignored`: receipts naming `acme/tools`, `brewsoakr/soaked`, `null`, and `"tap": 7` add nothing.
- `left_out_keg_without_its_formula_copy_is_ignored`.
- `newest_keg_decides_when_there_are_several`: `hello/2.12.2` (soak tap) and `hello/2.12.3` (`homebrew/core`); `newest` returns `2.12.3`: nothing added. With `newest` returning `2.12.2`: added. With `None`: nothing added.
- `versioned_names_and_stray_files_are_handled`: `python@3.12/3.12.7` is added under that name; a plain file in the Cellar and a missing Cellar directory add nothing and do not fail.

- [ ] **Step 2: Run them.** `cargo test --lib left_out 2>&1 | tail` Expected: compile error.

- [ ] **Step 3: Implement**, and call it from `ProcessBrew::installed_packages` after `parse_installed_json`, before `installed_names` is recorded. There `newest` folds the versions with `self.compare_versions(&[(PkgKind::Formula, a, b)])` and returns `None` on an error. A non-zero `brew info` stays fatal (unchanged).

- [ ] **Step 4: Run the tests, then the done gate.**

- [ ] **Step 5: Commit**

```bash
git add src/brew.rs
git commit -m "feat: read soak-tap kegs brew hides from its inventory (#17)"
```

---

### Task 4: Byte-exact tap files and the soak-tap command

**Files:**
- Modify: `src/tap.rs:15-57` (`write_blob`, `sanitize_unofficial`, new functions), `src/atomic.rs:10-31`
- Test: `src/tap.rs`, `src/atomic.rs` tests

**Interfaces:**
- Consumes: `origin::SOAK_TAP`; `flags::filter_for_verb` (`src/flags.rs:160`), `flags::without_greedy` (`src/flags.rs:272`); `tap::is_stripped_flag` (`src/tap.rs:61`).
- Produces:

```rust
// src/atomic.rs
pub fn write_bytes(path: &Path, body: &[u8]) -> Result<(), Error>; // `write` calls it

// src/tap.rs
/// The cutoff blob with every line removed whose first non-blank text is
/// `no_autobump!`. Every other byte, line endings included, is unchanged.
pub fn sanitize_unofficial(rb: &[u8]) -> Vec<u8>;
/// Writes `sanitize_unofficial(blob)` to `Formula/<name>.rb` or `Casks/<name>.rb`
/// through `.<name>.rb.tmp-<pid>` and a rename.
pub fn write_blob(tap_root: &Path, pkg: &PkgRef, blob: &[u8]) -> Result<PathBuf, Error>;
/// `ericfitz/brewsoak/<name>`.
pub fn soak_token(name: &str) -> String;
/// `upgrade` for an installed formula, else `install`.
pub fn soak_tap_verb(kind: PkgKind, installed: bool) -> &'static str;
/// `brew <verb> --formula|--cask [flags] ericfitz/brewsoak/<name>`, flags
/// filtered for that verb.
pub fn soak_tap_args(pkg: &PkgRef, installed: bool, user_flags: &[String]) -> Vec<String>;
```

`soak_tap_args` drops `is_stripped_flag` words, applies `flags::without_greedy` when the verb is `upgrade`, then keeps `filter_for_verb(verb, ..).kept`. `brew_install_args` stays for third-party by-name installs.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn sanitize_leaves_a_blob_without_the_stanza_byte_for_byte() {
    for blob in [&b"a\r\nb"[..], b"class X < Formula\nend", b"\xff\xfe raw\n", b""] {
        assert_eq!(sanitize_unofficial(blob), blob);
    }
}
#[test]
fn sanitize_removes_only_the_stanza_line() {
    let rb = b"class S < Formula\r\n  no_autobump! because: :bumped_by_upstream\r\n  # no_autobump! stays\r\nend";
    assert_eq!(sanitize_unofficial(rb), b"class S < Formula\r\n  # no_autobump! stays\r\nend");
}
```

- `write_blob_writes_the_sanitized_bytes_and_leaves_no_temp_file`: the directory holds exactly `wget.rb`.
- `write_blob_replaces_an_existing_file` and `write_blob_paths_for_odd_names`: `Formula/python@3.12.rb`, `Formula/c++filt.rb`, `Casks/font-hack.rb`; a formula and a cask named `docker` land in different directories.
- `soak_tap_args_follow_the_installed_state`: absent formula `["install","--formula","ericfitz/brewsoak/wget"]`; installed formula `["upgrade","--formula","ericfitz/brewsoak/wget"]`; cask, installed or not, `["install","--cask","ericfitz/brewsoak/firefox"]`.
- `soak_tap_args_filter_flags_for_the_verb_run`: flags `["--greedy","--dry-run","--git","--ignore-dependencies"]` give `["upgrade","--formula","--dry-run","ericfitz/brewsoak/wget"]` on an installed formula and `["install","--formula","--dry-run","--git","ericfitz/brewsoak/wget"]` on an absent one.
- `soak_tap_upgrade_never_receives_g` (Review Focus 5): on an installed formula, flags `["-g"]` give exactly `["upgrade","--formula","ericfitz/brewsoak/wget"]` and flags `["-vg"]` give exactly `["upgrade","--formula","-v","ericfitz/brewsoak/wget"]`.
- `atomic::tests::write_bytes_round_trips_non_utf8`.

- [ ] **Step 2: Run them.** `cargo test --lib tap:: atomic:: 2>&1 | tail` Expected: compile errors.

- [ ] **Step 3: Implement.** Update the existing `sanitize_unofficial_strips_no_autobump` test to bytes. Core was surveyed on 2026-11-27: 142 `no_autobump!` lines, every one a single line of the form `no_autobump! because: ...`.

- [ ] **Step 4: Run the tests, then the done gate.**

- [ ] **Step 5: Commit**

```bash
git add src/tap.rs src/atomic.rs
git commit -m "feat: write soak-tap files byte for byte and build soak-tap commands (#17)"
```

---

### Task 5: The soak-tap module: state, staging and settling

**Files:**
- Create: `src/soaktap.rs`; add `pub mod soaktap;` to `src/lib.rs:1-27`
- Modify: `src/cmd/deps.rs:126-137` (`cutoff_version` moves to `soaktap::file_version`; its callers use the new name)
- Test: `src/soaktap.rs` tests

**Interfaces:**
- Consumes: `TapInfo`, `OwnTap`, `Pkg.own_tap` (Task 1); `Brew::tap_new`, `Brew::tap_info`, `Brew::receipt_tap` (Task 2); `tap::sanitize_unofficial`, `tap::write_blob`, `tap::tap_formula_path`, `tap::tap_cask_path`, `atomic::write_bytes` (Task 4); `lock::RunLock` (`RunLock::for_tests()` in tests).
- Produces:

```rust
pub const UNDO_DIR: &str = "soak-tap-undo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapState { NotTapped, Remote(String), Untrusted, Ready }
/// `None`: not tapped. A non-null remote: `Remote`, whatever `trusted` says.
pub fn tap_state(entry: Option<&TapInfo>) -> TapState;
/// The entry's `path` when it has one, else `tap_root` (decision P1).
pub fn tap_dir(entry: Option<&TapInfo>, tap_root: &Path) -> PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady { Untrusted, DryRunNotTapped, Remote(String), CreateFailed(String) }
impl NotReady { pub fn note(&self) -> String }

/// Version of a formula or cask file in brew's spelling (`1.0.0_2`); `None` when it does not parse.
pub fn file_version(kind: PkgKind, rb: &[u8]) -> Option<String>;
/// The receipt names the soak tap and `dir` has a file for the package whose
/// `file_version` equals that of `pkg.receipt_rb`.
pub fn anchored(dir: &Path, pkg: &Pkg) -> bool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepRule { Pass, WriteCutoff, WouldReplace, CannotReplace }
/// Spec "Dependency closure and ordering": an installed formula dependency this run does not upgrade.
pub fn installed_dep_rule(
    satisfied: bool,
    own_tap: Option<OwnTap>,
    installed_version: Option<&str>,
    cutoff_version: Option<&str>,
    installs_cutoff: bool,
) -> DepRule;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staged { Unchanged, Written, Leftover, NotReady(NotReady) }

pub struct SoakTap { /* private */ }
impl SoakTap {
    pub fn new(entry: Option<&TapInfo>, tap_root: &Path, cache: &Path, dry_run: bool) -> Self;
    /// Current tap directory (`tap_dir`), whether or not the tap is ready.
    pub fn dir(&self) -> &Path;
    /// Decided on the first call and kept for the run. A real run creates a
    /// tap that is not tapped, reads tap-info again and applies the row that
    /// then holds. Never fatal.
    pub fn ready(&mut self, brew: &impl Brew) -> Result<(), NotReady>;
    /// Make the package's tap file the sanitized cutoff. `version` is the
    /// cutoff version when the caller knows it; a later `Some` replaces it.
    pub fn stage(&mut self, lock: &RunLock, brew: &impl Brew, pkg: &PkgRef,
                 cutoff: &[u8], version: Option<&str>) -> Result<Staged, Error>;
    /// Keep or put back every file staged since the last settle.
    pub fn settle(&mut self, lock: &RunLock, brew: &impl Brew) -> Result<(), Error>;
}
```

Rules the bodies must follow:
- `ready`: `Ready` is `Ok`. `Untrusted` and `Remote(r)` map to the same-named `NotReady`. `NotTapped` on a dry run is `DryRunNotTapped` with no brew call. `NotTapped` on a real run calls `brew.tap_new(SOAK_TAP)`: `Err(first)` is `CreateFailed(first)`; `Ok` re-reads `brew.tap_info()` (an error there is `CreateFailed` with its first line) and applies the table to the new entry, updating `dir()`.
- `stage`, in order: (1) `ready` fails: `Staged::NotReady`, nothing touched. (2) The package was already staged this run: record `version`, return `Unchanged`. (3) An undo entry of either form exists: `Leftover`, nothing touched. (4) The file already has the wanted bytes: `Unchanged`, no undo entry. (5) Write the undo entry with `atomic::write_bytes` (`<name>.rb` with the previous bytes, or an empty `<name>.absent`), then the tap file with `tap::write_blob`: `Written`.
- `settle`, per staged file: keep it and delete the undo entry when the run is not a dry run and `brew.receipt_tap(kind, name, version)` is the soak tap, where `version` is the recorded one, else `file_version` of the tap file. Otherwise put back (restore the previous bytes with `atomic::write_bytes`, or delete the file for `.absent`), then delete the undo entry. A second `settle` does nothing.
- `installed_dep_rule`: `satisfied` is `Pass`. Otherwise by `own_tap`: `None` is `Pass` when `installs_cutoff`, else `WouldReplace`; `Some(Soak)` is `WriteCutoff` when both versions are `Some` and equal, else `CannotReplace`; `Some(Legacy)` is `CannotReplace`.

`NotReady::note` (exact copy):

```text
Untrusted:        brewsoak installs soaked packages from its own tap, ericfitz/brewsoak, and brew does not trust it yet; run brew trust ericfitz/brewsoak once, then run brewsoak again.
DryRunNotTapped:  brewsoak's tap, ericfitz/brewsoak, does not exist yet; a real run creates it, and it then needs brew trust ericfitz/brewsoak.
Remote(r):        ericfitz/brewsoak is tapped from <r>; brewsoak only uses a local tap of that name; remove it with brew untap ericfitz/brewsoak.
CreateFailed(e):  brewsoak could not create its tap (<e>).
```

- [ ] **Step 1: Write the failing tests** (temp tap and cache directories, `MockBrew`)

State and notes:
- `tap_state_follows_the_spec_table`: `None` is `NotTapped`; remote `https://x` with `trusted: true` is `Remote("https://x")`; local untrusted is `Untrusted`; local trusted is `Ready`.
- `tap_dir_prefers_the_reported_path`: entry path `/p` and `tap_root` `/q` give `/p`; no path gives `/q`; no entry gives `/q`.
- `not_ready_notes_are_the_spec_text`: the four strings above, compared whole.
- `ready_real_run_creates_a_missing_tap_once_and_then_holds_as_untrusted`: `MockBrew { soak_tap: None, .. }`: `Err(NotReady::Untrusted)`; exactly one `tap-new` run after two `ready` calls.
- `ready_dry_run_creates_nothing`: `Err(DryRunNotTapped)`; `runs` is empty.
- `ready_reports_a_failed_tap_new`: `tap_new_error: Some("Error: nope\nmore".into())` gives `Err(CreateFailed("Error: nope"))`.
- `ready_remote_untrusted_and_trusted`: three entries, three answers; the trusted one makes no brew call.

Staging and settling (`pkg` = formula `wget`, cutoff `formula_rb`-style text at `1.1.0`):
- `stage_writes_the_file_and_an_absent_marker`: `Written`; `Formula/wget.rb` equals the sanitized cutoff; `<cache>/soak-tap-undo/Formula/wget.absent` exists and is empty.
- `stage_over_another_file_saves_the_previous_bytes`: `soak-tap-undo/Formula/wget.rb` holds the old bytes.
- `stage_with_the_wanted_bytes_touches_nothing`: `Unchanged`; no undo directory entry.
- `stage_twice_in_one_run_is_unchanged_and_keeps_one_undo_entry`.
- `stage_meets_a_leftover_undo_entry`: an entry seeded before `SoakTap::new`: `Leftover`; the tap file and the entry are byte-identical afterwards.
- `stage_on_an_unready_tap_writes_nothing`: untrusted entry: `Staged::NotReady(NotReady::Untrusted)`; tap and cache directories are empty.
- `undo_entry_exists_before_the_tap_file_changes`: make `<tap>/Formula` a plain file so the tap write fails; `stage` is `Err` and the undo entry exists.
- `stage_keeps_kinds_and_odd_names_apart` (Review Focus 2): formula `docker` and cask `docker`, and `python@3.12`, each get their own tap file and undo entry.
- `settle_keeps_the_file_when_the_keg_is_from_the_tap`: `receipt_taps[("wget","1.1.0")] = "ericfitz/brewsoak"`: the file stays, the undo entry is gone.
- `settle_puts_back_without_a_matching_keg`: no receipt: an `.absent` entry deletes the file; a `.rb` entry restores the old bytes.
- `settle_puts_back_when_the_keg_is_from_another_tap`: receipt `homebrew/core`.
- `settle_on_a_dry_run_always_puts_back`: even with the matching receipt.
- `settle_uses_the_file_version_when_none_was_given` and `settle_puts_back_an_unparseable_file`.
- `settle_twice_is_a_no_op`.

Facts:
- `anchored_needs_the_receipt_and_a_file_at_the_installed_version`: true for `own_tap: Some(Soak)` with a tap file at the receipt's version; false with a file at another version, no file, an unparseable file, `own_tap: Some(Legacy)` (whatever either tap holds) and `own_tap: None`.
- `installed_dep_rule_table`: one assertion per row of the rule list above (eight rows, including `Some(Soak)` with `installed_version: None`).

- [ ] **Step 2: Run them.** `cargo test --lib soaktap:: 2>&1 | tail` Expected: compile error (module missing).

- [ ] **Step 3: Implement `src/soaktap.rs`** to the signatures and rules above.

- [ ] **Step 4: Run the tests, then the done gate.**

- [ ] **Step 5: Commit**

```bash
git add src/soaktap.rs src/lib.rs src/cmd/deps.rs
git commit -m "feat: add the soak-tap module with reversible tap writes (#17)"
```

---

### Task 6: Recovery, pruning and the maintenance gate

**Files:**
- Modify: `src/soaktap.rs`
- Test: `src/soaktap.rs` tests; one test in `src/prune.rs` (after `:108`)

**Interfaces:**
- Consumes: Task 5's undo layout, `file_version`, `tap_state`, `tap_dir`; `Inventory`, `Pkg.own_tap`.
- Produces:

```rust
/// Settle every undo entry found on disk, by the settle rule with
/// `file_version` of the tap file as the version.
pub fn recover(lock: &RunLock, brew: &impl Brew, dir: &Path, cache: &Path) -> Result<(), Error>;
/// Remove every `.rb` under `dir/Formula` and `dir/Casks` that has no undo
/// entry and no installed package of that kind and name whose receipt names
/// the soak tap.
pub fn prune(lock: &RunLock, dir: &Path, cache: &Path, inv: &Inventory,
             verbose: bool, out: &mut impl Write) -> Result<(), Error>;
/// Recovery when the tap is local (`Untrusted` or `Ready`), then pruning when
/// it is `Ready`. Nothing when it is not tapped or tapped from a remote.
#[allow(clippy::too_many_arguments)]
pub fn maintain(lock: &RunLock, brew: &impl Brew, entry: Option<&TapInfo>, tap_root: &Path,
                cache: &Path, inv: &Inventory, verbose: bool, out: &mut impl Write) -> Result<(), Error>;
```

Rules:
- `recover`: an entry is `<name>.rb` or `<name>.absent` directly under `soak-tap-undo/Formula` or `/Casks`; any other file is left alone. When both forms exist for one name, the `.rb` bytes are what is put back, and both are deleted. A missing tap file with no matching keg just deletes the entry.
- `prune`: walks real directories only; a `.rb` that is a symlink is removed as a link; a linked directory is not entered. The test is the receipt alone, never the version. A removal that fails prints `brewsoak: warning: could not prune <path>: <error>` and continues. With `verbose`, each removal prints `pruned <path> (no installed package uses it)`.

- [ ] **Step 1: Write the failing tests**

Recovery:
- `recover_keeps_a_file_whose_keg_is_from_the_tap`, `recover_puts_back_previous_bytes_without_a_matching_keg`, `recover_deletes_the_file_for_an_absent_marker`, `recover_puts_back_an_unparseable_file`.
- `recover_twice_is_a_no_op` and `recover_without_an_undo_directory_creates_nothing`.
- `recover_handles_entries_it_never_wrote` (Review Focus 3): `wget.rb` and `wget.absent` together restore the `.rb` bytes and remove both; `.wget.rb.tmp-99` and `notes.txt` are still there afterwards.

Prune (inventory built with `Inventory::build`):
- `prune_removes_a_file_no_installed_package_uses`.
- `prune_never_removes_a_file_while_the_receipt_names_the_tap`: installed `wget` with `own_tap: Some(Soak)`; the tap file has another version and stays.
- `prune_keeps_a_file_with_an_undo_entry`.
- `prune_is_per_kind`: an installed cask `docker` from the soak tap does not protect `Formula/docker.rb`.
- `prune_touches_nothing_outside_formula_and_casks`: `README.md`, `.github/x.yml` and `Formula/notes.txt` stay.
- `prune_removes_a_stale_link_and_spares_its_target` (Review Focus 4): `Formula/x.rb` linking to a file outside the tap: the link goes, the target stays; a linked directory `Formula/sub` is not entered.
- `prune_removes_a_nested_stale_file`: `Formula/a/old.rb` goes.

Gate:
- `maintain_recovers_and_prunes_a_ready_tap`, `maintain_recovers_but_does_not_prune_an_untrusted_tap`, `maintain_does_nothing_when_not_tapped_or_remote` (both sub-cases leave a seeded undo entry and stale file byte-identical).

`src/prune.rs`:
- `prune_cache_leaves_the_soak_tap_and_undo_directories_untouched`: seed `<cache>/soak-tap-undo/Formula/wget.absent` and a soak tap directory elsewhere with two files; snapshot both trees (path to bytes); run `prune_cache`; the snapshots are equal.

- [ ] **Step 2: Run them.** `cargo test --lib soaktap:: prune:: 2>&1 | tail` Expected: compile errors.

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run the tests, then the done gate.**

- [ ] **Step 5: Commit**

```bash
git add src/soaktap.rs src/prune.rs
git commit -m "feat: recover and prune brewsoak's tap (#17)"
```

---

### Task 7: Thread the lock and the tap directory; maintenance points

Spec "When brewsoak touches the tap outside an install".

**Files:**
- Modify: `src/lib.rs:53` (`World::tap_root`), `src/lib.rs:108-112` (`RealWorld::tap_root`), `src/lib.rs:239-258` and `:316-410` (dispatch arms), test worlds at `src/lib.rs:649` and `:730`
- Modify: `src/cmd/upgrade.rs:15-48`, `src/cmd/install.rs:13-47`, `src/cmd/apply.rs:37-47` (`reinstall`), `src/cmd/apply.rs:180-196` (`apply_many`), `src/cmd/update.rs:161-185`
- Modify: every test call of those five functions, and the helpers at `src/cmd/test_support.rs:257-311`
- Test: `src/lib.rs`, `src/cmd/update.rs`, `src/cmd/apply.rs` tests

**Interfaces:**
- Consumes: `soaktap::maintain` (Task 6); `Inventory.soak_tap` (Task 1); `test_cache`, `soak_tap_at` (Task 2).
- Produces:

```rust
// trait World
/// Where brewsoak expects its tap: `$(brew --repository)/Library/Taps/ericfitz/homebrew-brewsoak`.
fn tap_root(&self) -> Result<PathBuf, Error>;

// lock is the new first parameter; `_tap_root` becomes `tap_root`
pub fn upgrade(lock: &RunLock, brew: &impl Brew, git: &impl GitStore, snaps: &Snapshots, cache: &Path,
    tap_root: &Path, inv: &Inventory, cfg: &Config, names: &[String], user_flags: &[String],
    out: &mut impl Write) -> Result<RunResult, Error>;
pub fn install(lock: &RunLock, /* then as today */) -> Result<RunResult, Error>;
pub fn reinstall(lock: &RunLock, /* then as today */) -> Result<RunResult, Error>;
pub(super) fn apply_many(lock: &RunLock, /* then as today */) -> Result<RunResult, Error>;
// tap_root is new, after cache
pub fn update(lock: &RunLock, brew: &impl Brew, git: &impl GitStore, gh: &impl GithubApi, cache: &Path,
    tap_root: &Path, cfg: &Config, inv: &Inventory, now: time::OffsetDateTime, verbose: bool,
    out: &mut impl Write) -> Result<(), Error>;
```

Where `soaktap::maintain(lock, brew, inv.soak_tap.as_ref(), tap_root, cache, inv, verbose, out)` runs:
- `cmd::update`, right after `prune::prune_cache` (`src/cmd/update.rs:185`).
- `apply_many` and `reinstall`, after `validate_tokens` and before `update_before_soaked_work`, only when the run is not a dry run.
- Nowhere else: not in `outdated`, `info`, a dry run, or `cmd::ensure_snapshots`.

`RealWorld::tap_root` runs `brew --repository` through `self.brew.run` and joins `Library/Taps/ericfitz/homebrew-brewsoak`; a failure is `Error::Brew` with brew's text. Dispatch calls `world.tap_root()?` in the `Update`, `Upgrade`, `Install` and `Reinstall` arms and passes `lock.get()?`.

- [ ] **Step 1: Write the failing tests**

Shared seeding helper in `src/cmd/test_support.rs`: `seed_stale_tap(tap: &Path, cache: &Path)` writes `Formula/gone.rb` (no installed package) and an undo entry `soak-tap-undo/Formula/back.absent` with a tap file `Formula/back.rb`; `tree(dir) -> BTreeMap<PathBuf, Vec<u8>>` snapshots a directory.

- `cmd::update::tests::update_recovers_and_prunes_the_soak_tap`: after `update`, `gone.rb`, `back.rb` and the undo entry are gone.
- `cmd::apply::tests::real_upgrade_install_and_reinstall_maintain_the_tap_first`: each of the three verbs, real run, with one installed package at its cutoff: the same three paths are gone.
- `cmd::apply::tests::dry_run_never_maintains_the_tap`: with `--dry-run` the tap and undo trees are unchanged for all three verbs.
- `tests::outdated_info_and_ensure_snapshots_never_touch_the_tap` (`src/lib.rs`, `dispatch` on the test world): `outdated`, `info` and a forced refresh through `outdated` on an empty cache leave both trees unchanged.
- `tests::untrusted_local_tap_is_recovered_but_not_pruned` (`src/lib.rs`): the world's brew reports `soak_tap_at(&tap_root, false)`; after `brewsoak update` the undo entry is settled and `gone.rb` is still there.
- `tests::absent_or_remote_tap_is_left_alone` (`src/lib.rs`): `soak_tap: None`, and an entry with a remote: both trees unchanged after `brewsoak update`.

- [ ] **Step 2: Run them.** Expected: compile errors (arity).

- [ ] **Step 3: Implement.** Find every call to update with `rg -n '\b(upgrade|install|reinstall|apply_many|update)\(' src/cmd src/lib.rs`; tests pass `&RunLock::for_tests()`. The compiler reports any call that was missed.

- [ ] **Step 4: Run the tests, then the done gate.**

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/cmd
git commit -m "feat: recover and prune brewsoak's tap at update and at the start of a real run (#17)"
```

---

### Task 8: Cutover: install core and cask packages through the soak tap

After this task no soaked `homebrew/core` or `homebrew/cask` package goes through the by-name gate. `src/cmd/deps.rs` and `src/cmd/apply.rs` change together.

**Files:**
- Modify: `src/cmd/deps.rs:25-125` (`NameWhy`, `NameFail`, `CutoffDepWalk`), `src/cmd/deps.rs:157-342` (`visit`)
- Modify: `src/cmd/apply.rs:272-319` (`ApplySession` fields), `:743-805` (`install_cutoff`), `:807-833` (`collect_cutoff_deps`), `:836-931` (`install_missing_dep`), `:968-1000` (`hold_target_by_name`), `:1073-1109` (`record_name_install`), `:1193-1198` (`NameRun`)
- Modify: `src/cmd/notes.rs:25-43` (`by_name_note`)
- Modify: `src/cmd/test_support.rs:215-251` (`run_is_name_install`, `assert_no_path_args`)
- Test: `src/cmd/deps.rs`, `src/cmd/apply.rs`, `src/cmd/upgrade.rs`, `src/cmd/install.rs`

**Interfaces:**
- Consumes: `SoakTap`, `Staged`, `NotReady`, `DepRule`, `installed_dep_rule`, `file_version` (Task 5); `tap::soak_token`, `tap::soak_tap_args`, `tap::soak_tap_verb` (Task 4); `ByName.satisfied`, `MockBrew.on_call`, `MockBrew.receipt_taps` (Task 2); `Pkg.own_tap`, `Inventory.soak_tap` (Task 1); `lock` and `tap_root` parameters (Task 7).
- Produces:

```rust
// src/cmd/deps.rs
pub(super) enum NameWhy {
    NotCutoff { brew_version: String }, ChangedSinceCutoff { what: &'static str, version: String },
    OtherTap { tap: String, origin: String }, Lookup(String), Deps(String), NoCutoff,
    /// brew could not load the tap file or list its dependencies (first line).
    SoakedCopy(String),
    WouldReplace { dep: String, installed: String, brew_version: String, target: String },
    CannotReplace { dep: String, installed: String },
}
pub(super) enum NameFail { Untrusted(String), TapNotReady(NotReady), Interrupted, Why(NameWhy) }
// CutoffDepWalk gains: pub(super) lock: &'a RunLock, pub(super) soak: &'a mut SoakTap,
//                      pub(super) target: &'a str

// src/cmd/notes.rs: `bypass` is the whole command, e.g. "brew upgrade wget"
pub(super) fn by_name_note(name: &str, dep: Option<&str>, why: &str, bypass: &str) -> String;
// => "{name}: [dependency {dep}: ]{why}; use `{bypass}` to bypass brewsoak."

// src/cmd/apply.rs
pub(super) enum NameRun { Ran { failed: bool }, Untrusted(String), TapNotReady }
// ApplySession gains: lock: &'a RunLock, soak: SoakTap
// (SoakTap::new(inv.soak_tap.as_ref(), tap_root, cache, dry_run))

// src/cmd/test_support.rs
/// No argument is a path; no command is `trust` or `untrust`; no argument is
/// `--ignore-dependencies` or contains `brewsoakr/soaked`; every argument that
/// starts with `ericfitz/brewsoak/` names one of `soak_names`.
pub(super) fn assert_soak_tap_commands(brew: &MockBrew, soak_names: &[&str]);
```

`NameWhy::text()` (exact copy; `waits_on_issue_17` is deleted and no text names issue #17):

```text
NotCutoff:          brew would install {brew_version}, which is not the soaked cutoff; it installs once that change soaks
ChangedSinceCutoff: (unchanged)
OtherTap:           brew resolves the name to {tap}, not {origin}; brewsoak cannot install it by name
Lookup, Deps, NoCutoff: (unchanged)
SoakedCopy(e):      brew could not load its soaked copy ({e})
WouldReplace:       brew would replace installed {dep} {installed} with {brew_version}, which has not soaked; {target} installs once it has
CannotReplace:      brew cannot replace installed {dep} {installed} as a dependency; run brew upgrade {dep} first
```

Other holds:
- `TapNotReady(n)`: inline `held {target}: brewsoak's tap is not ready`; `n.note()` is deferred under `Held:` once per run, however many packages it holds; the target takes back its `upgraded` count (`hold_target_for_dep`).
- `Interrupted`: inline `held {target}: an earlier brewsoak run was interrupted`; note `{target}: an earlier brewsoak run was interrupted; run brewsoak update to finish it` (no bypass tail).
- The untrusted third-party hold, the ambiguous-name hold and the held-tap refusal keep their text.

Member rules in `visit` (a member is a soak-tap member when `origin::is_core_or_cask(origin_tap)`; any other member keeps today's code path, bare names and gate included):

| Member | Rule |
|---|---|
| Root, or Install | `soak.stage(lock, brew, pkg, cutoff, file_version(kind, cutoff))`. `NotReady(n)` holds with `TapNotReady(n)`; `Leftover` holds with `Interrupted`. Then `brew.deps(kind, &tap::soak_token(name))`. An ambiguous-name error propagates as today. Brew's untrusted-tap text naming `ericfitz/brewsoak` holds with `TapNotReady(NotReady::Untrusted)`; naming another tap, `Untrusted(tap)`; any other brew error, `SoakedCopy(first_line)`. `by_name` is not asked. |
| Check (installed formula, not upgraded this run) | Ask `by_name`. Untrusted-tap text naming `ericfitz/brewsoak` holds with `TapNotReady(Untrusted)`; other errors as today. Then `installed_dep_rule(answer.satisfied, pkg.own_tap, file_version(Formula, pkg.receipt_rb), file_version(Formula, cutoff), answer.installs_cutoff(origin))`: `Pass` passes; `WriteCutoff` stages the cutoff (same two hold outcomes) and passes; `WouldReplace` and `CannotReplace` hold with the same-named `NameWhy` (`brew_version` is `answer.version`). A passing member is walked through: its dependencies are listed with the soak token only when `pkg.own_tap == Some(Soak)`, its tap file exists in `soak.dir()` and `soaktap::tap_state(inv.soak_tap.as_ref())` is `TapState::Ready` (read the state; do not call `ready`, which could create the tap). Otherwise by bare name. |
| Installed cask dependency; no-soak or unsoakable dependency; dependency in a held tap or without a cutoff | As today (the dependency loop, `src/cmd/deps.rs:252-331`). |

A soak-tap member that needs its cutoff file and has no cutoff blob holds with `NameWhy::NoCutoff`. The demotion of `Install` to `Check` at `src/cmd/deps.rs:200-211` applies to third-party members only.

`ApplySession` rules:
- A soak-tap dependency or target runs `tap::soak_tap_args(&pkg, installed, &self.name_install_flags())`, where `installed` is `self.inv.find_in(origin, name).is_some()`; `note_dropped_flags` uses `tap::soak_tap_verb`. Before the command, `install_missing_dep` and `install_cutoff` call `soak.stage` again with the view's cutoff version (`report::identity_version`), and treat `NotReady` and `Leftover` as the holds above. A third-party package runs `tap::brew_install_args` as today.
- `record_name_install`: brew's untrusted-tap text naming `ericfitz/brewsoak` is `NameRun::TapNotReady` (hold with `NotReady::Untrusted`). `forget_origin` after a real install is unchanged.
- `install_cutoff` settles on every path once the walk has started: run the body, then `self.soak.settle(self.lock, self.brew)`, and return the first error of the two.

- [ ] **Step 1: Write the failing tests**

World helpers to add in `src/cmd/test_support.rs`: `soak_world(tap: &Path) -> (MockBrew, InMemoryGit)`, the `local_tap_world` packages (`:313`) with `soak_tap: Some(soak_tap_at(tap, true))`; `tap_file(tap, "Formula/alpha.rb") -> Option<Vec<u8>>`.

Tap state, `src/cmd/apply.rs` (each world also has the third-party package of `packer_world` and `hashicorp_tap`, `src/cmd/test_support.rs:148-171`, with an upgrade, to prove the run continues):
- `not_tapped_real_run_creates_the_tap_then_holds`: `soak_tap: None`: `runs` has one `tap-new`; output has `held alpha: brewsoak's tap is not ready` and the `Untrusted` note once; `packer` installs by bare name; `combine_exit` of the result is 1; `assert_soak_tap_commands(&brew, &[])`.
- `not_tapped_dry_run_creates_nothing`: no `tap-new`; the `DryRunNotTapped` note.
- `remote_soak_tap_is_held_and_never_used`: the `Remote` note with the remote; no soak token in any command.
- `untrusted_soak_tap_holds_every_soak_tap_package_with_one_note`: two core packages: two `held ... not ready` lines, the note exactly once, `held 2` in the summary.
- `failed_tap_new_is_a_hold_not_an_error`: the `CreateFailed` note; `Ok` result.
- `tap_info_without_a_trusted_field_is_used`: parse `[{"name":"ericfitz/brewsoak","remote":null}]` into `brew.taps`: `alpha` installs.

Verb, token and file:
- `upgrade_installs_the_cutoff_from_the_soak_tap` (replaces `upgrade_installs_the_cutoff_by_name`, `src/cmd/upgrade.rs:71`): visible runs are `[["update"], ["upgrade","--formula","ericfitz/brewsoak/alpha"]]`.
- `soak_tap_verb_follows_the_installed_state_for_every_brewsoak_verb`: `brewsoak upgrade|install|reinstall` of an installed formula run `brew upgrade ... ericfitz/brewsoak/<name>`; `brewsoak install` of an absent formula runs `brew install ...`; a cask always `brew install --cask ...`.
- `tap_holds_the_cutoff_file_when_brew_is_asked`: an `on_call` hook copies `Formula/alpha.rb` at each `deps` and `upgrade` call into a shared `Arc<Mutex<Vec<..>>>`; every copy equals `tap::sanitize_unofficial(cutoff)`; with a blob that has no `no_autobump!` line it equals the blob.
- `soak_tap_runs_carry_only_soak_tap_tokens_and_no_paths` (rewrite of `soaked_runs_never_hand_brew_a_path_or_full_token`, `src/cmd/apply.rs:2277`): after its runs, `assert_soak_tap_commands(&brew, &[...core names...])`.
- `third_party_file_is_never_written_to_the_tap`: upgrading a `hashicorp/tap` package leaves the tap directory without `Formula/` and runs `["install","--formula","packer"]`.
- `run_without_soak_tap_work_never_creates_the_tap`: `soak_tap: None` and only that `hashicorp/tap` package to upgrade: no `tap-new` run, and `packer` installs.
- `real_install_from_the_tap_removes_a_stale_origin_record`: a cache with `"formula:alpha" = "acme/tools"` in `origins.toml`: gone after a real install; still there after a dry run and after brew's `already installed` answer.

Closure, `src/cmd/deps.rs`:
- `missing_and_upgraded_dependencies_install_from_the_tap_before_the_target`: order of visible runs is `install ... ericfitz/brewsoak/newdep`, `upgrade ... ericfitz/brewsoak/olddep`, `upgrade ... ericfitz/brewsoak/target`; `deps_calls` hold only soak tokens.
- `satisfied_dependency_is_left_alone_and_walked_through`: `by_name["dep"]` with `satisfied: true`: no command for `dep`; its own missing dependency is installed from the tap.
- `unsatisfied_dependency_from_core_passes_when_brew_has_the_cutoff`: `satisfied: false, same_source: true`: target installs.
- `unsatisfied_dependency_brew_might_replace_holds_the_target`: `same_source: false, version: "2.0"`, installed `1.0`: output has `held target: dependency dep: brew would replace installed dep 1.0 with 2.0, which has not soaked; target installs once it has`.
- `soak_receipt_dependency_at_its_cutoff_gets_its_file_and_passes`: `own_tap: Some(Soak)`, no tap file, cutoff version equals installed: the target installs, and with `receipt_taps[("dep", v)]` set the file `Formula/dep.rb` is still there after the run.
- `legacy_receipt_dependency_that_brew_cannot_replace_holds_the_target`: `own_tap: Some(Legacy)` (and `Some(Soak)` with another cutoff version), unsatisfied, not upgraded: `brew cannot replace installed dep 1.0 as a dependency; run brew upgrade dep first`; no file is written for the legacy one; no output contains `has no file`.
- `legacy_receipt_dependency_that_is_satisfied_does_not_hold`.
- `satisfied_dependency_is_never_listed_through_a_tap_that_is_not_ready`: the soak tap is tapped from a remote (and, in a second case, local and untrusted); a `hashicorp/tap` target has a satisfied dependency with `own_tap: Some(Soak)` and a file in the tap directory: no `deps` call and no command carries `ericfitz/brewsoak/`, and the target installs.
- `third_party_target_installs_its_core_dependency_from_the_tap`: a `hashicorp/tap` target with a missing core dependency: `install --formula ericfitz/brewsoak/dep` then `install --formula packer`.
- `soaked_copy_that_brew_cannot_load_holds_the_target`: `deps_errors["ericfitz/brewsoak/alpha"] = "Error: alpha: boom\nmore"`: `held alpha: brew could not load its soaked copy (Error: alpha: boom)`; the run continues.
- `untrusted_soak_tap_text_from_brew_maps_to_the_tap_not_ready_hold`: `deps_errors` with `Refusing to load formula ericfitz/brewsoak/dep from untrusted tap ericfitz/brewsoak.` on a third-party target's dependency: the `Untrusted` note, and no text containing `NO_SOAK`.

Settle, `src/cmd/apply.rs`:
- `tap_file_is_kept_after_a_real_install`: `receipt_taps[("alpha","1.1.0")] = "ericfitz/brewsoak"`: file present, undo directory has no entry.
- `tap_file_is_put_back_after_a_brew_failure_without_a_new_keg`: `next_status: 1`, no receipt.
- `tap_file_is_put_back_after_already_installed_over_another_taps_keg`: stdout `Warning: alpha 1.1.0 already installed`, receipt `homebrew/core`.
- `tap_file_is_kept_when_brew_fails_but_the_new_keg_exists`: `next_status: 1` with the receipt.
- `tap_files_are_put_back_after_a_hold_and_after_a_dry_run`: dependencies' files included; the tap tree equals its tree before the run. On the dry run every dependency and target command carries `--dry-run`.
- `dry_run_meeting_a_leftover_undo_entry_holds_that_target`: seeded entry for `alpha`: `held alpha: an earlier brewsoak run was interrupted`, the note above, entry and tap file byte-identical, a second package still runs.
- `second_run_writes_nothing`: with the keg at the cutoff and the file in place, a second `upgrade` runs no brew command for `alpha` and creates no undo entry.

- [ ] **Step 2: Run them.** `cargo test --lib cmd:: 2>&1 | rg 'FAILED|error' | head -30` Expected: compile errors, then failures.

- [ ] **Step 3: Implement** the interfaces and rules above.

- [ ] **Step 4: Migrate the existing tests**

- `run_is_name_install` (`src/cmd/test_support.rs:215`), `name_install_order` (`src/cmd/apply.rs:1473`), `run_is_soaked_dep_install` (`src/cmd/deps.rs:1527`) and `installs` (`src/cmd/deps.rs:1854`) accept `install` or `upgrade` and compare the last argument with the `ericfitz/brewsoak/` prefix removed.
- Delete `assert_no_path_args` and its test module (`src/cmd/test_support.rs:225-251`, `:630-698`); its callers use `assert_soak_tap_commands`.
- `gate_tests` (`src/cmd/deps.rs:1743`): each test whose package is in core moves to a third-party world (`tapped("acme/tools", Some("https://example.com/acme/tools"))`, `tap_snaps`, `formula_pkg_from`), so it still pins the gate for third-party packages, with the new `NotCutoff` and `OtherTap` text. `different_version_keeps_the_issue_17_text` becomes `different_version_says_it_installs_once_the_change_soaks`. A scenario that cannot exist for a third-party package is covered by the new tests above; say which in the commit body.
- Every assertion on `issue #17` text (`rg -n 'issue #17|issue_17' src`) changes to the new text; none may remain.
- Exact-argument assertions for core packages change to the soak-tap form.

- [ ] **Step 5: Run the tests, then the done gate.** Also run `rg -n 'issue #17|waits_on_issue_17|assert_no_path_args' src` and expect no output.

- [ ] **Step 6: Commit**

```bash
git add src/cmd
git commit -m "feat: install soaked core and cask packages from brewsoak's own tap (#17)"
```

---

### Task 9: An unusable tap that already has packages; own-tap tokens

Spec "An untrusted tap that already has packages" (holding, `outdated`, `info`) and "Receipts, inventory and origin" (explicit tokens).

**Files:**
- Modify: `src/cmd/apply.rs:94-170` (reinstall loop), `:506-513` (`apply_one`), `:530-582` (`apply_resolved`)
- Modify: `src/cmd/outdated.rs:150-244`, `src/cmd/info.rs:46-229`, `src/nosoak.rs:168-371` (`run_step_after`)
- Test: `src/cmd/apply.rs`, `src/cmd/install.rs`, `src/cmd/outdated.rs`, `src/cmd/info.rs`, `src/nosoak.rs`

**Interfaces:**
- Consumes: `soaktap::tap_state`, `TapState`, `NotReady::note` (Task 5); `Pkg.own_tap`, `Inventory.soak_tap`, `origin::own_tap` (Task 1); the `TapNotReady` hold of Task 8.
- Produces:

```rust
// src/soaktap.rs
/// `Some` when the tap is tapped but unusable (untrusted, or from a remote)
/// and at least one installed package's receipt names it.
pub fn unusable_with_packages(inv: &Inventory) -> Option<NotReady>;
```

Rules:
- `upgrade`, `install`, `reinstall`: while `tap_state(inv.soak_tap)` is `Untrusted` or `Remote`, every installed soaked package with `own_tap == Some(OwnTap::Soak)` that the run reaches is held: line `held <name>: brewsoak's tap is not ready`, the note once, `held` counted, `refused` set, no brew command for it. A package whose action is `InstallCutoff` is held by Task 8's hold, so it keeps its `[i/N]` line. For any other action the hold comes right after the view is resolved, before `counts.note` (`src/cmd/apply.rs:666`), in place of that action's own outcome. In `reinstall` it comes before the true repair. A pinned package on `upgrade` keeps its pinned skip.
- The no-soak step: when brew's output for a no-soak run contains its untrusted-tap refusal naming `ericfitz/brewsoak`, `run_step_after` adds `NotReady::Untrusted.note()` to `StepResult.notes` once. The failure itself is reported as today.
- An explicit token whose tap is `ericfitz/brewsoak` or `brewsoakr/soaked` is refused for `upgrade`, `install`, `reinstall` and `info`: note `<name>: <tap> is brewsoak's own tap; name the package without the tap`, counted as held, `refused` set, the next name still runs. It gets no bypass hint. The check comes before `refuses_tap_switch` (`src/cmd/apply.rs:478-493`), which would otherwise print a `brew <verb> ericfitz/brewsoak/<name>` command.
- `outdated`: when `unusable_with_packages` is `Some(n)`, `n.note()` is one more line in the `==> Held` section. `info`: the same note is the last line of output. Both still list the package with its identities, and exit as today.

- [ ] **Step 1: Write the failing tests** (world: `soak_tap: Some(soak_tap_at(tap, false))`, installed `hello` with `own_tap: Some(Soak)` and an upgrade available, a second core package `alpha`, a third-party package)

- `upgrade_holds_a_package_installed_from_the_untrusted_tap`: `held hello: brewsoak's tap is not ready`; no command names `hello`; the third-party package installs; result is `Ok` with `refused`.
- `install_of_a_package_from_the_untrusted_tap_is_a_hold_not_a_fresh_install` (`src/cmd/install.rs`): `brewsoak install hello`: no `install` run at all.
- `reinstall_holds_it_before_the_true_repair`: installed matches HEAD: no `reinstall` run.
- `package_at_its_cutoff_from_the_untrusted_tap_is_still_held`.
- `remote_soak_tap_holds_packages_installed_from_it`: the `Remote` note.
- `explicit_own_tap_token_is_refused`: `upgrade ericfitz/brewsoak/hello alpha` and `install brewsoakr/soaked/hello`: the note above, no command names `hello`, `alpha` is still processed, exit 1; no output line contains `brew upgrade ericfitz/brewsoak/` or `brew install brewsoakr/soaked/`.
- `outdated_lists_the_package_and_the_note` and `info_lists_the_package_and_the_note`: `hello` appears with its versions; the `Untrusted` note appears once; `refused` is false for `outdated`.
- `outdated_without_such_a_package_has_no_note`: untrusted tap, no package from it.
- `nosoak::tests::untrusted_soak_tap_refusal_adds_the_note_once`: a failing no-soak run whose output is `Error: Refusing to load formula ericfitz/brewsoak/hello from untrusted tap ericfitz/brewsoak.`: `notes` holds the `Untrusted` note exactly once and `status` is brew's exit code. The same refusal naming `hashicorp/tap` adds no such note.

- [ ] **Step 2: Run them.** Expected: failures (packages are held later or not at all; tokens get the unsoakable note).

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run the tests, then the done gate.**

- [ ] **Step 5: Commit**

```bash
git add src/soaktap.rs src/nosoak.rs src/cmd
git commit -m "feat: hold packages installed from an unusable soak tap and refuse own-tap tokens (#17)"
```

---

### Task 10: Bypass hints and no-soak tokens

Spec "Bypassing brewsoak" and "The no-soak step".

**Files:**
- Modify: `src/cmd/notes.rs:6-23` (`refusal_message`), new `bypass_command`
- Modify: `src/cmd/apply.rs:495-504` (`nosoak_target`), `:100-108` (reinstall no-soak branch), and the hint sites `:690`, `:701`, `:721`, `:940-966`, `:968-1000`, `:1002-1035`
- Modify: `src/nosoak.rs:86-92` (`Target`), `:183-187` (tokens in `run_step_after`)
- Test: `src/cmd/notes.rs`, `src/cmd/apply.rs`, `src/nosoak.rs`

**Interfaces:**
- Consumes: `soaktap::anchored`, `SoakTap::dir` (Task 5); `Pkg.own_tap` (Task 1); `by_name_note(.., bypass)` (Task 8).
- Produces:

```rust
// src/cmd/notes.rs
/// The brew command that bypasses brewsoak for one package.
pub(super) fn bypass_command(brew_verb: &str, token: &str, installed: Option<&Pkg>, anchored: bool) -> String;
pub fn refusal_message(action: DesiredAction, name: &str, bypass: &str) -> Option<String>;

// src/nosoak.rs
pub struct Target { /* existing fields */
    /// Installed from the soak tap: goes to brew under its official tap.
    pub official: bool }
/// `homebrew/core/<name>` (or `homebrew/cask/<name>` for a cask) when `official`, else `brew_token`.
pub fn target_token(t: &Target) -> String;
```

`bypass_command` table:

| Package | Result |
|---|---|
| Not installed; receipt does not name the soak tap; formula that is not anchored | `brew {brew_verb} {token}` |
| Anchored formula, `upgrade` or `install` | `brew upgrade homebrew/core/{name}` |
| Anchored formula, `reinstall` | `brew reinstall homebrew/core/{name}` |
| Cask whose receipt names the soak tap, `upgrade` or `install` | `brew upgrade --cask homebrew/cask/{name}` |
| The same cask, `reinstall` | `brew reinstall --cask homebrew/cask/{name}` |

`Target.official` is true for an installed formula that is anchored (`soaktap::anchored(self.soak.dir(), pkg)`) and for an installed cask with `own_tap == Some(OwnTap::Soak)`, for all three verbs; such a target has `kind` set.

- [ ] **Step 1: Write the failing tests**

- `notes::tests::bypass_command_table`: one assertion per row; a formula with `own_tap: Some(Legacy)` and one with `Some(Soak)` but `anchored == false` get `brew upgrade wget`.
- `apply::tests::refusal_of_an_anchored_formula_names_core`: an anchored formula that is too new at HEAD: the note ends ``use `brew upgrade homebrew/core/hello` to bypass brewsoak.``
- `apply::tests::reinstall_refusal_of_an_anchored_formula_names_core_reinstall`: ahead of soak, `brewsoak reinstall`: `brew reinstall homebrew/core/hello`.
- `apply::tests::soak_receipt_cask_hint_names_homebrew_cask_whether_or_not_its_file_exists`.
- `apply::tests::legacy_receipt_keeps_the_bare_hint`.
- `nosoak::tests::official_target_goes_to_brew_under_its_official_tap`: formula gives `["upgrade","homebrew/core/hello"]`; a cask `homebrew/cask/font-hack`; a non-official core target stays `hello`.
- `apply::tests::no_soak_anchored_formula_uses_the_core_token` and `no_soak_legacy_receipt_formula_keeps_its_bare_name`.

- [ ] **Step 2: Run them.** Expected: compile errors.

- [ ] **Step 3: Implement.** Every hint site named above computes the command once per target with `bypass_command`; the unsoakable hint (`src/cmd/apply.rs:566-576`) is third-party only and stays.

- [ ] **Step 4: Run the tests, then the done gate.**

- [ ] **Step 5: Commit**

```bash
git add src/cmd src/nosoak.rs
git commit -m "feat: name the official tap in bypass hints and no-soak tokens for soak-tap packages (#17)"
```

---

### Task 11: Output

Spec "Output" and "Dry run".

**Files:**
- Modify: `src/quiet.rs:154-173` (`line`, `filter_line`), `:337-415` (`section_line`), `:692-718` (`installed_versions`)
- Modify: `src/cmd/apply.rs:836-931` (`install_missing_dep`)
- Create: `tests/fixtures/soak_tap_run.txt`, `tests/fixtures/soak_tap_quiet.txt`
- Test: `src/quiet.rs`, `tests/quiet_golden.rs`, `src/cmd/apply.rs`

**Interfaces:**
- Consumes: `origin::SOAK_TAP`.
- Produces: no new public items. Rules:
  - Every line the filter shows has each `ericfitz/brewsoak/` removed. The log is unchanged, and `--raw` shows the line as brew wrote it.
  - `==> Trusted formula ericfitz/brewsoak/<name>` and `==> Trusted cask ericfitz/brewsoak/<name>` are not shown. A `Trusted` line for any other tap is shown as today.
  - `quiet::installed_from_output` reads `x` from `Warning: ericfitz/brewsoak/x 1.0 is already installed and up-to-date.`
  - On a dry run, a missing dependency that brewsoak would install prints `would install <dep> <cutoff version> (dependency of <target>)` once.
  - Brew's dependency preview must stay off the terminal in soaked mode. Its form was read in Homebrew 7.0.17-27-g96c963e (`Library/Homebrew/install.rb:494-497` and `:629`): a formula prints `==> Would install 1 dependency for parent:` (or `Would upgrade ...`), with the bare formula name, then one line per dependency; a cask prints the same heading with its full name. The existing rule at `src/quiet.rs:365-372` drops a `==> Would ` heading and its list; the tests below pin that for these forms.

- [ ] **Step 1: Write the failing tests**

- `quiet::tests::soak_tap_prefix_is_not_shown` (soaked mode, no expected version): `Error: ericfitz/brewsoak/hello: boom` shows `Error: hello: boom`; `Warning: ericfitz/brewsoak/hello 2.12.3 is available and more recent than version 2.12.2.` shows the same sentence starting `Warning: hello 2.12.3`.
- `quiet::tests::trusted_lines_for_the_soak_tap_go_to_the_log_only`: the two forms above give `None`; `==> Trusted formula hashicorp/tap/packer` is shown.
- `quiet::tests::dry_run_dependency_preview_of_a_soak_tap_target_is_dropped`: in soaked mode each of these gives `None`: `==> Would install 1 dependency for parent:` then `hello 2.12.3`; `==> Would upgrade 2 dependencies for parent:` then `hello 2.12.2 -> 2.12.3` and `wget 1.0 -> 1.1`; `==> Would install 1 dependency for ericfitz/brewsoak/font-hack:` then `hello`. A following `Error: boom` line is shown.
- `quiet::tests::installed_from_output_reads_full_names`.
- `cmd::apply::tests::already_installed_line_recognizes_full_names`: `warning: ericfitz/brewsoak/x 1.0 already installed` is true; the `but outdated` form is false.
- `soak_tap_transcript_collapses` in `tests/quiet_golden.rs`, built like `noisy_upgrade_transcript_collapses` (`:26`): the fixture `soak_tap_run.txt` holds, in order, the `Trusted formula` line, `==> Upgrading ericfitz/brewsoak/hello`, `  2.12.2 -> 2.12.3`, `==> Pouring hello--2.12.3.arm64_tahoe.bottle.tar.gz`, a Cellar summary line, then the formula dry-run preview block above. The expected file has no `ericfitz/brewsoak`, no `Trusted` and no `hello 2.12.3` preview line.
- `cmd::apply::tests::dry_run_reports_each_dependency_once_at_its_cutoff_version`: a dry-run upgrade of `alpha` with missing dependency `beta` (cutoff `1.1.0`): exactly one output line contains `beta`, and it is `would install beta 1.1.0 (dependency of alpha)`.

- [ ] **Step 2: Run them.** Expected: failures.

- [ ] **Step 3: Implement.** Create the expected golden file with `BREWSOAK_BLESS=1 cargo test --test quiet_golden`, then read it and confirm it matches the rules.

- [ ] **Step 4: Run the tests, then the done gate.**

- [ ] **Step 5: Commit**

```bash
git add src/quiet.rs src/cmd/apply.rs tests
git commit -m "feat: show bare names and keep brew's trust lines in the log (#17)"
```

---

### Task 12: Documentation

**Files:**
- Modify: `README.md:13-23` (install), `:155-169` (third-party taps and no-soak), `:195-221` (replace the issue #17 section)
- Modify: `AGENTS.md:3-19` ("Installing soaked formulae"), `:21-29` ("Origins and staging", the `Library/Taps` rule is at `:26`), `:52` (cache state)
- Modify: `docs/superpowers/specs/2026-08-12-brewsoakr-design.md:19` and `:179-181`; `docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md:49-51` and `:82-90`; `docs/superpowers/specs/2026-11-26-path-install-stopgap.md:1-6` and `:39-44`; `docs/superpowers/specs/2026-11-27-local-tap-install-design.md:13-14` (status)

**Interfaces:**
- Consumes: the behavior of Tasks 1 to 11. Check every claim against the source or the spec, not memory.
- Produces: documentation only.

- [ ] **Step 1: README.** Replace "Homebrew 7.0.14 and later (issue #17)" with a section "brewsoak's own tap" that states, each from the spec section named:
  - the minimum Homebrew, 7.0.14, and that the design was verified on one build, 7.0.17-27-g96c963e ("Supported Homebrew");
  - the first run creates `ericfitz/brewsoak` and holds until the user runs `brew trust ericfitz/brewsoak` once; brewsoak never runs it ("Tap name, location and bootstrap", "Trust"); add the same step to the install section;
  - brew records an `ericfitz/brewsoak/<name>` trust entry for every package brewsoak installs, upgrades or previews with `--dry-run`, and a dry run changes tap files for the length of the run ("Trust", "Dry run");
  - plain `brew upgrade` and `brew outdated` leave a package installed from the tap alone; the bypass commands table ("Interaction with plain `brew`", "Bypassing brewsoak");
  - attestation checks are skipped for bottles poured through the tap when `HOMEBREW_VERIFY_ATTESTATIONS` is set;
  - what untrusting the tap does and the two ways out: `brew trust ericfitz/brewsoak`, or `brew upgrade homebrew/core/<name>`;
  - a no-soak package's dependencies stay at their soaked versions (replace the sentence at `README.md:166-169`); `brewsoak outdated` does not list a no-soak package installed from the tap until it has been upgraded once;
  - a leftover `brewsoakr/soaked` tap can be removed with `brew untap brewsoakr/soaked`;
  - the repository `ericfitz/homebrew-brewsoak` does not exist on GitHub and must never be created, and why;
  - third-party tap packages keep the by-name gate, with the hold texts of Task 8.
- [ ] **Step 2: AGENTS.md.** Replace the stopgap rules with this design's: which packages use the tap; the tap-ready rule; tap files (sanitized cutoff, undo entry first, settle by the keg); verbs and tokens; the dependency table in one bullet; the two maintenance points; holds never fatal. Scope both "never touch `Library/Taps`" rules to allow `.rb` files under `Formula/` and `Casks/` of brewsoak's own tap. Add: never create `ericfitz/homebrew-brewsoak`; never run `brew trust`. Keep the sentence that `brewsoakr/soaked` survives only as `origin::STAGING_TAP`. Keep the rules that still hold (env pairs, `--ignore-dependencies`, receipts, `brew update`).
- [ ] **Step 3: Supersession notes**, each dated 2026-11-27, linking the new spec and naming the Human decision:
  - 2026-08-12: "brewsoak has no tap of its own" is amended: brewsoak owns `ericfitz/brewsoak` (Human decisions 1 and 5).
  - 2026-10-05: the "Accepted consequence" is amended (Human decision 4); the trust decision stands, with per-package trust accepted for brewsoak's own tap (Human decision 2).
  - 2026-11-26: superseded for `homebrew/core` and `homebrew/cask` packages; "Tokens: the bare name" is amended for soaked installs and stands for third-party packages.
  - 2026-11-27 spec: status line becomes "implemented by `../plans/2026-11-27-local-tap-install.md`".
- [ ] **Step 4: Check.** `rg -n 'issue #17|by bare name|no tap of its own' README.md AGENTS.md` shows no claim that is now false. Check the changed files for private hostnames and paths. Run the done gate.
- [ ] **Step 5: Commit**

```bash
git add README.md AGENTS.md docs/superpowers/specs
git commit -m "docs: document brewsoak's own tap and supersede the stopgap (#17)"
```

---

### Task 13: Manual release check (Eric runs this; implementers do not)

Real brew, on the maintainer's machine, before release. Nothing here is automated. If any observed result differs from "Expect", stop: it is a design change, not something to patch around. Tasks 8, 10 and 11 rely on three facts the spec lists under "Not verified"; they are confirmed here, so the branch must not be released before this task passes.

Setup: `cargo build --release`; `B=target/release/brewsoak`; `TAP="$(brew --repository)/Library/Taps/ericfitz/homebrew-brewsoak"`; `CACHE="$HOME/Library/Caches/brewsoak"`. Record the done-gate output first.

- [ ] **13.1 First real run creates the tap and holds.** `$B upgrade ast-grep`. Expect: `held ast-grep: brewsoak's tap is not ready`, the "brew does not trust it yet" note, exit 1; `brew tap-info --json ericfitz/brewsoak | jq '.[0] | {remote, trusted}'` prints `null` and `false`; `brew list --versions ast-grep` is unchanged; `jq . ~/.homebrew/trust.json` has no `ericfitz/brewsoak` entry.
- [ ] **13.2 Trust.** Eric runs `brew trust ericfitz/brewsoak`.
- [ ] **13.3 Dry run.** `$B upgrade --dry-run ast-grep`. Expect: the upgrade is announced, exit 0, no hold, and no dependency line at a version newer than its cutoff; afterwards `find "$TAP" -name '*.rb'` prints nothing and `$CACHE/soak-tap-undo` has no entry. Save the "Full brew log" of this run: if it has a `Would install` or `Would upgrade` dependency block, compare its lines with the preview block in `tests/fixtures/soak_tap_run.txt`; if brew's wording differs, replace the fixture lines, re-bless and re-run the done gate.
- [ ] **13.4 Real run** (needs Eric's approval: `Approve: brewsoak upgrade ast-grep on this machine`). `$B upgrade ast-grep`. Expect: `jq -r .source.tap "$(brew --cellar)"/ast-grep/*/INSTALL_RECEIPT.json` prints `ericfitz/brewsoak`; `brew outdated` does not list `ast-grep`; `$TAP/Formula/ast-grep.rb` exists; `$CACHE/soak-tap-undo` has no entry. Save the "Full brew log" and compare its lines with `tests/fixtures/soak_tap_run.txt`; replace the fixture and re-bless if brew's wording differs.
- [ ] **13.5 Bare reinstall of an anchored formula.** `brew reinstall ast-grep`. Expect: it reinstalls the installed version and the receipt still names `ericfitz/brewsoak`.
- [ ] **13.6 A real cask.** Pick an outdated cask `<token>` from `$B outdated`. `$B upgrade --dry-run <token>` (this runs `brew deps --1 --full-name --cask ericfitz/brewsoak/<token>`). Expect: no "could not load its soaked copy" hold. Then `$B upgrade <token>`. Expect: `jq -r .source.tap "$(brew --caskroom)/<token>/.metadata/INSTALL_RECEIPT.json"` prints `ericfitz/brewsoak`.
- [ ] **13.7 Cask bypass forms.** `brew upgrade --cask --dry-run homebrew/cask/<token>` and `brew reinstall --cask homebrew/cask/<token>`. Expect: the first plans an upgrade or reports the latest version; the second reinstalls and the receipt then names `homebrew/cask`.
- [ ] **13.8 A keg from another non-core tap.** Raw brew only: brewsoak classes a local throwaway tap as not soakable and would never issue this command. Skip it if `hello` is installed. First save `brew tap`, `brew list --formula` and `jq -S . ~/.homebrew/trust.json`. Then: `T="$(brew --repository)/Library/Taps/spike17c/homebrew-t3"`; `mkdir -p "$T/Formula"`; `brew cat hello > "$T/Formula/hello.rb"`; `brew trust spike17c/t3`; `brew install --formula spike17c/t3/hello`; copy the same file to `$TAP/Formula/hello.rb` with a line `revision 1` added after its `sha256` line; `brew upgrade --formula ericfitz/brewsoak/hello`. Expect: brew upgrades `hello` to the `_1` revision and `jq -r .source.tap "$(brew --cellar)"/hello/*_1/INSTALL_RECEIPT.json` prints `ericfitz/brewsoak`. Cleanup: `brew uninstall hello`; `rm "$TAP/Formula/hello.rb"`; `brew untap spike17c/t3`; `brew untrust --tap spike17c/t3`; `brew untrust --formula ericfitz/brewsoak/hello`; then the three saved outputs must match again.
- [ ] **13.9 `no_autobump!` in casks.** `git -C "$CACHE/cask.git" grep -n 'no_autobump!' refs/brewsoak/head -- Casks | rg -v 'no_autobump! because: (:[a-z_]+|"[^"]*")\s*$'`. Expect: no output (every stanza is one line).
- [ ] **13.10 Second run.** `$B upgrade ast-grep` again. Expect: nothing installed, no `tap-new`, no undo entry.

---

## Self-Review

1. **Spec coverage.** Tap name, classification, bootstrap: Tasks 1, 5. Trust and the four notes: Tasks 5, 8. Untrusted tap with packages: Tasks 3, 8, 9. Tap files, undo, settle: Tasks 4, 5, 8. Pruning and when the tap is touched: Tasks 6, 7. Commands: Tasks 4, 8. Dependency table: Tasks 5, 8. Stopgap gate removal and hold texts: Task 8. Bypass and no-soak tokens: Task 10. Receipts, inventory, origin, explicit tokens: Tasks 1, 3, 9. Casks: Tasks 4, 5, 8, 10. Dry run and output: Tasks 8, 11. Re-running: Tasks 5, 6, 8. Documentation and supersession: Task 12. Real-brew checks: Task 13. `taps::staging_root` and `taps::staged_load_failure`: removed in Task 1.
2. **Types.** `OwnTap`, `TapInfo`, `NotReady`, `Staged`, `DepRule`, `SoakTap`, `NameFail`, `NameRun` and `Target.official` are each defined in one task and used under the same name later.
3. **Review Focus.** Each of the five lines has a named test: `sanitize_leaves_a_blob_without_the_stanza_byte_for_byte` (1), `write_blob_paths_for_odd_names`, `stage_keeps_kinds_and_odd_names_apart`, `prune_is_per_kind` (2), `recover_handles_entries_it_never_wrote` (3), `prune_removes_a_stale_link_and_spares_its_target` (4), `soak_tap_upgrade_never_receives_g` (5).
4. **Open points resolved in this plan, for the maintainer to confirm.** Decisions P1 to P5 above; a package installed from an unusable soak tap is held even when it needs nothing (Task 9, the literal reading of the spec's "Holding" paragraph); the cask `reinstall` bypass form (Task 10) and the dry-run dependency line (Task 11); the copy of the own-tap token refusal (Task 9); the newest of several hidden kegs is chosen by `brew`'s version order (Task 3).
