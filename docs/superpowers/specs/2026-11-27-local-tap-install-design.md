# Installing soaked packages from brewsoak's own local tap

Date: 2026-11-27
Issue: #17 (this is the long-term fix; it closes the issue)
Related: #18 (trust recorded for third-party full tokens; stays open), #19
(stopgap review follow-ups), #22 (formulae from an untrusted third-party tap
missing from the inventory)
Extends: [2026-08-12-brewsoakr-design.md](2026-08-12-brewsoakr-design.md),
[2026-10-05-tap-soak-and-no-soak-design.md](2026-10-05-tap-soak-and-no-soak-design.md)
Supersedes for `homebrew/core` and `homebrew/cask` packages:
[2026-11-26-path-install-stopgap.md](2026-11-26-path-install-stopgap.md)

Status: implemented by `../plans/2026-11-27-local-tap-install.md`.

## Intent

Homebrew 7.0.14+ (commit `ec249da5f7`) no longer loads a formula or cask from a
file path, and its policy document says "Use a tap and refer to packages by
name" (`docs/Homebrew-Package-Policies.md`, line 100). brewsoak 1.6.1 ships a
stopgap: it installs a package by name only while brew's own current file is
byte-identical to the soaked cutoff file, and holds it otherwise. A package
whose file changes more often than the soak time, or whose dependency closure
contains any unsoaked change, never installs.

This design gives brewsoak back the ability to install the cutoff file itself:
it writes the cutoff file into a local Homebrew tap that brewsoak owns and
installs the package from that tap by its full name.

## Goals

- A soaked `homebrew/core` formula or `homebrew/cask` cask installs at its
  cutoff whether or not brew's current file is the cutoff.
- Every earlier human decision that still applies keeps holding (see
  "Constraints").
- A hold is never fatal, and a dry run reports the holds a real run would.
- A run that dies halfway leaves nothing that the next run cannot put right.
- The packages already on a machine (installed by path before 1.6.1, by name
  by 1.6.1, or by plain brew) move to the new mechanism without an uninstall.

## Non-goals

- Soaking third-party tap packages through the local tap. They keep the
  1.6.1 by-name gate (Human decision 3).
- Moving every installed package into the local tap up front. Packages move
  when brewsoak next upgrades them (Human decision 6).
- Repairing a local tap the user has edited by hand.
- Changing `outdated`, `info`, the plan total, the soak clock, the snapshot
  model or the no-soak configuration.
- Fixing issue #18 or the remaining items of issue #19.

## Anti-goals

- brewsoak must never run `brew trust` or `brew untrust`, set
  `HOMEBREW_NO_REQUIRE_TAP_TRUST`, or edit Homebrew's trust store.
- brewsoak must never pass `--ignore-dependencies`, edit a keg receipt, or
  uninstall a keg to move it between taps.
- brewsoak must never copy a third-party tap's file into its own tap in this
  design: that would install the file under brewsoak's trust instead of the
  trust the user gave, or did not give, to the tap it came from.
- brewsoak must never run an `ericfitz/brewsoak/<name>` command against a tap
  of that name that is not its own local one (Human decision 5).
- The GitHub repository `ericfitz/homebrew-brewsoak` must never be created
  (Human decision 5).
- brewsoak must never write, prune or install from the legacy tap
  `brewsoakr/soaked`.
- brewsoak must never use a private Homebrew Ruby API to load a file from a
  path.

## Constraints: earlier human decisions that still hold

| Decision | Source |
|---|---|
| The soak clock starts on any file change; the eligible artifact is the cutoff file | 2026-08-12 |
| `brew` is the only installer | 2026-08-12 |
| `--ignore-dependencies` is never passed; the cutoff dependency closure is installed first | 2026-08-12, step 5 |
| Installed newer than the cutoff is left alone; no downgrade | 2026-08-12 |
| A mixed run applies what it can, refuses the rest and exits 1; a refusal says why and how to bypass brewsoak | 2026-08-12 |
| brewsoak never trusts a third-party tap on the user's behalf and never runs `brew trust` | 2026-10-05, amended 2026-10-06 |
| brewsoak never fetches into, or moves the checkout of, a repository under `$(brew --repository)/Library/Taps` | 2026-10-05 |
| A no-soak package is installed by brew from its real tap and ends in the same state as `brew upgrade <pkg>` | 2026-10-05 |
| A formula's origin comes from its keg receipt, not `brew info` | 2026-10-05 |
| Holds are never fatal; the next package still runs | 2026-11-26 |
| The by-name gate compares exact file bytes (where the gate still applies) | 2026-11-27 |
| A changed file at the same version is a soak wait, with its own hold text | 2026-11-27 |
| A run with soaked work runs `brew update` once, before that work, also on a dry run; the no-soak step does not repeat a successful one | 2026-11-27 |
| A dry run still runs the one `brew update` | 2026-10-06 |

Decisions this design amends, each named where it happens:

- 2026-08-13 "brewsoak has no tap of its own": brewsoak owns a tap again,
  `ericfitz/brewsoak` (Human decisions 1 and 5 below). The 2026-10-06 ruling
  that the name `brewsoakr/soaked` is recognized only as a leftover is not
  amended: it stands as written.
- 2026-11-27 "Tokens: the bare name, never `user/repo/name`": amended for
  soaked `homebrew/core` and `homebrew/cask` installs (Human decision 2
  below). It stands for third-party tap packages.
- The AGENTS.md rule "never touch `$(brew --repository)/Library/Taps`" gets one
  exception: the directory of brewsoak's own tap.
- 2026-10-05 "Accepted consequence: `brew upgrade` also upgrades a no-soak
  package's outdated dependencies to brew's latest". With this design that is
  no longer true for a dependency installed from brewsoak's tap. Amended by
  Human decision 4 below.

## Human decisions (made by the maintainer, Eric, on 2026-11-27)

These are architectural decisions made by the human maintainer. They are not
to be changed without the maintainer's approval.

1. **The long-term mechanism is a local tap** ("option A"). It was chosen over
   the four alternatives in "Approaches considered": a local bottle-file
   install, a local tap plus bottle-file hybrid, Homebrew's private Ruby
   loaders, and keeping the stopgap permanently.
2. **Trust.** Installs from brewsoak's tap use full tokens, `<tap>/<name>`.
   Homebrew records per-package trust for every full-token install
   (`==> Trusted formula <tap>/<name>`), even when the tap itself is already
   trusted. This is accepted for brewsoak's own tap. The user trusts
   brewsoak's tap once, themselves, with `brew trust <tap>`; brewsoak still
   never runs `brew trust` or `brew untrust`. The acceptance does not extend
   to third-party taps: issue #18 stays open for the no-soak step and the
   `reinstall` true repair. This amends the stopgap's "tokens: the bare name"
   decision for soaked installs.

Decisions 3 to 6 are the maintainer's answers, given later on 2026-11-27, to
the four questions the first draft of this spec asked. The questions and the
options are under "Questions answered by the maintainer".

3. **Third-party tap packages stay out of the soak tap.** Only
   `homebrew/core` and `homebrew/cask` packages are installed from it.
   Third-party tap packages keep the 1.6.1 by-name gate, and a third-party
   package whose name also exists in core stays held, as it is today.
4. **Soaking wins for the dependencies of a no-soak package.** A dependency
   installed from the soak tap stays at its soaked version when brew upgrades
   a no-soak package that depends on it. This amends the 2026-10-05 "accepted
   consequence" that `brew upgrade` also upgrades a no-soak package's
   outdated dependencies to brew's latest, so that no-soak extends to its
   dependencies: that no longer holds for a dependency installed from the
   soak tap. The consequence is documented, not prevented: a no-soak package
   can run against an older dependency than its bottle was built with until
   that dependency's update has soaked.
5. **The soak tap is `ericfitz/brewsoak`**, a local tap at
   `$(brew --repository)/Library/Taps/ericfitz/homebrew-brewsoak`. This
   reverses Choice 2 of the first draft, which reused `brewsoakr/soaked`,
   because nobody owns the name `brewsoakr` on GitHub (Fact 35) while the
   maintainer owns `ericfitz`. The GitHub repository `ericfitz/homebrew-brewsoak` does
   not exist (Fact 42) and must never be created; "Tap name, location and
   bootstrap" says why. `brewsoakr/soaked` stays recognized only as the legacy
   staging tap: `origin::STAGING_TAP`, its classification, and the
   legacy-receipt rule. brewsoak still uses its tap only when brew reports it
   as local.
6. **No up-front migration.** An installed package moves to the soak tap at
   its next soaked upgrade.

## Approaches considered

| Approach | For | Against |
|---|---|---|
| **A. Local tap** (chosen) | The path Homebrew's policy names. Installs the cutoff file itself. brew does the download, the pour, the receipt and the linking. Works for formulae and casks. | The user must trust the tap once. The costs found by the spike are listed under "What option A costs". |
| B. Local bottle-file install | No tap and no trust step. | Not spiked. brewsoak would have to fetch and verify bottles itself. It has no answer for source-only formulae, casks or dependencies. `Formulary::FromBottleLoader.from_path` is marked `@api private` (`formulary.rb`, lines 395-400). |
| C. A plus bottle files | Could skip brew's bottle download. | Both mechanisms to build and test, and the trust step of A anyway. |
| D. Private Ruby loaders under `brew ruby` (`Formulary.from_path`) | Closest to the pre-1.6.1 path install. No tap. | `Formulary.from_path` is marked "`@api private` ... Homebrew/brew callers only" (`formulary.rb`, lines 819-826). brewsoak would drive the installer from a script, outside brew's command-line checks, on an interface Homebrew can change in any release, as it changed path loading. |
| E. Keep the stopgap | Nothing new to build. | A package that changes more often than the soak time never installs, and any unsoaked change in a dependency closure holds the target. |

### What option A costs

All of these come from the spike below and are designed for in this document.

- `brew install <tap>/<name>` refuses a package whose keg was installed from
  another tap (Fact 15). `brew upgrade <tap>/<name>` does not (Fact 16).
- A package installed from the tap follows the tap's file from then on. Plain
  `brew upgrade` and `brew outdated` no longer see a newer core version of it
  (Fact 20). That stops an accidental unsoaked upgrade, and it changes the
  command that bypasses brewsoak (Fact 24).
- If the tap's file and the installed keg disagree, any install that depends
  on the package fails with brew's "already installed from" error, and brew's
  dry run does not predict it (Facts 22 and 23).
- A name present in two non-core taps is ambiguous everywhere (Fact 14), so
  third-party files cannot be copied into the tap.
- brew skips bottle attestation checks for a core bottle poured through the
  tap when `HOMEBREW_VERIFY_ATTESTATIONS` is set (Fact 33). The bottle's
  SHA-256 in the formula file is still enforced.
- A dependency installed from the tap is not upgraded when a newer dependent
  wants a newer version of it (Source fact 1).

## Verified facts

Spike of 2026-11-27 on Homebrew 7.0.17-27-g96c963e, macOS arm64. Two
throwaway taps were used: `S` is `spike17a/t1` (in the role of brewsoak's
tap) and `T` is `spike17b/t2` (in the role of a third-party tap). `hello` is
the core formula (2.12.3, no dependencies). Test formulae, the test cask, both
taps and every trust entry were removed afterwards; the check is under
"Spike cleanup".

Bootstrap and trust:

1. `HOMEBREW_DEVELOPER=1 brew tap-new --no-git S` creates
   `$(brew --repository)/Library/Taps/spike17a/homebrew-t1` with a `Formula`
   directory, `README.md` and four files under `.github`. A second call fails:
   `Error: Tap is already installed!`, exit 1. Neither call changes the trust
   store.
2. A directory made with `mkdir -p .../Taps/spike17b/homebrew-t2/Formula` is a
   tap: `brew tap` lists it and brew loads formulae from it.
3. `brew tap-info --json` reports `path`, `remote` (`null` for a local tap)
   and `trusted` for each tap. `trusted` is `false` for a new tap and `true`
   after `brew trust <tap>`. On the maintainer's machine `brewsoakr/soaked`
   reports `trusted: true`, `remote: null`.
4. On an untrusted tap, `brew deps --1 --full-name --formula S/hello`,
   `brew info S/hello` and `Formulary.factory("S/hello")` under `brew ruby`
   all refuse (`Refusing to load formula S/hello from untrusted tap S.`,
   `Homebrew::UntrustedTapError`) and leave the trust store unchanged.
5. `brew install --dry-run --formula S/hello` on the untrusted tap prints
   `==> Trusted formula S/hello`, adds `S/hello` to `trustedformulae` and
   goes on. With the tap trusted, every full-token `install` and `upgrade`
   still adds an entry: on a dry run, and on an `upgrade` that then fails
   with "not installed". A cask adds an entry to `trustedcasks`.
6. `brew trust --json=v1` lists the trust store without changing it.
7. While `T` was tapped and untrusted, the `brew install` commands of the
   spike (real and dry run) printed a multi-line
   `Warning: The following taps are not trusted:` block naming it.

Files:

8. A verbatim core file in the tap has `ruby_source_checksum` equal to the
   SHA-256 of the file (`hello`: `3d85fa64...`), and the same holds for a
   cask file.
9. A file that contains `no_autobump!` does not load from the tap:
   `Error: S/hello: no_autobump! can only be used in official Homebrew taps.`
   (`formula.rb` line 5212; `cask/dsl.rb` lines 693-694). No other
   official-tap-only stanza was found in `formula.rb` or `cask/dsl.rb`.
10. A verbatim core file installed from the tap pours core's own bottle from
    `ghcr.io/v2/homebrew/core`.

Name resolution:

11. A bare name that exists in core and in `S` resolves to core and is never
    ambiguous (`brew info --json=v2 hello` reports `homebrew/core`).
12. A missing dependency named bare (`depends_on "hello"` in `S/parent`, with
    a `hello.rb` in `S`) is installed from core: its receipt says
    `homebrew/core`.
13. `brew deps --1 --full-name --formula S/parent` prints `hello`.
14. A name that exists in `S` and `T` and not in core is ambiguous:
    `brew info`, `brew install`, `brew upgrade` and `brew reinstall` with the
    bare name fail with `Formulae found in multiple taps`. This happens while
    `T` is untrusted and after the package is installed from `S`. A formula
    that names it as a bare dependency can be neither listed by `brew deps`
    nor installed. `Formulary.factory` and `Formulary.resolve` raise
    `TapFormulaAmbiguityError`.

Moving a formula between taps:

15. `brew install --formula S/hello` over a keg whose receipt says
    `homebrew/core` is refused, for the same version and for a newer one, on
    a dry run too: `hello was installed from the homebrew/core tap but you
    are trying to install it from the S tap.` (`install/check.rb` line 55;
    only `cmd/install.rb` calls it). The other direction
    (`brew install homebrew/core/hello` over an `S` keg) is refused the same
    way. This is the refusal commit `d322486` ran into.
16. `brew upgrade --formula S/hello` with a newer file in `S` upgrades a keg
    whose receipt says `homebrew/core` (2.12.3 to 2.12.3_1), a keg whose
    receipt tap is `null` (2.12.3 to 2.12.3_1) and a keg from `S` itself
    (2.12.2 to 2.12.3). The new keg's receipt says `S`.
17. With the same version in `S`, `brew upgrade S/hello` answers
    `already installed` and leaves the receipt alone. `brew install S/hello`
    over a `null`-receipt keg answers the same.
18. `brew reinstall --formula S/hello` over a `homebrew/core` keg of the same
    version reinstalls it; the receipt then says `S`.
    `brew reinstall homebrew/core/hello` moves it back.
19. `brew upgrade --formula S/x` for a package that is not installed fails:
    `Error: S/x not installed`, exit 1.

A keg installed from the tap ("anchored", defined under "Terms"):

20. Keg 2.12.2 from `S`, `S` file at 2.12.2, core at 2.12.3: `brew outdated`,
    `brew upgrade hello` and a bare `brew upgrade --dry-run` do not offer it.
    `Formulary.factory("hello")` is core's 2.12.3 (not installed), while
    `Formulary.resolve("hello")` is `S/hello` 2.12.2 (installed) and
    `Dependency.new("hello").installed?` is `true`.
21. In that state a package with a bare `depends_on "hello"` installs and
    brew leaves `hello` alone.
22. `S` file newer than the keg (file 2.12.3, keg 2.12.2 from `S`): the same
    install fails with `Error: hello is already installed from S! Please
    brew uninstall hello first.`, exit 1 (`formula_installer.rb` lines
    985-991). The dry run had printed `Would upgrade 1 dependency for
    <parent>:` and `hello 2.12.2 -> 2.12.3`, with no error. The keg stays
    installed and linked.
23. `S` file deleted (keg 2.12.2 from `S`): the bare name falls back to core.
    `brew outdated` lists `hello (2.12.2) < 2.12.3`; a plain
    `brew upgrade hello` upgrades it from core and the receipt then says
    `homebrew/core`; an install that depends on it fails as in Fact 22.
24. `brew upgrade homebrew/core/hello` over the anchored 2.12.2 keg upgrades
    it to core's 2.12.3; the receipt then says `homebrew/core`.
25. `brew untap S` while kegs are installed from `S` is refused (exit 1) and
    lists them.
26. The keg's own copy of its formula (`.brew/hello.rb`) is byte-identical to
    the tap file after a source build and differs after a bottle pour.

Casks (a harmless test cask with one `binary` artifact, given the token
`font-hack`, which also exists in `homebrew/cask` at 3.003):

27. `brew info --cask S/font-hack` loads the tap's file; the bare token
    resolves to `homebrew/cask`.
28. `brew install --cask S/font-hack` installs it. The receipt
    `Caskroom/font-hack/.metadata/INSTALL_RECEIPT.json` has `source.tap` `S`
    and `source.version`. `brew info --json=v2 --installed` reports `tap: S`.
29. Plain brew does not call it outdated against `homebrew/cask`. After the
    `S` file moves to a newer version, `brew install --cask S/font-hack`
    upgrades it (its dry run prints nothing) and
    `brew upgrade --cask --dry-run S/font-hack` plans the upgrade.
30. With the receipt's tap hand-edited to `homebrew/cask` (a simulation: no
    real `homebrew/cask` cask was installed), `brew install --cask
    S/font-hack` with a newer file upgrades it with no refusal, and the
    receipt then says `S`. With the same version it answers `already
    installed`; `brew reinstall --cask S/font-hack` moves the receipt.
31. `S` cask file deleted: brew keeps describing the installed cask as
    `S/font-hack` at its installed version and never calls it outdated. This
    differs from a formula (Fact 23).
32. `brew upgrade --cask --dry-run homebrew/cask/font-hack` over the cask
    installed from `S` plans `1.2 -> 3.003` (dry run only).

Other:

33. `HOMEBREW_VERIFY_ATTESTATIONS=1 brew reinstall --formula S/hello` prints
    `Warning: Skipping attestation verification for hello. S/hello is from S,
    which uses a non-default remote and cannot be attested safely.` and
    installs.
34. The maintainer's machine has 165 formula kegs: 101 with a `null` receipt
    tap (installed by path before 1.6.1), 60 with `homebrew/core`, 4 with
    `ericfitz/tap`, none with `brewsoakr/soaked`. Its `brewsoakr/soaked` tap
    holds 52 formula files dated 2026-08-13 and no casks.
35. GitHub has no user or organization named `brewsoakr`
    (`gh api users/brewsoakr` answers 404).

An untrusted tap that already has packages installed from it (second spike,
same day and method: `hello` 2.12.2 and the test cask installed from `S`,
then `brew untrust --tap S`; a package in the trusted tap `T` has a bare
`depends_on "hello"`):

36. `brew untrust --tap S` removes the tap's entry and also every
    `S/<name>` entry in `trustedformulae` and `trustedcasks`.
37. `brew info --json=v2 --installed` still exits 0, with nothing on stderr,
    and silently leaves the formula out (165 formulae where there were 166).
    The cask is still listed, with `tap: S`. `brew outdated --json=v2` also
    exits 0 without the formula. `brew tap-info` reports `trusted: false`.
    After `brew trust S` the formula is listed again.
38. In that state `brew deps --1 --full-name --formula hello` (bare) exits 0
    and `Formulary.factory("hello")` loads core's file. `brew deps S/hello`,
    `brew info hello`, `brew upgrade hello`, `Formulary.resolve("hello")` and
    `Dependency.new("hello").installed?` all fail with `Refusing to load
    formula S/hello from untrusted tap S.` (`Homebrew::UntrustedTapError`).
    `brew list --versions hello` works. A bare `brew upgrade --dry-run`
    exits 0 and warns about the untrusted tap.
39. In that state a plain `brew install` of the package that depends on
    `hello` fails with the same refusal, exit 1. The `hello` keg is
    untouched.
40. In that state `brew upgrade homebrew/core/hello` upgrades the keg from
    core (2.12.2 to 2.12.3); its receipt then says `homebrew/core` and
    `brew info --json=v2 --installed` lists it again.
41. With `S` trusted, the cask's receipt naming `S` and its tap file deleted
    (the state of Fact 31): a bare `brew upgrade --cask --dry-run font-hack`
    answers `the latest version is already installed`, while
    `brew upgrade --cask --dry-run homebrew/cask/font-hack` plans
    `1.0 -> 3.003`.

After the maintainer's decisions:

42. `gh api repos/ericfitz/homebrew-brewsoak` answers 404: GitHub has no
    such repository (checked by the maintainer's session and again for this
    spec, 2026-11-27).

### Source facts (read in Homebrew's source, not run)

1. `Dependency#installed?` returns `true` as soon as the formula it resolved
   has its latest version installed, before it looks at a dependent's bottle
   minimums (`dependency.rb` line 145), and it resolves through the keg's
   receipt tap (`Formulary.resolve`). So brew never upgrades an anchored
   dependency because a dependent's bottle was built against a newer one.
2. brew skips a satisfied dependency but still expands that dependency's own
   dependencies (`formula_installer.rb` line 830, `Dependable::SKIP`).
3. `brew update` skips a tap directory that has no `.git`
   (`cmd/update.sh` lines 752 and 1034).
4. A developer command such as `tap-new` turns Homebrew's developer mode on
   only when `HOMEBREW_DEVELOPER` is unset (`brew.sh` lines 593-604).
   brewsoak sets `HOMEBREW_DEVELOPER=1` on every child except `update`. Not
   observable on the spike machine, where the mode was already on.
5. A tap's formula files are `Formula/**/*.rb`; for two files with the same
   base name the longer path wins (`tap.rb` lines 1094-1117).
6. `Trust.trust_fully_qualified_items!` skips an official tap
   (`next if tap.implicitly_trusted?`), so a `homebrew/core/<name>` token
   records no trust.
7. `brew install user/repo/name` first installs the tap `user/repo` when it
   is not tapped (`cmd/install.rb` line 192, `ensure_installed!`), which for
   a tap with no local directory means cloning it from GitHub, and then
   records trust for the named item (line 194).
8. `brew reinstall <name>` resolves a bare name through the installed keg
   (`cmd/reinstall.rb` line 137, `method: :resolve`; `cli/named_args.rb`
   lines 349-350), as `brew upgrade <name>` was seen to do in Fact 20. So a
   bare reinstall of an anchored package should load its tap file.

### Not verified

- `brew deps --1 --full-name --cask <tap>/<token>`.
- A cask whose receipt really came from `homebrew/cask` (Fact 30 is a
  simulation), and a real run of Fact 32.
- A bare `brew reinstall <name>` of an anchored package (Source fact 8 says
  what to expect; it was not run).
- A `no_autobump!` stanza that spans more than one line.
- Any Homebrew other than 7.0.17-27-g96c963e (see "Supported Homebrew").
- Anything on Linux.

The builder verifies the first three before relying on them (see "What is
left to the builder").

### Spike cleanup

After each of the two spikes, `brew list --formula`, `brew list --cask`,
`brew tap`, `~/.homebrew/trust.json` (compared with `jq -S`) and the
`source.tap` and `source.path` of all 165 keg receipts were each identical to
the snapshot taken before the first spike. Neither spike tap directory, no test keg, cask, link,
lock, log or cached download remained.

## Design choices in this spec (by Claude)

Every item here is Claude's call, not a human decision. The maintainer has
not ruled on them, except where an item says a human decision confirmed or
replaced it. Each is explained where the design uses it. The numbers are kept
so that references stay valid.

1. Only `homebrew/core` and `homebrew/cask` packages use the local tap.
   Third-party tap packages keep the by-name gate. *Confirmed by Human
   decision 3.*
2. *Replaced by Human decision 5.* The first draft reused the tap name
   `brewsoakr/soaked`; the tap is `ericfitz/brewsoak`. What remains of this
   choice is the guard: brewsoak uses the tap only when brew reports it as a
   local tap.
3. brewsoak creates the tap with `brew tap-new --no-git` on the first real
   run that needs it. A dry run does not create it.
4. Trust is read from `brew tap-info`. An untrusted tap holds every package
   that needs it. There is no fall back to the by-name gate.
5. The tap file is the cutoff file with `no_autobump!` lines removed and
   nothing else changed. The layout is flat.
6. A tap file is written just before brew is asked about the package, and is
   kept or put back according to the keg brew leaves behind. An undo copy in
   the cache makes that survive a crash.
7. A formula that is not installed goes to `brew install`; an installed one
   goes to `brew upgrade`. A cask goes to `brew install`.
8. Dependencies are installed from the tap first, by full token. Whether brew
   will leave an installed dependency alone is asked of brew's own dependency
   test.
9. An installed dependency that brew would replace with an unsoaked version
   holds the target. brewsoak does not reinstall a package just to move it
   into the tap. *Confirmed by Human decision 6.*
10. The by-name gate is removed for core and cask packages and kept for
    third-party ones. No hold text names issue #17 any more.
11. A hint that bypasses brewsoak, and a no-soak token, name the real tap in
    full (`homebrew/core/<name>`) for a package installed from the tap.
12. brewsoak removes tap files that no installed package uses, at the points
    Choice 16 names.
13. A dry run writes the same tap files, forwards `--dry-run`, and puts every
    file back.
14. The terminal shows bare names; the tap prefix and brew's `Trusted` lines
    go to the log only.
15. When the soak tap is untrusted and packages are already installed from
    it, brewsoak completes its inventory from the kegs on disk and holds
    those packages. The run is never fatal for that reason.
16. Outside an install, brewsoak touches tap files at two points only:
    `brewsoak update`, and the start of a real `upgrade`, `install` or
    `reinstall`. Recovery needs the tap to be local; pruning also needs it
    to be trusted. A dry run, `outdated` and `info` never do either.
    `prune::prune_cache` does not change.
17. "Installed from the soak tap" means anchored: the receipt names the tap
    and the tap has a file at the installed version. A receipt that names
    the tap without such a file, and any receipt that names the legacy tap
    `brewsoakr/soaked`, gets the classification a `brewsoakr/soaked` receipt
    has today.
18. The minimum Homebrew is 7.0.14, documented and not checked at run time.

## Design

### Terms

- **The soak tap**: brewsoak's own local tap, `ericfitz/brewsoak` (Human
  decision 5).
- **The legacy tap**: `brewsoakr/soaked`, the staging tap of pre-1.0 builds. brewsoak
  recognizes the name and never uses, writes or prunes the tap.
- **Soak-tap package**: a soaked package whose origin is `homebrew/core` or
  `homebrew/cask`. Only these are installed from the soak tap.
- **Anchored**: an installed package whose receipt names the soak tap and
  whose tap file has the installed version. brew resolves an anchored
  package to the tap file and therefore leaves it alone (Facts 20, 21, 29).
  Wherever this document says a package is "installed from the soak tap", it
  means anchored (Choice 17).
- **Legacy receipt**: a receipt that names the legacy tap, or one that names
  the soak tap while the soak tap has no file at the installed version. A
  keg left by a pre-1.0 build is the first kind (no released build created
  that tap; see
  [../plans/2026-10-06-remove-staging-tap.md](../plans/2026-10-06-remove-staging-tap.md));
  a keg whose soak-tap file was deleted by hand is the second. Neither is
  "installed from the soak tap": the package keeps the classification a
  `brewsoakr/soaked` receipt has today, a receipt that says nothing about origin.
  brew resolves the bare name of the second kind to core (Fact 23), and of
  the first kind to core as well unless the legacy tap still holds a file of
  that name.
- **The tap invariant**: every package brewsoak installs from the soak tap
  stays anchored. brewsoak keeps that true at the end of every run. A tap
  file at another version beside such a formula makes installs that depend
  on it fail (Fact 22).

### Which packages use the soak tap (Choice 1)

| Package | Mechanism |
|---|---|
| Soaked, origin `homebrew/core` or `homebrew/cask` | The soak tap, this design |
| Soaked, origin a third-party tap | The 1.6.1 by-name gate, unchanged apart from its hold text |
| No-soak | brew, from its real tap, as today |
| Unsoakable | Never installed by brewsoak, as today |

A third-party file is never written into the soak tap, for two reasons. The
name would exist in two non-core taps and become ambiguous for every bare
use, including bare dependencies (Fact 14). And the file would be loaded
under the trust of brewsoak's tap, not the trust the user gave its own tap,
which the 2026-10-05 decision forbids. A core or cask name is never ambiguous
(Fact 11).

One known case stays as it is today: a third-party package whose name also
exists in core (`cyclonedx/cyclonedx/cyclonedx-gomod` on the maintainer's
machine) is held, because its bare name resolves to core (Human
decision 3).

### Supported Homebrew (Choice 18)

The minimum is Homebrew 7.0.14, the release that removed path loading and
the floor the 1.6.1 stopgap was written for. The README states it. brewsoak
does not check brew's version at run time.

Every fact in this document was observed on one build, 7.0.17-27-g96c963e.
Nothing was run on 7.0.14, 7.0.15 or 7.0.16, or on anything older. Below
7.0.14 this design is unsupported and untested; what can be said from the
design alone is:

- The 2026-10-05 spec records brew's untrusted-tap refusal from Homebrew
  7.0.8 on. A brew without tap trust reports no `trusted` field, which
  brewsoak reads as trusted, and never refuses a tap.
- brewsoak asks brew, not its own model of brew, wherever it can: the
  dependency list comes from `brew deps` on the tap file, "will brew leave
  this dependency alone" from brew's own dependency test, and "did it land
  in the tap" from the receipt brew wrote. A brew that behaves differently
  from Facts 15 to 26 should therefore produce a brew error or a hold, not a
  silent unsoaked install. That is reasoning, not a test result.

A runtime version check is left out because it would have to guess at
versions nobody has tested, in both directions. If a later Homebrew changes
one of the facts, that is a new issue with a new spike.

### Tap name, location and bootstrap (Human decision 5, Choice 3)

The soak tap is `ericfitz/brewsoak`, at the `path` that `brew tap-info`
reports for it
(`$(brew --repository)/Library/Taps/ericfitz/homebrew-brewsoak`). It gets its
own constant beside `origin::STAGING_TAP`, and it is classified by name
before the "no HTTPS remote" rule, as the legacy name is: never fetched,
never soakable, never an origin. A receipt that names it says nothing about
origin.

**The repository `ericfitz/homebrew-brewsoak` must never exist on GitHub.**
`brew install ericfitz/brewsoak/<name>` on a machine where the tap is not
tapped makes brew clone `github.com/ericfitz/homebrew-brewsoak` and then
trust the named package (Source fact 7). While the repository does not exist
(Fact 42) that clone fails and nothing is installed. If it existed, a user
who typed such a command, or any tool that did, would install whatever the
repository held, unsoaked, under the name of brewsoak's tap. A machine that
tapped it would also make brewsoak hold everything, because the tap would no
longer be local. Only the owner of the `ericfitz` account can create it,
which is why the name was chosen over `brewsoakr/soaked`, whose owner name anyone
could register (Fact 35). The README and AGENTS.md carry this rule.

**The maintainer's real tap, `ericfitz/tap`.** It is a different tap:
another name, another directory (`homebrew-tap`), cloned from GitHub, and the
home of the `brewsoak` formula itself. brewsoak matches the soak tap by its
exact name and requires `remote: null`, so `ericfitz/tap` is never mistaken
for it and keeps the class it has today (soakable, or no-soak where the
config says so). A token `ericfitz/tap/<name>` is a third-party token as
before; a token `ericfitz/brewsoak/<name>` on brewsoak's command line is
refused (see "Receipts, inventory and origin").

**The legacy tap.** `origin::STAGING_TAP` (`brewsoakr/soaked`), `TapClass::Staging`
and the receipt rules for that name keep their meaning and nothing more.
brewsoak never writes, prunes or installs from that tap. On the maintainer's
machine it holds 52 old files and no keg refers to it (Fact 34); it can be
removed by hand with `brew untap brewsoakr/soaked`, and the README says so.

**First use.** No machine has the soak tap yet, the maintainer's included.
The first real run that needs it creates it and then holds every soak-tap
package until the user has run `brew trust ericfitz/brewsoak` once. The note
of that hold, and the README, give that command.

brewsoak reads the tap's state from the `brew tap-info --json --installed`
call it already makes; `TapInfo` gains `path` and `trusted`. Before any
soak-tap work in `upgrade`, `install` or `reinstall`:

| State of `ericfitz/brewsoak` | Real run | Dry run |
|---|---|---|
| Not tapped | Run `brew tap-new --no-git ericfitz/brewsoak`, read `tap-info` again, and continue with the row that then applies | Create nothing; treat as untrusted |
| Tapped, `remote` not null | Hold (below). brewsoak never uses a tap of this name that was cloned from somewhere | The same |
| Tapped, local, `trusted: false` | Hold (below) | The same |
| Tapped, local, trusted | Use it | Use it |

A `tap-info` entry with no `trusted` field (a Homebrew older than tap trust)
counts as trusted; brew's own refusal text remains the backstop, as today. A
`trusted` value that is present and not a boolean counts as untrusted.
`brew tap-new` is the documented way to make a tap
(`docs/How-to-Create-and-Maintain-a-Tap.md`) and creates the `Formula`
directory (Fact 1); brewsoak creates `Casks` when it first writes a cask. A
failed `tap-new` is a hold with brew's first line, not a fatal error. The
check that the tap is local also means brewsoak never issues a full token
for a tap that brew would otherwise clone from GitHub (Source fact 7,
Human decision 5).

A run with no soak-tap work never creates the tap. `brewsoak update`,
`outdated` and `info` never create it.

### Trust (Human decision 2, Choice 4)

brewsoak never runs `brew trust`. When the soak tap cannot be used, every
soak-tap package that the run would install or upgrade is held: it keeps its
`[i/N]` line, prints `held <name>: brewsoak's tap is not ready`, and counts
as held. The reason is deferred once per run under `Held:`, not once per
package:

- not trusted:
  `brewsoak installs soaked packages from its own tap, ericfitz/brewsoak, and
  brew does not trust it yet; run brew trust ericfitz/brewsoak once, then run
  brewsoak again.`
- dry run, not tapped:
  `brewsoak's tap, ericfitz/brewsoak, does not exist yet; a real run creates
  it, and it then needs brew trust ericfitz/brewsoak.`
- cloned from a remote:
  `ericfitz/brewsoak is tapped from <remote>; brewsoak only uses a local tap
  of that name; remove it with brew untap ericfitz/brewsoak.`
- `tap-new` failed:
  `brewsoak could not create its tap (<first line of brew's error>).`

Packages that need no install (already soaked, ahead of soak, refused), the
third-party packages and the no-soak step proceed as usual. The run exits 1.
The decision is made before brew is asked to install anything, because an
install with a full token would trust the package and go ahead even on an
untrusted tap (Fact 5). The same refusal text from a later `brew deps` or
install maps to the same hold.

There is no fall back to the by-name gate while the tap is untrusted. One
mechanism per origin keeps the receipts and the hold reasons predictable, and
the missing trust step stays visible until it is done.

Accepted under Human decision 2 and recorded here so nobody is surprised:
brew adds a `ericfitz/brewsoak/<name>` entry to the user's trust store for
every package brewsoak installs, upgrades or previews with `--dry-run`
(Fact 5). brewsoak never removes them.

### An untrusted tap that already has packages (Choice 15)

This is the state after `brew untrust ericfitz/brewsoak`, after a trust store
is restored or wiped, or on a machine that has kegs with that receipt and has
never trusted the tap. brew then hides the tap's formulae from
`brew info --json=v2 --installed` without an error (Fact 37), refuses every
load that goes through the tap (Fact 38), and fails plain installs that
depend on those formulae (Fact 39). brewsoak must not mistake such a package
for one that is not installed, and must not abort.

**Reading the inventory.** After it parses brew's JSON, brewsoak lists the
directories of the Cellar and takes every formula that brew did not list and
whose newest keg has a receipt naming the soak tap. It adds each one to the
inventory from disk: the name, the newest keg's version, and the keg's own
formula copy (`.brew/<name>.rb`, which is what brewsoak reads as the
installed identity today). Its pin state is unknown and is taken as not
pinned, which has no effect because the package is held. This runs on every
inventory load, whatever `tap-info` says, so the two sources cannot disagree
about which packages exist. Casks need nothing: brew still lists them, with
the soak tap as their `tap` (Fact 37). A keg that brew did not list and whose
receipt names any other tap, the legacy tap included, is ignored, as it is
today. Fact 37 very likely applies to formulae from any untrusted third-party
tap; that is tracked in issue #22 and is not part of this design.

**Holding.** While the soak tap is not usable (untrusted, or tapped from a
remote), every installed package whose receipt names it is held for
`upgrade`, `install` and `reinstall`, with the line and the once-per-run note
of "Trust". `brewsoak install <name>` for such a package is that hold, never
a fresh install. `outdated` and `info` still show the package, with its
installed, cutoff and HEAD identities computed as usual from the keg's
formula copy, and add the same note; they exit as they do today. Every other
package proceeds.

**Dependencies.** When such a package is in another target's closure, brew's
dependency test fails for it with the untrusted-tap refusal (Fact 38). That
is one row of the loader script's answer, not a failure of the script. The
target is held, and because the tap named is brewsoak's own, the note is the
tap-not-ready note, not the third-party text that suggests `NO_SOAK`. This
is how a third-party target with such a dependency is held.

**The no-soak step.** A no-soak package that is itself installed from the
soak tap goes to brew under its `homebrew/core/<name>` token, which works
while the tap is untrusted (Fact 40). A no-soak package that depends on one
fails in brew with the refusal (Fact 39); that is brew's failure for that
package, reported as today, and the tap-not-ready note says what to do.

**If the inventory call itself fails.** A `brew info --json=v2 --installed`
that exits non-zero stays fatal, as it is today: brew could not describe the
machine, and brewsoak has no second source for casks, pins or third-party
kegs. The untrusted soak tap does not cause that failure (Fact 37). This is a
stated limit, not something this design repairs.

The README tells the user the two ways out: `brew trust ericfitz/brewsoak`,
or moving a package back to core with `brew upgrade homebrew/core/<name>`
(Fact 40).

### Tap files (Choices 5 and 6)

**Content.** The file is `tap::sanitize_unofficial` of the raw cutoff blob:
lines that start with `no_autobump!` are dropped and nothing else changes
(Fact 9). A blob with no such line is written byte for byte, and brew's
`ruby_source_checksum` of it then equals the SHA-256 of the cutoff blob
(Fact 8). The stanza only controls Homebrew's own version-bump automation.

*Decision (Eric, 2026-11-27, issue #23):* a multi-line `no_autobump!` stanza
is removed whole: a line ending in `,` or `\` pulls in the next line, and a
`do ... end` block runs to the first later line with the stanza's own
indentation that starts with `end`. A block with no such line loses only its
first line. Every other byte stays unchanged.
The by-name gate for third-party packages keeps hashing the raw blob.

**Layout.** `Formula/<name>.rb` and `Casks/<token>.rb`, flat, written by
`tap::write_blob` with the soak tap's directory as `tap_root` (the parameter
that `upgrade`, `install` and `reinstall` still thread through). brewsoak
owns every `.rb` file under those two directories and nothing else in the
tap. `write_blob` changes to write a temp file and rename it, as
`atomic::write` does; that temp name (`.<name>.rb.tmp-<pid>`) does not end
in `.rb`, so brew never loads it.

**Writing.** brewsoak changes a package's tap file only while it holds the run
lock, and only when it is about to ask brew about that package: `brew deps`
on it, or its install or upgrade. Before it replaces or creates the file it
saves an undo entry in `<cache>/soak-tap-undo/{Formula,Casks}/`:
`<name>.rb` with the previous bytes, or an empty `<name>.absent` when there
was no file. The undo entry is written with `atomic::write` before the tap
file changes. A file that already has the wanted bytes is not touched and
gets no undo entry.

**Settling.** When a target is finished, whatever the outcome, every file with
an undo entry is settled, dependencies included:

- **Keep** the new file and delete the undo entry when the package is now
  installed at the file's version with a receipt that names the soak tap.
  For a formula that is `source.tap` in
  `<cellar>/<name>/<version>/INSTALL_RECEIPT.json`; for a cask it is
  `source.tap` and `source.version` in
  `<caskroom>/<token>/.metadata/INSTALL_RECEIPT.json` (Fact 28).
- **Put back** otherwise: restore the previous bytes, or delete the file for
  an `.absent` entry, then delete the undo entry.

The keg decides, not brew's exit status. A brew that fails after it has
poured the new keg leaves a file that must stay, and a brew that answers
"already installed" for a keg from another tap (Fact 17) leaves one that must
go. The version is the cutoff version the view already has, in brew's
spelling (`1.0.0_2`).

**Why it matters.** The settle step is what keeps the tap invariant. A new file
left beside an old anchored keg makes every install that depends on it fail
(Fact 22). An old file put back beside a new keg does the same.

**Pruning** (Choice 12). A new function in the soak-tap module, not
`prune::prune_cache`, removes every `.rb` under the soak tap's `Formula` and
`Casks` that has no undo entry and whose name is not that of an installed
package with a receipt naming the soak tap. That clears the file of a
package the user uninstalled and the file of a package the user moved back
to core. The legacy tap is not pruned. The test is the receipt alone, not the version: a
file is never removed while an installed package of that name has a receipt
naming the tap, whether or not the versions match. Removing the file of an
anchored package would release a formula to core's current version (Fact 23)
and freeze a cask (Fact 31). This is the one place brewsoak deletes under
`Library/Taps`, and only inside its own tap.

**When brewsoak touches the tap outside an install** (Choice 16).

| Point | Recovery (settling leftover undo entries) | Pruning |
|---|---|---|
| `brewsoak update`, after its refresh | When the tap is local | When the tap is local and trusted |
| Start of a real `upgrade`, `install` or `reinstall`, before any soak-tap work | When the tap is local | When the tap is local and trusted |
| Any dry run | Never | Never |
| `outdated`, `info`, and the refresh inside `cmd::ensure_snapshots` | Never | Never |
| Tap not tapped, or tapped from a remote | Never | Never |

Both take the `&RunLock`, and recovery runs before pruning. "Local" is
`remote: null` in `tap-info`; "trusted" is as defined under "Tap name,
location and bootstrap". Recovery does not need trust: it only restores
brewsoak's own writes and decides from receipts it reads from disk, without
asking brew to load anything from the tap. Pruning waits for trust so that a
tap in a state the user has not approved is left exactly as it is.

`prune::prune_cache` keeps its signature and its contract (only paths under
`<cache>`). It is called from `cmd/update.rs` and from
`cmd::ensure_snapshots`, and the second of those also runs for `outdated`,
`info` and dry runs, which must not change the tap. Its existing tests stay
as they are; one test is added that it leaves a soak tap directory untouched.
The undo directory `<cache>/soak-tap-undo/` is not something `prune_cache`
removes.

### Commands (Choice 7)

For a soak-tap package, with its tap file written:

| Package | Command |
|---|---|
| Formula, not installed | `brew install --formula [flags] ericfitz/brewsoak/<name>` |
| Formula, installed, from any tap | `brew upgrade --formula [flags] ericfitz/brewsoak/<name>` |
| Cask, installed or not | `brew install --cask [flags] ericfitz/brewsoak/<token>` |

`brew install` refuses a formula whose keg came from another tap (Fact 15),
which is every keg on a machine today. `brew upgrade` moves it (Fact 16) and
refuses a package that is not installed (Fact 19), so the verb follows the
installed state. This applies to all three brewsoak verbs: a soaked
`brewsoak install` or `brewsoak reinstall` of an installed formula is a move
to the cutoff artifact, which is what `brew upgrade` with the cutoff file in
the tap does. User flags are filtered for the brew verb
actually run (`flags::filter_for_verb`), with the existing dropped-flag note.
The cask command is the one verified for both states (Facts 28-30).

When the tap file has the installed version (a change that left the version
alone), brew answers `already installed` and nothing is replaced, as with a
by-name install today. For an anchored package the new file is kept; for any
other it is put back.

The environment of brew children does not change.

### Dependency closure and ordering (Choices 8 and 9)

The walk (`CutoffDepWalk::visit`) keeps its shape: dependencies first, the
target last, one failing member holds the target, answers kept for the run.
What changes is where a member's dependency list comes from and what "brew
will leave it alone" means.

A soak-tap member's dependencies are listed with
`brew deps --1 --full-name --formula|--cask ericfitz/brewsoak/<name>`, after
its cutoff file is written. That is the cutoff's own list (Fact 13), and it
is also the load test: if brew cannot load the file, the member fails here.
A third-party member is listed by bare name after it passes the by-name gate,
as today.

brew installs a missing bare-named dependency from core's current file, not
from the tap (Fact 12), and upgrades an outdated one the same way. So every
soak-tap dependency that needs installing is installed from the soak tap, by
full token, before its dependent.

| Member (soak-tap origin) | Rule |
|---|---|
| The target | Tap file written; installed or upgraded last |
| A missing dependency | Must be eligible, as today; tap file written; `brew install` before its dependents |
| An installed formula dependency this run upgrades to its cutoff | Tap file written; `brew upgrade` before its dependents. It is anchored afterwards |
| An installed formula dependency this run does not upgrade | Passes when brew's own dependency test says it is satisfied. If not, and its receipt names another tap or none: it passes only when brew's current file is byte-identical to its cutoff (the by-name gate: brew would then install the cutoff itself); otherwise the target is held. If not, and it has a legacy receipt: see below |
| An installed cask dependency | Left alone, as today |
| A no-soak or unsoakable dependency; a dependency in a held tap or with no cutoff | As today |

"brew's own dependency test" is `Dependency.new(name).installed?`, asked in
the existing `brew ruby` loader script (`BY_NAME_RUBY`) as one more column.
It replaces the script's `Formulary.factory(name).latest_version_installed?`
for this purpose, which is wrong for an anchored package: it looks at core's
current version, while brew's installer looks at the tap file (Fact 20). The
test is true for an anchored dependency, whatever core has moved to, and for
a dependency from any tap that is at brew's current version. The walk still
goes through a satisfied dependency to its own dependencies (Source fact 2).

The held case is an installed dependency that sits at its cutoff, or ahead of
it, while brew has a newer, unsoaked version, and whose keg is not anchored.
brew might replace it with that version. brewsoak cannot tell whether brew
would, because that depends on the bottle minimums of the dependent, so it
holds, as the stopgap does. Unlike the stopgap this ends by itself: once the
dependency's newer version has soaked, brewsoak upgrades it from the soak tap
and it is anchored from then on. Each dependency can cause this at most until
its next soaked upgrade. Reinstalling the dependency from the tap would
anchor it at once (Fact 18); this design does not do that (Human
decision 6).

A dependency with a legacy receipt (Choice 17) is treated like any other
dependency that is not anchored, and the missing tap file is never by itself
a reason to hold its dependents. In the usual cases nothing special happens:
at brew's current version it is satisfied; with a newer cutoff it is upgraded
from the tap first and is anchored afterwards (Fact 16). Two details follow
from the evidence:

- When brew's test says it is not satisfied, its receipt names the soak tap
  and its cutoff has the installed version, brewsoak writes the cutoff file.
  The receipt already names the tap, so the package is anchored from that
  moment and passes. The settle step keeps the file. This cannot help a
  receipt that names the legacy tap, because brewsoak writes nothing there;
  such a dependency falls under the next detail.
- When it is not satisfied, is not upgraded this run and its cutoff is
  another version (it is ahead of its cutoff, pinned or refused), the target
  is held, and the by-name gate is not consulted: brew cannot replace a keg
  whose receipt names another tap as a dependency, whatever version it would
  choose (Fact 23).

A third-party member keeps the 1.6.1 rules. A third-party target's core
dependencies follow the table above, so they no longer have to pass the
by-name gate to be installed.

### What happens to the stopgap gate (Choice 10)

- Removed for soak-tap packages: the checksum comparison, the requirement
  that brew's current file be the cutoff, and every hold that said so.
- Kept for third-party packages: `Brew::by_name`, `NameQuery`, the SHA-256 of
  the raw cutoff blob, the tap-resolution check, and the untrusted-tap and
  ambiguous-name holds.
- Kept for everything: the `brew ruby` loader script, which now also answers
  the dependency test, and the run's one `brew update`.
- `NameWhy::waits_on_issue_17` goes away. No message names issue #17.

### Holds and messages

Never fatal. The shapes below replace the stopgap's
`...; brewsoak cannot install a soaked copy until issue #17 is fixed; ...`.
`<bypass>` is described under "Bypassing brewsoak".

Soak-tap packages:

- tap not ready: see "Trust".
- brew cannot load the soaked copy, or cannot list its dependencies:
  `<name>: [dependency <dep>: ]brew could not load its soaked copy (<first
  line of brew's error>); <bypass>`
- an installed dependency brew might replace:
  `<name>: dependency <dep>: brew would replace installed <dep> <installed>
  with <brew's version>, which has not soaked; <name> installs once it has;
  <bypass>`
- an installed dependency with a legacy receipt that brew cannot replace
  (the second detail under "Dependency closure and ordering"):
  `<name>: dependency <dep>: brew cannot replace installed <dep> <installed>
  as a dependency; run brew upgrade <dep> first; <bypass>`
  (a bare `brew upgrade` works for such a formula, Fact 23). There is no
  hold that says the tap has no file.
- a package installed from an untrusted soak tap, as a target or as a
  dependency: the tap-not-ready hold of "Trust".
- on a dry run only, a leftover undo entry: see "Re-running".
- a failed install or upgrade is brew's failure for that package, as today.
- a dependency that did not end up installed from the soak tap (2026-11-27
  note, added after the build): the target is held with
  `held <target>: its dependency <dep> was not installed from brewsoak's tap`
  and the note `<target>: held: its dependency <dep> was not installed from
  brewsoak's tap (<reason>)`. The reason is brew's first error line when the
  dependency's command failed; `brew left the installed version in place` when
  brew answered that it is already installed; and otherwise `brew ran, but its
  keg is not from brewsoak's tap at the cutoff version`. The hold exists
  because brew would otherwise pull the dependency from core's current file
  when it installs the target. The README lists it too.

Third-party packages (the by-name gate), same reasons as 1.6.1 with a new
tail:

- `brew would install <version>, which is not the soaked cutoff; it installs
  once that change soaks`
- `brew's <version> formula|cask changed since the soaked cutoff; it
  installs once the change soaks` (unchanged, 2026-11-27 human decision)
- `brew resolves the name to <tap>, not <origin>; brewsoak cannot install it
  by name`
- `brew could not describe it by name (...)`, `brew could not read its
  dependencies (...)`, `it has no soaked cutoff yet` (unchanged reasons)

The untrusted third-party tap hold, the ambiguous-name hold and the held-tap
refusal keep their text.

### Bypassing brewsoak (Choice 11)

A plain `brew upgrade <name>` does nothing for an anchored package (Fact 20),
and `brew install homebrew/core/<name>` is refused for it (Fact 15). So the
hint in a refusal or hold depends on where the installed package came from:

| Package | Hint |
|---|---|
| Not installed | `brew install <token>` as today |
| Installed, receipt does not name the soak tap | `brew <verb> <token>` as today |
| Formula with a legacy receipt | `brew <verb> <token>` as today: a `brewsoakr/soaked` receipt gets the hint it gets now, and the bare name of a soak-tap receipt without its file resolves to core (Fact 23) |
| Formula installed from the soak tap (anchored), `upgrade` or `install` | `brew upgrade homebrew/core/<name>` (Fact 24) |
| Formula installed from the soak tap (anchored), `reinstall` | `brew reinstall homebrew/core/<name>` (Fact 18) |
| Cask whose receipt names the soak tap, anchored or not | `brew upgrade --cask homebrew/cask/<token>` (Facts 32, 41) |

The cask row does not follow the legacy rule, because the evidence does not
allow it: a cask whose tap file is gone is resolved from its installed copy,
so the bare token does nothing for it (Facts 31, 41).

A `homebrew/core/<name>` token records no trust (Source fact 6). After such a
bypass the package's receipt names core again and brewsoak treats it like any
other package from core. The formula forms were run for real; the cask
upgrade form only as a dry run, and a cask reinstall form
(`brew reinstall --cask homebrew/cask/<token>`) not at all, so the builder
confirms both cask forms before they go into a hint.

### Receipts, inventory and origin

- A receipt that names the soak tap maps to "no receipt tap", exactly as one
  that names `brewsoakr/soaked` does today (`brew.rs`), so the origin comes from the
  origin record and then the
  default, `homebrew/core` or `homebrew/cask`. That default is right, because
  only core and cask packages are installed from the soak tap. After a real
  install from the soak tap brewsoak removes the package's `origins.toml`
  record (`forget_origin`), as it does after a by-name install today, so a
  stale third-party record cannot mislead.
- The inventory needs two new facts per installed package. The first is
  whether its receipt names the soak tap: for a formula, `source.tap` in the
  keg receipt; for a cask, `tap` in `brew info --json=v2 --installed`
  (Fact 28). The second is whether it is anchored: the first fact, plus a
  file in the soak tap for that name whose version, read with the existing
  identity parser, is the installed version. Both come from disk and from
  the JSON brewsoak already has; neither asks brew anything new. A tap file
  that does not parse means not anchored. "Anchored" drives the formula
  bypass hints, the no-soak tokens and the dependency rule. The receipt
  alone drives the prune, the cask hints and the untrusted-tap hold.
- A formula that brew leaves out of its JSON because the soak tap is
  untrusted is added from disk (see "An untrusted tap that already has
  packages").
- `TapClass::Staging` stays for the legacy tap, and the soak tap is
  classified the same way (the builder decides whether it shares the class
  or gets its own): never fetched, never soakable and never an origin. An
  explicit `ericfitz/brewsoak/<name>` or `brewsoakr/soaked/<name>` on the command
  line is refused with a line that says to name the package without the tap.
  A `NO_SOAK` or `[[TAP]]` entry that names the soak tap is handled as one
  that names `brewsoakr/soaked` is handled today.
- `<cache>/staging/` is still read for kegs installed by path before 1.6.1
  and still never written.

### The no-soak step and the `reinstall` true repair

- **No-soak tokens.** A no-soak formula that is installed from the soak tap
  (anchored: it was soaked before it was added to `NO_SOAK`) goes to brew as
  `homebrew/core/<name>`, and a no-soak cask whose receipt names the soak
  tap as `homebrew/cask/<token>`: the same split as the bypass hints. A bare
  name would follow the tap file and never upgrade (Fact 20); the full name
  upgrades it from core and the receipt then names core (Fact 24). A formula
  with a legacy receipt keeps its bare name, which brew resolves to core
  (Fact 23). The existing tap switch for a third-party no-soak package
  staged before 1.6.1 is unchanged.
  `brewsoak outdated` takes its no-soak list from `brew outdated`, which does
  not list such a package until it has been upgraded once this way.
- **No-soak dependencies.** brew no longer upgrades an anchored dependency of
  a no-soak package to brew's latest (Source fact 1). The dependency stays at
  its soaked version until brewsoak upgrades it. Human decision 4 accepts
  this and amends the 2026-10-05 "accepted consequence": soaking wins for
  the dependency. Until the dependency's update has soaked, the no-soak
  package can run against an older dependency than its bottle was built
  with. brewsoak builds nothing to prevent that; the README states it, and
  the 2026-10-05 spec gets a dated note.
- **True repair.** Unchanged: `brew reinstall <token>` with the token brewsoak
  uses today. For an anchored package the bare name is expected to resolve
  to its tap file (Source fact 8, not run), which has the installed version,
  so the package would be reinstalled in place and stay anchored. The
  builder confirms this with a real run; if brew reinstalls from core
  instead, that is a design change to raise, not to absorb.

### Interaction with plain `brew`

- Plain `brew upgrade` and `brew outdated` leave an anchored package alone.
  A user who runs `brew upgrade` by habit no longer pulls unsoaked versions
  of the packages brewsoak manages. Packages not yet moved (receipt `null` or
  core) are upgraded by plain brew as today.
- A plain `brew install <core package>` leaves its anchored dependencies at
  their soaked versions and installs missing ones from core.
- `brew untap ericfitz/brewsoak` is refused while packages are installed from
  it (Fact 25).
- `brew update` ignores the soak tap (Source fact 3).
- With `HOMEBREW_VERIFY_ATTESTATIONS` set, brew warns and skips the
  attestation check for bottles poured through the soak tap (Fact 33).
- While the soak tap exists and is untrusted, brew prints its untrusted-taps
  warning on the user's own installs (Fact 7).
- If the user untrusts the soak tap while packages are installed from it,
  brew drops their per-package trust too (Fact 36), hides those formulae
  from `brew info --installed` (Fact 37) and refuses plain installs that
  depend on them (Fact 39), until the tap is trusted again or the packages
  are moved back to core (Fact 40).

These go into the README with the trust step.

### Casks

Casks use the same tap, the same file rules and the same settle step, with
`Casks/<token>.rb` and `brew install --cask ericfitz/brewsoak/<token>`. A cask
moves from `homebrew/cask` to the soak tap with no refusal (Fact 30, a
simulation; the builder confirms it with a real cask). The existing cask rules
do not change: comparison by version when the receipt has no sha or url,
`auto_updates` casks left to the app on a bare run, `--greedy` handling,
language blocks. A cask's formula dependencies follow the dependency table.

### `brew update`

Unchanged from the 2026-11-27 decision. brew's own data still matters: it
decides whether an installed dependency that is not anchored is at brew's
current version, and the by-name gate for third-party packages reads it.

### Dry run (Choice 13)

A dry run makes every decision a real run makes, in the same order. It writes
the same tap files, asks the same `brew deps` questions, forwards `--dry-run`
on the same install and upgrade commands, and then puts every file back: on a
dry run the settle step always restores. It does not create the tap, recover
leftover undo entries or prune (Choice 16).

One hold is a dry run's alone: a target that needs a tap file with an undo
entry left by an interrupted run is held (see "Re-running"), where a real run
would first recover and then go on. The dry run says so instead of changing
the tap.

Two things differ from the rule that a dry run changes nothing outside the
cache (commit `743c5db`). Tap files change for the length of the run. And
brew records per-package trust for a dry-run install (Fact 5), which Human
decision 2 accepts. Both are stated in the README.

brew's own dry run is not a full preview, in two ways.

- It does not predict the failure in Fact 22. brewsoak's dependency rule
  prevents that state before brew is asked.
- On a real run brewsoak has installed the dependencies from the soak tap
  before it asks brew for the target. On a dry run it has not, so brew's
  preview of the target lists those dependencies itself, at core's current
  versions (`Would install 1 dependency for <target>:` then `hello 2.12.3`
  in the spike, where a real run would have installed the tap's file first).
  Those versions may be unsoaked ones that a real run never installs. So on
  a dry run of a soak-tap target the summarizer keeps brew's
  `Would install|upgrade N dependenc(y|ies) for <target>:` block out of the
  terminal (it still goes to the log). The dependencies are reported by
  brewsoak's own lines and by the dry run of each dependency's own command
  from the tap, which names the cutoff version.

### Output (Choice 14)

brew now prints `ericfitz/brewsoak/<name>` and `==> Trusted formula ...` lines.
The summarizer shows the bare name and sends the `Trusted` line to the log
only. Every byte still reaches the log, and `--raw` still turns the
summarizer off. `apply::already_installed_line` and
`quiet::installed_from_output` must recognize the full-name forms.

### Migration

Nothing is migrated up front (Human decision 6).

| Installed package | What happens |
|---|---|
| Receipt tap `null`, installed by path before 1.6.1 (101 kegs on the maintainer's machine) | Its next soaked upgrade runs `brew upgrade ericfitz/brewsoak/<name>` and anchors it (Fact 16) |
| Receipt `homebrew/core` or `homebrew/cask`, installed by 1.6.1 by name or by plain brew | The same |
| Third-party package staged before 1.6.1 (`cats`, `cyclonedx-gomod`) | Unchanged: the by-name gate, or the existing hold |
| The legacy tap `brewsoakr/soaked` and its pre-1.0 files | Left alone; brewsoak never writes or prunes it. No keg refers to it on the maintainer's machine (Fact 34), so `brew untap brewsoakr/soaked` removes it |
| A keg from a pre-1.0 build whose receipt names `brewsoakr/soaked` (none on the maintainer's machine, Fact 34; no released build made one) | A legacy receipt: classified as today, bypass hint as today, anchored to the soak tap by its next soaked upgrade. That upgrade crosses from one non-core tap to another, which the spike did not run (Fact 16 covers core, `null` and same-tap receipts) |
| A soak-tap receipt whose tap file is gone | A legacy receipt: anchored again by its next soaked upgrade, or when the walk writes its cutoff file |
| `brewsoakr/soaked/*` entries already in the trust store | Left alone |
| The soak tap `ericfitz/brewsoak` | Created by the first real run that needs it; held until `brew trust ericfitz/brewsoak` |
| `<cache>/staging/` | Left alone; pruned for untapped taps as today |

Until a dependency has had its first soaked upgrade it is not anchored, and
the "brew might replace it" hold can apply to its dependents.

### Code map

| Area | Change |
|---|---|
| `src/origin.rs`, `src/taps.rs` | a constant for `ericfitz/brewsoak` beside `STAGING_TAP`; both names classified before the remote rule; `TapInfo` gains `path` and `trusted` |
| `src/tap.rs` | `write_blob` and `sanitize_unofficial` are used again; command builder chooses `install` or `upgrade` and takes the full token |
| new module | soak-tap state, undo entries, settle, recovery |
| `src/brew.rs` | `tap-new` call; receipt check for the settle step; loader script gains the dependency-test column; installed packages carry the two soak-tap facts; formulae brew left out are added from the Cellar |
| `src/cmd/deps.rs` | per-origin member rules; hold reasons |
| `src/cmd/apply.rs` | tap readiness check; install through the tap; settle after each target; bypass hints |
| `src/nosoak.rs` | full official token for a package installed from the soak tap |
| `src/prune.rs` | `prune_cache` unchanged; one test that it leaves the soak tap alone. The soak-tap prune lives in the new module |
| `src/quiet.rs` | full-name forms; `Trusted` lines |
| `src/lib.rs` | `World::tap_root` returns the soak tap's directory |
| `AGENTS.md`, `README.md`, older specs | rules and supersession notes (see "What is left to the builder") |

`taps::staging_root` is not used by this design (the layout is flat) and
nothing else calls it; the builder removes it. `taps::staged_load_failure` is
not needed either, because any brew error while loading a soaked copy is the
same hold; the builder removes it unless the plan finds a use.

## Re-running

**A second run.** A package upgraded by the first run is `NoOpAlreadySoaked`.
Its tap file already has the wanted bytes, so nothing is written and no undo
entry is made. A held package is evaluated again from scratch. `tap-new` is
not called again, because the tap is listed. Pruning finds nothing. Item
trust entries are already present, and brew does not add them twice.

**Two runs at once.** `upgrade`, `install`, `reinstall` and `update` hold
`<cache>/run.lock` from the start of dispatch, so two brewsoak runs never
write the tap or the undo directory together. Every tap write, settle and
prune takes a `&RunLock`. A plain `brew` running beside brewsoak is not
excluded: between a tap write and its settle, a brew install that depends on
that package could fail as in Fact 22. brew's own locks protect the kegs.
Nothing unsoaked can be installed this way.

> **2026-11-27 note (observation, after the build).** The sentence "brew's own
> locks protect the kegs" did not hold in one observed overlap. brewsoak's
> start-of-run prune removed a tap file during a plain `brew upgrade`, and the
> keg was then lost. No mechanism is claimed. The README now warns not to run
> brewsoak and a plain `brew upgrade` or `brew install` at the same time.

**Leftovers from a failed run.** Undo entries are the only new persistent
state. At the points and under the conditions of Choice 16 (`brewsoak update`
and the start of a real `upgrade`, `install` or `reinstall`, tap local),
brewsoak settles each entry it finds by the same rule as a normal settle:
keep the new file when the package is installed at that file's version from
the soak tap, otherwise put the old one back. A dry run does not recover. If
it needs a file that still has an undo entry from an earlier run, it holds
that target with `<name>: an earlier brewsoak run was interrupted; run
brewsoak update to finish it`, and never overwrites or settles that entry. The file's version is read with the existing
identity parser; a file that does not parse is put back. Settling is
idempotent, and a crash while settling leaves entries that the next run
settles again. Until that next run, the worst state is a new file beside an
old anchored keg: plain brew would upgrade that package from the tap file,
which is a soaked cutoff file, and installs that depend on it would fail with
brew's error. A tap that was created and never trusted is found again by the
next run. A half-finished brew install is brew's to report, as today.

A tap file with no undo entry and no installed package of that name whose
receipt names the tap can only come from outside brewsoak; the next prune
removes it.

**The untrusted state.** Entering and leaving it changes nothing on disk that
brewsoak owns. A run in that state holds the affected packages, a second run
holds them again, and the first run after `brew trust ericfitz/brewsoak` sees
them through brew's JSON and carries on. Pruning is skipped throughout, so no
tap file is lost while the tap is untrusted.

## Testing

Unit tests with `MockBrew`, `InMemoryGit` and a temp directory as the soak
tap. `MockBrew` gains: `path` and `trusted` on its tap entries, a recorded
`tap-new`, a settable answer for the receipt check, and a hook that runs when
a command is recorded, so a test can assert what the tap held at the moment
brew was called.

- Tap state: not tapped (real run creates it, then holds as untrusted; dry
  run creates nothing), tapped from a remote, untrusted, trusted, no
  `trusted` field, `tap-new` failure. In each hold case a second package of
  another class still runs, the run exits 1, and the note appears once.
- No recorded command is ever `trust` or `untrust`, and no child environment
  has `HOMEBREW_NO_REQUIRE_TAP_TRUST` (existing pins stay).
- File content: byte-exact without `no_autobump!`; the stanza removed with it;
  formula and cask paths.
- Verb: install when absent, upgrade when installed, for each of
  `brewsoak upgrade`, `install`, `reinstall`; cask always install; flags
  filtered for the verb run.
- Tokens: every soak-tap command carries `ericfitz/brewsoak/<name>`; no
  third-party file is ever written to the tap and no third-party command
  carries the soak tap's name; no argument is a path; no command, write or
  prune ever names `brewsoakr/soaked`.
- Names: a cloned `ericfitz/tap` beside the local `ericfitz/brewsoak` keeps
  its class and its packages keep their origin; `ericfitz/brewsoak` tapped
  from a remote is held and never used; an explicit
  `ericfitz/brewsoak/<name>` on the command line is refused.
- Closure: a missing dependency and an upgraded one are installed from the
  tap before the target; a satisfied dependency is left and walked through;
  each row of the dependency table; a third-party target with core
  dependencies.
- Settle: kept after a real install; put back after a brew failure with no
  new keg, after "already installed" over a keg from another tap, after a
  hold, and after a dry run; kept when brew fails but the new keg exists.
- Recovery: an undo entry with a matching keg, with a non-matching keg, with
  an `.absent` marker, with an unparseable file; settling twice.
- Prune: removes stale files; never removes a file while an
  installed package of that name has a receipt naming the tap (matching
  version or not) or while it has an undo entry; touches nothing outside
  `Formula` and `Casks`.
- When the tap is touched: prune and recovery run in `brewsoak update` and
  at the start of a real `upgrade`, `install` and `reinstall`; neither runs
  in a dry run, in `outdated`, in `info` or through `ensure_snapshots`;
  recovery runs and prune does not when the tap is local but untrusted;
  neither runs when the tap is absent or tapped from a remote.
  `prune_cache` leaves a soak tap directory byte for byte as it was.
- A dry run that meets a leftover undo entry holds that target and leaves
  the entry and the tap file alone.
- Untrusted tap with packages installed from it: brew's JSON lacks the
  formula, the Cellar has it with a receipt naming the tap. It is in the
  inventory, `upgrade`, `install <name>` and `reinstall` hold it and never
  run `brew install` for it, `outdated` and `info` list it with the note, a
  second package still installs, a third-party target that depends on it is
  held with the tap-not-ready note, and the run is not fatal. A keg brew did
  not list whose receipt names another tap is not added.
- Legacy receipt, both kinds (the receipt names `brewsoakr/soaked`; the receipt
  names the soak tap with no tap file at the installed version): the package
  is not anchored; a `brewsoakr/soaked` receipt is never anchored, whatever either tap
  holds, and the walk writes no file for it; its bypass hint and no-soak token
  use the bare name; a dependent is not held when brew's test says it is
  satisfied; the walk writes the cutoff file and passes when the cutoff has
  the installed version (soak-tap receipt only); the "brew cannot replace"
  hold appears only for a dependency that is not satisfied and not upgraded
  this run; no output says the tap has no file. The same receipt
  with a tap file at the installed version is anchored. A cask whose receipt
  names the tap gets the `homebrew/cask/<token>` hint either way.
- Hold texts, bypass hints for each row of the bypass table, no-soak tokens.
- Summarizer: golden output with full-name lines and `Trusted` lines; on a
  dry run, brew's dependency-preview block for a soak-tap target stays out of
  the terminal and in the log, and each dependency appears once, at its
  cutoff version.
- A `trusted` value that is not a boolean, and a receipt whose `source.tap`
  is not a string or null, are treated as "not trusted" and "not from the
  soak tap": rejected, not guessed.

Real brew, by hand, before release (the builder states the commands and their
output in the plan's final task):

- the three items under "Not verified" that the design depends on;
- on the maintainer's machine, where the soak tap does not exist yet: a
  first real run creates `ericfitz/brewsoak` and holds with the trust note;
  the maintainer runs `brew trust ericfitz/brewsoak`;
- `brewsoak upgrade --dry-run ast-grep`, then, with the maintainer's
  approval, a real `brewsoak upgrade ast-grep`: the receipt names
  `ericfitz/brewsoak`, `brew outdated` no longer lists it, and no undo entry
  remains.

Done gate: `cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
cargo build && cargo test`.

## What is left to the builder

- The implementation plan and task split.
- Exact names: the new module, the `MockBrew` hook, the inventory flag, the
  undo directory.
- How the settle step reads receipts (a new `Brew` method or an existing
  one).
- The summarizer rules for the new lines.
- Verifying, with a real cask and before relying on them: `brew deps` with a
  soak-tap cask token; a cask moving from a real `homebrew/cask` receipt; the
  cask bypass commands.
- Verifying that a bare `brew reinstall <name>` of an anchored formula
  reinstalls from the tap file and leaves the receipt naming the soak tap.
- Checking how `no_autobump!` appears across current core and cask files, and
  holding a package whose copy does not load.
- AGENTS.md: replace the "Installing soaked formulae" stopgap rules with this
  design's; scope the `Library/Taps` rule to allow brewsoak's own tap; add
  that `ericfitz/homebrew-brewsoak` must never be created on GitHub. The
  sentence that `brewsoakr/soaked` survives only as `origin::STAGING_TAP` stays true.
- README: the minimum Homebrew (7.0.14) and the one build this was verified
  on; the one-time `brew trust ericfitz/brewsoak`; the effect on plain
  `brew upgrade`; the bypass commands; the trust entries; attestations; what
  untrusting the tap does and the two ways out; that a no-soak package's
  dependencies stay at their soaked versions (Human decision 4); that a
  leftover `brewsoakr/soaked` tap can be removed with `brew untap`.
- Where the pin state of a formula added from the Cellar could be read, if
  the plan finds it is needed after all.
- Dated supersession notes in the 2026-08-12, 2026-10-05 and 2026-11-26
  specs, including the amendment of the 2026-10-05 "accepted consequence"
  (Human decision 4).
- Confirming with a real run that `brew upgrade ericfitz/brewsoak/<name>`
  moves a keg whose receipt names another non-core tap, before the
  legacy-tap migration row is relied on.

Any gap found while building is raised as a design change, not filled by a
guess.

## Questions answered by the maintainer (2026-11-27)

The first draft of this spec asked four questions. All four are resolved, and
the answers are recorded as Human decisions 3 to 6. No question is open.

| Question | Options offered | Answer |
|---|---|---|
| 1. Third-party tap packages | A: keep the by-name gate, name collisions stay held. B: A, plus name-collision packages through the soak tap. C: all third-party packages through the soak tap | A (Human decision 3) |
| 2. Dependencies of no-soak packages | A: soaking wins for the dependency; document it. B: move such dependencies to core before the no-soak step | A (Human decision 4) |
| 3. The tap name, given that `brewsoakr` is unregistered on GitHub | A: keep `brewsoakr/soaked` and register the name. B: rename to a namespace the maintainer owns. C: accept the risk | B: `ericfitz/brewsoak` (Human decision 5) |
| 4. Moving installed packages into the tap | A: at their next soaked upgrade. B: reinstall a dependency instead of holding its dependent. C: a one-time reinstall of everything | A (Human decision 6) |

Tracked elsewhere: issue #22 (formulae from an untrusted third-party tap are
missing from brewsoak's inventory, Fact 37) and issue #18.
