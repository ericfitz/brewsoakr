use crate::git::{GitStore, REF_CUTOFF, REF_HEAD};
use crate::github::{GithubApi, cutoff_instant};
use crate::taps;
use crate::{Error, SoakHours};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const CORE_REMOTE: &str = "https://github.com/Homebrew/homebrew-core";
pub const CASK_REMOTE: &str = "https://github.com/Homebrew/homebrew-cask";
pub const CORE_REPO: &str = "Homebrew/homebrew-core";
pub const CASK_REPO: &str = "Homebrew/homebrew-cask";
/// Name of the remote inside every tap clone (spec issue 1).
pub const TAP_REMOTE_NAME: &str = "origin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapSnapshot {
    pub cutoff_sha: String,
    pub head_sha: String,
    pub cutoff_time: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapState {
    pub hours: SoakHours,
    /// `None` when no commit is older than the tap's cutoff.
    pub cutoff_sha: Option<String>,
    pub head_sha: String,
    pub cutoff_time: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshots {
    pub core: TapSnapshot,
    pub cask: TapSnapshot,
    pub hours: SoakHours,
    pub core_hours: SoakHours,
    pub cask_hours: SoakHours,
    pub taps: BTreeMap<String, TapState>,
    /// Taps whose refresh failed this run (`Error::Git` text). Not persisted,
    /// so the next run tries again.
    pub held_taps: BTreeMap<String, String>,
}

impl Snapshots {
    pub fn core_only(core: TapSnapshot, cask: TapSnapshot, hours: SoakHours) -> Self {
        Self {
            core,
            cask,
            hours,
            core_hours: hours,
            cask_hours: hours,
            taps: BTreeMap::new(),
            held_taps: BTreeMap::new(),
        }
    }

    pub fn tap(&self, name: &str) -> Option<&TapState> {
        self.taps.get(&name.to_ascii_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapPlan {
    pub name: String,
    pub remote: String,
    pub hours: SoakHours,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshPlan {
    pub hours: SoakHours,
    pub core_hours: SoakHours,
    pub cask_hours: SoakHours,
    pub taps: Vec<TapPlan>,
}

impl RefreshPlan {
    pub fn uniform(hours: SoakHours) -> Self {
        Self {
            hours,
            core_hours: hours,
            cask_hours: hours,
            taps: Vec::new(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TapStateFile {
    hours: u32,
    cutoff: String,
    head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cutoff_time: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StateFile {
    hours: u32,
    #[serde(default)]
    core_hours: Option<u32>,
    #[serde(default)]
    cask_hours: Option<u32>,
    core_cutoff: String,
    core_head: String,
    cask_cutoff: String,
    cask_head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    core_cutoff_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cask_cutoff_time: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    taps: BTreeMap<String, TapStateFile>,
}

pub fn refresh(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    hours: SoakHours,
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    refresh_with(git, gh, cache, &RefreshPlan::uniform(hours), now, progress)
}

/// Core and cask failures abort (v1). A tap failure holds only that tap.
pub fn refresh_with(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    plan: &RefreshPlan,
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    std::fs::create_dir_all(cache)?;
    writeln!(progress, "fetching {CORE_REPO}…")?;
    let core = refresh_core_tap(
        git,
        gh,
        &cache.join("core.git"),
        CORE_REMOTE,
        CORE_REPO,
        cutoff_instant(now, plan.core_hours),
    )?;
    writeln!(progress, "fetching {CASK_REPO}…")?;
    let cask = refresh_core_tap(
        git,
        gh,
        &cache.join("cask.git"),
        CASK_REMOTE,
        CASK_REPO,
        cutoff_instant(now, plan.cask_hours),
    )?;
    let mut snaps = Snapshots {
        core,
        cask,
        hours: plan.hours,
        core_hours: plan.core_hours,
        cask_hours: plan.cask_hours,
        taps: BTreeMap::new(),
        held_taps: BTreeMap::new(),
    };
    refresh_taps(git, cache, &mut snaps, &plan.taps, now, progress)?;
    Ok(snaps)
}

pub fn refresh_taps(
    git: &impl GitStore,
    cache: &Path,
    snaps: &mut Snapshots,
    taps: &[TapPlan],
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<(), Error> {
    for tap in taps {
        let name = tap.name.to_ascii_lowercase();
        writeln!(progress, "fetching {name}…")?;
        match refresh_tap(git, cache, tap, now) {
            Ok(state) => {
                snaps.held_taps.remove(&name);
                snaps.taps.insert(name, state);
            }
            Err(e @ Error::Git { .. }) => {
                snaps.taps.remove(&name);
                snaps.held_taps.insert(name, e.to_string());
            }
            Err(e) => return Err(e),
        }
    }
    write_state(cache, snaps)
}

pub fn refresh_tap(
    git: &impl GitStore,
    cache: &Path,
    tap: &TapPlan,
    now: OffsetDateTime,
) -> Result<TapState, Error> {
    let dir = taps::clone_dir(cache, &tap.name.to_ascii_lowercase());
    std::fs::create_dir_all(dir.parent().unwrap_or(cache))?;
    git.init_bare(&dir)?;
    git.set_remote(&dir, TAP_REMOTE_NAME, &tap.remote)?;
    git.fetch_history(&dir, TAP_REMOTE_NAME, REF_HEAD)?;
    let head_sha = git.rev_parse(&dir, REF_HEAD)?.ok_or_else(|| Error::Git {
        action: format!("resolving the fetched head of {}", tap.name),
        detail: format!("{REF_HEAD} is missing after fetch"),
    })?;
    let until = cutoff_instant(now, tap.hours).unix_timestamp();
    let cutoff = git.rev_list_before(&dir, REF_HEAD, until)?;
    let (cutoff_sha, cutoff_time) = match cutoff {
        Some((sha, when)) => {
            git.update_ref(&dir, REF_CUTOFF, &sha)?;
            (Some(sha), OffsetDateTime::from_unix_timestamp(when).ok())
        }
        None => (None, None),
    };
    Ok(TapState {
        hours: tap.hours,
        cutoff_sha,
        head_sha,
        cutoff_time,
    })
}

pub fn tap_needs_refresh(snaps: &Snapshots, name: &str, hours: SoakHours) -> bool {
    match snaps.tap(name) {
        Some(state) => state.hours != hours,
        None => true,
    }
}

pub fn load_state(cache: &Path) -> Result<Option<Snapshots>, Error> {
    let path = cache.join("state.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let parsed: StateFile =
        toml::from_str(&raw).map_err(|e| Error::Other(format!("state.toml: {e}")))?;
    let hours_of = |n: u32| {
        SoakHours::new(n).ok_or_else(|| Error::Other(format!("invalid hours in state.toml: {n}")))
    };
    let hours = hours_of(parsed.hours)?;
    let mut taps = BTreeMap::new();
    for (name, t) in parsed.taps {
        taps.insert(
            name.to_ascii_lowercase(),
            TapState {
                hours: hours_of(t.hours)?,
                cutoff_sha: (!t.cutoff.is_empty()).then_some(t.cutoff),
                head_sha: t.head,
                cutoff_time: parse_state_time(t.cutoff_time.as_deref()),
            },
        );
    }
    Ok(Some(Snapshots {
        core: TapSnapshot {
            cutoff_sha: parsed.core_cutoff,
            head_sha: parsed.core_head,
            cutoff_time: parse_state_time(parsed.core_cutoff_time.as_deref()),
        },
        cask: TapSnapshot {
            cutoff_sha: parsed.cask_cutoff,
            head_sha: parsed.cask_head,
            cutoff_time: parse_state_time(parsed.cask_cutoff_time.as_deref()),
        },
        hours,
        core_hours: parsed
            .core_hours
            .map(hours_of)
            .transpose()?
            .unwrap_or(hours),
        cask_hours: parsed
            .cask_hours
            .map(hours_of)
            .transpose()?
            .unwrap_or(hours),
        taps,
        held_taps: BTreeMap::new(),
    }))
}

fn parse_state_time(raw: Option<&str>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw?, &Rfc3339).ok()
}

fn fmt_time(t: Option<OffsetDateTime>) -> Option<String> {
    t.and_then(|t| t.format(&Rfc3339).ok())
}

fn write_state(cache: &Path, snaps: &Snapshots) -> Result<(), Error> {
    let file = StateFile {
        hours: snaps.hours.get(),
        core_hours: Some(snaps.core_hours.get()),
        cask_hours: Some(snaps.cask_hours.get()),
        core_cutoff: snaps.core.cutoff_sha.clone(),
        core_head: snaps.core.head_sha.clone(),
        cask_cutoff: snaps.cask.cutoff_sha.clone(),
        cask_head: snaps.cask.head_sha.clone(),
        core_cutoff_time: fmt_time(snaps.core.cutoff_time),
        cask_cutoff_time: fmt_time(snaps.cask.cutoff_time),
        taps: snaps
            .taps
            .iter()
            .map(|(name, t)| {
                (
                    name.clone(),
                    TapStateFile {
                        hours: t.hours.get(),
                        cutoff: t.cutoff_sha.clone().unwrap_or_default(),
                        head: t.head_sha.clone(),
                        cutoff_time: fmt_time(t.cutoff_time),
                    },
                )
            })
            .collect(),
    };
    let body = toml::to_string(&file).map_err(|e| Error::Other(format!("state.toml: {e}")))?;
    std::fs::write(cache.join("state.toml"), body)?;
    Ok(())
}

fn refresh_core_tap(
    git: &impl GitStore,
    gh: &impl GithubApi,
    dir: &Path,
    remote: &str,
    repo: &str,
    until: OffsetDateTime,
) -> Result<TapSnapshot, Error> {
    git.init_bare(dir)?;
    let head_sha = gh.head_sha(repo)?;
    let (cutoff_sha, cutoff_time) = match gh.latest_commit_until(repo, until) {
        Ok(info) => (info.sha, Some(info.committer_time)),
        Err(_) => (cutoff_via_shallow(git, dir, remote, until)?, None),
    };
    git.fetch_depth1(dir, remote, &cutoff_sha, REF_CUTOFF)?;
    git.fetch_depth1(dir, remote, &head_sha, REF_HEAD)?;
    git.gc_prune(dir)?;
    Ok(TapSnapshot {
        cutoff_sha,
        head_sha,
        cutoff_time,
    })
}

/// Resolve cutoff via a forced shallow fetch when GitHub lookup fails.
/// Only ProcessGit can shallow-fetch; InMemoryGit returns `Error::Git`.
fn cutoff_via_shallow(
    git: &impl GitStore,
    dir: &Path,
    remote: &str,
    until: OffsetDateTime,
) -> Result<String, Error> {
    let unix = until.unix_timestamp();
    git.fetch_shallow_since(dir, remote, unix)?;
    git.log_sha_before(dir, unix)?.ok_or_else(|| Error::Git {
        action: "looking up the last commit at or before the soak cutoff".into(),
        detail: "git returned no commit in the fetched window".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::InMemoryGit;
    use crate::github::{CommitInfo, StaticGithub};
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("fixed now")
    }

    fn fixture_gh() -> StaticGithub {
        let now = now();
        StaticGithub {
            head: "headsha".into(),
            commits: vec![
                CommitInfo {
                    sha: "headsha".into(),
                    committer_time: now - Duration::hours(2),
                },
                CommitInfo {
                    sha: "tenh".into(),
                    committer_time: now - Duration::hours(10),
                },
                CommitInfo {
                    sha: "thirtyh".into(),
                    committer_time: now - Duration::hours(30),
                },
            ],
        }
    }

    #[test]
    fn refresh_sets_core_cutoff_to_pre_soak_commit() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let hours = SoakHours::new(24).expect("hours >= 1");
        let snaps = refresh(
            &git,
            &fixture_gh(),
            dir.path(),
            hours,
            now(),
            &mut std::io::sink(),
        )
        .expect("refresh");
        assert_eq!(snaps.core.cutoff_sha, "thirtyh");
        assert_eq!(snaps.core.head_sha, "headsha");
        assert_eq!(snaps.cask.cutoff_sha, "thirtyh");
        assert_eq!(snaps.cask.head_sha, "headsha");
        assert_eq!(snaps.hours, hours);
    }

    #[test]
    fn load_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_state(dir.path()).expect("missing state").is_none());

        let git = InMemoryGit::new();
        let hours = SoakHours::new(24).expect("hours >= 1");
        let snaps = refresh(
            &git,
            &fixture_gh(),
            dir.path(),
            hours,
            now(),
            &mut std::io::sink(),
        )
        .expect("refresh");
        let loaded = load_state(dir.path())
            .expect("load")
            .expect("state.toml written");
        assert_eq!(loaded.core.cutoff_sha, snaps.core.cutoff_sha);
        assert_eq!(loaded.core.head_sha, snaps.core.head_sha);
        assert_eq!(loaded.cask.cutoff_sha, snaps.cask.cutoff_sha);
        assert_eq!(loaded.cask.head_sha, snaps.cask.head_sha);
        assert_eq!(loaded.hours, snaps.hours);

        let raw = std::fs::read_to_string(dir.path().join("state.toml")).expect("read state");
        assert!(raw.contains("hours = 24"), "{raw}");
        assert!(!raw.contains("SOAK_HOURS"), "{raw}");
    }

    #[test]
    fn smaller_hours_moves_cutoff_forward() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let gh = fixture_gh();

        let first = refresh(
            &git,
            &gh,
            dir.path(),
            SoakHours::new(24).expect("24"),
            now(),
            &mut std::io::sink(),
        )
        .expect("refresh 24h");
        assert_eq!(first.core.cutoff_sha, "thirtyh");

        let second = refresh(
            &git,
            &gh,
            dir.path(),
            SoakHours::new(8).expect("8"),
            now(),
            &mut std::io::sink(),
        )
        .expect("refresh 8h");
        assert_eq!(second.core.cutoff_sha, "tenh");
        assert_eq!(second.core.head_sha, "headsha");
        assert_eq!(second.hours.get(), 8);

        let loaded = load_state(dir.path())
            .expect("load")
            .expect("state.toml written");
        assert_eq!(loaded.core.cutoff_sha, "tenh");
        assert_eq!(loaded.hours.get(), 8);
    }

    fn tap_plan(name: &str, remote: &str, hours: u32) -> TapPlan {
        TapPlan {
            name: name.into(),
            remote: remote.into(),
            hours: SoakHours::new(hours).unwrap(),
        }
    }

    fn tap_git(now: OffsetDateTime) -> InMemoryGit {
        let git = InMemoryGit::new();
        let n = now.unix_timestamp();
        git.insert_commits(
            "https://github.com/hashicorp/homebrew-tap",
            &[
                ("hc3", n - 3600),
                ("hc2", n - 50 * 3600),
                ("hc1", n - 100 * 3600),
            ],
        );
        git.insert_commits(
            "https://github.com/cyclonedx/homebrew-cyclonedx",
            &[("cy2", n - 3600), ("cy1", n - 30 * 3600)],
        );
        git.insert_commits("https://github.com/young/homebrew-tap", &[("y1", n - 3600)]);
        git
    }

    #[test]
    fn refresh_tap_pins_head_and_rev_list_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let state = refresh_tap(
            &git,
            dir.path(),
            &tap_plan(
                "hashicorp/tap",
                "https://github.com/hashicorp/homebrew-tap",
                72,
            ),
            now(),
        )
        .expect("refresh tap");
        assert_eq!(state.head_sha, "hc3");
        assert_eq!(
            state.cutoff_sha.as_deref(),
            Some("hc1"),
            "72h cutoff skips the 50h commit"
        );
        assert_eq!(state.hours.get(), 72);
        assert_eq!(state.cutoff_time, Some(now() - Duration::hours(100)));
        let clone = crate::taps::clone_dir(dir.path(), "hashicorp/tap");
        assert_eq!(
            git.remote_url(&clone).as_deref(),
            Some("https://github.com/hashicorp/homebrew-tap")
        );
        assert_eq!(git.rev_parse(&clone, REF_HEAD).unwrap(), Some("hc3".into()));
        assert_eq!(
            git.rev_parse(&clone, REF_CUTOFF).unwrap(),
            Some("hc1".into())
        );
    }

    #[test]
    fn per_tap_hours_give_different_cutoffs() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let a = refresh_tap(
            &git,
            dir.path(),
            &tap_plan(
                "hashicorp/tap",
                "https://github.com/hashicorp/homebrew-tap",
                24,
            ),
            now(),
        )
        .unwrap();
        let b = refresh_tap(
            &git,
            dir.path(),
            &tap_plan(
                "hashicorp/tap",
                "https://github.com/hashicorp/homebrew-tap",
                72,
            ),
            now(),
        )
        .unwrap();
        assert_eq!(a.cutoff_sha.as_deref(), Some("hc2"));
        assert_eq!(b.cutoff_sha.as_deref(), Some("hc1"));
    }

    #[test]
    fn tap_younger_than_window_has_no_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let state = refresh_tap(
            &git,
            dir.path(),
            &tap_plan("young/tap", "https://github.com/young/homebrew-tap", 24),
            now(),
        )
        .unwrap();
        assert_eq!(state.head_sha, "y1");
        assert_eq!(state.cutoff_sha, None);
        assert_eq!(state.cutoff_time, None);
    }

    #[test]
    fn one_failing_tap_is_held_and_others_continue() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        git.fail_remote("https://github.com/hashicorp/homebrew-tap");
        let plan = RefreshPlan {
            taps: vec![
                tap_plan(
                    "hashicorp/tap",
                    "https://github.com/hashicorp/homebrew-tap",
                    72,
                ),
                tap_plan(
                    "cyclonedx/cyclonedx",
                    "https://github.com/cyclonedx/homebrew-cyclonedx",
                    24,
                ),
            ],
            ..RefreshPlan::uniform(SoakHours::new(24).unwrap())
        };
        let mut progress = Vec::new();
        let snaps = refresh_with(&git, &fixture_gh(), dir.path(), &plan, now(), &mut progress)
            .expect("core/cask ok");
        assert!(
            snaps
                .taps
                .get("cyclonedx/cyclonedx")
                .is_some_and(|t| t.cutoff_sha.as_deref() == Some("cy1"))
        );
        assert!(!snaps.taps.contains_key("hashicorp/tap"));
        let held = snaps.held_taps.get("hashicorp/tap").expect("held");
        assert!(held.contains("fetching history"), "{held}");
        let text = String::from_utf8(progress).unwrap();
        assert!(text.contains("fetching hashicorp/tap"), "{text}");
        assert!(text.contains("fetching cyclonedx/cyclonedx"), "{text}");
    }

    #[test]
    fn state_round_trips_taps_and_per_origin_hours() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let plan = RefreshPlan {
            hours: SoakHours::new(24).unwrap(),
            core_hours: SoakHours::new(8).unwrap(),
            cask_hours: SoakHours::new(24).unwrap(),
            taps: vec![
                tap_plan(
                    "hashicorp/tap",
                    "https://github.com/hashicorp/homebrew-tap",
                    72,
                ),
                tap_plan("young/tap", "https://github.com/young/homebrew-tap", 24),
            ],
        };
        let snaps = refresh_with(
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(snaps.core.cutoff_sha, "tenh", "core uses its own 8h window");
        assert_eq!(snaps.cask.cutoff_sha, "thirtyh");
        let raw = std::fs::read_to_string(dir.path().join("state.toml")).unwrap();
        assert!(raw.contains("[taps.\"hashicorp/tap\"]"), "{raw}");
        assert!(raw.contains("core_hours = 8"), "{raw}");
        let loaded = load_state(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, snaps);
        assert_eq!(loaded.tap("young/tap").unwrap().cutoff_sha, None);
        assert_eq!(loaded.tap("hashicorp/tap").unwrap().hours.get(), 72);
    }

    #[test]
    fn v1_state_file_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("state.toml"),
            "hours = 24\ncore_cutoff = \"a\"\ncore_head = \"b\"\ncask_cutoff = \"c\"\ncask_head = \"d\"\ncore_cutoff_time = \"2023-11-13T22:13:20Z\"\n",
        )
        .unwrap();
        let s = load_state(dir.path()).unwrap().unwrap();
        assert_eq!(s.core_hours.get(), 24);
        assert_eq!(s.cask_hours.get(), 24);
        assert!(s.taps.is_empty());
        assert!(s.held_taps.is_empty());
        assert!(s.core.cutoff_time.is_some());
    }

    #[test]
    fn tap_needs_refresh_when_missing_or_hours_differ() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let plan = RefreshPlan {
            taps: vec![tap_plan(
                "hashicorp/tap",
                "https://github.com/hashicorp/homebrew-tap",
                72,
            )],
            ..RefreshPlan::uniform(SoakHours::new(24).unwrap())
        };
        let mut snaps = refresh_with(
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert!(!tap_needs_refresh(
            &snaps,
            "hashicorp/tap",
            SoakHours::new(72).unwrap()
        ));
        assert!(tap_needs_refresh(
            &snaps,
            "hashicorp/tap",
            SoakHours::new(24).unwrap()
        ));
        assert!(tap_needs_refresh(
            &snaps,
            "cyclonedx/cyclonedx",
            SoakHours::new(24).unwrap()
        ));
        refresh_taps(
            &git,
            dir.path(),
            &mut snaps,
            &[tap_plan(
                "cyclonedx/cyclonedx",
                "https://github.com/cyclonedx/homebrew-cyclonedx",
                24,
            )],
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert!(snaps.tap("cyclonedx/cyclonedx").is_some());
        assert!(
            snaps.tap("hashicorp/tap").is_some(),
            "existing tap state kept"
        );
        let loaded = load_state(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.taps.len(), 2, "refresh_taps persists");
    }

    #[test]
    fn mixed_case_tap_names_are_lowercased_for_state_and_clone() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let mut snaps = Snapshots::core_only(
            TapSnapshot {
                cutoff_sha: "a".into(),
                head_sha: "b".into(),
                cutoff_time: None,
            },
            TapSnapshot {
                cutoff_sha: "c".into(),
                head_sha: "d".into(),
                cutoff_time: None,
            },
            SoakHours::new(24).unwrap(),
        );
        refresh_taps(
            &git,
            dir.path(),
            &mut snaps,
            &[tap_plan(
                "HashiCorp/Tap",
                "https://github.com/hashicorp/homebrew-tap",
                72,
            )],
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert!(
            snaps.taps.contains_key("hashicorp/tap"),
            "{:?}",
            snaps.taps.keys()
        );
        assert!(!snaps.taps.contains_key("HashiCorp/Tap"));
        assert!(snaps.tap("HashiCorp/Tap").is_some());
        let clone = crate::taps::clone_dir(dir.path(), "hashicorp/tap");
        assert_eq!(git.rev_parse(&clone, REF_HEAD).unwrap(), Some("hc3".into()));
        assert!(tap_needs_refresh(
            &snaps,
            "HashiCorp/Tap",
            SoakHours::new(24).unwrap()
        ));
        assert!(!tap_needs_refresh(
            &snaps,
            "HashiCorp/Tap",
            SoakHours::new(72).unwrap()
        ));
    }
}
