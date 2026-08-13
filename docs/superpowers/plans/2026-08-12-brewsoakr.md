# brewsoakr Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a Rust CLI `brewsoakr` that wraps Homebrew and only installs homebrew-core formulae and homebrew-cask casks whose definition files are older than a soak window and still present (and not deprecated/disabled) at upstream HEAD.

**Architecture:** A clap CLI resolves soak hours, classifies argv (soaked vs passthrough), and refreshes two depth-1 git snapshots per tap (cutoff at T−soak, current HEAD). Pure eligibility logic picks a cutoff `.rb`; a local `brewsoakr/soaked` tap plus `brew` performs the install. Git, GitHub, and `brew` sit behind traits so unit tests never touch the network.

**Tech Stack:** Rust 2024 (system rustc/cargo), serde, toml, ureq, time. `std::process::Command` for git and brew. Manual argv walk (no clap — brew’s flag soup). `tempfile` as a dev-dependency.

**Spec:** `docs/superpowers/specs/2026-08-12-brewsoakr-design.md`

## Global Constraints

- Soak duration is an integer number of hours `>= 1`. Default is `24`.
- CLI flag is `--soak-hours`. Environment variable is `BREWSOAK_SOAK_HOURS`. Config key is `SOAK_HOURS` in `~/.config/brewsoak/config.toml`.
- Precedence is CLI > environment > config file > `24`.
- Invalid config file or invalid env is silently ignored (treat as unset). Invalid `--soak-hours` is a usage error, exit `2`.
- `--soak-hours N` with `N != 24` writes the config file. `--soak-hours 24` deletes it. Env never persists.
- Soaked commands: `update`, `upgrade`, `install`, `reinstall`, `outdated`, `info`. Every other subcommand is `exec brew` (strip only `--soak-hours` and its value).
- Third-party tokens `user/tap/name` pass through to `brew`. Homebrew self-update is not implemented.
- Two snapshots only per tap: `refs/brewsoak/cutoff` and `refs/brewsoak/head`. HTTPS remotes only. Never SSH.
- Cutoff vs HEAD uses raw git blobs. Installed vs cutoff/HEAD uses parsed identity (`version`/`revision`/`rebuild`/source `sha256` for formulae; `version`/`sha256`/`url` for casks).
- Survived = name resolves at HEAD and HEAD blob has no `deprecate!` or `disable!` call (`^\s*(deprecate!|disable!)\b`).
- No soak-bypass flag. Refusals explain why and name the equivalent `brew` command.
- Ahead of soak is not a refusal and must not downgrade. Mixed `upgrade` applies eligible packages, refuses the rest, exit `1` if any refusal.
- `brew` is the only installer. After installing/leaving the cutoff dep closure, install the target with `--ignore-dependencies`.
- Exit `0` success (ahead-of-soak notes allowed), `1` any refusal, `2` usage / missing brew. If `brew` fails and we already owed `1`, keep `1` unless `brew` was `> 1`.
- Unit tests must not use the network or a real Homebrew install. Traits mock git, GitHub, and brew.
- Platform target is macOS. Do not add Linuxbrew-specific paths.

## File map

Create these files. Do not invent others unless a task says so.

| File | Responsibility |
|---|---|
| `Cargo.toml` | Package `brewsoakr`, bin + lib, dependencies |
| `src/lib.rs` | Module exports |
| `src/main.rs` | `fn main`, map `Error` to exit codes, `exec` passthrough |
| `src/error.rs` | `Error`, `exit_code()` |
| `src/hours.rs` | `SoakHours` newtype |
| `src/config.rs` | Resolve hours, read/write/delete config |
| `src/cli.rs` | Argv parse → `Invocation` |
| `src/identity.rs` | Parse formula/cask identity; `deprecate!`/`disable!` |
| `src/eligibility.rs` | `UpstreamStatus`, `DesiredAction` |
| `src/paths.rs` | Config path, cache dir, brew binary, tap dir, formula/cask git paths |
| `src/github.rs` | `GithubApi` trait + `UreqGithub` |
| `src/git.rs` | `GitStore` trait + `LibexecGit` |
| `src/snapshot.rs` | Refresh cutoff+HEAD, prune, `git show` blobs |
| `src/resolve.rs` | Name → git path, alias lookup, blob fetch |
| `src/brew.rs` | `Brew` trait + `ProcessBrew` |
| `src/tap.rs` | Write `brewsoakr/soaked`, dep closure, install |
| `src/cmd.rs` | Soaked command implementations |

---

### Task 1: Crate skeleton, `SoakHours`, `Error`

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/main.rs`
- Create: `src/error.rs`
- Create: `src/hours.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `SoakHours::new(u32) -> Option<SoakHours>`, `SoakHours::DEFAULT` (`24`), `SoakHours::get(self) -> u32`, `Error` enum, `Error::exit_code(&self) -> i32`

- [ ] **Step 1: Write the failing tests**

Create `src/hours.rs` with tests only (the type can be a stub that does not compile until step 3 — prefer compiling tests that fail assertions). Write `src/hours.rs` as:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakHours(u32);

impl SoakHours {
    pub const DEFAULT: Self = Self(24);

    pub fn new(n: u32) -> Option<Self> {
        todo!()
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero() {
        assert_eq!(SoakHours::new(0), None);
    }

    #[test]
    fn accepts_one() {
        assert_eq!(SoakHours::new(1).map(|h| h.get()), Some(1));
    }

    #[test]
    fn default_is_24() {
        assert_eq!(SoakHours::DEFAULT.get(), 24);
    }
}
```

Create `src/error.rs`:

```rust
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Usage(String),
    Refusal(String),
    Brew { status: i32, message: String },
    Io(std::io::Error),
    Other(String),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        todo!()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(s) | Error::Refusal(s) | Error::Other(s) => write!(f, "{s}"),
            Error::Brew { message, .. } => write!(f, "{message}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_2() {
        assert_eq!(Error::Usage("x".into()).exit_code(), 2);
    }

    #[test]
    fn refusal_is_1() {
        assert_eq!(Error::Refusal("x".into()).exit_code(), 1);
    }

    #[test]
    fn brew_gt_1_is_brew() {
        assert_eq!(Error::Brew { status: 3, message: "x".into() }.exit_code(), 3);
    }

    #[test]
    fn brew_1_is_1() {
        assert_eq!(Error::Brew { status: 1, message: "x".into() }.exit_code(), 1);
    }
}
```

Create `src/lib.rs`:

```rust
pub mod error;
pub mod hours;

pub use error::Error;
pub use hours::SoakHours;
```

Create `src/main.rs`:

```rust
fn main() {
    eprintln!("not implemented");
    std::process::exit(2);
}
```

Create `Cargo.toml`:

```toml
[package]
name = "brewsoakr"
version = "0.1.0"
edition = "2024"
description = "Homebrew wrapper that soaks formula and cask updates"
license = "MIT"

[dependencies]
serde = { version = "1", features = ["derive"] }
time = { version = "0.3", features = ["formatting", "parsing"] }
toml = "0.8"
ureq = "2"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib hours::tests error::tests`

Expected: compile succeeds, tests fail on `todo!()` (or `new` returns `None` for `1` if you returned `None` instead of `todo!()`).

- [ ] **Step 3: Implement `SoakHours::new` and `Error::exit_code`**

```rust
// SoakHours::new
pub fn new(n: u32) -> Option<Self> {
    (n >= 1).then_some(Self(n))
}

// Error::exit_code
pub fn exit_code(&self) -> i32 {
    match self {
        Error::Usage(_) => 2,
        Error::Refusal(_) => 1,
        Error::Brew { status, .. } => *status,
        Error::Io(_) | Error::Other(_) => 1,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: all tests PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/lib.rs src/main.rs src/error.rs src/hours.rs
git commit -m "feat: add crate skeleton, SoakHours, and Error exit codes"
```

---

### Task 2: Config resolution and persist

**Files:**
- Create: `src/config.rs`
- Create: `src/paths.rs`
- Modify: `src/lib.rs` (add `pub mod config; pub mod paths;`)

**Interfaces:**
- Consumes: `SoakHours`, `Error`
- Produces:
  - `paths::config_file() -> PathBuf` → `~/.config/brewsoak/config.toml` (`$HOME/.config/brewsoak/config.toml`)
  - `ResolvedHours { hours: SoakHours, persist: PersistAction }`
  - `PersistAction::{None, Write(SoakHours), Delete}`
  - `resolve_hours(cli: Option<u32>, env: Option<&str>, file_contents: Option<&str>) -> Result<ResolvedHours, Error>`
  - `apply_persist(action: PersistAction, path: &Path) -> Result<(), Error>`
  - `read_file(path: &Path) -> Option<String>` (None if missing or unreadable)

Precedence inside `resolve_hours`: CLI (if `SoakHours::new` succeeds; else `Error::Usage`) > env (silent ignore if invalid) > file (silent ignore if invalid TOML / missing `SOAK_HOURS` / non-integer / `< 1`) > default 24.

CLI persist: if CLI present and valid and `!= 24` → `Write`; if CLI present and `== 24` → `Delete`; otherwise `None`.

File format:

```toml
SOAK_HOURS = 48
```

- [ ] **Step 1: Write the failing tests**

```rust
// src/config.rs
use crate::{Error, SoakHours};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistAction {
    None,
    Write(SoakHours),
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedHours {
    pub hours: SoakHours,
    pub persist: PersistAction,
}

pub fn resolve_hours(
    cli: Option<u32>,
    env: Option<&str>,
    file_contents: Option<&str>,
) -> Result<ResolvedHours, Error> {
    todo!()
}

pub fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

pub fn apply_persist(action: PersistAction, path: &Path) -> Result<(), Error> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_when_nothing_set() {
        let r = resolve_hours(None, None, None).unwrap();
        assert_eq!(r.hours.get(), 24);
        assert_eq!(r.persist, PersistAction::None);
    }

    #[test]
    fn cli_wins_and_persists() {
        let r = resolve_hours(Some(48), Some("12"), Some("SOAK_HOURS = 6\n")).unwrap();
        assert_eq!(r.hours.get(), 48);
        assert_eq!(r.persist, PersistAction::Write(SoakHours::new(48).unwrap()));
    }

    #[test]
    fn cli_24_deletes() {
        let r = resolve_hours(Some(24), Some("48"), None).unwrap();
        assert_eq!(r.hours.get(), 24);
        assert_eq!(r.persist, PersistAction::Delete);
    }

    #[test]
    fn cli_zero_is_usage() {
        assert!(matches!(resolve_hours(Some(0), None, None), Err(Error::Usage(_))));
    }

    #[test]
    fn env_used_when_no_cli() {
        let r = resolve_hours(None, Some("36"), Some("SOAK_HOURS = 6\n")).unwrap();
        assert_eq!(r.hours.get(), 36);
        assert_eq!(r.persist, PersistAction::None);
    }

    #[test]
    fn invalid_env_falls_through() {
        let r = resolve_hours(None, Some("nope"), Some("SOAK_HOURS = 8\n")).unwrap();
        assert_eq!(r.hours.get(), 8);
    }

    #[test]
    fn invalid_file_is_default() {
        for contents in ["", "hours = 48\n", "SOAK_HOURS = 0\n", "SOAK_HOURS = \"x\"\n", "[[["] {
            let r = resolve_hours(None, None, Some(contents)).unwrap();
            assert_eq!(r.hours.get(), 24, "contents={contents:?}");
        }
    }

    #[test]
    fn apply_write_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        apply_persist(PersistAction::Write(SoakHours::new(48).unwrap()), &path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SOAK_HOURS = 48\n");
        apply_persist(PersistAction::Delete, &path).unwrap();
        assert!(!path.exists());
        apply_persist(PersistAction::Delete, &path).unwrap(); // missing is ok
    }
}
```

`src/paths.rs`:

```rust
use std::path::PathBuf;

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_file() -> PathBuf {
    home_dir().join(".config/brewsoak/config.toml")
}

pub fn cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("brewsoak");
    }
    home_dir().join("Library/Caches/brewsoak")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_under_home_config() {
        assert!(config_file().ends_with(".config/brewsoak/config.toml"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests`

Expected: FAIL on `todo!()`.

- [ ] **Step 3: Implement `resolve_hours` and `apply_persist`**

Parse file with `toml::Value` and read integer `SOAK_HOURS`. Create parent dirs on `Write`. `Delete` uses `std::fs::remove_file` and ignores `NotFound`.

```rust
pub fn resolve_hours(
    cli: Option<u32>,
    env: Option<&str>,
    file_contents: Option<&str>,
) -> Result<ResolvedHours, Error> {
    if let Some(n) = cli {
        let hours = SoakHours::new(n)
            .ok_or_else(|| Error::Usage("--soak-hours must be an integer >= 1".into()))?;
        let persist = if hours == SoakHours::DEFAULT {
            PersistAction::Delete
        } else {
            PersistAction::Write(hours)
        };
        return Ok(ResolvedHours { hours, persist });
    }
    if let Some(raw) = env {
        if let Some(hours) = raw.parse::<u32>().ok().and_then(SoakHours::new) {
            return Ok(ResolvedHours { hours, persist: PersistAction::None });
        }
    }
    if let Some(contents) = file_contents {
        if let Some(hours) = parse_file(contents) {
            return Ok(ResolvedHours { hours, persist: PersistAction::None });
        }
    }
    Ok(ResolvedHours {
        hours: SoakHours::DEFAULT,
        persist: PersistAction::None,
    })
}

fn parse_file(contents: &str) -> Option<SoakHours> {
    let v: toml::Value = toml::from_str(contents).ok()?;
    let n = v.get("SOAK_HOURS")?.as_integer()?;
    let n = u32::try_from(n).ok()?;
    SoakHours::new(n)
}

pub fn apply_persist(action: PersistAction, path: &Path) -> Result<(), Error> {
    match action {
        PersistAction::None => Ok(()),
        PersistAction::Write(hours) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, format!("SOAK_HOURS = {}\n", hours.get()))?;
            Ok(())
        }
        PersistAction::Delete => {
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
    }
}
```

Implement `PartialEq` on `SoakHours` (already derived).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/paths.rs src/lib.rs
git commit -m "feat: resolve soak hours from CLI, env, and config.toml"
```

---

### Task 3: CLI argv parsing

**Files:**
- Create: `src/cli.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Error`
- Produces:
  - `Invocation { soak_hours: Option<u32>, command: Command, brew_args: Vec<String> }`
  - `Command::{Update, Upgrade{names}, Install{names, force_cask, force_formula}, Reinstall{names}, Outdated, Info{names}, Passthrough{args}}`
  - `parse_argv(args: &[String]) -> Result<Invocation, Error>`
  - args are **without** argv0

Rules:

- Scan all args for `--soak-hours` / `--soak-hours=N`. The following arg is the value for the space form. Strip both from the argv passed to brew / subcommands. Missing/non-integer value → `Error::Usage`.
- First remaining non-flag token is the subcommand. Flags before it stay in `brew_args` (and also apply to soaked commands).
- Known soaked subcommands: `update`, `upgrade`, `install`, `reinstall`, `outdated`, `info`.
- `install`/`upgrade`/`reinstall`/`info`: remaining non-flag tokens that do not start with `-` are `names`, except `--cask`/`--formula` set the force flags on `install` (and are also kept in `brew_args`).
- Unknown subcommand or no subcommand → `Passthrough { args: original args minus soak-hours }`.
- `--help` / `-h` / `help` may be passthrough to a later help string; for v1, `brewsoakr --help` is passthrough only if you have not implemented clap help. Prefer a short clap-style error on `brewsoakr` with no args: `Error::Usage` mentioning available commands. No-args → `Error::Usage`.

- [ ] **Step 1: Write the failing tests**

```rust
// src/cli.rs — put parse_argv behind todo!() and add:
#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn upgrade_with_flag_before_and_after() {
        let i = parse_argv(&s(&["--soak-hours", "48", "upgrade", "-v", "wget"])).unwrap();
        assert_eq!(i.soak_hours, Some(48));
        assert!(matches!(i.command, Command::Upgrade { ref names } if names == &["wget"]));
        assert!(i.brew_args.iter().any(|a| a == "-v"));
    }

    #[test]
    fn soak_hours_after_subcommand() {
        let i = parse_argv(&s(&["upgrade", "--soak-hours=12", "foo"])).unwrap();
        assert_eq!(i.soak_hours, Some(12));
        assert!(matches!(i.command, Command::Upgrade { ref names } if names == &["foo"]));
    }

    #[test]
    fn services_is_passthrough_without_soak_flag() {
        let i = parse_argv(&s(&["--soak-hours", "48", "services", "start", "foo"])).unwrap();
        assert_eq!(i.soak_hours, Some(48));
        match i.command {
            Command::Passthrough { args } => {
                assert_eq!(args, s(&["services", "start", "foo"]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_soak_value_is_usage() {
        assert!(matches!(parse_argv(&s(&["--soak-hours"])), Err(Error::Usage(_))));
    }

    #[test]
    fn no_args_is_usage() {
        assert!(matches!(parse_argv(&[]), Err(Error::Usage(_))));
    }

    #[test]
    fn install_cask_flag() {
        let i = parse_argv(&s(&["install", "--cask", "firefox"])).unwrap();
        match i.command {
            Command::Install { names, force_cask, force_formula } => {
                assert_eq!(names, ["firefox"]);
                assert!(force_cask);
                assert!(!force_formula);
            }
            other => panic!("{other:?}"),
        }
    }
}
```

Define `Invocation` and `Command` in the same file; `parse_argv` is `todo!()`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cli::tests`

Expected: FAIL on `todo!()`.

- [ ] **Step 3: Implement `parse_argv`**

Walk the slice once to extract soak hours, collect remaining. Classify remaining[0]. For soaked commands, split flags vs names (`--cask`/`--formula` recognized). Do not use clap derive for the brew-compatible argv; a manual walk is the implementation. clap may still be used later for `--help` text only — do not force clap derive onto brew's flag soup.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/lib.rs
git commit -m "feat: parse brew-compatible argv and --soak-hours"
```

---

### Task 4: Identity parser and deprecate/disable detection

**Files:**
- Create: `src/identity.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: nothing
- Produces:
  - `FormulaIdentity { version: String, revision: u32, rebuild: u32, sha256: String }`
  - `CaskIdentity { version: String, sha256: String, url: String }`
  - `PkgIdentity::{Formula(FormulaIdentity), Cask(CaskIdentity)}` with `Eq`
  - `parse_formula(rb: &str) -> Result<FormulaIdentity, Error>`
  - `parse_cask(rb: &str) -> Result<CaskIdentity, Error>`
  - `is_deprecated_or_disabled(rb: &str) -> bool`

Formula parse rules:

- `version "…"` if present; else last path segment of the first `url "…"` with archive suffix stripped (`wget-1.21.4.tar.gz` → `1.21.4`).
- `revision N` else `0`.
- First `rebuild N` else `0`.
- First `sha256 "…"` that appears **before** a line matching `^\s*bottle\s+do\b`; if none, first `sha256 "…"` in the file.

Cask: first `version "…"`, first `sha256 "…"`, first `url "…"`.

`is_deprecated_or_disabled`: any line matching `^\s*(deprecate!|disable!)\b`.

- [ ] **Step 1: Write the failing tests**

Use these fixtures in `identity.rs` tests:

```ruby
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "aaa111"
  revision 1
  bottle do
    rebuild 2
    sha256 cellar: :any, arm64_sequoia: "bbb222"
  end
end
```

Expect `version=1.21.4`, `revision=1`, `rebuild=2`, `sha256=aaa111`.

Second fixture: same version string, different source sha256 → different `FormulaIdentity`.

Cask fixture:

```ruby
cask "foo" do
  version "3.0"
  sha256 "ccc333"
  url "https://example.com/foo-3.0.dmg"
end
```

Deprecated fixture includes `  deprecate! date: "2024-01-01", because: :unmaintained`.

A comment line `# disable! maybe` must **not** match.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib identity::tests`

Expected: FAIL.

- [ ] **Step 3: Implement parsers with `regex` or manual line scans**

Do **not** add the `regex` crate unless a line scan is painful. Prefer `str` methods and a tiny helper:

```rust
fn first_quoted(rb: &str, key: &str) -> Option<String> {
    for line in rb.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(key) {
            // key is `version ` or `url ` etc.
            ...
        }
    }
    None
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/identity.rs src/lib.rs Cargo.toml
git commit -m "feat: parse formula and cask identity from Ruby DSL"
```

---

### Task 5: Eligibility and desired action

**Files:**
- Create: `src/eligibility.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `PkgIdentity`
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamStatus {
    Eligible,
    TooNew,
    Yanked,
    Deprecated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredAction {
    NoOpAlreadySoaked,
    LeaveAheadOfSoak,
    InstallCutoff,
    RefuseTooNew,
    RefuseYanked,
    RefuseDeprecated,
}

/// `cutoff_blob` / `head_blob` are raw git bytes. `installed` is parsed receipt identity.
pub fn upstream_status(
    cutoff_blob: Option<&[u8]>,
    head_blob: Option<&[u8]>,
) -> UpstreamStatus {
    todo!()
}

pub fn desired_action(
    status: UpstreamStatus,
    installed: Option<&PkgIdentity>,
    cutoff_id: Option<&PkgIdentity>,
    head_id: Option<&PkgIdentity>,
) -> DesiredAction {
    todo!()
}
```

`upstream_status`:

- `head_blob` is None → `Yanked` (even if cutoff exists).
- `head_blob` is Some and `is_deprecated_or_disabled` → `Deprecated`.
- `cutoff_blob` is None → `TooNew`.
- else `Eligible`.

`desired_action`:

- `Yanked` → `RefuseYanked`
- `Deprecated` → `RefuseDeprecated`
- `TooNew` → `RefuseTooNew`
- `Eligible` + `installed` is None → `InstallCutoff`
- `Eligible` + `installed == cutoff_id` → `NoOpAlreadySoaked`
- `Eligible` + `installed == head_id` and `cutoff_blob`/`head` identities differ → `LeaveAheadOfSoak`
- `Eligible` + anything else → `InstallCutoff`

Do not compare raw installed bytes.

- [ ] **Step 1: Write the failing tests**

One test per table row in the spec (too new, yanked, deprecated, already soaked, ahead of soak, behind soak / other identity, not installed eligible). Include a same-version different-sha256 pair that yields `InstallCutoff` when installed matches neither (old bottle) and `LeaveAheadOfSoak` when installed identity equals HEAD.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib eligibility::tests`

Expected: FAIL.

- [ ] **Step 3: Implement the two functions** using `identity::is_deprecated_or_disabled` and `PkgIdentity` equality.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/eligibility.rs src/lib.rs
git commit -m "feat: encode soak eligibility and desired-state table"
```

---

### Task 6: Git store trait and prune

**Files:**
- Create: `src/git.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Error`
- Produces:

```rust
pub const REF_CUTOFF: &str = "refs/brewsoak/cutoff";
pub const REF_HEAD: &str = "refs/brewsoak/head";

pub trait GitStore {
    fn init_bare(&self, dir: &Path) -> Result<(), Error>;
    fn fetch_depth1(&self, dir: &Path, remote: &str, sha: &str, ref_name: &str) -> Result<(), Error>;
    fn show(&self, dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, Error>;
    fn rev_parse(&self, dir: &Path, rev: &str) -> Result<Option<String>, Error>;
    fn gc_prune(&self, dir: &Path) -> Result<(), Error>;
}

pub struct ProcessGit;

impl GitStore for ProcessGit { /* Command::new("git") */ }
```

`show` returns `None` if the path is missing (git exit 128 / “does not exist”). Never use SSH remotes; callers pass `https://github.com/Homebrew/homebrew-core`.

Also produce `InMemoryGit` under `#[cfg(test)]` mapping `(sha, path) -> bytes` and recording fetched `(sha, ref_name)` so later tasks can inject blobs without spawning git.

- [ ] **Step 1: Write the failing tests**

Test `InMemoryGit`:

- `show` missing path → `None`
- `show` present → bytes
- `fetch_depth1` records the ref; `rev_parse(REF_CUTOFF)` returns that sha
- After `fetch_depth1` of a new cutoff sha, `rev_parse(REF_CUTOFF)` is the new sha (old replaced)

Do not spawn real git in this task’s tests.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib git::tests`

Expected: FAIL.

- [ ] **Step 3: Implement `InMemoryGit` fully and `ProcessGit` with `git init --bare`, `git fetch --depth=1 <remote> <sha>:<ref>`, `git show <sha>:<path>`, `git rev-parse <ref>`, `git -C <dir> gc --prune=now`.**

`ProcessGit` tests are not required in this task. Implement it so Task 8 can use it.

Fetch command:

```text
git --git-dir <dir> fetch --depth=1 <remote> <sha>:<ref_name>
```

Show:

```text
git --git-dir <dir> show <sha>:<path>
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/git.rs src/lib.rs
git commit -m "feat: add GitStore trait, in-memory mock, and process git"
```

---

### Task 7: GitHub API cutoff lookup

**Files:**
- Create: `src/github.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Error`, `SoakHours`
- Produces:

```rust
pub struct CommitInfo {
    pub sha: String,
    pub committer_time: time::OffsetDateTime,
}

pub trait GithubApi {
    fn head_sha(&self, repo: &str) -> Result<String, Error>;
    fn latest_commit_until(&self, repo: &str, until: time::OffsetDateTime) -> Result<CommitInfo, Error>;
}

pub struct UreqGithub {
    pub base: String, // "https://api.github.com"
}

pub struct StaticGithub {
    pub head: String,
    pub commits: Vec<CommitInfo>, // newest first
}
```

`repo` is `"Homebrew/homebrew-core"` or `"Homebrew/homebrew-cask"`.

`UreqGithub::latest_commit_until` GET `{base}/repos/{repo}/commits?until={rfc3339}&per_page=1` with header `User-Agent: brewsoakr`. Parse `sha` and `commit.committer.date`.

`StaticGithub` returns the first commit with `committer_time <= until`, or `Error::Other` if none.

Also produce:

```rust
pub fn cutoff_instant(now: time::OffsetDateTime, hours: SoakHours) -> time::OffsetDateTime {
    now - time::Duration::hours(i64::from(hours.get()))
}
```

- [ ] **Step 1: Write the failing tests**

`StaticGithub` with two commits (30h ago sha `aa`, 2h ago sha `bb`, head `bb`): `latest_commit_until(now - 24h)` → `aa`. `cutoff_instant` subtracts exactly the hours.

Do not call the live GitHub API.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib github::tests`

Expected: FAIL.

- [ ] **Step 3: Implement `cutoff_instant`, `StaticGithub`, and `UreqGithub`.** Use `time::format_description::well_known::Rfc3339`. On HTTP failure, return `Error::Other` with the status (snapshot refresh will fall back to git in Task 8).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/github.rs src/lib.rs
git commit -m "feat: look up soak cutoff commit via GitHub API"
```

---

### Task 8: Snapshot refresh

**Files:**
- Create: `src/snapshot.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `GitStore`, `GithubApi`, `SoakHours`, `paths::cache_dir`, `Error`
- Produces:

```rust
pub const CORE_REMOTE: &str = "https://github.com/Homebrew/homebrew-core";
pub const CASK_REMOTE: &str = "https://github.com/Homebrew/homebrew-cask";
pub const CORE_REPO: &str = "Homebrew/homebrew-core";
pub const CASK_REPO: &str = "Homebrew/homebrew-cask";

pub struct TapSnapshot {
    pub cutoff_sha: String,
    pub head_sha: String,
}

pub struct Snapshots {
    pub core: TapSnapshot,
    pub cask: TapSnapshot,
    pub hours: SoakHours,
}

pub fn refresh(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    hours: SoakHours,
    now: time::OffsetDateTime,
) -> Result<Snapshots, Error>;

pub fn load_state(cache: &Path) -> Result<Option<Snapshots>, Error>;
```

Layout:

```
<cache>/core.git/          # bare repo
<cache>/cask.git/
<cache>/state.toml
```

`state.toml`:

```toml
hours = 24
core_cutoff = "…"
core_head = "…"
cask_cutoff = "…"
cask_head = "…"
```

(`hours` here is metadata about the last refresh, not user config. Do not name it `SOAK_HOURS`.)

`refresh` per tap:

1. `git.init_bare`
2. `head = gh.head_sha(repo)`
3. `cutoff = gh.latest_commit_until(repo, cutoff_instant(now, hours))`; on error, `Error::Other` in v1 (no shallow-since fallback until a later hardening commit — still implement a `ProcessGit` fallback function `fn cutoff_via_shallow(git, remote, until) -> Result<String, Error>` used when GitHub fails: `git fetch --shallow-since=<unix> <remote> HEAD`, `git log -1 --before=<unix> --format=%H`, then fetch only the two SHAs depth-1 into the bare repo and `gc_prune`). Call that fallback when `latest_commit_until` returns `Err`.
4. `fetch_depth1(remote, cutoff, REF_CUTOFF)` and `fetch_depth1(remote, head, REF_HEAD)`
5. `gc_prune`
6. write `state.toml`

`load_state` reads `state.toml` or `None`.

- [ ] **Step 1: Write the failing tests**

Use `InMemoryGit` + `StaticGithub`. After `refresh`, `Snapshots.core.cutoff_sha` is the pre-soak commit, `head_sha` is HEAD. `load_state` round-trips. A second `refresh` with smaller hours moves cutoff forward (use commits at 30h and 10h; 24h cutoff is 30h-old; 8h cutoff is 10h-old).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib snapshot::tests`

Expected: FAIL.

- [ ] **Step 3: Implement `refresh` and `load_state`.** Fallback `cutoff_via_shallow` may return `Error::Other("github and git fallback failed")` if `InMemoryGit` cannot do shallow — gate real git fallback on `ProcessGit` only (`if` the store is process git, or pass an optional fallback closure). Keep the unit test on the GitHub-success path.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot.rs src/lib.rs
git commit -m "feat: refresh two-commit core and cask snapshots"
```

---

### Task 9: Name resolution and blob access

**Files:**
- Create: `src/resolve.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `GitStore`, `TapSnapshot` / shas, `identity`, `eligibility`
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgKind { Formula, Cask }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgRef {
    pub name: String,
    pub kind: PkgKind,
}

pub fn git_path(pkg: &PkgRef) -> String;
// Formula: Formula/<first>/<name>.rb
// Cask:    Casks/<first>/<name>.rb
// first character of the name, as lowercase ASCII if ASCII, else the first char as-is.

pub fn is_third_party(token: &str) -> bool;
// true iff the token contains two slashes (user/tap/name) or one slash that is not a version pin.
// Treat `org/repo/formula` as third-party. Treat `wget` and `openssl@3` as first-party.

pub struct ResolvedBlobs {
    pub cutoff: Option<Vec<u8>>,
    pub head: Option<Vec<u8>>,
}

pub fn resolve_blobs(
    git: &impl GitStore,
    repo_dir: &Path,
    cutoff_sha: &str,
    head_sha: &str,
    pkg: &PkgRef,
) -> Result<ResolvedBlobs, Error>;
```

`resolve_blobs`: `show` the primary path at both SHAs. If HEAD path is missing, try `Aliases/<name>` (file content is the canonical name, one line) at HEAD; if present, retry `git_path` for that canonical name at both SHAs. If cutoff primary path is missing, also try the canonical name from the HEAD alias. Do **not** treat a rename (cutoff has old name, HEAD only has new name, no alias) as survived — that stays yanked for the old name.

- [ ] **Step 1: Write the failing tests**

- `git_path` for `wget` / `firefox` / `openssl@3`
- `is_third_party("user/tap/foo")` true, `is_third_party("wget")` false
- `resolve_blobs` with InMemoryGit populated at `Formula/w/wget.rb`
- alias: HEAD `Aliases/wget` → `wget-extra`, blobs at `Formula/w/wget-extra.rb`
- missing everywhere → both `None`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib resolve::tests`

Expected: FAIL.

- [ ] **Step 3: Implement path helpers and `resolve_blobs`.**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/resolve.rs src/lib.rs
git commit -m "feat: resolve formula and cask paths in snapshot trees"
```

---

### Task 10: Brew trait and passthrough

**Files:**
- Create: `src/brew.rs`
- Modify: `src/paths.rs` (add `brew_bin()`)
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Error`, `PkgKind`, `PkgRef`
- Produces:

```rust
pub struct InstalledPkg {
    pub name: String,
    pub kind: PkgKind,
    pub receipt_rb: String,
}

pub trait Brew {
    fn brew_bin(&self) -> &Path;
    fn run(&self, args: &[String]) -> Result<std::process::Output, Error>;
    fn installed_core(&self) -> Result<Vec<InstalledPkg>, Error>;
    fn tap_new_soaked(&self) -> Result<(), Error>;
    fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error>;
}

pub struct ProcessBrew { pub bin: PathBuf }
pub struct MockBrew {
    pub installed: Vec<InstalledPkg>,
    pub deps: BTreeMap<String, Vec<String>>,
    pub runs: Mutex<Vec<Vec<String>>>,
    pub next_status: i32,
}
```

`paths::brew_bin()`: if `HOMEBREW_PREFIX` set → `$HOMEBREW_PREFIX/bin/brew`, else `brew` looked up later via `which`-style: use `HOMEBREW_PREFIX` or default `/opt/homebrew/bin/brew` if that file exists, else `"brew"` and let `Command` search PATH. Missing binary when actually running is `Error::Usage("brew not found")`.

`ProcessBrew::installed_core` runs `brew info --json=v2 --installed` and keeps packages whose tap is `homebrew/core` or `homebrew/cask` (or empty tap for API-mode core). Receipt: prefer `json["installed"][0]` plus reading `.brew/*.rb` from `json["bottle"]`/`["installed"]` prefix — simpler: run `brew --cellar` / `brew --caskroom` and read `<cellar>/<name>/<version>/.brew/<name>.rb`. If the JSON route is easier, parse `installed.version` and read that keg’s `.brew` file. Skip third-party taps.

`ProcessBrew::deps`: `brew deps --formula <token>` or `brew deps --cask <token>`. Parse stdout lines as names.

`MockBrew` records `run` args and returns `next_status`.

Passthrough helper (used by `main`):

```rust
pub fn passthrough_exec(bin: &Path, args: &[String]) -> Error {
    // std::os::unix::process::CommandExt::exec
}
```

- [ ] **Step 1: Write the failing tests**

- `MockBrew::deps` returns configured names
- `MockBrew::installed_core` returns the vec
- `brew_bin` honors `HOMEBREW_PREFIX` (set in-test via a function `brew_bin_from_env(prefix: Option<&str>, path_exists: impl Fn(&Path)->bool)`)

Do not invoke real brew.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib brew::tests paths::tests`

Expected: FAIL.

- [ ] **Step 3: Implement `ProcessBrew` and `MockBrew`.** Keep JSON parsing in a pure `fn parse_installed_json(v: &str, read_receipt: impl Fn(&str, PkgKind)->Option<String>)` so tests can feed a fixture JSON without brew.

Fixture: one core formula `wget`, one cask `firefox`, one tap formula `acme/tools/foo` that must be dropped.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/brew.rs src/paths.rs src/lib.rs
git commit -m "feat: add Brew trait, mock, and installed-package listing"
```

---

### Task 11: Local tap, dep closure, install invocation

**Files:**
- Create: `src/tap.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Brew`, `PkgRef`, `PkgKind`, `Error`
- Produces:

```rust
pub const TAP_USER: &str = "brewsoakr";
pub const TAP_NAME: &str = "soaked";
// brew token: brewsoakr/soaked/<name>

pub fn tap_formula_path(tap_root: &Path, name: &str) -> PathBuf;
// tap_root/Formula/<name>.rb
pub fn tap_cask_path(tap_root: &Path, name: &str) -> PathBuf;
// tap_root/Casks/<name>.rb

pub fn write_blob(tap_root: &Path, pkg: &PkgRef, blob: &[u8]) -> Result<PathBuf, Error>;

pub fn dep_closure(
    brew: &impl Brew,
    kind: PkgKind,
    name: &str,
    is_core: impl Fn(&str) -> bool,
) -> Result<Vec<String>, Error>;
// BFS/DFS via brew.deps, keep names where is_core(name) is true, toposort
// (deps before dependents). Skip names already in the visiting path (cycles).

pub fn install_token(pkg: &PkgRef) -> String; // "brewsoakr/soaked/wget"

pub fn brew_install_args(pkg: &PkgRef, user_flags: &[String], ignore_deps: bool) -> Vec<String>;
// ["install", "--formula"|"--cask", optional "--ignore-dependencies", ...user_flags without subcommand, token]
```

- [ ] **Step 1: Write the failing tests**

- `write_blob` creates `Formula/wget.rb` with exact bytes
- `dep_closure`: `foo` → `bar`, `bar` → `baz`; `is_core` all true → `["baz", "bar"]` (foo not included; callers add the target themselves) **or** include foo last — pick **deps first, target excluded** and test that. Document it here: `dep_closure` returns dependencies only, target not included.
- `is_core` false for `linux-headers` style skip
- `brew_install_args` for a formula with `ignore_deps=true` contains `--ignore-dependencies` and `brewsoakr/soaked/wget`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib tap::tests`

Expected: FAIL.

- [ ] **Step 3: Implement write, closure, and arg builder.**

Toposort: Kahn or DFS post-order. If `brew.deps` returns empty, closure is empty.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tap.rs src/lib.rs
git commit -m "feat: materialize soaked tap files and compute dep closure"
```

---

### Task 12: Refusal copy and `update` / query plumbing

**Files:**
- Create: `src/cmd.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: all of the above
- Produces:

```rust
pub fn refusal_message(action: DesiredAction, name: &str, brew_verb: &str) -> Option<String>;
// None for NoOpAlreadySoaked, LeaveAheadOfSoak, InstallCutoff
// Some for Refuse*: why + "use `brew {verb} {name}` to bypass brewsoakr."

pub fn ahead_message(name: &str) -> String;
// "{name} is ahead of soak; leaving installed artifact unchanged"

pub struct RunResult {
    pub refused: bool,
    pub brew_status: Option<i32>,
}

pub fn combine_exit(refused: bool, brew_status: Option<i32>) -> i32;
// 0 if !refused && brew_status.unwrap_or(0)==0
// if refused && brew_status.unwrap_or(0) <= 1 → 1
// if brew_status > 1 → brew_status
```

`cmd::update`:

```rust
pub fn update(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    hours: SoakHours,
    now: time::OffsetDateTime,
    out: &mut impl Write,
) -> Result<(), Error>;
```

Calls `snapshot::refresh`, writes soak hours, four SHAs, and a one-line “snapshots refreshed”. Do not call `brew update`.

- [ ] **Step 1: Write the failing tests**

- Each refuse variant’s message contains `brew install` / `brew upgrade` as passed in `brew_verb` and does not mention `--now`
- `LeaveAheadOfSoak` → `refusal_message` is `None`
- `combine_exit(true, Some(0)) == 1`, `combine_exit(false, Some(0)) == 0`, `combine_exit(true, Some(3)) == 3`
- `update` with InMemoryGit+StaticGithub writes both SHAs to `out` and creates `state.toml`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cmd::tests`

Expected: FAIL.

- [ ] **Step 3: Implement messages, `combine_exit`, and `update`.**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cmd.rs src/lib.rs
git commit -m "feat: add refusal copy, exit combining, and update command"
```

---

### Task 13: `outdated` and `info`

**Files:**
- Modify: `src/cmd.rs`

**Interfaces:**
- Consumes: `Brew`, `GitStore`, snapshots, `resolve_blobs`, `desired_action`
- Produces:

```rust
pub fn outdated(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    out: &mut impl Write,
) -> Result<RunResult, Error>;

pub fn info(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    names: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error>;
```

If `snaps` would be missing, the caller refreshes (Task 16 wiring). These functions assume snapshots exist.

`outdated`: for each `installed_core()` package, resolve blobs, parse identities, `desired_action`. Print three sections:

```
==> Outdated (will upgrade)
name (installed_ver) < cutoff_ver

==> Held
name: <refusal why>

==> Ahead of soak
name
```

Omit empty sections. `RunResult.refused` is **false** (read-only; listing holds is not a refusal).

`info`: for each name, print name, installed identity (or “not installed”), cutoff identity (or “none”), HEAD identity (or “none”), and the action `brewsoakr` would take.

- [ ] **Step 1: Write the failing tests**

Mock three installed formulae:

1. installed identity = old, cutoff = mid, head = new → Outdated
2. installed = old, no cutoff, head = new → Held (too new)
3. installed = head, cutoff = mid → Ahead of soak

Assert the three section headers and names. `info` on package 1 mentions cutoff version and “install cutoff” / upgrade wording.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cmd::tests`

Expected: FAIL on new tests.

- [ ] **Step 3: Implement `outdated` and `info`.** Core vs cask repo dir: `cache/core.git` vs `cache/cask.git` from Task 8. Use `PkgKind` from the installed record.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cmd.rs
git commit -m "feat: show soaked outdated and info views"
```

---

### Task 14: `upgrade` and `install`

**Files:**
- Modify: `src/cmd.rs`
- Modify: `src/tap.rs` if a small helper is needed

**Interfaces:**
- Consumes: tap, brew, eligibility, resolve, snapshots
- Produces:

```rust
pub fn upgrade(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String], // empty = all installed core/cask
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error>;

pub fn install(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String],
    force_cask: bool,
    force_formula: bool,
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error>;
```

Shared inner `fn apply_one(...)` used by both:

1. If `is_third_party(name)` → `brew.run` the original verb + name + flags; do not soak.
2. Resolve kind: `force_cask` / `force_formula`, else try formula path then cask path at HEAD.
3. `resolve_blobs`, parse ids, `upstream_status`, `desired_action` (install: `installed` is Some only if that name is in `installed_core`).
4. Match action:
   - `NoOpAlreadySoaked` → print already-soaked / already installed, continue
   - `LeaveAheadOfSoak` → print `ahead_message`, continue (not refused)
   - `Refuse*` → print `refusal_message(..., brew_verb)`, mark refused
   - `InstallCutoff` → `write_blob` cutoff; `dep_closure`; for each dep, if not installed: if that dep is eligible write+`brew.run(install token)` **without** `--ignore-dependencies` on deps is ok; if dep missing and not eligible, refuse **target**; then `brew.run(brew_install_args(target, flags, true))`

Empty `upgrade` names → iterate `installed_core()`. Continue after brew failures; record the worst `brew_status`.

`install` with empty names → `Error::Usage`.

- [ ] **Step 1: Write the failing tests**

Use `MockBrew` + `InMemoryGit`:

1. Mixed upgrade: installed `ok` (behind, eligible) and `new` (too new). After `upgrade([])`, `MockBrew.runs` contains an install of `brewsoakr/soaked/ok` with `--ignore-dependencies`, does not contain `new`, `RunResult.refused == true`.
2. Ahead-of-soak only: `refused == false`, no install run, output contains ahead message.
3. `install(["fresh"])` eligible, not installed → install run, `refused == false`.
4. `install(["fresh"])` too new → no install, output contains `brew install fresh`, `refused == true`.
5. Target eligible, dep `lib` missing and yanked → refuse target, no target install.
6. Third-party `acme/tools/foo` on upgrade → `brew.run` args contain `acme/tools/foo` and not `brewsoakr/soaked`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cmd::tests`

Expected: FAIL.

- [ ] **Step 3: Implement `apply_one`, `upgrade`, and `install`.** Call `brew.tap_new_soaked()` once per command if any install will run.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cmd.rs src/tap.rs
git commit -m "feat: soak-aware upgrade and install via local tap"
```

---

### Task 15: `reinstall`

**Files:**
- Modify: `src/cmd.rs`

**Interfaces:**
- Consumes: same as install
- Produces:

```rust
pub fn reinstall(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error>;
```

For each name (required; empty → `Error::Usage`):

- Not installed → `Error::Refusal` message like brew (“reinstall: no installed keg”) counting as refused.
- Installed identity **equals HEAD identity** → `brew.run(["reinstall", ...flags, name])` (not the soaked tap). This is the true-repair passthrough.
- Else treat as `apply_one` with verb `reinstall` but mutating path still installs the **cutoff** artifact (same as install/upgrade), not `brew reinstall` of HEAD. If desired action is `LeaveAheadOfSoak` or `NoOpAlreadySoaked` where cutoff identity equals installed (already soaked, HEAD differs — wait: if installed == cutoff and HEAD differs, identity==HEAD is false, so we are in the else branch; desired action is `NoOpAlreadySoaked`. Do **not** call `brew reinstall`. Print already soaked. If `LeaveAheadOfSoak`, refuse with a message that reinstall would pull a too-new artifact and to use `brew reinstall <name>`.)

Clarify:

| Installed vs HEAD vs cutoff | Behavior |
|---|---|
| installed == HEAD (true repair) | `exec`/`run` `brew reinstall <name>` |
| installed == cutoff (already soaked, HEAD newer) | No-op, not a refusal |
| installed == HEAD is the true-repair row; if HEAD != cutoff this **is** ahead of soak and installed==HEAD → **true repair wins** (user asked to repair the artifact they have). Spec: “installed identity equals HEAD → brew reinstall”. Do that even when HEAD is newer than cutoff. |
| else eligible behind | install cutoff via tap |
| else refuse | refuse + `brew reinstall` hint |

- [ ] **Step 1: Write the failing tests**

1. installed == HEAD != cutoff → `brew reinstall wget` in `runs`, no tap token.
2. installed == cutoff != HEAD → no brew run, not refused.
3. installed == old, eligible → tap install cutoff with `--ignore-dependencies`.
4. not installed → refused, no run.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cmd::tests::reinstall`

Name the tests `reinstall_true_repair`, `reinstall_already_soaked`, `reinstall_behind`, `reinstall_missing`.

Expected: FAIL.

- [ ] **Step 3: Implement `reinstall`.**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cmd.rs
git commit -m "feat: reinstall true-repair passthrough and soaked fallback"
```

---

### Task 16: `main` wiring

**Files:**
- Modify: `src/main.rs`
- Modify: `src/cmd.rs` if you need `fn ensure_snapshots(...)` 
- Modify: `src/lib.rs` (`pub fn run(...)` for integration from main)

**Interfaces:**
- Consumes: every public entry above
- Produces: `brewsoakr::run(args: &[String]) -> i32` so tests can drive the process without `exec`.

```rust
pub fn run(args: &[String]) -> i32 {
    match run_inner(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("brewsoakr: {e}");
            e.exit_code()
        }
    }
}
```

`run_inner`:

1. `parse_argv(args)`
2. `env = std::env::var("BREWSOAK_SOAK_HOURS").ok()`
3. `file = config::read_file(&paths::config_file())`
4. `resolved = resolve_hours(inv.soak_hours, env.as_deref(), file.as_deref())?`
5. `apply_persist(resolved.persist, &paths::config_file())?`
6. Match command:
   - `Passthrough` → `passthrough_exec` from `main` only; `run()` in tests should return a distinguished path: implement `run_inner` returning `Err(Error::Other("passthrough"))` is wrong. Instead `run` takes an optional `Brew`/`Git` only in lib tests. Keep `main` as:

```rust
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match brewsoakr::dispatch(&args, RealWorld) {
        Ok(Dispatch::Exit(c)) => c,
        Ok(Dispatch::Exec(bin, argv)) => brewsoakr::brew::exec(&bin, &argv),
        Err(e) => {
            eprintln!("brewsoakr: {e}");
            e.exit_code()
        }
    };
    std::process::exit(code);
}
```

```rust
pub enum Dispatch {
    Exit(i32),
    Exec(PathBuf, Vec<String>),
}

pub struct RealWorld; // ProcessGit + UreqGithub + ProcessBrew + now=OffsetDateTime::now_utc()
```

Refresh policy:

- `Update` → always `snapshot::refresh`
- `Upgrade` / `Install` / `Reinstall` → always refresh
- `Outdated` / `Info` → `load_state` or refresh if `None`

Prefetch (installed cutoff+HEAD blobs) can be a loop in `refresh` callers using `installed_core` + `resolve_blobs` (cache warm). Skip if `installed_core` fails; still proceed.

- [ ] **Step 1: Write the failing tests**

In `src/lib.rs` or `src/cmd.rs` tests, build a `World` trait object / generic `dispatch(&args, world)`:

1. `["services", "start", "foo"]` → `Dispatch::Exec` whose argv is `["services", "start", "foo"]` (no `--soak-hours`).
2. `["--soak-hours", "48", "services", "start", "x"]` → persist file `SOAK_HOURS = 48` under a temp HOME, and Exec without the flag.
3. `["outdated"]` with empty snapshots + mock world that records a refresh → refresh happened, exit 0.

Use `tempfile` and set `HOME` / `XDG_CACHE_HOME` via the `World` providing paths, **not** by mutating process-global env if that races tests. Give `World` `fn config_path`, `fn cache_path`, `fn env_soak`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib`

Expected: FAIL (missing `dispatch`).

- [ ] **Step 3: Implement `World` trait, `RealWorld`, `dispatch`, and `main`.** `Error::Brew` from commands maps through `combine_exit`. `dispatch` returns `Dispatch::Exit(combine_exit(...))` for soaked commands.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib && cargo build`

Expected: PASS, binary at `target/debug/brewsoakr`.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/lib.rs src/cmd.rs
git commit -m "feat: wire brewsoakr dispatch, persist, and exec passthrough"
```

---

### Task 17: Lint, full test, rustfmt

**Files:**
- Modify: any that fail fmt/clippy

**Interfaces:**
- Consumes: the crate
- Produces: clean `cargo test`, `cargo fmt`, `cargo clippy --all-targets -- -D warnings`

- [ ] **Step 1: Run the full test suite**

Run: `cargo test`

Expected: all unit tests PASS. Zero ignored tests required.

- [ ] **Step 2: Format**

Run: `cargo fmt --all`

- [ ] **Step 3: Clippy**

Run: `cargo clippy --all-targets -- -D warnings`

Fix every finding. Do not add `#[allow]` unless clippy is wrong and a one-line comment explains why.

- [ ] **Step 4: Re-run tests**

Run: `cargo test && cargo build`

Expected: PASS.

- [ ] **Step 5: Commit if anything changed**

```bash
git add -u
git commit -m "chore: fmt and clippy cleanups"
```

If the tree is clean, skip the commit.

---

## Plan self-review

**Spec coverage**

| Spec item | Task |
|---|---|
| `--soak-hours`, `BREWSOAK_SOAK_HOURS`, `SOAK_HOURS`, default 24, precedence | 2, 3, 16 |
| Silent invalid file/env; CLI usage error | 2, 3 |
| Persist CLI ≠24, delete ==24 | 2, 16 |
| Soaked vs passthrough commands | 3, 16 |
| Third-party passthrough | 9, 14, 16 |
| Two snapshots, prune, HTTPS | 6, 7, 8 |
| Cutoff via GitHub `until`, fallback shallow | 7, 8 |
| Raw blobs cutoff vs HEAD; identity for receipts | 4, 5 |
| `deprecate!` / `disable!` | 4, 5 |
| Desired-state table, no downgrade | 5, 14 |
| Mixed upgrade exit 1 | 12, 14 |
| Refusal copy points at `brew` | 12, 14 |
| `update` does not update Homebrew | 12 |
| `outdated` / `info` soaked view | 13 |
| `upgrade` / `install` tap + `--ignore-dependencies` | 11, 14 |
| Dep missing ineligible refuses target | 14 |
| `reinstall` true repair vs soak | 15 |
| Refresh policy | 16 |
| Exit 0/1/2 and brew `> 1` | 1, 12, 16 |
| Unit tests mocked, no network | all tasks |
| Cache `~/Library/Caches/brewsoak` or `$XDG_CACHE_HOME/brewsoak` | 2, 8 |
| Config `~/.config/brewsoak/config.toml` | 2 |
| Prefetch installed blobs | 16 |

**Placeholder scan:** none remaining. Shallow-since fallback is specified in Task 8, not TBD.

**Type consistency:** `SoakHours`, `Error`, `PersistAction`, `Invocation`/`Command`, `PkgIdentity`, `UpstreamStatus`, `DesiredAction`, `GitStore`, `GithubApi`, `Brew`, `PkgRef`/`PkgKind`, `Snapshots`, `Dispatch` are named the same in every later task.
