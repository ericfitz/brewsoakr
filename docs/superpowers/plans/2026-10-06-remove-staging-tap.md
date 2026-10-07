# Remove the Staging Tap Code Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete the dead `brewsoakr/soaked` staging-tap code (`brew tap-new` + `brew trust`, and the `skip_tap_trust` fallback) and bring AGENTS.md and the design specs in line with what brewsoak does today, while keeping the rule that a `brewsoakr/soaked` receipt or tap-info entry is classified as staged.

**Architecture:** brewsoak installs every soaked package from a staged `.rb` path under `<cache>/staging/` (`brew install --formula <path>`), so it never needs a tap of its own. The `Brew` trait method `tap_new_soaked`, its two implementations, the `trust_soaked_tap` helper, the `skip_tap_trust` cell and the `HOMEBREW_NO_REQUIRE_TAP_TRUST` env branch are removed in one pass; the compiler and clippy name every ripple. `origin::STAGING_TAP` and `TapClass::Staging` stay: a keg staged by a pre-1.0 build may still carry the name in its receipt, and the maintainer's machine still has the tap directory.

**Tech Stack:** Rust 2024, `std::process::Command` behind the `Brew` trait, `MockBrew` for tests. No dependency changes.

**Spec:** GitHub issue #4 (`gh issue view 4 --repo ericfitz/brewsoakr`). Design context: `docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md` (Human decisions: "brewsoak never trusts a third-party tap on the user's behalf") and `docs/superpowers/specs/2026-08-12-brewsoakr-design.md` ("Invoking brew").

## Evidence: the staging tap is dead code

Every reference to the staging-tap machinery in `src/` (from `rg -n 'tap_new_soaked|trust_soaked_tap|skip_tap_trust|tap_already_exists|trust_command_unavailable|NO_REQUIRE_TAP_TRUST' /Users/efitz/Projects/brewsoakr/src`, 2026-10-06, commit `85792ee`):

| Location | What | Production or test |
|---|---|---|
| `src/brew.rs:39` | Trait doc comment mentions "tap-new, and trust" (and `--repository`, which nothing calls either; `brew_dir` is only called with `--cellar`) | production (doc) |
| `src/brew.rs:44` | `fn tap_new_soaked(&self) -> Result<(), Error>;` on `trait Brew` | production (definition) |
| `src/brew.rs:65`, `:108` | `skip_tap_trust: Cell<bool>` field and initializer on `ProcessBrew` | production |
| `src/brew.rs:239-249` | `impl Brew for ProcessBrew`: `tap_new_soaked` runs `brew tap-new brewsoakr/soaked --no-git`, then `trust_soaked_tap()` | production (definition, no caller) |
| `src/brew.rs:285-310` | `apply_brewsoak_brew_env(cmd, skip_tap_trust)` / `brewsoak_brew_env_pairs(skip_tap_trust)`; the `bool` only adds `HOMEBREW_NO_REQUIRE_TAP_TRUST=1` | production |
| `src/brew.rs:336` | `spawn_brew` reads `self.skip_tap_trust.get()` | production |
| `src/brew.rs:375-386` | `ProcessBrew::trust_soaked_tap` runs `brew trust brewsoakr/soaked`; on "unknown command" sets `skip_tap_trust` | production (only caller: `tap_new_soaked`) |
| `src/brew.rs:425-432` | `trust_command_unavailable` | production (only caller: `trust_soaked_tap`) |
| `src/brew.rs:466-474` | `impl Brew for MockBrew`: `tap_new_soaked` records `tap-new` and `trust` runs | test double |
| `src/brew.rs:537-539` | `tap_already_exists` | production (only caller: `tap_new_soaked`) |
| `src/brew.rs:1473`, `:1529` | Tests call `brewsoak_brew_env_pairs(false)` | test |
| `src/brew.rs:1598-1608` | Test `mock_tap_new_soaked_records_trust`: the only call of `tap_new_soaked()` in the tree | test |
| `src/cmd.rs:3297-3299`, `:3383-3385` | `fn tap_new_soaked` forwarding wrappers in the two test-only `FailDepsBrew` impls (inside `#[cfg(test)] mod tests`, which starts at `src/cmd.rs:2278`) | test |

Nothing in `src/cmd.rs`, `src/tap.rs`, `src/inventory.rs`, `src/taps.rs`, `src/origin.rs`, `src/lib.rs`, `src/main.rs` or `tests/` calls `tap_new_soaked`, `trust_soaked_tap` or reads `skip_tap_trust`.

History (`git grep -n 'tap_new_soaked()' <ref> -- src`):

- `a94462a` (feat: soak-aware upgrade and install via local tap): `src/cmd.rs:421: self.brew.tap_new_soaked()?;` is the only production call that ever existed.
- `d322486` (2026-08-13, fix: install soaked formulae from a file path) removed that call. `d322486` is `v1.0.0~5`, so no released brewsoak (`v1.0.0` through `v1.2.0`) has ever created or trusted `brewsoakr/soaked`. At `v1.0.0` and `v1.2.0` the only call is the mock test.

Consequences:

- A `brewsoakr/soaked` tap or receipt can only exist on a machine that ran a pre-release build (the maintainer's machine does; the `brew tap-info` fixture at `src/taps.rs:147` shows `/opt/homebrew/Library/Taps/brewsoakr/homebrew-soaked`). The classification rule stays and is already pinned by tests: `src/taps.rs:165-180` `classify_follows_the_spec_table` (`TapClass::Staging`), `src/inventory.rs:298` (`inv.tap_class("brewsoakr/soaked") == Some(TapClass::Staging)`), `src/brew.rs:1387` (a receipt whose `source.tap` is `brewsoakr/soaked` is read as the staging tap, not an origin). `origin::STAGING_TAP` (`src/origin.rs:12`) stays.
- The test `untrusted_tap_refusal_holds_the_target_and_never_runs_brew_trust` (`src/cmd.rs:3137-3165`) asserts no `trust` run happens. `cmd.rs` never called `tap_new_soaked`, so this assertion was never about the staging tap; it pins the 2026-10-05 human decision that brewsoak never runs `brew trust` for a third-party tap. After this change no code path can emit `trust`, so the assertion is a pure regression guard against reintroduction. **Keep it unchanged.**
- The `Brew` trait doc comment at `src/brew.rs:39` is rewritten; nothing else in the trait changes.

Verdict: acceptance criterion (2) of the issue applies (delete); criterion (3) does not.

## Global Constraints

- Done gate for every task end and the final step: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`. Baseline on `main` (`85792ee`): all pass, `367 passed` in the lib unit tests plus `1 passed` in `tests/quiet_golden.rs` (368 total).
- Work on branch `issue-4-remove-staging-tap` cut from `main`. The orchestrator squashes the branch into one commit on `main`; per-task commits are still made on the branch.
- Every commit message ends with these two trailer lines, verbatim:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy
  ```
- American English in code, comments and docs. Rust edition 2024.
- `rg` always gets an explicit path (`rg PATTERN /abs/path`).
- Stage only the files named in each task; never `git add -A`. Do not stage `HANDOFF.md` (untracked, machine-local).
- Project rules (AGENTS.md) that this change must not disturb: install from a staged `.rb` path, never a `brewsoakr/soaked/<name>` token; every brew child keeps `HOMEBREW_DEVELOPER=1`, `HOMEBREW_NO_AUTO_UPDATE=1`, `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1`, `HOMEBREW_NO_COLOR=1`, and `HOMEBREW_FORBID_PACKAGES_FROM_PATHS` unset; a `brewsoakr/soaked` receipt tap is "staged", never an origin; `brew tap-info` entries named `brewsoakr/soaked` are classified by name before the "no HTTPS remote = unsoakable" rule.
- `use std::cell::Cell;` at `src/brew.rs:5` stays: `ProcessBrew.raw: Cell<bool>` still needs it.

## Review Focus

Input classes and failure modes a person could hit after this change, with the test that pins each:

1. A keg installed by a pre-release build whose `INSTALL_RECEIPT.json` has `"source": {"tap": "brewsoakr/soaked"}`: the origin must still fall through to `origins.toml`, then core, never `brewsoakr/soaked`. Pinned by `src/brew.rs:1387` (`read_formula_receipt_source` test) and AGENTS.md "Origins and staging"; unchanged by Task 1 and re-run by its done gate.
2. A machine whose `brew tap-info --json --installed` still lists `brewsoakr/soaked` with `remote: null`: it must be classified `TapClass::Staging` and ignored, never fetched and never "unsoakable". Pinned by `src/taps.rs:165-180` `classify_follows_the_spec_table` and `src/inventory.rs:298`; unchanged, re-run by Task 1's gate.
3. A brew child must never run with `HOMEBREW_NO_REQUIRE_TAP_TRUST` set: brewsoak must not switch Homebrew's tap-trust check off for anyone, which the old `skip_tap_trust` fallback did. New regression pin in Task 1: `brewsoak_brew_env_never_disables_tap_trust`.
4. An untrusted-tap refusal from brew (Homebrew 7.0.8+ `Refusing to load formula ... from untrusted tap ...`) must still hold the package with the `run brew trust <tap>` note and never run `brew trust` itself. Pinned by `src/cmd.rs:3137` `untrusted_tap_refusal_holds_the_target_and_never_runs_brew_trust` and `src/cmd.rs:3368` `deps_untrusted_tap_refusal_holds_with_trust_note`; both keep their `tap_new_soaked`-free `FailDepsBrew` wrappers after Task 1 and are re-run by its gate.
5. A reader of the 2026-08-12 design spec following "Invoking brew" step 1 (`brew tap-new brewsoak/soaked --no-git`) by hand would create a tap brewsoak never uses. Task 2 adds a dated supersession note at that section and at the "local tap" lock so the historical text is marked, not silently rewritten. There is no automated test for docs; Task 2's step 4 greps the specs for remaining live-tense mentions.

---

### Task 1: Remove the staging-tap code from `src/brew.rs` and the test wrappers in `src/cmd.rs`

**Files:**
- Modify: `src/brew.rs:39`, `:44`, `:65`, `:108`, `:239-249`, `:285-310`, `:336`, `:375-386`, `:425-432`, `:466-474`, `:537-539`, `:1473`, `:1529`, `:1598-1608` (line numbers as of `85792ee`; they shift as you delete, so work top-down or use the anchors quoted below)
- Modify: `src/cmd.rs:3297-3299`, `src/cmd.rs:3383-3385`
- Test: `src/brew.rs` (`mod tests`, starts at `:884`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `trait Brew` without `tap_new_soaked`; `fn brewsoak_brew_env_pairs() -> Vec<(&'static str, Option<&'static str>)>` and `fn apply_brewsoak_brew_env(cmd: &mut Command)` (both private to `brew.rs`, no `bool` parameter); `ProcessBrew` without a `skip_tap_trust` field. Task 2 relies on none of these; it only cites them in docs.

- [ ] **Step 1: Create the branch**

```bash
cd /Users/efitz/Projects/brewsoakr && git switch -c issue-4-remove-staging-tap main
```

- [ ] **Step 2: Remove the trait method and let the compiler list the ripple (the "red" step for a deletion)**

In `src/brew.rs`, delete this line from `pub trait Brew` (currently `:44`):

```rust
    fn tap_new_soaked(&self) -> Result<(), Error>;
```

and change the `run` doc comment just above it (currently `:39`) from

```rust
    /// Capturing run for JSON, deps, `--repository`, tap-new, and trust.
```

to

```rust
    /// Capturing run for JSON (`info`, `outdated`, `tap-info`), `deps`, and `--cellar`.
```

Run: `cd /Users/efitz/Projects/brewsoakr && cargo build --all-targets --keep-going 2>&1 | rg -n 'E0407|-->'`
Expected: `error[E0407]: method `tap_new_soaked` is not a member of trait `Brew`` four times in total: two in `src/brew.rs` (the `ProcessBrew` impl and the `MockBrew` impl) and two in `src/cmd.rs` (the two `FailDepsBrew` impls). The lib and the lib-test target compile as separate units, so without `--keep-going` the two files can show up on consecutive builds. If an error names any other file, or any error other than E0407 mentions `tap_new_soaked`, stop: that is a caller this plan did not find, and the issue's acceptance criterion (3) applies instead.

- [ ] **Step 3: Delete the four implementations**

In `src/brew.rs`, inside `impl Brew for ProcessBrew`, delete the whole method (anchor: `fn tap_new_soaked(&self) -> Result<(), Error> {` followed by `"tap-new".into(),`):

```rust
    fn tap_new_soaked(&self) -> Result<(), Error> {
        let output = self.run(&[
            "tap-new".into(),
            "brewsoakr/soaked".into(),
            "--no-git".into(),
        ])?;
        if !(output.status.success() || tap_already_exists(&output)) {
            return Err(brew_fail(&output));
        }
        self.trust_soaked_tap()
    }

```

In `src/brew.rs`, inside `impl Brew for MockBrew`, delete:

```rust
    fn tap_new_soaked(&self) -> Result<(), Error> {
        let _ = self.run(&[
            "tap-new".into(),
            "brewsoakr/soaked".into(),
            "--no-git".into(),
        ])?;
        let _ = self.run(&["trust".into(), "brewsoakr/soaked".into()])?;
        Ok(())
    }

```

In `src/cmd.rs`, in **both** `impl Brew for FailDepsBrew` blocks (tests `deps_failure_on_staged_tap_formula_holds_with_note` and `deps_untrusted_tap_refusal_holds_with_trust_note`), delete:

```rust
            fn tap_new_soaked(&self) -> Result<(), Error> {
                self.0.tap_new_soaked()
            }
```

Run: `cd /Users/efitz/Projects/brewsoakr && cargo build --all-targets 2>&1 | rg -n '^(error|warning)' `
Expected: `cargo build --all-targets` compiles. There are no `#![deny(warnings)]` attributes, so dead-code *warnings* appear here for `trust_soaked_tap`, `trust_command_unavailable` and `tap_already_exists`; they become errors under the clippy gate. Step 4 removes them.

- [ ] **Step 4: Delete the trust fallback, the `skip_tap_trust` cell, and the two helpers**

In `src/brew.rs`, `pub struct ProcessBrew` (anchor `skip_tap_trust: Cell<bool>,`): delete the field line

```rust
    skip_tap_trust: Cell<bool>,
```

In `ProcessBrew::new` (anchor `skip_tap_trust: Cell::new(false),`): delete

```rust
            skip_tap_trust: Cell::new(false),
```

Keep `use std::cell::Cell;` at the top of the file: `raw: Cell<bool>` still uses it.

Replace the env helpers (anchor `fn apply_brewsoak_brew_env(cmd: &mut Command, skip_tap_trust: bool) {` through the closing brace of `brewsoak_brew_env_pairs`) with this exact text; the doc comment above `apply_brewsoak_brew_env` is unchanged:

```rust
fn apply_brewsoak_brew_env(cmd: &mut Command) {
    for (key, value) in brewsoak_brew_env_pairs() {
        match value {
            Some(v) => {
                cmd.env(key, v);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
}

fn brewsoak_brew_env_pairs() -> Vec<(&'static str, Option<&'static str>)> {
    vec![
        ("HOMEBREW_NO_AUTO_UPDATE", Some("1")),
        ("HOMEBREW_NO_COLOR", Some("1")),
        ("HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK", Some("1")),
        ("HOMEBREW_DEVELOPER", Some("1")),
        ("HOMEBREW_FORBID_PACKAGES_FROM_PATHS", None),
    ]
}
```

In `ProcessBrew::spawn_brew` (anchor `apply_brewsoak_brew_env(&mut cmd, self.skip_tap_trust.get());`) change the call to:

```rust
        apply_brewsoak_brew_env(&mut cmd);
```

In `impl ProcessBrew` (the block that also holds `cellar`, `brew_dir`, `spawn_brew`), delete the whole method:

```rust

    fn trust_soaked_tap(&self) -> Result<(), Error> {
        let output = self.run(&["trust".into(), "brewsoakr/soaked".into()])?;
        if output.status.success() {
            return Ok(());
        }
        if trust_command_unavailable(&output) {
            // Homebrew < 6 has no `brew trust`. Disable the Homebrew 6 check
            // for later brew deps/install of brewsoakr/soaked/*.
            self.skip_tap_trust.set(true);
            return Ok(());
        }
        Err(brew_fail(&output))
    }
```

Delete the free function (anchor `fn trust_command_unavailable(output: &Output) -> bool {`):

```rust
fn trust_command_unavailable(output: &Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    text.contains("unknown command")
        || text.contains("unknown subcommand")
        || text.contains("invalid command")
}

```

Delete the free function (anchor `fn tap_already_exists(output: &Output) -> bool {`):

```rust
fn tap_already_exists(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stderr).contains("already exists")
}

```

`brew_fail` stays: `tap_info`, `outdated_names`, `deps` and `installed_packages` still call it.

- [ ] **Step 5: Update the tests that used the removed API, and add the regression pin**

In `src/brew.rs` `mod tests`, change both existing calls

```rust
        let pairs = brewsoak_brew_env_pairs(false);
```

(in `brewsoak_brew_env_enables_path_installs` and `brewsoak_brew_env_turns_brew_colour_off`) to

```rust
        let pairs = brewsoak_brew_env_pairs();
```

Delete the test (anchor `fn mock_tap_new_soaked_records_trust()`):

```rust
    #[test]
    fn mock_tap_new_soaked_records_trust() {
        let brew = MockBrew::new();
        brew.tap_new_soaked().expect("tap");
        let runs = brew.runs.lock().expect("runs");
        assert!(
            runs.iter()
                .any(|args| args == &["trust".to_string(), "brewsoakr/soaked".into()]),
            "expected brew trust brewsoakr/soaked: {runs:?}"
        );
    }

```

Add this test directly after `brewsoak_brew_env_turns_brew_colour_off`. It is a regression pin for Review Focus item 3 (it passes immediately; the "red" for this task was Step 2):

```rust
    #[test]
    fn brewsoak_brew_env_never_disables_tap_trust() {
        // brewsoak never trusts a tap on the user's behalf (2026-10-05 human
        // decision), so it must not switch Homebrew's tap-trust check off
        // either. The removed brewsoakr/soaked fallback used to set this.
        let pairs = brewsoak_brew_env_pairs();
        assert!(
            !pairs
                .iter()
                .any(|(k, _)| *k == "HOMEBREW_NO_REQUIRE_TAP_TRUST"),
            "{pairs:?}"
        );
    }
```

Then run `cd /Users/efitz/Projects/brewsoakr && cargo fmt` so rustfmt reflows the new block before the `cargo fmt --check` gate, and `cargo test --lib brewsoak_brew_env` (expected: 3 passed).

- [ ] **Step 6: Confirm nothing is left and the kept rule is intact**

Run:

```bash
rg -n 'tap_new_soaked|trust_soaked_tap|skip_tap_trust|tap_already_exists|trust_command_unavailable|tap-new' /Users/efitz/Projects/brewsoakr/src /Users/efitz/Projects/brewsoakr/tests
```

Expected: no output.

Run:

```bash
rg -n 'NO_REQUIRE_TAP_TRUST' /Users/efitz/Projects/brewsoakr/src
```

Expected: exactly one line, inside `brewsoak_brew_env_never_disables_tap_trust`.

Run:

```bash
rg -n 'STAGING_TAP|TapClass::Staging' /Users/efitz/Projects/brewsoakr/src
```

Expected: still present in `src/origin.rs` (the const), `src/taps.rs` (`classify` and `classify_follows_the_spec_table`), `src/inventory.rs:298`, and wherever `brew.rs` reads receipts. These are the kept rule; do not touch them.

- [ ] **Step 7: Done gate**

Run: `cd /Users/efitz/Projects/brewsoakr && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
Expected: fmt clean, clippy clean, build ok, `test result: ok. 367 passed` for the lib (367 lib baseline, minus `mock_tap_new_soaked_records_trust`, plus `brewsoak_brew_env_never_disables_tap_trust`) and `test result: ok. 1 passed` for `tests/quiet_golden.rs`; 368 total, unchanged from `main`. Paste the four `test result:` lines into the task report.

- [ ] **Step 8: Commit**

```bash
cd /Users/efitz/Projects/brewsoakr && git add src/brew.rs src/cmd.rs && git commit -F - <<'EOF'
refactor: remove the unused brewsoakr/soaked staging tap code

brew tap-new / brew trust brewsoakr/soaked and the skip_tap_trust
fallback have had no production caller since d322486 (path installs,
before 1.0.0). The staging-tap name stays in origin::STAGING_TAP so a
receipt or tap-info entry left by a pre-release build is still
classified as staged, never as an origin.

Closes #4.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy
EOF
```

---

### Task 2: Bring AGENTS.md and the design specs in line

**Files:**
- Modify: `AGENTS.md:5`, `AGENTS.md:19`
- Modify: `docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md:40-45` (Human decisions bullet), `:57-72` (the "Rulings confirmed by the maintainer (2026-10-06)" list), `:192` (tap classification table row)
- Modify: `docs/superpowers/specs/2026-08-12-brewsoakr-design.md:19` (lock line) and `:177-183` ("Invoking brew" section)
- Not modified: `README.md` (no staging-tap mention), `docs/superpowers/plans/2026-08-12-brewsoakr.md` and `docs/superpowers/plans/2026-10-05-tap-soak-and-no-soak.md` (historical execution records; leave as written), `HANDOFF.md` (untracked).

**Interfaces:**
- Consumes: the names from Task 1 as they now exist (`origin::STAGING_TAP`, `TapClass::Staging`, no `tap_new_soaked`). Docs cite them; no code changes.
- Produces: nothing for later tasks.

Policy for this task: the 2026-10-05 spec's "Human decisions" section and the 2026-08-12 "Decisions (locked)" list are maintainer rulings. Do not rewrite a dated decision silently; amend with a dated attribution to issue #4 (the maintainer's own request), or add a dated supersession note, matching the "Supersedes the 2026-08-12 lock" pattern the 2026-10-05 spec already uses.

- [ ] **Step 1: AGENTS.md**

Replace line 5

```markdown
- Install from a staged `.rb` path under the brewsoak cache, not `brewsoakr/soaked/<name>`.
```

with

```markdown
- Install from a staged `.rb` path under the brewsoak cache. brewsoak has no tap of its own; `brewsoakr/soaked` survives only as `origin::STAGING_TAP` so pre-1.0 receipts and `tap-info` entries classify as staged, never as an origin (issue #4).
```

Replace line 19

```markdown
- `brew tap-info --json --installed` reports `remote: null` for API-mode `homebrew/core` and `homebrew/cask` and for the staging tap; classify those by name before applying the "no HTTPS remote = unsoakable" rule.
```

with

```markdown
- `brew tap-info --json --installed` reports `remote: null` for API-mode `homebrew/core` and `homebrew/cask` and for a leftover `brewsoakr/soaked` tap; classify those by name before applying the "no HTTPS remote = unsoakable" rule.
```

- [ ] **Step 2: 2026-10-05 spec**

In `docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md`, in the Human decisions bullet that currently ends (lines 44-45)

```markdown
  `brew trust <tap>`. brewsoak keeps trusting its own `brewsoakr/soaked`
  staging tap only.
```

replace those two lines with

```markdown
  `brew trust <tap>`. brewsoak keeps trusting its own `brewsoakr/soaked`
  staging tap only. *Amended 2026-10-06 (issue #4): brewsoak runs
  `brew trust` for no tap at all. The staging tap was superseded by path
  installs from `<cache>/staging/` before 1.0.0 and its `tap-new`/`trust`
  code is removed; the name is recognized only so a receipt or `tap-info`
  entry left by a pre-release build is classified as staged.*
```

In the "Rulings confirmed by the maintainer (2026-10-06)" list (the `- **Rulings confirmed by the maintainer (2026-10-06):**` bullet, after the sub-bullet that begins `A dry run still runs the one no-soak`), add this sub-bullet:

```markdown
  - brewsoak has no staging tap (issue #4). `brewsoakr/soaked` was
    superseded by path installs before 1.0.0; the `brew tap-new` and
    `brew trust brewsoakr/soaked` code is removed, so brewsoak never runs
    `brew trust`. `origin::STAGING_TAP` and `TapClass::Staging` stay so a
    pre-release keg's receipt or tap entry is treated as staged, never as an
    origin.
```

In the tap classification table (line 192), replace

```markdown
| brewsoak's staging tap | Ignored |
```

with

```markdown
| `brewsoakr/soaked` (a leftover pre-1.0 staging tap) | Ignored |
```

Leave lines 160 ("not brewsoak's staging tap") and 358 (testing list) as they are: both describe the kept classification rule.

- [ ] **Step 3: 2026-08-12 spec**

In `docs/superpowers/specs/2026-08-12-brewsoakr-design.md`, replace the lock line (line 19)

```markdown
- `brew` is the only installer. brewsoak writes cutoff files into a local tap and invokes `brew`.
```

with

```markdown
- `brew` is the only installer. brewsoak writes cutoff files into a local tap and invokes `brew`. *Superseded 2026-08-13 (path installs; recorded 2026-10-06, issue #4): cutoff files are staged under `<cache>/staging/` and installed by path; brewsoak has no tap of its own.*
```

In the "Invoking brew" section, insert this paragraph between the `## Invoking brew` heading and the numbered list (step 1 `On first use: brew tap-new brewsoak/soaked --no-git.`):

```markdown
*Superseded 2026-08-13 (path installs; recorded 2026-10-06, issue #4). brewsoak creates no tap. In place of steps 1-4 below it stages the cutoff `.rb` under `<cache>/staging/` (`Formula/<name>.rb` or `Casks/<name>.rb`; tap packages under `staging/taps/<user>/<repo>/`), asks `brew deps --1 --formula <path>` for dependencies, and installs each dep and the target with `brew install --formula <path>`. Step 5 still applies. The original text is kept as the design record.*
```

Leave the numbered steps as written.

- [ ] **Step 4: Check for remaining live-tense mentions**

Run:

```bash
rg -n 'tap-new|brew trust brewsoak|trusting its own' /Users/efitz/Projects/brewsoakr/AGENTS.md /Users/efitz/Projects/brewsoakr/README.md /Users/efitz/Projects/brewsoakr/docs/superpowers/specs
```

Expected: only the lines written in Steps 2-3 (the 2026-10-05 amended bullet and ruling sub-bullet, and the 2026-08-12 historical step 1 under its supersession note); AGENTS.md should produce no hit. Any other hit describes the tap as live and needs the same treatment.

- [ ] **Step 5: Done gate**

Run: `cd /Users/efitz/Projects/brewsoakr && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`
Expected: unchanged from Task 1: fmt and clippy clean, `367 passed` lib, `1 passed` golden (368 total, same as `main`). Paste the `test result:` lines into the task report.

- [ ] **Step 6: Commit**

```bash
cd /Users/efitz/Projects/brewsoakr && git add AGENTS.md docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md docs/superpowers/specs/2026-08-12-brewsoakr-design.md && git commit -F - <<'EOF'
docs: brewsoak has no staging tap; brewsoakr/soaked is a leftover name

Record the issue #4 ruling in the 2026-10-05 spec, mark the 2026-08-12
local-tap design as superseded by path installs, and say in AGENTS.md
that the name is kept only to classify pre-release receipts as staged.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1kLwgjkPVEo4dku1MRWzy
EOF
```

---

## Final step (orchestrator)

- [ ] On `issue-4-remove-staging-tap`, run the done gate one more time and record its output: `cd /Users/efitz/Projects/brewsoakr && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test` (expected `367 passed` + `1 passed`).
- [ ] `git log --oneline main..issue-4-remove-staging-tap` shows exactly the two task commits. Squash them into one commit on `main` with the Task 1 message body (it already carries `Closes #4` and the trailer lines), then confirm `git log origin/main..main` before any push; pushing is an outward-facing action and waits for Eric.

## Self-review notes

- Spec coverage: issue criterion (1) is the Evidence section; criterion (2) is Task 1 (code) + Task 2 (docs) with the kept classification rule verified in Task 1 Step 6; criterion (3) is explicitly not applicable, with a stop condition in Task 1 Step 2 if the compiler finds a caller this plan missed.
- Type consistency: `brewsoak_brew_env_pairs()` and `apply_brewsoak_brew_env(&mut cmd)` are spelled the same in Steps 4, 5 and 6 of Task 1; the three deleted helpers are named identically in the Evidence table, Step 4 and Step 6.
- Review Focus: items 1, 2 and 4 are pinned by existing tests that Task 1's gate re-runs; item 3 gets its new test in Task 1 Step 5; item 5 is a docs item checked by Task 2 Step 4.
