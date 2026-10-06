use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::eligibility::{self, DesiredAction, UpstreamStatus};
use crate::git::GitStore;
use crate::github::GithubApi;
use crate::identity::{self, PkgIdentity};
use crate::inventory::{self, Inventory, Pkg, PkgClass};
use crate::nosoak;
use crate::origin::{self, OriginRecords};
use crate::quiet;
use crate::report::{self, Counts};
use crate::resolve::{self, PkgKind, PkgRef};
use crate::snapshot::{self, RefreshPlan, Snapshots, TapPlan};
use crate::tap;
use crate::taps;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn refusal_message(action: DesiredAction, name: &str, brew_verb: &str) -> Option<String> {
    let why = match action {
        DesiredAction::RefuseTooNew => format!("{name} is too new (born inside the soak window)"),
        DesiredAction::RefuseYanked => format!("{name} is missing at HEAD (yanked)"),
        DesiredAction::RefuseDeprecated => format!("{name} is deprecated or disabled at HEAD"),
        DesiredAction::NoOpAlreadySoaked
        | DesiredAction::LeaveAheadOfSoak
        | DesiredAction::LeaveAutoUpdates
        | DesiredAction::InstallCutoff => return None,
    };
    Some(format!(
        "{why}; use `brew {brew_verb} {name}` to bypass brewsoak."
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

/// `(origin, name)` of every `user/tap/name` token on the command line.
pub fn explicit_tap_tokens(names: &[String]) -> Vec<(String, String)> {
    names
        .iter()
        .filter_map(|n| {
            let token = inventory::parse_token(n).ok()?;
            token.origin.map(|o| (o, token.name))
        })
        .collect()
}

pub fn refresh_plan(cfg: &Config, inv: &Inventory, extra: &[(String, String)]) -> RefreshPlan {
    RefreshPlan {
        hours: cfg.hours,
        core_hours: cfg.effective_hours(origin::CORE),
        cask_hours: cfg.effective_hours(origin::CASK),
        taps: inv
            .needed_taps(extra, cfg)
            .into_iter()
            .map(|(name, remote)| TapPlan {
                hours: cfg.effective_hours(&name),
                name,
                remote,
            })
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn ensure_snapshots(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    cfg: &Config,
    inv: &Inventory,
    extra: &[(String, String)],
    now: time::OffsetDateTime,
    force: bool,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    let plan = refresh_plan(cfg, inv, extra);
    let snaps = if force {
        snapshot::refresh_with(git, gh, cache, &plan, now, progress)?
    } else {
        match snapshot::load_state(cache)? {
            Some(mut s) => {
                // outdated/info reuse the stored snapshot, but refresh any
                // tap with no state or a different soak window.
                let stale: Vec<TapPlan> = plan
                    .taps
                    .iter()
                    .filter(|t| snapshot::tap_needs_refresh(&s, &t.name, t.hours))
                    .cloned()
                    .collect();
                if !stale.is_empty() {
                    snapshot::refresh_taps(git, cache, &mut s, &stale, now, progress)?;
                }
                return Ok(s);
            }
            None => snapshot::refresh_with(git, gh, cache, &plan, now, progress)?,
        }
    };
    prefetch_installed(git, cache, inv, &snaps);
    Ok(snaps)
}

type Classified = (
    String,
    DesiredAction,
    Option<PkgIdentity>,
    Option<PkgIdentity>,
    Option<PkgIdentity>,
);

/// Installed soaked packages whose origin has a snapshot in this run. A tap
/// that failed to refresh (or has no state) contributes nothing.
fn snapshotted_pkgs<'a>(inv: &'a Inventory, snaps: &'a Snapshots) -> impl Iterator<Item = &'a Pkg> {
    inv.pkgs.iter().filter(|p| {
        p.class == PkgClass::Soaked
            && (origin::is_core_or_cask(&p.origin) || snaps.tap(&p.origin).is_some())
    })
}

fn classify_installed(
    git: &impl GitStore,
    cache: &Path,
    snaps: &Snapshots,
    inv: &Inventory,
) -> Result<Vec<Classified>, Error> {
    let mut out = Vec::new();
    for pkg in snapshotted_pkgs(inv, snaps) {
        let Some(mut view) = resolve_view(
            git,
            snaps,
            cache,
            &pkg.origin,
            &pkg.name,
            pkg.kind,
            Some(&pkg.receipt_rb),
        )?
        else {
            continue;
        };
        view.action = bare_action(&view);
        out.push((
            pkg.name.clone(),
            view.action,
            view.installed,
            view.cutoff,
            view.head,
        ));
    }
    Ok(out)
}

fn write_update_summary(
    git: &impl GitStore,
    cache: &Path,
    snaps: &Snapshots,
    inv: &Inventory,
    before: &[Classified],
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    let after = classify_installed(git, cache, snaps, inv)?;
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
                DesiredAction::LeaveAutoUpdates => "updates itself",
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

pub fn prefetch_installed(git: &impl GitStore, cache: &Path, inv: &Inventory, snaps: &Snapshots) {
    for pkg in snapshotted_pkgs(inv, snaps) {
        let _ = resolve_pkg_blobs(git, snaps, cache, &pkg.origin, &pkg.name, pkg.kind);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn update(
    brew: &impl Brew,
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    cfg: &Config,
    inv: &Inventory,
    now: time::OffsetDateTime,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    writeln!(
        out,
        "updating soak snapshots; soak window {}h",
        cfg.hours.get()
    )?;
    let previous = snapshot::load_state(cache)?;
    let before = match &previous {
        Some(prev) => classify_installed(git, cache, prev, inv).unwrap_or_default(),
        None => Vec::new(),
    };
    let plan = refresh_plan(cfg, inv, &[]);
    let snaps = snapshot::refresh_with(git, gh, cache, &plan, now, out)?;
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
    for (name, state) in &snaps.taps {
        match &state.cutoff_sha {
            Some(sha) => writeln!(
                out,
                "{name} cutoff: {}",
                report::format_cutoff(sha, state.cutoff_time)
            )?,
            None => writeln!(
                out,
                "{name} cutoff: none (no commit is older than the soak window)"
            )?,
        }
        writeln!(out, "{name} head: {}", report::short_sha(&state.head_sha))?;
    }
    for (name, err) in &snaps.held_taps {
        writeln!(
            out,
            "tap {name} could not be refreshed; its packages are held.\n{}",
            indented_detail(err)
        )?;
    }
    prefetch_installed(git, cache, inv, &snaps);
    write_update_summary(git, cache, &snaps, inv, &before, verbose, out)?;
    if inv.any_no_soak() {
        writeln!(out, "no-soak packages installed; updating brew")?;
        let output = brew.run_visible(&["update".to_string()])?;
        write_session_tail(&brew.session_report(), out)?;
        if !output.status.success() {
            let status = output.status.code().unwrap_or(1);
            writeln!(out, "brew update failed (exit {status})")?;
            writeln!(out, "snapshots refreshed")?;
            return Err(Error::Brew {
                status,
                message: String::new(),
            });
        }
    }
    writeln!(out, "snapshots refreshed")?;
    Ok(())
}

/// Why a package cannot be soaked, worded for a note.
fn unsoakable_note(name: &str, origin_tap: &str, tapped: bool) -> String {
    if tapped {
        format!(
            "{name}: tap {origin_tap} has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK"
        )
    } else {
        format!(
            "{name}: tap {origin_tap} is not installed; run brew tap {origin_tap} first, or add it to NO_SOAK"
        )
    }
}

/// Detail text (often git's multi-line stderr) on its own lines, each
/// indented two spaces so it reads as belonging to the line above it.
fn indented_detail(detail: &str) -> String {
    detail
        .lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn held_tap_note(name: &str, origin_tap: &str, err: &str) -> String {
    format!(
        "{name}: tap {origin_tap} could not be refreshed; held.\n{}",
        indented_detail(err)
    )
}

/// origin + name + class for a command-line token, installed or not.
struct Resolved {
    origin: String,
    name: String,
    class: PkgClass,
    receipt_tap: Option<String>,
    /// origin is core/cask or appears in `brew tap-info --installed`.
    tapped: bool,
    /// The token spelled out `user/tap/name`.
    named_origin: bool,
}

fn resolve_token(raw: &str, inv: &Inventory, cfg: &Config) -> Result<Resolved, Error> {
    let token = inventory::parse_token(raw)?;
    let installed = match &token.origin {
        Some(o) => inv.find_in(o, &token.name),
        None => inv.find(&token.name),
    };
    let origin_tap = match (&token.origin, installed) {
        (Some(o), _) => o.clone(),
        (None, Some(p)) => p.origin.clone(),
        // Not installed, bare name: core. `natural_kind` may flip it to cask.
        (None, None) => origin::CORE.to_string(),
    };
    let tapped = origin::is_core_or_cask(&origin_tap) || inv.tap_class(&origin_tap).is_some();
    let class = match installed {
        Some(p) if token.origin.is_none() => p.class,
        _ => inv.class_for(&origin_tap, &token.name, cfg),
    };
    Ok(Resolved {
        origin: origin_tap,
        name: token.name,
        class,
        receipt_tap: installed.and_then(|p| p.receipt_tap.clone()),
        tapped,
        named_origin: token.origin.is_some(),
    })
}

/// Reject a malformed explicit token before anything is installed, so a bad
/// name late in the list cannot cut a run short after side effects.
fn validate_tokens(names: &[String]) -> Result<(), Error> {
    names
        .iter()
        .try_for_each(|raw| inventory::parse_token(raw).map(drop))
}

/// A bare name that resolved as a cask belongs to `homebrew/cask`, which may
/// itself be on the `NO_SOAK` list.
fn settle_origin(r: &mut Resolved, kind: PkgKind, inv: &Inventory, cfg: &Config) {
    if origin::is_core_or_cask(&r.origin) {
        r.origin = origin::default_origin(kind).to_string();
        r.class = inv.class_for(&r.origin, &r.name, cfg);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn outdated(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
    cfg: &Config,
    extra_args: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
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
    let mut upgrades = Vec::new();
    let mut held = Vec::new();
    let mut ahead = Vec::new();
    let mut auto_updates = Vec::new();
    let mut pinned = Vec::new();
    let mut soaked = 0usize;
    let mut no_soak_names = Vec::new();
    for pkg in &inv.pkgs {
        if verbose {
            writeln!(
                out,
                "{}",
                report::origin_line(
                    &pkg.name,
                    &pkg.origin,
                    cfg.effective_hours(&pkg.origin),
                    pkg.class
                )
            )?;
        }
        match pkg.class {
            PkgClass::NoSoak => {
                if pkg.pinned {
                    pinned.push(pkg.name.clone());
                } else {
                    no_soak_names.push(pkg.name.clone());
                }
                if verbose {
                    writeln!(out, "{}: no-soak; brew decides", pkg.name)?;
                }
                continue;
            }
            PkgClass::Unsoakable => {
                held.push(unsoakable_note(
                    &pkg.name,
                    &pkg.origin,
                    inv.tap_class(&pkg.origin).is_some(),
                ));
                continue;
            }
            PkgClass::Soaked => {}
        }
        if pkg.pinned {
            pinned.push(pkg.name.clone());
            if verbose {
                writeln!(out, "{}: pinned; skipped", pkg.name)?;
            }
            continue;
        }
        if let Some(err) = snaps.held_taps.get(&pkg.origin) {
            held.push(held_tap_note(&pkg.name, &pkg.origin, err));
            continue;
        }
        let Some(mut view) = resolve_view(
            git,
            snaps,
            cache,
            &pkg.origin,
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
        view.action = bare_action(&view);
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
                    .unwrap_or_else(|| "unknown".into());
                let cutoff_ver = view
                    .cutoff
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "none".into());
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
            DesiredAction::LeaveAutoUpdates => {
                let installed_ver = view
                    .installed
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "unknown".into());
                let cutoff_ver = view
                    .cutoff
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "none".into());
                auto_updates.push(format!("{} ({installed_ver}) < {cutoff_ver}", pkg.name));
            }
            DesiredAction::NoOpAlreadySoaked => soaked += 1,
        }
    }
    let brew_outdated = if no_soak_names.is_empty() {
        Vec::new()
    } else {
        brew.outdated_names()?
    };
    let no_soak: Vec<String> = no_soak_names
        .iter()
        .filter(|n| brew_outdated.contains(n))
        .map(|n| format!("{n} (no-soak, brew)"))
        .collect();
    write_section_always(out, "==> Outdated (will upgrade)", &upgrades)?;
    write_section_always(out, "==> No-soak (brew)", &no_soak)?;
    write_section_always(out, "==> Held", &held)?;
    write_section_always(out, "==> Ahead of soak", &ahead)?;
    write_section_always(
        out,
        "==> Auto-updates (left to the app; name it to upgrade)",
        &auto_updates,
    )?;
    write_section_always(out, "==> Pinned", &pinned)?;
    if upgrades.is_empty()
        && no_soak.is_empty()
        && held.is_empty()
        && ahead.is_empty()
        && auto_updates.is_empty()
        && pinned.is_empty()
    {
        writeln!(out, "nothing outdated (already soaked: {soaked})")?;
    }
    Ok(RunResult {
        refused: false,
        brew_status: None,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn info(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
    cfg: &Config,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let mut refused = false;
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
        inv.pkgs.iter().map(|p| p.name.clone()).collect()
    } else {
        names.to_vec()
    };
    for (i, raw) in owned_names.iter().enumerate() {
        if long_form && i > 0 {
            writeln!(out)?;
        }
        let mut r = resolve_token(raw, inv, cfg)?;
        let mut kind = None;
        if r.class == PkgClass::Soaked && !snaps.held_taps.contains_key(&r.origin) {
            let k = natural_kind(git, snaps, cache, inv, &r.origin, &r.name)?;
            settle_origin(&mut r, k, inv, cfg);
            kind = Some(k);
        }
        if verbose {
            writeln!(
                out,
                "{}",
                report::origin_line(&r.name, &r.origin, cfg.effective_hours(&r.origin), r.class)
            )?;
        }
        match r.class {
            PkgClass::NoSoak => {
                let installed = inv
                    .find_in(&r.origin, &r.name)
                    .and_then(|p| parse_pkg(p.kind, &p.receipt_rb).ok());
                let inst = installed.as_ref().map(report::identity_version);
                if long_form {
                    writeln!(out, "{raw}")?;
                    writeln!(
                        out,
                        "installed: {}",
                        inst.as_deref().unwrap_or("not installed")
                    )?;
                    writeln!(out, "origin: {}", r.origin)?;
                    writeln!(out, "soak: no-soak (brew decides)")?;
                    writeln!(out, "action: no-soak")?;
                } else {
                    writeln!(
                        out,
                        "{raw}  {}  no-soak (brew)",
                        inst.as_deref().unwrap_or("-")
                    )?;
                }
                continue;
            }
            PkgClass::Unsoakable => {
                if long_form {
                    writeln!(out, "{raw}")?;
                    writeln!(out, "origin: {}", r.origin)?;
                    writeln!(out, "action: unsoakable")?;
                } else {
                    writeln!(out, "{raw}  -  unsoakable")?;
                }
                if !names.is_empty() {
                    let token = nosoak::brew_token(&r.origin, &r.name);
                    writeln!(
                        out,
                        "{}; use `brew info {token}` to bypass brewsoak.",
                        unsoakable_note(&r.name, &r.origin, r.tapped)
                    )?;
                    refused = true;
                }
                continue;
            }
            PkgClass::Soaked => {}
        }
        if let Some(err) = snaps.held_taps.get(&r.origin) {
            writeln!(out, "{raw}")?;
            writeln!(out, "origin: {}", r.origin)?;
            writeln!(out, "{}", held_tap_note(&r.name, &r.origin, err))?;
            continue;
        }
        let kind = kind.expect("soaked packages have a kind");
        let receipt = inv
            .find_in(&r.origin, &r.name)
            .map(|p| p.receipt_rb.as_str());
        let Some(view) = resolve_view(git, snaps, cache, &r.origin, &r.name, kind, receipt)? else {
            if long_form {
                writeln!(out, "{raw}")?;
                writeln!(out, "unparseable identity")?;
            } else {
                writeln!(out, "{raw}  -  unparseable identity")?;
            }
            if verbose {
                writeln!(out, "{raw}: unparseable identity; skipped")?;
            }
            continue;
        };
        if verbose {
            writeln!(
                out,
                "{}",
                report::evaluate_line(
                    raw,
                    view.action,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.head.as_ref(),
                    "classified for info",
                )
            )?;
        }
        if long_form {
            writeln!(out, "{raw}")?;
            writeln!(
                out,
                "installed: {}",
                view.installed
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "not installed".into())
            )?;
            writeln!(
                out,
                "cutoff: {}",
                view.cutoff
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "none".into())
            )?;
            writeln!(
                out,
                "head: {}",
                view.head
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "none".into())
            )?;
            writeln!(out, "origin: {}", r.origin)?;
            writeln!(out, "soak hours: {}", cfg.effective_hours(&r.origin).get())?;
            writeln!(out, "action: {}", report::human_action(view.action))?;
            for warn in &view.warnings {
                writeln!(out, "warning: {warn}")?;
            }
        } else {
            writeln!(
                out,
                "{}",
                report::compact_info_line(
                    raw,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.action,
                )
            )?;
        }
    }
    Ok(RunResult {
        refused,
        brew_status: None,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn upgrade(
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
    // A bare `brew soak upgrade` walks everything installed, so say up front
    // how many will actually change and count them off as they go.
    let plan_total = (names.is_empty()).then(|| plan_size(git, snaps, cache, inv));
    if let Some(total) = plan_total {
        writeln!(out, "upgrading {total} of {} packages", inv.pkgs.len())?;
    }
    apply_many(
        brew,
        git,
        snaps,
        cache,
        tap_root,
        inv,
        cfg,
        names,
        names.is_empty(),
        "upgrade",
        false,
        false,
        user_flags,
        plan_total,
        out,
    )
}

/// Why a bare run does not evaluate an installed package, in the order the
/// run checks. `plan_size` and the bare loop both read this, so the `[i/N]`
/// total cannot drift from what the loop announces.
#[derive(Debug, PartialEq, Eq)]
enum BareSkip<'a> {
    Pinned,
    /// No-soak or unsoakable: brew's, or a note.
    NotSoaked,
    /// Its tap could not be refreshed this run; carries git's error.
    HeldTap(&'a str),
}

fn bare_skip<'a>(pkg: &Pkg, snaps: &'a Snapshots) -> Option<BareSkip<'a>> {
    if pkg.pinned {
        Some(BareSkip::Pinned)
    } else if pkg.class != PkgClass::Soaked {
        Some(BareSkip::NotSoaked)
    } else {
        snaps
            .held_taps
            .get(&pkg.origin)
            .map(|err| BareSkip::HeldTap(err))
    }
}

/// How many installed packages the soak window says to change. Resolution is
/// cheap (cached blobs) and errors just mean "no total to show".
fn plan_size(git: &impl GitStore, snaps: &Snapshots, cache: &Path, inv: &Inventory) -> usize {
    // Keep this in step with `apply_resolved`: every package counted here
    // must reach `announce` there, and none other.
    snapshotted_pkgs(inv, snaps)
        .filter(|pkg| bare_skip(pkg, snaps).is_none())
        .filter(|pkg| {
            matches!(
                resolve_view(
                    git,
                    snaps,
                    cache,
                    &pkg.origin,
                    &pkg.name,
                    pkg.kind,
                    Some(&pkg.receipt_rb)
                ),
                Ok(Some(view)) if bare_action(&view) == DesiredAction::InstallCutoff
            )
        })
        .count()
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    inv: &Inventory,
    cfg: &Config,
    names: &[String],
    force_cask: bool,
    force_formula: bool,
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    if names.is_empty() {
        return Err(Error::Usage("install: no packages specified".into()));
    }
    apply_many(
        brew,
        git,
        snaps,
        cache,
        tap_root,
        inv,
        cfg,
        names,
        false,
        "install",
        force_cask,
        force_formula,
        user_flags,
        None,
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
        counts: Counts::default(),
        done: BTreeMap::new(),
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
        let Some(pkg) = inv.find_in(&r.origin, &r.name) else {
            return Err(Error::Refusal(format!(
                "reinstall: no installed keg: {}",
                r.name
            )));
        };
        let Some(view) = resolve_view(
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
            session.note_dropped_flags("reinstall", "repair reinstalls");
            args.push(token);
            if session.record_run(&args)? {
                session.counts.upgraded += 1;
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
fn apply_many(
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
        counts: Counts::default(),
        done: BTreeMap::new(),
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

struct ApplySession<'a, B, G, W> {
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

    /// Say once which user flags `brew <verb>` would reject and we dropped.
    fn note_dropped_flags(&mut self, verb: &str, context: &str) {
        let dropped = crate::flags::filter_for_verb(verb, self.user_flags).dropped;
        if let Some(note) = crate::flags::dropped_note(verb, &dropped, context)
            && !self.deferred.contains(&note)
        {
            self.defer(note);
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
        match crate::identity::parse_formula(&rb) {
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
            self.git, self.snaps, self.cache, &r.origin, &name, kind, receipt,
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
        if self.bare_run {
            view.action = bare_action(&view);
        }
        let did = match view.action {
            DesiredAction::InstallCutoff => "installing cutoff",
            DesiredAction::NoOpAlreadySoaked => "left unchanged",
            DesiredAction::LeaveAheadOfSoak => "left unchanged",
            DesiredAction::LeaveAutoUpdates => "left to the app",
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => "refused",
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

        let args = tap::brew_install_args(&pkg, &path, self.user_flags);
        self.note_dropped_flags("install", "staged installs");
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
        let status = eligibility::upstream_status(
            blobs.cutoff.as_deref(),
            blobs.head.as_deref(),
            &calendar_today(),
        );
        if status != UpstreamStatus::Eligible {
            self.refuse_ineligible_dep(target, target_token, dep, status)?;
            return Ok(false);
        }
        let want = blobs
            .cutoff
            .as_deref()
            .and_then(|bytes| parse_pkg_bytes(kind, bytes).ok());
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
        let path = tap::write_blob(&staging_dir(self.tap_root, dep_origin), &pkg, blob)?;
        let args = tap::brew_install_args(&pkg, &path, &[]);
        // A dep that brew cannot load is held by name; the target is still
        // attempted and brew reports the missing dependency.
        match self.record_staged_install(dep_origin, dep, kind, &args)? {
            StagedRun::Ran { .. } => {}
            StagedRun::LoadFailure => self.hold_staged_copy(dep),
            StagedRun::Untrusted(tap) => self.hold_untrusted(dep, &tap),
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

    /// The target was counted as upgraded when it was announced; it is held
    /// instead, so take that count back.
    fn hold_target_untrusted(&mut self, name: &str, tap: &str) {
        self.counts.upgraded = self.counts.upgraded.saturating_sub(1);
        self.hold_untrusted(name, tap);
    }

    /// Tap packages are remembered by origin; core and cask need no record.
    fn record_origin(&mut self, origin_tap: &str, name: &str, kind: PkgKind) -> Result<(), Error> {
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
        for t in &step.switched {
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

    /// Runs brew and returns whether it succeeded.
    fn record_run(&mut self, args: &[String]) -> Result<bool, Error> {
        let output = self.brew.run_visible(args)?;
        self.done
            .extend(quiet::installed_from_output(&output.stdout));
        let ok = !brew_failed(&output);
        merge_status(&mut self.brew_status, output);
        Ok(ok)
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
fn brew_failed(output: &std::process::Output) -> bool {
    !output.status.success() && !already_installed_message(output)
}

/// What one staged `brew install` came to.
enum StagedRun {
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

pub fn is_verbose(flags: &[String]) -> bool {
    flags.iter().any(|f| f == "-v" || f == "--verbose")
}

fn already_installed_message(output: &std::process::Output) -> bool {
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
fn staging_dir(tap_root: &Path, origin_tap: &str) -> PathBuf {
    taps::staging_root(tap_root, &origin_tap.to_ascii_lowercase())
}

/// A dependency in a tap that could not be refreshed this run.
struct HeldDep {
    dep: String,
    /// Why the dep's tap cannot be used, e.g. `tap acme/tools could not be
    /// refreshed`; git detail follows on its own lines.
    reason: String,
}

struct CutoffDeps {
    /// (origin, name, kind), dependencies first.
    deps: Vec<(String, String, PkgKind)>,
    held: Option<HeldDep>,
}

struct CutoffDepWalk<'a, B, G> {
    brew: &'a B,
    git: &'a G,
    snaps: &'a Snapshots,
    cache: &'a Path,
    tap_root: &'a Path,
    inv: &'a Inventory,
    cfg: &'a Config,
    visiting: HashSet<String>,
    visited: HashSet<String>,
    out: Vec<(String, String, PkgKind)>,
    held: Option<HeldDep>,
}

impl<B: Brew, G: GitStore> CutoffDepWalk<'_, B, G> {
    fn visit(
        &mut self,
        origin_tap: &str,
        name: &str,
        kind: PkgKind,
        include_self: bool,
    ) -> Result<(), Error> {
        let key = format!("{origin_tap}/{name}");
        if self.visiting.contains(&key) || self.visited.contains(&key) {
            return Ok(());
        }
        self.visiting.insert(key.clone());
        if include_self {
            write_cutoff_blob(
                self.git,
                self.snaps,
                self.cache,
                self.tap_root,
                origin_tap,
                name,
                kind,
            )?;
        }
        let root = staging_dir(self.tap_root, origin_tap);
        let staged = match kind {
            PkgKind::Formula => tap::tap_formula_path(&root, name),
            PkgKind::Cask => tap::tap_cask_path(&root, name),
        };
        let token = staged.to_string_lossy().into_owned();
        for dep in self.brew.deps(kind, &token)? {
            let Some((dep_origin, dep_name)) = self.dep_origin(&dep, origin_tap)? else {
                continue;
            };
            // Already installed, from any origin: brew's, not staged or walked.
            if self.inv.find(&dep_name).is_some() {
                continue;
            }
            if self.inv.class_for(&dep_origin, &dep_name, self.cfg) != PkgClass::Soaked {
                continue; // no-soak or unsoakable deps are brew's
            }
            if let Some(err) = self.snaps.held_taps.get(&dep_origin) {
                // Its cutoff is unknown: refuse before staging anything.
                self.held = Some(HeldDep {
                    dep: dep_name,
                    reason: format!(
                        "tap {dep_origin} could not be refreshed;\n  {}",
                        err.replace('\n', "\n  ")
                    ),
                });
                return Ok(());
            }
            if !origin::is_core_or_cask(&dep_origin) && self.snaps.tap(&dep_origin).is_none() {
                self.held = Some(HeldDep {
                    dep: dep_name,
                    reason: format!("tap {dep_origin} has no soak snapshot this run"),
                });
                return Ok(());
            }
            let dep_kind = natural_kind(
                self.git,
                self.snaps,
                self.cache,
                self.inv,
                &dep_origin,
                &dep_name,
            )?;
            if !cutoff_blob_exists(
                self.git,
                self.snaps,
                self.cache,
                &dep_origin,
                &dep_name,
                dep_kind,
            )? {
                // Nothing to stage: install_missing_dep refuses the target
                // from the dep's upstream status (too new, not found).
                self.out.push((dep_origin, dep_name, dep_kind));
                continue;
            }
            self.visit(&dep_origin, &dep_name, dep_kind, true)?;
            if self.held.is_some() {
                return Ok(());
            }
        }
        self.visiting.remove(&key);
        self.visited.insert(key);
        if include_self {
            self.out
                .push((origin_tap.to_string(), name.to_string(), kind));
        }
        Ok(())
    }

    /// (origin, name) a `brew deps` token resolves to, or None when brew keeps it.
    fn dep_origin(
        &self,
        dep: &str,
        dependent_origin: &str,
    ) -> Result<Option<(String, String)>, Error> {
        if let Ok(tok) = inventory::parse_token(dep)
            && let Some(o) = tok.origin
        {
            return Ok(Some((o, tok.name)));
        }
        // At cutoff or HEAD: a dep born inside the window still resolves to
        // its origin, so the walk refuses the target instead of leaving the
        // name for brew to install unsoaked from HEAD.
        let exists = |o: &str, kind| -> Result<bool, Error> {
            let blobs = resolve_pkg_blobs(self.git, self.snaps, self.cache, o, dep, kind)?;
            Ok(blobs.cutoff.is_some() || blobs.head.is_some())
        };
        if !origin::is_core_or_cask(dependent_origin)
            && !self.snaps.held_taps.contains_key(dependent_origin)
            && exists(dependent_origin, PkgKind::Formula)?
        {
            return Ok(Some((dependent_origin.to_string(), dep.to_string())));
        }
        if exists(origin::CORE, PkgKind::Formula)? {
            return Ok(Some((origin::CORE.to_string(), dep.to_string())));
        }
        if exists(origin::CASK, PkgKind::Cask)? {
            return Ok(Some((origin::CASK.to_string(), dep.to_string())));
        }
        Ok(None)
    }
}

struct ResolvedView {
    installed: Option<PkgIdentity>,
    cutoff: Option<PkgIdentity>,
    head: Option<PkgIdentity>,
    action: DesiredAction,
    cutoff_blob: Option<Vec<u8>>,
    warnings: Vec<String>,
    /// The HEAD cask says `auto_updates true`.
    auto_updates: bool,
}

/// What a bare `upgrade`/`outdated` does: an installed self-updating cask
/// that is behind the cutoff is left to the app, as brew leaves it without
/// `--greedy`. Everything else keeps its action.
fn bare_action(view: &ResolvedView) -> DesiredAction {
    if view.auto_updates && view.installed.is_some() && view.action == DesiredAction::InstallCutoff
    {
        DesiredAction::LeaveAutoUpdates
    } else {
        view.action
    }
}

fn resolve_view(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    origin_tap: &str,
    name: &str,
    kind: PkgKind,
    receipt_rb: Option<&str>,
) -> Result<Option<ResolvedView>, Error> {
    let blobs = resolve_pkg_blobs(git, snaps, cache, origin_tap, name, kind)?;
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
    let head_rb = blobs
        .head
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok());
    let warnings = match head_rb {
        Some(rb) => identity::upcoming_lifecycle_messages(rb, &today)
            .into_iter()
            .map(|msg| format!("{name} {msg}"))
            .collect(),
        None => Vec::new(),
    };
    let auto_updates = kind == PkgKind::Cask && head_rb.is_some_and(identity::cask_auto_updates);
    Ok(Some(ResolvedView {
        installed,
        cutoff,
        head,
        action,
        cutoff_blob: blobs.cutoff,
        warnings,
        auto_updates,
    }))
}

fn calendar_today() -> String {
    let d = time::OffsetDateTime::now_utc().date();
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

/// Which snapshot a package resolves against: core and cask use their own
/// repos; a tap uses its clone and the tap's state in `state.toml`. Only the
/// state's `cutoff_sha` is ever the cutoff; a stale ref in the clone is not.
fn resolve_pkg_blobs(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    origin_tap: &str,
    name: &str,
    kind: PkgKind,
) -> Result<resolve::ResolvedBlobs, Error> {
    if origin::is_core_or_cask(origin_tap) {
        let repo = tap_repo(cache, kind);
        let tap = match kind {
            PkgKind::Formula => &snaps.core,
            PkgKind::Cask => &snaps.cask,
        };
        let pkg = PkgRef {
            name: name.to_string(),
            kind,
        };
        return resolve::resolve_blobs(git, &repo, &tap.cutoff_sha, &tap.head_sha, &pkg);
    }
    let Some(state) = snaps.tap(origin_tap) else {
        return Ok(resolve::ResolvedBlobs {
            cutoff: None,
            head: None,
        });
    };
    let dir = taps::clone_dir(cache, &origin_tap.to_ascii_lowercase());
    let show_at = |sha: &str| -> Result<Option<Vec<u8>>, Error> {
        let tree = git.ls_tree(&dir, sha)?;
        match taps::resolve_path(&tree, kind, name) {
            Some(path) => git.show(&dir, sha, &path),
            None => Ok(None),
        }
    };
    let cutoff = match state.cutoff_sha.as_deref() {
        Some(sha) => show_at(sha)?,
        None => None,
    };
    let head = show_at(&state.head_sha)?;
    Ok(resolve::ResolvedBlobs { cutoff, head })
}

fn cutoff_blob_exists(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    origin_tap: &str,
    name: &str,
    kind: PkgKind,
) -> Result<bool, Error> {
    Ok(
        resolve_pkg_blobs(git, snaps, cache, origin_tap, name, kind)?
            .cutoff
            .is_some(),
    )
}

fn natural_kind(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
    origin_tap: &str,
    name: &str,
) -> Result<PkgKind, Error> {
    let same_origin = |o: &str| {
        o == origin_tap || (origin::is_core_or_cask(o) && origin::is_core_or_cask(origin_tap))
    };
    if let Some(pkg) = inv
        .pkgs
        .iter()
        .find(|p| p.name == name && same_origin(&p.origin))
    {
        return Ok(pkg.kind);
    }
    let formula = resolve_pkg_blobs(git, snaps, cache, origin_tap, name, PkgKind::Formula)?;
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
    origin_tap: &str,
    name: &str,
    kind: PkgKind,
) -> Result<(), Error> {
    let blobs = resolve_pkg_blobs(git, snaps, cache, origin_tap, name, kind)?;
    let blob = blobs.cutoff.as_deref().ok_or_else(|| {
        Error::Other(format!("{name} is eligible but the cutoff blob is missing"))
    })?;
    let pkg = PkgRef {
        name: name.to_string(),
        kind,
    };
    tap::write_blob(&staging_dir(tap_root, origin_tap), &pkg, blob)?;
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

/// Caveats brew printed and where the raw brew log went.
fn write_session_tail(
    report: &crate::brew::SessionReport,
    out: &mut impl Write,
) -> Result<(), Error> {
    for line in &report.caveats {
        writeln!(out, "{line}")?;
    }
    if let Some(path) = &report.log_path {
        writeln!(out, "full brew log: {}", path.display())?;
    }
    Ok(())
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
    use crate::SoakHours;
    use crate::brew::{InstalledPkg, MockBrew};
    use crate::config::Config;
    use crate::eligibility::DesiredAction;
    use crate::git::InMemoryGit;
    use crate::github::{CommitInfo, StaticGithub};
    use crate::inventory::Inventory;
    use crate::origin::OriginRecords;
    use crate::resolve::PkgKind;
    use crate::snapshot::{TapSnapshot, TapState};
    use crate::taps::TapInfo;
    use std::path::Path;
    use time::{Duration, OffsetDateTime};

    fn cfg24() -> Config {
        Config::uniform(SoakHours::new(24).expect("hours >= 1"))
    }

    fn cfg_with(file: &str) -> Config {
        let p = crate::config::parse_file(file);
        Config {
            taps: p.taps,
            no_soak: p.no_soak,
            ..cfg24()
        }
    }

    fn inv_from(brew: &MockBrew, cfg: &Config) -> Inventory {
        Inventory::build(
            brew.installed.clone(),
            &brew.taps,
            &OriginRecords::default(),
            cfg,
        )
    }

    fn tapped(name: &str, remote: Option<&str>) -> TapInfo {
        TapInfo {
            name: name.into(),
            remote: remote.map(str::to_string),
        }
    }

    fn formula_pkg_from(name: &str, tap: &str, receipt_rb: String) -> InstalledPkg {
        InstalledPkg {
            tap: Some(tap.into()),
            ..formula_pkg(name, receipt_rb)
        }
    }

    /// Snapshots with core/cask plus one soaked tap whose cutoff/head trees
    /// hold `Formula/<name>.rb` blobs.
    fn tap_snaps(git: &InMemoryGit, tap: &str, cutoff: Option<&str>, head: &str) -> Snapshots {
        let mut snaps = core_snaps();
        snaps.taps.insert(
            tap.into(),
            TapState {
                hours: SoakHours::new(24).unwrap(),
                cutoff_sha: cutoff.map(str::to_string),
                head_sha: head.into(),
                cutoff_time: None,
            },
        );
        if let Some(c) = cutoff {
            git.insert_tree(c, &["Formula/terraform.rb", "README.md"]);
        }
        git.insert_tree(head, &["Formula/terraform.rb", "README.md"]);
        snaps
    }

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
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&MockBrew::new(), &cfg);
        update(
            &MockBrew::new(),
            &git,
            &fixture_gh(),
            dir.path(),
            &cfg,
            &inv,
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
        let snaps = Snapshots::core_only(
            TapSnapshot {
                cutoff_sha: "cutoffsha".into(),
                head_sha: "headsha".into(),
                cutoff_time: None,
            },
            TapSnapshot {
                cutoff_sha: "caskcut".into(),
                head_sha: "caskhead".into(),
                cutoff_time: None,
            },
            SoakHours::new(24).expect("hours >= 1"),
        );
        (brew, git, snaps)
    }

    #[test]
    fn outdated_lists_upgrade_held_and_ahead_sections() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
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
        // Inside the one-year warning horizon whatever day the test runs:
        // later than today, no later than today + 1 year.
        let year: u32 = calendar_today()[..4].parse().unwrap();
        let soon = format!("{}-01-01", year + 1);
        let new = format!(
            "{}\n  deprecate! date: \"{soon}\", because: :deprecated_upstream\n",
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(&format!("warning: py scheduled to be deprecated on {soon}")),
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = info(
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &names,
            &[],
            &mut out,
        )
        .expect("info");
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result =
            info(&git, &snaps, unused_cache(), &inv, &cfg, &[], &[], &mut out).expect("info");
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

    #[test]
    fn soaked_tap_install_stages_under_tap_dir_and_writes_origin_record() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let staged = tap.path().join("taps/hashicorp/tap/Formula/terraform.rb");
        assert!(staged.exists(), "staged under the per-tap directory");
        assert!(
            !tap.path().join("Formula/terraform.rb").exists(),
            "never under the core staging root"
        );
        let runs = lock_runs(&brew);
        assert!(
            runs.iter()
                .any(|a| a.iter().any(|x| x == staged.to_str().unwrap())),
            "{runs:?}"
        );
        let records = OriginRecords::load(cache.path());
        assert_eq!(
            records.get(PkgKind::Formula, "terraform"),
            Some("hashicorp/tap")
        );
    }

    #[test]
    fn core_install_removes_a_stale_origin_record() {
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
        let cache = tempfile::tempdir().unwrap();
        let mut records = OriginRecords::default();
        records.set(PkgKind::Formula, "alpha", "old/tap");
        records.save(cache.path()).unwrap();
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        install(
            &brew,
            &git,
            &core_snaps(),
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["alpha".into()],
            false,
            false,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "alpha"),
            None
        );
    }

    #[test]
    fn dep_closure_crosses_taps_and_core() {
        // terraform (hashicorp/tap) depends on bare `helper` (same tap) and `zlib` (core);
        // helper depends on `acme/tools/widget` (another soakable tap).
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        git.insert_blob(
            "tapcut",
            "Formula/helper.rb",
            formula_rb("helper", "0.1.0", "h1"),
        );
        git.insert_blob(
            "taphead",
            "Formula/helper.rb",
            formula_rb("helper", "0.1.0", "h1"),
        );
        git.insert_blob(
            "acmecut",
            "Formula/widget.rb",
            formula_rb("widget", "2.0.0", "w1"),
        );
        git.insert_blob(
            "acmehead",
            "Formula/widget.rb",
            formula_rb("widget", "2.0.0", "w1"),
        );
        git.insert_tree("acmecut", &["Formula/widget.rb"]);
        git.insert_tree("acmehead", &["Formula/widget.rb"]);
        git.insert_blob(
            "cutoffsha",
            "Formula/z/zlib.rb",
            formula_rb("zlib", "1.3", "z1"),
        );
        git.insert_blob(
            "headsha",
            "Formula/z/zlib.rb",
            formula_rb("zlib", "1.3", "z1"),
        );
        let tap = tempfile::tempdir().unwrap();
        let staged = |p: &str| tap.path().join(p).to_string_lossy().into_owned();
        let mut deps = BTreeMap::new();
        deps.insert(
            staged("taps/hashicorp/tap/Formula/terraform.rb"),
            vec!["helper".to_string(), "zlib".to_string()],
        );
        deps.insert(
            staged("taps/hashicorp/tap/Formula/helper.rb"),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/terraform.rb", "Formula/helper.rb"]);
        git.insert_tree("taphead", &["Formula/terraform.rb", "Formula/helper.rb"]);
        snaps.taps.insert(
            "acme/tools".into(),
            TapState {
                hours: SoakHours::new(24).unwrap(),
                cutoff_sha: Some("acmecut".into()),
                head_sha: "acmehead".into(),
                cutoff_time: None,
            },
        );
        let cache = tempfile::tempdir().unwrap();
        install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        let runs = lock_runs(&brew);
        let installs: Vec<&str> = runs
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .map(|a| a.last().unwrap().as_str())
            .collect();
        assert_eq!(
            installs,
            vec![
                staged("taps/acme/tools/Formula/widget.rb"),
                staged("taps/hashicorp/tap/Formula/helper.rb"),
                staged("Formula/zlib.rb"),
                staged("taps/hashicorp/tap/Formula/terraform.rb"),
            ],
            "deps first, each from its own origin's staging dir: {runs:?}"
        );
        let records = OriginRecords::load(cache.path());
        assert_eq!(records.get(PkgKind::Formula, "widget"), Some("acme/tools"));
        assert_eq!(
            records.get(PkgKind::Formula, "helper"),
            Some("hashicorp/tap")
        );
        assert_eq!(records.get(PkgKind::Formula, "zlib"), None);
    }

    #[test]
    fn no_soak_and_unsoakable_deps_are_left_to_brew() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec![
                "ericfitz/tap/brewsoak".to_string(),
                "local/tap/thing".to_string(),
            ],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped(
                    "ericfitz/tap",
                    Some("https://github.com/ericfitz/homebrew-tap"),
                ),
                tapped("local/tap", None),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        let installs: Vec<&Vec<String>> = runs
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .collect();
        assert_eq!(installs.len(), 1, "only the target: {runs:?}");
        assert!(!run_has_token(&runs, "ericfitz/tap/brewsoak"));
    }

    #[test]
    fn staged_load_failure_holds_with_note_and_does_not_fail_the_run() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            next_status: 1,
            next_stdout: b"Error: cannot load such file -- ../lib/helper\n".to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert_eq!(
            r.brew_status, None,
            "a staged-load hold is not a brew failure"
        );
        assert!(
            text.contains(
                "terraform: cannot be installed from a staged copy; use brew, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(text.contains("held 1"), "{text}");
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"),
            None
        );
    }

    const PACKER_REFUSAL: &[u8] = b"Error: packer: Refusing to load formula hashicorp/tap/packer from untrusted tap hashicorp/tap.\nRun `brew trust --formula hashicorp/tap/packer` or `brew trust hashicorp/tap` to trust it.\n";

    fn packer_world() -> (InMemoryGit, Snapshots) {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/packer.rb",
            formula_rb("packer", "1.16.1", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/packer.rb",
            formula_rb("packer", "1.17.0", "newsha"),
        );
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/packer.rb"]);
        git.insert_tree("taphead", &["Formula/packer.rb"]);
        (git, snaps)
    }

    fn install_packer(
        brew: &MockBrew,
        git: &InMemoryGit,
        snaps: &Snapshots,
    ) -> (RunResult, String) {
        let cfg = cfg24();
        let inv = inv_from(brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            brew,
            git,
            snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/packer".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        (r, String::from_utf8(out).unwrap())
    }

    fn hashicorp_tap() -> Vec<TapInfo> {
        vec![tapped(
            "hashicorp/tap",
            Some("https://github.com/hashicorp/homebrew-tap"),
        )]
    }

    #[test]
    fn untrusted_tap_refusal_holds_the_target_and_never_runs_brew_trust() {
        let (git, snaps) = packer_world();
        let brew = MockBrew {
            taps: hashicorp_tap(),
            next_status: 1,
            next_stdout: PACKER_REFUSAL.to_vec(),
            ..MockBrew::new()
        };
        let (r, text) = install_packer(&brew, &git, &snaps);
        assert!(r.refused);
        assert_eq!(r.brew_status, None, "a trust hold is not a brew failure");
        assert!(
            text.contains(
                "packer: brew does not trust tap hashicorp/tap; run brew trust hashicorp/tap, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(
            text.contains("upgraded 0,") && text.contains("held 1"),
            "{text}"
        );
        let all: Vec<Vec<String>> = lock_runs(&brew)
            .into_iter()
            .chain(brew.visible_runs.lock().unwrap().clone())
            .collect();
        assert!(
            !all.iter().any(|a| a.iter().any(|x| x == "trust")),
            "brewsoak must never trust a third-party tap: {all:?}"
        );
    }

    #[test]
    fn untrusted_tap_refusal_on_a_dep_holds_the_dep() {
        let (git, snaps) = packer_world();
        git.insert_blob(
            "tapcut",
            "Formula/widget.rb",
            formula_rb("widget", "1.0.0", "w"),
        );
        git.insert_blob(
            "taphead",
            "Formula/widget.rb",
            formula_rb("widget", "1.0.0", "w"),
        );
        git.insert_tree("tapcut", &["Formula/packer.rb", "Formula/widget.rb"]);
        git.insert_tree("taphead", &["Formula/packer.rb", "Formula/widget.rb"]);
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/packer.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["hashicorp/tap/widget".to_string()],
        );
        let widget_refusal = String::from_utf8_lossy(PACKER_REFUSAL).replace("packer", "widget");
        let brew = MockBrew {
            deps,
            taps: hashicorp_tap(),
            next_outputs: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                (1, widget_refusal.into_bytes()),
                (1, PACKER_REFUSAL.to_vec()),
            ])),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/packer".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert_eq!(r.brew_status, None);
        for name in ["widget", "packer"] {
            assert!(
                text.contains(&format!(
                    "{name}: brew does not trust tap hashicorp/tap; run brew trust hashicorp/tap, or add it to NO_SOAK"
                )),
                "{text}"
            );
        }
        assert!(text.contains("upgraded 0,"), "{text}");
    }

    #[test]
    fn failed_brew_run_is_not_counted_as_upgraded() {
        let (git, snaps) = packer_world();
        let brew = MockBrew {
            taps: hashicorp_tap(),
            next_status: 1,
            next_stdout: b"Error: packer: download failed\n".to_vec(),
            ..MockBrew::new()
        };
        let (r, text) = install_packer(&brew, &git, &snaps);
        assert_eq!(r.brew_status, Some(1), "{text}");
        assert!(text.contains("upgraded 0,"), "{text}");
        assert!(!text.contains("held 1"), "a failure is not a hold: {text}");
    }

    #[test]
    fn failed_reinstall_repair_is_not_counted_as_upgraded() {
        let wget_new = formula_rb("wget", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new.clone());
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_new)],
            next_status: 1,
            next_stdout: b"Error: wget: download failed\n".to_vec(),
            ..MockBrew::new()
        };
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let result = call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["wget".to_string()],
            &mut out,
        )
        .expect("reinstall");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(result.brew_status, Some(1), "{text}");
        assert!(text.contains("upgraded 0,"), "{text}");
    }

    #[test]
    fn deps_failure_on_staged_tap_formula_holds_with_note() {
        struct FailDepsBrew(MockBrew);
        impl Brew for FailDepsBrew {
            fn brew_bin(&self) -> &Path {
                self.0.brew_bin()
            }
            fn run(&self, a: &[String]) -> Result<std::process::Output, Error> {
                self.0.run(a)
            }
            fn run_visible(&self, a: &[String]) -> Result<std::process::Output, Error> {
                self.0.run_visible(a)
            }
            fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
                self.0.installed_packages()
            }
            fn tap_new_soaked(&self) -> Result<(), Error> {
                self.0.tap_new_soaked()
            }
            fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
                self.0.tap_info()
            }
            fn outdated_names(&self) -> Result<Vec<String>, Error> {
                self.0.outdated_names()
            }
            fn deps(&self, _k: PkgKind, token: &str) -> Result<Vec<String>, Error> {
                Err(Error::Brew {
                    status: 1,
                    message: format!(
                        "Error: No available formula with the name \"helper\" ({token})"
                    ),
                })
            }
        }
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = FailDepsBrew(MockBrew {
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        });
        let cfg = cfg24();
        let inv = inv_from(&brew.0, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("cannot be installed from a staged copy")
        );
        assert!(
            lock_runs(&brew.0)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install"))
        );
    }

    #[test]
    fn deps_untrusted_tap_refusal_holds_with_trust_note() {
        struct FailDepsBrew(MockBrew);
        impl Brew for FailDepsBrew {
            fn brew_bin(&self) -> &Path {
                self.0.brew_bin()
            }
            fn run(&self, a: &[String]) -> Result<std::process::Output, Error> {
                self.0.run(a)
            }
            fn run_visible(&self, a: &[String]) -> Result<std::process::Output, Error> {
                self.0.run_visible(a)
            }
            fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
                self.0.installed_packages()
            }
            fn tap_new_soaked(&self) -> Result<(), Error> {
                self.0.tap_new_soaked()
            }
            fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
                self.0.tap_info()
            }
            fn outdated_names(&self) -> Result<Vec<String>, Error> {
                self.0.outdated_names()
            }
            fn deps(&self, _k: PkgKind, token: &str) -> Result<Vec<String>, Error> {
                Err(Error::Brew {
                    status: 1,
                    message: format!(
                        "Error: terraform: Refusing to load formula hashicorp/tap/terraform from untrusted tap hashicorp/tap. ({token})"
                    ),
                })
            }
        }
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = FailDepsBrew(MockBrew {
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        });
        let cfg = cfg24();
        let inv = inv_from(&brew.0, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "terraform: brew does not trust tap hashicorp/tap; run brew trust hashicorp/tap, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(!text.contains("staged copy"), "{text}");
        assert!(
            lock_runs(&brew.0)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install"))
        );
    }

    #[test]
    fn dep_in_held_tap_refuses_the_target() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        snaps.held_taps.insert(
            "acme/tools".into(),
            "while fetching history from origin, git failed:\nfatal: boom".into(),
        );
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("cannot install terraform: dependency widget")
                && text.contains("acme/tools could not be refreshed"),
            "{text}"
        );
        assert!(
            text.contains("upgraded 0, already soaked 0, held 1"),
            "a held dependency holds the target, it does not upgrade it: {text}"
        );
    }

    #[test]
    fn dep_in_tap_without_snapshot_refuses_target_and_run_continues() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
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
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into(), "alpha".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert!(
            text.contains("cannot install terraform: dependency widget")
                && text.contains("acme/tools has no soak snapshot"),
            "{text}"
        );
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "alpha"),
            "run continued"
        );
        assert!(!run_is_soaked_install(&lock_runs(&brew), "terraform"));
    }

    #[test]
    fn explicit_tap_dep_too_new_refuses_target_as_too_new() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        git.insert_blob(
            "acmehead",
            "Formula/widget.rb",
            formula_rb("widget", "2.0.0", "w1"),
        );
        git.insert_tree("acmecut", &["README.md"]);
        git.insert_tree("acmehead", &["Formula/widget.rb"]);
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        snaps.taps.insert(
            "acme/tools".into(),
            TapState {
                hours: SoakHours::new(24).unwrap(),
                cutoff_sha: Some("acmecut".into()),
                head_sha: "acmehead".into(),
                cutoff_time: None,
            },
        );
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            tempfile::tempdir().unwrap().path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert!(
            text.contains("cannot install terraform: dependency widget is too new"),
            "{text}"
        );
        assert!(
            lock_runs(&brew)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install"))
        );
    }

    /// A tap target with one bare dep `helper`; `setup` stages the git side.
    fn bare_dep_refusal(setup: impl FnOnce(&InMemoryGit)) -> (String, RunResult, MockBrew) {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["helper".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        setup(&git);
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            tempfile::tempdir().unwrap().path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        (String::from_utf8(out).unwrap(), r, brew)
    }

    fn assert_dep_too_new_refusal(text: &str, r: &RunResult, brew: &MockBrew) {
        assert!(r.refused, "{text}");
        assert!(
            text.contains("cannot install terraform: dependency helper is too new"),
            "{text}"
        );
        assert!(
            lock_runs(brew)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install")),
            "nothing may be installed: {:?}",
            lock_runs(brew)
        );
        assert!(
            text.contains("upgraded 0, already soaked 0, held 1"),
            "a dep refusal holds the target, it does not upgrade it: {text}"
        );
    }

    #[test]
    fn same_tap_bare_dep_only_at_tap_head_refuses_target_as_too_new() {
        let (text, r, brew) = bare_dep_refusal(|git| {
            git.insert_blob(
                "taphead",
                "Formula/helper.rb",
                formula_rb("helper", "0.1.0", "h1"),
            );
            git.insert_tree("taphead", &["Formula/terraform.rb", "Formula/helper.rb"]);
        });
        assert_dep_too_new_refusal(&text, &r, &brew);
    }

    #[test]
    fn bare_dep_only_at_core_head_refuses_target_as_too_new() {
        let (text, r, brew) = bare_dep_refusal(|git| {
            git.insert_blob(
                "headsha",
                "Formula/h/helper.rb",
                formula_rb("helper", "0.1.0", "h1"),
            );
        });
        assert_dep_too_new_refusal(&text, &r, &brew);
    }

    #[test]
    fn installed_dep_from_held_tap_does_not_refuse_target() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            installed: vec![formula_pkg("widget", formula_rb("widget", "2.0.0", "w1"))],
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        snaps
            .held_taps
            .insert("acme/tools".into(), "fatal: boom".into());
        let cache = tempfile::tempdir().unwrap();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!r.refused);
        assert!(run_is_soaked_install(&lock_runs(&brew), "terraform"));
    }

    #[test]
    fn dependency_cycle_terminates_and_installs_dep_first() {
        let git = InMemoryGit::new();
        for (sha, ver) in [("tapcut", "1.1.0"), ("taphead", "1.2.0")] {
            git.insert_blob(
                sha,
                "Formula/terraform.rb",
                formula_rb("terraform", ver, "s1"),
            );
            git.insert_blob(
                sha,
                "Formula/helper.rb",
                formula_rb("helper", "0.1.0", "h1"),
            );
        }
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["helper".to_string()],
        );
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/helper.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["terraform".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/terraform.rb", "Formula/helper.rb"]);
        git.insert_tree("taphead", &["Formula/terraform.rb", "Formula/helper.rb"]);
        let cache = tempfile::tempdir().unwrap();
        install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        let installs: Vec<String> = lock_runs(&brew)
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .map(|a| a.last().unwrap().clone())
            .collect();
        assert_eq!(installs.len(), 2, "{installs:?}");
        assert!(
            installs[0].ends_with("helper.rb") && installs[1].ends_with("terraform.rb"),
            "{installs:?}"
        );
    }

    fn core_snaps() -> Snapshots {
        Snapshots::core_only(
            TapSnapshot {
                cutoff_sha: "cutoffsha".into(),
                head_sha: "headsha".into(),
                cutoff_time: None,
            },
            TapSnapshot {
                cutoff_sha: "caskcut".into(),
                head_sha: "caskhead".into(),
                cutoff_time: None,
            },
            SoakHours::new(24).expect("hours >= 1"),
        )
    }

    fn formula_pkg(name: &str, receipt_rb: String) -> InstalledPkg {
        InstalledPkg {
            name: name.into(),
            kind: PkgKind::Formula,
            receipt_rb,
            pinned: false,
            tap: None,
            staged_path: None,
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

    fn upgrade_names(brew: &MockBrew, git: &InMemoryGit, names: &[String]) -> String {
        upgrade_names_flags(brew, git, names, &[])
    }

    fn upgrade_names_flags(
        brew: &MockBrew,
        git: &InMemoryGit,
        names: &[String],
        flags: &[String],
    ) -> String {
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(brew, &cfg);
        upgrade(
            brew,
            git,
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            names,
            flags,
            &mut out,
        )
        .expect("upgrade");
        String::from_utf8(out).expect("utf8")
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
        let note =
            "brew install does not accept --greedy, --ignore-pinned; dropped from staged installs";
        assert_eq!(text.matches(note).count(), 1, "{text}");
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = upgrade(
            &brew,
            &git,
            &snaps,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        upgrade(
            &brew,
            &git,
            &snaps,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let err = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(brew, &cfg);
        reinstall(
            brew,
            git,
            snaps,
            unused_cache(),
            tap,
            &inv,
            &cfg,
            names,
            &[],
            out,
        )
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
    fn reinstall_cask_true_repair_with_version_only_receipt() {
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/a/app.rb", cask_rb("app", "1.0.0"));
        git.insert_blob("caskhead", "Casks/a/app.rb", cask_rb("app", "1.1.0"));
        let brew = MockBrew {
            installed: vec![cask_pkg_from(
                "app",
                "homebrew/cask",
                crate::brew::version_only_cask_receipt("app", "1.1.0"),
            )],
            ..MockBrew::new()
        };
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["app".to_string()],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert!(
            runs.iter()
                .any(|a| a == &["reinstall".to_string(), "app".to_string()]),
            "installed == HEAD is a true repair: {runs:?}"
        );
    }

    #[test]
    fn reinstall_true_repair_when_receipt_has_no_rebuild() {
        let head = |rebuild: bool| {
            let bottle = if rebuild {
                "  bottle do\n    rebuild 1\n    sha256 cellar: :any, arm64_tahoe: \"bbb\"\n  end\n"
            } else {
                ""
            };
            format!(
                "class Wget < Formula\n  url \"https://example.com/wget-1.2.0.tar.gz\"\n  sha256 \"newsha\"\n{bottle}end\n"
            )
        };
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        git.insert_blob("headsha", "Formula/w/wget.rb", head(true));
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", head(false))],
            ..MockBrew::new()
        };
        let tap = tempfile::tempdir().unwrap();
        call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["wget".to_string()],
            &mut Vec::new(),
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert!(
            runs.iter()
                .any(|a| a == &["reinstall".to_string(), "wget".to_string()]),
            "a receipt without rebuild still matches HEAD: {runs:?}"
        );
    }

    #[test]
    fn malformed_explicit_token_is_rejected_before_any_brew_run() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "headsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let names = ["wget".to_string(), "a/b".to_string()];
        let mut out = Vec::new();
        let err = install(
            &brew,
            &git,
            &core_snaps(),
            tempfile::tempdir().unwrap().path(),
            tap.path(),
            &inv,
            &cfg,
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert!(
            lock_runs(&brew).is_empty(),
            "no side effects: {:?}",
            lock_runs(&brew)
        );
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn bare_upgrade_total_leaves_auto_updates_casks_out() {
        let (brew, git, snaps) = auto_updates_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            tempfile::tempdir().unwrap().path(),
            tempfile::tempdir().unwrap().path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("upgrading 0 of 1 packages"), "{text}");
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
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

    fn local_tap_world() -> (MockBrew, InMemoryGit) {
        let alpha_old = formula_rb("alpha", "1.0.0", "oldsha");
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
                formula_pkg("alpha", alpha_old),
                formula_pkg_from("thing", "local/tap", formula_rb("thing", "1.0.0", "oldsha")),
            ],
            taps: vec![tapped("local/tap", None)],
            ..MockBrew::new()
        };
        (brew, git)
    }

    #[test]
    fn info_names_an_unsoakable_tap_package_and_refuses_it() {
        let (brew, git) = local_tap_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let names = ["alpha".to_string(), "local/tap/thing".to_string()];
        let r = info(
            &git,
            &core_snaps(),
            unused_cache(),
            &inv,
            &cfg,
            &names,
            &[],
            &mut out,
        )
        .expect("info");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "core name still soaked: {text}");
        assert!(text.contains("origin: local/tap"), "{text}");
        assert!(text.contains("unsoakable"), "{text}");
        assert!(text.contains("use `brew info local/tap/thing`"), "{text}");
        assert!(r.refused, "explicitly named unsoakable package is refused");
        assert!(
            brew.visible_runs.lock().expect("visible").is_empty(),
            "info never hands a tap package to brew"
        );
    }

    #[test]
    fn outdated_notes_unsoakable_packages_without_brew() {
        let (brew, git) = local_tap_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let r = outdated(
            &brew,
            &git,
            &core_snaps(),
            unused_cache(),
            &inv,
            &cfg,
            &["acme/tools/foo".to_string()],
            &mut out,
        )
        .expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "soaked listing still runs: {text}");
        assert!(
            text.contains(
                "thing: tap local/tap has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(!r.refused);
        assert!(brew.visible_runs.lock().expect("visible").is_empty());
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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

    #[test]
    fn upgrade_bare_soaks_tap_package_from_its_tap_snapshot() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "hashicorp/tap",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("upgrading terraform 1.0.0 -> 1.1.0"),
            "{text}"
        );
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "terraform"),
            "{:?}",
            lock_runs(&brew)
        );
        assert!(!r.refused);
        assert!(
            !run_has_token(&lock_runs(&brew), "hashicorp/tap/terraform"),
            "soaked tap packages are never brew tokens"
        );
    }

    #[test]
    fn staged_tap_upgrade_records_origin_despite_outdated_notice() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "hashicorp/tap",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            // What brew prints on every upgrade of an outdated keg.
            next_stdout: b"Warning: terraform 1.0.0 is already installed but outdated (so it will be upgraded).\n"
                .to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"),
            Some("hashicorp/tap")
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

    fn cask_pkg_from(name: &str, tap: &str, receipt_rb: String) -> InstalledPkg {
        InstalledPkg {
            name: name.into(),
            kind: PkgKind::Cask,
            receipt_rb,
            pinned: false,
            tap: Some(tap.into()),
            staged_path: None,
        }
    }

    /// Installed cask `ant` from anthropics/tap (version-only receipt, as
    /// Homebrew >= 4 leaves it) while core also carries a formula `ant`.
    fn tap_cask_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/a/ant.rb",
            formula_rb("ant", "1.10.0", "c"),
        );
        git.insert_blob(
            "headsha",
            "Formula/a/ant.rb",
            formula_rb("ant", "1.10.0", "c"),
        );
        git.insert_blob("tapcut", "Casks/ant.rb", cask_rb("ant", "1.38.0"));
        git.insert_blob("taphead", "Casks/ant.rb", cask_rb("ant", "1.39.0"));
        let brew = MockBrew {
            installed: vec![cask_pkg_from(
                "ant",
                "anthropics/tap",
                crate::brew::version_only_cask_receipt("ant", "1.37.0"),
            )],
            taps: vec![tapped(
                "anthropics/tap",
                Some("https://github.com/anthropics/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let snaps = tap_snaps(&git, "anthropics/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Casks/ant.rb"]);
        git.insert_tree("taphead", &["Casks/ant.rb"]);
        (brew, git, snaps)
    }

    #[test]
    fn info_bare_name_uses_installed_tap_cask_not_core_formula() {
        let (brew, git, snaps) = tap_cask_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        info(
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["ant".into()],
            &["-v".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("ant: origin anthropics/tap; soak 24h"),
            "{text}"
        );
        assert!(
            text.contains(
                "ant\ninstalled: 1.37.0\ncutoff: 1.38.0\nhead: 1.39.0\norigin: anthropics/tap\nsoak hours: 24\naction: would upgrade"
            ),
            "{text}"
        );
        assert!(!text.contains("homebrew/core"), "{text}");
    }

    #[test]
    fn upgrade_bare_soaks_tap_cask_from_its_tap_snapshot() {
        let (brew, git, snaps) = tap_cask_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &["-v".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("ant: origin anthropics/tap; soak 24h"),
            "{text}"
        );
        assert!(text.contains("upgrading ant 1.37.0 -> 1.38.0"), "{text}");
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "ant"), "{runs:?}");
        assert!(
            runs.iter().any(|a| a.iter().any(|x| x == "--cask")
                && a.iter().any(|x| x.ends_with("/Casks/ant.rb"))),
            "staged from the tap's Casks/ path: {runs:?}"
        );
        assert!(!r.refused);
    }

    #[test]
    fn info_explicit_tap_token_finds_installed_cask_sharing_a_core_formula_name() {
        let git = InMemoryGit::new();
        let core = formula_rb("vacuum", "0.30.6", "s");
        git.insert_blob("cutoffsha", "Formula/v/vacuum.rb", core.clone());
        git.insert_blob("headsha", "Formula/v/vacuum.rb", core.clone());
        git.insert_blob("tapcut", "Casks/vacuum.rb", cask_rb("vacuum", "0.30.6"));
        git.insert_blob("taphead", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0"));
        let brew = MockBrew {
            installed: vec![
                formula_pkg_from("vacuum", "homebrew/core", core),
                cask_pkg_from(
                    "vacuum",
                    "daveshanley/vacuum",
                    crate::brew::version_only_cask_receipt("vacuum", "0.30.6"),
                ),
            ],
            taps: vec![tapped(
                "daveshanley/vacuum",
                Some("https://github.com/daveshanley/homebrew-vacuum"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "daveshanley/vacuum", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Casks/vacuum.rb"]);
        git.insert_tree("taphead", &["Casks/vacuum.rb"]);
        let mut out = Vec::new();
        info(
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["daveshanley/vacuum/vacuum".into(), "vacuum".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "daveshanley/vacuum/vacuum\ninstalled: 0.30.6\ncutoff: 0.30.6\nhead: 0.31.0\norigin: daveshanley/vacuum\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "vacuum\ninstalled: 0.30.6\ncutoff: 0.30.6\nhead: 0.30.6\norigin: homebrew/core\n"
            ),
            "bare name is the core formula: {text}"
        );
    }

    /// Installed core formula `vacuum` and tap cask `daveshanley/vacuum/vacuum`,
    /// both behind their cutoffs.
    fn same_name_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/v/vacuum.rb",
            formula_rb("vacuum", "0.30.6", "s"),
        );
        git.insert_blob(
            "headsha",
            "Formula/v/vacuum.rb",
            formula_rb("vacuum", "0.30.6", "s"),
        );
        git.insert_blob("tapcut", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0"));
        git.insert_blob("taphead", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0"));
        let brew = MockBrew {
            installed: vec![
                formula_pkg_from(
                    "vacuum",
                    "homebrew/core",
                    formula_rb("vacuum", "0.30.0", "old"),
                ),
                cask_pkg_from(
                    "vacuum",
                    "daveshanley/vacuum",
                    crate::brew::version_only_cask_receipt("vacuum", "0.30.0"),
                ),
            ],
            taps: vec![tapped(
                "daveshanley/vacuum",
                Some("https://github.com/daveshanley/homebrew-vacuum"),
            )],
            ..MockBrew::new()
        };
        let snaps = tap_snaps(&git, "daveshanley/vacuum", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Casks/vacuum.rb"]);
        git.insert_tree("taphead", &["Casks/vacuum.rb"]);
        (brew, git, snaps)
    }

    #[test]
    fn upgrade_bare_evaluates_same_name_formula_and_tap_cask_once_each() {
        let (brew, git, snaps) = same_name_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("upgrading vacuum 0.30.0 -> 0.30.6"),
            "formula: {text}"
        );
        assert!(
            text.contains("upgrading vacuum 0.30.0 -> 0.31.0"),
            "cask: {text}"
        );
        assert_eq!(text.matches("upgrading vacuum").count(), 2, "{text}");
        let runs = lock_runs(&brew);
        let installs: Vec<&Vec<String>> = runs
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .collect();
        assert_eq!(installs.len(), 2, "{runs:?}");
        assert!(
            installs.iter().any(|a| a.iter().any(|x| x == "--cask")
                && a.iter().any(|x| x.ends_with("/Casks/vacuum.rb"))),
            "cask staged from the tap's Casks/ path: {runs:?}"
        );
        assert!(
            installs
                .iter()
                .any(|a| !a.iter().any(|x| x == "--cask")
                    && a.iter().any(|x| x.ends_with("/vacuum.rb"))),
            "formula staged: {runs:?}"
        );
    }

    #[test]
    fn a_formula_poured_earlier_does_not_mark_a_same_name_cask_done() {
        let (mut brew, git, snaps) = same_name_world();
        // brew pours formula `vacuum` at the cask's cutoff version.
        brew.next_stdout = "\u{1f37a}  /opt/homebrew/Cellar/vacuum/0.31.0: 5 files, 1MB\n"
            .as_bytes()
            .to_vec();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            lock_runs(&brew)
                .iter()
                .any(|a| a.iter().any(|x| x == "--cask")),
            "the cask must still be installed"
        );
    }

    #[test]
    fn upgrade_bare_header_total_equals_announced_steps() {
        let git = InMemoryGit::new();
        for name in ["left", "right"] {
            let dir = &name[..1];
            git.insert_blob(
                "cutoffsha",
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, "1.1.0", "midsha"),
            );
            git.insert_blob(
                "headsha",
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, "1.1.0", "midsha"),
            );
        }
        let auto = format!("{}  auto_updates true\n", cask_rb("alt-tab", "11.8.0"));
        git.insert_blob("caskcut", "Casks/a/alt-tab.rb", auto.clone());
        git.insert_blob("caskhead", "Casks/a/alt-tab.rb", auto);
        let (vbrew, vgit, snaps) = same_name_world();
        for (sha, path, body) in [
            (
                "cutoffsha",
                "Formula/v/vacuum.rb",
                formula_rb("vacuum", "0.30.6", "s"),
            ),
            (
                "headsha",
                "Formula/v/vacuum.rb",
                formula_rb("vacuum", "0.30.6", "s"),
            ),
            ("tapcut", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0")),
            ("taphead", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0")),
        ] {
            git.insert_blob(sha, path, body);
        }
        drop(vgit);
        git.insert_tree("tapcut", &["Casks/vacuum.rb"]);
        git.insert_tree("taphead", &["Casks/vacuum.rb"]);
        let mut installed = vec![
            formula_pkg("left", formula_rb("left", "1.0.0", "oldsha")),
            formula_pkg("right", formula_rb("right", "1.0.0", "oldsha")),
            cask_pkg_from(
                "alt-tab",
                "homebrew/cask",
                crate::brew::version_only_cask_receipt("alt-tab", "7.38.1"),
            ),
        ];
        installed.extend(vbrew.installed.clone());
        // brew pours `right` while installing `left`: it is already at cutoff
        // when its turn comes.
        let brew = MockBrew {
            installed,
            taps: vbrew.taps.clone(),
            next_stdout: "\u{1f37a}  /opt/homebrew/Cellar/right/1.1.0: 5 files, 1MB\n"
                .as_bytes()
                .to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let header = text
            .lines()
            .find_map(|l| {
                l.strip_prefix("upgrading ")?
                    .split(' ')
                    .next()?
                    .parse::<usize>()
                    .ok()
            })
            .expect("header");
        let announced = text
            .lines()
            .filter(|l| l.starts_with('[') && l.contains(&format!("/{header}] ")))
            .count();
        assert_eq!(header, 4, "{text}");
        assert_eq!(announced, header, "{text}");
        assert!(!text.contains("alt-tab 7.38.1 ->"), "{text}");
    }

    /// Installed `alt-tab` 7.38.1 (self-updating app) behind a cask cutoff
    /// of 11.8.0 that says `auto_updates true`, like brew's own skip.
    fn auto_updates_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let git = InMemoryGit::new();
        let rb = format!("{}  auto_updates true\n", cask_rb("alt-tab", "11.8.0"));
        git.insert_blob("caskcut", "Casks/a/alt-tab.rb", rb.clone());
        git.insert_blob("caskhead", "Casks/a/alt-tab.rb", rb);
        let brew = MockBrew {
            installed: vec![cask_pkg_from(
                "alt-tab",
                "homebrew/cask",
                crate::brew::version_only_cask_receipt("alt-tab", "7.38.1"),
            )],
            ..MockBrew::new()
        };
        (brew, git, core_snaps())
    }

    #[test]
    fn auto_updates_cask_is_left_alone_on_bare_upgrade_but_upgraded_when_named() {
        let (brew, git, snaps) = auto_updates_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &["-v".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !run_is_soaked_install(&lock_runs(&brew), "alt-tab"),
            "bare run must not reinstall a self-updating app: {text}"
        );
        assert!(
            text.contains("alt-tab: auto-updates; installed 7.38.1 is left to the app"),
            "{text}"
        );
        assert!(
            text.contains("upgraded 0, already soaked 0, held 0, ahead 0, pinned 0, skipped 0, no-soak 0, auto-updates 1"),
            "{text}"
        );

        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["alt-tab".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("upgrading alt-tab 7.38.1 -> 11.8.0"),
            "{text}"
        );
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "alt-tab"),
            "named explicitly, it upgrades like `brew upgrade alt-tab`: {text}"
        );
    }

    #[test]
    fn outdated_lists_auto_updates_cask_in_its_own_section() {
        let (brew, git, snaps) = auto_updates_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("==> Outdated (will upgrade)\n(none)\n"),
            "brew does not list auto_updates casks without --greedy: {text}"
        );
        assert!(
            text.contains("==> Auto-updates (left to the app; name it to upgrade)\nalt-tab (7.38.1) < 11.8.0\n"),
            "{text}"
        );
        assert!(
            !text.contains("nothing outdated"),
            "a non-empty Auto-updates section is something outdated: {text}"
        );
    }

    #[test]
    fn upgrade_explicit_tap_token_is_soaked_not_passed_through() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "terraform"), "{runs:?}");
        assert!(!run_has_token(&runs, "hashicorp/tap/terraform"), "{runs:?}");
    }

    #[test]
    fn tap_without_cutoff_holds_its_packages_as_too_new() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "hashicorp/tap",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", None, "taphead");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
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
        assert!(text.contains("terraform is too new"), "{text}");
        assert!(
            lock_runs(&brew)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install")),
            "nothing installed"
        );
    }

    #[test]
    fn held_tap_holds_only_its_packages_with_git_note() {
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
                    "terraform",
                    "hashicorp/tap",
                    formula_rb("terraform", "1.0.0", "oldsha"),
                ),
            ],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = core_snaps();
        snaps.held_taps.insert(
            "hashicorp/tap".into(),
            "while fetching history from origin, git failed:\nfatal: boom".into(),
        );
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
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
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "alpha"),
            "core package still upgraded"
        );
        assert!(r.refused);
        assert!(
            text.contains("terraform: tap hashicorp/tap could not be refreshed")
                && text.contains("fatal: boom"),
            "{text}"
        );
    }

    #[test]
    fn unsoakable_tap_package_is_noted_in_bare_upgrade_and_refused_when_named() {
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "thing",
                "local/tap",
                formula_rb("thing", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped("local/tap", None)],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = core_snaps();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git_empty(),
            &snaps,
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
        assert!(!r.refused, "bare upgrade only notes unsoakable packages");
        assert!(text.contains("thing: tap local/tap has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK"), "{text}");
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git_empty(),
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["local/tap/thing".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused, "explicit unsoakable is a refusal");
        assert!(
            text.contains("use `brew upgrade local/tap/thing`"),
            "{text}"
        );
        assert!(lock_runs(&brew).is_empty());
    }

    fn git_empty() -> InMemoryGit {
        InMemoryGit::new()
    }

    #[test]
    fn explicit_token_for_untapped_tap_is_refused_with_tap_hint() {
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["nobody/tap/x".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("tap nobody/tap is not installed; run brew tap nobody/tap first, or add it to NO_SOAK"), "{text}");
        assert!(lock_runs(&brew).is_empty());
        // No-soak entry: brew handles it (brew auto-taps).
        let cfg = cfg_with("NO_SOAK = [\"nobody/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["nobody/tap/x".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        assert_eq!(runs[0], vec!["update".to_string()]);
        assert_eq!(runs[1], vec!["install", "nobody/tap/x"]);
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

    #[test]
    fn reinstall_no_soak_goes_to_brew_reinstall_after_update() {
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "brewsoak",
                "ericfitz/tap",
                formula_rb("brewsoak", "1.0.0", "b"),
            )],
            taps: vec![tapped(
                "ericfitz/tap",
                Some("https://github.com/ericfitz/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let r = reinstall(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["brewsoak".into()],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        assert_eq!(
            runs,
            vec![
                vec!["update".to_string()],
                vec!["reinstall".to_string(), "ericfitz/tap/brewsoak".to_string()]
            ]
        );
    }

    #[test]
    fn reinstall_true_repair_of_tap_package_uses_full_token() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "hashicorp/tap",
                formula_rb("terraform", "1.2.0", "newsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let tap = tempfile::tempdir().unwrap();
        reinstall(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["terraform".into()],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            lock_runs(&brew),
            vec![vec![
                "reinstall".to_string(),
                "hashicorp/tap/terraform".to_string()
            ]]
        );
    }

    #[test]
    fn explicit_tap_token_over_a_core_keg_is_refused_not_switched() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "homebrew/core",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let tap = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        assert!(String::from_utf8(out).unwrap().contains(
            "terraform is installed from homebrew/core; brewsoak does not switch taps; use brew"
        ));
        assert!(lock_runs(&brew).is_empty());
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"),
            None
        );
    }

    #[test]
    fn explicit_no_soak_tap_token_over_a_core_keg_is_also_refused() {
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "homebrew/core",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"hashicorp/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("use brew install hashicorp/tap/terraform"),
            "{text}"
        );
        assert!(lock_runs(&brew).is_empty());
    }

    #[test]
    fn fresh_cask_install_honors_no_soak_for_homebrew_cask() {
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/f/firefox.rb", cask_rb("firefox", "1.0"));
        git.insert_blob("caskhead", "Casks/f/firefox.rb", cask_rb("firefox", "1.0"));
        let brew = MockBrew::new();
        let cfg = cfg_with("NO_SOAK = [\"homebrew/cask\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        install(
            &brew,
            &git,
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["firefox".into()],
            false,
            false,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(
            runs,
            vec![
                vec!["update".to_string()],
                vec!["install".to_string(), "firefox".to_string()]
            ],
            "{runs:?}"
        );
    }

    #[test]
    fn tap_package_refusal_hint_uses_full_token() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", None, "taphead");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("use `brew install hashicorp/tap/terraform` to bypass")
        );
    }

    #[test]
    fn verbose_prints_origin_and_hours_per_package() {
        let (brew, git, snaps) = view_world();
        let cfg = cfg_with("[[TAP]]\nname = \"homebrew/core\"\nsoak_hours = 48\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["-v".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("alpha: origin homebrew/core; soak 48h"),
            "{text}"
        );
    }

    #[test]
    fn update_runs_brew_update_once_only_when_a_no_soak_package_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", formula_rb("wget", "1.0.0", "a"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        update(
            &brew,
            &git,
            &fixture_gh(),
            dir.path(),
            &cfg,
            &inv,
            now(),
            false,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !run_has_token(&lock_runs(&brew), "update"),
            "no no-soak package: no brew update"
        );
        let cfg = cfg_with("NO_SOAK = [\"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        update(
            &brew,
            &git,
            &fixture_gh(),
            dir.path(),
            &cfg,
            &inv,
            now(),
            false,
            &mut out,
        )
        .unwrap();
        let updates = lock_runs(&brew)
            .iter()
            .filter(|a| a == &&vec!["update".to_string()])
            .count();
        assert_eq!(updates, 1);
        assert!(String::from_utf8(out).unwrap().contains("updating brew"));
    }

    #[test]
    fn update_reports_a_failed_brew_update_after_writing_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", formula_rb("wget", "1.0.0", "a"))],
            next_status: 3,
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let err = update(
            &brew,
            &git,
            &fixture_gh(),
            dir.path(),
            &cfg,
            &inv,
            now(),
            false,
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Brew { status: 3, .. }), "{err:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("brew update failed (exit 3)"), "{text}");
        assert!(dir.path().join("state.toml").is_file());
    }

    #[test]
    fn outdated_lists_no_soak_from_brews_view_without_updating() {
        let (mut brew, git, snaps) = view_world();
        brew.installed.push(formula_pkg_from(
            "brewsoak",
            "ericfitz/tap",
            formula_rb("brewsoak", "1.0.0", "b"),
        ));
        brew.taps.push(tapped(
            "ericfitz/tap",
            Some("https://github.com/ericfitz/homebrew-tap"),
        ));
        brew.outdated = vec!["brewsoak".into(), "alpha".into()];
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("==> No-soak (brew)\nbrewsoak (no-soak, brew)"),
            "{text}"
        );
        assert!(
            !text.contains("alpha (no-soak"),
            "soaked packages are not listed from brew's view: {text}"
        );
        assert!(!run_has_token(&lock_runs(&brew), "update"));
    }

    #[test]
    fn info_shows_origin_hours_and_no_soak() {
        let (mut brew, git, snaps) = view_world();
        brew.installed.push(formula_pkg_from(
            "brewsoak",
            "ericfitz/tap",
            formula_rb("brewsoak", "1.0.0", "b"),
        ));
        brew.taps.push(tapped(
            "ericfitz/tap",
            Some("https://github.com/ericfitz/homebrew-tap"),
        ));
        let cfg = cfg_with(
            "NO_SOAK = [\"ericfitz/tap\"]\n[[TAP]]\nname = \"homebrew/core\"\nsoak_hours = 48\n",
        );
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        info(
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["alpha".into(), "brewsoak".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("alpha\ninstalled: 1.0.0\ncutoff: 1.1.0\nhead: 1.2.0\norigin: homebrew/core\nsoak hours: 48\naction: would upgrade"),
            "{text}"
        );
        assert!(
            text.contains("brewsoak\ninstalled: 1.0.0\n")
                && text.contains(
                    "origin: ericfitz/tap\nsoak: no-soak (brew decides)\naction: no-soak"
                ),
            "{text}"
        );
    }

    #[test]
    fn upgrade_summary_golden() {
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
                    formula_rb("brewsoak", "1.0.0", "b"),
                ),
                formula_pkg_from("thing", "local/tap", formula_rb("thing", "1.0.0", "t")),
                formula_pkg_from(
                    "terraform",
                    "hashicorp/tap",
                    formula_rb("terraform", "1.0.0", "t"),
                ),
                formula_pkg_pinned("curl", formula_rb("curl", "1.0.0", "c")),
            ],
            taps: vec![
                tapped(
                    "ericfitz/tap",
                    Some("https://github.com/ericfitz/homebrew-tap"),
                ),
                tapped("local/tap", None),
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
            ],
            next_stdout: b"\xf0\x9f\x8d\xba  /opt/homebrew/Cellar/alpha/1.1.0: 5 files, 1MB\n"
                .to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut snaps = core_snaps();
        snaps.held_taps.insert(
            "hashicorp/tap".into(),
            "while fetching history from origin to find the tap's soak cutoff, git failed:\nfatal: unable to access".into(),
        );
        let tap = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let got = String::from_utf8(out).unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upgrade_summary.txt");
        if std::env::var_os("BREWSOAK_BLESS").is_some() {
            std::fs::write(&path, &got).unwrap();
        }
        let want = std::fs::read_to_string(&path)
            .expect("fixture missing; run with BREWSOAK_BLESS=1 to create it");
        assert_eq!(
            got, want,
            "upgrade summary changed; rerun with BREWSOAK_BLESS=1 if intended"
        );
        assert!(got.contains("no-soak 1"), "{got}");
        assert!(got.contains("notes:"), "{got}");
        assert!(
            got.contains("thing: tap local/tap has no HTTPS remote"),
            "{got}"
        );
        assert!(
            got.contains("terraform: tap hashicorp/tap could not be refreshed"),
            "{got}"
        );
    }

    #[test]
    fn outdated_lists_a_pinned_no_soak_package_under_pinned() {
        let brew = MockBrew {
            installed: vec![formula_pkg_pinned("wget", formula_rb("wget", "1.0.0", "a"))],
            outdated: vec!["wget".into()],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(
            &brew,
            &InMemoryGit::new(),
            &core_snaps(),
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("==> Pinned\nwget"), "{text}");
        assert!(!text.contains("wget (no-soak"), "{text}");
    }
}
