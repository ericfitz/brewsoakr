//! brewsoak's own tap, `ericfitz/brewsoak`: its state, and the reversible
//! writes that stage a soaked package's cutoff file into it so brew can
//! install it by token. Every write is preceded by an undo entry under
//! `<cache>/soak-tap-undo/{Formula,Casks}/` and is kept or put back by
//! `settle` once the run knows which tap the keg came from.

use crate::Error;
use crate::brew::Brew;
use crate::identity::{self, PkgIdentity};
use crate::inventory::Pkg;
use crate::lock::RunLock;
use crate::origin::{OwnTap, SOAK_TAP};
use crate::resolve::{PkgKind, PkgRef};
use crate::taps::TapInfo;
use crate::{atomic, tap};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Directory under the cache that holds the undo entries.
pub const UNDO_DIR: &str = "soak-tap-undo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapState {
    NotTapped,
    Remote(String),
    Untrusted,
    Ready,
}

/// `None`: not tapped. A non-null remote: `Remote`, whatever `trusted` says.
pub fn tap_state(entry: Option<&TapInfo>) -> TapState {
    match entry {
        None => TapState::NotTapped,
        Some(e) => match &e.remote {
            Some(r) => TapState::Remote(r.clone()),
            None if e.trusted => TapState::Ready,
            None => TapState::Untrusted,
        },
    }
}

/// The entry's `path` when it has one, else `tap_root` (decision P1).
pub fn tap_dir(entry: Option<&TapInfo>, tap_root: &Path) -> PathBuf {
    entry
        .and_then(|e| e.path.clone())
        .unwrap_or_else(|| tap_root.to_path_buf())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady {
    Untrusted,
    DryRunNotTapped,
    Remote(String),
    CreateFailed(String),
}

impl NotReady {
    pub fn note(&self) -> String {
        match self {
            NotReady::Untrusted => format!(
                "brewsoak installs soaked packages from its own tap, {SOAK_TAP}, and brew does not trust it yet; run brew trust {SOAK_TAP} once, then run brewsoak again."
            ),
            NotReady::DryRunNotTapped => format!(
                "brewsoak's tap, {SOAK_TAP}, does not exist yet; a real run creates it, and it then needs brew trust {SOAK_TAP}."
            ),
            NotReady::Remote(r) => format!(
                "{SOAK_TAP} is tapped from {r}; brewsoak only uses a local tap of that name; remove it with brew untap {SOAK_TAP}."
            ),
            NotReady::CreateFailed(e) => format!("brewsoak could not create its tap ({e})."),
        }
    }
}

/// Version of a formula or cask file in brew's spelling (`1.0.0_2`); `None`
/// when it does not parse.
pub fn file_version(kind: PkgKind, rb: &[u8]) -> Option<String> {
    let rb = std::str::from_utf8(rb).ok()?;
    let id = match kind {
        PkgKind::Formula => PkgIdentity::Formula(identity::parse_formula(rb, None).ok()?),
        PkgKind::Cask => PkgIdentity::Cask(identity::parse_cask(rb, None).ok()?),
    };
    Some(crate::report::identity_version(&id))
}

fn tap_file(dir: &Path, kind: PkgKind, name: &str) -> PathBuf {
    match kind {
        PkgKind::Formula => tap::tap_formula_path(dir, name),
        PkgKind::Cask => tap::tap_cask_path(dir, name),
    }
}

/// The receipt names the soak tap and `dir` has a file for the package whose
/// `file_version` equals that of `pkg.receipt_rb`.
pub fn anchored(dir: &Path, pkg: &Pkg) -> bool {
    if pkg.own_tap != Some(OwnTap::Soak) {
        return false;
    }
    let Some(want) = file_version(pkg.kind, pkg.receipt_rb.as_bytes()) else {
        return false;
    };
    let Ok(bytes) = std::fs::read(tap_file(dir, pkg.kind, &pkg.name)) else {
        return false;
    };
    file_version(pkg.kind, &bytes).is_some_and(|have| have == want)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepRule {
    Pass,
    WriteCutoff,
    WouldReplace,
    CannotReplace,
}

/// Spec "Dependency closure and ordering": an installed formula dependency
/// this run does not upgrade.
pub fn installed_dep_rule(
    satisfied: bool,
    own_tap: Option<OwnTap>,
    installed_version: Option<&str>,
    cutoff_version: Option<&str>,
    installs_cutoff: bool,
) -> DepRule {
    if satisfied {
        return DepRule::Pass;
    }
    match own_tap {
        None if installs_cutoff => DepRule::Pass,
        None => DepRule::WouldReplace,
        Some(OwnTap::Soak) => match (installed_version, cutoff_version) {
            (Some(i), Some(c)) if i == c => DepRule::WriteCutoff,
            _ => DepRule::CannotReplace,
        },
        Some(OwnTap::Legacy) => DepRule::CannotReplace,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staged {
    Unchanged,
    Written,
    Leftover,
    NotReady(NotReady),
}

fn kind_dir(kind: PkgKind) -> &'static str {
    match kind {
        PkgKind::Formula => "Formula",
        PkgKind::Cask => "Casks",
    }
}

fn undo_rb(undo_root: &Path, kind: PkgKind, name: &str) -> PathBuf {
    undo_root.join(kind_dir(kind)).join(format!("{name}.rb"))
}

fn undo_absent(undo_root: &Path, kind: PkgKind, name: &str) -> PathBuf {
    undo_root
        .join(kind_dir(kind))
        .join(format!("{name}.absent"))
}

fn remove_if_present(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The one keep-or-put-back decision, shared by `SoakTap::settle` and
/// recovery of a crashed run's leftovers. Without an undo entry for the
/// package there is nothing to decide. The tap file is kept when `may_keep`
/// and brew's receipt for `version` (else the tap file's own version) names
/// the soak tap; otherwise the previous bytes are restored (an `.absent`
/// entry deletes the file). The undo entry is deleted last, so a crash in
/// between leaves an entry that the next settle finishes the same way.
pub(crate) fn settle_entry(
    brew: &impl Brew,
    tap_dir: &Path,
    undo_root: &Path,
    kind: PkgKind,
    name: &str,
    version: Option<&str>,
    may_keep: bool,
) -> Result<(), Error> {
    let rb = undo_rb(undo_root, kind, name);
    let absent = undo_absent(undo_root, kind, name);
    let file = tap_file(tap_dir, kind, name);
    let previous = match std::fs::read(&rb) {
        Ok(bytes) => Some(bytes),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            if !absent.exists() {
                return Ok(());
            }
            None
        }
        Err(e) => return Err(e.into()),
    };
    let keep = may_keep && {
        let version = match version {
            Some(v) => Some(v.to_string()),
            None => std::fs::read(&file)
                .ok()
                .and_then(|b| file_version(kind, &b)),
        };
        version
            .and_then(|v| brew.receipt_tap(kind, name, &v))
            .is_some_and(|t| crate::origin::own_tap(&t) == Some(OwnTap::Soak))
    };
    if !keep {
        match &previous {
            Some(bytes) => atomic::write_bytes(&file, bytes)?,
            None => remove_if_present(&file)?,
        }
    }
    remove_if_present(&rb)?;
    remove_if_present(&absent)
}

pub struct SoakTap {
    tap_root: PathBuf,
    dir: PathBuf,
    undo_root: PathBuf,
    dry_run: bool,
    state: TapState,
    decided: Option<Result<(), NotReady>>,
    /// Staged since the last settle, with the cutoff version when known.
    staged: BTreeMap<(PkgKind, String), Option<String>>,
}

impl SoakTap {
    pub fn new(entry: Option<&TapInfo>, tap_root: &Path, cache: &Path, dry_run: bool) -> Self {
        Self {
            tap_root: tap_root.to_path_buf(),
            dir: tap_dir(entry, tap_root),
            undo_root: cache.join(UNDO_DIR),
            dry_run,
            state: tap_state(entry),
            decided: None,
            staged: BTreeMap::new(),
        }
    }

    /// Current tap directory (`tap_dir`), whether or not the tap is ready.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn decide(&mut self, brew: &impl Brew) -> Result<(), NotReady> {
        if self.state == TapState::NotTapped {
            if self.dry_run {
                return Err(NotReady::DryRunNotTapped);
            }
            brew.tap_new(SOAK_TAP)
                .map_err(|e| NotReady::CreateFailed(first_line(&e)))?;
            let taps = brew
                .tap_info()
                .map_err(|e| NotReady::CreateFailed(first_line(&e.to_string())))?;
            let entry = taps.iter().find(|t| t.name.eq_ignore_ascii_case(SOAK_TAP));
            self.dir = tap_dir(entry, &self.tap_root);
            self.state = tap_state(entry);
            if self.state == TapState::NotTapped {
                return Err(NotReady::CreateFailed(
                    "brew created it but does not list it".into(),
                ));
            }
        }
        match &self.state {
            TapState::Ready => Ok(()),
            TapState::Untrusted => Err(NotReady::Untrusted),
            TapState::Remote(r) => Err(NotReady::Remote(r.clone())),
            TapState::NotTapped => Err(NotReady::DryRunNotTapped),
        }
    }

    /// Decided on the first call and kept for the run. A real run creates a
    /// tap that is not tapped, reads tap-info again and applies the row that
    /// then holds. Never fatal.
    pub fn ready(&mut self, brew: &impl Brew) -> Result<(), NotReady> {
        if let Some(d) = &self.decided {
            return d.clone();
        }
        let d = self.decide(brew);
        self.decided = Some(d.clone());
        d
    }

    /// Make the package's tap file the sanitized cutoff. `version` is the
    /// cutoff version when the caller knows it; a later `Some` replaces it.
    pub fn stage(
        &mut self,
        _lock: &RunLock,
        brew: &impl Brew,
        pkg: &PkgRef,
        cutoff: &[u8],
        version: Option<&str>,
    ) -> Result<Staged, Error> {
        if let Err(n) = self.ready(brew) {
            return Ok(Staged::NotReady(n));
        }
        let key = (pkg.kind, pkg.name.clone());
        if let Some(recorded) = self.staged.get_mut(&key) {
            if let Some(v) = version {
                *recorded = Some(v.to_string());
            }
            return Ok(Staged::Unchanged);
        }
        let rb = undo_rb(&self.undo_root, pkg.kind, &pkg.name);
        let absent = undo_absent(&self.undo_root, pkg.kind, &pkg.name);
        if rb.exists() || absent.exists() {
            return Ok(Staged::Leftover);
        }
        let wanted = tap::sanitize_unofficial(cutoff);
        let file = tap_file(&self.dir, pkg.kind, &pkg.name);
        let previous = match std::fs::read(&file) {
            Ok(bytes) => Some(bytes),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => None,
            Err(e) => return Err(e.into()),
        };
        if previous.as_deref() == Some(wanted.as_slice()) {
            return Ok(Staged::Unchanged);
        }
        match &previous {
            Some(bytes) => atomic::write_bytes(&rb, bytes)?,
            None => atomic::write_bytes(&absent, b"")?,
        }
        // Recorded before the write so a failed write is still settled.
        self.staged.insert(key, version.map(str::to_string));
        tap::write_blob(&self.dir, pkg, cutoff)?;
        Ok(Staged::Written)
    }

    /// Keep or put back every file staged since the last settle. A failure
    /// on one file does not stop the others; the first error is returned.
    pub fn settle(&mut self, _lock: &RunLock, brew: &impl Brew) -> Result<(), Error> {
        let staged = std::mem::take(&mut self.staged);
        let mut first = None;
        for ((kind, name), version) in staged {
            if let Err(e) = settle_entry(
                brew,
                &self.dir,
                &self.undo_root,
                kind,
                &name,
                version.as_deref(),
                !self.dry_run,
            ) {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no detail")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brew::MockBrew;
    use crate::inventory::PkgClass;
    use std::fs;

    fn formula_rb(version: &str) -> String {
        format!(
            "class X < Formula\n  url \"https://example.com/x-{version}.tar.gz\"\n  sha256 \"abc\"\nend\n"
        )
    }

    fn cask_rb(version: &str) -> String {
        format!(
            "cask \"x\"\n  version \"{version}\"\n  sha256 \"cafebabe\"\n  url \"https://example.com/x-{version}.zip\"\n"
        )
    }

    fn fref(name: &str) -> PkgRef {
        PkgRef {
            name: name.into(),
            kind: PkgKind::Formula,
        }
    }

    fn cref(name: &str) -> PkgRef {
        PkgRef {
            name: name.into(),
            kind: PkgKind::Cask,
        }
    }

    struct Env {
        tap: tempfile::TempDir,
        cache: tempfile::TempDir,
        lock: RunLock,
    }

    fn env() -> Env {
        Env {
            tap: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
            lock: RunLock::for_tests(),
        }
    }

    impl Env {
        fn soak(&self, entry: Option<&TapInfo>, dry: bool) -> SoakTap {
            SoakTap::new(entry, self.tap.path(), self.cache.path(), dry)
        }
        fn file(&self, kind: &str, name: &str) -> PathBuf {
            self.tap.path().join(kind).join(format!("{name}.rb"))
        }
        fn undo(&self, kind: &str, name: &str, ext: &str) -> PathBuf {
            self.cache
                .path()
                .join(UNDO_DIR)
                .join(kind)
                .join(format!("{name}.{ext}"))
        }
        fn put(&self, path: &Path, bytes: &[u8]) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }

    fn local(trusted: bool) -> TapInfo {
        let mut t = TapInfo::new(SOAK_TAP, None);
        t.trusted = trusted;
        t
    }

    fn brew_with_receipt(kind: PkgKind, name: &str, version: &str, tap: &str) -> MockBrew {
        let mut b = MockBrew::new();
        b.receipt_taps
            .insert((kind, name.into(), version.into()), tap.into());
        b
    }

    fn runs(b: &MockBrew) -> Vec<Vec<String>> {
        b.runs.lock().unwrap().clone()
    }

    #[test]
    fn tap_state_follows_the_spec_table() {
        assert_eq!(tap_state(None), TapState::NotTapped);
        let mut remote = TapInfo::new(SOAK_TAP, Some("https://x"));
        remote.trusted = true;
        assert_eq!(
            tap_state(Some(&remote)),
            TapState::Remote("https://x".into())
        );
        assert_eq!(tap_state(Some(&local(false))), TapState::Untrusted);
        assert_eq!(tap_state(Some(&local(true))), TapState::Ready);
    }

    #[test]
    fn tap_dir_prefers_the_reported_path() {
        let mut e = local(true);
        e.path = Some(PathBuf::from("/p"));
        assert_eq!(tap_dir(Some(&e), Path::new("/q")), PathBuf::from("/p"));
        assert_eq!(
            tap_dir(Some(&local(true)), Path::new("/q")),
            PathBuf::from("/q")
        );
        assert_eq!(tap_dir(None, Path::new("/q")), PathBuf::from("/q"));
    }

    #[test]
    fn not_ready_notes_are_the_spec_text() {
        assert_eq!(
            NotReady::Untrusted.note(),
            "brewsoak installs soaked packages from its own tap, ericfitz/brewsoak, and brew does not trust it yet; run brew trust ericfitz/brewsoak once, then run brewsoak again."
        );
        assert_eq!(
            NotReady::DryRunNotTapped.note(),
            "brewsoak's tap, ericfitz/brewsoak, does not exist yet; a real run creates it, and it then needs brew trust ericfitz/brewsoak."
        );
        assert_eq!(
            NotReady::Remote("https://x".into()).note(),
            "ericfitz/brewsoak is tapped from https://x; brewsoak only uses a local tap of that name; remove it with brew untap ericfitz/brewsoak."
        );
        assert_eq!(
            NotReady::CreateFailed("Error: nope".into()).note(),
            "brewsoak could not create its tap (Error: nope)."
        );
    }

    #[test]
    fn ready_real_run_creates_a_missing_tap_once_and_then_holds_as_untrusted() {
        let e = env();
        let brew = MockBrew {
            soak_tap: None,
            ..MockBrew::new()
        };
        let mut s = e.soak(None, false);
        assert_eq!(s.ready(&brew), Err(NotReady::Untrusted));
        assert_eq!(s.ready(&brew), Err(NotReady::Untrusted));
        let tap_new: Vec<_> = runs(&brew)
            .into_iter()
            .filter(|r| r.first().map(String::as_str) == Some("tap-new"))
            .collect();
        assert_eq!(tap_new.len(), 1);
    }

    #[test]
    fn ready_dry_run_creates_nothing() {
        let e = env();
        let brew = MockBrew {
            soak_tap: None,
            ..MockBrew::new()
        };
        let mut s = e.soak(None, true);
        assert_eq!(s.ready(&brew), Err(NotReady::DryRunNotTapped));
        assert!(runs(&brew).is_empty());
    }

    #[test]
    fn ready_reports_a_failed_tap_new() {
        let e = env();
        let mut brew = MockBrew {
            soak_tap: None,
            ..MockBrew::new()
        };
        brew.tap_new_error = Some("Error: nope\nmore".into());
        let mut s = e.soak(None, false);
        assert_eq!(
            s.ready(&brew),
            Err(NotReady::CreateFailed("Error: nope".into()))
        );
    }

    #[test]
    fn ready_remote_untrusted_and_trusted() {
        let e = env();
        let brew = MockBrew::new();
        let remote = TapInfo::new(SOAK_TAP, Some("https://x"));
        assert_eq!(
            e.soak(Some(&remote), false).ready(&brew),
            Err(NotReady::Remote("https://x".into()))
        );
        assert_eq!(
            e.soak(Some(&local(false)), false).ready(&brew),
            Err(NotReady::Untrusted)
        );
        assert_eq!(e.soak(Some(&local(true)), false).ready(&brew), Ok(()));
        assert!(runs(&brew).is_empty());
    }

    const OLD: &[u8] = b"old bytes\n";

    #[test]
    fn stage_writes_the_file_and_an_absent_marker() {
        let e = env();
        let brew = MockBrew::new();
        let cutoff = formula_rb("1.1.0");
        let mut s = e.soak(Some(&local(true)), false);
        let r = s
            .stage(
                &e.lock,
                &brew,
                &fref("wget"),
                cutoff.as_bytes(),
                Some("1.1.0"),
            )
            .unwrap();
        assert_eq!(r, Staged::Written);
        assert_eq!(
            fs::read(e.file("Formula", "wget")).unwrap(),
            cutoff.as_bytes()
        );
        let marker = e.undo("Formula", "wget", "absent");
        assert!(marker.exists());
        assert!(fs::read(marker).unwrap().is_empty());
    }

    #[test]
    fn stage_over_another_file_saves_the_previous_bytes() {
        let e = env();
        let brew = MockBrew::new();
        e.put(&e.file("Formula", "wget"), OLD);
        let mut s = e.soak(Some(&local(true)), false);
        let cutoff = formula_rb("1.1.0");
        let r = s
            .stage(&e.lock, &brew, &fref("wget"), cutoff.as_bytes(), None)
            .unwrap();
        assert_eq!(r, Staged::Written);
        assert_eq!(fs::read(e.undo("Formula", "wget", "rb")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "absent").exists());
    }

    #[test]
    fn stage_with_the_wanted_bytes_touches_nothing() {
        let e = env();
        let brew = MockBrew::new();
        let cutoff = formula_rb("1.1.0");
        e.put(&e.file("Formula", "wget"), cutoff.as_bytes());
        let mut s = e.soak(Some(&local(true)), false);
        let r = s
            .stage(&e.lock, &brew, &fref("wget"), cutoff.as_bytes(), None)
            .unwrap();
        assert_eq!(r, Staged::Unchanged);
        assert!(!e.cache.path().join(UNDO_DIR).exists());
    }

    #[test]
    fn stage_twice_in_one_run_is_unchanged_and_keeps_one_undo_entry() {
        let e = env();
        let brew = MockBrew::new();
        let cutoff = formula_rb("1.1.0");
        let mut s = e.soak(Some(&local(true)), false);
        let p = fref("wget");
        assert_eq!(
            s.stage(&e.lock, &brew, &p, cutoff.as_bytes(), None)
                .unwrap(),
            Staged::Written
        );
        assert_eq!(
            s.stage(&e.lock, &brew, &p, cutoff.as_bytes(), Some("1.1.0"))
                .unwrap(),
            Staged::Unchanged
        );
        let entries = fs::read_dir(e.cache.path().join(UNDO_DIR).join("Formula"))
            .unwrap()
            .count();
        assert_eq!(entries, 1);
    }

    #[test]
    fn stage_after_a_put_back_writes_again_for_a_second_target() {
        let e = env();
        let brew = MockBrew::new();
        let cutoff = formula_rb("1.1.0");
        let mut s = e.soak(Some(&local(true)), false);
        let p = fref("openssl@3");
        assert_eq!(
            s.stage(&e.lock, &brew, &p, cutoff.as_bytes(), None)
                .unwrap(),
            Staged::Written
        );
        s.settle(&e.lock, &brew).unwrap();
        assert!(!e.file("Formula", "openssl@3").exists());
        assert_eq!(
            s.stage(&e.lock, &brew, &p, cutoff.as_bytes(), None)
                .unwrap(),
            Staged::Written
        );
        assert!(e.file("Formula", "openssl@3").exists());
    }

    #[test]
    fn stage_meets_a_leftover_undo_entry() {
        let e = env();
        let brew = MockBrew::new();
        e.put(&e.file("Formula", "wget"), OLD);
        e.put(&e.undo("Formula", "wget", "rb"), b"saved\n");
        let mut s = e.soak(Some(&local(true)), false);
        let cutoff = formula_rb("1.1.0");
        let r = s
            .stage(&e.lock, &brew, &fref("wget"), cutoff.as_bytes(), None)
            .unwrap();
        assert_eq!(r, Staged::Leftover);
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert_eq!(
            fs::read(e.undo("Formula", "wget", "rb")).unwrap(),
            b"saved\n"
        );
        // The leftover is not adopted: settle leaves it for recovery.
        s.settle(&e.lock, &brew).unwrap();
        assert!(e.undo("Formula", "wget", "rb").exists());
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
    }

    #[test]
    fn stage_meets_a_leftover_absent_entry() {
        let e = env();
        let brew = MockBrew::new();
        e.put(&e.undo("Casks", "x", "absent"), b"");
        let mut s = e.soak(Some(&local(true)), false);
        let cutoff = cask_rb("1");
        let r = s
            .stage(&e.lock, &brew, &cref("x"), cutoff.as_bytes(), None)
            .unwrap();
        assert_eq!(r, Staged::Leftover);
        assert!(!e.file("Casks", "x").exists());
    }

    #[test]
    fn stage_on_an_unready_tap_writes_nothing() {
        let e = env();
        let brew = MockBrew::new();
        let mut s = e.soak(Some(&local(false)), false);
        let cutoff = formula_rb("1.1.0");
        let r = s
            .stage(&e.lock, &brew, &fref("wget"), cutoff.as_bytes(), None)
            .unwrap();
        assert_eq!(r, Staged::NotReady(NotReady::Untrusted));
        assert_eq!(fs::read_dir(e.tap.path()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(e.cache.path()).unwrap().count(), 0);
    }

    #[test]
    fn undo_entry_exists_before_the_tap_file_changes() {
        let e = env();
        let brew = MockBrew::new();
        e.put(&e.tap.path().join("Formula"), b"not a directory");
        let mut s = e.soak(Some(&local(true)), false);
        let cutoff = formula_rb("1.1.0");
        let r = s.stage(&e.lock, &brew, &fref("wget"), cutoff.as_bytes(), None);
        assert!(r.is_err());
        assert!(e.undo("Formula", "wget", "absent").exists());
        // A failed write is still settled: the entry goes, nothing is left behind.
        s.settle(&e.lock, &brew).unwrap();
        assert!(!e.undo("Formula", "wget", "absent").exists());
    }

    #[test]
    fn stage_keeps_kinds_and_odd_names_apart() {
        let e = env();
        let brew = MockBrew::new();
        let mut s = e.soak(Some(&local(true)), false);
        let f = formula_rb("1.0.0");
        let c = cask_rb("2.0.0");
        for (p, body) in [
            (fref("docker"), &f),
            (cref("docker"), &c),
            (fref("python@3.12"), &f),
        ] {
            assert_eq!(
                s.stage(&e.lock, &brew, &p, body.as_bytes(), None).unwrap(),
                Staged::Written
            );
        }
        assert_eq!(fs::read(e.file("Formula", "docker")).unwrap(), f.as_bytes());
        assert_eq!(fs::read(e.file("Casks", "docker")).unwrap(), c.as_bytes());
        assert!(e.file("Formula", "python@3.12").exists());
        assert!(e.undo("Formula", "docker", "absent").exists());
        assert!(e.undo("Casks", "docker", "absent").exists());
        assert!(e.undo("Formula", "python@3.12", "absent").exists());
    }

    fn staged_wget(e: &Env, dry: bool, old: Option<&[u8]>, body: &str, v: Option<&str>) -> SoakTap {
        if let Some(old) = old {
            e.put(&e.file("Formula", "wget"), old);
        }
        let mut s = e.soak(Some(&local(true)), dry);
        let brew = MockBrew::new();
        assert_eq!(
            s.stage(&e.lock, &brew, &fref("wget"), body.as_bytes(), v)
                .unwrap(),
            Staged::Written
        );
        s
    }

    #[test]
    fn settle_keeps_the_file_when_the_keg_is_from_the_tap() {
        let e = env();
        let body = formula_rb("1.1.0");
        let mut s = staged_wget(&e, false, None, &body, Some("1.1.0"));
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", "ericfitz/brewsoak");
        s.settle(&e.lock, &brew).unwrap();
        assert_eq!(
            fs::read(e.file("Formula", "wget")).unwrap(),
            body.as_bytes()
        );
        assert!(!e.undo("Formula", "wget", "absent").exists());
    }

    #[test]
    fn settle_puts_back_without_a_matching_keg() {
        let e = env();
        let body = formula_rb("1.1.0");
        let brew = MockBrew::new();
        let mut s = staged_wget(&e, false, None, &body, Some("1.1.0"));
        s.settle(&e.lock, &brew).unwrap();
        assert!(!e.file("Formula", "wget").exists());
        assert!(!e.undo("Formula", "wget", "absent").exists());

        let e = env();
        let mut s = staged_wget(&e, false, Some(OLD), &body, Some("1.1.0"));
        s.settle(&e.lock, &brew).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
    }

    #[test]
    fn settle_puts_back_when_the_keg_is_from_another_tap() {
        let e = env();
        let body = formula_rb("1.1.0");
        let mut s = staged_wget(&e, false, Some(OLD), &body, Some("1.1.0"));
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", "homebrew/core");
        s.settle(&e.lock, &brew).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
    }

    #[test]
    fn settle_on_a_dry_run_always_puts_back() {
        let e = env();
        let body = formula_rb("1.1.0");
        let mut s = staged_wget(&e, true, Some(OLD), &body, Some("1.1.0"));
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", "ericfitz/brewsoak");
        s.settle(&e.lock, &brew).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
    }

    #[test]
    fn settle_uses_the_file_version_when_none_was_given() {
        let e = env();
        let body = formula_rb("1.1.0");
        let mut s = staged_wget(&e, false, None, &body, None);
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", "ericfitz/brewsoak");
        s.settle(&e.lock, &brew).unwrap();
        assert!(e.file("Formula", "wget").exists());
    }

    #[test]
    fn settle_puts_back_an_unparseable_file() {
        let e = env();
        let mut s = staged_wget(&e, false, None, "not a formula\n", None);
        // Whatever the receipt says, no version is known to ask about.
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", "ericfitz/brewsoak");
        s.settle(&e.lock, &brew).unwrap();
        assert!(!e.file("Formula", "wget").exists());
        assert!(!e.undo("Formula", "wget", "absent").exists());
    }

    #[test]
    fn settle_twice_is_a_no_op() {
        let e = env();
        let body = formula_rb("1.1.0");
        let brew = MockBrew::new();
        let mut s = staged_wget(&e, false, Some(OLD), &body, Some("1.1.0"));
        s.settle(&e.lock, &brew).unwrap();
        // Something else changes the file; a second settle must not touch it.
        e.put(&e.file("Formula", "wget"), b"later\n");
        s.settle(&e.lock, &brew).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), b"later\n");
    }

    #[test]
    fn settle_entry_without_an_undo_entry_does_nothing() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"mine\n");
        settle_entry(
            &MockBrew::new(),
            e.tap.path(),
            &e.cache.path().join(UNDO_DIR),
            PkgKind::Formula,
            "wget",
            None,
            true,
        )
        .unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), b"mine\n");
    }

    fn pkg(own_tap: Option<OwnTap>, receipt_version: &str) -> Pkg {
        Pkg {
            name: "wget".into(),
            kind: PkgKind::Formula,
            origin: "homebrew/core".into(),
            class: PkgClass::Soaked,
            receipt_rb: formula_rb(receipt_version),
            receipt_tap: None,
            own_tap,
            pinned: false,
        }
    }

    #[test]
    fn anchored_needs_the_receipt_and_a_file_at_the_installed_version() {
        let e = env();
        let path = e.file("Formula", "wget");
        let soak = pkg(Some(OwnTap::Soak), "1.1.0");
        assert!(!anchored(e.tap.path(), &soak), "no file");
        e.put(&path, formula_rb("1.1.0").as_bytes());
        assert!(anchored(e.tap.path(), &soak));
        e.put(&path, formula_rb("1.2.0").as_bytes());
        assert!(!anchored(e.tap.path(), &soak), "other version");
        e.put(&path, b"junk\n");
        assert!(!anchored(e.tap.path(), &soak), "unparseable file");
        e.put(&path, formula_rb("1.1.0").as_bytes());
        assert!(!anchored(e.tap.path(), &pkg(Some(OwnTap::Legacy), "1.1.0")));
        assert!(!anchored(e.tap.path(), &pkg(None, "1.1.0")));
    }

    #[test]
    fn installed_dep_rule_table() {
        use DepRule::*;
        let s = Some(OwnTap::Soak);
        let l = Some(OwnTap::Legacy);
        assert_eq!(installed_dep_rule(true, None, None, None, false), Pass);
        assert_eq!(installed_dep_rule(false, None, None, None, true), Pass);
        assert_eq!(
            installed_dep_rule(false, None, None, None, false),
            WouldReplace
        );
        assert_eq!(
            installed_dep_rule(false, s, Some("1"), Some("1"), false),
            WriteCutoff
        );
        assert_eq!(
            installed_dep_rule(false, s, Some("1"), Some("2"), false),
            CannotReplace
        );
        assert_eq!(
            installed_dep_rule(false, s, None, Some("1"), false),
            CannotReplace
        );
        assert_eq!(
            installed_dep_rule(false, s, Some("1"), None, true),
            CannotReplace
        );
        assert_eq!(
            installed_dep_rule(false, l, Some("1"), Some("1"), true),
            CannotReplace
        );
    }
}
