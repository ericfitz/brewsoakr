# Follow-ups #19, #22, #24, #25, #26 and #27

Date: 2026-11-28
Issues: #19 (stopgap review follow-ups), #22 (untrusted third-party formulae missing from the
inventory), #24 (`file_version`), #25 (prune racing plain brew), #26 (kind flags on a bare run),
#27 (same version shown as `X < X`)
Extends: [2026-11-27-local-tap-install-design.md](2026-11-27-local-tap-install-design.md), which
stays the design authority. Its human decisions are frozen and none is amended here.
Plan: [../plans/2026-11-28-follow-ups-19-27.md](../plans/2026-11-28-follow-ups-19-27.md)

## Who decided what

- **No human decision was made for this document.** Eric asked for the six issues to be fixed
  autonomously.
- **Controller ruling**: a design call made by the controlling session on 2026-11-28. Each is quoted
  where it applies.
- **Architect's call**: a call made while writing this spec, either because a ruling left the choice
  open or because code or live brew disproved a ruling's premise. There are five corrections of a
  ruling, listed under "Corrections to the rulings". They can be reversed by Eric at the cost named
  there.

Human decisions this work relies on and leaves as they are (2026-11-27 spec): Human decision 2
(brewsoak never runs `brew trust` or `brew untrust`), #23 Decision 1 (the user's kind flags are
dropped from per-package commands), #23 Decision 3 (`soaktap::file_version` keeps brewsoak's own
parser), #23 Decision 7 (the README warns about a concurrent plain brew; the code fix is #25).

Everything below was checked on Homebrew 7.0.17-27-g96c963e, macOS arm64, on 2026-11-28, with
read-only brew commands and by reading `/opt/homebrew/Library/Homebrew`. Nothing was installed,
trusted or untrusted. Facts are numbered `F1`... for reference from the plan.

## Corrections to the rulings

| # | Ruling | What disproved it | Corrected call |
|---|---|---|---|
| 1 | #24: a tap file is current when a keg's receipt names the soak tap and its `.brew/<name>.rb` is **byte-equal** to the tap file | F8: the one keg installed from the soak tap on this machine (`ast-grep` 0.51.3, a bottle pour) has a `.brew/ast-grep.rb` that differs from the tap file: it has no `bottle do` block. F9: a cask keeps `Casks/<token>.json`, not a `.rb` | Compare **versions read by brewsoak's own parser on both files**, not bytes; casks keep the receipt's `source.version`. See "#24" |
| 2 | #25: take brew's per-formula/**cask** lock | F13: brew takes no cask lock while it installs or upgrades a cask, and has no global install lock | Formulae use brew's formula lock. Casks get a fresh receipt read just before the removal and nothing more. See "#25" |
| 3 | #22: decide trust from `brew trust --json=v1` | Human decision 2 and AGENTS.md: brewsoak never runs `brew trust`. F5: a per-formula trust entry makes brew list the formula, so a formula brew leaves out has no entry that works | Decide from `tap-info`'s `trusted`, which brewsoak already has. No `brew trust` child, no extra brew call. See "#22" |
| 4 | #22: read each hidden keg's tap from its receipt; premise that `cyclonedx-gomod` is listed because of its per-formula trust entry | F6: its receipt has `source.tap: null` and it is listed because core has a formula of that name. F17: a keg with a null receipt tap and no core namesake is hidden when its name resolves into an untrusted tap | A keg with no receipt tap gets its tap from `source.path` (`origin::staged_tap`), then from its `origins.toml` record. See "#22" |
| 5 | #19: "fix each item that still applies" covers item 3 | The 2026-11-27 dependency table says "as today" for it, and a test pins the old behavior | Fixed, as ruled, and recorded as an amendment of that table row (a Claude design row, not a human decision). See "#19, item 3" |

## #26: `--formula` and `--cask` on a bare run

**Controller ruling.** On a bare `brewsoak upgrade` the flag restricts the run to that kind, as
`brew upgrade --cask` does. Both flags together is a usage error unless brew accepts both. The flags
stay stripped from the per-package commands (#23 Decision 1). Cover `outdated` and every other
bare-run command that takes the flags.

**Facts.**

- F1. `brew outdated --formula --cask` prints its usage and `Error: Invalid usage: Options --formula
  and --cask are mutually exclusive.`, exit 1 (run 2026-11-28).
- F2. Source: `cmd/outdated.rb:52` and `cmd/info.rb:97` declare `conflicts "--formula", "--cask"`.
  `cmd/upgrade.rb:92-127`, `cmd/install.rb:57-137` and `cmd/reinstall.rb:54-83` declare
  `conflicts "--cask", "--formulae"` through their option loops; `cli/parser.rb:872-879`
  (`set_switch`) sets every alias of a switch, so that conflict also fires for `--formula`. brew
  rejects the pair on all five commands. Only `outdated` was run; the other four are source facts.
- F3. `brew upgrade` with no names and `--formula` upgrades only outdated formulae
  (`cmd/upgrade.rb:93-94`, the option's own description); `--cask` likewise (`:132-133`).
- F4. `src/cli.rs:588-589` reads only the spellings `--cask` and `--formula`. `--casks` and
  `--formulae` are stripped from brew's command (`tap::is_kind_flag`, `src/tap.rs:231`) but do not
  choose the kind of a typed name. Same pattern, fixed here.

**Behavior.**

- `flags::kind_filter(flags)` reads the four spellings: `--formula`/`--formulae` is formulae,
  `--cask`/`--casks` is casks, neither is no filter, and one of each is
  `Error::Usage("--formula and --cask are mutually exclusive")`.
- *Architect's call:* the usage error is raised while parsing, for `upgrade`, `install`,
  `reinstall`, `outdated` and `info`, with or without names, because brew rejects the pair on all
  five (F2). Before, `brewsoak install --cask --formula x` silently chose the cask. Cost if wrong:
  move the check into the bare paths only.
- Bare `upgrade`: only installed packages of that kind are evaluated, counted in the plan
  (`[i/N]` and `(N of M)`, where `M` is the number of installed packages of that kind) and handed
  to the no-soak step. The `-v` banner says `upgrading installed formulae` or
  `upgrading installed casks`.
- `outdated`: only packages of that kind are evaluated and listed in every section.
- Bare `info`: only packages of that kind are listed.
- `install`: the kind of a typed name is chosen by any of the four spellings (F4).
- Unchanged: the flags are still not forwarded to per-package commands; a single-kind no-soak run
  still forwards the user's flag (#23 Decision 1); named `upgrade`, `reinstall` and `info` still
  find a typed name's kind as they do today; `update` takes no kind flag.
- The filter is applied where packages are walked, never to the `Inventory`. Tap maintenance
  (recovery and pruning) always sees every installed package, so `upgrade --cask` cannot prune the
  tap file of an installed formula.

**Limit.** Bare `info` resolves each listed name as it does today, formula first. When an installed
formula and an installed cask share a name, `info --cask` prints the formula's line for it. That
is how bare `info` already treats such a pair.

## #27: the same version shown as `X < X`

**Controller ruling.** When the installed and cutoff versions are equal but brewsoak plans an
install because the identity differs, label it in `outdated`, `info` and the upgrade line. The
architect chooses the wording. Other output keeps its format.

**Facts.**

- F7. `liblinear` on this machine: keg `2.50`, receipt tap `homebrew/core`; the keg's formula copy
  has `url ".../liblinear-2.50.tar.gz"` and `sha256 "e5eeafe2..."`. `brew cat liblinear` now has
  `url ".../v250.tar.gz"`, `version "2.50"`, `sha256 "c875bab4..."` and `compatibility_version 1`.
  A formula's identity is version, revision, source `sha256` and, when both sides have one, the
  bottle rebuild (`src/identity.rs:165-175`); a keg's copy has no bottle block, so for an installed
  formula at the same version and revision the only difference brewsoak can see is the source
  checksum. A cask compares `sha256` and `url` (`:176-180`).

**Wording** (architect's call). The label is `source changed`. It applies when the action is
`InstallCutoff`, the package is installed, and `report::identity_version` of the installed and
cutoff identities are the same string, except for a `:latest` cask, which keeps its
`definition changed` wording.

| Where | Before | After |
|---|---|---|
| `outdated`, Outdated and Auto-updates sections | `liblinear (2.50) < 2.50` | `liblinear 2.50 (source changed)` |
| `info`, compact | `liblinear  2.50  would upgrade to 2.50` | `liblinear  2.50  would upgrade (source changed)` |
| `info`, long form | `action: would upgrade` | `action: would upgrade (source changed)` |
| upgrade line (`upgrading`, `installing`, `reinstalling`) | `[3/9] upgrading liblinear 2.50` | `[3/9] upgrading liblinear 2.50 (source changed)` |
| dependency line of a named run | `upgrading libx 1.0 -> 1.0 (dependency of wget)` | `upgrading libx 1.0 (source changed; dependency of wget)` |
| `-v` line | `installing cutoff 2.50; installed 2.50 is behind soak; ...` | `installing cutoff 2.50; installed 2.50 has the same version and a changed source; ...` |

A package that is not installed, and every line whose versions differ, prints as before.

**Limit, not changed here.** brew answers `already installed` when the tap file has the installed
version (2026-11-27 Fact 17), so for a formula the run then reports
`<name>: brew left the installed version in place: ...` and counts it as skipped, on every run until
the version moves. The label says why brewsoak tries; it does not make brew reinstall. Whether
brewsoak should run `brew reinstall` for a same-version source change is a separate design question
(issue #30, see "Open item").

## #24: when a tap file belongs to an installed package

**Controller ruling.** Option B, no extra brew calls: a tap file counts as current when a keg's
receipt names the soak tap and that keg's `.brew/<name>.rb` is byte-equal to the tap file. Apply it
wherever `file_version` decides keep or put-back. Keep `file_version` as the first check only where
it is still right. Check the #28 code for consistency.

**Facts.**

- F8. `/opt/homebrew/Cellar/ast-grep/0.51.3/INSTALL_RECEIPT.json` has `source.tap`
  `ericfitz/brewsoak`, `poured_from_bottle: true`. Its `.brew/ast-grep.rb` (SHA-256 `195c75bc...`)
  differs from the tap's `Formula/ast-grep.rb` (`b3c32fcf...`): `diff` shows only the eight-line
  `bottle do ... end` block missing from the keg's copy. A bottle carries the copy made when it was
  built; a source build writes the file with its bottle block removed
  (`formula_installer.rb:641-644`). 2026-11-27 Fact 26 had already recorded "differs after a bottle
  pour". A later edit of the core file that does not rebuild the bottle (a homepage, a livecheck)
  would add more differences, so "equal apart from the bottle block" is not safe either.
- F9. A cask keeps no copy of its `.rb`: `Caskroom/alt-tab/.metadata/7.38.1/<time>/Casks/alt-tab.json`
  (`cask/installer.rb:619-623` writes `.rb` only for a cask with uninstall flight blocks). Its
  receipt `Caskroom/<token>/.metadata/INSTALL_RECEIPT.json` has `source.tap` and `source.version`.
- F10. A `version :latest` cask's receipt says `source.version: "latest"` (`font-open-sans`,
  `font-roboto`), while brewsoak's identity version for it is `:latest`
  (`brew::version_only_cask_receipt`, `src/brew.rs:1703`). `receipt_tap(Cask, name, ":latest")`
  therefore never matches. Same pattern as #24, on the cask side; fixed here.
- F11. Where `file_version` decides today:
  - `soaktap::settle_entry` (`src/soaktap.rs:250-260`): `receipt_tap(kind, name, v)` with `v` the
    recorded version, else `file_version` of the tap file. Recovery always takes the second form.
  - `ApplySession::keg_from_soak_tap_at` (`src/cmd/apply.rs:1187`), used by `dep_at_cutoff`.
  - `gate_installed_dep` (`src/cmd/deps.rs:519-527`) and `dep_at_cutoff` (`apply.rs:1212-1236`)
    compare `file_version` of the keg's copy with `file_version` of the cutoff: the same parser on
    both sides, so the comparison is right whatever brew named the keg.
  - `soaktap::anchored` (`soaktap.rs:144`): the same parser on both sides. Right as it is.
  - The #28 check in `record_gate_install` (`apply.rs:1465-1476`) reads
    `installed_receipt_taps(kind, name)`: any keg, any version.
  The wrong step is the lookup `<cellar>/<name>/<v>/INSTALL_RECEIPT.json` with `v` from brewsoak's
  parser: it misses when brew named the keg directory differently.

**The rule** (architect's call, correcting the ruling). `soaktap::from_tap_at(brew, kind, name,
version, file)` answers "is this package installed from the soak tap at this file's version":

1. *First check, kept because a hit is always right:* take `version` when given, else
   `file_version(kind, file)`; when `brew.receipt_tap(kind, name, v)` names the soak tap, yes. For a
   cask, brewsoak's `:latest` is asked as `latest` (F10).
2. *Second check, formulae only:* for each keg of `name` whose receipt names the soak tap, whatever
   its directory is called, read its `.brew/<name>.rb`; when its `file_version` equals
   `file_version(kind, file)` and neither is `None`, yes.
3. Otherwise no. A keg copy that is missing or does not parse never matches.

This is option B with "same version by brewsoak's parser" in place of "same bytes". It costs no brew
call: it reads receipts and keg copies from disk, as the inventory already does. It does not depend
on what brew names a keg, and #23 Decision 3 (the parser stays) holds. The second check compares the
version only, not the checksum, so the existing rule "when the tap file has the installed version,
an anchored package keeps the new file" is unchanged.

**Where it is used.**

| Site | Change |
|---|---|
| `settle_entry` (settle and recovery) | keep the file when `from_tap_at(.., version, tap file bytes)` |
| `keg_from_soak_tap_at`, and through it `dep_at_cutoff` | `from_tap_at(.., None, cutoff bytes)` |
| `record_gate_install` (#28) | `from_tap_at(.., installing version, cutoff bytes)` replaces the version-free receipt read |
| `gate_installed_dep`, the second half of `dep_at_cutoff`, `soaktap::anchored` | none (F11); the WriteCutoff file is now kept because settle uses the rule above |

**The #28 code.** It is not consistent with settle today: `record_gate_install` accepts any keg
whose receipt names the tap, while settle wants one at the file's version. With an older anchored
keg and a brew that exits 0 without a no-op line and without a new keg, the run says "upgraded" and
settle puts the file back. Using one predicate at both sites removes the difference. The fallback
reason therefore changes from `its receipt does not name brewsoak's tap` to
`no keg of it is from brewsoak's tap at the cutoff version`.

**Re-running.**

- *A second run.* Nothing is staged for a package the first run finished, so nothing is settled.
  The rule only reads.
- *Two runs at once.* Settle, recovery and every tap write still take the `&RunLock`. A plain brew
  that replaces a keg while brewsoak reads its receipt can make the answer "no"; the file is then
  put back, which is the state before the run.
- *Leftovers from a failed run.* Recovery uses the same rule with no recorded version. A run that
  died after brew poured a keg whose directory name is not brewsoak's parsed version used to put the
  old file back beside the new soak-tap keg (issue #24); it now keeps the new file. A keg whose copy
  cannot be read still gets the old file back, as before.

## #25: pruning while a plain brew runs

**Controller ruling.** Before removing a tap file, try to take brew's own per-formula/cask lock
without blocking and hold it while removing. If it is busy, skip the file and say so under
`--verbose`. If no per-package lock exists, fall back to skipping all pruning while brew holds its
update or install lock. Use an flock already in the dependencies.

**Facts.**

- F12. brew's per-formula lock is `FormulaLock` (`lock_file/formula_lock.rb`): the file
  `HOMEBREW_PREFIX/var/homebrew/locks/<name>.formula.lock` (`lock_file.rb:25`,
  `startup/config.rb:55`), opened `RDWR | CREAT | NOFOLLOW` and locked with
  `flock(LOCK_EX | LOCK_NB)` (`lock_file.rb:39-46`); after locking brew checks that the path still
  exists with the same inode and retries otherwise (`:58-61`). `FormulaInstaller#install` takes it
  for the formula and its recursive dependencies (`formula_installer.rb:561`, `:2049-2061`) and
  releases it in `finish` or on any exception (`:659`, `:1165`). `Keg#lock` takes it for link,
  unlink and uninstall (`keg.rb:420-430`). `brew cleanup` removes lock files nobody holds
  (`cleanup.rb:775-791`). 133 such files exist on this machine.
- F13. `CaskLock` is constructed in one place, `cleanup.rb:572`, to name a file for deletion. No
  cask install, upgrade or uninstall takes it. brew has no global install lock; `lock update`
  (`cmd/update.sh:692`) covers `brew update` only, and `brew update` skips the soak tap (2026-11-27
  Source fact 3).
- F14. While brew pours a bottle, the new keg has no receipt naming the real tap yet: brew loads the
  bottle's tab (from the bottle manifest or the keg's own file, `utils/bottles.rb:146-168`), fills in
  the tap and writes it (`formula_installer.rb:1782-1801`), all inside the lock (F12). A brewsoak
  inventory read in that window can see a keg that does not name the soak tap. Read in the source,
  not observed.
- F15. `Cargo.toml` has neither `fs4` nor `libc`. The run lock already uses `std::fs::File::try_lock`
  (`src/lock.rs:104`, Rust 1.89), which the standard library documents as `flock` with
  `LOCK_EX | LOCK_NB` on Unix: the call brew makes. No crate is added. That the two exclude each
  other was not run; the plan's manual check 4 does.
- F16. `soaktap::prune` (`src/soaktap.rs:493-523`) decides from the inventory taken at the start of
  the run and from the undo directory. It never reads a receipt again.

**What the lock closes.** The inventory can be stale in two ways: brew is in the middle of an
install (F14), or brew finished one between the inventory read and the prune. Taking the lock fails
in the first case. In the second it succeeds, so the receipts are read again from disk while the
lock is held; a receipt that names the soak tap keeps the file. No mechanism is claimed for the
overlap seen on 2026-11-27; this closes the two windows the source shows.

**Behavior.** For each `.rb` under the tap's `Formula` that the existing test would remove:

1. Take `<locks>/<name>.formula.lock` without blocking, creating the directory and the file as brew
   does. A symbolic link at that path is refused. After locking, check the inode as brew does.
2. Busy: leave the file. With `--verbose`: `not pruning <path>: brew holds the lock for <name>`.
3. The lock cannot be taken for another reason (no lock directory, an I/O error): leave the file and
   always print `brewsoak: warning: not pruning <path>: could not take brew's lock for <name>
   (<why>)`.
4. Locked: read the receipts of every keg of `<name>` from disk. If one names the soak tap, leave
   the file (`--verbose`: `not pruning <path>: <name> is installed from brewsoak's tap`). Otherwise
   remove the file. Release the lock. The lock file itself is left, as brew leaves it.

*Architect's call, casks (correction 2).* For each `.rb` under `Casks` that the existing test would
remove, read the cask's receipt from disk again and keep the file when it names the soak tap; then
remove without a lock, because brew has none to take (F13) and no global lock to test. The window
left is the time between that read and the unlink. If a plain brew finishes installing that cask
from the soak tap inside it, the cask keeps its receipt and loses its file: brew then describes it
from its installed copy and never calls it outdated (2026-11-27 Fact 31), and brewsoak's next soaked
upgrade writes the file again. The ruling's fallback was not used for formulae because the
per-formula lock exists.

Rules that follow:

- brewsoak holds a brew formula lock only for the receipt read and the unlink of one file, and
  never while a brew child runs. Pruning runs before any install and in `brewsoak update`, so
  brewsoak cannot wait on itself.
- The lock directory is `$(brew --prefix)/var/homebrew/locks`, asked once per run and only when a
  formula file is about to be pruned.
- Recovery is unchanged: it touches only files with an undo entry, under the run lock.
- The README keeps its warning. Running brewsoak beside a plain `brew upgrade` or `brew install` is
  still unsupported: brewsoak's own brew children and its settle step are not covered by this.

**Re-running.**

- *A second run.* A file skipped as busy is examined again at the next maintenance point and removed
  once brew is done. Lock files brewsoak created stay until `brew cleanup` removes them (F12).
- *Two runs at once.* Two brewsoak runs: the run lock, as before. A plain brew that asks for the
  same formula lock in the instant brewsoak holds it fails with brew's own "operation in progress"
  error; the lock is held for one directory read and one unlink.
- *Leftovers from a failed run.* The kernel drops an flock when the process exits, so a killed
  brewsoak leaves at most an empty lock file. Pruning keeps no state of its own.

## #22: formulae brew hides from the inventory

**Controller ruling.** Build the inventory, then scan the Cellar for kegs whose name is not in
brew's installed list and read each one's tap from its receipt. When the tap is not core and not
trusted, print a note once per tap on `upgrade`, `outdated` and `info`. Never fatal. Unit-test with
a fake Cellar. Do not change Eric's trust settings.

**Facts.**

- F5. Source: `Formula.installed` is `racks.flat_map { Formulary.from_rack(rack) } rescue []`
  (`formula.rb:2886-2892`): any error drops that formula without a message. `Formulary.from_keg`
  loads `"#{tab.tap}/#{name}"` when the receipt has a tap (`formulary.rb:923-927`), and loading from
  an untrusted tap raises `Homebrew::UntrustedTapError` unless the tap or the formula
  `<tap>/<name>` is trusted (`trust.rb:213-223`). So the omission of 2026-11-27 Fact 37 applies to
  a formula from any untrusted tap, and a per-formula trust entry prevents it. `Formula.racks`
  skips a rack that is a symbolic link, starts with `.` or has no keg (`formula.rb:2861-2868`).
- F6. Live: `brew tap-info` reports `anthropics/tap`, `cyclonedx/cyclonedx` and `endava/tap` as
  `trusted: false`. All 165 Cellar racks are in `brew info --json=v2 --installed` (exit 0, empty
  stderr). `cats` (receipt `source.tap: endava/tap`) is listed because `brew trust --json=v1`
  (keys `taps`, `formulae`, `casks`, `commands`) has `endava/tap/cats`. `cyclonedx-gomod` is listed
  with `tap: homebrew/core` because its receipt has `source.tap: null`; its tap is recorded only in
  `source.path` (`<cache>/staging/taps/cyclonedx/cyclonedx/Formula/cyclonedx-gomod.rb`), which
  `origin::staged_tap` already reads. **No keg is hidden on this machine today**, and making one
  would mean changing trust settings, so the third-party case is confirmed from source (F5, F17) and
  not by a run.
- F17. Source: a keg whose receipt has no tap is loaded by its bare name, and only
  `FormulaUnavailableError` is rescued there (`formulary.rb:923-939`). The bare-name loader looks
  in core first and then in every installed tap, trusted or not (`formulary.rb:589-622`); loading
  the file it found then raises `UntrustedTapError`, a `RuntimeError`, for an untrusted tap
  (`formulary.rb:125`, `trust.rb:12`). So a keg staged by brewsoak before 1.6.1 (null receipt tap,
  tap recorded in `source.path`) is hidden too when its name is not in core and its tap is
  untrusted. `cyclonedx-gomod` escapes only because core has a namesake (F6).

**Behavior** (architect's calls: the trust source, correction 3; the tap of a keg without a receipt
tap, correction 4).

- After the inventory is built, brewsoak lists the Cellar racks (by `Formula.racks`' rules, F5) that
  are not among the installed formulae, the soak-tap kegs added from disk included.
- The tap of such a rack is, in order: `source.tap` of its newest keg receipt; else the tap its
  `source.path` names (`origin::staged_tap`, as for a listed package); else its `origins.toml`
  record. A rack with none of the three, or whose tap is `homebrew/core`, the soak tap or the
  legacy tap, gets no note.
- A hidden formula is reported when its tap is in `brew tap-info` with `trusted: false`. A tap that
  is not tapped, or that has no `trusted` field (an older brew), is not reported.
- One note per tap, names sorted:
  `brew does not trust tap <tap>, so it hides these installed formulae from brewsoak: <a>, <b>; run
  brew trust <tap> (or brew trust --formula <tap>/<name> for one formula) to let brewsoak manage
  them.`
- `upgrade` (bare or named): the note goes under `Notes`. `outdated`: under `==> Held`. `info`: at
  the end. It never changes the exit status, a count or what is installed.
- `prune::prune_cache` keeps the `origins.toml` record of every formula rack brew did not list.
  Today it drops the record of any package missing from the inventory (`src/prune.rs:28-32`), which
  for a hidden formula breaks the AGENTS.md rule "never prune a record ... while the package is
  installed". Same pattern, fixed here.
- Casks need nothing: brew still lists them (2026-11-27 Fact 37).

Why not `brew trust --json=v1`: it would be the first `brew trust` child brewsoak ever runs, against
Human decision 2 and the test helper that forbids the verb, and it adds nothing. An entry for the
formula would have made brew list it (F5), so "brew left it out and its tap is untrusted" already
means no entry works. The remaining case is a formula that fails to load for another reason while
its tap is also untrusted; the note then points at trust when the cause is elsewhere. Cost if wrong:
one read-only `brew trust --json=v1` call when a hidden keg exists.

**Limits.** brewsoak still does not know a hidden package: `brewsoak upgrade <hidden name>` treats
the name as not installed, and brew's own refusal (2026-11-27 Fact 15) is the backstop. A formula
hidden for a reason other than trust gets no note.

## #19: triage of the ten items

**Controller ruling.** Triage each item against main; fix each that still applies with a small,
tested change; an item that no longer applies gets a one-line reason; item 2 goes in the limits.

| Item | Verdict | Change |
|---|---|---|
| 1. One name that makes the loader script exit non-zero fails every query of the call | Applies (`src/brew.rs:977-986`) | Rows the script printed are used whatever its exit status; a name with no row gets brew's failure text |
| 2. An outside `brew update` between the gate and the install goes unnoticed | Applies, by design | "Limits" below. The README has no limits section, so nothing is added there |
| 3. An installed dependency in a held or snapshot-less tap is skipped without a gate | Applies (`src/cmd/deps.rs:226-234`) | See below |
| 4. No test ties a soaked install to a prior `by_name` query | Obsolete after #29 for core and cask packages (they no longer pass the by-name gate). Applies to third-party installs | `MockBrew` records one ordered event log; a helper asserts the query precedes the install |
| 5. Untrusted holds: no inline `held` line, note outside `Held:` | Applies (`src/cmd/apply.rs:1387-1410`) | Inline `held <name>: brew does not trust tap <tap>` and `held <target>: dependency <dep>: brew does not trust tap <tap>`; both notes move under `Held:` with their text unchanged |
| 6. An ERR row whose message contains a tab loses brew's text | Applies (`BY_NAME_RUBY`, `parse_by_name_row`) | The script replaces tabs with spaces; the parser joins extra ERR fields with a space |
| 7. `named_run_of_only_no_soak_packages_updates_in_the_no_soak_step` cannot tell which update site ran | Applies | A second test makes the update fail: one `update`, no `upgrade`, the no-soak note, and not the soaked-site note |
| 8. Overlong line in the `parse_by_name_lines` doc comment | Applies (`src/brew.rs:143`, 130 columns) | Rewrapped |
| 9. README shows only the formula wording of the changed-file hold | Obsolete after #29: `README.md:315` reads `formula\|cask` | None |
| 10. No test for the equal-version hold with a revision, or for the fallback to the issue #17 text | First half applies. Second half obsolete after #29: no text names issue #17; an unparseable cutoff now gives the `not the soaked cutoff` reason | Two tests: `1.0_1` on both sides gives the changed-file text; a cutoff that does not parse gives the other |

**Item 3** (amends the 2026-11-27 dependency table row "a dependency in a held tap or with no
cutoff: as today", for an installed dependency only; a Claude design row). An installed formula
dependency whose class is soaked and whose tap is held this run, or has no snapshot, is no longer
skipped outright. brewsoak asks brew's own dependency test for it (the loader row's `satisfied`,
by bare name, no cutoff). Satisfied: skipped as before, because brew leaves it alone. Not satisfied,
or brew could not answer: the target is held through the existing `HeldDep` path, with

- held tap: `tap <tap> could not be refreshed, and brew does not count the installed <dep> as up to
  date;` followed by git's error on its own lines;
- no snapshot: `tap <tap> has no soak snapshot this run, and brew does not count the installed
  <dep> as up to date`.

A cask dependency and a no-soak or unsoakable dependency are still left to brew. The dependencies of
a skipped dependency are still not walked.

## Limits

- **An outside `brew update` (#19 item 2).** brewsoak asks brew what a name is (`by_name`) and what
  it depends on (`brew deps`) once per run and keeps the answers. If another process runs
  `brew update`, or changes a tap, between that question and brewsoak's install, brewsoak does not
  notice. A third-party package could then install a definition newer than its cutoff, and brew
  could upgrade an installed dependency that is not anchored. A core or cask target itself is not
  affected: it installs from the tap file. The window is the length of one target's install.
- Bare `info --cask` with a formula and a cask of one name ("#26").
- A same-version source change is announced on every run ("#27").
- A hidden package is still unknown to brewsoak ("#22").
- Cask pruning has no lock ("#25").
- The Ruby side of the lock was read, not run. The plan's manual check holds brew's lock from
  `brew ruby` and watches brewsoak skip the file.

## Open item

One thing came up that this work does not decide, tracked in issue #30: for a same-version source
change brew does nothing, so the package is planned and skipped on every run ("#27"). Running
`brew reinstall` in that case would install the soaked change. It is a behavior change with a cost
(a reinstall of every such package) and needs Eric's decision.

## Testing

Unit tests only, with `MockBrew`, `InMemoryGit` and temp directories; the plan lists each test with
its values. New seams: a fake Cellar for the hidden-keg scan; a temp lock directory held by a
second file handle for the formula lock; `MockBrew` fields for busy locks, hidden kegs and the
ordered event log. No test runs real brew.

Done gate: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build &&
cargo test`.
