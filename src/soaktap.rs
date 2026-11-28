//! brewsoak's own tap, `ericfitz/brewsoak`: its state, and the reversible
//! writes that stage a soaked package's cutoff file into it so brew can
//! install it by token. Every write is preceded by an undo entry under
//! `<cache>/soak-tap-undo/{Formula,Casks}/` and is kept or put back by
//! `settle` once the run knows which tap the keg came from.

use crate::Error;
use crate::brew::Brew;
use crate::error::first_line;
use crate::identity::{self, PkgIdentity};
use crate::inventory::{Inventory, Pkg};
use crate::lock::RunLock;
use crate::origin::{OwnTap, SOAK_TAP};
use crate::resolve::{PkgKind, PkgRef};
use crate::taps::TapInfo;
use crate::{atomic, tap};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, Write};
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

/// Why the tap cannot be used, when it is tapped but `Untrusted` or `Remote`.
pub fn unusable(entry: Option<&TapInfo>) -> Option<NotReady> {
    match tap_state(entry) {
        TapState::Untrusted => Some(NotReady::Untrusted),
        TapState::Remote(r) => Some(NotReady::Remote(r)),
        TapState::NotTapped | TapState::Ready => None,
    }
}

/// `Some` when the tap is tapped but unusable (untrusted, or from a remote)
/// and at least one installed package's receipt names it.
pub fn unusable_with_packages(inv: &Inventory) -> Option<NotReady> {
    let why = unusable(inv.soak_tap.as_ref())?;
    inv.pkgs
        .iter()
        .any(|p| p.own_tap == Some(OwnTap::Soak))
        .then_some(why)
}

/// Version of a formula or cask file in brew's spelling (`1.0.0_2`); `None`
/// when it does not parse.
///
/// Known limitation: this is the existing identity parser, not brew's version
/// detection. For a formula whose keg directory name differs from the parsed
/// version, recovery can put the old file back beside a new soak-tap keg, and
/// the WriteCutoff rule never keeps its file; the package is then treated as a
/// legacy receipt until its next soaked upgrade. See issue #24.
pub fn file_version(kind: PkgKind, rb: &[u8]) -> Option<String> {
    let rb = std::str::from_utf8(rb).ok()?;
    let id = match kind {
        PkgKind::Formula => PkgIdentity::Formula(identity::parse_formula(rb, None).ok()?),
        PkgKind::Cask => PkgIdentity::Cask(identity::parse_cask(rb, None).ok()?),
    };
    Some(crate::report::identity_version(&id))
}

pub(crate) fn tap_file(dir: &Path, kind: PkgKind, name: &str) -> PathBuf {
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
    ///
    /// The list is emptied first, so an entry whose settle failed is not tried
    /// again in this run. Its undo entry stays on disk: a later `stage` of that
    /// package returns `Leftover`, and the next recovery settles it.
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

/// Settle every undo entry found on disk, by the settle rule with
/// `file_version` of the tap file as the version. An entry is `<name>.rb` or
/// `<name>.absent` directly under `soak-tap-undo/Formula` or `/Casks`; any
/// other file is left alone. Repeating it finds nothing; a run that died
/// halfway is finished by the next, because `settle_entry` deletes an entry
/// last. Every entry is tried; the first error is returned after the rest
/// are settled. Creates nothing when there is no undo directory. A directory
/// that cannot be read is skipped without a message; a later run handles its
/// entries.
pub fn recover(_lock: &RunLock, brew: &impl Brew, dir: &Path, cache: &Path) -> Result<(), Error> {
    let undo_root = cache.join(UNDO_DIR);
    let mut first = None;
    for kind in [PkgKind::Formula, PkgKind::Cask] {
        let Ok(entries) = std::fs::read_dir(undo_root.join(kind_dir(kind))) else {
            continue;
        };
        let mut names = BTreeSet::new();
        for ent in entries.filter_map(Result::ok) {
            if ent.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let path = ent.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("rb" | "absent")) {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.insert(stem.to_string());
            }
        }
        for name in names {
            if let Err(e) = settle_entry(brew, dir, &undo_root, kind, &name, None, true) {
                first.get_or_insert(e);
            }
        }
    }
    first.map_or(Ok(()), Err)
}

/// Remove every `.rb` under `dir/Formula` and `dir/Casks` that has no undo
/// entry and no installed package of that kind and name whose receipt names
/// the soak tap. Real directories only: a link is removed as a link and a
/// linked directory is not entered. The test is the receipt alone, never the
/// version. A removal that fails is reported and skipped. A directory that
/// cannot be read is skipped without a message; a later run handles its
/// entries.
pub fn prune(
    _lock: &RunLock,
    dir: &Path,
    cache: &Path,
    inv: &Inventory,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    let undo_root = cache.join(UNDO_DIR);
    for kind in [PkgKind::Formula, PkgKind::Cask] {
        let used = |name: &str| {
            undo_rb(&undo_root, kind, name).exists()
                || undo_absent(&undo_root, kind, name).exists()
                || inv
                    .pkgs
                    .iter()
                    .any(|p| p.kind == kind && p.name == name && p.own_tap == Some(OwnTap::Soak))
        };
        let root = dir.join(kind_dir(kind));
        match std::fs::symlink_metadata(&root) {
            Ok(m) if m.is_dir() => prune_walk(&root, &used, verbose, out)?,
            Ok(_) => writeln!(
                out,
                "brewsoak: warning: not pruning {}: not a real directory",
                root.display()
            )?,
            Err(_) => {}
        }
    }
    Ok(())
}

fn prune_walk(
    root: &Path,
    used: &impl Fn(&str) -> bool,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(());
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|e| Some((e.path(), e.file_type().ok()?)))
        .collect();
    paths.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, ft) in paths {
        if ft.is_dir() {
            prune_walk(&path, used, verbose, out)?;
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".rb"))
        else {
            continue;
        };
        if used(name) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) if verbose => writeln!(
                out,
                "pruned {} (no installed package uses it)",
                path.display()
            )?,
            Ok(()) => {}
            Err(e) => writeln!(
                out,
                "brewsoak: warning: could not prune {}: {}",
                path.display(),
                first_line(&e.to_string())
            )?,
        }
    }
    Ok(())
}

/// Recovery when the tap is local (`Untrusted` or `Ready`), then pruning when
/// it is `Ready`. Nothing when it is not tapped or tapped from a remote.
#[allow(clippy::too_many_arguments)]
pub fn maintain(
    lock: &RunLock,
    brew: &impl Brew,
    entry: Option<&TapInfo>,
    tap_root: &Path,
    cache: &Path,
    inv: &Inventory,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    let dir = tap_dir(entry, tap_root);
    match tap_state(entry) {
        TapState::NotTapped | TapState::Remote(_) => Ok(()),
        TapState::Untrusted => recover(lock, brew, &dir, cache),
        TapState::Ready => {
            recover(lock, brew, &dir, cache)?;
            prune(lock, &dir, cache, inv, verbose, out)
        }
    }
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
    fn ready_reports_a_tap_that_is_not_listed_after_creation() {
        let e = env();
        let brew = MockBrew {
            soak_tap: None,
            tap_unlisted_after_create: true,
            ..MockBrew::new()
        };
        let mut s = e.soak(None, false);
        assert_eq!(
            s.ready(&brew),
            Err(NotReady::CreateFailed(
                "brew created it but does not list it".into()
            ))
        );
    }

    #[test]
    fn ready_applies_the_row_that_holds_after_creation() {
        let e = env();
        let brew = MockBrew {
            soak_tap: None,
            tap_after_create: Some(TapInfo::new(SOAK_TAP, Some("https://x"))),
            ..MockBrew::new()
        };
        let mut s = e.soak(None, false);
        assert_eq!(s.ready(&brew), Err(NotReady::Remote("https://x".into())));
    }

    #[test]
    fn ready_reports_a_tap_info_error_after_creation() {
        let e = env();
        let brew = MockBrew {
            soak_tap: None,
            tap_info_error_after_create: Some("boom\nmore".into()),
            ..MockBrew::new()
        };
        let mut s = e.soak(None, false);
        assert_eq!(s.ready(&brew), Err(NotReady::CreateFailed("boom".into())));
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
    fn stage_meets_a_leftover_even_when_the_file_already_has_the_wanted_bytes() {
        let e = env();
        let brew = MockBrew::new();
        let cutoff = formula_rb("1.1.0");
        e.put(&e.file("Formula", "wget"), cutoff.as_bytes());
        e.put(&e.undo("Formula", "wget", "rb"), b"saved\n");
        let mut s = e.soak(Some(&local(true)), false);
        let r = s
            .stage(&e.lock, &brew, &fref("wget"), cutoff.as_bytes(), None)
            .unwrap();
        assert_eq!(r, Staged::Leftover);
        assert_eq!(
            fs::read(e.file("Formula", "wget")).unwrap(),
            cutoff.as_bytes()
        );
        assert_eq!(
            fs::read(e.undo("Formula", "wget", "rb")).unwrap(),
            b"saved\n"
        );
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
    fn settle_puts_back_when_the_keg_is_from_the_legacy_tap() {
        let e = env();
        let body = formula_rb("1.1.0");
        let mut s = staged_wget(&e, false, Some(OLD), &body, Some("1.1.0"));
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", "brewsoakr/soaked");
        s.settle(&e.lock, &brew).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
    }

    #[test]
    fn settle_trusts_the_recorded_version_over_the_file_version() {
        let e = env();
        let body = formula_rb("1.1.0");
        let mut s = staged_wget(&e, false, None, &body, Some("1.1.0_1"));
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0_1", "ericfitz/brewsoak");
        s.settle(&e.lock, &brew).unwrap();
        assert!(e.file("Formula", "wget").exists());
        // The file's own version has a receipt too, but the recorded one decides.
        let e = env();
        let mut s = staged_wget(&e, false, None, &body, Some("1.1.0_1"));
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", "ericfitz/brewsoak");
        s.settle(&e.lock, &brew).unwrap();
        assert!(!e.file("Formula", "wget").exists());
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
            keg_version: String::new(),
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
        assert_eq!(
            installed_dep_rule(true, s, Some("1"), Some("2"), false),
            Pass
        );
        assert_eq!(
            installed_dep_rule(true, l, Some("1"), Some("2"), false),
            Pass
        );
    }

    const SOAK_RECEIPT: &str = "ericfitz/brewsoak";

    fn recover_in(e: &Env, brew: &MockBrew) -> Result<(), Error> {
        recover(&e.lock, brew, e.tap.path(), e.cache.path())
    }

    #[test]
    fn recover_keeps_a_file_whose_keg_is_from_the_tap() {
        let e = env();
        let body = formula_rb("1.1.0");
        e.put(&e.file("Formula", "wget"), body.as_bytes());
        e.put(&e.undo("Formula", "wget", "absent"), b"");
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", SOAK_RECEIPT);
        recover_in(&e, &brew).unwrap();
        assert_eq!(
            fs::read(e.file("Formula", "wget")).unwrap(),
            body.as_bytes()
        );
        assert!(!e.undo("Formula", "wget", "absent").exists());

        // `.rb` entry over previous bytes: the new bytes stay, the entry goes.
        let e = env();
        e.put(&e.file("Casks", "x"), cask_rb("2.0").as_bytes());
        e.put(&e.undo("Casks", "x", "rb"), OLD);
        let brew = brew_with_receipt(PkgKind::Cask, "x", "2.0", SOAK_RECEIPT);
        recover_in(&e, &brew).unwrap();
        assert_eq!(
            fs::read(e.file("Casks", "x")).unwrap(),
            cask_rb("2.0").as_bytes()
        );
        assert!(!e.undo("Casks", "x", "rb").exists());
    }

    #[test]
    fn recover_puts_back_previous_bytes_without_a_matching_keg() {
        let e = env();
        e.put(&e.file("Formula", "wget"), formula_rb("1.1.0").as_bytes());
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
        recover_in(&e, &MockBrew::new()).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
    }

    #[test]
    fn recover_deletes_the_file_for_an_absent_marker() {
        let e = env();
        e.put(&e.file("Formula", "wget"), formula_rb("1.1.0").as_bytes());
        e.put(&e.undo("Formula", "wget", "absent"), b"");
        recover_in(&e, &MockBrew::new()).unwrap();
        assert!(!e.file("Formula", "wget").exists());
        assert!(!e.undo("Formula", "wget", "absent").exists());
        // A missing tap file with no keg just deletes the entry.
        e.put(&e.undo("Casks", "gone", "absent"), b"");
        recover_in(&e, &MockBrew::new()).unwrap();
        assert!(!e.undo("Casks", "gone", "absent").exists());
        assert!(!e.file("Casks", "gone").exists());
    }

    #[test]
    fn recover_puts_back_an_unparseable_file() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"not a formula\n");
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
        let brew = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", SOAK_RECEIPT);
        recover_in(&e, &brew).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
    }

    #[test]
    fn recover_after_a_partial_put_back_finishes_the_job() {
        // The file was already restored; the crash left the entry behind.
        let e = env();
        e.put(&e.file("Formula", "wget"), OLD);
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
        recover_in(&e, &MockBrew::new()).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
        // Same for an absent marker whose file is already gone.
        e.put(&e.undo("Formula", "curl", "absent"), b"");
        recover_in(&e, &MockBrew::new()).unwrap();
        assert!(!e.undo("Formula", "curl", "absent").exists());
        assert!(!e.file("Formula", "curl").exists());
    }

    #[test]
    fn recover_twice_is_a_no_op() {
        let e = env();
        e.put(&e.file("Formula", "wget"), formula_rb("1.1.0").as_bytes());
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
        recover_in(&e, &MockBrew::new()).unwrap();
        e.put(&e.file("Formula", "wget"), b"later\n");
        let before = crate::cmd::test_support::tree(e.tap.path());
        recover_in(&e, &MockBrew::new()).unwrap();
        assert_eq!(crate::cmd::test_support::tree(e.tap.path()), before);
    }

    #[test]
    fn recover_without_an_undo_directory_creates_nothing() {
        let e = env();
        recover_in(&e, &MockBrew::new()).unwrap();
        assert!(!e.cache.path().join(UNDO_DIR).exists());
        assert!(fs::read_dir(e.cache.path()).unwrap().next().is_none());
        assert!(fs::read_dir(e.tap.path()).unwrap().next().is_none());
    }

    #[test]
    fn recover_handles_entries_it_never_wrote() {
        let e = env();
        e.put(&e.file("Formula", "wget"), formula_rb("1.1.0").as_bytes());
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
        e.put(&e.undo("Formula", "wget", "absent"), b"");
        let dir = e.cache.path().join(UNDO_DIR).join("Formula");
        e.put(&dir.join(".wget.rb.tmp-99"), b"tmp");
        e.put(&dir.join("notes.txt"), b"n");
        recover_in(&e, &MockBrew::new()).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
        assert!(!e.undo("Formula", "wget", "absent").exists());
        assert!(dir.join(".wget.rb.tmp-99").exists());
        assert!(dir.join("notes.txt").exists());
    }

    fn inv_with(pkgs: Vec<Pkg>) -> Inventory {
        Inventory {
            pkgs,
            ..Inventory::default()
        }
    }

    fn installed(kind: PkgKind, name: &str, own_tap: Option<OwnTap>) -> Pkg {
        Pkg {
            name: name.into(),
            kind,
            ..pkg(own_tap, "1.0.0")
        }
    }

    fn prune_in(e: &Env, inv: &Inventory, verbose: bool) -> String {
        let mut out = Vec::new();
        prune(
            &e.lock,
            e.tap.path(),
            e.cache.path(),
            inv,
            verbose,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn prune_removes_a_file_no_installed_package_uses() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        e.put(&e.file("Casks", "docker"), b"b");
        let out = prune_in(&e, &inv_with(vec![]), true);
        assert!(!e.file("Formula", "wget").exists());
        assert!(!e.file("Casks", "docker").exists());
        let want = format!(
            "pruned {} (no installed package uses it)",
            e.file("Formula", "wget").display()
        );
        assert!(out.contains(&want), "{out}");
        assert_eq!(prune_in(&e, &inv_with(vec![]), true), "", "second run");
    }

    #[test]
    fn prune_is_quiet_without_verbose() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        assert_eq!(prune_in(&e, &inv_with(vec![]), false), "");
        assert!(!e.file("Formula", "wget").exists());
    }

    #[test]
    fn prune_never_removes_a_file_while_the_receipt_names_the_tap() {
        let e = env();
        let other = formula_rb("9.9.9");
        e.put(&e.file("Formula", "wget"), other.as_bytes());
        let inv = inv_with(vec![installed(
            PkgKind::Formula,
            "wget",
            Some(OwnTap::Soak),
        )]);
        prune_in(&e, &inv, true);
        assert_eq!(
            fs::read(e.file("Formula", "wget")).unwrap(),
            other.as_bytes()
        );
        // A legacy-tap receipt does not protect the soak tap file.
        let inv = inv_with(vec![installed(
            PkgKind::Formula,
            "wget",
            Some(OwnTap::Legacy),
        )]);
        prune_in(&e, &inv, false);
        assert!(!e.file("Formula", "wget").exists());
    }

    #[test]
    fn prune_keeps_a_file_with_an_undo_entry() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        e.put(&e.undo("Formula", "wget", "absent"), b"");
        e.put(&e.file("Casks", "x"), b"b");
        e.put(&e.undo("Casks", "x", "rb"), OLD);
        prune_in(&e, &inv_with(vec![]), true);
        assert!(e.file("Formula", "wget").exists());
        assert!(e.file("Casks", "x").exists());
    }

    #[test]
    fn prune_is_per_kind() {
        let e = env();
        e.put(&e.file("Formula", "docker"), b"a");
        e.put(&e.file("Casks", "docker"), b"b");
        let inv = inv_with(vec![installed(PkgKind::Cask, "docker", Some(OwnTap::Soak))]);
        prune_in(&e, &inv, true);
        assert!(!e.file("Formula", "docker").exists());
        assert!(e.file("Casks", "docker").exists());
    }

    #[test]
    fn prune_touches_nothing_outside_formula_and_casks() {
        let e = env();
        e.put(&e.tap.path().join("README.md"), b"r");
        e.put(&e.tap.path().join(".github/x.yml"), b"y");
        e.put(&e.tap.path().join("Formula/notes.txt"), b"n");
        e.put(&e.tap.path().join("Other/x.rb"), b"o");
        let before = crate::cmd::test_support::tree(e.tap.path());
        prune_in(&e, &inv_with(vec![]), true);
        assert_eq!(crate::cmd::test_support::tree(e.tap.path()), before);
    }

    #[test]
    fn prune_removes_a_stale_link_and_spares_its_target() {
        let e = env();
        let outside = tempfile::tempdir().unwrap();
        e.put(&outside.path().join("real.rb"), b"real");
        e.put(&outside.path().join("sub-dir/inner.rb"), b"inner");
        fs::create_dir_all(e.tap.path().join("Formula")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("real.rb"), e.file("Formula", "x")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("sub-dir"),
            e.tap.path().join("Formula/sub"),
        )
        .unwrap();
        prune_in(&e, &inv_with(vec![]), true);
        assert!(e.file("Formula", "x").symlink_metadata().is_err());
        assert_eq!(fs::read(outside.path().join("real.rb")).unwrap(), b"real");
        assert_eq!(
            fs::read(outside.path().join("sub-dir/inner.rb")).unwrap(),
            b"inner"
        );
    }

    #[test]
    fn prune_removes_a_nested_stale_file() {
        let e = env();
        e.put(&e.tap.path().join("Formula/a/old.rb"), b"o");
        prune_in(&e, &inv_with(vec![]), true);
        assert!(!e.tap.path().join("Formula/a/old.rb").exists());
    }

    #[test]
    fn prune_warns_and_continues_when_a_removal_fails() {
        use std::os::unix::fs::PermissionsExt;
        let e = env();
        e.put(&e.tap.path().join("Formula/a-locked/x.rb"), b"a");
        e.put(&e.file("Formula", "z"), b"z");
        let locked = e.tap.path().join("Formula/a-locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        // Probe: as root, or on a filesystem that ignores the mode, nothing blocks.
        let probe = locked.join("probe");
        let blocks = fs::write(&probe, b"p").is_err();
        if !blocks {
            let _ = fs::remove_file(&probe);
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("skipped: permissions do not block deletion here");
            return;
        }
        let out = prune_in(&e, &inv_with(vec![]), false);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(locked.join("x.rb").exists());
        assert!(out.contains("brewsoak: warning: could not prune"), "{out}");
        assert!(!e.file("Formula", "z").exists());
    }

    #[test]
    fn prune_skips_a_symlinked_formula_root() {
        let e = env();
        let outside = tempfile::tempdir().unwrap();
        e.put(&outside.path().join("keep.rb"), b"k");
        std::os::unix::fs::symlink(outside.path(), e.tap.path().join("Formula")).unwrap();
        e.put(&e.file("Casks", "old"), b"o");
        let out = prune_in(&e, &inv_with(vec![]), false);
        assert_eq!(fs::read(outside.path().join("keep.rb")).unwrap(), b"k");
        assert!(out.contains("not a real directory"), "{out}");
        assert!(!e.file("Casks", "old").exists());
    }

    #[test]
    fn prune_skips_a_symlinked_casks_root() {
        let e = env();
        let outside = tempfile::tempdir().unwrap();
        e.put(&outside.path().join("keep.rb"), b"k");
        std::os::unix::fs::symlink(outside.path(), e.tap.path().join("Casks")).unwrap();
        e.put(&e.file("Formula", "old"), b"o");
        let out = prune_in(&e, &inv_with(vec![]), false);
        assert_eq!(fs::read(outside.path().join("keep.rb")).unwrap(), b"k");
        assert!(
            out.contains("not pruning") && out.contains("Casks"),
            "{out}"
        );
        assert!(!e.file("Formula", "old").exists());
    }

    #[test]
    fn recover_settles_every_entry_and_returns_the_first_error() {
        let e = env();
        let dir = e.cache.path().join(UNDO_DIR).join("Formula");
        fs::create_dir_all(dir.join("target")).unwrap();
        std::os::unix::fs::symlink(dir.join("target"), dir.join("bad.rb")).unwrap();
        e.put(&e.file("Formula", "wget"), formula_rb("1.1.0").as_bytes());
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
        e.put(&e.file("Casks", "c"), cask_rb("1.0").as_bytes());
        e.put(&e.undo("Casks", "c", "absent"), b"");
        assert!(recover_in(&e, &MockBrew::new()).is_err());
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
        assert!(!e.file("Casks", "c").exists());
    }

    #[test]
    fn recover_restores_previous_bytes_when_the_tap_file_is_missing() {
        let e = env();
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
        recover_in(&e, &MockBrew::new()).unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
    }

    fn seeded(e: &Env) {
        e.put(&e.file("Formula", "stale"), b"s");
        e.put(&e.file("Formula", "wget"), formula_rb("1.1.0").as_bytes());
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
    }

    #[test]
    fn maintain_recovers_and_prunes_a_ready_tap() {
        let e = env();
        seeded(&e);
        let mut out = Vec::new();
        maintain(
            &e.lock,
            &MockBrew::new(),
            Some(&local(true)),
            e.tap.path(),
            e.cache.path(),
            &inv_with(vec![]),
            false,
            &mut out,
        )
        .unwrap();
        assert!(!e.file("Formula", "stale").exists());
        // Recovery put OLD back, and the entry is gone, so prune took it too.
        assert!(!e.undo("Formula", "wget", "rb").exists());
        assert!(!e.file("Formula", "wget").exists());
    }

    #[test]
    fn maintain_recovers_but_does_not_prune_an_untrusted_tap() {
        let e = env();
        seeded(&e);
        maintain(
            &e.lock,
            &MockBrew::new(),
            Some(&local(false)),
            e.tap.path(),
            e.cache.path(),
            &inv_with(vec![]),
            true,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(e.file("Formula", "stale").exists());
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
    }

    #[test]
    fn maintain_uses_the_tap_info_path_over_tap_root() {
        let e = env();
        let other = tempfile::tempdir().unwrap();
        let mut entry = local(true);
        entry.path = Some(e.tap.path().to_path_buf());
        e.put(&e.file("Formula", "stale"), b"s");
        let decoy = other.path().join("Formula/decoy.rb");
        e.put(&decoy, b"d");
        maintain(
            &e.lock,
            &MockBrew::new(),
            Some(&entry),
            other.path(),
            e.cache.path(),
            &inv_with(vec![]),
            false,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!e.file("Formula", "stale").exists());
        assert_eq!(fs::read(&decoy).unwrap(), b"d");
    }

    #[test]
    fn maintain_does_nothing_when_not_tapped_or_remote() {
        let mut remote = local(true);
        remote.remote = Some("https://example.com/x.git".into());
        for entry in [None, Some(&remote)] {
            let e = env();
            seeded(&e);
            let before = (
                crate::cmd::test_support::tree(e.tap.path()),
                crate::cmd::test_support::tree(e.cache.path()),
            );
            let mut out = Vec::new();
            maintain(
                &e.lock,
                &MockBrew::new(),
                entry,
                e.tap.path(),
                e.cache.path(),
                &inv_with(vec![]),
                true,
                &mut out,
            )
            .unwrap();
            assert_eq!(
                (
                    crate::cmd::test_support::tree(e.tap.path()),
                    crate::cmd::test_support::tree(e.cache.path())
                ),
                before
            );
            assert!(out.is_empty());
        }
    }

    #[test]
    fn settle_entry_with_may_keep_handles_leftover_entries() {
        // Direct use of the shared rule, as recovery calls it.
        let e = env();
        let undo = e.cache.path().join(UNDO_DIR);
        e.put(&e.file("Formula", "wget"), formula_rb("1.1.0").as_bytes());
        e.put(&e.undo("Formula", "wget", "rb"), OLD);
        settle_entry(
            &MockBrew::new(),
            e.tap.path(),
            &undo,
            PkgKind::Formula,
            "wget",
            None,
            true,
        )
        .unwrap();
        assert_eq!(fs::read(e.file("Formula", "wget")).unwrap(), OLD);
        assert!(!e.undo("Formula", "wget", "rb").exists());
    }
}
