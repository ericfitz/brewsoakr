# Sectioned upgrade output

Approved by Eric in chat on 2026-10-21 (mockup below). Applies to `upgrade`,
`install`, `reinstall` and the bare run (all go through `cmd/apply.rs`), and to
the fetch and brew-update lines that `update` shares.

## Why

The current output runs together: bare `fetching …` lines, an `upgrading M of N`
line, `no-soak:` lines with a run-on `brew upgrade a b c d` command, a run-on
counts line and a run-on `still soaking` note. Nothing says whether brew
actually upgraded the no-soak packages, and brew's `Warning: X V is available
and more recent than version C.` contradicts brewsoak when it deliberately
installs the soaked version C.

## Target output (approved mockup)

```
Fetching Homebrew history
  Homebrew/homebrew-core
  Homebrew/homebrew-cask
Fetching tap history
  anthropics/tap
  cyclonedx/cyclonedx
  endava/tap
  hashicorp/tap
Upgrading soaked packages (2 of 179)
  [1/2] upgrading cats 15.0.0 -> 15.0.3
    installing cats 15.0.3
  [2/2] upgrading certifi 2026.10.18 -> 2026.10.19
    installing certifi 2026.10.19
Updating Homebrew
  Already up-to-date.
Upgrading no-soak packages (brew decides)
  upgrading ericfitz/tap/agentbus 1.15.0 -> 1.16.0
    installing agentbus 1.16.0
  ericfitz/tap/brewsoak 1.5.2
    Already up-to-date.
  ericfitz/tap/sqdist 0.6.0
    Already up-to-date.
  ericfitz/tap/tmi-mcp 1.0.3
    Already up-to-date.
Summary
  upgraded           2
  already soaked   162
  soaking            8
  held               0
  ahead              1
  pinned             0
  skipped            0
  no-soak            4  (1 upgraded, 3 already up-to-date)
  auto-updates       2
  installed     31.2MB
  freed         28.5MB
Notes
  Still soaking (left at the cutoff):
    awscli 2.38.3
    c-ares 1.34.9
  Ahead of soak (installed artifact left unchanged):
    uv
  Held:
    foo: too new (born inside the soak window); use `brew upgrade foo` to bypass brewsoak.
Full brew log: /var/folders/.../brewsoak-67325.log
```

## Rules (Eric's decisions)

- Section header lines at the left margin; everything under one indented two
  spaces; already-indented lines (e.g. `installing`) go one level deeper.
- A section with nothing in it is not printed, except the soaked section of a
  bare `upgrade`, which always prints with `(N of M)` (also when N is 0). Named
  runs print the soaked header without a count.
- Dry run: headers start with `Would <verb>` instead of `<Verb>ing`.
- brew's own `Updating Homebrew...` line is dropped; the section header replaces it.
- No-soak section: no count in the header; one result per package, in the same
  `upgrading X a -> b` form as the soaked section (no `[i/N]` counter), or the
  package's token and version with an indented `Already up-to-date.` line.
- Notes: one package per line, grouped under a sub-heading per kind.
- Summary: one count per line, values right-aligned in one column, zero rows kept.
- Drop `Warning: X V is available and more recent than version C.` only when X
  is the package this brew command installs and C is the version brewsoak is
  installing for it. Other occurrences (e.g. dependencies) still show. The raw
  brew log is unchanged.

## Calls made by Claude (not discussed; flagged to Eric)

- No-soak results stream live in brew's order (brew prints its
  already-installed warnings before its upgrades), not re-sorted into target order.
- A no-soak package brew says nothing about gets an indented
  `no result from brew; see the full log` line, and counts as `no result` in the
  summary's no-soak split.
- Tap-switch reinstalls show as `reinstalling <token> (moving staged keg to its tap)`
  inside the no-soak section; the `no-soak: brew …` command echo lines are gone.
- No-soak dry run shows brew's `Would upgrade` list as `would upgrade X a -> b` lines.
- Zero-byte sizes print as `0B`.
- A tap whose fetch fails is still reported in Notes (`Held:`), not under the fetch header.
- Caveats brew printed become a `Caveats` section before `Full brew log:`.
- The trailing `already soaked: N formulae and casks` line is removed; the
  Summary carries that count.
- Notes that fit no group are listed one per line directly under `Notes`.
