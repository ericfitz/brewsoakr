use crate::brew::{Brew, InstalledPkg};
use crate::eligibility::{self, DesiredAction, UpstreamStatus};
use crate::git::GitStore;
use crate::github::GithubApi;
use crate::identity::{self, PkgIdentity};
use crate::report::{self, Counts};
use crate::resolve::{self, PkgKind, PkgRef};
use crate::snapshot::{self, Snapshots};
use crate::tap;
use crate::{Error, SoakHours};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn refusal_message(action: DesiredAction, name: &str, brew_verb: &str) -> Option<String> {
    let why = match action {
        DesiredAction::RefuseTooNew => format!("{name} is too new (born inside the soak window)"),
        DesiredAction::RefuseYanked => format!("{name} is missing at HEAD (yanked)"),
        DesiredAction::RefuseDeprecated => format!("{name} is deprecated or disabled at HEAD"),
        DesiredAction::NoOpAlreadySoaked
        | DesiredAction::LeaveAheadOfSoak
        | DesiredAction::InstallCutoff => return None,
    };
    Some(format!(
        "{why}; use `brew {brew_verb} {name}` to bypass brewsoakr."
    ))
}

pub fn ahead_message(name: &str) -> String {
    format!("{name} is ahead of soak; leaving installed artifact unchanged")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub refused: bool,
    pub brew_status: Option<i32>,
}

pub fn combine_exit(refused: bool, brew_status: Option<i32>) -> i32 {
    let brew = brew_status.unwrap_or(0);
    if brew > 1 {
        brew
    } else if refused {
        1
    } else {
        brew
    }
}

#[allow(clippy::too_many_arguments)]
pub fn ensure_snapshots(
    git: &impl GitStore,
    gh: &impl GithubApi,
    brew: &impl Brew,
    cache: &Path,
    hours: SoakHours,
    now: time::OffsetDateTime,
    force: bool,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    let snaps = if force {
        snapshot::refresh(git, gh, cache, hours, now, progress)?
    } else {
        match snapshot::load_state(cache)? {
            Some(s) => return Ok(s),
            None => snapshot::refresh(git, gh, cache, hours, now, progress)?,
        }
    };
    prefetch_installed(brew, git, cache, &snaps);
    Ok(snaps)
}

type Classified = (
    String,
    DesiredAction,
    Option<PkgIdentity>,
    Option<PkgIdentity>,
    Option<PkgIdentity>,
);

fn classify_installed(
    brew: &impl Brew,
    git: &impl GitStore,
    cache: &Path,
    snaps: &Snapshots,
) -> Result<Vec<Classified>, Error> {
    let mut out = Vec::new();
    let installed = brew.installed_core()?;
    for pkg in installed {
        let Some(view) = resolve_view(
            git,
            snaps,
            cache,
            &pkg.name,
            pkg.kind,
            Some(&pkg.receipt_rb),
        )?
        else {
            continue;
        };
        out.push((
            pkg.name,
            view.action,
            view.installed,
            view.cutoff,
            view.head,
        ));
    }
    Ok(out)
}

fn write_update_summary(
    brew: &impl Brew,
    git: &impl GitStore,
    cache: &Path,
    snaps: &Snapshots,
    before: &[Classified],
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    let after = classify_installed(brew, git, cache, snaps)?;
    let before_map: std::collections::HashMap<_, _> = before
        .iter()
        .map(|(n, a, _, _, _)| (n.as_str(), *a))
        .collect();
    let mut eligible = Vec::new();
    let mut soaking = Vec::new();
    let mut gone = Vec::new();
    for (name, action, inst, cut, head) in &after {
        if verbose {
            let did = match action {
                DesiredAction::InstallCutoff => "eligible after this update",
                DesiredAction::RefuseTooNew => "still soaking",
                DesiredAction::RefuseYanked => "gone at HEAD",
                DesiredAction::NoOpAlreadySoaked => "already at cutoff",
                DesiredAction::LeaveAheadOfSoak => "ahead of soak",
                DesiredAction::RefuseDeprecated => "deprecated at HEAD",
            };
            writeln!(
                out,
                "{}",
                report::evaluate_line(
                    name,
                    *action,
                    inst.as_ref(),
                    cut.as_ref(),
                    head.as_ref(),
                    did
                )
            )?;
        }
        match action {
            DesiredAction::InstallCutoff => {
                let was = before_map.get(name.as_str());
                if was.is_none() || matches!(was, Some(DesiredAction::RefuseTooNew)) {
                    eligible.push(name.clone());
                }
            }
            DesiredAction::RefuseTooNew => soaking.push(name.clone()),
            DesiredAction::RefuseYanked => gone.push(name.clone()),
            _ => {}
        }
    }
    write_section(out, "==> Became eligible", &eligible)?;
    write_section(out, "==> Still soaking", &soaking)?;
    write_section(out, "==> Gone at HEAD", &gone)?;
    if eligible.is_empty() && soaking.is_empty() && gone.is_empty() {
        writeln!(out, "no installed packages changed soak status")?;
    }
    Ok(())
}

pub fn prefetch_installed(brew: &impl Brew, git: &impl GitStore, cache: &Path, snaps: &Snapshots) {
    let Ok(installed) = brew.installed_core() else {
        return;
    };
    for pkg in installed {
        let _ = resolve_pkg_blobs(git, snaps, cache, &pkg.name, pkg.kind);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn update(
    brew: &impl Brew,
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    hours: SoakHours,
    now: time::OffsetDateTime,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    writeln!(out, "updating soak snapshots; soak window {}h", hours.get())?;
    let previous = snapshot::load_state(cache)?;
    let before = match &previous {
        Some(prev) => classify_installed(brew, git, cache, prev).unwrap_or_default(),
        None => Vec::new(),
    };
    let snaps = snapshot::refresh(git, gh, cache, hours, now, out)?;
    writeln!(
        out,
        "core cutoff: {}",
        report::format_cutoff(&snaps.core.cutoff_sha, snaps.core.cutoff_time)
    )?;
    writeln!(
        out,
        "core head: {}",
        report::short_sha(&snaps.core.head_sha)
    )?;
    writeln!(
        out,
        "cask cutoff: {}",
        report::format_cutoff(&snaps.cask.cutoff_sha, snaps.cask.cutoff_time)
    )?;
    writeln!(
        out,
        "cask head: {}",
        report::short_sha(&snaps.cask.head_sha)
    )?;
    prefetch_installed(brew, git, cache, &snaps);
    write_update_summary(brew, git, cache, &snaps, &before, verbose, out)?;
    writeln!(out, "snapshots refreshed")?;
    Ok(())
}

pub fn outdated(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    extra_args: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let brew_status = passthrough_third_party(brew, "outdated", extra_args, extra_args)?;
    let verbose = is_verbose(extra_args);
    if verbose {
        writeln!(
            out,
            "{}",
            report::soak_banner(
                "checking outdated",
                snaps.hours.get(),
                &snaps.core,
                &snaps.cask
            )
        )?;
    }
    let installed = brew.installed_core()?;
    let mut upgrades = Vec::new();
    let mut held = Vec::new();
    let mut ahead = Vec::new();
    let mut pinned = Vec::new();
    let mut soaked = 0usize;
    for pkg in &installed {
        if pkg.pinned {
            pinned.push(pkg.name.clone());
            if verbose {
                writeln!(out, "{}: pinned; skipped", pkg.name)?;
            }
            continue;
        }
        let Some(view) = resolve_view(
            git,
            snaps,
            cache,
            &pkg.name,
            pkg.kind,
            Some(&pkg.receipt_rb),
        )?
        else {
            held.push(format!("{}: unparseable identity", pkg.name));
            if verbose {
                writeln!(out, "{}: unparseable identity; skipped", pkg.name)?;
            }
            continue;
        };
        for warn in &view.warnings {
            writeln!(out, "warning: {warn}")?;
        }
        if verbose {
            writeln!(
                out,
                "{}",
                report::evaluate_line(
                    &pkg.name,
                    view.action,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.head.as_ref(),
                    "classified for outdated",
                )
            )?;
        }
        match view.action {
            DesiredAction::InstallCutoff => {
                let installed_ver = view
                    .installed
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or("unknown");
                let cutoff_ver = view
                    .cutoff
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or("none");
                upgrades.push(format!("{} ({installed_ver}) < {cutoff_ver}", pkg.name));
            }
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => {
                if let Some(why) = hold_why(view.action) {
                    held.push(format!("{}: {why}", pkg.name));
                }
            }
            DesiredAction::LeaveAheadOfSoak => ahead.push(pkg.name.clone()),
            DesiredAction::NoOpAlreadySoaked => soaked += 1,
        }
    }
    write_section_always(out, "==> Outdated (will upgrade)", &upgrades)?;
    write_section_always(out, "==> Held", &held)?;
    write_section_always(out, "==> Ahead of soak", &ahead)?;
    write_section_always(out, "==> Pinned", &pinned)?;
    if upgrades.is_empty() && held.is_empty() && ahead.is_empty() && pinned.is_empty() {
        writeln!(out, "nothing outdated (already soaked: {soaked})")?;
    }
    Ok(RunResult {
        refused: false,
        brew_status,
    })
}

pub fn info(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let mut brew_status = None;
    let installed = brew.installed_core()?;
    let verbose = is_verbose(user_flags);
    let long_form = verbose || !names.is_empty();
    if verbose {
        writeln!(
            out,
            "{}",
            report::soak_banner("showing info", snaps.hours.get(), &snaps.core, &snaps.cask)
        )?;
    }
    let owned_names: Vec<String> = if names.is_empty() {
        installed.iter().map(|p| p.name.clone()).collect()
    } else {
        names.to_vec()
    };
    for (i, name) in owned_names.iter().enumerate() {
        if resolve::is_third_party(name) {
            let mut args = vec!["info".to_string()];
            args.extend(user_flags.iter().cloned());
            args.push(name.clone());
            merge_status(&mut brew_status, brew.run_visible(&args)?);
            continue;
        }
        if long_form && i > 0 {
            writeln!(out)?;
        }
        let Some(view) = resolve_named(git, snaps, cache, name, &installed)? else {
            if long_form {
                writeln!(out, "{name}")?;
                writeln!(out, "unparseable identity")?;
            } else {
                writeln!(out, "{name}  -  unparseable identity")?;
            }
            if verbose {
                writeln!(out, "{name}: unparseable identity; skipped")?;
            }
            continue;
        };
        if verbose {
            writeln!(
                out,
                "{}",
                report::evaluate_line(
                    name,
                    view.action,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.head.as_ref(),
                    "classified for info",
                )
            )?;
        }
        if long_form {
            writeln!(out, "{name}")?;
            writeln!(
                out,
                "installed: {}",
                view.installed
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or("not installed")
            )?;
            writeln!(
                out,
                "cutoff: {}",
                view.cutoff
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or("none")
            )?;
            writeln!(
                out,
                "head: {}",
                view.head
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or("none")
            )?;
            writeln!(out, "action: {}", report::human_action(view.action))?;
            for warn in &view.warnings {
                writeln!(out, "warning: {warn}")?;
            }
        } else {
            writeln!(
                out,
                "{}",
                report::compact_info_line(
                    name,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.action,
                )
            )?;
        }
    }
    Ok(RunResult {
        refused: false,
        brew_status,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn upgrade(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let installed = brew.installed_core()?;
    let targets: Vec<String> = if names.is_empty() {
        installed.iter().map(|p| p.name.clone()).collect()
    } else {
        names.to_vec()
    };
    apply_many(
        brew, git, snaps, cache, tap_root, &targets, "upgrade", false, false, user_flags,
        &installed, out,
    )
}

#[allow(clippy::too_many_arguments)]
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
) -> Result<RunResult, Error> {
    if names.is_empty() {
        return Err(Error::Usage("install: no packages specified".into()));
    }
    let installed = brew.installed_core()?;
    apply_many(
        brew,
        git,
        snaps,
        cache,
        tap_root,
        names,
        "install",
        force_cask,
        force_formula,
        user_flags,
        &installed,
        out,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn reinstall(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    if names.is_empty() {
        return Err(Error::Usage("reinstall: no packages specified".into()));
    }
    let installed = brew.installed_core()?;
    let mut session = ApplySession {
        brew,
        git,
        snaps,
        cache,
        tap_root,
        user_flags,
        installed: &installed,
        brew_verb: "reinstall",
        force_cask: false,
        force_formula: false,
        refused: false,
        brew_status: None,
        counts: Counts::default(),
        out,
    };
    if is_verbose(user_flags) {
        writeln!(
            session.out,
            "{}",
            report::soak_banner("reinstalling", snaps.hours.get(), &snaps.core, &snaps.cask)
        )?;
    }
    for name in names {
        if resolve::is_third_party(name) {
            session.apply_one(name)?;
            continue;
        }
        let Some(pkg) = installed.iter().find(|p| p.name == *name) else {
            return Err(Error::Refusal(format!(
                "reinstall: no installed keg: {name}"
            )));
        };
        let Some(view) = resolve_view(git, snaps, cache, name, pkg.kind, Some(&pkg.receipt_rb))?
        else {
            writeln!(session.out, "{name}: unparseable identity; skipping")?;
            continue;
        };
        if view.installed.as_ref() == view.head.as_ref() {
            if is_verbose(user_flags) {
                writeln!(
                    session.out,
                    "{name}: installed matches HEAD; brew reinstall (true repair)"
                )?;
            }
            let mut args = vec!["reinstall".to_string()];
            args.extend(user_flags.iter().cloned());
            args.push(name.to_string());
            session.record_run(&args)?;
            session.counts.upgraded += 1;
            continue;
        }
        session.apply_one(name)?;
    }
    writeln!(session.out, "{}", report::counts_line(&session.counts))?;
    Ok(RunResult {
        refused: session.refused,
        brew_status: session.brew_status,
    })
}

#[allow(clippy::too_many_arguments)]
fn apply_many(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String],
    brew_verb: &str,
    force_cask: bool,
    force_formula: bool,
    user_flags: &[String],
    installed: &[InstalledPkg],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let mut session = ApplySession {
        brew,
        git,
        snaps,
        cache,
        tap_root,
        user_flags,
        installed,
        brew_verb,
        force_cask,
        force_formula,
        refused: false,
        brew_status: None,
        counts: Counts::default(),
        out,
    };
    if is_verbose(user_flags) {
        let doing = match brew_verb {
            "upgrade" => "upgrading installed formulae and casks",
            "install" => "installing",
            "reinstall" => "reinstalling",
            other => other,
        };
        writeln!(
            session.out,
            "{}",
            report::soak_banner(doing, snaps.hours.get(), &snaps.core, &snaps.cask)
        )?;
    }
    for name in names {
        session.apply_one(name)?;
    }
    writeln!(session.out, "{}", report::counts_line(&session.counts))?;
    if session.counts.nothing_to_do() && session.counts.soaked > 0 && brew_verb == "upgrade" {
        writeln!(
            session.out,
            "already soaked: {} formulae and casks",
            session.counts.soaked
        )?;
    }
    Ok(RunResult {
        refused: session.refused,
        brew_status: session.brew_status,
    })
}

struct ApplySession<'a, B, G, W> {
    brew: &'a B,
    git: &'a G,
    snaps: &'a Snapshots,
    cache: &'a Path,
    tap_root: &'a Path,
    user_flags: &'a [String],
    installed: &'a [InstalledPkg],
    brew_verb: &'a str,
    force_cask: bool,
    force_formula: bool,
    refused: bool,
    brew_status: Option<i32>,
    counts: Counts,
    out: &'a mut W,
}

impl<B: Brew, G: GitStore, W: Write> ApplySession<'_, B, G, W> {
    fn apply_one(&mut self, name: &str) -> Result<(), Error> {
        if resolve::is_third_party(name) {
            if is_verbose(self.user_flags) {
                writeln!(
                    self.out,
                    "{name}: third-party; passing through to brew {}",
                    self.brew_verb
                )?;
            }
            let mut args = vec![self.brew_verb.to_string()];
            args.extend(self.user_flags.iter().cloned());
            args.push(name.to_string());
            self.record_run(&args)?;
            self.counts.upgraded += 1;
            return Ok(());
        }

        if self.installed.iter().any(|p| p.name == name && p.pinned) && self.brew_verb == "upgrade"
        {
            self.counts.pinned += 1;
            if is_verbose(self.user_flags) {
                writeln!(self.out, "{name}: pinned; skipped")?;
            }
            return Ok(());
        }

        let kind = self.resolve_kind(name)?;
        let receipt = self
            .installed
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.receipt_rb.as_str());
        let Some(view) = resolve_view(self.git, self.snaps, self.cache, name, kind, receipt)?
        else {
            self.counts.skipped += 1;
            writeln!(self.out, "{name}: unparseable identity; skipping")?;
            return Ok(());
        };
        for warn in &view.warnings {
            writeln!(self.out, "warning: {warn}")?;
        }
        let did = match view.action {
            DesiredAction::InstallCutoff => "installing cutoff",
            DesiredAction::NoOpAlreadySoaked => "left unchanged",
            DesiredAction::LeaveAheadOfSoak => "left unchanged",
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => "refused",
        };
        if is_verbose(self.user_flags) {
            writeln!(
                self.out,
                "{}",
                report::evaluate_line(
                    name,
                    view.action,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.head.as_ref(),
                    did,
                )
            )?;
        }
        self.counts.note(view.action);
        match view.action {
            DesiredAction::NoOpAlreadySoaked => {
                if self.brew_verb == "install" {
                    writeln!(self.out, "{name} is already installed")?;
                }
            }
            DesiredAction::LeaveAheadOfSoak => {
                if self.brew_verb == "reinstall" {
                    writeln!(
                        self.out,
                        "{name} is ahead of soak; reinstall would pull a too-new artifact; use `brew reinstall {name}` to bypass brewsoakr."
                    )?;
                    self.refused = true;
                } else {
                    writeln!(self.out, "{}", ahead_message(name))?;
                }
            }
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => {
                if let Some(msg) = refusal_message(view.action, name, self.brew_verb) {
                    writeln!(self.out, "{msg}")?;
                }
                self.refused = true;
            }
            DesiredAction::InstallCutoff => {
                self.install_cutoff(name, kind, &view)?;
            }
        }
        Ok(())
    }

    fn install_cutoff(
        &mut self,
        name: &str,
        kind: PkgKind,
        view: &ResolvedView,
    ) -> Result<(), Error> {
        let pkg = PkgRef {
            name: name.to_string(),
            kind,
        };
        let blob = view.cutoff_blob.as_deref().ok_or_else(|| {
            Error::Other(format!("{name} is eligible but the cutoff blob is missing"))
        })?;
        let path = tap::write_blob(self.tap_root, &pkg, blob)?;

        let deps = self.collect_cutoff_deps(name, kind)?;
        for (dep, dep_kind) in deps {
            if self.installed.iter().any(|p| p.name == dep) {
                continue;
            }
            if !self.install_missing_dep(name, dep_kind, &dep)? {
                return Ok(());
            }
        }

        let args = tap::brew_install_args(&pkg, &path, self.user_flags);
        self.record_run(&args)
    }

    fn collect_cutoff_deps(
        &self,
        root: &str,
        root_kind: PkgKind,
    ) -> Result<Vec<(String, PkgKind)>, Error> {
        let mut walk = CutoffDepWalk {
            brew: self.brew,
            git: self.git,
            snaps: self.snaps,
            cache: self.cache,
            tap_root: self.tap_root,
            installed: self.installed,
            visiting: HashSet::new(),
            visited: HashSet::new(),
            out: Vec::new(),
        };
        walk.visit(root, root_kind, false)?;
        Ok(walk.out)
    }

    /// Install a missing cutoff dep. Returns false when the target was refused.
    fn install_missing_dep(
        &mut self,
        target: &str,
        kind: PkgKind,
        dep: &str,
    ) -> Result<bool, Error> {
        let blobs = resolve_pkg_blobs(self.git, self.snaps, self.cache, dep, kind)?;
        let status = eligibility::upstream_status(
            blobs.cutoff.as_deref(),
            blobs.head.as_deref(),
            &calendar_today(),
        );
        if status != UpstreamStatus::Eligible {
            self.refuse_ineligible_dep(target, dep, status)?;
            return Ok(false);
        }
        let blob = blobs.cutoff.as_deref().ok_or_else(|| {
            Error::Other(format!("{dep} is eligible but the cutoff blob is missing"))
        })?;
        let pkg = PkgRef {
            name: dep.to_string(),
            kind,
        };
        let path = tap::write_blob(self.tap_root, &pkg, blob)?;
        let args = tap::brew_install_args(&pkg, &path, &[]);
        self.record_run(&args)?;
        Ok(true)
    }

    fn refuse_ineligible_dep(
        &mut self,
        target: &str,
        dep: &str,
        status: UpstreamStatus,
    ) -> Result<(), Error> {
        self.refused = true;
        let why = match status {
            UpstreamStatus::TooNew => {
                format!("{dep} is too new (born inside the soak window)")
            }
            UpstreamStatus::Yanked => format!("{dep} is missing at HEAD (yanked)"),
            UpstreamStatus::Deprecated => {
                format!("{dep} is deprecated or disabled at HEAD")
            }
            UpstreamStatus::Eligible => return Ok(()),
        };
        writeln!(
            self.out,
            "cannot install {target}: dependency {why}; use `brew {} {target}` to bypass brewsoakr.",
            self.brew_verb
        )?;
        Ok(())
    }

    fn resolve_kind(&self, name: &str) -> Result<PkgKind, Error> {
        if self.force_cask {
            return Ok(PkgKind::Cask);
        }
        if self.force_formula {
            return Ok(PkgKind::Formula);
        }
        natural_kind(self.git, self.snaps, self.cache, self.installed, name)
    }

    fn record_run(&mut self, args: &[String]) -> Result<(), Error> {
        let output = self.brew.run_visible(args)?;
        merge_status(&mut self.brew_status, output);
        Ok(())
    }
}

fn merge_status(slot: &mut Option<i32>, output: std::process::Output) {
    let mut code = output.status.code().unwrap_or(1);
    if code != 0 && already_installed_message(&output) {
        code = 0;
    }
    *slot = Some(match *slot {
        Some(prev) => prev.max(code),
        None => code,
    });
}

pub fn is_verbose(flags: &[String]) -> bool {
    flags.iter().any(|f| f == "-v" || f == "--verbose")
}

fn already_installed_message(output: &std::process::Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{stdout}\n{stderr}")
        .to_ascii_lowercase()
        .contains("already installed")
}

fn passthrough_third_party(
    brew: &impl Brew,
    verb: &str,
    extra_args: &[String],
    name_args: &[String],
) -> Result<Option<i32>, Error> {
    let third: Vec<String> = name_args
        .iter()
        .filter(|a| !a.starts_with('-') && resolve::is_third_party(a))
        .cloned()
        .collect();
    if third.is_empty() {
        return Ok(None);
    }
    let mut args = vec![verb.to_string()];
    args.extend(extra_args.iter().filter(|a| a.starts_with('-')).cloned());
    args.extend(third);
    let output = brew.run_visible(&args)?;
    let mut status = None;
    merge_status(&mut status, output);
    Ok(status)
}

struct CutoffDepWalk<'a, B, G> {
    brew: &'a B,
    git: &'a G,
    snaps: &'a Snapshots,
    cache: &'a Path,
    tap_root: &'a Path,
    installed: &'a [InstalledPkg],
    visiting: HashSet<String>,
    visited: HashSet<String>,
    out: Vec<(String, PkgKind)>,
}

impl<B: Brew, G: GitStore> CutoffDepWalk<'_, B, G> {
    fn visit(&mut self, name: &str, kind: PkgKind, include_self: bool) -> Result<(), Error> {
        if self.visiting.contains(name) || self.visited.contains(name) {
            return Ok(());
        }
        self.visiting.insert(name.to_string());
        if include_self {
            write_cutoff_blob(self.git, self.snaps, self.cache, self.tap_root, name, kind)?;
        }
        let staged = match kind {
            PkgKind::Formula => tap::tap_formula_path(self.tap_root, name),
            PkgKind::Cask => tap::tap_cask_path(self.tap_root, name),
        };
        let token = staged.to_string_lossy().into_owned();
        for dep in self.brew.deps(kind, &token)? {
            if !cutoff_in_either_tree(self.git, self.snaps, self.cache, &dep)? {
                continue;
            }
            let dep_kind = natural_kind(self.git, self.snaps, self.cache, self.installed, &dep)?;
            self.visit(&dep, dep_kind, true)?;
        }
        self.visiting.remove(name);
        self.visited.insert(name.to_string());
        if include_self {
            self.out.push((name.to_string(), kind));
        }
        Ok(())
    }
}

struct ResolvedView {
    installed: Option<PkgIdentity>,
    cutoff: Option<PkgIdentity>,
    head: Option<PkgIdentity>,
    action: DesiredAction,
    cutoff_blob: Option<Vec<u8>>,
    warnings: Vec<String>,
}

fn resolve_named(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
    installed: &[InstalledPkg],
) -> Result<Option<ResolvedView>, Error> {
    if let Some(pkg) = installed.iter().find(|p| p.name == name) {
        return resolve_view(git, snaps, cache, name, pkg.kind, Some(&pkg.receipt_rb));
    }
    let formula_blobs = resolve_pkg_blobs(git, snaps, cache, name, PkgKind::Formula)?;
    if formula_blobs.cutoff.is_some() || formula_blobs.head.is_some() {
        return resolve_view(git, snaps, cache, name, PkgKind::Formula, None);
    }
    resolve_view(git, snaps, cache, name, PkgKind::Cask, None)
}

fn resolve_view(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
    kind: PkgKind,
    receipt_rb: Option<&str>,
) -> Result<Option<ResolvedView>, Error> {
    let blobs = resolve_pkg_blobs(git, snaps, cache, name, kind)?;
    let installed = match receipt_rb {
        Some(rb) => match parse_pkg(kind, rb) {
            Ok(id) => Some(id),
            Err(_) => return Ok(None),
        },
        None => None,
    };
    let cutoff = match blobs.cutoff.as_deref() {
        Some(bytes) => match parse_pkg_bytes(kind, bytes) {
            Ok(id) => Some(id),
            Err(_) => return Ok(None),
        },
        None => None,
    };
    let head = match blobs.head.as_deref() {
        Some(bytes) => match parse_pkg_bytes(kind, bytes) {
            Ok(id) => Some(id),
            Err(_) => return Ok(None),
        },
        None => None,
    };
    let today = calendar_today();
    let status =
        eligibility::upstream_status(blobs.cutoff.as_deref(), blobs.head.as_deref(), &today);
    let action =
        eligibility::desired_action(status, installed.as_ref(), cutoff.as_ref(), head.as_ref());
    let warnings = match blobs
        .head
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
    {
        Some(rb) => identity::upcoming_lifecycle_messages(rb, &today)
            .into_iter()
            .map(|msg| format!("{name} {msg}"))
            .collect(),
        None => Vec::new(),
    };
    Ok(Some(ResolvedView {
        installed,
        cutoff,
        head,
        action,
        cutoff_blob: blobs.cutoff,
        warnings,
    }))
}

fn calendar_today() -> String {
    let d = time::OffsetDateTime::now_utc().date();
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

fn resolve_pkg_blobs(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
    kind: PkgKind,
) -> Result<resolve::ResolvedBlobs, Error> {
    let repo = tap_repo(cache, kind);
    let tap = match kind {
        PkgKind::Formula => &snaps.core,
        PkgKind::Cask => &snaps.cask,
    };
    let pkg = PkgRef {
        name: name.to_string(),
        kind,
    };
    resolve::resolve_blobs(git, &repo, &tap.cutoff_sha, &tap.head_sha, &pkg)
}

fn cutoff_blob_exists(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
    kind: PkgKind,
) -> Result<bool, Error> {
    Ok(resolve_pkg_blobs(git, snaps, cache, name, kind)?
        .cutoff
        .is_some())
}

fn cutoff_in_either_tree(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
) -> Result<bool, Error> {
    Ok(
        cutoff_blob_exists(git, snaps, cache, name, PkgKind::Formula)?
            || cutoff_blob_exists(git, snaps, cache, name, PkgKind::Cask)?,
    )
}

fn natural_kind(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    installed: &[InstalledPkg],
    name: &str,
) -> Result<PkgKind, Error> {
    if let Some(pkg) = installed.iter().find(|p| p.name == name) {
        return Ok(pkg.kind);
    }
    let formula = resolve_pkg_blobs(git, snaps, cache, name, PkgKind::Formula)?;
    if formula.cutoff.is_some() || formula.head.is_some() {
        Ok(PkgKind::Formula)
    } else {
        Ok(PkgKind::Cask)
    }
}

fn write_cutoff_blob(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    name: &str,
    kind: PkgKind,
) -> Result<(), Error> {
    let blobs = resolve_pkg_blobs(git, snaps, cache, name, kind)?;
    let blob = blobs.cutoff.as_deref().ok_or_else(|| {
        Error::Other(format!("{name} is eligible but the cutoff blob is missing"))
    })?;
    let pkg = PkgRef {
        name: name.to_string(),
        kind,
    };
    tap::write_blob(tap_root, &pkg, blob)?;
    Ok(())
}

fn tap_repo(cache: &Path, kind: PkgKind) -> PathBuf {
    match kind {
        PkgKind::Formula => cache.join("core.git"),
        PkgKind::Cask => cache.join("cask.git"),
    }
}

fn parse_pkg(kind: PkgKind, rb: &str) -> Result<PkgIdentity, Error> {
    match kind {
        PkgKind::Formula => Ok(PkgIdentity::Formula(identity::parse_formula(rb)?)),
        PkgKind::Cask => Ok(PkgIdentity::Cask(identity::parse_cask(rb)?)),
    }
}

fn parse_pkg_bytes(kind: PkgKind, bytes: &[u8]) -> Result<PkgIdentity, Error> {
    let rb = std::str::from_utf8(bytes)
        .map_err(|_| Error::Other("package blob is not valid UTF-8".into()))?;
    parse_pkg(kind, rb)
}

fn hold_why(action: DesiredAction) -> Option<&'static str> {
    match action {
        DesiredAction::RefuseTooNew => Some("too new (born inside the soak window)"),
        DesiredAction::RefuseYanked => Some("missing at HEAD (yanked)"),
        DesiredAction::RefuseDeprecated => Some("deprecated or disabled at HEAD"),
        _ => None,
    }
}

fn write_section(out: &mut impl Write, header: &str, lines: &[String]) -> Result<(), Error> {
    if lines.is_empty() {
        return Ok(());
    }
    write_section_always(out, header, lines)
}

fn write_section_always(out: &mut impl Write, header: &str, lines: &[String]) -> Result<(), Error> {
    writeln!(out, "{header}")?;
    if lines.is_empty() {
        writeln!(out, "(none)")?;
    } else {
        for line in lines {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brew::{InstalledPkg, MockBrew};
    use crate::eligibility::DesiredAction;
    use crate::git::InMemoryGit;
    use crate::github::{CommitInfo, StaticGithub};
    use crate::resolve::PkgKind;
    use crate::snapshot::TapSnapshot;
    use std::path::Path;
    use time::{Duration, OffsetDateTime};

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

    fn assert_refuse_copy(action: DesiredAction, name: &str, brew_verb: &str) {
        let msg = refusal_message(action, name, brew_verb).expect("refuse message");
        assert!(
            msg.contains(&format!("brew {brew_verb} {name}")),
            "{action:?} missing `brew {brew_verb} {name}`: {msg}"
        );
        assert!(
            !msg.contains("--now"),
            "{action:?} must not mention --now: {msg}"
        );
    }

    #[test]
    fn refuse_too_new_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseTooNew, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseTooNew, "wget", "upgrade");
    }

    #[test]
    fn refuse_yanked_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseYanked, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseYanked, "wget", "upgrade");
    }

    #[test]
    fn refuse_deprecated_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseDeprecated, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseDeprecated, "wget", "upgrade");
    }

    #[test]
    fn leave_ahead_of_soak_is_not_a_refusal() {
        assert_eq!(
            refusal_message(DesiredAction::LeaveAheadOfSoak, "wget", "upgrade"),
            None
        );
    }

    #[test]
    fn combine_exit_refused_with_ok_brew_is_1() {
        assert_eq!(combine_exit(true, Some(0)), 1);
    }

    #[test]
    fn combine_exit_success_is_0() {
        assert_eq!(combine_exit(false, Some(0)), 0);
    }

    #[test]
    fn combine_exit_brew_gt_1_wins() {
        assert_eq!(combine_exit(true, Some(3)), 3);
    }

    #[test]
    fn update_writes_shas_and_creates_state_toml() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let hours = SoakHours::new(24).expect("hours >= 1");
        let mut out = Vec::new();
        update(
            &MockBrew::new(),
            &git,
            &fixture_gh(),
            dir.path(),
            hours,
            now(),
            false,
            &mut out,
        )
        .expect("update");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("thirtyh"), "{text}");
        assert!(text.contains("headsha"), "{text}");
        assert!(text.contains("fetching Homebrew/homebrew-core"), "{text}");
        assert!(text.contains("soak window 24h"), "{text}");
        assert!(
            dir.path().join("state.toml").is_file(),
            "state.toml missing"
        );
    }

    fn unused_cache() -> &'static Path {
        Path::new("/brewsoak-in-memory-unused")
    }

    fn formula_rb(name: &str, version: &str, sha: &str) -> String {
        format!(
            "class X < Formula\n  url \"https://example.com/{name}-{version}.tar.gz\"\n  sha256 \"{sha}\"\nend\n"
        )
    }

    fn view_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let alpha_old = formula_rb("alpha", "1.0.0", "oldsha");
        let alpha_mid = formula_rb("alpha", "1.1.0", "midsha");
        let alpha_new = formula_rb("alpha", "1.2.0", "newsha");
        let beta_old = formula_rb("beta", "1.0.0", "oldsha");
        let beta_new = formula_rb("beta", "1.2.0", "newsha");
        let gamma_mid = formula_rb("gamma", "1.1.0", "midsha");
        let gamma_new = formula_rb("gamma", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", alpha_mid);
        git.insert_blob("headsha", "Formula/a/alpha.rb", alpha_new);
        git.insert_blob("headsha", "Formula/b/beta.rb", beta_new);
        git.insert_blob("cutoffsha", "Formula/g/gamma.rb", gamma_mid);
        git.insert_blob("headsha", "Formula/g/gamma.rb", gamma_new.clone());

        let brew = MockBrew {
            installed: vec![
                formula_pkg("alpha", alpha_old),
                formula_pkg("beta", beta_old),
                formula_pkg("gamma", gamma_new),
            ],
            ..MockBrew::new()
        };
        let snaps = Snapshots {
            core: TapSnapshot {
                cutoff_sha: "cutoffsha".into(),
                head_sha: "headsha".into(),
                cutoff_time: None,
            },
            cask: TapSnapshot {
                cutoff_sha: "caskcut".into(),
                head_sha: "caskhead".into(),
                cutoff_time: None,
            },
            hours: SoakHours::new(24).expect("hours >= 1"),
        };
        (brew, git, snaps)
    }

    #[test]
    fn outdated_lists_upgrade_held_and_ahead_sections() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let result =
            outdated(&brew, &git, &snaps, unused_cache(), &[], &mut out).expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("==> Outdated (will upgrade)"),
            "missing outdated header: {text}"
        );
        assert!(text.contains("==> Held"), "missing held header: {text}");
        assert!(
            text.contains("==> Ahead of soak"),
            "missing ahead header: {text}"
        );
        assert!(text.contains("alpha"), "missing alpha: {text}");
        assert!(text.contains("beta"), "missing beta: {text}");
        assert!(text.contains("gamma"), "missing gamma: {text}");
        assert!(!result.refused, "listing holds is not a refusal");
    }

    #[test]
    fn outdated_warns_future_deprecate_and_does_not_hold() {
        let old = formula_rb("py", "3.13.0", "oldsha");
        let mid = formula_rb("py", "3.14.0", "midsha");
        let new = format!(
            "{}\n  deprecate! date: \"2099-11-01\", because: :deprecated_upstream\n",
            formula_rb("py", "3.14.1", "newsha").trim_end()
        );
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/p/py.rb", mid);
        git.insert_blob("headsha", "Formula/p/py.rb", new);
        let brew = MockBrew {
            installed: vec![formula_pkg("py", old)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let mut out = Vec::new();
        outdated(&brew, &git, &snaps, unused_cache(), &[], &mut out).expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("warning: py scheduled to be deprecated on 2099-11-01"),
            "{text}"
        );
        assert!(text.contains("==> Outdated"), "{text}");
        assert!(
            !text.contains("py:") || !text.contains("Held"),
            "future deprecate must not hold: {text}"
        );
    }

    #[test]
    fn info_mentions_cutoff_version_and_install_cutoff() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let names = ["alpha".to_string()];
        let result =
            info(&brew, &git, &snaps, unused_cache(), &names, &[], &mut out).expect("info");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("1.1.0"), "missing cutoff version: {text}");
        assert!(
            text.contains("install cutoff") || text.to_ascii_lowercase().contains("upgrade"),
            "missing install cutoff / upgrade wording: {text}"
        );
        assert!(!result.refused, "info is read-only");
    }

    #[test]
    fn info_without_names_lists_installed_core() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let result = info(&brew, &git, &snaps, unused_cache(), &[], &[], &mut out).expect("info");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "missing installed alpha: {text}");
        assert!(text.contains("beta"), "missing installed beta: {text}");
        assert!(text.contains("gamma"), "missing installed gamma: {text}");
        assert!(
            !text.contains("action:"),
            "nameless info must be compact: {text}"
        );
        assert!(!result.refused, "info is read-only");
    }

    fn core_snaps() -> Snapshots {
        Snapshots {
            core: TapSnapshot {
                cutoff_sha: "cutoffsha".into(),
                head_sha: "headsha".into(),
                cutoff_time: None,
            },
            cask: TapSnapshot {
                cutoff_sha: "caskcut".into(),
                head_sha: "caskhead".into(),
                cutoff_time: None,
            },
            hours: SoakHours::new(24).expect("hours >= 1"),
        }
    }

    fn formula_pkg(name: &str, receipt_rb: String) -> InstalledPkg {
        InstalledPkg {
            name: name.into(),
            kind: PkgKind::Formula,
            receipt_rb,
            pinned: false,
        }
    }

    fn formula_pkg_pinned(name: &str, receipt_rb: String) -> InstalledPkg {
        InstalledPkg {
            pinned: true,
            ..formula_pkg(name, receipt_rb)
        }
    }

    fn lock_runs(brew: &MockBrew) -> Vec<Vec<String>> {
        brew.runs.lock().expect("runs").clone()
    }

    fn run_has_token(runs: &[Vec<String>], token: &str) -> bool {
        runs.iter().any(|args| args.iter().any(|a| a == token))
    }

    fn run_is_soaked_install(runs: &[Vec<String>], name: &str) -> bool {
        let suffix = format!("/{name}.rb");
        runs.iter().any(|args| {
            args.first().map(String::as_str) == Some("install")
                && args.iter().any(|a| a.ends_with(&suffix))
                && !args.iter().any(|a| a == "--ignore-dependencies")
        })
    }

    #[test]
    fn upgrade_mixed_applies_eligible_and_refuses_too_new() {
        let ok_old = formula_rb("ok", "1.0.0", "oldsha");
        let ok_mid = formula_rb("ok", "1.1.0", "midsha");
        let ok_new = formula_rb("ok", "1.2.0", "newsha");
        let new_old = formula_rb("new", "1.0.0", "oldsha");
        let new_head = formula_rb("new", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/o/ok.rb", ok_mid);
        git.insert_blob("headsha", "Formula/o/ok.rb", ok_new);
        git.insert_blob("headsha", "Formula/n/new.rb", new_head);

        let brew = MockBrew {
            installed: vec![formula_pkg("ok", ok_old), formula_pkg("new", new_old)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "ok"),
            "expected soaked path install of ok without --ignore-dependencies: {runs:?}"
        );
        assert!(
            !run_has_token(&runs, "new") && !run_has_token(&runs, "brewsoakr/soaked/new"),
            "must not install too-new package: {runs:?}"
        );
        assert!(result.refused, "mixed upgrade must refuse the too-new pkg");
    }

    #[test]
    fn upgrade_ahead_of_soak_is_not_a_refusal() {
        let ahead_mid = formula_rb("ahead", "1.1.0", "midsha");
        let ahead_new = formula_rb("ahead", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/ahead.rb", ahead_mid);
        git.insert_blob("headsha", "Formula/a/ahead.rb", ahead_new.clone());

        let brew = MockBrew {
            installed: vec![formula_pkg("ahead", ahead_new)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        let runs = lock_runs(&brew);
        assert!(!result.refused, "ahead of soak is not a refusal");
        assert!(
            !runs
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("install")),
            "ahead of soak must not install: {runs:?}"
        );
        assert!(
            text.contains(&ahead_message("ahead")),
            "missing ahead message: {text}"
        );
    }

    #[test]
    fn upgrade_already_soaked_is_silent_by_default() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid.clone());
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_mid)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("wget is already soaked"),
            "already soaked must be silent without -v: {text}"
        );
        assert!(
            text.contains("already soaked"),
            "summary must mention already soaked: {text}"
        );
        assert!(
            lock_runs(&brew).is_empty(),
            "already soaked must not invoke brew"
        );
    }

    #[test]
    fn upgrade_already_soaked_prints_when_verbose() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid.clone());
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_mid)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &[],
            &["--verbose".to_string()],
            &mut out,
        )
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("wget:") && text.contains("soaked"),
            "verbose upgrade must print already soaked: {text}"
        );
    }

    #[test]
    fn install_fresh_eligible_runs_install() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);

        let brew = MockBrew::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "fresh"),
            "expected soaked install of fresh: {runs:?}"
        );
        assert!(!result.refused, "eligible fresh install must not refuse");
    }

    #[test]
    fn install_fresh_too_new_refuses() {
        let fresh_head = formula_rb("fresh", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_head);

        let brew = MockBrew::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let text = String::from_utf8(out).expect("utf8");
        let runs = lock_runs(&brew);
        assert!(
            !runs
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("install")),
            "too-new install must not run brew install: {runs:?}"
        );
        assert!(
            text.contains("brew install fresh"),
            "refusal must mention brew install fresh: {text}"
        );
        assert!(result.refused, "too-new install must refuse");
    }

    #[test]
    fn install_refuses_target_when_dep_yanked() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let lib_mid = formula_rb("lib", "1.0.0", "libsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        git.insert_blob("cutoffsha", "Formula/lib/lib.rb", lib_mid);

        let mut deps = std::collections::BTreeMap::new();
        deps.insert("fresh".into(), vec!["lib".into()]);
        let brew = MockBrew {
            deps,
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/fresh"),
            "yanked dep must refuse target install: {runs:?}"
        );
        assert!(result.refused, "missing yanked dep refuses the target");
    }

    #[test]
    fn upgrade_third_party_passthrough() {
        let brew = MockBrew::new();
        let git = InMemoryGit::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["acme/tools/foo".to_string()];
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_has_token(&runs, "acme/tools/foo"),
            "third-party must brew.run original name: {runs:?}"
        );
        assert!(
            !runs
                .iter()
                .any(|args| args.iter().any(|a| a.contains("brewsoakr/soaked"))),
            "third-party must not use soaked tap: {runs:?}"
        );
        let _ = result;
    }

    fn cask_rb(name: &str, version: &str) -> String {
        format!(
            "cask \"{name}\"\n  version \"{version}\"\n  sha256 \"cafebabe\"\n  url \"https://example.com/{name}-{version}.zip\"\n"
        )
    }

    fn run_is_soaked_dep_install(runs: &[Vec<String>], name: &str, kind_flag: &str) -> bool {
        let suffix = format!("/{name}.rb");
        runs.iter().any(|args| {
            args.first().map(String::as_str) == Some("install")
                && args.iter().any(|a| a.ends_with(&suffix))
                && args.iter().any(|a| a == kind_flag)
                && !args.iter().any(|a| a == "--ignore-dependencies")
        })
    }

    struct FailShowGit {
        inner: InMemoryGit,
        fail_substr: &'static str,
    }

    impl GitStore for FailShowGit {
        fn init_bare(&self, dir: &Path) -> Result<(), Error> {
            self.inner.init_bare(dir)
        }

        fn fetch_depth1(
            &self,
            dir: &Path,
            remote: &str,
            sha: &str,
            ref_name: &str,
        ) -> Result<(), Error> {
            self.inner.fetch_depth1(dir, remote, sha, ref_name)
        }

        fn show(&self, dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, Error> {
            if path.contains(self.fail_substr) {
                return Err(Error::Other(format!("git show failed: {path}")));
            }
            self.inner.show(dir, sha, path)
        }

        fn rev_parse(&self, dir: &Path, rev: &str) -> Result<Option<String>, Error> {
            self.inner.rev_parse(dir, rev)
        }

        fn gc_prune(&self, dir: &Path) -> Result<(), Error> {
            self.inner.gc_prune(dir)
        }
    }

    #[test]
    fn install_uses_cutoff_tap_deps_not_head_graph() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let lib_mid = formula_rb("lib", "1.0.0", "libmid");
        let lib_new = formula_rb("lib", "1.1.0", "libnew");
        let head_mid = formula_rb("headonly", "1.0.0", "hmid");
        let head_new = formula_rb("headonly", "1.1.0", "hnew");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        git.insert_blob("cutoffsha", "Formula/lib/lib.rb", lib_mid);
        git.insert_blob("headsha", "Formula/lib/lib.rb", lib_new);
        git.insert_blob("cutoffsha", "Formula/h/headonly.rb", head_mid);
        git.insert_blob("headsha", "Formula/h/headonly.rb", head_new);

        let mut deps = std::collections::BTreeMap::new();
        deps.insert("fresh".into(), vec!["headonly".into()]);
        deps.insert("fresh".into(), vec!["lib".into()]);
        let brew = MockBrew {
            deps,
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_dep_install(&runs, "lib", "--formula"),
            "cutoff dep lib must be installed from tap: {runs:?}"
        );
        assert!(
            run_is_soaked_install(&runs, "fresh"),
            "target must still install: {runs:?}"
        );
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/headonly"),
            "HEAD-only dep must not enter soak path: {runs:?}"
        );
        assert!(!result.refused);
    }

    #[test]
    fn install_cask_installs_formula_dep() {
        let app_mid = cask_rb("app", "1.0.0");
        let app_new = cask_rb("app", "1.1.0");
        let lib_mid = formula_rb("lib", "1.0.0", "libmid");
        let lib_new = formula_rb("lib", "1.1.0", "libnew");

        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/a/app.rb", app_mid);
        git.insert_blob("caskhead", "Casks/a/app.rb", app_new);
        git.insert_blob("cutoffsha", "Formula/lib/lib.rb", lib_mid);
        git.insert_blob("headsha", "Formula/lib/lib.rb", lib_new);

        let mut deps = std::collections::BTreeMap::new();
        deps.insert("app".into(), vec!["lib".into()]);
        let brew = MockBrew {
            deps,
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["app".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            true,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_dep_install(&runs, "lib", "--formula"),
            "cask must soak-install formula dep: {runs:?}"
        );
        assert!(
            run_is_soaked_install(&runs, "app"),
            "cask target must path-install without --ignore-dependencies: {runs:?}"
        );
        assert!(!result.refused);
    }

    #[test]
    fn install_git_show_error_aborts_before_target() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let lib_mid = formula_rb("lib", "1.0.0", "libmid");
        let lib_new = formula_rb("lib", "1.1.0", "libnew");

        let inner = InMemoryGit::new();
        inner.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        inner.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        inner.insert_blob("cutoffsha", "Formula/lib/lib.rb", lib_mid);
        inner.insert_blob("headsha", "Formula/lib/lib.rb", lib_new);
        let git = FailShowGit {
            inner,
            fail_substr: "Formula/lib/lib.rb",
        };

        let mut deps = std::collections::BTreeMap::new();
        deps.insert("fresh".into(), vec!["lib".into()]);
        deps.insert("fresh".into(), vec!["lib".into()]);
        let brew = MockBrew {
            deps,
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let err = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect_err("git show failure must abort the command");
        assert!(
            matches!(err, Error::Other(ref msg) if msg.contains("git show failed")),
            "{err}"
        );
        let runs = lock_runs(&brew);
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/fresh"),
            "must not install the target after git failure: {runs:?}"
        );
    }

    fn call_reinstall(
        brew: &MockBrew,
        git: &InMemoryGit,
        snaps: &Snapshots,
        tap: &Path,
        names: &[String],
        out: &mut Vec<u8>,
    ) -> Result<RunResult, Error> {
        reinstall(brew, git, snaps, unused_cache(), tap, names, &[], out)
    }

    #[test]
    fn reinstall_true_repair() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid);
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new.clone());

        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_new)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["wget".to_string()];
        let result =
            call_reinstall(&brew, &git, &snaps, tap.path(), &names, &mut out).expect("reinstall");
        let runs = lock_runs(&brew);
        assert!(
            runs.iter()
                .any(|args| args == &["reinstall".to_string(), "wget".to_string()]),
            "true repair must brew reinstall wget: {runs:?}"
        );
        assert!(
            !runs
                .iter()
                .any(|args| args.iter().any(|a| a.contains("brewsoakr/soaked"))),
            "true repair must not use soaked tap: {runs:?}"
        );
        assert!(!result.refused, "true repair is not a refusal");
    }

    #[test]
    fn reinstall_already_soaked() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid.clone());
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);

        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_mid)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["wget".to_string()];
        let result =
            call_reinstall(&brew, &git, &snaps, tap.path(), &names, &mut out).expect("reinstall");
        let runs = lock_runs(&brew);
        assert!(
            runs.is_empty(),
            "already soaked must not brew reinstall or install: {runs:?}"
        );
        assert!(!result.refused, "already soaked is not a refusal");
    }

    #[test]
    fn reinstall_behind() {
        let wget_old = formula_rb("wget", "1.0.0", "oldsha");
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid);
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);

        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_old)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["wget".to_string()];
        let result =
            call_reinstall(&brew, &git, &snaps, tap.path(), &names, &mut out).expect("reinstall");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "wget"),
            "behind reinstall must path-install cutoff without --ignore-dependencies: {runs:?}"
        );
        assert!(
            !runs
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("reinstall")),
            "behind reinstall must not brew reinstall HEAD: {runs:?}"
        );
        assert!(!result.refused, "eligible behind reinstall must not refuse");
    }

    #[test]
    fn reinstall_missing() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid);
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);

        let brew = MockBrew::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["wget".to_string()];
        let err = call_reinstall(&brew, &git, &snaps, tap.path(), &names, &mut out)
            .expect_err("missing reinstall must refuse");
        let runs = lock_runs(&brew);
        assert!(
            runs.is_empty(),
            "missing reinstall must not run brew: {runs:?}"
        );
        match err {
            Error::Refusal(msg) => {
                assert!(
                    msg.contains("no installed keg"),
                    "missing reinstall refusal: {msg}"
                );
            }
            other => panic!("expected Error::Refusal, got {other:?}"),
        }
    }

    #[test]
    fn outdated_skips_unparseable_identity_and_continues() {
        let alpha_old = formula_rb("alpha", "1.0.0", "oldsha");
        let alpha_mid = formula_rb("alpha", "1.1.0", "midsha");
        let alpha_new = formula_rb("alpha", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", alpha_mid);
        git.insert_blob("headsha", "Formula/a/alpha.rb", alpha_new);
        git.insert_blob("cutoffsha", "Formula/b/bad.rb", "class Bad; end\n");
        git.insert_blob("headsha", "Formula/b/bad.rb", "class Bad; end\n");

        let brew = MockBrew {
            installed: vec![
                formula_pkg("bad", "class Bad; end\n".into()),
                formula_pkg("alpha", alpha_old),
            ],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let mut out = Vec::new();
        let result =
            outdated(&brew, &git, &snaps, unused_cache(), &[], &mut out).expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "good pkg must still list: {text}");
        assert!(
            text.contains("bad") && text.contains("unparseable"),
            "unparseable pkg must be held, not abort: {text}"
        );
        assert!(!result.refused);
    }

    #[test]
    fn upgrade_nameless_skips_unparseable_and_continues() {
        let ok_old = formula_rb("ok", "1.0.0", "oldsha");
        let ok_mid = formula_rb("ok", "1.1.0", "midsha");
        let ok_new = formula_rb("ok", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/o/ok.rb", ok_mid);
        git.insert_blob("headsha", "Formula/o/ok.rb", ok_new);
        git.insert_blob("cutoffsha", "Formula/b/bad.rb", "class Bad; end\n");
        git.insert_blob("headsha", "Formula/b/bad.rb", "class Bad; end\n");

        let brew = MockBrew {
            installed: vec![
                formula_pkg("bad", "class Bad; end\n".into()),
                formula_pkg("ok", ok_old),
            ],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "ok"),
            "parse failure must not abort remaining upgrades: {runs:?}"
        );
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/bad"),
            "unparseable must not install: {runs:?}"
        );
        assert!(!result.refused, "unparseable skip is not a soak refusal");
    }

    #[test]
    fn upgrade_nameless_skips_pinned_formula() {
        let pin_old = formula_rb("pin", "1.0.0", "oldsha");
        let pin_mid = formula_rb("pin", "1.1.0", "midsha");
        let pin_new = formula_rb("pin", "1.2.0", "newsha");
        let ok_old = formula_rb("ok", "1.0.0", "oldsha");
        let ok_mid = formula_rb("ok", "1.1.0", "midsha");
        let ok_new = formula_rb("ok", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/p/pin.rb", pin_mid);
        git.insert_blob("headsha", "Formula/p/pin.rb", pin_new);
        git.insert_blob("cutoffsha", "Formula/o/ok.rb", ok_mid);
        git.insert_blob("headsha", "Formula/o/ok.rb", ok_new);

        let brew = MockBrew {
            installed: vec![
                formula_pkg_pinned("pin", pin_old),
                formula_pkg("ok", ok_old),
            ],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "ok"),
            "unpinned must still upgrade: {runs:?}"
        );
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/pin"),
            "pinned must be skipped: {runs:?}"
        );
        assert!(!result.refused);
    }

    #[test]
    fn outdated_skips_pinned_formula() {
        let pin_old = formula_rb("pin", "1.0.0", "oldsha");
        let pin_mid = formula_rb("pin", "1.1.0", "midsha");
        let pin_new = formula_rb("pin", "1.2.0", "newsha");
        let ok_old = formula_rb("ok", "1.0.0", "oldsha");
        let ok_mid = formula_rb("ok", "1.1.0", "midsha");
        let ok_new = formula_rb("ok", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/p/pin.rb", pin_mid);
        git.insert_blob("headsha", "Formula/p/pin.rb", pin_new);
        git.insert_blob("cutoffsha", "Formula/o/ok.rb", ok_mid);
        git.insert_blob("headsha", "Formula/o/ok.rb", ok_new);

        let brew = MockBrew {
            installed: vec![
                formula_pkg_pinned("pin", pin_old),
                formula_pkg("ok", ok_old),
            ],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let mut out = Vec::new();
        outdated(&brew, &git, &snaps, unused_cache(), &[], &mut out).expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("ok"), "unpinned must list: {text}");
        assert!(
            text.contains("==> Pinned"),
            "missing pinned section: {text}"
        );
        let outdated_block = text.split("==> Pinned").next().unwrap_or(&text);
        assert!(
            !outdated_block
                .split("==> Held")
                .next()
                .unwrap_or("")
                .contains("pin"),
            "pinned must not list as outdated: {text}"
        );
    }

    #[test]
    fn info_mixed_third_party_passthrough() {
        let alpha_old = formula_rb("alpha", "1.0.0", "oldsha");
        let alpha_mid = formula_rb("alpha", "1.1.0", "midsha");
        let alpha_new = formula_rb("alpha", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", alpha_mid);
        git.insert_blob("headsha", "Formula/a/alpha.rb", alpha_new);
        let brew = MockBrew {
            installed: vec![formula_pkg("alpha", alpha_old)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let mut out = Vec::new();
        let names = ["alpha".to_string(), "acme/tools/foo".to_string()];
        info(&brew, &git, &snaps, unused_cache(), &names, &[], &mut out).expect("info");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "core name still soaked: {text}");
        let visible = brew.visible_runs.lock().expect("visible");
        assert!(
            visible
                .iter()
                .any(|args| args == &["info".to_string(), "acme/tools/foo".into()]),
            "third-party must brew.run_visible info: {visible:?}"
        );
    }

    #[test]
    fn outdated_mixed_third_party_passthrough() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &["acme/tools/foo".to_string()],
            &mut out,
        )
        .expect("outdated");
        let visible = brew.visible_runs.lock().expect("visible");
        assert!(
            visible
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("outdated")
                    && args.iter().any(|a| a == "acme/tools/foo")),
            "third-party must brew.run_visible outdated: {visible:?}"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "soaked listing still runs: {text}");
    }

    #[test]
    fn install_uses_file_path_not_tap_token() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        let brew = MockBrew::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            runs.iter().any(|args| {
                args.first().map(String::as_str) == Some("install")
                    && args.iter().any(|a| a.ends_with("/fresh.rb"))
                    && !args.iter().any(|a| a.contains("brewsoakr/soaked"))
            }),
            "install must use a file path, not a tap token: {runs:?}"
        );
    }

    #[test]
    fn install_uses_run_visible_for_soaked_install() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        let brew = MockBrew::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let visible = brew.visible_runs.lock().expect("visible");
        assert!(
            visible
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("install")
                    && args.iter().any(|a| a.ends_with("/fresh.rb"))),
            "soaked install must use run_visible: {visible:?}"
        );
    }

    #[test]
    fn install_already_installed_nonzero_is_success() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        let brew = MockBrew {
            next_status: 1,
            next_stderr: b"Error: fresh 1.1.0 is already installed\n".to_vec(),
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        assert!(
            !result.refused,
            "already-installed must not be a soak refusal"
        );
        assert_eq!(
            result.brew_status,
            Some(0),
            "non-zero already-installed must be treated as success"
        );
    }
}
