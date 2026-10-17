# Follow-ups from the v1.4.0 review (PR #6)

The final review of PR #6 left five notes open. This spec covers all five. It
does not cover the older follow-ups listed in HANDOFF.

## 1. Block depth tracking (prerequisite for items 4 and 5)

`identity::depth1_lines` treats only `do` blocks and `class`/`module`/`cask`
as openers. It treats every `end` as a closer, so the `end` of an `if`,
`unless`, `case`, `begin` or `def` closes the wrong entry. In `cyclonedx-cli.rb`
this lets the `if Hardware::CPU.arm?` and `on_linux` bodies leak to depth 1.
Today's parse finds the right sha256 only by accident.

Fix: a line that starts with `if `, `unless `, `case `, `begin`, `while `,
`until ` or `def ` also opens a block. Two kinds of line don't open one:

- a one-liner that closes on the same line (ends with ` end`);
- an endless `def name(...) = expr`.

A modifier `x if y` doesn't start with `if`, so it opens nothing.

Consequence: when the host is unknown, `cyclonedx-cli` (all urls and sha256s
inside `on_macos`/`on_linux`) becomes unparseable and is reported as such.
That's correct behavior replacing an accident, and it matches how casks
behave with an unknown host.

## 2. Formula platform blocks are resolved against the host (review note 4)

`parse_formula` takes the host, like `parse_cask`. With a known host:

- `on_arm`, `on_intel`, `on_macos` and `on_<codename>` blocks that apply are
  transparent, as for casks. Nested blocks also resolve, as in heroku's
  `on_macos do on_arm do`.
- An `if` whose condition is built only from `Hardware::CPU.arm?`,
  `Hardware::CPU.intel?`, `Hardware::CPU.is_64_bit?`, `OS.mac?`, `OS.linux?`,
  `!` and `&&` is transparent when the condition holds on the host. Each
  `elsif` in such a chain is evaluated the same way while no earlier branch
  has held; endava/tap `cats` keeps its arm url in one. `else` is
  transparent when no branch held. An unrecognized condition is opaque, and
  so is the rest of its chain.
- In Ruby, the later definition wins. For a formula, `version`, `url` and
  `sha256` are the last applicable definitions before `bottle do`. heroku
  declares the x64 url first and the arm override after it.
- Casks keep first-match. Their fixtures (ngrok, wireshark-app) define each
  field once per platform.

With an unknown host, formulae parse as before item 1, with blocks opaque
and first-match.

## 3. Lifecycle flags honor platform blocks (review note 3)

`deprecate!`/`disable!` inside a block that doesn't apply to the host
(`on_linux`, `on_intel` on arm) no longer refuses the package or warns
about it. With a known host, flags are read from the lines `depth1_lines`
returns for that host. With an unknown host, every line is still scanned.
Refusing is the conservative failure.

The same all-lines scan in `keyword_quoted` (`tag:`/`revision:` for git
urls) is listed here and left alone. A git-url formula with per-platform
tags hasn't been seen.

## 4. Self-updating casks newer than the cutoff (review note 2)

A bare run without `--greedy`/`--greedy-auto-updates` leaves every
installed `auto_updates` cask to the app, as brew does. `bare_action`
therefore also turns `LeaveNewerThanCutoff` and `HoldUnordered` into
`LeaveAutoUpdates`. Before, such a cask was counted "ahead" with a note
about a re-pointed formula, or held. `LeaveAheadOfSoak` (installed ==
HEAD) is unchanged. `outdated` prints the auto-updates entry with the
right comparison (`<`, `>` or `?`). The newer-than-cutoff note no longer
says "formula".

## 5. `reinstall` upgrades outdated dependencies (review note 1). Decision by Claude, approved by Eric 2026-10-17

Kept as is. A brewsoak `reinstall` of a package behind its cutoff installs
the cutoff, which is an upgrade. It takes the same path as `upgrade`, and it
has the same linkage hazard that the 2026-10-16 decision (made by Eric)
addressed. So installed, soaked, unpinned formula dependencies whose own
action is `InstallCutoff` are upgraded to their cutoffs first, announced
and counted.

This is more aggressive than brew. `Dependency#installed?` upgrades an
installed dependency only when it isn't at the latest version and the
bottle's recorded minimum version isn't met; brewsoak doesn't read
bottle minimums. The true-repair branch (installed == HEAD, plain
`brew reinstall`) doesn't walk dependencies.

## 6. Cask dependencies are not reordered (review note 5). Decision by Claude, approved by Eric 2026-10-17

Kept as is. An installed cask dependency isn't upgraded ahead of its
dependent, for two reasons:

- Casks don't link against each other, so order can't break the
  dependent. brew treats any installed version of a cask dependency as
  satisfied.
- Walking it early would upgrade a self-updating cask that a non-greedy
  bare run leaves to the app.

The dependency keeps its own plan entry. Installed formula dependencies of a
cask are still walked.

## 7. Formula versions derived by brew (decision by Eric, 2026-10-17)

A corpus scan found ~550 core formulae whose url-derived version differs from
Homebrew's (`sf_19` vs `19`, `download` vs `8.14.18`, `9.10.3-src` vs `9.10.3`).
The version feeds brew's version ordering (`settle_order`) and every
version shown. Eric chose to ask brew rather than port more of
`Version.parse`.

- An explicit `version` stanza is still read from the file.
- When the version is derived (no `version` stanza), `parse_formula` records
  the url and git `tag:` it derived it from. brewsoak asks brew for
  `Version.detect(url, tag:)` in one batched `brew ruby` call, memoized for
  the run (failures too), like `compare_versions`. Bare runs prefetch every
  derived version they will parse in one call, before ordering.
- If brew can't answer (a failed call, an empty result, or a url with `#{`
  interpolation), the local heuristic version is kept. The package parses
  exactly as before, and both sides of a comparison still use the same rule.

## 8. Cask `language` blocks (decision by Eric, 2026-10-17: fix before the PR)

29 casks keep `version`, `sha256` or `url` inside `language "xx", default: true do`
blocks, which stayed opaque. cave-story was unparseable, and firefox and
thunderbird had no sha256. With a known host, brewsoak now selects one
language block per cask, as Homebrew's `Cask::DSL#language_eval` does, and
reads that block as transparent. The other language blocks stay opaque.

- Preferred languages come from brew. `brew::HOST_PLATFORM_RUBY` also prints
  `Cask::Config.new.languages`, the list a cask load uses, on a `LANG` line.
  An installed cask is evaluated with the languages in its saved
  `<caskroom>/<token>/.metadata/config.json` (`explicit`, then `env`, then
  `default`, deduplicated, as `Cask::Config#languages` orders them). That
  list applies to all three sides of that cask, matching what `brew upgrade`
  would pick.
- Selection follows Homebrew's `Locale` rules (`locale.rb`):
  - Each preferred language is taken in order.
  - The first block with a locale `eql?` to it wins; failing that, the first
    block whose locale it `include?`s.
  - If no preferred language matches, the `default: true` block is used.
  - Locales that don't parse are skipped.
- With an unknown host, or without a `default: true` block, language blocks
  stay opaque, as before.

## Re-running

No persistent state is added. Classification and parsing are recomputed each
run.
