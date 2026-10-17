use super::bare::{BareSkip, bare_action, bare_skip, latest_left, latest_note};
use super::deps::{CutoffDepWalk, CutoffDeps, HeldDep};
use super::notes::{
    ahead_message, held_tap_note, newer_than_cutoff_note, refusal_message, unsoakable_note,
    write_session_tail,
};
use super::view::{
    Resolved, ResolvedView, calendar_today, host_for_pkg, natural_kind, parse_formula_detected,
    parse_pkgs, resolve_pkg_blobs, resolve_token, resolve_view, settle_origin, validate_tokens,
    view_versions,
};
use super::{RunResult, is_verbose};
use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::eligibility::{self, DesiredAction, UpstreamStatus};
use crate::flags;
use crate::git::GitStore;
use crate::identity::PkgIdentity;
use crate::inventory::{Inventory, Pkg, PkgClass};
use crate::nosoak;
use crate::origin::{self, OriginRecords};
use crate::quiet;
use crate::report::{self, Counts};
use crate::resolve::{PkgKind, PkgRef};
use crate::snapshot::Snapshots;
use crate::tap;
use crate::taps;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

#[allow(clippy::too_many_arguments)]
pub fn reinstall(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    inv: &Inventory,
    cfg: &Config,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    if names.is_empty() {
        return Err(Error::Usage("reinstall: no packages specified".into()));
    }
    validate_tokens(names)?;
    let plan_total = None;
    let mut session = ApplySession {
        brew,
        git,
        snaps,
        cache,
        tap_root,
        user_flags,
        inv,
        cfg,
        brew_verb: "reinstall",
        bare_run: false,
        force_cask: false,
        force_formula: false,
        refused: false,
        brew_status: None,
        counts: Counts {
            dry_run: flags::is_dry_run(user_flags),
            ..Counts::default()
        },
        done: BTreeMap::new(),
        counted_deps: HashSet::new(),
        deferred: Vec::new(),
        nosoak: Vec::new(),
        origins: OriginRecords::load(cache),
        plan_total,
        plan_index: 0,
        out,
    };
    if is_verbose(user_flags) {
        writeln!(
            session.out,
            "{}",
            report::soak_banner("reinstalling", snaps.hours.get(), &snaps.core, &snaps.cask)
        )?;
    }
    for raw in names {
        let r = resolve_token(raw, inv, cfg)?;
        if session.refuses_tap_switch(&r) {
            continue;
        }
        match r.class {
            PkgClass::NoSoak => {
                session.nosoak.push(nosoak::Target {
                    origin: r.origin,
                    name: r.name,
                    switch_tap: false,
                    kind: None,
                });
                continue;
            }
            PkgClass::Unsoakable => {
                // apply_one produces the refusal.
                session.apply_one(raw)?;
                continue;
            }
            PkgClass::Soaked => {}
        }
        // A held tap keeps its last good table, but that is never a basis
        // for running brew against the tap's current HEAD: hold it as
        // `apply_one` does (it defers the held-tap note and refuses).
        if snaps.held_taps.contains_key(&r.origin) {
            session.apply_one(raw)?;
            continue;
        }
        let Some(pkg) = inv.find_in(&r.origin, &r.name) else {
            return Err(Error::Refusal(format!(
                "reinstall: no installed keg: {}",
                r.name
            )));
        };
        let Some(view) = resolve_view(
            brew,
            git,
            snaps,
            cache,
            &r.origin,
            &r.name,
            pkg.kind,
            Some(&pkg.receipt_rb),
        )?
        else {
            writeln!(session.out, "{}: unparseable identity; skipping", r.name)?;
            continue;
        };
        if eligibility::identities_match(view.installed.as_ref(), view.head.as_ref()) {
            if is_verbose(user_flags) {
                writeln!(
                    session.out,
                    "{}: installed matches HEAD; brew reinstall (true repair)",
                    r.name
                )?;
            }
            // Full token, so a tap formula resolves in its own tap.
            let token = nosoak::brew_token(&r.origin, &r.name);
            writeln!(session.out, "reinstalling {}", r.name)?;
            let mut args = vec!["reinstall".to_string()];
            args.extend(crate::flags::filter_for_verb("reinstall", user_flags).kept);
            session.note_dropped_flags("reinstall", user_flags, "repair reinstalls");
            args.push(token);
            let (ok, installed_now) = session.record_run(&args)?;
            if ok {
                session.counts.upgraded += 1;
            }
            // Brew reinstalled from its own tap, so the keg's receipt now
            // names the tap and any staged-install record is stale.
            if installed_now {
                session.forget_origin(pkg.kind, &r.name)?;
            }
            continue;
        }
        session.apply_one(raw)?;
    }
    session.run_nosoak()?;
    session.write_tail()?;
    Ok(RunResult {
        refused: session.refused,
        brew_status: session.brew_status,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_many(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    inv: &Inventory,
    cfg: &Config,
    names: &[String],
    bare_run: bool,
    brew_verb: &str,
    force_cask: bool,
    force_formula: bool,
    user_flags: &[String],
    plan_total: Option<usize>,
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let mut session = ApplySession {
        brew,
        git,
        snaps,
        cache,
        tap_root,
        user_flags,
        inv,
        cfg,
        brew_verb,
        bare_run,
        force_cask,
        force_formula,
        refused: false,
        brew_status: None,
        counts: Counts {
            dry_run: flags::is_dry_run(user_flags),
            ..Counts::default()
        },
        done: BTreeMap::new(),
        counted_deps: HashSet::new(),
        deferred: Vec::new(),
        nosoak: Vec::new(),
        origins: OriginRecords::load(cache),
        plan_total,
        plan_index: 0,
        out,
    };
    validate_tokens(names)?;
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
    if bare_run {
        // Walk the installed packages themselves: a formula and a cask can
        // share a name, and a name alone finds only one of them.
        for pkg in &inv.pkgs {
            session.apply_pkg(pkg)?;
        }
    } else {
        for name in names {
            session.apply_one(name)?;
        }
    }
    session.run_nosoak()?;
    session.write_tail()?;
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

pub(super) struct ApplySession<'a, B, G, W> {
    brew: &'a B,
    git: &'a G,
    snaps: &'a Snapshots,
    cache: &'a Path,
    tap_root: &'a Path,
    user_flags: &'a [String],
    inv: &'a Inventory,
    cfg: &'a Config,
    brew_verb: &'a str,
    /// A bare `upgrade`: every installed package, none named by the user.
    bare_run: bool,
    force_cask: bool,
    force_formula: bool,
    refused: bool,
    brew_status: Option<i32>,
    counts: Counts,
    /// Cellar versions brew has reported installing so far this session,
    /// including transitive dependencies nobody asked for by name. The
    /// installed snapshot is taken once and goes stale as soon as brew
    /// upgrades a dependency for us.
    done: BTreeMap<String, String>,
    /// Installed dependencies a named run upgraded and counted ahead of their
    /// own plan entry, so that entry does not count them a second time.
    counted_deps: HashSet<String>,
    /// Holds, skips, and warnings, printed as one block at the end so they
    /// are not lost in the middle of the install scroll.
    deferred: Vec<String>,
    /// No-soak packages collected for the single brew step after soaked work.
    nosoak: Vec<nosoak::Target>,
    /// Where each tap-staged keg came from; saved after every real install.
    origins: OriginRecords,
    /// Number of packages this run expects to change, when known up front.
    plan_total: Option<usize>,
    plan_index: usize,
    out: &'a mut W,
}

impl<B: Brew, G: GitStore, W: Write> ApplySession<'_, B, G, W> {
    fn defer(&mut self, msg: String) {
        self.deferred.push(msg);
    }

    /// Say once which of `flags` `brew <verb>` would reject and we dropped.
    fn note_dropped_flags(&mut self, verb: &str, flags: &[String], context: &str) {
        let dropped = crate::flags::filter_for_verb(verb, flags).dropped;
        if let Some(note) = crate::flags::dropped_note(verb, &dropped, context)
            && !self.deferred.contains(&note)
        {
            self.defer(note);
        }
    }

    /// What a staged `brew install` may see. On an upgrade the greedy flags
    /// are brewsoak's (read by `bare_action`), so they are neither forwarded
    /// nor reported as dropped; `-g` would otherwise reach brew install as
    /// `--git`. The install table filter still applies in `brew_install_args`.
    fn staged_install_flags(&self) -> Vec<String> {
        if self.brew_verb == "upgrade" {
            crate::flags::without_greedy(self.user_flags)
        } else {
            self.user_flags.to_vec()
        }
    }

    /// `[3/26] upgrading node 26.7.0 -> 26.8.1`, so each package announces
    /// itself once instead of leaving brew's output to speak for it.
    fn announce(&mut self, name: &str, view: &ResolvedView) -> Result<(), Error> {
        self.plan_index += 1;
        let counter = match self.plan_total {
            Some(total) => format!("[{}/{}] ", self.plan_index, total),
            None => String::new(),
        };
        let to = view
            .cutoff
            .as_ref()
            .map(report::identity_version)
            .unwrap_or_else(|| "?".into());
        let doing = match self.brew_verb {
            "install" => "installing",
            "reinstall" => "reinstalling",
            _ => "upgrading",
        };
        match view.installed.as_ref().map(report::identity_version) {
            Some(from) if from != to => {
                writeln!(self.out, "{counter}{doing} {name} {from} -> {to}")?
            }
            _ => writeln!(self.out, "{counter}{doing} {name} {to}")?,
        }
        Ok(())
    }

    /// The package was planned (it is in the `[i/N]` total) but brew already
    /// upgraded it as a dependency, so it still takes its numbered line.
    fn announce_already_done(&mut self, name: &str, view: &ResolvedView) -> Result<(), Error> {
        self.plan_index += 1;
        let Some(total) = self.plan_total else {
            return Ok(());
        };
        let to = view
            .cutoff
            .as_ref()
            .map(report::identity_version)
            .unwrap_or_else(|| "?".into());
        writeln!(
            self.out,
            "[{}/{total}] {name} {to}: already upgraded as a dependency",
            self.plan_index
        )?;
        Ok(())
    }

    /// Everything worth saying once the packages are done: the counts, the
    /// byte delta, held/skipped packages, caveats, and where the raw brew log
    /// went for anyone who wants the detail we filtered out.
    fn write_tail(&mut self) -> Result<(), Error> {
        writeln!(self.out, "{}", report::counts_line(&self.counts))?;
        let report = self.brew.session_report();
        if report.added_bytes > 0 || report.freed_bytes > 0 {
            writeln!(
                self.out,
                "installed {}, freed {}",
                quiet::human_size(report.added_bytes),
                quiet::human_size(report.freed_bytes)
            )?;
        }
        if !self.deferred.is_empty() {
            writeln!(self.out, "notes:")?;
            for note in &self.deferred {
                for (i, line) in note.lines().enumerate() {
                    if i == 0 {
                        writeln!(self.out, "  {line}")?;
                    } else {
                        writeln!(self.out, "    {}", line.trim_start())?;
                    }
                }
            }
        }
        write_session_tail(&report, self.out)
    }

    /// True when brew already put this package at the artifact we wanted, as
    /// a dependency of something installed earlier in this session. The keg
    /// directory name is no evidence: brew's version heuristics differ from
    /// ours (`go1.27.1` is keg `1.27.1`), and brew may have resolved a dep at
    /// a version other than the cutoff. So the keg's own formula is read and
    /// compared by artifact; an unreadable keg is not done. Casks never are.
    fn already_done(&self, name: &str, kind: PkgKind, want: Option<&PkgIdentity>) -> bool {
        if kind == PkgKind::Cask {
            return false;
        }
        let (Some(version), Some(want)) = (self.done.get(name), want) else {
            return false;
        };
        let Some(rb) = self.brew.keg_receipt(name, version) else {
            return false;
        };
        match parse_formula_detected(self.brew, &rb, self.brew.host_platform().as_ref()) {
            Ok(have) => PkgIdentity::Formula(have).same_artifact(want),
            Err(_) => false,
        }
    }
}

impl<B: Brew, G: GitStore, W: Write> ApplySession<'_, B, G, W> {
    /// An explicit `user/tap/name` over a keg from another origin is refused:
    /// brewsoak never uninstalls a keg to switch taps. Returns true when it
    /// refused.
    fn refuses_tap_switch(&mut self, r: &Resolved) -> bool {
        if !r.named_origin || self.inv.find_in(&r.origin, &r.name).is_some() {
            return false;
        }
        let Some(other) = self.inv.find(&r.name).map(|p| p.origin.clone()) else {
            return false;
        };
        let token = nosoak::brew_token(&r.origin, &r.name);
        self.defer(format!(
            "{} is installed from {other}; brewsoak does not switch taps; use brew {} {token}",
            r.name, self.brew_verb
        ));
        self.counts.held += 1;
        self.refused = true;
        true
    }

    fn nosoak_target(&self, r: Resolved, installed: Option<&Pkg>) -> nosoak::Target {
        nosoak::Target {
            switch_tap: r.receipt_tap.is_none()
                && !origin::is_core_or_cask(&r.origin)
                && installed.is_some(),
            kind: installed.map(|p| p.kind),
            origin: r.origin,
            name: r.name,
        }
    }

    fn apply_one(&mut self, raw: &str) -> Result<(), Error> {
        let r = resolve_token(raw, self.inv, self.cfg)?;
        if self.refuses_tap_switch(&r) {
            return Ok(());
        }
        self.apply_resolved(r, None)
    }

    /// One installed package from a bare run, with its own origin and kind.
    fn apply_pkg(&mut self, pkg: &Pkg) -> Result<(), Error> {
        let r = Resolved {
            origin: pkg.origin.clone(),
            name: pkg.name.clone(),
            class: pkg.class,
            receipt_tap: pkg.receipt_tap.clone(),
            tapped: origin::is_core_or_cask(&pkg.origin)
                || self.inv.tap_class(&pkg.origin).is_some(),
            named_origin: false,
        };
        self.apply_resolved(r, Some(pkg))
    }

    /// `known` is the installed package a bare run is walking; an explicit
    /// token looks its package up by origin and name instead.
    fn apply_resolved(&mut self, mut r: Resolved, known: Option<&Pkg>) -> Result<(), Error> {
        let name = r.name.clone();
        let inv = self.inv;
        let installed = known.or_else(|| inv.find_in(&r.origin, &name));
        let snaps = self.snaps;
        let skip = known.and_then(|p| bare_skip(p, snaps));
        let pinned = match known {
            Some(_) => skip == Some(BareSkip::Pinned),
            None => installed.is_some_and(|p| p.pinned),
        };
        if pinned && self.brew_verb == "upgrade" && (self.bare_run || r.class != PkgClass::NoSoak) {
            self.counts.pinned += 1;
            if is_verbose(self.user_flags) {
                writeln!(self.out, "{name}: pinned; skipped")?;
            }
            return Ok(());
        }
        if is_verbose(self.user_flags) {
            writeln!(
                self.out,
                "{}",
                report::origin_line(
                    &name,
                    &r.origin,
                    self.cfg.effective_hours(&r.origin),
                    r.class
                )
            )?;
        }
        match r.class {
            PkgClass::NoSoak => {
                let target = self.nosoak_target(r, installed);
                self.nosoak.push(target);
                return Ok(());
            }
            PkgClass::Unsoakable => {
                let msg = unsoakable_note(&name, &r.origin, r.tapped);
                if self.bare_run {
                    self.defer(msg);
                } else {
                    let token = nosoak::brew_token(&r.origin, &name);
                    self.defer(format!(
                        "{msg}; use `brew {} {token}` to bypass brewsoak.",
                        self.brew_verb
                    ));
                    self.counts.held += 1;
                    self.refused = true;
                }
                return Ok(());
            }
            PkgClass::Soaked => {}
        }
        let held_err = match (known, skip) {
            (Some(_), Some(BareSkip::HeldTap(err))) => Some(err),
            (Some(_), _) => None,
            (None, _) => snaps.held_taps.get(&r.origin).map(String::as_str),
        };
        if let Some(err) = held_err {
            let note = held_tap_note(&name, &r.origin, err);
            self.defer(note);
            self.counts.held += 1;
            self.refused = true;
            return Ok(());
        }
        let kind = match known {
            Some(pkg) => pkg.kind,
            None => self.resolve_kind(&r.origin, &name)?,
        };
        settle_origin(&mut r, kind, self.inv, self.cfg);
        if r.class == PkgClass::NoSoak {
            // A fresh cask install under `NO_SOAK = ["homebrew/cask"]`.
            let target = self.nosoak_target(r, installed);
            self.nosoak.push(target);
            return Ok(());
        }
        let receipt = installed.map(|p| p.receipt_rb.as_str());
        let Some(mut view) = resolve_view(
            self.brew, self.git, self.snaps, self.cache, &r.origin, &name, kind, receipt,
        )?
        else {
            self.counts.skipped += 1;
            self.defer(format!("{name}: unparseable identity; skipping"));
            return Ok(());
        };
        if view.action == DesiredAction::RefuseYanked
            && view.cutoff.is_none()
            && origin::is_core_or_cask(&r.origin)
            && installed.is_some_and(|p| p.receipt_tap.is_none())
        {
            // Staged from a tap, the origin record is gone, and the name
            // resolves nowhere in core or cask.
            self.defer(format!(
                "{name}: origin unknown; reinstall it from its tap with brew"
            ));
            self.counts.held += 1;
            self.refused = true;
            return Ok(());
        }
        for warn in view.warnings.clone() {
            self.defer(format!("warning: {warn}"));
        }
        let greedy = flags::greedy_mode(self.user_flags);
        if self.bare_run {
            view.action = bare_action(&view, greedy);
        }
        let leaves_latest = self.bare_run && latest_left(&view, greedy);
        let did = match view.action {
            DesiredAction::InstallCutoff => "installing cutoff",
            DesiredAction::NoOpAlreadySoaked if leaves_latest => {
                "left unchanged; its contents cannot be soaked"
            }
            DesiredAction::NoOpAlreadySoaked => "left unchanged",
            DesiredAction::LeaveAheadOfSoak | DesiredAction::LeaveNewerThanCutoff => {
                "left unchanged"
            }
            DesiredAction::LeaveAutoUpdates => "left to the app",
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated
            | DesiredAction::HoldUnordered => "refused",
        };
        if is_verbose(self.user_flags) {
            writeln!(
                self.out,
                "{}",
                report::evaluate_line(
                    &name,
                    view.action,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.head.as_ref(),
                    did,
                )
            )?;
        }
        self.counts.note(view.action);
        let token = nosoak::brew_token(&r.origin, &name);
        match view.action {
            DesiredAction::NoOpAlreadySoaked => {
                if self.brew_verb == "install" {
                    writeln!(self.out, "{name} is already installed")?;
                }
                if leaves_latest {
                    // brew would reinstall it under --greedy; say why we did not.
                    self.defer(latest_note(&name));
                }
            }
            // Silent, as brew is: the app updates itself. `-v` shows the line.
            DesiredAction::LeaveAutoUpdates => {}
            DesiredAction::LeaveAheadOfSoak => {
                if self.brew_verb == "reinstall" {
                    self.defer(format!(
                        "{name} is ahead of soak; reinstall would pull a too-new artifact; use `brew reinstall {token}` to bypass brewsoak."
                    ));
                    self.refused = true;
                } else {
                    self.defer(ahead_message(&name));
                }
            }
            DesiredAction::LeaveNewerThanCutoff => {
                let (installed_ver, cutoff_ver) = view_versions(&view);
                if self.brew_verb == "reinstall" {
                    self.defer(format!(
                        "{name} is newer than its cutoff {cutoff_ver}; reinstall would downgrade it; use `brew reinstall {token}` to bypass brewsoak."
                    ));
                    self.refused = true;
                } else {
                    self.defer(newer_than_cutoff_note(&name, &installed_ver, &cutoff_ver));
                }
            }
            DesiredAction::HoldUnordered => {
                let (installed_ver, cutoff_ver) = view_versions(&view);
                self.defer(format!(
                    "{name}: brew could not compare installed {installed_ver} with cutoff {cutoff_ver}; holding it"
                ));
                self.refused = true;
            }
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => {
                if let Some(msg) = refusal_message(view.action, &token, self.brew_verb) {
                    self.defer(msg);
                }
                self.refused = true;
            }
            DesiredAction::InstallCutoff => {
                // brew may already have upgraded this as a dependency of an
                // earlier package; running it again just prints a warning.
                if self.already_done(&name, kind, view.cutoff.as_ref()) {
                    if self.counted_deps.remove(&name) {
                        self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
                    }
                    self.announce_already_done(&name, &view)?;
                    return Ok(());
                }
                self.announce(&name, &view)?;
                self.install_cutoff(&name, &r.origin, kind, &view)?;
            }
        }
        Ok(())
    }

    fn install_cutoff(
        &mut self,
        name: &str,
        origin_tap: &str,
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
        let path = tap::write_blob(&staging_dir(self.tap_root, origin_tap), &pkg, blob)?;

        let token = nosoak::brew_token(origin_tap, name);
        let walked = match self.collect_cutoff_deps(origin_tap, name, kind) {
            Ok(w) => w,
            // A bare dependency name matches formulae in several taps: brew
            // will not pick one, and neither will brewsoak.
            Err(Error::Brew { message, .. }) if taps::ambiguous_formula(&message) => {
                self.refuse_ambiguous_dep(name, &token, &message)?;
                return Ok(());
            }
            // brew could not even read the staged tap file's dependencies.
            Err(Error::Brew { message, .. }) if !origin::is_core_or_cask(origin_tap) => {
                match taps::untrusted_tap(&message) {
                    Some(tap) => self.hold_target_untrusted(name, &tap),
                    None => self.hold_target_staged_copy(name),
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if let Some(held) = walked.held {
            return self.refuse_held_dep(name, &token, &held);
        }
        for (dep_origin, dep, dep_kind) in walked.deps {
            if !self.install_missing_dep(name, &token, &dep_origin, dep_kind, &dep)? {
                return Ok(());
            }
        }

        let install_flags = self.staged_install_flags();
        let args = tap::brew_install_args(&pkg, &path, &install_flags);
        self.note_dropped_flags("install", &install_flags, "staged installs");
        match self.record_staged_install(origin_tap, name, kind, &args)? {
            StagedRun::Ran { failed } => {
                // Counted as upgraded when it was announced; brew failed.
                if failed {
                    self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
                }
            }
            StagedRun::LoadFailure => self.hold_target_staged_copy(name),
            StagedRun::Untrusted(tap) => self.hold_target_untrusted(name, &tap),
        }
        Ok(())
    }

    fn collect_cutoff_deps(
        &self,
        root_origin: &str,
        root: &str,
        root_kind: PkgKind,
    ) -> Result<CutoffDeps, Error> {
        let mut walk = CutoffDepWalk {
            brew: self.brew,
            git: self.git,
            snaps: self.snaps,
            cache: self.cache,
            tap_root: self.tap_root,
            inv: self.inv,
            cfg: self.cfg,
            visiting: HashSet::new(),
            visited: HashSet::new(),
            out: Vec::new(),
            held: None,
        };
        walk.visit(root_origin, root, root_kind, false)?;
        Ok(CutoffDeps {
            deps: walk.out,
            held: walk.held,
        })
    }

    /// Install a missing cutoff dep from its own origin. Returns false when
    /// the target was refused.
    fn install_missing_dep(
        &mut self,
        target: &str,
        target_token: &str,
        dep_origin: &str,
        kind: PkgKind,
        dep: &str,
    ) -> Result<bool, Error> {
        let blobs = resolve_pkg_blobs(self.git, self.snaps, self.cache, dep_origin, dep, kind)?;
        let host = host_for_pkg(self.brew, kind, dep, self.brew.host_platform().as_ref());
        let status = eligibility::upstream_status(
            blobs.cutoff.as_deref(),
            blobs.head.as_deref(),
            &calendar_today(),
            host.as_ref(),
        );
        if status != UpstreamStatus::Eligible {
            self.refuse_ineligible_dep(target, target_token, dep, status)?;
            return Ok(false);
        }
        // The wanted cutoff and the installed receipt are parsed together:
        // one brew call for their url-derived versions, all or none.
        let cutoff_rb = blobs
            .cutoff
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok());
        let installed_rb = self
            .inv
            .find_in(dep_origin, dep)
            .map(|p| p.receipt_rb.as_str());
        let sides: Vec<&str> = [cutoff_rb, installed_rb].into_iter().flatten().collect();
        let mut parsed = parse_pkgs(self.brew, kind, &sides, host.as_ref())
            .into_iter()
            .map(Result::ok);
        let want = if cutoff_rb.is_some() {
            parsed.next().flatten()
        } else {
            None
        };
        let installed_id = if installed_rb.is_some() {
            parsed.next().flatten()
        } else {
            None
        };
        if self.already_done(dep, kind, want.as_ref()) {
            return Ok(true);
        }
        let blob = blobs.cutoff.as_deref().ok_or_else(|| {
            Error::Other(format!("{dep} is eligible but the cutoff blob is missing"))
        })?;
        let pkg = PkgRef {
            name: dep.to_string(),
            kind,
        };
        // A bare run reaches an installed dep again at its own plan entry,
        // which counts it; any other run counts it here.
        let counted_here = !self.bare_run && self.inv.find_in(dep_origin, dep).is_some();
        if counted_here {
            let from = installed_id.as_ref().map(report::identity_version);
            let to = want.as_ref().map(report::identity_version);
            let (from, to) = (
                from.unwrap_or_else(|| "?".into()),
                to.unwrap_or_else(|| "?".into()),
            );
            writeln!(
                self.out,
                "upgrading {dep} {from} -> {to} (dependency of {target})"
            )?;
            self.counts.upgraded += 1;
            self.counted_deps.insert(dep.to_string());
        }
        let path = tap::write_blob(&staging_dir(self.tap_root, dep_origin), &pkg, blob)?;
        let install_flags = self.staged_install_flags();
        let args = tap::brew_install_args(&pkg, &path, &install_flags);
        // A dep that brew cannot load is held by name; the target is still
        // attempted and brew reports the missing dependency.
        match self.record_staged_install(dep_origin, dep, kind, &args)? {
            StagedRun::Ran { failed } => {
                if failed && counted_here && self.counted_deps.remove(dep) {
                    self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
                }
            }
            StagedRun::LoadFailure => self.hold_staged_copy(dep),
            // The target cannot install without this dep: hold the target once
            // (one line, one count) rather than the dep and then the target.
            StagedRun::Untrusted(tap) => {
                if counted_here && self.counted_deps.remove(dep) {
                    self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
                }
                self.hold_target_untrusted_dep(target, dep, &tap);
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn refuse_ineligible_dep(
        &mut self,
        target: &str,
        target_token: &str,
        dep: &str,
        status: UpstreamStatus,
    ) -> Result<(), Error> {
        self.hold_target_for_dep();
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
            "cannot install {target}: dependency {why}; use `brew {} {target_token}` to bypass brewsoak.",
            self.brew_verb
        )?;
        Ok(())
    }

    /// A dependency lives in a tap whose history could not be refreshed this
    /// run, so its cutoff is unknown: refuse the target.
    fn refuse_held_dep(
        &mut self,
        target: &str,
        target_token: &str,
        held: &HeldDep,
    ) -> Result<(), Error> {
        self.hold_target_for_dep();
        writeln!(
            self.out,
            "cannot install {target}: dependency {}: {}\nuse `brew {} {target_token}` to bypass brewsoak.",
            held.dep, held.reason, self.brew_verb
        )?;
        Ok(())
    }

    /// brew cannot resolve one of the target's dependency names because it
    /// matches formulae in more than one tap.
    fn refuse_ambiguous_dep(
        &mut self,
        target: &str,
        target_token: &str,
        message: &str,
    ) -> Result<(), Error> {
        self.hold_target_for_dep();
        writeln!(
            self.out,
            "cannot install {target}: a dependency name is ambiguous;\n  {}\nuse `brew {} {target_token}` to bypass brewsoak.",
            message.trim().replace('\n', "\n  "),
            self.brew_verb
        )?;
        Ok(())
    }

    /// The target was counted as upgraded when it was announced; a dependency
    /// refusal holds it instead, so take that count back.
    fn hold_target_for_dep(&mut self) {
        self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
        self.counts.held += 1;
        self.refused = true;
    }

    /// brew could not load a tap package from its staged copy.
    fn hold_staged_copy(&mut self, name: &str) {
        self.defer(format!(
            "{name}: cannot be installed from a staged copy; use brew, or add it to NO_SOAK"
        ));
        self.counts.held += 1;
        self.refused = true;
    }

    /// The target was counted as upgraded when it was announced; it is held
    /// instead, so take that count back.
    fn hold_target_staged_copy(&mut self, name: &str) {
        self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
        self.hold_staged_copy(name);
    }

    /// brew refuses to load a package from a tap the user has not trusted.
    /// Trusting a third-party tap is the user's decision, never brewsoak's.
    fn hold_untrusted(&mut self, name: &str, tap: &str) {
        self.defer(format!(
            "{name}: brew does not trust tap {tap}; run brew trust {tap}, or add it to NO_SOAK"
        ));
        self.counts.held += 1;
        self.refused = true;
    }

    /// A dependency lives in a tap brew does not trust, so the target cannot
    /// install: one hold line naming the dependency and the tap, and the
    /// target counted once as held.
    fn hold_target_untrusted_dep(&mut self, target: &str, dep: &str, tap: &str) {
        self.hold_target_for_dep();
        self.defer(format!(
            "{target}: dependency {dep}: brew does not trust tap {tap}; run brew trust {tap}, or add it to NO_SOAK"
        ));
    }

    /// The target was counted as upgraded when it was announced; it is held
    /// instead, so take that count back.
    fn hold_target_untrusted(&mut self, name: &str, tap: &str) {
        self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
        self.hold_untrusted(name, tap);
    }

    /// Tap packages are remembered by origin; core and cask need no record.
    fn record_origin(&mut self, origin_tap: &str, name: &str, kind: PkgKind) -> Result<(), Error> {
        // A dry run installs nothing, so it proves nothing about origin.
        if self.counts.dry_run {
            return Ok(());
        }
        let want = (!origin::is_core_or_cask(origin_tap)).then(|| origin_tap.to_ascii_lowercase());
        if self.origins.get(kind, name) == want.as_deref() {
            return Ok(());
        }
        match want {
            Some(tap) => self.origins.set(kind, name, &tap),
            None => self.origins.remove(kind, name),
        }
        self.origins.save(self.cache)
    }

    /// Runs one staged install and records where the keg came from.
    /// A tap package brew could not load or would not trust is reported
    /// without brew's status, so the caller holds it instead.
    fn record_staged_install(
        &mut self,
        origin_tap: &str,
        name: &str,
        kind: PkgKind,
        args: &[String],
    ) -> Result<StagedRun, Error> {
        let output = self.brew.run_visible(args)?;
        let text = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() && !origin::is_core_or_cask(origin_tap) {
            if let Some(tap) = taps::untrusted_tap(&text) {
                return Ok(StagedRun::Untrusted(tap));
            }
            if taps::staged_load_failure(&text) {
                return Ok(StagedRun::LoadFailure);
            }
        }
        self.done
            .extend(quiet::installed_from_output(&output.stdout));
        // Only a real install proves the keg came from this origin; brew's
        // "already installed" answer says nothing about where the keg came from.
        let installed_now = output.status.success() && !already_installed_message(&output);
        let failed = brew_failed(&output);
        merge_status(&mut self.brew_status, output);
        if installed_now {
            self.record_origin(origin_tap, name, kind)?;
        }
        Ok(StagedRun::Ran { failed })
    }

    fn resolve_kind(&self, origin_tap: &str, name: &str) -> Result<PkgKind, Error> {
        if self.force_cask {
            return Ok(PkgKind::Cask);
        }
        if self.force_formula {
            return Ok(PkgKind::Formula);
        }
        natural_kind(self.git, self.snaps, self.cache, self.inv, origin_tap, name)
    }

    /// Spec "No-soak packages": after all soaked work, one `brew update` and
    /// one `brew <verb>` with every no-soak package as a full token.
    fn run_nosoak(&mut self) -> Result<(), Error> {
        let targets = std::mem::take(&mut self.nosoak);
        let step = nosoak::run_step(
            self.brew,
            self.brew_verb,
            self.user_flags,
            &targets,
            self.out,
        )?;
        self.counts.no_soak += step.count;
        self.done.extend(step.installed);
        // The receipt now names the tap, so a staged-keg record is stale.
        for t in step.switched.iter().filter(|_| !self.counts.dry_run) {
            let kind = t.kind.unwrap_or(PkgKind::Formula);
            if self.origins.get(kind, &t.name).is_some() {
                self.origins.remove(kind, &t.name);
                self.origins.save(self.cache)?;
            }
        }
        for note in step.notes {
            self.defer(note);
        }
        if let Some(code) = step.status {
            max_status(&mut self.brew_status, code);
        }
        Ok(())
    }

    /// Runs brew; returns whether it succeeded, and whether it really
    /// installed something (not brew's "already installed" no-op answer).
    fn record_run(&mut self, args: &[String]) -> Result<(bool, bool), Error> {
        let output = self.brew.run_visible(args)?;
        self.done
            .extend(quiet::installed_from_output(&output.stdout));
        let ok = !brew_failed(&output);
        let installed_now = output.status.success() && !already_installed_message(&output);
        merge_status(&mut self.brew_status, output);
        Ok((ok, installed_now))
    }

    /// Drops a package's origin record whatever tap it names. For when brew
    /// itself moved the keg, so the record no longer describes it.
    fn forget_origin(&mut self, kind: PkgKind, name: &str) -> Result<(), Error> {
        // A dry run installs nothing, so it proves nothing about origin.
        if self.counts.dry_run || self.origins.get(kind, name).is_none() {
            return Ok(());
        }
        self.origins.remove(kind, name);
        self.origins.save(self.cache)
    }
}

pub(crate) fn merge_status(slot: &mut Option<i32>, output: std::process::Output) {
    let mut code = output.status.code().unwrap_or(1);
    if code != 0 && already_installed_message(&output) {
        code = 0;
    }
    max_status(slot, code);
}

/// brew exited non-zero and not with its "already installed" no-op answer.
pub(super) fn brew_failed(output: &std::process::Output) -> bool {
    !output.status.success() && !already_installed_message(output)
}

/// What one staged `brew install` came to.
pub(super) enum StagedRun {
    /// brew ran; `failed` when it exited non-zero for a reason of its own.
    Ran { failed: bool },
    /// brew could not load the tap package's staged copy.
    LoadFailure,
    /// brew does not trust this tap (lowercase `user/repo`).
    Untrusted(String),
}

pub(crate) fn max_status(slot: &mut Option<i32>, code: i32) {
    *slot = Some(slot.map_or(code, |prev| prev.max(code)));
}

pub(super) fn already_installed_message(output: &std::process::Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{stdout}\n{stderr}")
        .to_ascii_lowercase()
        .lines()
        .any(already_installed_line)
}

/// brew's no-op answer for one (lowercased) output line. `x 1.0 is already
/// installed but outdated (so it will be upgraded).` precedes a real upgrade,
/// so it is not one.
pub(crate) fn already_installed_line(line: &str) -> bool {
    line.contains("already installed") && !line.contains("already installed but outdated")
}

/// Where `dir`'s staged `.rb` files live for this origin; tap names are
/// lowercase everywhere they are compared or used as paths.
pub(super) fn staging_dir(tap_root: &Path, origin_tap: &str) -> PathBuf {
    taps::staging_root(tap_root, &origin_tap.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::brew::{InstalledPkg, MockBrew};
    use crate::config::Config;

    use crate::git::InMemoryGit;

    use crate::inventory::Inventory;
    use crate::origin::OriginRecords;
    use crate::resolve::PkgKind;

    use std::path::Path;

    use crate::cmd::test_support::{
        call_reinstall, cask_pkg_from, cask_rb, cfg_with, cfg24, core_snaps, formula_pkg,
        formula_pkg_from, formula_pkg_pinned, formula_rb, git_empty, inv_from, lock_runs,
        run_has_token, run_is_soaked_install, tapped, unused_cache, upgrade_names,
        upgrade_names_flags,
    };
    use crate::cmd::upgrade::upgrade;

    fn two_outdated_world(next_stdout: &str) -> (MockBrew, InMemoryGit) {
        two_outdated_world_with(formula_rb, next_stdout)
    }

    /// A formula whose url version is `go<ver>` (keg `<ver>`), as brew's
    /// version heuristics name it; identity.rs cannot reproduce those.
    fn go_style_rb(name: &str, ver: &str, sha: &str) -> String {
        format!(
            "class X < Formula\n  url \"https://example.com/{name}/go{ver}.src.tar.gz\"\n  sha256 \"{sha}\"\nend\n"
        )
    }

    /// `left` and `right` installed at 1.0.0, soak cutoff 1.1.0, HEAD 1.2.0.
    /// The mock Cellar holds a keg receipt for `right` at the cutoff version
    /// (what brew would have written), at HEAD, and at an unrelated 1.0.9.
    fn two_outdated_world_with(
        rb: fn(&str, &str, &str) -> String,
        next_stdout: &str,
    ) -> (MockBrew, InMemoryGit) {
        let git = InMemoryGit::new();
        for name in ["left", "right"] {
            let dir = &name[..1];
            git.insert_blob(
                "cutoffsha",
                &format!("Formula/{dir}/{name}.rb"),
                rb(name, "1.1.0", "midsha"),
            );
            git.insert_blob(
                "headsha",
                &format!("Formula/{dir}/{name}.rb"),
                rb(name, "1.2.0", "newsha"),
            );
        }
        let mut kegs = BTreeMap::new();
        for (ver, rb_ver, sha) in [
            ("1.1.0", "1.1.0", "midsha"),
            ("1.2.0", "1.2.0", "newsha"),
            ("1.0.9", "1.0.9", "othersha"),
        ] {
            kegs.insert(
                ("right".to_string(), ver.to_string()),
                rb("right", rb_ver, sha),
            );
        }
        let brew = MockBrew {
            installed: vec![
                formula_pkg("left", rb("left", "1.0.0", "oldsha")),
                formula_pkg("right", rb("right", "1.0.0", "oldsha")),
            ],
            next_stdout: next_stdout.as_bytes().to_vec(),
            kegs,
            ..MockBrew::new()
        };
        (brew, git)
    }

    fn upgrade_both(brew: &MockBrew, git: &InMemoryGit) -> String {
        upgrade_names(brew, git, &["left".to_string(), "right".to_string()])
    }

    #[test]
    fn package_brew_already_upgraded_as_a_dependency_is_not_installed_again() {
        // brew reports pouring `right` at the cutoff version while installing
        // `left`, so the installed snapshot taken at startup is already stale.
        let (brew, git) =
            two_outdated_world("\u{1f37a}  /opt/homebrew/Cellar/right/1.1.0: 5 files, 1MB\n");
        let text = upgrade_both(&brew, &git);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(
            visible.len(),
            1,
            "right was already at cutoff; expected no second brew run: {visible:?}"
        );
        assert!(text.contains("upgrading left 1.0.0 -> 1.1.0"), "{text}");
        assert!(!text.contains("upgrading right"), "{text}");
    }

    #[test]
    fn go_style_dependency_poured_at_the_cutoff_counts_as_done() {
        // Identity version `go1.1.0`, keg `1.1.0`: compared by the keg's own
        // receipt, not by version strings.
        let (brew, git) = two_outdated_world_with(
            go_style_rb,
            "\u{1f37a}  /opt/homebrew/Cellar/right/1.1.0: 5 files, 1MB\n",
        );
        let text = upgrade_names(&brew, &git, &[]);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 1, "{visible:?}\n{text}");
        assert!(
            text.contains("[2/2] right go1.1.0: already upgraded as a dependency"),
            "{text}"
        );
    }

    #[test]
    fn dependency_poured_at_head_instead_of_the_cutoff_is_still_installed() {
        let (brew, git) = two_outdated_world_with(
            go_style_rb,
            "\u{1f37a}  /opt/homebrew/Cellar/right/1.2.0: 5 files, 1MB\n",
        );
        upgrade_names(&brew, &git, &[]);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 2, "{visible:?}");
    }

    #[test]
    fn unreadable_keg_receipt_is_not_done() {
        let (mut brew, git) =
            two_outdated_world("\u{1f37a}  /opt/homebrew/Cellar/right/1.1.0: 5 files, 1MB\n");
        brew.kegs.clear();
        upgrade_names(&brew, &git, &[]);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 2, "{visible:?}");
    }

    /// `node` depends on `simdutf`; both are installed, `node` sorts first.
    /// Cutoff is 1.1.0 and HEAD 1.2.0 for both. `simdutf_installed` and
    /// `node_installed` are the installed versions.
    fn node_simdutf_world(
        node_installed: &str,
        simdutf_installed: &str,
        next_stdout: &str,
    ) -> (MockBrew, InMemoryGit) {
        let git = InMemoryGit::new();
        for name in ["node", "simdutf"] {
            let dir = &name[..1];
            git.insert_blob(
                "cutoffsha",
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, "1.1.0", "midsha"),
            );
            git.insert_blob(
                "headsha",
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, "1.2.0", "newsha"),
            );
        }
        let mut kegs = BTreeMap::new();
        kegs.insert(
            ("simdutf".to_string(), "1.1.0".to_string()),
            formula_rb("simdutf", "1.1.0", "midsha"),
        );
        let mut deps = BTreeMap::new();
        deps.insert("node".to_string(), vec!["simdutf".to_string()]);
        let brew = MockBrew {
            installed: vec![
                formula_pkg("node", formula_rb("node", node_installed, "oldsha")),
                formula_pkg(
                    "simdutf",
                    formula_rb(
                        "simdutf",
                        simdutf_installed,
                        if simdutf_installed == "1.1.0" {
                            "midsha"
                        } else {
                            "oldsha"
                        },
                    ),
                ),
            ],
            deps,
            kegs,
            next_stdout: next_stdout.as_bytes().to_vec(),
            ..MockBrew::new()
        };
        (brew, git)
    }

    /// Names of the staged packages `brew install` ran, in order.
    fn staged_install_order(brew: &MockBrew) -> Vec<String> {
        brew.visible_runs
            .lock()
            .expect("visible")
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .filter_map(|a| a.last())
            .map(|p| {
                Path::new(p)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(p)
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn outdated_installed_dependency_is_upgraded_before_its_dependent() {
        let (brew, git) = node_simdutf_world(
            "1.0.0",
            "1.0.0",
            "\u{1f37a}  /opt/homebrew/Cellar/simdutf/1.1.0: 5 files, 1MB\n",
        );
        upgrade_names(&brew, &git, &[]);
        assert_eq!(staged_install_order(&brew), ["simdutf", "node"]);
    }

    #[test]
    fn named_upgrade_upgrades_an_outdated_installed_dependency_first() {
        let (brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        let text = upgrade_names(&brew, &git, &["node".to_string()]);
        assert_eq!(staged_install_order(&brew), ["simdutf", "node"]);
        assert!(
            text.contains("upgrading simdutf 1.0.0 -> 1.1.0 (dependency of node)"),
            "{text}"
        );
        assert!(text.contains("upgraded 2,"), "{text}");
    }

    #[test]
    fn named_reinstall_upgrades_an_outdated_installed_dependency_first() {
        let (brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["node".to_string()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(staged_install_order(&brew), ["simdutf", "node"], "{text}");
        assert_eq!(
            text.matches("upgrading simdutf 1.0.0 -> 1.1.0 (dependency of node)")
                .count(),
            1,
            "{text}"
        );
        assert!(text.contains("upgraded 2,"), "simdutf counted once: {text}");
    }

    #[test]
    fn reinstall_true_repair_does_not_walk_dependencies() {
        // node is at HEAD (a plain repair); its dependency is behind.
        let (mut brew, git) = node_simdutf_world("1.2.0", "1.0.0", "");
        brew.installed[0] = formula_pkg("node", formula_rb("node", "1.2.0", "newsha"));
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["node".to_string()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(staged_install_order(&brew).is_empty(), "{text}");
        assert!(!text.contains("dependency of"), "{text}");
        assert!(
            lock_runs(&brew)
                .iter()
                .any(|a| a == &["reinstall".to_string(), "node".to_string()]),
            "{text}"
        );
    }

    /// Cask `app` (sorts first) depends on cask `helper`; both installed at
    /// 1.0.0 behind a 1.1.0 cutoff.
    fn cask_with_cask_dependency_world() -> (MockBrew, InMemoryGit) {
        let git = InMemoryGit::new();
        for name in ["app", "helper"] {
            let dir = &name[..1];
            git.insert_blob(
                "caskcut",
                &format!("Casks/{dir}/{name}.rb"),
                cask_rb(name, "1.1.0"),
            );
            git.insert_blob(
                "caskhead",
                &format!("Casks/{dir}/{name}.rb"),
                cask_rb(name, "1.2.0"),
            );
        }
        let mut deps = BTreeMap::new();
        deps.insert("app".to_string(), vec!["helper".to_string()]);
        let brew = MockBrew {
            installed: ["app", "helper"]
                .map(|n| {
                    cask_pkg_from(
                        n,
                        "homebrew/cask",
                        crate::brew::version_only_cask_receipt(n, "1.0.0"),
                    )
                })
                .to_vec(),
            deps,
            ..MockBrew::new()
        };
        (brew, git)
    }

    #[test]
    fn installed_cask_dependency_is_not_reordered_ahead_of_its_dependent() {
        let (brew, git) = cask_with_cask_dependency_world();
        let text = upgrade_names(&brew, &git, &[]);
        assert!(!text.contains("dependency of"), "{text}");
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "app"), "{text}");
        assert!(
            run_is_soaked_install(&runs, "helper"),
            "helper upgrades at its own plan entry: {text}"
        );
        assert!(text.contains("upgraded 2,"), "{text}");
    }

    #[test]
    fn dependency_named_alongside_its_dependent_is_counted_once() {
        let (brew, git) = node_simdutf_world(
            "1.0.0",
            "1.0.0",
            "\u{1f37a}  /opt/homebrew/Cellar/simdutf/1.1.0: 5 files, 1MB\n",
        );
        let names = ["node".to_string(), "simdutf".to_string()];
        let text = upgrade_names(&brew, &git, &names);
        assert!(text.contains("upgraded 2,"), "{text}");
    }

    #[test]
    fn dry_run_reaches_a_dependency_install() {
        let (brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        let flags = ["--dry-run".to_string()];
        upgrade_names_flags(&brew, &git, &["node".to_string()], &flags);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 2, "{visible:?}");
        for args in &visible {
            assert!(args.iter().any(|a| a == "--dry-run"), "{args:?}");
        }
    }

    #[test]
    fn dependency_upgraded_first_is_counted_once_and_not_run_again() {
        let (brew, git) = node_simdutf_world(
            "1.0.0",
            "1.0.0",
            "\u{1f37a}  /opt/homebrew/Cellar/simdutf/1.1.0: 5 files, 1MB\n",
        );
        let text = upgrade_names(&brew, &git, &[]);
        assert_eq!(staged_install_order(&brew), ["simdutf", "node"], "{text}");
        assert!(
            text.contains("[2/2] simdutf 1.1.0: already upgraded as a dependency"),
            "{text}"
        );
        assert!(text.contains("upgraded 2"), "{text}");
    }

    #[test]
    fn installed_dependency_at_the_cutoff_is_not_reinstalled() {
        let (brew, git) = node_simdutf_world("1.0.0", "1.1.0", "");
        upgrade_names(&brew, &git, &[]);
        assert_eq!(staged_install_order(&brew), ["node"]);
    }

    #[test]
    fn installed_dependency_newer_than_the_cutoff_is_left_alone() {
        let (mut brew, git) = node_simdutf_world("1.0.0", "1.3.0", "");
        brew.orderings.insert(
            ("1.3.0".to_string(), "1.1.0".to_string()),
            std::cmp::Ordering::Greater,
        );
        let text = upgrade_names(&brew, &git, &[]);
        assert_eq!(staged_install_order(&brew), ["node"], "{text}");
    }

    #[test]
    fn pinned_installed_dependency_is_not_upgraded_for_its_dependent() {
        let (mut brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        brew.installed[1].pinned = true;
        upgrade_names(&brew, &git, &[]);
        assert_eq!(staged_install_order(&brew), ["node"]);
    }

    #[test]
    fn staged_install_drops_flags_brew_install_rejects_and_says_so_once() {
        let (brew, git) = two_outdated_world("");
        let flags = ["--greedy", "--ignore-pinned", "--verbose"].map(String::from);
        let text = upgrade_names_flags(&brew, &git, &[], &flags);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 2, "{visible:?}");
        for args in &visible {
            assert!(
                !args
                    .iter()
                    .any(|a| a == "--greedy" || a == "--ignore-pinned"),
                "{args:?}"
            );
            assert!(args.iter().any(|a| a == "--verbose"), "{args:?}");
        }
        // `--greedy` is brewsoak's on an upgrade, so it is not "dropped".
        let note = "brew install does not accept --ignore-pinned; dropped from staged installs";
        assert_eq!(text.matches(note).count(), 1, "{text}");
        assert!(!text.contains("does not accept --greedy"), "{text}");
    }

    #[test]
    fn revision_only_upgrade_announces_both_revisions() {
        let rb = |rev: u32, sha: &str| {
            format!(
                "class X < Formula\n  url \"https://example.com/node-26.10.0.tar.gz\"\n  sha256 \"{sha}\"\n  revision {rev}\nend\n"
            )
        };
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/n/node.rb", rb(2, "midsha"));
        git.insert_blob("headsha", "Formula/n/node.rb", rb(2, "midsha"));
        let brew = MockBrew {
            installed: vec![formula_pkg("node", rb(1, "oldsha"))],
            ..MockBrew::new()
        };
        let text = upgrade_names(&brew, &git, &["node".to_string()]);
        assert!(
            text.contains("upgrading node 26.10.0_1 -> 26.10.0_2"),
            "{text}"
        );
    }

    #[test]
    fn package_at_a_different_version_is_still_installed() {
        // Same shape, but brew left `right` on a version we did not want.
        let (brew, git) =
            two_outdated_world("\u{1f37a}  /opt/homebrew/Cellar/right/1.0.9: 5 files, 1MB\n");
        upgrade_both(&brew, &git);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 2, "{visible:?}");
    }

    #[test]
    fn holds_print_after_the_package_work_not_during() {
        let ok_old = formula_rb("ok", "1.0.0", "oldsha");
        let ok_mid = formula_rb("ok", "1.1.0", "midsha");
        let new_old = formula_rb("new", "1.0.0", "oldsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/o/ok.rb", ok_mid);
        git.insert_blob(
            "headsha",
            "Formula/o/ok.rb",
            formula_rb("ok", "1.2.0", "newsha"),
        );
        git.insert_blob(
            "headsha",
            "Formula/n/new.rb",
            formula_rb("new", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg("ok", ok_old), formula_pkg("new", new_old)],
            ..MockBrew::new()
        };
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        upgrade(
            &brew,
            &git,
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("upgrading 1 of 2 packages"), "{text}");
        assert!(text.contains("notes:"), "{text}");
        let counts = text.find("upgraded 1,").expect("counts line");
        let hold = text.find("new is too new").expect("hold line");
        assert!(hold > counts, "holds belong after the counts line: {text}");
    }

    #[test]
    fn already_installed_message_tells_a_no_op_from_an_upgrade() {
        let said = |text: &str| {
            already_installed_message(&std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: text.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        };
        assert!(said(
            "Warning: foo 1.0 is already installed and up-to-date.\n"
        ));
        assert!(said(
            "Warning: ericfitz/tap/agentbus 1.13.0 already installed\n"
        ));
        assert!(said("Warning: Cask 'x' is already installed.\n"));
        assert!(said("Error: fresh 1.1.0 is already installed\n"));
        assert!(!said(
            "cats 13.8.0 is already installed but outdated (so it will be upgraded).\n"
        ));
        // A real no-op for another package still counts.
        assert!(said(
            "cats 13.8.0 is already installed but outdated (so it will be upgraded).\nWarning: foo 1.0 is already installed and up-to-date.\n"
        ));
    }

    #[test]
    fn upgrade_runs_soaked_work_first_then_one_update_and_one_upgrade_for_no_soak() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/a/alpha.rb",
            formula_rb("alpha", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "headsha",
            "Formula/a/alpha.rb",
            formula_rb("alpha", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![
                formula_pkg("alpha", formula_rb("alpha", "1.0.0", "oldsha")),
                formula_pkg_from(
                    "brewsoak",
                    "ericfitz/tap",
                    formula_rb("brewsoak", "1.0.0", "oldsha"),
                ),
                formula_pkg("wget", formula_rb("wget", "1.0.0", "oldsha")),
            ],
            taps: vec![tapped(
                "ericfitz/tap",
                Some("https://github.com/ericfitz/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\", \"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs[..1], "alpha"),
            "soaked first: {runs:?}"
        );
        assert_eq!(runs[1], vec!["update".to_string()]);
        assert_eq!(runs[2], vec!["upgrade", "ericfitz/tap/brewsoak", "wget"]);
        assert_eq!(runs.len(), 3);
        assert!(!r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no-soak 2"), "{text}");
    }

    #[test]
    fn upgrade_without_no_soak_packages_never_runs_brew_update() {
        let (brew, git) = two_outdated_world("");
        let _ = upgrade_both(&brew, &git);
        assert!(!run_has_token(&lock_runs(&brew), "update"));
    }

    #[test]
    fn upgrade_bare_leaves_pinned_no_soak_out_of_brew_tokens() {
        let brew = MockBrew {
            installed: vec![
                formula_pkg_pinned("wget", formula_rb("wget", "1.0.0", "oldsha")),
                formula_pkg("curl", formula_rb("curl", "1.0.0", "oldsha")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"wget\", \"curl\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(runs[1], vec!["upgrade", "curl"], "{runs:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("pinned 1") && text.contains("no-soak 1"),
            "{text}"
        );
        // Explicitly named: handed to brew, which errors as it would by hand.
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["wget".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(
            runs.last().unwrap(),
            &vec!["upgrade".to_string(), "wget".to_string()]
        );
    }

    #[test]
    fn no_soak_staged_keg_switches_tap_with_reinstall() {
        let brew = MockBrew {
            installed: vec![InstalledPkg {
                tap: None,
                ..formula_pkg("vault", formula_rb("vault", "1.0.0", "oldsha"))
            }],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"hashicorp/tap\"]\n");
        let mut origins = OriginRecords::default();
        origins.set(PkgKind::Formula, "vault", "hashicorp/tap");
        let inv = Inventory::build(brew.installed.clone(), &brew.taps, &origins, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(
            runs[1],
            vec!["reinstall", "hashicorp/tap/vault"],
            "{runs:?}"
        );
    }

    fn switch_world(next: Vec<(i32, Vec<u8>)>) -> (MockBrew, Config) {
        let brew = MockBrew {
            installed: vec![InstalledPkg {
                tap: None,
                ..formula_pkg("vault", formula_rb("vault", "1.0.0", "oldsha"))
            }],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            next_outputs: std::sync::Mutex::new(next.into()),
            ..MockBrew::new()
        };
        (brew, cfg_with("NO_SOAK = [\"hashicorp/tap\"]\n"))
    }

    fn switch_upgrade(brew: &MockBrew, cfg: &Config, cache: &Path) -> String {
        let mut origins = OriginRecords::default();
        origins.set(PkgKind::Formula, "vault", "hashicorp/tap");
        origins.save(cache).unwrap();
        let inv = Inventory::build(brew.installed.clone(), &brew.taps, &origins, cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            brew,
            &git_empty(),
            &core_snaps(),
            cache,
            tap.path(),
            &inv,
            cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn successful_tap_switch_removes_the_stale_origin_record() {
        let (brew, cfg) = switch_world(vec![
            (0, Vec::new()),
            (
                0,
                b"\xf0\x9f\x8d\xba  /opt/homebrew/Cellar/vault/1.2.0: 5 files, 1MB\n".to_vec(),
            ),
        ]);
        let cache = tempfile::tempdir().unwrap();
        switch_upgrade(&brew, &cfg, cache.path());
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "vault"),
            None
        );
    }

    #[test]
    fn dry_run_leaves_the_origin_record_alone() {
        // The switch is skipped under a dry run, so nothing is "switched";
        // and the plain run's state must not be touched either.
        let (brew, cfg) = switch_world(vec![(0, Vec::new()), (0, Vec::new())]);
        let cache = tempfile::tempdir().unwrap();
        let mut origins = OriginRecords::default();
        origins.set(PkgKind::Formula, "vault", "hashicorp/tap");
        origins.save(cache.path()).unwrap();
        let inv = Inventory::build(brew.installed.clone(), &brew.taps, &origins, &cfg);
        let tap = tempfile::tempdir().unwrap();
        upgrade(
            &brew,
            &git_empty(),
            &core_snaps(),
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &["--dry-run".to_string()],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "vault"),
            Some("hashicorp/tap")
        );
    }

    #[test]
    fn failed_tap_switch_keeps_the_origin_record() {
        let (brew, cfg) = switch_world(vec![
            (0, Vec::new()),
            (
                0,
                b"Warning: hashicorp/tap/vault 1.2.0 is already installed and up-to-date.\n"
                    .to_vec(),
            ),
        ]);
        let cache = tempfile::tempdir().unwrap();
        let text = switch_upgrade(&brew, &cfg, cache.path());
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "vault"),
            Some("hashicorp/tap")
        );
        assert!(text.contains("did not replace the staged keg"), "{text}");
    }

    #[test]
    fn staged_package_with_lost_origin_record_notes_unknown_origin() {
        // vault was staged from a tap (receipt tap null); origins.toml was lost;
        // the name resolves nowhere in core or cask.
        let brew = MockBrew {
            installed: vec![InstalledPkg {
                tap: None,
                ..formula_pkg("vault", formula_rb("vault", "1.0.0", "v"))
            }],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert!(
            text.contains("vault: origin unknown; reinstall it from its tap with brew"),
            "{text}"
        );
        assert!(!text.contains("vault is missing at HEAD"), "{text}");
        assert!(text.contains("held 1"), "{text}");
    }
}
