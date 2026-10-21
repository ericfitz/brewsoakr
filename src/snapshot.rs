use crate::git::{GitStore, REF_CUTOFF, REF_HEAD};
use crate::github::{GithubApi, cutoff_instant};
use crate::lock::RunLock;
use crate::section::Section;
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
    /// Taps whose refresh failed this run (`Error::Git` text). Not persisted.
    /// Such a tap may still have a (stale) entry in `taps`; consumers check
    /// this map first.
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
    lock: &RunLock,
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    hours: SoakHours,
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    refresh_with(
        lock,
        git,
        gh,
        cache,
        &RefreshPlan::uniform(hours),
        now,
        progress,
    )
}

/// Core and cask failures abort (v1). A tap failure holds only that tap.
pub fn refresh_with(
    lock: &RunLock,
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    plan: &RefreshPlan,
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    std::fs::create_dir_all(cache)?;
    let mut fetching = Section::new(&mut *progress, "Fetching Homebrew history");
    writeln!(fetching, "{CORE_REPO}")?;
    let core = refresh_core_tap(
        git,
        gh,
        &cache.join("core.git"),
        CORE_REMOTE,
        CORE_REPO,
        cutoff_instant(now, plan.core_hours),
    )?;
    writeln!(fetching, "{CASK_REPO}")?;
    let cask = refresh_core_tap(
        git,
        gh,
        &cache.join("cask.git"),
        CASK_REMOTE,
        CASK_REPO,
        cutoff_instant(now, plan.cask_hours),
    )?;
    drop(fetching);
    let mut snaps = Snapshots {
        core,
        cask,
        hours: plan.hours,
        core_hours: plan.core_hours,
        cask_hours: plan.cask_hours,
        taps: seeded_taps(cache, &plan.taps),
        held_taps: BTreeMap::new(),
    };
    refresh_taps(lock, git, cache, &mut snaps, &plan.taps, now, progress)?;
    // The new cutoffs are recorded; only now may the old cutoffs' objects go.
    git.gc_prune(&cache.join("core.git"))?;
    git.gc_prune(&cache.join("cask.git"))?;
    Ok(snaps)
}

/// The last good table of each planned tap, so a failed fetch keeps it. Taps
/// outside the plan (untapped) are not carried over. An unreadable
/// `state.toml` seeds nothing: this refresh rewrites it.
fn seeded_taps(cache: &Path, planned: &[TapPlan]) -> BTreeMap<String, TapState> {
    let Ok(Some(previous)) = load_state(cache) else {
        return BTreeMap::new();
    };
    planned
        .iter()
        .filter_map(|tap| {
            let name = tap.name.to_ascii_lowercase();
            let state = previous.taps.get(&name)?.clone();
            Some((name, state))
        })
        .collect()
}

pub fn refresh_taps(
    _lock: &RunLock,
    git: &impl GitStore,
    cache: &Path,
    snaps: &mut Snapshots,
    taps: &[TapPlan],
    now: OffsetDateTime,
    progress: &mut impl Write,
) -> Result<(), Error> {
    let mut fetching = Section::new(&mut *progress, "Fetching tap history");
    for tap in taps {
        let name = tap.name.to_ascii_lowercase();
        writeln!(fetching, "{name}")?;
        match refresh_tap(git, cache, tap, now) {
            Ok(state) => {
                snaps.held_taps.remove(&name);
                snaps.taps.insert(name, state);
            }
            Err(e @ Error::Git { .. }) => {
                // The last good table (if any) stays, in memory and on disk;
                // `held_taps` is what keeps this run from using it for installs.
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
        None => {
            // Nothing is old enough this run: a pin left from an earlier run
            // would anchor a commit that is no longer the cutoff.
            git.delete_ref(&dir, REF_CUTOFF)?;
            (None, None)
        }
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
    crate::atomic::write(&cache.join("state.toml"), &body)
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
    // No gc here: state.toml still names the previous cutoff until the whole
    // refresh succeeds, so its objects must stay readable until then.
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
            &RunLock::for_tests(),
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
            &RunLock::for_tests(),
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
            &RunLock::for_tests(),
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
            &RunLock::for_tests(),
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
    fn tap_with_no_cutoff_drops_a_stale_cutoff_pin() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let clone = crate::taps::clone_dir(dir.path(), "young/tap");
        git.update_ref(&clone, REF_CUTOFF, "old").unwrap();
        let plan = tap_plan("young/tap", "https://github.com/young/homebrew-tap", 24);
        let state = refresh_tap(&git, dir.path(), &plan, now()).expect("refresh tap");
        assert_eq!(state.cutoff_sha, None);
        assert_eq!(git.rev_parse(&clone, REF_CUTOFF).unwrap(), None);
        assert_eq!(git.rev_parse(&clone, REF_HEAD).unwrap(), Some("y1".into()));
        // A second run has no pin to remove and still succeeds.
        refresh_tap(&git, dir.path(), &plan, now()).expect("second refresh");
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
        let snaps = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut progress,
        )
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
        assert!(text.contains("\n  hashicorp/tap\n"), "{text}");
        assert!(text.contains("\n  cyclonedx/cyclonedx\n"), "{text}");
    }

    #[test]
    fn fetch_sections_have_headers() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
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
        refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut progress,
        )
        .expect("refresh");
        assert_eq!(
            String::from_utf8(progress).unwrap(),
            "Fetching Homebrew history\n  Homebrew/homebrew-core\n  Homebrew/homebrew-cask\n\
             Fetching tap history\n  hashicorp/tap\n  cyclonedx/cyclonedx\n"
        );
    }

    #[test]
    fn no_taps_no_tap_header() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let mut progress = Vec::new();
        refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &RefreshPlan::uniform(SoakHours::new(24).unwrap()),
            now(),
            &mut progress,
        )
        .expect("refresh");
        assert_eq!(
            String::from_utf8(progress).unwrap(),
            "Fetching Homebrew history\n  Homebrew/homebrew-core\n  Homebrew/homebrew-cask\n"
        );
    }

    /// `StaticGithub` whose commit lookups for `fail_repo` fail, like an
    /// outage that hits the cask fetch after core has already moved.
    struct FailRepoGithub {
        inner: StaticGithub,
        fail_repo: &'static str,
    }

    impl GithubApi for FailRepoGithub {
        fn head_sha(&self, repo: &str) -> Result<String, Error> {
            if repo == self.fail_repo {
                return Err(Error::Other(format!("github: {repo} unreachable")));
            }
            self.inner.head_sha(repo)
        }

        fn latest_commit_until(
            &self,
            repo: &str,
            until: OffsetDateTime,
        ) -> Result<crate::github::CommitInfo, Error> {
            self.inner.latest_commit_until(repo, until)
        }
    }

    #[test]
    fn failed_cask_refresh_leaves_the_old_core_cutoff_readable() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        git.insert_blob("thirtyh", "Formula/a.rb", "old cutoff blob");
        // Cask stays on a 8h window throughout so it never pins core's cutoffs.
        let plan = |core: u32| RefreshPlan {
            core_hours: SoakHours::new(core).unwrap(),
            cask_hours: SoakHours::new(8).unwrap(),
            ..RefreshPlan::uniform(SoakHours::new(24).unwrap())
        };
        let first = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan(24),
            now(),
            &mut std::io::sink(),
        )
        .expect("first refresh");
        assert_eq!(first.core.cutoff_sha, "thirtyh");
        git.insert_blob("tenh", "Formula/a.rb", "new cutoff blob");
        // A shorter window moves core's cutoff to `tenh`; the cask fetch then
        // fails before state.toml is rewritten.
        let failing = FailRepoGithub {
            inner: fixture_gh(),
            fail_repo: CASK_REPO,
        };
        let err = refresh_with(
            &RunLock::for_tests(),
            &git,
            &failing,
            dir.path(),
            &plan(8),
            now(),
            &mut std::io::sink(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unreachable"), "{err}");
        let stored = load_state(dir.path()).unwrap().expect("state kept");
        assert_eq!(stored.core.cutoff_sha, "thirtyh", "state is unchanged");
        let core_dir = dir.path().join("core.git");
        assert_eq!(
            git.show(&core_dir, &stored.core.cutoff_sha, "Formula/a.rb")
                .unwrap()
                .as_deref(),
            Some(b"old cutoff blob".as_slice()),
            "the stored cutoff's objects must survive a failed refresh"
        );
        // The next successful run records the new cutoff, then gc's the old.
        let third = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan(8),
            now(),
            &mut std::io::sink(),
        )
        .expect("recovery run");
        assert_eq!(third.core.cutoff_sha, "tenh");
        assert_eq!(
            load_state(dir.path()).unwrap().unwrap().core.cutoff_sha,
            "tenh"
        );
        assert_eq!(
            git.show(&core_dir, "tenh", "Formula/a.rb")
                .unwrap()
                .as_deref(),
            Some(b"new cutoff blob".as_slice())
        );
        assert_eq!(
            git.show(&core_dir, "thirtyh", "Formula/a.rb").unwrap(),
            None
        );
    }

    #[test]
    fn gc_runs_only_after_state_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let failing = FailRepoGithub {
            inner: fixture_gh(),
            fail_repo: CASK_REPO,
        };
        refresh(
            &RunLock::for_tests(),
            &git,
            &failing,
            dir.path(),
            SoakHours::new(24).unwrap(),
            now(),
            &mut std::io::sink(),
        )
        .unwrap_err();
        assert!(git.gcs().is_empty(), "{:?}", git.gcs());
        refresh(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            SoakHours::new(24).unwrap(),
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(git.gcs().len(), 2, "core and cask each gc'd once");
    }

    #[test]
    fn failed_tap_fetch_keeps_the_last_good_table_until_a_fetch_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let remote = "https://github.com/hashicorp/homebrew-tap";
        let plan = RefreshPlan {
            taps: vec![tap_plan("hashicorp/tap", remote, 72)],
            ..RefreshPlan::uniform(SoakHours::new(24).unwrap())
        };
        let first = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        let good = first.tap("hashicorp/tap").cloned().expect("fetched");
        assert_eq!(good.head_sha, "hc3");

        git.fail_remote(remote);
        let second = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut std::io::sink(),
        )
        .expect("a tap failure never aborts");
        assert_eq!(second.tap("hashicorp/tap"), Some(&good), "kept in memory");
        let note = second.held_taps.get("hashicorp/tap").expect("held");
        assert!(note.contains("fetching history"), "{note}");
        let stored = load_state(dir.path()).unwrap().unwrap();
        assert_eq!(stored.tap("hashicorp/tap"), Some(&good), "kept on disk");
        assert!(stored.held_taps.is_empty(), "held is never persisted");

        // The remote recovers with a newer head: the table is replaced.
        git.heal_remote(remote);
        let n = now().unix_timestamp();
        git.insert_commits(remote, &[("hc4", n - 60), ("hc3", n - 3600)]);
        let third = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(third.tap("hashicorp/tap").unwrap().head_sha, "hc4");
        assert!(third.held_taps.is_empty());
        assert_eq!(
            load_state(dir.path())
                .unwrap()
                .unwrap()
                .tap("hashicorp/tap")
                .unwrap()
                .head_sha,
            "hc4"
        );
    }

    #[test]
    fn failed_refresh_taps_keeps_an_existing_table_in_memory_and_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let remote = "https://github.com/hashicorp/homebrew-tap";
        let plan = RefreshPlan {
            taps: vec![tap_plan("hashicorp/tap", remote, 72)],
            ..RefreshPlan::uniform(SoakHours::new(24).unwrap())
        };
        let mut snaps = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        git.fail_remote(remote);
        refresh_taps(
            &RunLock::for_tests(),
            &git,
            dir.path(),
            &mut snaps,
            &plan.taps,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert!(snaps.tap("hashicorp/tap").is_some());
        assert!(snaps.held_taps.contains_key("hashicorp/tap"));
        assert!(
            load_state(dir.path())
                .unwrap()
                .unwrap()
                .tap("hashicorp/tap")
                .is_some()
        );
    }

    #[test]
    fn refresh_with_seeds_only_planned_taps_from_the_stored_state() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        let both = RefreshPlan {
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
        refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &both,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        // cyclonedx is no longer planned (untapped) and its remote is down:
        // its table must drop out rather than be carried over.
        let only_hashicorp = RefreshPlan {
            taps: vec![both.taps[0].clone()],
            ..both.clone()
        };
        git.fail_remote("https://github.com/cyclonedx/homebrew-cyclonedx");
        let snaps = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &only_hashicorp,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert!(snaps.tap("hashicorp/tap").is_some());
        assert!(snaps.tap("cyclonedx/cyclonedx").is_none());
        assert!(
            load_state(dir.path())
                .unwrap()
                .unwrap()
                .tap("cyclonedx/cyclonedx")
                .is_none()
        );
    }

    #[test]
    fn a_tap_failing_on_the_first_run_has_no_table() {
        let dir = tempfile::tempdir().unwrap();
        let git = tap_git(now());
        git.fail_remote("https://github.com/hashicorp/homebrew-tap");
        let plan = RefreshPlan {
            taps: vec![tap_plan(
                "hashicorp/tap",
                "https://github.com/hashicorp/homebrew-tap",
                72,
            )],
            ..RefreshPlan::uniform(SoakHours::new(24).unwrap())
        };
        let snaps = refresh_with(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            &plan,
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert!(snaps.tap("hashicorp/tap").is_none());
        assert!(snaps.held_taps.contains_key("hashicorp/tap"));
    }

    #[test]
    fn a_corrupt_state_file_does_not_block_a_forced_refresh() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.toml"), "hours = 2").unwrap();
        let git = InMemoryGit::new();
        refresh(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            SoakHours::new(24).unwrap(),
            now(),
            &mut std::io::sink(),
        )
        .expect("refresh replaces the unreadable state");
        assert!(load_state(dir.path()).unwrap().is_some());
    }

    #[test]
    fn write_state_leaves_no_temp_file_and_replaces_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let h = |n| SoakHours::new(n).unwrap();
        let first = refresh(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            h(24),
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        // A truncated leftover temp from a crashed writer is swept on the next write.
        let stale = dir.path().join(".state.toml.tmp-999999");
        std::fs::write(&stale, "hours = ").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        let second = refresh(
            &RunLock::for_tests(),
            &git,
            &fixture_gh(),
            dir.path(),
            h(8),
            now(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_ne!(first.core.cutoff_sha, second.core.cutoff_sha);
        assert_eq!(load_state(dir.path()).unwrap().unwrap(), second);
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains(".tmp")),
            "no temp files left: {names:?}"
        );
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
            &RunLock::for_tests(),
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
            &RunLock::for_tests(),
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
            &RunLock::for_tests(),
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
            &RunLock::for_tests(),
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
