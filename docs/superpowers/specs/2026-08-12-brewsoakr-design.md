# brewsoakr design

Date: 2026-08-12

A Rust CLI wrapper around Homebrew that only installs core formulae and casks whose definition files have soaked for a configurable number of hours and still exist (and are not deprecated/disabled) at current upstream HEAD. The goal is delayed acceptance of supply-chain updates so a compromised package can be yanked before it is installed.

## Decisions (locked)

- Soak clock starts on **any** formula or cask **file** change (version bump, bottle rebuild, revision, dependency edit).
- Scope is **homebrew-core formulae and homebrew-cask casks**. Third-party taps pass through to `brew`. Homebrew self-update is out of scope.
- Soaked commands: `update`, `upgrade`, `install`, `reinstall`, `outdated`, `info`. Everything else is `exec brew` with the original argv.
- `reinstall` pass-through only for a **true repair** (installed identity equals HEAD). Otherwise it follows soak rules.
- No `--now` / soak-bypass flag. Refusals explain why and tell the user to run plain `brew`.
- Soak duration is hours, integer `>= 1`, default **24**.
- Mixed `upgrade`: apply every eligible upgrade; refuse the rest with explanation; exit non-zero if anything was refused.
- Installed newer than the soaked candidate: **leave it**. Do not downgrade. A soaked `reinstall`/`install` that would refresh a too-new artifact is a refusal.
- History is two snapshots per tap: tree at T−soak (cutoff) and tree at HEAD. No intermediate blobs. Git history older than the cutoff is pruned.
- Eligible artifact is the cutoff `.rb`. Survival is “name still resolves at HEAD and HEAD file has no `deprecate!` / `disable!`”.
- `brew` is the only installer. brewsoakr writes cutoff files into a local tap and invokes `brew`.

## Naming and configuration

| Source | Name | Persistence |
|---|---|---|
| CLI flag | `--soak-hours N` | If present and `N != 24`, write config. If `N == 24`, delete config file. |
| Environment | `BREWSOAK_SOAK_HOURS` | Ephemeral. Never written to disk. |
| Config file | `~/.config/brewsoak/config.toml` key `SOAK_HOURS` | Read at startup. |

Precedence: **CLI > environment > config file > 24**.

Config file example:

```toml
SOAK_HOURS = 48
```

Invalid or missing config (unreadable, bad TOML, missing key, non-integer, `SOAK_HOURS < 1`) is **silently ignored**; default 24. Extra keys are ignored. Path is not XDG-overridable in v1.

Invalid `BREWSOAK_SOAK_HOURS` (unset is fine; non-integer or `< 1`) is **silently ignored**, same as a bad file.

Invalid `--soak-hours` (missing value, non-integer, `< 1`) is a usage error, exit 2. The CLI is explicit; only file and env are silent.

`--soak-hours` is accepted before or after the subcommand. After a successful parse of `--soak-hours N`, persist as specified above (create `~/.config/brewsoak/` if needed). We never write any other files in that directory.

## Architecture

`brewsoakr` owns soak policy and history. `brew` downloads bottles, compiles, links, and manages the Cellar/Caskroom.

1. **CLI** — clap; same argv shape as `brew` plus `--soak-hours`.
2. **Config** — resolve hours from CLI / env / file / default.
3. **Snapshot store** — under `~/Library/Caches/brewsoak/` (or `$XDG_CACHE_HOME/brewsoak` if set):
   - shallow clones of `https://github.com/Homebrew/homebrew-core` and `https://github.com/Homebrew/homebrew-cask` (HTTPS only, never SSH)
   - only two commits persisted per tap: `refs/brewsoak/cutoff` and `refs/brewsoak/head` (depth-1 fetches)
   - files read via `git show <sha>:<path>`; blobs fetched on demand
4. **Eligibility** — cutoff exists ∧ survived at HEAD. Install artifact = cutoff blob.
5. **Local tap** — `brewsoakr/soaked` at `$(brew --repository)/Library/Taps/brewsoakr/homebrew-soaked`. Cutoff `.rb` written to `Formula/<name>.rb` or `Casks/<name>.rb`.

`brew` binary: `$HOMEBREW_PREFIX/bin/brew` if `HOMEBREW_PREFIX` is set, else `brew` on `PATH`.

Two concurrent `brewsoakr` processes are unsupported.

## Snapshots

### Finding the two commits

For each of homebrew-core and homebrew-cask:

1. Resolve HEAD from `origin` over HTTPS.
2. Resolve cutoff as the latest commit with committer time `<= now - soak_hours`. Prefer GitHub commits API (`until=T−soak`, `per_page=1`). If that fails, a temporary `--shallow-since` fetch, then discard extra history.
3. Persist **only** those two commits as depth-1 fetches (`refs/brewsoak/cutoff`, `refs/brewsoak/head`). `git gc --prune=now` everything else.

`--shallow-since=T−soak` alone is not enough: it starts after the boundary. We must keep **one commit at or before T−soak**.

Increasing soak hours requires a deeper cutoff (new SHA). Decreasing hours moves cutoff forward; the old cutoff is GC’d.

### When to refresh

- `update` always refreshes.
- `install`, `upgrade`, and soaked `reinstall` refresh first (a yank must not be missed).
- `outdated` and `info` use the last snapshots; if none exist, refresh once.

Refresh also prefetches cutoff+HEAD blobs for every **installed** core formula and cask. Other names fetch those two blobs on demand.

State metadata (SHAs, soak hours used, timestamp) lives next to the clones under the cache directory, not in `~/.config/brewsoak/`.

## Eligibility

### Name resolution

- Formula: `Formula/<first-letter>/<name>.rb`, plus alias files in the core tap.
- Cask: `Casks/<first-letter>/<name>.rb`.
- Token `user/tap/name` (third-party) is **not** resolved here → passthrough to `brew`.

Renames (cutoff has `foo`, HEAD only has `bar`) count as **not survived** under `foo`. v1 does not follow renames.

### Survived

The name still resolves at HEAD, and the HEAD file does not contain a `deprecate!` or `disable!` call. Check is line-level on the HEAD blob; we do not evaluate Ruby. Any `deprecate!` / `disable!` at HEAD makes the package ineligible, even if the file is still present.

### Eligible

Path exists in the cutoff tree **and** survived at HEAD. The install artifact is the cutoff blob, bytes as-is.

### Comparing files vs installed receipts

Cutoff vs HEAD (did the definition change? is HEAD ahead of soak?) uses **raw git blobs**. Both sides are unmodified upstream files.

Installed vs cutoff/HEAD cannot use raw bytes: Homebrew rewrites the Cellar/Caskroom copy. Identity is parsed from the `.rb`:

- Formula: `version`, `revision`, bottle `rebuild`, and the stable-source `sha256` (not per-platform bottle hashes)
- Cask: `version`, `sha256`, `url`

Sources: installed receipt (Cellar `*/.brew/<name>.rb` or Caskroom `.metadata`), cutoff blob, HEAD blob.

Homebrew rewrites Cellar receipts and **drops the `bottle` block**, so a missing `rebuild` is unknown, not `0`. Compare rebuild only when both sides have it. Cutoff vs HEAD still sees rebuild (both are unmodified upstream files), so a same-version bottle rebuild still starts the soak clock.

A no-op “already soaked” line is printed only with `-v` / `--verbose`.

### Desired-state table

| Installed identity | Meaning | Mutating command |
|---|---|---|
| Equal to cutoff | Already soaked | No-op |
| Equal to HEAD, and HEAD ≠ cutoff | Ahead of soak | Leave; note it (not a refusal) |
| Anything else, and eligible | Behind soak | Install cutoff `.rb` |
| No cutoff (born inside the window) | Too new | Refuse |
| Cutoff exists, did not survive | Pulled / deprecated | Refuse |
| Not installed, eligible | Fresh install | Install cutoff `.rb` |

File-hash / identity comparison is required so a same-version bottle rebuild still soaks.

## Commands

`brewsoakr` uses the same argv shape as `brew`. Extra flags brewsoakr owns: `--soak-hours N`, `-v`/`--verbose` (already-soaked lines), `-V`/`--version`, `-h`/`--help`. Other flags (`--debug`, `--formula`, `--cask`, …) are forwarded to `brew` when we invoke it.

`brewsoakr --version` and `brewsoakr -V` print `brewsoakr <Cargo.toml version>` and exit 0. They are not passed through to `brew`. `brewsoakr --help`, `brewsoakr -h`, and `brewsoakr help` (no topic) print brewsoakr help and exit 0. `brewsoakr help <cmd>` still execs `brew help <cmd>`.

### `update`

Fetches both remotes, moves cutoff, sets HEAD, prefetches installed blobs, prunes clones. Does **not** update the Homebrew tool. Prints soak hours, cutoff SHAs **with committer time**, fetch progress per tap, and a short summary (became eligible / still soaking / gone at HEAD). `-v` also prints every installed package and why it classified that way.

### `outdated`

Lists only what **`upgrade` would change**: installed core formulae/casks whose cutoff identity is not the installed identity and which are eligible and not ahead of soak.

Held packages (soaking, yanked/deprecated) are printed separately with why, not as outdated. Ahead-of-soak packages are noted separately and are not refusals.

### `info`

Shows installed identity, soaked (cutoff) candidate, and HEAD. Makes clear which one `brewsoakr` would install.

- With names: those packages (third-party tokens `exec brew info`).
- With no names: every installed homebrew/core formula and homebrew/cask cask (including pinned).

### `upgrade` [names…]

- No names: all installed core formulae and casks (the set `brew upgrade` would consider, including non-greedy casks; exclude third-party taps).
- With names: those packages plus the dependency closure taken from their **cutoff** `.rb` files, not HEAD.
- Third-party `user/tap/foo`: `exec brew` for that token.

Per package, apply the desired-state table. Eligible upgrades run. Any refusal → non-zero exit even if some upgrades succeeded. Ahead of soak is not a refusal.

### `install`

Same eligibility as upgrade; the target need not be installed. No candidate → hard refuse, explain, “use `brew install …`”. Already installed at the soaked identity → same as `brew` (report, exit 0). Third-party tap → passthrough.

### `reinstall`

If installed identity equals **HEAD** → `exec brew reinstall` with the user’s flags (true repair).

Otherwise treat as a soaked install of the cutoff artifact. If that would not refresh the installed artifact (including ahead-of-soak), refuse. Do not pull a too-new bottle via `brew reinstall`.

### Everything else

`services`, `tap`, `doctor`, `cleanup`, unknown subcommands, etc. → `exec brew` with the original args (strip only `--soak-hours` and its value if present).

## Invoking brew

1. On first use: `brew tap-new brewsoakr/soaked --no-git`.
2. Write each needed cutoff blob to the tap (`Formula/<name>.rb` or `Casks/<name>.rb`).
3. `brew deps --formula brewsoakr/soaked/<name>` or `brew deps --cask brewsoakr/soaked/<name>` for the dependency name list. Keep names that exist in the cutoff core/cask trees; recurse; toposort. `uses_from_macos` and non-core deps stay with `brew`.
4. For each dep in order:
   - already installed (any identity, including ahead of soak) → leave it
   - eligible and missing → `brew install brewsoakr/soaked/<dep>`
   - missing and not eligible → refuse the **target**
5. Install the target from the staged cutoff `.rb` without `--ignore-dependencies` (Homebrew treats that flag as an unsupported developer option). The cutoff dep closure is already installed, so `brew` should treat those deps as satisfied. Set `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1` so `brew` does not then upgrade dependents to HEAD. Include build deps only when `brew deps` says they are needed.

User flags are forwarded. `brew` keeps stdout/stderr. brewsoakr prints refusals and ahead-of-soak notes around that. If `brew` fails for an eligible package (missing bottle, compile error), that package takes `brew`’s exit code; remaining eligible packages still run.

### Out of scope (v1)

- Casks that self-update after install
- Following formula/cask renames
- Third-party taps
- Updating the Homebrew tool itself
- Concurrent brewsoakr processes

## Failures and exit status

Refusals always say **why** (too new: how recent the HEAD change is; yanked: missing at HEAD; deprecated: `deprecate!`/`disable!`; ineligible dep) and **how to bypass** (`brew install …` / `brew upgrade …` / `brew reinstall …`).

| Outcome | Exit |
|---|---|
| Nothing to do, or all soaked actions succeeded, only ahead-of-soak notes | 0 |
| One or more refusals | 1 |
| `brew` failed for an eligible package | `brew`’s code; if we already owed 1 for a refusal, keep 1 unless `brew` was `> 1`, then prefer `brew`’s |
| Usage, invalid `--soak-hours`, `brew` not found | 2 |

## Testing

Rust, `cargo test`. No live Homebrew network in unit tests.

Cover:

- Config: default 24; valid file `SOAK_HOURS`; each invalid file shape (silent default); `BREWSOAK_SOAK_HOURS` valid and invalid; precedence CLI > env > file > default; `--soak-hours` persist when `!= 24` and delete when `== 24`.
- Eligibility table: cutoff+HEAD present; too new; yanked; deprecated; ahead of soak; same version string but file/identity change (bottle rebuild).
- Mixed `upgrade`: eligible applied, held refused, ahead noted, exit 1.
- `reinstall`: identity==HEAD → passthrough; else soaked path.
- Dep closure: missing ineligible dep refuses the target; already-installed dep left alone.
- Snapshot prune: after refresh, only cutoff+HEAD refs remain.
- Argv: unknown subcommand and `user/tap/foo` are passthrough.

Git/GitHub and real `brew` sit behind traits and are mocked in unit tests. A `cargo test -- --ignored` integration test may call real `brew` if present (list/info only; no installs unless a later fixture tap is added).

## Implementation notes

- Language: Rust, system toolchain (rustc 1.95 / cargo 1.95 on the author’s machine).
- Suggested crates: `clap`, `serde`, `toml`, `thiserror`. Prefer std + `std::process::Command` for git/`brew`. Add `ureq` or similar only if the GitHub commits API client is not done with a thin HTTP call.
- Platform: macOS (Homebrew 4+ API-mode prefix). Linuxbrew is not a v1 target.

## What this is not

- Not a delayed checkout of the user’s live `homebrew-core` tap (this machine has none; brew is API-mode).
- Not a 24-hour journal of the JSON API (that cannot reconstruct day-one history or bottle rebuilds cleanly).
- Not a replacement for `brew` for commands we do not change.
