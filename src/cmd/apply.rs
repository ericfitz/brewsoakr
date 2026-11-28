use super::bare::{BareSkip, bare_action, bare_skip, latest_left, latest_note};
use super::deps::{
    CutoffDepWalk, CutoffDeps, HeldDep, NameFail, NameHold, NameWhy, Role, first_line,
};
use super::notes::{
    NoteKind, by_name_note, held_tap_note, newer_than_cutoff_note, refusal_message,
    unsoakable_note, write_notes, write_session_tail,
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
use crate::lock::RunLock;
use crate::nosoak;
use crate::origin::{self, OriginRecords};
use crate::quiet;
use crate::report::{self, Counts, Verb};
use crate::resolve::{PkgKind, PkgRef};
use crate::section::Section;
use crate::snapshot::Snapshots;
use crate::soaktap::{self, NotReady, SoakTap, Staged};
use crate::tap;
use crate::taps;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::Path;

#[allow(clippy::too_many_arguments)]
pub fn reinstall(
    lock: &RunLock,
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
    let dry_run = flags::is_dry_run(user_flags);
    if !dry_run {
        soaktap::maintain(
            lock,
            brew,
            inv.soak_tap.as_ref(),
            tap_root,
            cache,
            inv,
            is_verbose(user_flags),
            out,
        )?;
    }
    let mut session = ApplySession {
        lock,
        brew,
        git,
        snaps,
        cache,
        user_flags,
        inv,
        cfg,
        brew_verb: "reinstall",
        soak: SoakTap::new(inv.soak_tap.as_ref(), tap_root, cache, dry_run),
        verb: Verb::Reinstall,
        bare_run: false,
        force_cask: false,
        force_formula: false,
        refused: false,
        brew_status: None,
        brew_updated: None,
        counts: Counts {
            dry_run,
            ..Counts::default()
        },
        done: BTreeMap::new(),
        counted_deps: HashSet::new(),
        deferred: Vec::new(),
        soaking: Vec::new(),
        ahead: Vec::new(),
        nosoak: Vec::new(),
        origins: OriginRecords::load(cache),
        plan_total,
        plan_index: 0,
        out: Section::new(out, report::soaked_header(Verb::Reinstall, dry_run, None)),
    };
    if is_verbose(user_flags) {
        writeln!(
            session.out.get_mut(),
            "{}",
            report::soak_banner("reinstalling", snaps.hours.get(), &snaps.core, &snaps.cask)
        )?;
    }
    session.update_before_soaked_work(names)?;
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
    lock: &RunLock,
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
    let verb = Verb::parse(brew_verb)
        .ok_or_else(|| Error::Usage(format!("unknown brew verb: {brew_verb}")))?;
    let dry_run = flags::is_dry_run(user_flags);
    // Only a bare upgrade knows its plan up front, and always shows it.
    let plan = plan_total
        .filter(|_| bare_run)
        .map(|total| (total, inv.pkgs.len()));
    let mut session = ApplySession {
        lock,
        brew,
        git,
        snaps,
        cache,
        user_flags,
        inv,
        cfg,
        brew_verb,
        soak: SoakTap::new(inv.soak_tap.as_ref(), tap_root, cache, dry_run),
        verb,
        bare_run,
        force_cask,
        force_formula,
        refused: false,
        brew_status: None,
        brew_updated: None,
        counts: Counts {
            dry_run,
            ..Counts::default()
        },
        done: BTreeMap::new(),
        counted_deps: HashSet::new(),
        deferred: Vec::new(),
        soaking: Vec::new(),
        ahead: Vec::new(),
        nosoak: Vec::new(),
        origins: OriginRecords::load(cache),
        plan_total,
        plan_index: 0,
        out: Section::new(out, report::soaked_header(verb, dry_run, plan)),
    };
    validate_tokens(names)?;
    if !dry_run {
        soaktap::maintain(
            lock,
            brew,
            inv.soak_tap.as_ref(),
            tap_root,
            cache,
            inv,
            is_verbose(user_flags),
            session.out.get_mut(),
        )?;
    }
    if is_verbose(user_flags) {
        let doing = match brew_verb {
            "upgrade" => "upgrading installed formulae and casks",
            "install" => "installing",
            "reinstall" => "reinstalling",
            other => other,
        };
        writeln!(
            session.out.get_mut(),
            "{}",
            report::soak_banner(doing, snaps.hours.get(), &snaps.core, &snaps.cask)
        )?;
    }
    session.update_before_soaked_work(names)?;
    if plan.is_some() {
        session.out.open()?;
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
    Ok(RunResult {
        refused: session.refused,
        brew_status: session.brew_status,
    })
}

pub(super) struct ApplySession<'a, B, G, W: Write> {
    lock: &'a RunLock,
    brew: &'a B,
    git: &'a G,
    snaps: &'a Snapshots,
    cache: &'a Path,
    user_flags: &'a [String],
    inv: &'a Inventory,
    cfg: &'a Config,
    brew_verb: &'a str,
    /// brewsoak's own tap: staged cutoff files for this run's installs.
    soak: SoakTap,
    /// `brew_verb`, parsed; picks section header wording.
    verb: Verb,
    /// A bare `upgrade`: every installed package, none named by the user.
    bare_run: bool,
    force_cask: bool,
    force_formula: bool,
    refused: bool,
    brew_status: Option<i32>,
    /// `Some(true)`: this run's `brew update` succeeded. `Some(false)`: it
    /// failed. `None`: it did not run.
    brew_updated: Option<bool>,
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
    deferred: Vec<(NoteKind, String)>,
    /// `name version` of each package a bare run left at its cutoff while a
    /// newer artifact soaks; listed together under `Notes`.
    soaking: Vec<String>,
    /// Packages installed ahead of soak, left unchanged.
    ahead: Vec<String>,
    /// No-soak packages collected for the single brew step after soaked work.
    nosoak: Vec<nosoak::Target>,
    /// Where each tap-staged keg came from; saved after every real install.
    origins: OriginRecords,
    /// Number of packages this run expects to change, when known up front.
    plan_total: Option<usize>,
    plan_index: usize,
    /// The soaked section. Its header goes out with the first line of
    /// per-package output; the tail writes past it through `get_mut`.
    out: Section<&'a mut W>,
}

impl<B: Brew, G: GitStore, W: Write> ApplySession<'_, B, G, W> {
    /// Spec "`brew update` before soaked work": one `brew update` ahead of any
    /// loader or `brew deps` question, so brew's data is fresh for the gate.
    /// A failure is a note and an exit code, never a hold.
    fn update_before_soaked_work(&mut self, names: &[String]) -> Result<(), Error> {
        let soaked = if self.bare_run {
            self.plan_total.is_some_and(|n| n > 0)
        } else {
            names.iter().any(|raw| {
                resolve_token(raw, self.inv, self.cfg)
                    .is_ok_and(|r| matches!(r.class, PkgClass::Soaked))
            })
        };
        if !soaked {
            return Ok(());
        }
        match nosoak::brew_update(self.brew, self.out.get_mut())? {
            None => self.brew_updated = Some(true),
            Some(code) => {
                self.brew_updated = Some(false);
                self.defer(format!(
                    "brew update failed (exit {code}); brew's data may be older than the soak cutoff, so more packages may be held"
                ));
                max_status(&mut self.brew_status, code);
            }
        }
        Ok(())
    }

    fn defer(&mut self, msg: String) {
        self.deferred.push((NoteKind::Other, msg));
    }

    /// A package brewsoak refused or could not act on.
    fn defer_held(&mut self, msg: String) {
        self.deferred.push((NoteKind::Held, msg));
    }

    /// Say once which of `flags` `brew <verb>` would reject and we dropped.
    fn note_dropped_flags(&mut self, verb: &str, flags: &[String], context: &str) {
        let dropped = crate::flags::filter_for_verb(verb, flags).dropped;
        if let Some(note) = crate::flags::dropped_note(verb, &dropped, context)
            && !self.deferred.iter().any(|(_, m)| *m == note)
        {
            self.defer(note);
        }
    }

    /// What a soaked `brew install` may see. On an upgrade the greedy flags
    /// are brewsoak's (read by `bare_action`), so they are neither forwarded
    /// nor reported as dropped; `-g` would otherwise reach brew install as
    /// `--git`. The install table filter still applies in `brew_install_args`.
    fn name_install_flags(&self) -> Vec<String> {
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
        let doing = match self.verb {
            Verb::Install => "installing",
            Verb::Reinstall => "reinstalling",
            Verb::Upgrade => "upgrading",
        };
        let from = view.installed.as_ref().map(report::identity_version);
        if report::latest_redefined(from.as_deref(), Some(&to)) {
            writeln!(
                self.out,
                "{counter}reinstalling {name} (latest definition changed)"
            )?;
            return Ok(());
        }
        match from {
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

    /// Everything worth saying once the packages are done: the summary, held
    /// and skipped packages, caveats, and where the raw brew log went for
    /// anyone who wants the detail we filtered out.
    fn write_tail(&mut self) -> Result<(), Error> {
        let report = self.brew.session_report();
        let out = self.out.get_mut();
        let mut summary = Section::new(&mut *out, "Summary");
        for line in report::summary_lines(&self.counts, report.added_bytes, report.freed_bytes) {
            writeln!(summary, "{line}")?;
        }
        drop(summary);
        write_notes(out, &self.soaking, &self.ahead, &self.deferred)?;
        write_session_tail(&report, out)
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
                    self.defer_held(msg);
                } else {
                    let token = nosoak::brew_token(&r.origin, &name);
                    self.defer_held(format!(
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
            self.defer_held(note);
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
                    view.newer_soaking(),
                    did,
                )
            )?;
        }
        self.counts.note(view.action, view.newer_soaking());
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
                // brew calls it outdated; say why nothing was installed.
                if let Some(label) = view.soaking_label() {
                    if self.bare_run {
                        self.soaking.push(format!("{name} {label}"));
                    } else {
                        let (installed_ver, _) = view_versions(&view);
                        let hours = self.cfg.effective_hours(&r.origin);
                        self.defer(report::soaking_note(&name, &installed_ver, &label, hours));
                    }
                }
            }
            // Silent, as brew is: the app updates itself. `-v` shows the line.
            DesiredAction::LeaveAutoUpdates => {}
            DesiredAction::LeaveAheadOfSoak => {
                if self.brew_verb == "reinstall" {
                    self.defer_held(format!(
                        "{name} is ahead of soak; reinstall would pull a too-new artifact; use `brew reinstall {token}` to bypass brewsoak."
                    ));
                    self.refused = true;
                } else {
                    self.ahead.push(name.clone());
                }
            }
            DesiredAction::LeaveNewerThanCutoff => {
                let (installed_ver, cutoff_ver) = view_versions(&view);
                if self.brew_verb == "reinstall" {
                    self.defer_held(format!(
                        "{name} is newer than its cutoff {cutoff_ver}; reinstall would downgrade it; use `brew reinstall {token}` to bypass brewsoak."
                    ));
                    self.refused = true;
                } else {
                    self.defer(newer_than_cutoff_note(&name, &installed_ver, &cutoff_ver));
                }
            }
            DesiredAction::HoldUnordered => {
                let (installed_ver, cutoff_ver) = view_versions(&view);
                self.defer_held(format!(
                    "{name}: brew could not compare installed {installed_ver} with cutoff {cutoff_ver}; holding it"
                ));
                self.refused = true;
            }
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => {
                if let Some(msg) = refusal_message(view.action, &token, self.brew_verb) {
                    self.defer_held(msg);
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

    /// Install one soaked target with its dependencies. Whatever happens, the
    /// tap files staged for it are kept or put back afterwards; the first
    /// error of the install and the settle is returned.
    fn install_cutoff(
        &mut self,
        name: &str,
        origin_tap: &str,
        kind: PkgKind,
        view: &ResolvedView,
    ) -> Result<(), Error> {
        let installed = self.install_cutoff_staged(name, origin_tap, kind, view);
        let settled = self.soak.settle(self.lock, self.brew);
        installed.and(settled)
    }

    fn install_cutoff_staged(
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
        let token = nosoak::brew_token(origin_tap, name);
        let walked = match self.collect_cutoff_deps(origin_tap, name, kind) {
            Ok(w) => w,
            // A bare dependency name matches formulae in several taps: brew
            // will not pick one, and neither will brewsoak.
            Err(Error::Brew { message, .. }) if taps::ambiguous_formula(&message) => {
                self.refuse_ambiguous_dep(name, &token, &message)?;
                return Ok(());
            }
            // brew could not list the dependencies of a third-party target
            // by name.
            Err(Error::Brew { message, .. }) => {
                match taps::untrusted_tap(&message) {
                    Some(tap) => self.hold_target_untrusted(name, &tap),
                    None => {
                        let hold = NameHold {
                            dep: None,
                            why: NameFail::Why(NameWhy::Deps(first_line(&message))),
                        };
                        self.hold_target_by_name(name, &token, &hold)?;
                    }
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if let Some(hold) = walked.by_name {
            return self.hold_target_by_name(name, &token, &hold);
        }
        if let Some(held) = walked.held {
            return self.refuse_held_dep(name, &token, &held);
        }
        for (dep_origin, dep, dep_kind) in walked.deps {
            if !self.install_missing_dep(name, &token, &dep_origin, dep_kind, &dep)? {
                return Ok(());
            }
        }

        let installing = view.cutoff.as_ref().map(report::identity_version);
        match self.run_install(&pkg, origin_tap, view.cutoff_blob.as_deref(), installing)? {
            InstallRun::Ran { failed } => {
                // Counted as upgraded when it was announced; brew failed.
                if failed {
                    self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
                }
            }
            InstallRun::Untrusted(tap) => self.hold_target_untrusted(name, &tap),
            InstallRun::NotReady(n) => self.hold_target_not_ready(name, &n)?,
            InstallRun::Interrupted => self.hold_target_interrupted(name)?,
        }
        Ok(())
    }

    /// Install a target or dependency by the route its origin has: a package
    /// of homebrew/core or homebrew/cask from brewsoak's tap, after its cutoff
    /// file is staged (again, when an earlier settle put it back); any other
    /// by its bare name.
    fn run_install(
        &mut self,
        pkg: &PkgRef,
        origin_tap: &str,
        cutoff: Option<&[u8]>,
        installing: Option<String>,
    ) -> Result<InstallRun, Error> {
        let install_flags = self.name_install_flags();
        let args = if origin::is_core_or_cask(origin_tap) {
            let cutoff = cutoff.ok_or_else(|| {
                Error::Other(format!(
                    "{}: no cutoff file to stage for the soak tap",
                    pkg.name
                ))
            })?;
            match self
                .soak
                .stage(self.lock, self.brew, pkg, cutoff, installing.as_deref())?
            {
                Staged::NotReady(n) => return Ok(InstallRun::NotReady(n)),
                Staged::Leftover => return Ok(InstallRun::Interrupted),
                Staged::Unchanged | Staged::Written => {}
            }
            let installed = self.inv.find_in(origin_tap, &pkg.name).is_some();
            let verb = tap::soak_tap_verb(pkg.kind, installed);
            self.note_dropped_flags(verb, &install_flags, "soaked installs");
            tap::soak_tap_args(pkg, installed, &install_flags)
        } else {
            self.note_dropped_flags("install", &install_flags, "soaked installs");
            tap::brew_install_args(pkg, &pkg.name, &install_flags)
        };
        Ok(
            match self.record_name_install(origin_tap, &pkg.name, pkg.kind, installing, &args)? {
                NameRun::Ran { failed } => InstallRun::Ran { failed },
                NameRun::Untrusted(tap) => InstallRun::Untrusted(tap),
                NameRun::TapNotReady => InstallRun::NotReady(NotReady::Untrusted),
            },
        )
    }

    fn collect_cutoff_deps(
        &mut self,
        root_origin: &str,
        root: &str,
        root_kind: PkgKind,
    ) -> Result<CutoffDeps, Error> {
        let mut walk = CutoffDepWalk {
            brew: self.brew,
            git: self.git,
            snaps: self.snaps,
            cache: self.cache,
            inv: self.inv,
            cfg: self.cfg,
            lock: self.lock,
            soak: &mut self.soak,
            target: root,
            visiting: HashSet::new(),
            visited: HashSet::new(),
            out: Vec::new(),
            held: None,
            by_name: None,
        };
        walk.visit(root_origin, root, root_kind, Role::Root)?;
        Ok(CutoffDeps {
            deps: walk.out,
            held: walk.held,
            by_name: walk.by_name,
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
            let versions = match (from, to) {
                (Some(from), Some(to)) => format!(" {from} -> {to}"),
                (Some(from), None) => format!(" {from}"),
                (None, Some(to)) => format!(" {to}"),
                (None, None) => String::new(),
            };
            writeln!(
                self.out,
                "upgrading {dep}{versions} (dependency of {target})"
            )?;
            self.counts.upgraded += 1;
            self.counted_deps.insert(dep.to_string());
        }
        let installing = want.as_ref().map(report::identity_version);
        match self.run_install(&pkg, dep_origin, blobs.cutoff.as_deref(), installing)? {
            InstallRun::Ran { failed } => {
                if failed && counted_here && self.counted_deps.remove(dep) {
                    self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
                }
            }
            // The target cannot install without this dep: hold the target.
            InstallRun::NotReady(n) => {
                self.uncount_dep(dep);
                self.hold_target_not_ready(target, &n)?;
                return Ok(false);
            }
            InstallRun::Interrupted => {
                self.uncount_dep(dep);
                self.hold_target_interrupted(target)?;
                return Ok(false);
            }
            // The target cannot install without this dep: hold the target once
            // (one line, one count) rather than the dep and then the target.
            InstallRun::Untrusted(tap) => {
                if counted_here {
                    self.uncount_dep(dep);
                    writeln!(
                        self.out,
                        "held {dep} (dependency of {target}): brew does not trust tap {tap}"
                    )?;
                }
                self.hold_target_untrusted_dep(target, dep, &tap);
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Take back the upgraded count of a dependency that was announced and
    /// counted but then did not install.
    fn uncount_dep(&mut self, dep: &str) {
        if self.counted_deps.remove(dep) {
            self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
        }
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

    /// The by-name gate failed for the target or one of its dependencies:
    /// hold the target with one inline line and one note, and carry on.
    fn hold_target_by_name(
        &mut self,
        target: &str,
        target_token: &str,
        hold: &NameHold,
    ) -> Result<(), Error> {
        let why = match &hold.why {
            NameFail::Untrusted(tap) => {
                match &hold.dep {
                    Some(dep) => self.hold_target_untrusted_dep(target, dep, tap),
                    None => self.hold_target_untrusted(target, tap),
                }
                return Ok(());
            }
            NameFail::TapNotReady(n) => return self.hold_target_not_ready(target, n),
            NameFail::Interrupted => return self.hold_target_interrupted(target),
            NameFail::Why(why) => why.text(),
        };
        self.hold_target_for_dep();
        match &hold.dep {
            Some(dep) => writeln!(self.out, "held {target}: dependency {dep}: {why}")?,
            None => writeln!(self.out, "held {target}: {why}")?,
        }
        let bypass = format!("brew {} {target_token}", self.brew_verb);
        self.defer_held(by_name_note(target, hold.dep.as_deref(), &why, &bypass));
        Ok(())
    }

    /// brewsoak's tap cannot be used this run: hold the target with one
    /// inline line, and say why once however many packages it holds.
    fn hold_target_not_ready(&mut self, target: &str, n: &NotReady) -> Result<(), Error> {
        self.hold_target_for_dep();
        writeln!(self.out, "held {target}: brewsoak's tap is not ready")?;
        let note = n.note();
        if !self
            .deferred
            .iter()
            .any(|(kind, text)| *kind == NoteKind::Held && *text == note)
        {
            self.defer_held(note);
        }
        Ok(())
    }

    /// An earlier run left an undo entry for the package: nothing is written
    /// until `brewsoak update` finishes it.
    fn hold_target_interrupted(&mut self, target: &str) -> Result<(), Error> {
        self.hold_target_for_dep();
        writeln!(
            self.out,
            "held {target}: an earlier brewsoak run was interrupted"
        )?;
        self.defer_held(format!(
            "{target}: an earlier brewsoak run was interrupted; run brewsoak update to finish it"
        ));
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

    /// Runs one by-name install. A real install writes a receipt that names
    /// the tap, so any origin record is dropped. A tap package brew would not
    /// trust is reported without brew's status, so the caller holds it.
    fn record_name_install(
        &mut self,
        origin_tap: &str,
        name: &str,
        kind: PkgKind,
        installing: Option<String>,
        args: &[String],
    ) -> Result<NameRun, Error> {
        // The version being installed is on purpose; brew's "newer version is
        // available" warning about it is noise.
        let ctx = quiet::RunContext {
            expected: installing
                .into_iter()
                .map(|v| (name.to_ascii_lowercase(), v))
                .collect(),
            ..quiet::RunContext::default()
        };
        let output = self.brew.run_visible_in(args, &mut self.out, &ctx)?;
        let text = String::from_utf8_lossy(&output.stdout);
        if !output.status.success()
            && let Some(tap) = taps::untrusted_tap(&text)
        {
            if tap.eq_ignore_ascii_case(origin::SOAK_TAP) {
                return Ok(NameRun::TapNotReady);
            }
            if !origin::is_core_or_cask(origin_tap) {
                return Ok(NameRun::Untrusted(tap));
            }
        }
        self.done
            .extend(quiet::installed_from_output(&output.stdout));
        // Only a real install writes a receipt; brew's "already installed"
        // answer changes nothing.
        let installed_now = output.status.success() && !already_installed_message(&output);
        let failed = brew_failed(&output);
        merge_status(&mut self.brew_status, output);
        if installed_now {
            self.forget_origin(kind, name)?;
        }
        Ok(NameRun::Ran { failed })
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
        let step = nosoak::run_step_after(
            self.brew,
            self.brew_verb,
            self.user_flags,
            &targets,
            self.brew_updated == Some(true),
            self.out.get_mut(),
        )?;
        self.counts.no_soak += step.count;
        self.counts.no_soak_upgraded += step.upgraded;
        self.counts.no_soak_current += step.current;
        self.counts.no_soak_no_result += step.no_result;
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
        let output = self.brew.run_visible(args, &mut self.out)?;
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

/// What one by-name `brew install` came to.
pub(super) enum NameRun {
    /// brew ran; `failed` when it exited non-zero for a reason of its own.
    Ran { failed: bool },
    /// brew does not trust this tap (lowercase `user/repo`).
    Untrusted(String),
    /// brew does not trust brewsoak's own tap.
    TapNotReady,
}

/// What `run_install` came to.
enum InstallRun {
    Ran { failed: bool },
    Untrusted(String),
    NotReady(NotReady),
    Interrupted,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::test_support::summary_has;

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
        run_is_name_install, strip_soak_prefix, tapped, test_cache, upgrade_names,
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
    fn dry_run_soaked_header() {
        let (brew, git) = two_outdated_world("");
        let flags = ["--dry-run".to_string()];
        let text = upgrade_names_flags(
            &brew,
            &git,
            &["left".to_string(), "right".to_string()],
            &flags,
        );
        assert!(
            text.starts_with("Updating Homebrew\nWould upgrade soaked packages\n  "),
            "{text}"
        );
        assert!(text.contains("\n  would upgrade "), "{text}");
    }

    #[test]
    fn soaked_output_is_indented_under_its_header() {
        let (brew, git) =
            two_outdated_world("\u{1f37a}  /opt/homebrew/Cellar/left/1.1.0: 5 files, 1MB\n");
        let text = upgrade_both(&brew, &git);
        assert!(
            text.starts_with(
                "Updating Homebrew\nUpgrading soaked packages\n  upgrading left 1.0.0 -> 1.1.0\n"
            ),
            "{text}"
        );
        assert!(text.contains("\nSummary\n  upgraded"), "{text}");
    }

    #[test]
    fn apply_many_rejects_an_unknown_brew_verb() {
        let (brew, git) = two_outdated_world("");
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let err = apply_many(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["left".to_string()],
            false,
            "frobnicate",
            false,
            false,
            &[],
            None,
            &mut out,
        )
        .expect_err("unknown verb must be rejected");
        assert!(err.to_string().contains("frobnicate"), "{err}");
        assert!(out.is_empty(), "nothing printed");
        assert!(lock_runs(&brew).is_empty());
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
            2,
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
        assert_eq!(visible.len(), 2, "{visible:?}\n{text}");
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
        assert_eq!(visible.len(), 3, "{visible:?}");
    }

    #[test]
    fn unreadable_keg_receipt_is_not_done() {
        let (mut brew, git) =
            two_outdated_world("\u{1f37a}  /opt/homebrew/Cellar/right/1.1.0: 5 files, 1MB\n");
        brew.kegs.clear();
        upgrade_names(&brew, &git, &[]);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 3, "{visible:?}");
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

    /// Names of the packages `brew install` ran by bare name, in order.
    fn name_install_order(brew: &MockBrew) -> Vec<String> {
        brew.visible_runs
            .lock()
            .expect("visible")
            .iter()
            .filter(|a| matches!(a.first().map(String::as_str), Some("install" | "upgrade")))
            .filter_map(|a| a.last().map(|l| strip_soak_prefix(l).to_string()))
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
        assert_eq!(name_install_order(&brew), ["simdutf", "node"]);
    }

    #[test]
    fn named_upgrade_upgrades_an_outdated_installed_dependency_first() {
        let (brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        let text = upgrade_names(&brew, &git, &["node".to_string()]);
        assert_eq!(name_install_order(&brew), ["simdutf", "node"]);
        assert!(
            text.contains("upgrading simdutf 1.0.0 -> 1.1.0 (dependency of node)"),
            "{text}"
        );
        assert!(summary_has(&text, "upgraded", "2"), "{text}");
    }

    /// Core `node` depends on `widget`, installed from `hashicorp/tap` at
    /// 1.0.0 behind a 1.1.0 cutoff. `refusal` is brew's answer to the by-name
    /// `widget` install. Returns the output, the brew and the run's result.
    fn node_with_tap_dep_world(refusal: &str) -> (String, MockBrew, super::super::RunResult) {
        let (mut brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        brew.installed[1] = formula_pkg_from(
            "widget",
            "hashicorp/tap",
            formula_rb("widget", "1.0.0", "oldsha"),
        );
        brew.taps = vec![tapped(
            "hashicorp/tap",
            Some("https://github.com/hashicorp/homebrew-tap"),
        )];
        brew.deps
            .insert("node".to_string(), vec!["hashicorp/tap/widget".to_string()]);
        // The run's `brew update` takes the first queued answer.
        brew.next_outputs.lock().unwrap().push_back((0, Vec::new()));
        brew.next_outputs
            .lock()
            .unwrap()
            .push_back((1, refusal.as_bytes().to_vec()));
        git.insert_blob(
            "tapcut",
            "Formula/widget.rb",
            formula_rb("widget", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/widget.rb",
            formula_rb("widget", "1.2.0", "newsha"),
        );
        git.insert_tree("tapcut", &["Formula/widget.rb"]);
        git.insert_tree("taphead", &["Formula/widget.rb"]);
        let snaps =
            crate::cmd::test_support::tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/widget.rb"]);
        git.insert_tree("taphead", &["Formula/widget.rb"]);
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["node".to_string()],
            &[],
            &mut out,
        )
        .unwrap();
        (String::from_utf8(out).unwrap(), brew, r)
    }

    #[test]
    fn untrusted_dependency_that_was_announced_is_then_reported_held() {
        let (text, _, _) = node_with_tap_dep_world(
            "Error: widget: Refusing to load formula hashicorp/tap/widget from untrusted tap hashicorp/tap.\n",
        );
        assert!(
            text.contains("upgrading widget 1.0.0 -> 1.1.0 (dependency of node)"),
            "{text}"
        );
        assert!(
            text.contains(
                "held widget (dependency of node): brew does not trust tap hashicorp/tap"
            ),
            "{text}"
        );
        assert!(summary_has(&text, "upgraded", "0"), "{text}");
        assert!(summary_has(&text, "held", "1"), "{text}");
    }

    #[test]
    fn dependency_brew_load_error_is_brews_failure_and_not_counted_upgraded() {
        let (text, _, r) = node_with_tap_dep_world("Error: cannot load such file -- helper\n");
        assert!(
            text.contains("upgrading widget 1.0.0 -> 1.1.0 (dependency of node)"),
            "{text}"
        );
        assert!(!text.contains("staged copy"), "{text}");
        assert!(!summary_has(&text, "upgraded", "2"), "{text}");
        assert!(
            summary_has(&text, "held", "0"),
            "a brew failure is not a hold: {text}"
        );
        assert!(
            r.brew_status.is_some_and(|s| s != 0),
            "brew's failure keeps the run's exit code: {:?}",
            r.brew_status
        );
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
        assert_eq!(name_install_order(&brew), ["simdutf", "node"], "{text}");
        assert_eq!(
            text.matches("upgrading simdutf 1.0.0 -> 1.1.0 (dependency of node)")
                .count(),
            1,
            "{text}"
        );
        assert!(
            summary_has(&text, "upgraded", "2"),
            "simdutf counted once: {text}"
        );
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
        assert!(name_install_order(&brew).is_empty(), "{text}");
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
        assert!(run_is_name_install(&runs, "app"), "{text}");
        assert!(
            run_is_name_install(&runs, "helper"),
            "helper upgrades at its own plan entry: {text}"
        );
        assert!(summary_has(&text, "upgraded", "2"), "{text}");
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
        assert!(summary_has(&text, "upgraded", "2"), "{text}");
    }

    #[test]
    fn dry_run_reaches_a_dependency_install() {
        let (brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        let flags = ["--dry-run".to_string()];
        upgrade_names_flags(&brew, &git, &["node".to_string()], &flags);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 3, "{visible:?}");
        assert_eq!(visible[0], ["update"], "{visible:?}");
        for args in &visible[1..] {
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
        assert_eq!(name_install_order(&brew), ["simdutf", "node"], "{text}");
        assert!(
            text.contains("[2/2] simdutf 1.1.0: already upgraded as a dependency"),
            "{text}"
        );
        assert!(summary_has(&text, "upgraded", "2"), "{text}");
    }

    #[test]
    fn installed_dependency_at_the_cutoff_is_not_reinstalled() {
        let (brew, git) = node_simdutf_world("1.0.0", "1.1.0", "");
        upgrade_names(&brew, &git, &[]);
        assert_eq!(name_install_order(&brew), ["node"]);
    }

    #[test]
    fn installed_dependency_newer_than_the_cutoff_is_left_alone() {
        let (mut brew, git) = node_simdutf_world("1.0.0", "1.3.0", "");
        brew.orderings.insert(
            ("1.3.0".to_string(), "1.1.0".to_string()),
            std::cmp::Ordering::Greater,
        );
        let text = upgrade_names(&brew, &git, &[]);
        assert_eq!(name_install_order(&brew), ["node"], "{text}");
    }

    #[test]
    fn pinned_installed_dependency_is_not_upgraded_for_its_dependent() {
        let (mut brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        brew.installed[1].pinned = true;
        upgrade_names(&brew, &git, &[]);
        assert_eq!(name_install_order(&brew), ["node"]);
    }

    #[test]
    fn name_install_drops_flags_brew_install_rejects_and_says_so_once() {
        let (brew, git) = two_outdated_world("");
        let flags = ["--greedy", "--ignore-pinned", "--verbose"].map(String::from);
        let text = upgrade_names_flags(&brew, &git, &[], &flags);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 3, "{visible:?}");
        for args in &visible[1..] {
            assert!(
                !args
                    .iter()
                    .any(|a| a == "--greedy" || a == "--ignore-pinned"),
                "{args:?}"
            );
            assert!(args.iter().any(|a| a == "--verbose"), "{args:?}");
        }
        // `--greedy` is brewsoak's on an upgrade, so it is not "dropped".
        let note = "brew upgrade does not accept --ignore-pinned; dropped from soaked installs";
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
    fn soaked_install_tells_the_filter_which_version_it_installs() {
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
        assert!(text.contains("-> 26.10.0_2"), "{text}");
        let ctx = brew.last_ctx.lock().unwrap().clone().expect("a brew run");
        let want: std::collections::BTreeMap<String, String> =
            [("node".to_string(), "26.10.0_2".to_string())].into();
        assert_eq!(ctx.expected, want);
    }

    #[test]
    fn package_at_a_different_version_is_still_installed() {
        // Same shape, but brew left `right` on a version we did not want.
        let (brew, git) =
            two_outdated_world("\u{1f37a}  /opt/homebrew/Cellar/right/1.0.9: 5 files, 1MB\n");
        upgrade_both(&brew, &git);
        let visible = brew.visible_runs.lock().expect("visible").clone();
        assert_eq!(visible.len(), 3, "{visible:?}");
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
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("Upgrading soaked packages (1 of 2)"),
            "{text}"
        );
        let summary = text.find("\nSummary\n").expect("summary section");
        let notes = text.find("\nNotes\n  Held:\n").expect("notes section");
        let hold = text.find("new is too new").expect("hold line");
        assert!(
            hold > notes && notes > summary,
            "holds follow the summary: {text}"
        );
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

    /// `alpha` is soaked work; `brewsoak` and `wget` are no-soak.
    fn soaked_and_no_soak_world() -> (MockBrew, InMemoryGit, Config) {
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
        (brew, git, cfg)
    }

    #[test]
    fn upgrade_runs_soaked_work_first_then_one_update_and_one_upgrade_for_no_soak() {
        let (brew, git, cfg) = soaked_and_no_soak_world();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(runs[0], vec!["update".to_string()], "{runs:?}");
        assert!(
            run_is_name_install(&runs[1..2], "alpha"),
            "soaked first: {runs:?}"
        );
        assert_eq!(runs[2], vec!["upgrade", "ericfitz/tap/brewsoak", "wget"]);
        assert_eq!(
            runs.len(),
            3,
            "the no-soak step repeats no update: {runs:?}"
        );
        assert!(!r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(summary_has(&text, "no-soak", "2"), "{text}");
    }

    /// Wraps a `MockBrew` and logs every `run_visible_in`, `deps` and
    /// `by_name` call in one list, in call order.
    struct LogBrew {
        inner: MockBrew,
        log: std::sync::Mutex<Vec<String>>,
    }

    impl LogBrew {
        fn note(&self, entry: String) {
            self.log.lock().unwrap().push(entry);
        }
    }

    impl crate::brew::Brew for LogBrew {
        fn brew_bin(&self) -> &Path {
            self.inner.brew_bin()
        }
        fn run(&self, a: &[String]) -> Result<std::process::Output, Error> {
            self.inner.run(a)
        }
        fn run_visible_in(
            &self,
            a: &[String],
            t: &mut dyn std::io::Write,
            ctx: &crate::quiet::RunContext,
        ) -> Result<std::process::Output, Error> {
            self.note(format!("run {}", a.join(" ")));
            self.inner.run_visible_in(a, t, ctx)
        }
        fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
            self.inner.installed_packages()
        }
        fn tap_info(&self) -> Result<Vec<crate::taps::TapInfo>, Error> {
            self.inner.tap_info()
        }
        fn tap_new(&self, tap: &str) -> Result<(), String> {
            self.inner.tap_new(tap)
        }
        fn receipt_tap(&self, kind: PkgKind, name: &str, version: &str) -> Option<String> {
            self.inner.receipt_tap(kind, name, version)
        }
        fn outdated_names(&self, greedy: crate::flags::Greedy) -> Result<Vec<String>, Error> {
            self.inner.outdated_names(greedy)
        }
        fn by_name(
            &self,
            q: &[crate::brew::NameQuery],
        ) -> Result<Vec<Result<crate::brew::ByName, String>>, Error> {
            self.note("by_name".to_string());
            self.inner.by_name(q)
        }
        fn deps(&self, k: PkgKind, token: &str) -> Result<Vec<String>, Error> {
            self.note(format!("deps {token}"));
            self.inner.deps(k, token)
        }
    }

    #[test]
    fn soaked_upgrade_runs_brew_update_before_asking_brew_anything() {
        let (inner, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        let brew = LogBrew {
            inner,
            log: std::sync::Mutex::new(Vec::new()),
        };
        let cfg = cfg24();
        let inv = inv_from(&brew.inner, &cfg);
        upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tempfile::tempdir().unwrap().path(),
            &inv,
            &cfg,
            &["node".to_string()],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        let log = brew.log.lock().unwrap().clone();
        assert_eq!(
            log.first().map(String::as_str),
            Some("run update"),
            "{log:?}"
        );
        assert_eq!(
            log.iter().filter(|e| *e == "run update").count(),
            1,
            "{log:?}"
        );
        assert!(
            log.iter().any(|e| e.starts_with("deps ") || e == "by_name"),
            "the gate asked brew something after the update: {log:?}"
        );
    }

    #[test]
    fn bare_upgrade_with_an_empty_plan_runs_no_brew_update() {
        // Installed at the cutoff while HEAD is newer: nothing to upgrade.
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid.clone());
        git.insert_blob(
            "headsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_mid)],
            ..MockBrew::new()
        };
        let text = upgrade_names(&brew, &git, &[]);
        assert!(lock_runs(&brew).is_empty(), "{:?}", lock_runs(&brew));
        assert!(!text.contains("Updating Homebrew"), "{text}");
    }

    #[test]
    fn named_run_of_only_no_soak_packages_updates_in_the_no_soak_step() {
        let (brew, git, cfg) = soaked_and_no_soak_world();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tempfile::tempdir().unwrap().path(),
            &inv,
            &cfg,
            &["wget".to_string()],
            &[],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(
            runs,
            vec![
                vec!["update".to_string()],
                vec!["upgrade".into(), "wget".into()]
            ],
            "{runs:?}"
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("Updating Homebrew").count(), 1, "{text}");
    }

    #[test]
    fn run_with_soaked_and_no_soak_work_updates_exactly_once() {
        let (brew, git, cfg) = soaked_and_no_soak_world();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tempfile::tempdir().unwrap().path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let updates = lock_runs(&brew)
            .iter()
            .filter(|a| a.as_slice() == ["update"])
            .count();
        assert_eq!(updates, 1);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("Updating Homebrew").count(), 1, "{text}");
        assert!(
            text.find("Updating Homebrew") < text.find("Upgrading soaked packages"),
            "{text}"
        );
    }

    /// `left` upgraded by name with `queue` as brew's first answers.
    fn named_left_run(
        queue: Vec<(i32, Vec<u8>)>,
        flags: &[String],
    ) -> (MockBrew, String, super::super::RunResult) {
        let (brew, git) = two_outdated_world("");
        brew.next_outputs.lock().unwrap().extend(queue);
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let r = upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tempfile::tempdir().unwrap().path(),
            &inv,
            &cfg,
            &["left".to_string()],
            flags,
            &mut out,
        )
        .unwrap();
        (brew, String::from_utf8(out).unwrap(), r)
    }

    #[test]
    fn failed_brew_update_notes_it_and_the_package_still_installs() {
        let (brew, text, r) = named_left_run(vec![(1, b"Error: offline".to_vec())], &[]);
        let visible = brew.visible_runs.lock().unwrap().clone();
        assert!(
            visible
                .iter()
                .any(|a| a == &["upgrade", "--formula", "ericfitz/brewsoak/left"]),
            "{visible:?}"
        );
        assert!(
            text.contains(
                "brew update failed (exit 1); brew's data may be older than the soak cutoff, so more packages may be held"
            ),
            "{text}"
        );
        let note = text
            .find("brew update failed (exit 1)")
            .expect("update failure note");
        assert!(
            text.find("\nNotes\n").is_some_and(|n| n < note),
            "the note belongs in the Notes block: {text}"
        );
        assert!(!text.contains("Held:"), "{text}");
        assert!(summary_has(&text, "held", "0"), "{text}");
        assert_eq!(r.brew_status, Some(1));
        assert!(!r.refused);
    }

    #[test]
    fn failed_brew_update_then_failing_install_keeps_the_max_status() {
        let (brew, _text, r) = named_left_run(
            vec![
                (3, b"Error: offline".to_vec()),
                (1, b"Error: boom".to_vec()),
            ],
            &[],
        );
        assert!(!name_install_order(&brew).is_empty());
        assert_eq!(r.brew_status, Some(3));
    }

    #[test]
    fn reinstall_with_a_failed_update_notes_it_and_still_reinstalls() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "headsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", formula_rb("wget", "1.0.0", "oldsha"))],
            ..MockBrew::new()
        };
        brew.next_outputs
            .lock()
            .unwrap()
            .push_back((1, b"Error: offline".to_vec()));
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["wget".to_string()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(name_install_order(&brew), ["wget"], "{text}");
        assert!(text.contains("brew update failed (exit 1)"), "{text}");
        assert_eq!(r.brew_status, Some(1));
        assert!(!r.refused);
    }

    #[test]
    fn soak_tap_runs_carry_only_soak_tap_tokens_and_no_paths() {
        use crate::cmd::install::install;
        use crate::cmd::test_support::assert_soak_tap_commands;

        let run_install = |brew: &MockBrew, git: &InMemoryGit, name: &str, cask: bool| {
            let cfg = cfg24();
            let inv = inv_from(brew, &cfg);
            let tap = tempfile::tempdir().unwrap();
            install(
                &crate::lock::RunLock::for_tests(),
                brew,
                git,
                &core_snaps(),
                test_cache(),
                tap.path(),
                &inv,
                &cfg,
                &[name.to_string()],
                cask,
                false,
                &[],
                &mut Vec::new(),
            )
            .unwrap();
        };

        // Named upgrade with an upgrading dependency.
        let (brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["node".to_string()],
            &[],
            &mut out,
        )
        .unwrap();
        assert!(!name_install_order(&brew).is_empty());
        assert_soak_tap_commands(&brew, &["node", "simdutf"]);

        // Bare upgrade.
        let (brew, git) = two_outdated_world("");
        let inv = inv_from(&brew, &cfg);
        upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!name_install_order(&brew).is_empty());
        assert_soak_tap_commands(&brew, &["left", "right"]);

        // Fresh install with a missing dependency.
        let git = InMemoryGit::new();
        for (dir, name) in [("f", "fresh"), ("lib", "lib")] {
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
        let mut brew = MockBrew::new();
        brew.deps
            .insert("fresh".to_string(), vec!["lib".to_string()]);
        run_install(&brew, &git, "fresh", false);
        assert_eq!(name_install_order(&brew), ["lib", "fresh"]);
        assert_soak_tap_commands(&brew, &["lib", "fresh"]);

        // Cutoff reinstall of an outdated package.
        let (brew, git) = node_simdutf_world("1.0.0", "1.0.0", "");
        call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["node".to_string()],
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!name_install_order(&brew).is_empty());
        assert_soak_tap_commands(&brew, &["node", "simdutf"]);

        // Cask target.
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/f/firefox.rb", cask_rb("firefox", "1.0"));
        git.insert_blob("caskhead", "Casks/f/firefox.rb", cask_rb("firefox", "1.1"));
        let brew = MockBrew::new();
        run_install(&brew, &git, "firefox", true);
        assert_eq!(name_install_order(&brew), ["firefox"]);
        assert_soak_tap_commands(&brew, &["firefox"]);

        // Tap formula dependency.
        let (_text, brew, _r) = node_with_tap_dep_world("");
        assert!(!name_install_order(&brew).is_empty());
        assert_soak_tap_commands(&brew, &["node"]);
    }

    #[test]
    fn failed_brew_update_is_retried_by_the_no_soak_step() {
        let (brew, git, cfg) = soaked_and_no_soak_world();
        brew.next_outputs
            .lock()
            .unwrap()
            .push_back((1, b"Error: offline".to_vec()));
        let inv = inv_from(&brew, &cfg);
        upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git,
            &core_snaps(),
            test_cache(),
            tempfile::tempdir().unwrap().path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        let updates = lock_runs(&brew)
            .iter()
            .filter(|a| a.as_slice() == ["update"])
            .count();
        assert_eq!(updates, 2, "{:?}", lock_runs(&brew));
    }

    #[test]
    fn dry_run_still_runs_brew_update() {
        let (brew, _, _) = named_left_run(Vec::new(), &["--dry-run".to_string()]);
        let visible = brew.visible_runs.lock().unwrap().clone();
        assert_eq!(
            visible.first().map(Vec::as_slice),
            Some(&["update".to_string()][..])
        );
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
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git_empty(),
            &core_snaps(),
            test_cache(),
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
            summary_has(&text, "pinned", "1") && summary_has(&text, "no-soak", "1"),
            "{text}"
        );
        // Explicitly named: handed to brew, which errors as it would by hand.
        let mut out = Vec::new();
        upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git_empty(),
            &core_snaps(),
            test_cache(),
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
        let inv = Inventory::build(
            brew.installed.clone(),
            &brew.tap_info().expect("mock taps"),
            &origins,
            &cfg,
        );
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git_empty(),
            &core_snaps(),
            test_cache(),
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
        let inv = Inventory::build(
            brew.installed.clone(),
            &brew.tap_info().expect("mock taps"),
            &origins,
            cfg,
        );
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &crate::lock::RunLock::for_tests(),
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
        let inv = Inventory::build(
            brew.installed.clone(),
            &brew.tap_info().expect("mock taps"),
            &origins,
            &cfg,
        );
        let tap = tempfile::tempdir().unwrap();
        upgrade(
            &crate::lock::RunLock::for_tests(),
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
            &crate::lock::RunLock::for_tests(),
            &brew,
            &git_empty(),
            &core_snaps(),
            test_cache(),
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
        assert!(summary_has(&text, "held", "1"), "{text}");
    }

    /// Run `verb` for `wget` (installed at its cutoff) against `tap` and the
    /// per-thread test cache, with `flags`.
    fn run_verb(verb: &str, tap: &Path, flags: &[String]) {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", formula_rb("wget", "1.1.0", "midsha"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let lock = crate::lock::RunLock::for_tests();
        let names = ["wget".to_string()];
        let mut out = Vec::new();
        match verb {
            "upgrade" => upgrade(
                &lock,
                &brew,
                &git,
                &core_snaps(),
                test_cache(),
                tap,
                &inv,
                &cfg,
                &names,
                flags,
                &mut out,
            ),
            "install" => crate::cmd::install::install(
                &lock,
                &brew,
                &git,
                &core_snaps(),
                test_cache(),
                tap,
                &inv,
                &cfg,
                &names,
                false,
                false,
                flags,
                &mut out,
            ),
            "reinstall" => reinstall(
                &lock,
                &brew,
                &git,
                &core_snaps(),
                test_cache(),
                tap,
                &inv,
                &cfg,
                &names,
                flags,
                &mut out,
            ),
            other => panic!("unknown verb {other}"),
        }
        .unwrap_or_else(|e| panic!("{verb}: {e}"));
    }

    #[test]
    fn real_upgrade_install_and_reinstall_maintain_the_tap_first() {
        use crate::cmd::test_support::{seed_stale_tap, tree};
        for verb in ["upgrade", "install", "reinstall"] {
            let tap = tempfile::tempdir().unwrap();
            seed_stale_tap(tap.path(), test_cache());
            run_verb(verb, tap.path(), &[]);
            assert!(
                tree(tap.path()).is_empty(),
                "{verb}: {:?}",
                tree(tap.path())
            );
            assert!(
                tree(&test_cache().join(crate::soaktap::UNDO_DIR)).is_empty(),
                "{verb}: undo entry survived"
            );
        }
    }

    #[test]
    fn dry_run_never_maintains_the_tap() {
        use crate::cmd::test_support::{seed_stale_tap, tree};
        let undo = test_cache().join(crate::soaktap::UNDO_DIR);
        for verb in ["upgrade", "install", "reinstall"] {
            let tap = tempfile::tempdir().unwrap();
            seed_stale_tap(tap.path(), test_cache());
            let (tap_before, undo_before) = (tree(tap.path()), tree(&undo));
            assert_eq!(tap_before.len(), 2);
            assert_eq!(undo_before.len(), 1);
            run_verb(verb, tap.path(), &["--dry-run".to_string()]);
            assert_eq!(tree(tap.path()), tap_before, "{verb}");
            assert_eq!(tree(&undo), undo_before, "{verb}");
        }
    }

    mod soak_tap_tests {
        use super::*;
        use crate::cmd::combine_exit;
        use crate::cmd::test_support::{
            assert_soak_tap_commands, run_has_token, run_upgrade, soak_and_packer_world,
            soak_tap_at, soak_world, summary_has, tap_file, tree,
        };
        use crate::origin::SOAK_TAP;
        use crate::soaktap::{NotReady, UNDO_DIR};
        use crate::taps::TapInfo;

        fn cutoff_alpha() -> String {
            formula_rb("alpha", "1.1.0", "midsha")
        }

        fn undo_tree() -> BTreeMap<std::path::PathBuf, Vec<u8>> {
            tree(&test_cache().join(UNDO_DIR))
        }

        fn visible(brew: &MockBrew) -> Vec<Vec<String>> {
            brew.visible_runs.lock().unwrap().clone()
        }

        fn strs(args: &[&str]) -> Vec<String> {
            args.iter().map(|a| a.to_string()).collect()
        }

        fn tap_news(brew: &MockBrew) -> usize {
            lock_runs(brew)
                .iter()
                .filter(|a| a.first().map(String::as_str) == Some("tap-new"))
                .count()
        }

        #[test]
        fn not_tapped_real_run_creates_the_tap_then_holds() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git, snaps) = soak_and_packer_world(tap.path());
            brew.soak_tap = None;
            let (r, text) = run_upgrade(
                &brew,
                &git,
                &snaps,
                tap.path(),
                &["alpha", "hashicorp/tap/packer"],
                &[],
            );
            assert_eq!(tap_news(&brew), 1, "{:?}", lock_runs(&brew));
            assert!(
                text.contains("held alpha: brewsoak's tap is not ready"),
                "{text}"
            );
            assert_eq!(
                text.matches(&NotReady::Untrusted.note()).count(),
                1,
                "{text}"
            );
            assert!(visible(&brew).contains(&strs(&["install", "--formula", "packer"])));
            assert_eq!(combine_exit(r.refused, r.brew_status), 1);
            assert_soak_tap_commands(&brew, &[]);
        }

        #[test]
        fn not_tapped_dry_run_creates_nothing() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git, snaps) = soak_and_packer_world(tap.path());
            brew.soak_tap = None;
            let (_, text) =
                run_upgrade(&brew, &git, &snaps, tap.path(), &["alpha"], &["--dry-run"]);
            assert_eq!(tap_news(&brew), 0);
            assert!(text.contains(&NotReady::DryRunNotTapped.note()), "{text}");
            assert_soak_tap_commands(&brew, &[]);
        }

        #[test]
        fn remote_soak_tap_is_held_and_never_used() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git, snaps) = soak_and_packer_world(tap.path());
            let remote = "https://example.com/x/homebrew-brewsoak";
            brew.soak_tap = Some(TapInfo::new(SOAK_TAP, Some(remote)));
            let (_, text) = run_upgrade(&brew, &git, &snaps, tap.path(), &["alpha"], &[]);
            assert!(
                text.contains(&NotReady::Remote(remote.into()).note()),
                "{text}"
            );
            assert!(
                text.contains("held alpha: brewsoak's tap is not ready"),
                "{text}"
            );
            assert!(!run_has_token(&lock_runs(&brew), "ericfitz/brewsoak/alpha"));
            assert_soak_tap_commands(&brew, &[]);
        }

        #[test]
        fn untrusted_soak_tap_holds_every_soak_tap_package_with_one_note() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git) = two_outdated_world("");
            brew.soak_tap = Some(soak_tap_at(tap.path(), false));
            let (_, text) = run_upgrade(&brew, &git, &core_snaps(), tap.path(), &[], &[]);
            assert!(
                text.contains("held left: brewsoak's tap is not ready"),
                "{text}"
            );
            assert!(
                text.contains("held right: brewsoak's tap is not ready"),
                "{text}"
            );
            assert_eq!(
                text.matches(&NotReady::Untrusted.note()).count(),
                1,
                "{text}"
            );
            assert!(summary_has(&text, "held", "2"), "{text}");
            assert!(summary_has(&text, "upgraded", "0"), "{text}");
            assert_soak_tap_commands(&brew, &[]);
        }

        #[test]
        fn failed_tap_new_is_a_hold_not_an_error() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git) = soak_world(tap.path());
            brew.soak_tap = None;
            brew.tap_new_error = Some("Error: nope".into());
            let (r, text) = run_upgrade(&brew, &git, &core_snaps(), tap.path(), &["alpha"], &[]);
            assert!(r.refused, "{text}");
            assert!(
                text.contains(&NotReady::CreateFailed("Error: nope".into()).note()),
                "{text}"
            );
        }

        #[test]
        fn tap_info_without_a_trusted_field_is_used() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git) = soak_world(tap.path());
            brew.soak_tap = None;
            brew.taps.extend(
                crate::taps::parse_tap_info_json(r#"[{"name":"ericfitz/brewsoak","remote":null}]"#)
                    .unwrap(),
            );
            let (r, text) = run_upgrade(&brew, &git, &core_snaps(), tap.path(), &["alpha"], &[]);
            assert!(!r.refused, "{text}");
            assert!(
                visible(&brew).contains(&strs(&[
                    "upgrade",
                    "--formula",
                    "ericfitz/brewsoak/alpha"
                ])),
                "{:?}",
                visible(&brew)
            );
        }

        fn run_verbs(
            brew: &MockBrew,
            git: &InMemoryGit,
            tap: &Path,
            verb: &str,
            cask: bool,
            name: &str,
        ) {
            let cfg = cfg24();
            let inv = inv_from(brew, &cfg);
            let lock = crate::lock::RunLock::for_tests();
            let names = [name.to_string()];
            let snaps = core_snaps();
            let mut out = Vec::new();
            match verb {
                "upgrade" => upgrade(
                    &lock,
                    brew,
                    git,
                    &snaps,
                    test_cache(),
                    tap,
                    &inv,
                    &cfg,
                    &names,
                    &[],
                    &mut out,
                ),
                "install" => crate::cmd::install::install(
                    &lock,
                    brew,
                    git,
                    &snaps,
                    test_cache(),
                    tap,
                    &inv,
                    &cfg,
                    &names,
                    cask,
                    false,
                    &[],
                    &mut out,
                ),
                "reinstall" => reinstall(
                    &lock,
                    brew,
                    git,
                    &snaps,
                    test_cache(),
                    tap,
                    &inv,
                    &cfg,
                    &names,
                    &[],
                    &mut out,
                ),
                other => panic!("{other}"),
            }
            .unwrap_or_else(|e| panic!("{verb}: {e}"));
        }

        #[test]
        fn soak_tap_verb_follows_the_installed_state_for_every_brewsoak_verb() {
            let want = strs(&["upgrade", "--formula", "ericfitz/brewsoak/alpha"]);
            for verb in ["upgrade", "install", "reinstall"] {
                let tap = tempfile::tempdir().unwrap();
                let (brew, git) = soak_world(tap.path());
                run_verbs(&brew, &git, tap.path(), verb, false, "alpha");
                assert!(
                    visible(&brew).contains(&want),
                    "{verb}: {:?}",
                    visible(&brew)
                );
            }
            // An absent formula installs.
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git) = soak_world(tap.path());
            brew.installed.retain(|p| p.name != "alpha");
            run_verbs(&brew, &git, tap.path(), "install", false, "alpha");
            assert!(
                visible(&brew).contains(&strs(&[
                    "install",
                    "--formula",
                    "ericfitz/brewsoak/alpha"
                ])),
                "{:?}",
                visible(&brew)
            );
            // A cask always installs, installed or not.
            let git = InMemoryGit::new();
            git.insert_blob("caskcut", "Casks/f/firefox.rb", cask_rb("firefox", "1.0"));
            git.insert_blob("caskhead", "Casks/f/firefox.rb", cask_rb("firefox", "1.1"));
            let cask_cmd = strs(&["install", "--cask", "ericfitz/brewsoak/firefox"]);
            let tap = tempfile::tempdir().unwrap();
            let mut brew = MockBrew::new();
            brew.soak_tap = Some(soak_tap_at(tap.path(), true));
            run_verbs(&brew, &git, tap.path(), "install", true, "firefox");
            assert!(visible(&brew).contains(&cask_cmd), "{:?}", visible(&brew));
            let tap = tempfile::tempdir().unwrap();
            let mut brew = MockBrew::new();
            brew.soak_tap = Some(soak_tap_at(tap.path(), true));
            brew.installed.push(cask_pkg_from(
                "firefox",
                "homebrew/cask",
                crate::brew::version_only_cask_receipt("firefox", "0.9"),
            ));
            run_verbs(&brew, &git, tap.path(), "upgrade", true, "firefox");
            assert!(visible(&brew).contains(&cask_cmd), "{:?}", visible(&brew));
        }

        fn files_seen_by_brew(blob: &str) -> Vec<Option<Vec<u8>>> {
            let tap = tempfile::tempdir().unwrap();
            let (brew, git) = soak_world(tap.path());
            git.insert_blob("cutoffsha", "Formula/a/alpha.rb", blob.to_string());
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let (seen_in, dir) = (seen.clone(), tap.path().to_path_buf());
            *brew.on_call.lock().unwrap() = Some(Box::new(move |args: &[String]| {
                let asks_alpha = args.last().is_some_and(|a| a == "ericfitz/brewsoak/alpha");
                let verb = args.first().map(String::as_str);
                let deps = verb == Some("deps");
                if asks_alpha && (deps || verb == Some("upgrade")) {
                    seen_in
                        .lock()
                        .unwrap()
                        .push(std::fs::read(dir.join("Formula/alpha.rb")).ok());
                }
            }));
            let (_, text) = run_upgrade(&brew, &git, &core_snaps(), tap.path(), &["alpha"], &[]);
            assert!(!text.contains("held alpha"), "{text}");
            seen.lock().unwrap().clone()
        }

        #[test]
        fn tap_holds_the_cutoff_file_when_brew_is_asked() {
            let with_stanza = format!(
                "{}  no_autobump! because: :bumped_by_upstream\n",
                cutoff_alpha().trim_end_matches("end\n")
            ) + "end\n";
            assert!(with_stanza.contains("no_autobump!"));
            let seen = files_seen_by_brew(&with_stanza);
            assert_eq!(seen.len(), 2, "deps and upgrade: {seen:?}");
            let want = crate::tap::sanitize_unofficial(with_stanza.as_bytes());
            assert_ne!(want, with_stanza.as_bytes());
            for file in &seen {
                assert_eq!(file.as_deref(), Some(want.as_slice()));
            }
            // No stanza: the file is the blob, byte for byte.
            let plain = cutoff_alpha();
            for file in files_seen_by_brew(&plain) {
                assert_eq!(file.as_deref(), Some(plain.as_bytes()));
            }
        }

        #[test]
        fn third_party_file_is_never_written_to_the_tap() {
            let tap = tempfile::tempdir().unwrap();
            let (brew, git, snaps) = soak_and_packer_world(tap.path());
            run_upgrade(
                &brew,
                &git,
                &snaps,
                tap.path(),
                &["hashicorp/tap/packer"],
                &[],
            );
            assert!(!tap.path().join("Formula").exists());
            assert_eq!(
                visible(&brew)[1..],
                [strs(&["install", "--formula", "packer"])]
            );
            assert_soak_tap_commands(&brew, &[]);
        }

        #[test]
        fn run_without_soak_tap_work_never_creates_the_tap() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git, snaps) = soak_and_packer_world(tap.path());
            brew.soak_tap = None;
            brew.installed.retain(|p| p.name != "alpha");
            let (r, text) = run_upgrade(&brew, &git, &snaps, tap.path(), &[], &[]);
            assert_eq!(tap_news(&brew), 0, "{:?}", lock_runs(&brew));
            assert!(!r.refused, "{text}");
            assert!(visible(&brew).contains(&strs(&["install", "--formula", "packer"])));
        }

        fn run_with_origin_record(flags: &[&str], next_stdout: &str) -> Option<String> {
            let tap = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let (mut brew, git) = soak_world(tap.path());
            brew.next_stdout = next_stdout.as_bytes().to_vec();
            let mut records = OriginRecords::default();
            records.set(PkgKind::Formula, "alpha", "acme/tools");
            records.save(cache.path()).unwrap();
            let cfg = cfg24();
            let inv = inv_from(&brew, &cfg);
            let flags: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
            upgrade(
                &crate::lock::RunLock::for_tests(),
                &brew,
                &git,
                &core_snaps(),
                cache.path(),
                tap.path(),
                &inv,
                &cfg,
                &["alpha".to_string()],
                &flags,
                &mut Vec::new(),
            )
            .unwrap();
            OriginRecords::load(cache.path())
                .get(PkgKind::Formula, "alpha")
                .map(str::to_string)
        }

        #[test]
        fn real_install_from_the_tap_removes_a_stale_origin_record() {
            assert_eq!(run_with_origin_record(&[], ""), None);
            assert_eq!(
                run_with_origin_record(&["--dry-run"], "").as_deref(),
                Some("acme/tools")
            );
            assert_eq!(
                run_with_origin_record(&[], "Warning: alpha 1.1.0 already installed\n").as_deref(),
                Some("acme/tools")
            );
        }

        fn run_alpha(
            next_status: i32,
            next_stdout: &str,
            receipt: Option<&str>,
        ) -> (tempfile::TempDir, String) {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git) = soak_world(tap.path());
            brew.next_status = next_status;
            brew.next_stdout = next_stdout.as_bytes().to_vec();
            if let Some(r) = receipt {
                brew.receipt_taps
                    .insert((PkgKind::Formula, "alpha".into(), "1.1.0".into()), r.into());
            }
            let (_, text) = run_upgrade(&brew, &git, &core_snaps(), tap.path(), &["alpha"], &[]);
            (tap, text)
        }

        #[test]
        fn tap_file_is_kept_after_a_real_install() {
            let (tap, text) = run_alpha(0, "", Some("ericfitz/brewsoak"));
            assert_eq!(
                tap_file(tap.path(), "Formula/alpha.rb"),
                Some(cutoff_alpha().into_bytes()),
                "{text}"
            );
            assert!(undo_tree().is_empty(), "{:?}", undo_tree());
        }

        #[test]
        fn tap_file_is_put_back_after_a_brew_failure_without_a_new_keg() {
            let (tap, _) = run_alpha(1, "", None);
            assert_eq!(tap_file(tap.path(), "Formula/alpha.rb"), None);
            assert!(undo_tree().is_empty(), "{:?}", undo_tree());
        }

        #[test]
        fn tap_file_is_put_back_after_already_installed_over_another_taps_keg() {
            let (tap, _) = run_alpha(
                0,
                "Warning: alpha 1.1.0 already installed\n",
                Some("homebrew/core"),
            );
            assert_eq!(tap_file(tap.path(), "Formula/alpha.rb"), None);
            assert!(undo_tree().is_empty(), "{:?}", undo_tree());
        }

        #[test]
        fn tap_file_is_kept_when_brew_fails_but_the_new_keg_exists() {
            let (tap, _) = run_alpha(1, "", Some("ericfitz/brewsoak"));
            assert_eq!(
                tap_file(tap.path(), "Formula/alpha.rb"),
                Some(cutoff_alpha().into_bytes())
            );
            assert!(undo_tree().is_empty(), "{:?}", undo_tree());
        }

        /// `alpha` with the missing dependency `libx`, whose own dependency
        /// listing fails, so `alpha` is held after both files were staged.
        fn alpha_with_libx(tap: &Path) -> (MockBrew, InMemoryGit) {
            let (mut brew, git) = soak_world(tap);
            git.insert_blob(
                "cutoffsha",
                "Formula/lib/libx.rb",
                formula_rb("libx", "1.1.0", "midsha"),
            );
            git.insert_blob(
                "headsha",
                "Formula/lib/libx.rb",
                formula_rb("libx", "1.2.0", "newsha"),
            );
            brew.deps.insert("alpha".into(), vec!["libx".into()]);
            (brew, git)
        }

        #[test]
        fn tap_files_are_put_back_after_a_hold_and_after_a_dry_run() {
            // A hold after both files were staged.
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git) = alpha_with_libx(tap.path());
            brew.deps_errors
                .insert("libx".into(), "Error: libx: boom".into());
            let before = tree(tap.path());
            let (_, text) = run_upgrade(&brew, &git, &core_snaps(), tap.path(), &["alpha"], &[]);
            assert!(text.contains("held alpha: dependency libx"), "{text}");
            assert!(brew.deps_calls.lock().unwrap().len() >= 2);
            assert_eq!(tree(tap.path()), before);
            assert!(undo_tree().is_empty(), "{:?}", undo_tree());
            // A dry run stages both and puts both back; every command is dry.
            let tap = tempfile::tempdir().unwrap();
            let (brew, git) = alpha_with_libx(tap.path());
            let before = tree(tap.path());
            run_upgrade(
                &brew,
                &git,
                &core_snaps(),
                tap.path(),
                &["alpha"],
                &["--dry-run"],
            );
            let soak: Vec<Vec<String>> = visible(&brew)
                .into_iter()
                .filter(|a| {
                    a.last()
                        .is_some_and(|l| l.starts_with("ericfitz/brewsoak/"))
                })
                .collect();
            assert_eq!(soak.len(), 2, "{soak:?}");
            for args in &soak {
                assert!(args.iter().any(|a| a == "--dry-run"), "{args:?}");
            }
            assert_eq!(tree(tap.path()), before);
            assert!(undo_tree().is_empty(), "{:?}", undo_tree());
        }

        #[test]
        fn dry_run_meeting_a_leftover_undo_entry_holds_that_target() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git) = two_outdated_world("");
            brew.soak_tap = Some(soak_tap_at(tap.path(), true));
            for (dir, file, bytes) in [
                (tap.path().join("Formula"), "left.rb", "stale"),
                (
                    test_cache().join(UNDO_DIR).join("Formula"),
                    "left.rb",
                    "previous",
                ),
            ] {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(file), bytes).unwrap();
            }
            let (tap_before, undo_before) = (tree(tap.path()), undo_tree());
            let (r, text) =
                run_upgrade(&brew, &git, &core_snaps(), tap.path(), &[], &["--dry-run"]);
            assert!(r.refused, "{text}");
            assert!(
                text.contains("held left: an earlier brewsoak run was interrupted"),
                "{text}"
            );
            assert!(
                text.contains(
                    "left: an earlier brewsoak run was interrupted; run brewsoak update to finish it"
                ),
                "{text}"
            );
            assert!(
                visible(&brew)
                    .iter()
                    .any(|a| a.last().map(String::as_str) == Some("ericfitz/brewsoak/right")),
                "{:?}",
                visible(&brew)
            );
            assert_eq!(
                tree(tap.path()).get(&tap.path().join("Formula/left.rb")),
                tap_before.get(&tap.path().join("Formula/left.rb"))
            );
            assert_eq!(undo_tree(), undo_before);
        }

        #[test]
        fn second_run_writes_nothing() {
            let tap = tempfile::tempdir().unwrap();
            let (mut brew, git) = soak_world(tap.path());
            brew.installed[0] = InstalledPkg {
                own_tap: Some(crate::origin::OwnTap::Soak),
                ..formula_pkg("alpha", cutoff_alpha())
            };
            std::fs::create_dir_all(tap.path().join("Formula")).unwrap();
            std::fs::write(tap.path().join("Formula/alpha.rb"), cutoff_alpha()).unwrap();
            let before = tree(tap.path());
            run_upgrade(&brew, &git, &core_snaps(), tap.path(), &["alpha"], &[]);
            let commands: Vec<Vec<String>> = lock_runs(&brew)
                .into_iter()
                .filter(|a| a.iter().any(|x| x.contains("alpha")))
                .collect();
            assert!(commands.is_empty(), "{commands:?}");
            assert_eq!(tree(tap.path()), before);
            assert!(undo_tree().is_empty());
        }

        #[test]
        fn recovery_and_pruning_happen_before_any_stage_or_install() {
            use crate::cmd::test_support::seed_stale_tap;
            let tap = tempfile::tempdir().unwrap();
            let (brew, git) = soak_world(tap.path());
            seed_stale_tap(tap.path(), test_cache());
            let snapshot = std::sync::Arc::new(std::sync::Mutex::new(None));
            let (snap_in, dir) = (snapshot.clone(), tap.path().to_path_buf());
            *brew.on_call.lock().unwrap() = Some(Box::new(move |args: &[String]| {
                let alpha = args.last().is_some_and(|a| a == "ericfitz/brewsoak/alpha");
                if alpha && args.first().map(String::as_str) == Some("upgrade") {
                    *snap_in.lock().unwrap() = Some((
                        dir.join("Formula/back.rb").exists(),
                        dir.join("Formula/gone.rb").exists(),
                        dir.join("Formula/alpha.rb").exists(),
                        test_cache()
                            .join(UNDO_DIR)
                            .join("Formula/back.absent")
                            .exists(),
                        test_cache()
                            .join(UNDO_DIR)
                            .join("Formula/alpha.rb")
                            .exists()
                            || test_cache()
                                .join(UNDO_DIR)
                                .join("Formula/alpha.absent")
                                .exists(),
                    ));
                }
            }));
            run_upgrade(&brew, &git, &core_snaps(), tap.path(), &["alpha"], &[]);
            let seen = snapshot.lock().unwrap().expect("the soak-tap upgrade ran");
            // back.rb recovered away, gone.rb pruned, alpha staged with its
            // undo entry, and the old entry settled.
            assert_eq!(seen, (false, false, true, false, true));
        }
    }
}
