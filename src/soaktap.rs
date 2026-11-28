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
use crate::lock::BrewLockError;
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

/// Where brewsoak's tap lives when tap-info does not say. Resolving it can
/// cost a `brew --repository`, so callers ask only when they need a directory
/// and tap-info has no `path` (Minor 7). A `&Path` is already resolved.
pub trait TapRootSource {
    fn tap_root(&self) -> Result<PathBuf, Error>;
}

impl TapRootSource for Path {
    fn tap_root(&self) -> Result<PathBuf, Error> {
        Ok(self.to_path_buf())
    }
}

impl TapRootSource for PathBuf {
    fn tap_root(&self) -> Result<PathBuf, Error> {
        Ok(self.clone())
    }
}

/// The entry's `path` when it has one, else `tap_root` (decision P1).
pub fn tap_dir(
    entry: Option<&TapInfo>,
    tap_root: &(impl TapRootSource + ?Sized),
) -> Result<PathBuf, Error> {
    match entry.and_then(|e| e.path.clone()) {
        Some(path) => Ok(path),
        None => tap_root.tap_root(),
    }
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
/// This is brewsoak's own parser, not brew's version detection. A caller
/// compares it with the same parser's reading of the keg's own copy
/// (`from_tap_at`), never with a keg directory name alone.
pub fn file_version(kind: PkgKind, rb: &[u8]) -> Option<String> {
    let rb = std::str::from_utf8(rb).ok()?;
    let id = match kind {
        PkgKind::Formula => PkgIdentity::Formula(identity::parse_formula(rb, None).ok()?),
        PkgKind::Cask => PkgIdentity::Cask(identity::parse_cask(rb, None).ok()?),
    };
    Some(crate::report::identity_version(&id))
}

/// `name` is installed from the soak tap at the version of `file`.
/// First check: the receipt at `version` (else `file_version(kind, file)`)
/// names the soak tap; a cask's `:latest` is asked as `latest`. Second check,
/// formulae only: a keg whose receipt names the soak tap has a formula copy
/// with the same `file_version` as `file`. Two versions that do not parse are
/// never equal.
pub fn from_tap_at(
    brew: &impl Brew,
    kind: PkgKind,
    name: &str,
    version: Option<&str>,
    file: Option<&[u8]>,
) -> bool {
    let names_soak =
        |tap: Option<String>| tap.is_some_and(|t| crate::origin::own_tap(&t) == Some(OwnTap::Soak));
    let parsed = file.and_then(|f| file_version(kind, f));
    let asked = version.map(str::to_string).or_else(|| parsed.clone());
    let exact = asked.is_some_and(|v| {
        let v = if kind == PkgKind::Cask && v == ":latest" {
            "latest"
        } else {
            v.as_str()
        };
        names_soak(brew.receipt_tap(kind, name, v))
    });
    if exact || kind != PkgKind::Formula {
        return exact;
    }
    let Some(want) = parsed else {
        return false;
    };
    brew.soak_tap_keg_files(name)
        .iter()
        .any(|rb| file_version(kind, rb.as_bytes()).as_deref() == Some(want.as_str()))
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
    let bytes = std::fs::read(&file).ok();
    let keep = may_keep && from_tap_at(brew, kind, name, version, bytes.as_deref());
    if !keep {
        match &previous {
            Some(bytes) => atomic::write_bytes(&file, bytes)?,
            None => remove_if_present(&file)?,
        }
    }
    remove_if_present(&rb)?;
    remove_if_present(&absent)
}

pub struct SoakTap<'a> {
    tap_root: Box<dyn Fn() -> Result<PathBuf, Error> + 'a>,
    /// The tap-info `path`; `None` means ask `tap_root` when a directory is needed.
    dir: Option<PathBuf>,
    undo_root: PathBuf,
    dry_run: bool,
    state: TapState,
    decided: Option<Result<(), NotReady>>,
    /// Staged since the last settle, with the cutoff version when known.
    staged: BTreeMap<(PkgKind, String), Option<String>>,
}

impl<'a> SoakTap<'a> {
    pub fn new<R: TapRootSource + ?Sized>(
        entry: Option<&TapInfo>,
        tap_root: &'a R,
        cache: &Path,
        dry_run: bool,
    ) -> Self {
        Self {
            tap_root: Box::new(move || tap_root.tap_root()),
            dir: entry.and_then(|e| e.path.clone()),
            undo_root: cache.join(UNDO_DIR),
            dry_run,
            state: tap_state(entry),
            decided: None,
            staged: BTreeMap::new(),
        }
    }

    /// `anchored` against the current tap directory. A receipt that does not
    /// name the soak tap, or a directory that cannot be resolved, is not anchored.
    ///
    /// The unresolvable directory is deliberately not an error here: the
    /// callers only pick a hint or a no-soak token for a package that is
    /// already installed, and every brew command of the run that follows
    /// needs the same `brew --repository` answer. `dir()` reports it: `maintain`
    /// fails a real run on it, and `stage` returns it (a dry run's dependency
    /// walk holds that target).
    pub fn anchored(&self, pkg: &Pkg) -> bool {
        pkg.own_tap == Some(OwnTap::Soak) && self.dir().is_ok_and(|dir| anchored(&dir, pkg))
    }

    /// Current tap directory (`tap_dir`), whether or not the tap is ready.
    /// Asks for the tap root only when tap-info gave no `path`.
    pub fn dir(&self) -> Result<PathBuf, Error> {
        match &self.dir {
            Some(d) => Ok(d.clone()),
            None => (self.tap_root)(),
        }
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
            self.dir = entry.and_then(|e| e.path.clone());
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
    /// then holds. Never fatal: it returns a `NotReady` hold and never resolves the
    /// tap directory (`stage`, which calls it, can still fail with an `Error`
    /// when `dir` cannot be resolved).
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
    /// A tap that is not ready is `Staged::NotReady`. An `Err` means the tap
    /// directory could not be resolved (`dir`, an `Error::Brew` from
    /// `brew --repository`) or I/O failed. A real run fails earlier and
    /// fatally in `maintain`. In a dry run the dependency walk catches the
    /// `Error::Brew` (`install_cutoff_staged`) and holds that one target; an
    /// I/O error is still fatal. A dry run stages too (and settles it back),
    /// so it needs the directory as well.
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
        let dir = self.dir()?;
        let file = tap_file(&dir, pkg.kind, &pkg.name);
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
        tap::write_blob(&dir, pkg, cutoff)?;
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
        if staged.is_empty() {
            return Ok(());
        }
        let dir = self.dir()?;
        let mut first = None;
        for ((kind, name), version) in staged {
            if let Err(e) = settle_entry(
                brew,
                &dir,
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
    brew: &impl Brew,
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
            Ok(m) if m.is_dir() => prune_walk(&root, kind, brew, &used, verbose, out)?,
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

/// Whether a receipt on disk now names the soak tap for `name`.
fn receipt_names_soak(brew: &impl Brew, kind: PkgKind, name: &str) -> bool {
    brew.installed_receipt_taps(kind, name)
        .iter()
        .any(|t| crate::origin::own_tap(t) == Some(OwnTap::Soak))
}

fn prune_walk(
    root: &Path,
    kind: PkgKind,
    brew: &impl Brew,
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
            prune_walk(&path, kind, brew, used, verbose, out)?;
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
        if name.is_empty() {
            if verbose {
                writeln!(
                    out,
                    "not pruning {}: the file has no formula name",
                    path.display()
                )?;
            }
            continue;
        }
        // The inventory is a snapshot: the receipt is read again under brew's
        // own lock (formulae), so an install in flight is never undone.
        let _guard = if kind == PkgKind::Formula {
            match brew.lock_formula(name) {
                Ok(guard) => Some(guard),
                Err(BrewLockError::Busy) => {
                    if verbose {
                        writeln!(
                            out,
                            "not pruning {}: brew holds the lock for {name}",
                            path.display()
                        )?;
                    }
                    continue;
                }
                Err(BrewLockError::Unavailable(why)) => {
                    writeln!(
                        out,
                        "brewsoak: warning: not pruning {}: could not take brew's lock for {name} ({})",
                        path.display(),
                        first_line(&why)
                    )?;
                    continue;
                }
            }
        } else {
            None
        };
        if receipt_names_soak(brew, kind, name) {
            if verbose {
                writeln!(
                    out,
                    "not pruning {}: {name} is installed from brewsoak's tap",
                    path.display()
                )?;
            }
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
    tap_root: &(impl TapRootSource + ?Sized),
    cache: &Path,
    inv: &Inventory,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    match tap_state(entry) {
        TapState::NotTapped | TapState::Remote(_) => Ok(()),
        TapState::Untrusted => recover(lock, brew, &tap_dir(entry, tap_root)?, cache),
        TapState::Ready => {
            let dir = tap_dir(entry, tap_root)?;
            recover(lock, brew, &dir, cache)?;
            prune(lock, brew, &dir, cache, inv, verbose, out)
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
        fn soak(&self, entry: Option<&TapInfo>, dry: bool) -> SoakTap<'_> {
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
        assert_eq!(
            tap_dir(Some(&e), Path::new("/q")).unwrap(),
            PathBuf::from("/p")
        );
        assert_eq!(
            tap_dir(Some(&local(true)), Path::new("/q")).unwrap(),
            PathBuf::from("/q")
        );
        assert_eq!(tap_dir(None, Path::new("/q")).unwrap(), PathBuf::from("/q"));
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

    fn staged_wget<'a>(
        e: &'a Env,
        dry: bool,
        old: Option<&[u8]>,
        body: &str,
        v: Option<&str>,
    ) -> SoakTap<'a> {
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

    /// A tap root that counts its asks and can fail.
    struct CountingRoot {
        path: PathBuf,
        fails: bool,
        calls: std::cell::Cell<usize>,
    }

    impl TapRootSource for CountingRoot {
        fn tap_root(&self) -> Result<PathBuf, Error> {
            self.calls.set(self.calls.get() + 1);
            if self.fails {
                // What `RealWorld::tap_root` returns when brew fails.
                return Err(Error::Brew {
                    status: 1,
                    message: "brew --repository failed".into(),
                });
            }
            Ok(self.path.clone())
        }
    }

    fn counting(path: &Path, fails: bool) -> CountingRoot {
        CountingRoot {
            path: path.to_path_buf(),
            fails,
            calls: std::cell::Cell::new(0),
        }
    }

    #[test]
    fn a_dry_run_stage_with_a_ready_tap_and_no_path_asks_for_the_root_once() {
        // The dry run still writes the file brew is asked about (and settles
        // it back), so the directory is needed: one ask, not avoidable.
        let e = env();
        let root = counting(e.tap.path(), false);
        let mut s = SoakTap::new(Some(&local(true)), &root, e.cache.path(), true);
        let cutoff = formula_rb("1.1.0");
        let r = s
            .stage(
                &e.lock,
                &MockBrew::new(),
                &fref("wget"),
                cutoff.as_bytes(),
                None,
            )
            .unwrap();
        assert_eq!(r, Staged::Written);
        assert_eq!(root.calls.get(), 1);
        s.settle(&e.lock, &MockBrew::new()).unwrap();
        assert!(!e.file("Formula", "wget").exists(), "a dry run restores");
    }

    #[test]
    fn a_failing_tap_root_fails_stage_and_only_degrades_anchored() {
        let e = env();
        let root = counting(e.tap.path(), true);
        let mut s = SoakTap::new(Some(&local(true)), &root, e.cache.path(), false);
        let cutoff = formula_rb("1.1.0");
        let r = s.stage(
            &e.lock,
            &MockBrew::new(),
            &fref("wget"),
            cutoff.as_bytes(),
            None,
        );
        assert!(matches!(r, Err(Error::Brew { message, .. }) if message.contains("--repository")));
        assert!(!s.anchored(&pkg(Some(OwnTap::Soak), "1.1.0")), "degrades");
        assert!(s.dir().is_err());
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

    const GO_KEG: &str = "go1.1_2";

    fn brew_with_go_keg(tap: &str, copy: &str) -> MockBrew {
        let mut b = brew_with_receipt(PkgKind::Formula, "wget", GO_KEG, tap);
        b.kegs.insert(("wget".into(), GO_KEG.into()), copy.into());
        b
    }

    #[test]
    fn from_tap_at_accepts_a_receipt_at_the_given_version() {
        let b = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", SOAK_RECEIPT);
        assert!(from_tap_at(
            &b,
            PkgKind::Formula,
            "wget",
            Some("1.1.0"),
            None
        ));
        assert!(!from_tap_at(
            &b,
            PkgKind::Formula,
            "wget",
            Some("1.0.0"),
            None
        ));
    }

    #[test]
    fn from_tap_at_reads_the_version_from_the_file_when_none_is_given() {
        let b = brew_with_receipt(PkgKind::Formula, "wget", "1.1.0", SOAK_RECEIPT);
        let file = formula_rb("1.1.0");
        assert!(from_tap_at(
            &b,
            PkgKind::Formula,
            "wget",
            None,
            Some(file.as_bytes())
        ));
    }

    #[test]
    fn from_tap_at_accepts_a_keg_brew_named_differently() {
        let b = brew_with_go_keg(SOAK_RECEIPT, &formula_rb("1.1.0"));
        let same = formula_rb("1.1.0");
        let other = formula_rb("1.2.0");
        assert!(from_tap_at(
            &b,
            PkgKind::Formula,
            "wget",
            Some("1.1.0"),
            Some(same.as_bytes())
        ));
        assert!(!from_tap_at(
            &b,
            PkgKind::Formula,
            "wget",
            Some("1.1.0"),
            Some(other.as_bytes())
        ));
        assert!(!from_tap_at(
            &b,
            PkgKind::Formula,
            "wget",
            Some("1.1.0"),
            None
        ));
    }

    #[test]
    fn from_tap_at_ignores_a_keg_from_another_tap() {
        let b = brew_with_go_keg("homebrew/core", &formula_rb("1.1.0"));
        let file = formula_rb("1.1.0");
        assert!(!from_tap_at(
            &b,
            PkgKind::Formula,
            "wget",
            Some("1.1.0"),
            Some(file.as_bytes())
        ));
    }

    #[test]
    fn from_tap_at_rejects_copies_that_do_not_parse() {
        let b = brew_with_go_keg(SOAK_RECEIPT, "not ruby");
        assert!(!from_tap_at(
            &b,
            PkgKind::Formula,
            "wget",
            None,
            Some(b"not ruby")
        ));
    }

    #[test]
    fn from_tap_at_asks_a_latest_cask_as_latest() {
        let b = brew_with_receipt(PkgKind::Cask, "nightly", "latest", SOAK_RECEIPT);
        assert!(from_tap_at(
            &b,
            PkgKind::Cask,
            "nightly",
            Some(":latest"),
            None
        ));
    }

    #[test]
    fn from_tap_at_has_no_second_check_for_a_cask() {
        let mut b = brew_with_receipt(PkgKind::Cask, "docker", "4.0", SOAK_RECEIPT);
        b.kegs
            .insert(("docker".into(), "4.0".into()), cask_rb("4.1"));
        let file = cask_rb("4.1");
        assert!(!from_tap_at(
            &b,
            PkgKind::Cask,
            "docker",
            Some("4.1"),
            Some(file.as_bytes())
        ));
    }

    #[test]
    fn settle_keeps_the_file_when_brew_named_the_keg_differently() {
        let e = env();
        let body = formula_rb("1.1.0");
        let mut s = staged_wget(&e, false, None, &body, Some("1.1.0"));
        let brew = brew_with_go_keg(SOAK_RECEIPT, &formula_rb("1.1.0"));
        s.settle(&e.lock, &brew).unwrap();
        assert_eq!(
            fs::read(e.file("Formula", "wget")).unwrap(),
            body.as_bytes()
        );
        assert!(!e.undo("Formula", "wget", "absent").exists());
    }

    #[test]
    fn recover_keeps_a_file_whose_keg_brew_named_differently() {
        let e = env();
        let body = formula_rb("1.1.0");
        e.put(&e.file("Formula", "wget"), body.as_bytes());
        e.put(&e.undo("Formula", "wget", "absent"), b"");
        let brew = brew_with_go_keg(SOAK_RECEIPT, &formula_rb("1.1.0"));
        recover_in(&e, &brew).unwrap();
        assert_eq!(
            fs::read(e.file("Formula", "wget")).unwrap(),
            body.as_bytes()
        );
        assert!(!e.undo("Formula", "wget", "absent").exists());
        let before = crate::cmd::test_support::tree(e.tap.path());
        recover_in(&e, &brew).unwrap();
        assert_eq!(crate::cmd::test_support::tree(e.tap.path()), before);
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

    fn prune_in(e: &Env, brew: &MockBrew, inv: &Inventory, verbose: bool) -> String {
        let mut out = Vec::new();
        prune(
            &e.lock,
            brew,
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
        let out = prune_in(&e, &MockBrew::new(), &inv_with(vec![]), true);
        assert!(!e.file("Formula", "wget").exists());
        assert!(!e.file("Casks", "docker").exists());
        let want = format!(
            "pruned {} (no installed package uses it)",
            e.file("Formula", "wget").display()
        );
        assert!(out.contains(&want), "{out}");
        assert_eq!(
            prune_in(&e, &MockBrew::new(), &inv_with(vec![]), true),
            "",
            "second run"
        );
    }

    #[test]
    fn prune_is_quiet_without_verbose() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        assert_eq!(prune_in(&e, &MockBrew::new(), &inv_with(vec![]), false), "");
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
        prune_in(&e, &MockBrew::new(), &inv, true);
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
        prune_in(&e, &MockBrew::new(), &inv, false);
        assert!(!e.file("Formula", "wget").exists());
    }

    #[test]
    fn prune_keeps_a_file_with_an_undo_entry() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        e.put(&e.undo("Formula", "wget", "absent"), b"");
        e.put(&e.file("Casks", "x"), b"b");
        e.put(&e.undo("Casks", "x", "rb"), OLD);
        prune_in(&e, &MockBrew::new(), &inv_with(vec![]), true);
        assert!(e.file("Formula", "wget").exists());
        assert!(e.file("Casks", "x").exists());
    }

    #[test]
    fn prune_is_per_kind() {
        let e = env();
        e.put(&e.file("Formula", "docker"), b"a");
        e.put(&e.file("Casks", "docker"), b"b");
        let inv = inv_with(vec![installed(PkgKind::Cask, "docker", Some(OwnTap::Soak))]);
        prune_in(&e, &MockBrew::new(), &inv, true);
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
        prune_in(&e, &MockBrew::new(), &inv_with(vec![]), true);
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
        prune_in(&e, &MockBrew::new(), &inv_with(vec![]), true);
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
        prune_in(&e, &MockBrew::new(), &inv_with(vec![]), true);
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
        let out = prune_in(&e, &MockBrew::new(), &inv_with(vec![]), false);
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
        let out = prune_in(&e, &MockBrew::new(), &inv_with(vec![]), false);
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
        let out = prune_in(&e, &MockBrew::new(), &inv_with(vec![]), false);
        assert_eq!(fs::read(outside.path().join("keep.rb")).unwrap(), b"k");
        assert!(
            out.contains("not pruning") && out.contains("Casks"),
            "{out}"
        );
        assert!(!e.file("Formula", "old").exists());
    }

    fn soak_receipt(brew: &mut MockBrew, kind: PkgKind, name: &str, version: &str) {
        brew.receipt_taps
            .insert((kind, name.into(), version.into()), SOAK_TAP.into());
    }

    #[test]
    fn prune_skips_a_formula_whose_brew_lock_is_busy() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        let mut brew = MockBrew::new();
        brew.busy_formulae.insert("wget".into());
        let out = prune_in(&e, &brew, &inv_with(vec![]), true);
        assert!(e.file("Formula", "wget").exists());
        assert_eq!(
            out,
            format!(
                "not pruning {}: brew holds the lock for wget\n",
                e.file("Formula", "wget").display()
            )
        );
        assert_eq!(prune_in(&e, &brew, &inv_with(vec![]), false), "");
        // Brew finished: the next run prunes it, and a third run has nothing to do.
        let free = MockBrew::new();
        prune_in(&e, &free, &inv_with(vec![]), false);
        assert!(!e.file("Formula", "wget").exists());
        assert_eq!(prune_in(&e, &free, &inv_with(vec![]), true), "");
    }

    #[test]
    fn prune_warns_when_brews_lock_cannot_be_taken() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        let mut brew = MockBrew::new();
        brew.lock_error = Some("no such directory".into());
        let out = prune_in(&e, &brew, &inv_with(vec![]), false);
        assert!(e.file("Formula", "wget").exists());
        assert_eq!(
            out,
            format!(
                "brewsoak: warning: not pruning {}: could not take brew's lock for wget (no such directory)\n",
                e.file("Formula", "wget").display()
            )
        );
    }

    #[test]
    fn prune_reads_the_receipt_again_under_the_lock() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        let mut brew = MockBrew::new();
        soak_receipt(&mut brew, PkgKind::Formula, "wget", "1.0.0");
        let out = prune_in(&e, &brew, &inv_with(vec![]), true);
        assert!(e.file("Formula", "wget").exists());
        assert_eq!(
            out,
            format!(
                "not pruning {}: wget is installed from brewsoak's tap\n",
                e.file("Formula", "wget").display()
            )
        );
        assert_eq!(*brew.lock_calls.lock().unwrap(), ["wget"]);
    }

    #[test]
    fn prune_reads_a_cask_receipt_again_and_takes_no_lock() {
        let e = env();
        e.put(&e.file("Casks", "docker"), b"a");
        let mut brew = MockBrew::new();
        soak_receipt(&mut brew, PkgKind::Cask, "docker", "4.0");
        let out = prune_in(&e, &brew, &inv_with(vec![]), true);
        assert!(e.file("Casks", "docker").exists());
        assert_eq!(
            out,
            format!(
                "not pruning {}: docker is installed from brewsoak's tap\n",
                e.file("Casks", "docker").display()
            )
        );
        assert!(brew.lock_calls.lock().unwrap().is_empty());
        let plain = MockBrew::new();
        prune_in(&e, &plain, &inv_with(vec![]), false);
        assert!(!e.file("Casks", "docker").exists());
        assert!(plain.lock_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn prune_takes_no_lock_for_a_file_it_keeps_anyway() {
        let e = env();
        e.put(&e.file("Formula", "wget"), b"a");
        e.put(&e.file("Formula", "curl"), b"b");
        e.put(&e.undo("Formula", "curl", "absent"), b"");
        let inv = inv_with(vec![installed(
            PkgKind::Formula,
            "wget",
            Some(OwnTap::Soak),
        )]);
        let brew = MockBrew::new();
        prune_in(&e, &brew, &inv, true);
        assert!(e.file("Formula", "wget").exists());
        assert!(e.file("Formula", "curl").exists());
        assert!(brew.lock_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn prune_keeps_a_file_with_no_name_and_says_so() {
        let e = env();
        e.put(&e.tap.path().join("Formula/.rb"), b"a");
        let brew = MockBrew::new();
        let out = prune_in(&e, &brew, &inv_with(vec![]), true);
        assert!(e.tap.path().join("Formula/.rb").exists());
        assert_eq!(
            out,
            format!(
                "not pruning {}: the file has no formula name\n",
                e.tap.path().join("Formula/.rb").display()
            )
        );
        assert!(brew.lock_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn prune_locks_each_formula_by_its_file_stem() {
        let e = env();
        e.put(&e.file("Formula", "python@3.12"), b"a");
        e.put(&e.tap.path().join("Formula/sub/c++filt.rb"), b"b");
        let brew = MockBrew::new();
        prune_in(&e, &brew, &inv_with(vec![]), false);
        assert_eq!(*brew.lock_calls.lock().unwrap(), ["python@3.12", "c++filt"]);
        assert!(!e.file("Formula", "python@3.12").exists());
        assert!(!e.tap.path().join("Formula/sub/c++filt.rb").exists());
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
