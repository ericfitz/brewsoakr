# Older follow-ups: six carried-over bugs

Date: 2026-10-17. Diagnosis only; no code changed. Extends
[2026-10-05-tap-soak-and-no-soak-design.md](2026-10-05-tap-soak-and-no-soak-design.md). Line numbers: `fix/older-followups` at ab4644e.

Cross-cutting state facts (apply to every "State behavior" below): no
inter-process lock exists (`rg lock src` finds only in-process `Mutex` memos in
`brew.rs`), so two runs interleave freely. `state.toml` and `origins.toml` are
plain `std::fs::write` (`snapshot.rs:324`, `origin.rs:73`): a reader racing a
writer or a crash mid-write leaves a truncated file, and `load_state` then
fails every command (`snapshot.rs:244`) until the user deletes it;
`config.rs:325-390` has temp + fsync + rename to reuse (part of B4). A
cache-wide `flock` is the full answer to overlapping runs; Eric chose it (see
"Decisions"), implemented below. Every fix keeps git in `src/git.rs`, surfaces `Error::Git {
action, detail }`, and touches nothing under `$(brew --repository)/Library/Taps`.
## B1. No-soak `outdated` ignores `--greedy`

Problem: `outdated --greedy` never lists a no-soak self-updating cask that
`upgrade --greedy` would upgrade.
Evidence: `cmd.rs:457` reads `greedy`; the no-soak list at `cmd.rs:613-622`
calls `brew.outdated_names()` with no arguments, and `ProcessBrew`
(`brew.rs:571`) runs `brew outdated --json=v2` bare, though `brew outdated`
accepts all three greedy flags and `nosoak::run_step` passes `--greedy` to
`brew upgrade`. Scenario: `NO_SOAK = ["homebrew/cask"]`, cask `alt-tab`
(`auto_updates true`) behind: `outdated --greedy` prints `(none)` under
`==> No-soak (brew)`, `upgrade --greedy` upgrades it.

Fix: `Brew::outdated_names(&self, greedy: flags::Greedy)` (`brew.rs:72`; also
the two test wrappers at `cmd.rs:3827,3910`); `ProcessBrew` appends brew's
spelling of the mode; `cmd::outdated` passes `greedy` (`-g` is already
expanded by `flags::greedy_mode`, `flags.rs:248`).
State behavior: read-only; none.

Test plan: `MockBrew::outdated_names` records its argument (a
`Mutex<Vec<Greedy>>` field, like `compares`); extend
`outdated_lists_no_soak_from_brews_view_without_updating` (`cmd.rs:9190`) with
`--greedy` and assert the recorded mode; a `ProcessBrew` args test for
`outdated --json=v2 --greedy`.
Decision needed: none.
## B2. `git::missing_object` treats every exit 128 as "not there"

Problem: a wiped, corrupt or unreadable clone is reported as per-package soak
status, never as a git error.
Evidence: `git.rs:162-165` is true for `code == 128` or stderr `does not
exist`; `show`, `rev_parse`, `log_sha_before` map it to `Ok(None)` (`git.rs:222,
236, 301`). Probe (`LC_ALL=C`): missing path and missing commit (both `fatal:
path 'x' does not exist in '<sha>'`), missing ref, `not a git repository` and
`objects/` mode 000 all exit 128. With no cutoff and no head blob,
`resolve_view_unordered` (`cmd.rs:2452-2456`) reaches `eligibility.rs:67-69`
as `RefuseYanked`: after `rm -rf core.git`, `outdated` lists every core formula
as "missing at HEAD (yanked)" and `upgrade` refuses them all, exit 1, with no
hint about the cache. `rev_list_before` does it right (`git.rs:394-406`).
Fix: `missing_object` matches stderr only (`does not exist in`, `unknown
revision`, `bad revision`, `ambiguous argument`, `Not a valid object name`);
any other failure is `Error::Git`. Because an absent commit gives the same
"path does not exist" text, `show` then verifies the commit once per `(dir,
sha)` (`rev-parse --verify --quiet <sha>^{commit}`, memoized beside `trees`)
and returns `Error::Git` with detail `commit <sha> is not in the soak store;
run brewsoak update` when it is missing.

State behavior: read-only; a run whose clone another run is re-fetching now
fails loudly instead of misclassifying. Second run: same result. Overlapping
runs: the run lock serializes them. Failed-run leftovers: a wiped or corrupt
clone is `Error::Git`; the next `update` re-inits it (tests:
`show_on_a_non_repo_is_error_git`, `show_of_an_unknown_commit_names_the_sha_and_the_fix`). Test plan: real git in a tempdir as in `rev_list_before_on_a_non_repo_is_error_git`
(`git.rs:1007`): non-repo `show`/`rev_parse` -> `Err(Error::Git)`; unknown sha
-> `Err` naming it; missing path on a real commit -> `Ok(None)`. Tighten
`process_git_show_spawns_git_once_per_blob` (`git.rs:1048`) to `Err`.
Decision needed: none.
## B3. Stale `refs/brewsoak/cutoff`

Problem as noted: a cutoff pin left at an old commit. Not a correctness bug
in that direction; the real hazard is the inverse.
Evidence: nothing reads `REF_CUTOFF` outside tests (`snapshot.rs:216,342`
write it; `cmd.rs:2512` says only the state's `cutoff_sha` is ever the cutoff;
`resolve_pkg_blobs` passes sha strings, `cmd.rs:2531,2547`). The pin only
anchors objects through `gc_prune` (`snapshot.rs:344`, core/cask only).
(a) `refresh_tap` leaves a tap's pin at the old cutoff when the tap has no
cutoff this run (`snapshot.rs:214-219`): cosmetic, taps are never gc'd.
(b) `refresh_core_tap` moves core's pins and runs `gc_prune` (`snapshot.rs:342-344`)
before `refresh_with` writes `state.toml` (`snapshot.rs:167,194`). If the cask
refresh then fails, `state.toml` keeps a core cutoff sha whose objects were
just pruned; the next `outdated` (reuses state, `cmd.rs:122-135`) hits B2 and
refuses every core formula (after B2: an `Error::Git` naming the commit, a
stranded state until the next successful `update`).

Fix: (b) move `gc_prune` out of `refresh_core_tap` to after `write_state` in
`refresh_with`: a failed cask fetch then leaves the old cutoff objects
readable until the next successful refresh gc's them, with no partial writes
(and no "previous state" problem on a first run). (a) optional:
`GitStore::delete_ref`, called when `rev_list_before` is `None`.

State behavior: second run converges (fetches are idempotent, pins force).
Overlapping runs: serialized by the run lock, so one run's `gc_prune` can no
longer drop the other's cutoff. Failed-run leftovers: a run that dies after the
pin move but before `write_state` leaves `state.toml` at the old cutoff, whose
objects were not pruned, so it is consistent; the next successful run records
the new cutoff and then gc's (tests:
`failed_cask_refresh_leaves_the_old_core_cutoff_readable`,
`gc_runs_only_after_state_is_written`). `gc_prune` now runs for core and cask
at the end of `refresh_with`; a gc failure there is an `Error::Git` after
`state.toml` is already written. (a) is skipped: cosmetic.
Test plan: `snapshot.rs`, `InMemoryGit` plus a `StaticGithub` wrapper whose
cask `head_sha` fails (like `FailShowGit`, `cmd.rs:5355`): `refresh_with` is
`Err`, `load_state` shows the new core cutoff. Give `InMemoryGit` a `gc_prune`
that drops unreferenced blobs so the hazard is visible.
Decision needed: none ((a) may be skipped; say so in the PR).
## B4. Tap fetch failure deletes the last good state

Problem: one failed fetch erases the tap's `[taps."x"]` table from
`state.toml`; the "not persisted, next run tries again" comment at
`snapshot.rs:42-44` describes `held_taps`, but the deletion is persisted.
Evidence: `refresh_taps` on `Error::Git` does `snaps.taps.remove(&name)` then
`write_state` (`snapshot.rs:187-194`). Scenario: `brewsoak upgrade` offline
holds `hashicorp/tap` (correct) but drops its table, so the next `outdated`
(still offline) refetches (`tap_needs_refresh`, `snapshot.rs:229-234`), fails
and holds again, where core/cask would have used the stored snapshot (core
keeps its state because `refresh_with` returns before writing).
Fix: two parts, because the force path (`update`, `upgrade`, `install`,
`reinstall`) is worse than the scenario above: `refresh_with` starts from
`taps: BTreeMap::new()` (`snapshot.rs:158-167`) without `load_state`, so there
is nothing to keep. (1) `refresh_with` seeds `snaps.taps` from
`load_state(cache)` with the entries of the *planned* taps only (unplanned
tables keep dropping out, see B6). (2) `refresh_taps` on `Error::Git` leaves
the seeded `TapState` in place, records `held_taps`, writes state. Every
consumer checks `held_taps` before `taps`
(`cmd.rs:520,743,894,1466,2114,2174,2212`); add that check to
`snapshotted_pkgs` (`cmd.rs:154-159`) so `update`'s summary and
`prefetch_installed` skip a held tap. Temp + rename for both cache files.

State behavior: second run: same state, held again or refreshed (a
successful fetch replaces the kept table wholesale). Overlapping runs: the run
lock serializes them; writes are also temp + rename, so a reader never sees a
torn file. Failed-run leftovers: a stale temp next to `state.toml` or
`origins.toml` (older than ten minutes) is removed on the next write
(`config::remove_stale_temps`); an unreadable `state.toml` seeds nothing and is
replaced by the next forced refresh (tests:
`failed_tap_fetch_keeps_the_last_good_table_until_a_fetch_succeeds`,
`refresh_with_seeds_only_planned_taps_from_the_stored_state`,
`a_corrupt_state_file_does_not_block_a_forced_refresh`,
`write_state_leaves_no_temp_file_and_replaces_wholesale`).
Test plan: extend `one_failing_tap_is_held_and_others_continue`
(`snapshot.rs:597`): `refresh_with` once, `fail_remote`, `refresh_with` again;
`load_state` keeps the table, `held_taps` has the note. In `cmd.rs`,
`tap_snaps` + `held_taps.insert` (`cmd.rs:4013`); `classify_installed` skips it.

Decision needed: with the table kept, the next `outdated`/`info` uses the
last good snapshot without a network call. A: accept (core/cask already work
this way). B: persist `failed_at` and retry in `tap_needs_refresh`. Recommend
A: mutating commands force-refresh (`lib.rs:313,344`), so a stale snapshot is
only reported, never installed from.
## B5. Bare dep not searched across taps

Problem: a tap formula's `depends_on "foo"` where `foo` lives in a third tap
is left to brew, which installs it unsoaked from that tap's HEAD.
Evidence: `dep_origin` (`cmd.rs:2194-2224`) tries the dependent's tap, core,
cask, then `None`, and the caller `continue`s (`cmd.rs:2095-2097`). Probe with
a throwaway formula: `brew deps --1 --formula <path>` prints `cats`,
`terraform`; with `--full-name`, `endava/tap/cats`, `hashicorp/tap/terraform`,
and `wget` stays bare. Homebrew (`formulary.rb:791-835`, `FromNameLoader`)
tries core first, then every installed non-core tap, and raises
`TapFormulaAmbiguityError` on two matches: brewsoak diverges twice (no
cross-tap search; dependent's tap before core, the 2026-10-05 lock). Also
`cmd.rs:2095` runs before the installed check at `2101`, so an installed dep
from a third tap never gets upgrade-dependency-first.

Fix (A, recommended): `ProcessBrew::deps` adds `--full-name` (`brew.rs:585`);
`inventory::parse_token` already reads `user/repo/name`, and `dep_origin`
keeps its bare path only as a fallback. A `TapFormulaAmbiguityError` exit from
`deps` holds the target with brew's message as the note instead of
`Error::Brew`. Move the `inv.find` check ahead of origin resolution.
State behavior: read-only resolution; install order unchanged.
Test plan: `MockBrew.deps` keyed by the staged path returns `endava/tap/cats`
for a `hashicorp/tap` dependent, `tap_snaps` (`cmd.rs:2849`) for both taps (see
`no_soak_and_unsoakable_deps_are_left_to_brew`, `cmd.rs:3477`): the dep is
staged from `endava/tap`'s cutoff and installed first. Second test: dep's tap
has no snapshot -> target held (`cmd.rs:2125-2131`). `ProcessBrew` args test.

Decision needed: A: adopt brew's resolution via `--full-name` (dissolves the
"dependent's tap first" lock; brew's real install resolves the same way, so
the lock never matched what brew installed). B: keep the lock and extend
`dep_origin` to search `snaps.taps`, holding on two matches. Recommend A.
Either way a dep in a tap with no installed package has no snapshot this run
and is held; lazy `refresh_tap` in the walk is a follow-up.
## B6. No pruning on untap

Problem: untapping leaves `[taps."x"]` in `state.toml`, `<cache>/taps/x.git`
and `<cache>/staging/taps/x/` forever; `daveshanley/vacuum` was cleaned by hand
on 2026-10-16 and `staging/taps/daveshanley/vacuum/Casks` is still on disk.
Evidence: no removal code exists (`rg "remove_dir|prune" src` finds only
config backups and `gc_prune`). The `state.toml` table is the one thing that
does not linger: `refresh_with` rebuilds `taps` from the plan
(`snapshot.rs:158-167`, pinned by `ensure_snapshots_refreshes_only_needed_taps`,
`lib.rs:735`), so the table survives only until the next forced refresh. What
lingers: the clone, the staging dir, and `origins.toml` records of uninstalled
packages (cosmetic: a receipt tap or core staging overrides them). A keg still
installed from the untapped tap is correctly reported `tap x is not installed;
run brew tap x first...` (`cmd.rs:366`) because its record survives.
Fix: after a forced refresh, remove clone dirs and staging dirs under
`<cache>` whose tap is not in `inv.taps`, and `origins.toml` records whose
package is not installed (`inv.find_in`). Never drop a record because its tap
is gone: `resolve_origin` would fall back to core (`origin.rs:136-139`) and
brewsoak could soak-upgrade the keg from a same-named core formula. Report
removals under `-v`. B4's seeding copies planned taps only, so tables keep
pruning themselves.

State behavior: second run: nothing to prune, no writes. Overlapping runs:
deleting a clone another run reads gives it `Error::Git` (after B2). Failed-run
leftovers: a half-removed clone goes next run; `refresh_tap` re-inits one.
Test plan: `TestWorld` (`lib.rs:514`), cache seeded with `[taps."gone/tap"]`,
`taps/gone/tap.git/`, `staging/taps/gone/tap/`, `MockBrew.taps` without it;
`dispatch(["update"])`; all three gone, a live tap's kept. Origins: an
uninstalled package's record removed, an installed untapped keg's kept.

Decision needed: A: automatic on `update` and every forced refresh. B: an
explicit `brewsoak clean` subcommand. Recommend A: everything pruned is
regenerable cache, and the untapped case then needs no user action.

## Decisions (made by Eric, 2026-10-17)

- **B4: option A.** Keep a tap's last good snapshot when its fetch fails.
  `outdated` and `info` use it offline, as they already do for core and
  cask. Mutating commands always force a refresh, so a stale snapshot is
  reported but never installed from. No `failed_at` retry marker.
- **B5: option A.** brewsoak calls `brew deps --full-name` and lets brew
  resolve bare dependency names: core first, then a unique installed tap.
  If brew reports the name is ambiguous (`TapFormulaAmbiguityError`), the
  target is held. This replaces the 2026-10-05 "dependent's tap first"
  rule (docs/superpowers/specs/2026-10-05-tap-soak-and-no-soak-design.md).
- **B6: option A.** Prune automatically after every forced refresh:
  - clone and staging directories of taps that are no longer tapped;
  - origin records of packages that are no longer installed.
  A record is never pruned just because its tap is gone. `-v` lists each
  removal.
- **Run lock: option A.** Mutating commands take an exclusive `flock` on
  the cache. A second run waits briefly, then fails with a clear message
  that names the other run (pid and command). `state.toml` and
  `origins.toml` are also written atomically (temp file + rename).

### Run lock and atomic writes: implementation

`src/lock.rs`: `std::fs::File::try_lock` (stable since Rust 1.89; `Cargo.toml`
now declares `rust-version = "1.89"`; no new dependency) on `<cache>/run.lock`,
retried every 100 ms for 5 s, then `Error::Refusal` naming the holder's pid and
command from `<cache>/run.lock.holder` (written whole by temp + rename after
acquiring, removed on release, so a waiter never reads a partial record). `dispatch` takes it for
`update`, `outdated`, `info`, `upgrade`, `install` and `reinstall`, and holds it to the end of the run. `outdated` and `info` lock because
they refresh and write `state.toml` and the clones when the stored snapshot is
missing or a tap is stale; `version`, `help`, `settings` and
passthrough do not touch the cache (a settings edit writes only the config
file, atomically, last writer wins; the existing contract is that settings
never creates the cache). State behavior: second run takes the lock
after the first exits; overlapping runs wait 5 s, then fail with the message;
a run that died leaves `run.lock.holder` with a dead pid, but the kernel dropped
its flock, so the next run is not blocked and replaces the record. The lock file
is never deleted (removal would race a waiter). `src/atomic.rs` writes
`state.toml` and `origins.toml` through a `0600` temp file, fsync, rename and a
directory fsync.

### Implementation notes (B1, B3a, B5, B6, lock scope)

- **B1.** `Brew::outdated_names(greedy)`; `ProcessBrew` appends `--greedy`
  (both kinds), `--greedy-auto-updates` or `--greedy-latest`, the same mapping
  `flags::greedy_mode` reads. `cmd::outdated` passes its mode.
- **B3 (a).** `GitStore::delete_ref` (`git update-ref -d`, idempotent);
  `refresh_tap` deletes `refs/brewsoak/cutoff` when the tap has no cutoff.
  A failure is `Error::Git`.
- **B5.** `ProcessBrew::deps` runs `brew deps --1 --full-name`. Probed against
  Homebrew 5.x: the flag is accepted with `--cask` (cask `depends_on formula:`
  deps print as before, e.g. `mactex-no-gui` -> `ghostscript`), so one spelling
  serves both kinds. `CutoffDepWalk::dep_origin` no longer searches the
  dependent's tap: a full name is that tap, a bare name is core then cask. The
  installed-dependency check now runs before origin resolution. brew's
  `Formulae found in multiple taps` / `TapFormulaAmbiguityError` on the `deps`
  call holds the target (`taps::ambiguous_formula`, `refuse_ambiguous_dep`).
  A dep in a tap with no soak snapshot is still held. State behavior:
  read-only; a second run resolves the same way.
- **B6.** `prune::prune_cache` runs after every refresh that rewrites the
  snapshots (`update`; `ensure_snapshots` on a forced or first-time refresh),
  under the run lock. It removes `<cache>/taps/<user>/<repo>.git` and
  `<cache>/staging/taps/<user>/<repo>/` of taps not in `inv.taps`, and
  `origins.toml` records whose package is not installed (a record is kept while
  its package is installed, whatever its tap). `-v` prints each removal. A
  failed removal is a warning line, not an error. State behavior: second run
  finds nothing and writes nothing; two runs are serialized by the lock; a
  half-pruned run is finished next time.
- **Lock scope (decision by Claude, 2026-10-17; refines "Run lock: option A").**
  `update`, `upgrade`, `install` and `reinstall` lock up front. `outdated` and
  `info` start without the lock and `cmd::ensure_snapshots` takes it (through
  the dispatch's `lock::LockSlot`, so one process never locks twice) at the
  moment it finds the stored snapshot missing, unreadable or stale. After
  acquiring it reads `state.toml` again and decides again, because another run
  may have refreshed while this one waited; a fresh snapshot is then used
  without a write. The decision and the acquisition are one step, so no
  refresh can run on a state read taken before the lock. Everything that
  writes `state.toml`, the clones, gc or prune (`snapshot::refresh_with`,
  `refresh_taps`, `prune::prune_cache`, `cmd::update`) takes a `&RunLock`, so
  the compiler rejects an unlocked call. A run that only reads is safe without
  the lock because `state.toml` and `origins.toml` are written atomically; a
  reader racing a prune or gc sees `Error::Git` (B2), never a torn file.
