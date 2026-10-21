# Sectioned Output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorganize brewsoak's apply-command output into headed, indented sections, report a per-package result for no-soak packages, print the summary as a table, and drop brew's contradicting "is available and more recent" warning for soaked installs.

**Architecture:** A lazy, indenting `Write` wrapper (`section::Section`) prints a header on first write and indents every line under it; each output phase writes through one. The brew line filter (`quiet::Filter`) gets a per-run context (`quiet::RunContext`) carrying a mode (soaked vs. no-soak) and the versions brewsoak expects to install, so the same brew lines can be dropped in one section and reported in another.

**Tech Stack:** Rust 2024, std only (no new crates).

**Spec:** `docs/superpowers/specs/2026-10-21-sectioned-output-design.md` (contains the approved mockup; every header string and layout below comes from it).

## Global Constraints

- Done gate (run before claiming any task done, show output): `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
- No new dependencies.
- Header strings, exact: `Fetching Homebrew history`, `Fetching tap history`, `Updating Homebrew`, `Summary`, `Notes`, `Caveats`; soaked section `<Verbing> soaked packages`, no-soak section `<Verbing> no-soak packages (brew decides)`, where `<Verbing>` is `Upgrading`/`Installing`/`Reinstalling`, or `Would upgrade`/`Would install`/`Would reinstall` on a dry run.
- Indent unit is two spaces. Section headers are at column 0.
- The raw brew log (`$TMPDIR/brewsoak-*.log`) must keep every byte; only the terminal view changes.
- American English in all new text; match surrounding comment density and naming.
- Existing tests that assert old strings must be updated to the new format, not deleted. Known sites: `src/snapshot.rs:693-696`, `src/snapshot.rs:868`, `src/cmd/upgrade.rs:228`, `src/cmd/upgrade.rs:1131`, `src/cmd/apply.rs:1776`, `src/nosoak.rs:400`, `src/nosoak.rs:572`, `src/report.rs:266-318`, `src/quiet.rs` tests near 498-520 and 700-730. Find the rest with `rg -n 'fetching |no-soak: |notes:|still soaking \(left|counts_line|already soaked:' src`.

## Review Focus

1. Soaked section must not grow duplicate lines: brew's `already installed` warnings stay dropped in soaked runs (the soaked path prints its own line at `src/cmd/apply.rs:636`). Test in Task 3.
2. A no-soak package brew never mentions (e.g. brew errors out early) must still get a result line (`no result from brew; see the full log`). Test in Task 3.
3. The "more recent" warning for a dependency brew pulls in (ca-certificates under certifi) must still show. Test in Task 4.
4. A bare upgrade with zero planned upgrades still prints `Upgrading soaked packages (0 of N)`; a named run where nothing is printed for soaked packages prints no soaked header at all. Test in Task 2.
5. Dry runs: headers switch to `Would …`, and the no-soak `Would upgrade` list is shown, not dropped. Tests in Tasks 2 and 3.

---

### Task 1: Section writer, fetch sections, Updating Homebrew section

**Files:**
- Create: `src/section.rs` (register in `src/lib.rs`)
- Modify: `src/snapshot.rs:141-225` (`refresh_with`, `refresh_taps`)
- Modify: `src/nosoak.rs:153` (brew update), `src/cmd/update.rs:225-235` (its brew update)
- Modify: `src/quiet.rs` (`normal_line`: drop brew's `Updating Homebrew...`)

**Interfaces:**
- Produces: `pub struct Section<W: Write>`; `Section::new(inner: W, header: impl Into<String>) -> Self` (lazy: header written on first byte); `Section::open(&mut self) -> std::io::Result<()>` (write header now if not yet written); `Section::started(&self) -> bool`; `Section::into_inner(self) -> W`; `impl<W: Write> Write for Section<W>` prefixing two spaces at each line start (blank lines get no trailing spaces). Nesting a `Section` inside a `Section` yields four spaces.

- [ ] **Step 1: Write failing tests in `src/section.rs`**
  - `lazy_header_only_on_first_write`: writing nothing → inner is empty; writing `"a\nb\n"` → `"H\n  a\n  b\n"`.
  - `open_writes_header_without_body`: `open()` then nothing → `"H\n"`.
  - `partial_lines_indent_once`: `write!("ab")`, `write!("c\n")` → `"H\n  abc\n"`.
  - `nested_sections_indent_twice`.
  - `blank_line_not_indented`: `"a\n\nb\n"` → `"H\n  a\n\n  b\n"`.
- [ ] **Step 2: Run `cargo test section::` — expect compile failure / FAIL.**
- [ ] **Step 3: Implement `Section`.**
- [ ] **Step 4: Update fetch output.** In `refresh_with`, write `Homebrew/homebrew-core` and `Homebrew/homebrew-cask` (the `CORE_REPO`/`CASK_REPO` values, no `fetching ` prefix, no `…`) through a `Section::new(&mut *progress, "Fetching Homebrew history")`. In `refresh_taps`, write each tap name through a lazy `Section::new(.., "Fetching tap history")` (no taps → no header). Update the tests at `src/snapshot.rs:693-696` and `:868` to assert the new lines, and add `fetch_sections_have_headers` asserting the exact block from the spec mockup for a core+cask+two-tap refresh, and `no_taps_no_tap_header`.
- [ ] **Step 5: Updating Homebrew section.** In `nosoak::run_step`, replace `writeln!(out, "no-soak: updating brew")` with running `brew update` through `Section::new(&mut *out, "Updating Homebrew")` after calling `open()`. Same in `cmd/update.rs` (replace `no-soak packages installed; updating brew`). In `quiet.rs` drop a `==> Updating Homebrew...` line (test `drops_updating_homebrew_banner`; `Already up-to-date.` still passes through). Update `src/nosoak.rs:400` test.
- [ ] **Step 6: Run the done gate — PASS.**
- [ ] **Step 7: Commit** `feat: section headers for fetch and brew update output`.

### Task 2: Soaked section, Summary table, grouped Notes, Caveats, Full brew log

**Files:**
- Modify: `src/cmd/upgrade.rs:28-38` (plan header), `src/cmd/apply.rs` (session output, `defer`, `write_tail`, trailing `already soaked:` line at ~248), `src/report.rs:185-205` (`counts_line` → `summary_lines`), `src/cmd/notes.rs:92-103` (`write_session_tail`)

**Interfaces:**
- Consumes: `section::Section` (Task 1).
- Produces:
  - `report::Counts` gains `no_soak_upgraded: usize`, `no_soak_current: usize`, `no_soak_no_result: usize` (filled by Task 3; zero until then).
  - `pub fn summary_lines(c: &Counts, added_bytes: u64, freed_bytes: u64) -> Vec<String>` replacing `counts_line` (delete `counts_line`).
  - Section header builders in `report.rs`: the verb is `pub enum Verb { Upgrade, Install, Reinstall }` in `report.rs` with `Verb::parse(&str) -> Option<Verb>`; callers turn `None` into an `Error` rather than falling back to a default (unknown values are rejected). Signatures: `soaked_header(verb: Verb, dry_run: bool, plan: Option<(usize, usize)>) -> String`, `nosoak_header(verb: Verb, dry_run: bool) -> String`.
  - `ApplySession` notes become typed: `enum NoteKind { Held, Other }`, `fn defer(&mut self, msg)` keeps `Other`, new `fn defer_held(&mut self, msg)`; ahead-of-soak packages go into `ahead: Vec<String>` (names only).

- [ ] **Step 1: Failing tests in `report.rs`:**
  - `summary_lines_table`: counts `upgraded 2, soaked 162, soaking 8, ahead 1, no_soak 4 (1 upgraded, 3 current), auto_updates 2`, added `31.2MB`-worth bytes, freed `28.5MB`-worth → exactly the 11 Summary rows of the spec mockup (without the 2-space section indent; `Section` adds it). Format: `format!("{label:<lw$} {value:>vw$}")` where `lw`/`vw` are the max label/value widths, then for no-soak the suffix `  (1 upgraded, 3 already up-to-date)`; suffix only when `no_soak > 0`; add `, N no result` when `no_soak_no_result > 0`; dry run labels `would upgrade` and suffix `N would upgrade`.
  - `summary_keeps_zero_rows`: all-zero counts → 11 rows, sizes `0B`.
  - `headers_by_verb`: `soaked_header(Upgrade,false,Some((2,179)))` = `Upgrading soaked packages (2 of 179)`; `(Install,true,None)` = `Would install soaked packages`; `nosoak_header(Reinstall,false)` = `Reinstalling no-soak packages (brew decides)`.
  - `verb_parse_rejects_unknown`: `Verb::parse("frobnicate") == None`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement the report functions.**
- [ ] **Step 4: Soaked section.** In `upgrade.rs` remove the `upgrading N of M packages` line; in `apply_many` the bare-upgrade path writes the soaked section header eagerly (`open()`) with `(plan_total of inv.pkgs.len())`; named runs and install/reinstall write it lazily without count. All per-package output (progress lines, brew filter output, `pinned; skipped`, `is already installed`, etc.) goes through the soaked `Section`. The verbose `soak_banner` stays at column 0 before the section. Tests (in `apply.rs`/`upgrade.rs` test modules, using the existing `MockBrew` helpers): `bare_upgrade_zero_plan_prints_header` (`Upgrading soaked packages (0 of N)` present), `named_run_nothing_to_do_no_soaked_header`, `dry_run_soaked_header`.
- [ ] **Step 5: Tail.** `write_tail` writes, in order: `Summary` section (rows from `summary_lines`), `Notes` section, `Caveats` section (lazy), then `Full brew log: <path>` at column 0. Notes layout inside the `Notes` section: `Still soaking (left at the cutoff):` then each soaking entry on its own line indented two more; `Ahead of soak (installed artifact left unchanged):` then each name; `Held:` then each held note; then `Other` notes one per line with no sub-heading. Multi-line notes: continuation lines indented two more than their first line. Empty groups are omitted. Classify as Held: refusals (`RefuseTooNew/Yanked/Deprecated`), `HoldUnordered`, reinstall-ahead and reinstall-newer refusals, `held_tap_note`, `unsoakable_note`. `write_session_tail` (also used by `cmd/update.rs`) gets the same `Caveats` section + `Full brew log:` capitalization. Remove the trailing `already soaked: N formulae and casks` line. Update tests at `upgrade.rs:228`, `upgrade.rs:1131`, `apply.rs:1776`, `report.rs:266-318`; add `notes_grouped_one_per_line` asserting the Notes block shape from the mockup.
- [ ] **Step 6: Run the done gate — PASS.**
- [ ] **Step 7: Commit** `feat: soaked section, summary table and grouped notes`.

### Task 3: No-soak per-package results

**Files:**
- Modify: `src/quiet.rs` (`Filter`, `start_run`), `src/brew.rs:61-80` (trait), `:374`, `:730-770`, `:777` (real + mock), `src/nosoak.rs:135-260`, `src/cmd/apply.rs:1086-1112` (`run_nosoak`)

**Interfaces:**
- Consumes: `Section`, `report::nosoak_header`, `Verb`, `Counts.no_soak_*` (Tasks 1–2).
- Produces:
  - `pub enum RunMode { Soaked, NoSoak }` (default `Soaked`) and `#[derive(Default)] pub struct RunContext { pub mode: RunMode }` in `quiet.rs`; `Filter::start_run(&mut self, ctx: &RunContext)`.
  - `Brew::run_visible_in(&self, args: &[String], terminal: &mut dyn Write, ctx: &quiet::RunContext) -> Result<Output, Error>`; `run_visible` becomes a provided method calling it with `RunContext::default()`. `MockBrew` records the last `RunContext` it was given so tests can assert the mode.
  - `nosoak::StepResult` gains `upgraded: usize`, `current: usize`, `no_result: usize`; `run_nosoak` copies them into `Counts.no_soak_*`.

Filter behavior in `RunMode::NoSoak` (Soaked keeps today's behavior exactly):
- `Warning: <token> <version> already installed` and `Warning: <token> <version> is already installed and up-to-date.` → emit `"<token> <version>\n  Already up-to-date."`.
- `==> Upgrading <name>` followed by `  <a> -> <b>` → emit `upgrading <name> <a> -> <b>` (the following `Pouring` line already yields `  installing …`). A cask upgrade with no version line emits `upgrading <name>` when the next non-version line arrives.
- `==> Would upgrade N formula(e)/cask(s):` list lines `<name> <a> -> <b>` → emit `would upgrade <name> <a> -> <b>`; `Would upgrade N dependenc…` lists stay dropped.

`run_step` changes: run the verb's brew command(s) through `Section::new(out, nosoak_header(..))` with `RunContext { mode: NoSoak }`; delete the `no-soak: brew …` echo lines; for each tap-switch target write `reinstalling <token> (moving staged keg to its tap)` before its reinstall run. After the runs, classify each target from captured stdout (match the token or its last `/` segment, case-insensitive): upgraded (an `Upgrading`/`Pouring`/Cellar line or `installed` map entry), current (an already-installed warning), else no result → write `<token>\n  no result from brew; see the full log` in the section.

- [ ] **Step 1: Failing tests.** In `quiet.rs`: `nosoak_already_installed_reported` (both warning forms), `nosoak_upgrade_line` (fixture: `==> Upgrading ericfitz/tap/agentbus\n  1.15.0 -> 1.16.0\n==> Pouring agentbus--1.16.0.arm64_sequoia.bottle.tar.gz\n` → `upgrading ericfitz/tap/agentbus 1.15.0 -> 1.16.0`, `  installing agentbus 1.16.0`), `nosoak_dry_run_list` (fixture `==> Would upgrade 1 formula:\ncertifi 2026.7.22_1 -> 2026.10.18\n==> Would upgrade 1 dependency for certifi:\nca-certificates 2026-09-25 -> 2026-10-20\n` → only `would upgrade certifi 2026.7.22_1 -> 2026.10.18`), `soaked_mode_still_drops_already_installed` (existing behavior pinned). In `nosoak.rs`: `classifies_upgraded_current_and_no_result` (mock stdout mentions one target upgraded, one already installed, third absent → counts 1/1/1 and output contains `no result from brew; see the full log` under the third token), `runs_in_nosoak_mode` (MockBrew's recorded context), `switch_line_per_package` (replaces the old `:572` assertion).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement `RunContext`, trait method, filter mode.**
- [ ] **Step 4: Implement `run_step` changes and `run_nosoak` count copy.**
- [ ] **Step 5: Run the done gate — PASS.**
- [ ] **Step 6: Commit** `feat: per-package results for no-soak packages`.

### Task 4: Drop brew's "more recent" warning for the version being installed

**Files:**
- Modify: `src/quiet.rs` (`RunContext`, `normal_line`), `src/cmd/apply.rs` (where the soaked install/upgrade calls brew)

**Interfaces:**
- Consumes: `RunContext`, `Brew::run_visible_in` (Task 3).
- Produces: `RunContext` gains `pub expected: BTreeMap<String, String>` (package name, lowercase, no tap prefix → version brewsoak is installing).

Rule: drop `Warning: <x> <v> is available and more recent than version <c>.` iff `expected` has `<x>` (compare its last `/` segment, lowercase) mapped to exactly `<c>`. Otherwise pass it through unchanged.

- [ ] **Step 1: Failing tests in `quiet.rs`:** `drops_more_recent_for_expected_version` (fixture line from the cats log: `Warning: cats 15.0.3 is available and more recent than version 15.0.0.` with expected `cats→15.0.0` → dropped), `keeps_more_recent_for_dependency` (`ca-certificates` not in expected → shown), `keeps_more_recent_when_version_differs` (expected `cats→14.0.0` → shown), `keeps_more_recent_with_tap_prefix_match` (`endava/tap/cats` matches expected `cats`).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement; in `apply.rs` pass `expected = {name: cutoff identity_version}` for the package each soaked brew run installs.** Add an apply-level test asserting MockBrew's recorded context carries the cutoff version.
- [ ] **Step 4: Run the done gate — PASS.**
- [ ] **Step 5: Commit** `fix: drop brew's newer-version warning for the soaked version being installed`.

### Task 5: README output examples

**Files:**
- Modify: `README.md` (every example of upgrade/install output, `notes:`, `fetching`, counts line)

- [ ] **Step 1:** `rg -n 'fetching |no-soak: |notes:|still soaking|upgraded [0-9]|already soaked' README.md` and rewrite each example in the new format, taken from a real `cargo run -- upgrade --dry-run` on this machine (Docs rule: check against live output). No private hostnames or paths beyond `$TMPDIR`-style placeholders.
- [ ] **Step 2: Run the done gate — PASS.**
- [ ] **Step 3: Commit** `docs: README output examples for sectioned output`.
