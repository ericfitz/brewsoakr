use super::apply::staging_dir;
use super::bare::snapshotted_pkgs;
use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::eligibility::{self, DesiredAction};
use crate::git::GitStore;
use crate::identity::{self, HostPlatform, PkgIdentity};
use crate::inventory::{self, Inventory, PkgClass};
use crate::origin::{self};
use crate::report::{self};
use crate::resolve::{self, PkgKind, PkgRef};
use crate::snapshot::Snapshots;
use crate::tap;
use crate::taps;
use std::path::{Path, PathBuf};

/// The installed and cutoff versions of a view, for notes.
pub(super) fn view_versions(view: &ResolvedView) -> (String, String) {
    let version = |id: &Option<PkgIdentity>| {
        id.as_ref()
            .map(report::identity_version)
            .unwrap_or_else(|| "?".into())
    };
    (version(&view.installed), version(&view.cutoff))
}

/// origin + name + class for a command-line token, installed or not.
pub(super) struct Resolved {
    pub(super) origin: String,
    pub(super) name: String,
    pub(super) class: PkgClass,
    pub(super) receipt_tap: Option<String>,
    /// origin is core/cask or appears in `brew tap-info --installed`.
    pub(super) tapped: bool,
    /// The token spelled out `user/tap/name`.
    pub(super) named_origin: bool,
}

pub(super) fn resolve_token(raw: &str, inv: &Inventory, cfg: &Config) -> Result<Resolved, Error> {
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
pub(super) fn validate_tokens(names: &[String]) -> Result<(), Error> {
    names
        .iter()
        .try_for_each(|raw| inventory::parse_token(raw).map(drop))
}

/// A bare name that resolved as a cask belongs to `homebrew/cask`, which may
/// itself be on the `NO_SOAK` list.
pub(super) fn settle_origin(r: &mut Resolved, kind: PkgKind, inv: &Inventory, cfg: &Config) {
    if origin::is_core_or_cask(&r.origin) {
        r.origin = origin::default_origin(kind).to_string();
        r.class = inv.class_for(&r.origin, &r.name, cfg);
    }
}

pub(super) struct ResolvedView {
    pub(super) installed: Option<PkgIdentity>,
    pub(super) cutoff: Option<PkgIdentity>,
    pub(super) head: Option<PkgIdentity>,
    pub(super) action: DesiredAction,
    pub(super) cutoff_blob: Option<Vec<u8>>,
    pub(super) warnings: Vec<String>,
    /// The HEAD cask says `auto_updates true`.
    pub(super) auto_updates: bool,
    /// The cutoff cask says `version :latest`. Read from the cutoff, not
    /// HEAD: the cutoff is what brewsoak would install, and a cask whose
    /// cutoff moved from `:latest` to a number soaks like any other.
    pub(super) latest: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_view(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    origin_tap: &str,
    name: &str,
    kind: PkgKind,
    receipt_rb: Option<&str>,
) -> Result<Option<ResolvedView>, Error> {
    let host = brew.host_platform();
    let Some(mut view) = resolve_view_unordered(
        brew,
        git,
        snaps,
        cache,
        origin_tap,
        name,
        kind,
        receipt_rb,
        host.as_ref(),
    )?
    else {
        return Ok(None);
    };
    settle_order(brew, kind, &mut view);
    Ok(Some(view))
}

/// brewsoak never downgrades. `desired_action` says `InstallCutoff` for any
/// installed version that differs from the cutoff, so ask brew which side is
/// newer: an installed version strictly newer than the cutoff is left alone,
/// and a comparison brew cannot make holds the package.
pub(super) fn settle_order(brew: &impl Brew, kind: PkgKind, view: &mut ResolvedView) {
    let Some(pair) = order_pair(kind, view) else {
        return;
    };
    let order = brew
        .compare_versions(std::slice::from_ref(&pair))
        .ok()
        .and_then(|answers| answers.into_iter().next());
    view.action = match order {
        Some(std::cmp::Ordering::Greater) => DesiredAction::LeaveNewerThanCutoff,
        Some(_) => DesiredAction::InstallCutoff,
        None => DesiredAction::HoldUnordered,
    };
}

/// The versions to order for a would-be install over an installed version.
/// An equal version string (a different artifact, same version) is an
/// upgrade, and `:latest` has no order.
pub(super) fn order_pair(kind: PkgKind, view: &ResolvedView) -> Option<crate::brew::VersionPair> {
    if view.action != DesiredAction::InstallCutoff {
        return None;
    }
    let mut installed = report::identity_version(view.installed.as_ref()?);
    let mut cutoff = report::identity_version(view.cutoff.as_ref()?);
    if kind == PkgKind::Cask {
        // Everything after the first comma is a build token (`3.39.11,dy27w…,a`)
        // that `Version.new` orders lexically, which says nothing about age.
        for v in [&mut installed, &mut cutoff] {
            *v = v.split(',').next().unwrap_or_default().to_string();
        }
    }
    let latest = |v: &str| v == ":latest" || v == "latest";
    if installed == cutoff || latest(&installed) || latest(&cutoff) {
        return None;
    }
    Some((kind, installed, cutoff))
}

/// Ask brew for the version of every url-derived formula a bare run will
/// parse (installed receipt, cutoff and HEAD) in one call, so each parse finds
/// its answer memoized. A failure is left for each parse to meet and fall back.
pub(super) fn prefetch_versions(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
    host: Option<&HostPlatform>,
) {
    let mut sources: Vec<identity::VersionSource> = Vec::new();
    let mut add = |rb: &str| {
        if let Ok(id) = identity::parse_formula(rb, host)
            && let Some(src) = id.derived_from
            && !sources.contains(&src)
        {
            sources.push(src);
        }
    };
    // Every package a caller can resolve, pinned ones included: `info` and
    // `update` resolve them though a bare upgrade leaves them alone.
    for pkg in snapshotted_pkgs(inv, snaps).filter(|p| p.kind == PkgKind::Formula) {
        add(&pkg.receipt_rb);
        if let Ok(blobs) = resolve_pkg_blobs(git, snaps, cache, &pkg.origin, &pkg.name, pkg.kind) {
            for bytes in [blobs.cutoff.as_deref(), blobs.head.as_deref()]
                .into_iter()
                .flatten()
            {
                if let Ok(rb) = std::str::from_utf8(bytes) {
                    add(rb);
                }
            }
        }
    }
    if !sources.is_empty() {
        let _ = brew.detect_versions(&sources);
    }
}

/// Ask brew for every version order a bare run will need in one call, so
/// `resolve_view` finds them memoized. A failure here is left for each
/// package's own comparison to report.
pub(super) fn prefetch_orderings(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
) {
    let mut pairs = Vec::new();
    let host = brew.host_platform();
    prefetch_versions(brew, git, snaps, cache, inv, host.as_ref());
    for pkg in snapshotted_pkgs(inv, snaps) {
        if let Ok(Some(view)) = resolve_view_unordered(
            brew,
            git,
            snaps,
            cache,
            &pkg.origin,
            &pkg.name,
            pkg.kind,
            Some(&pkg.receipt_rb),
            host.as_ref(),
        ) && let Some(pair) = order_pair(pkg.kind, &view)
        {
            pairs.push(pair);
        }
    }
    if !pairs.is_empty() {
        let _ = brew.compare_versions(&pairs);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_view_unordered(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    origin_tap: &str,
    name: &str,
    kind: PkgKind,
    receipt_rb: Option<&str>,
    host: Option<&HostPlatform>,
) -> Result<Option<ResolvedView>, Error> {
    let own_host = host_for_pkg(brew, kind, name, host);
    let host = own_host.as_ref();
    let blobs = resolve_pkg_blobs(git, snaps, cache, origin_tap, name, kind)?;
    // The three sides are parsed together so a formula's url-derived versions
    // are asked of brew in one call, and all resolve or none do.
    fn text(bytes: &Option<Vec<u8>>) -> Option<Result<&str, ()>> {
        bytes
            .as_deref()
            .map(|b| std::str::from_utf8(b).map_err(|_| ()))
    }
    let (Some(cutoff_rb), Some(head_rb)) = (
        text(&blobs.cutoff).transpose().ok(),
        text(&blobs.head).transpose().ok(),
    ) else {
        return Ok(None);
    };
    let sides = [receipt_rb, cutoff_rb, head_rb];
    let present: Vec<&str> = sides.iter().flatten().copied().collect();
    let mut parsed = parse_pkgs(brew, kind, &present, host).into_iter();
    let mut next_side = |side: Option<&str>| -> Result<Option<PkgIdentity>, ()> {
        match side {
            Some(_) => parsed.next().and_then(Result::ok).map(Some).ok_or(()),
            None => Ok(None),
        }
    };
    let (Ok(installed), Ok(cutoff), Ok(head)) = (
        next_side(sides[0]),
        next_side(sides[1]),
        next_side(sides[2]),
    ) else {
        return Ok(None);
    };
    let today = calendar_today();
    let status =
        eligibility::upstream_status(blobs.cutoff.as_deref(), blobs.head.as_deref(), &today, host);
    let action =
        eligibility::desired_action(status, installed.as_ref(), cutoff.as_ref(), head.as_ref());
    let head_rb = blobs
        .head
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok());
    let warnings = match head_rb {
        Some(rb) => identity::upcoming_lifecycle_messages(rb, &today, host)
            .into_iter()
            .map(|msg| format!("{name} {msg}"))
            .collect(),
        None => Vec::new(),
    };
    let auto_updates =
        kind == PkgKind::Cask && head_rb.is_some_and(|rb| identity::cask_auto_updates(rb, host));
    let latest = cutoff.as_ref().is_some_and(PkgIdentity::is_latest_cask);
    Ok(Some(ResolvedView {
        installed,
        cutoff,
        head,
        action,
        cutoff_blob: blobs.cutoff,
        warnings,
        auto_updates,
        latest,
    }))
}

pub(super) fn calendar_today() -> String {
    let d = time::OffsetDateTime::now_utc().date();
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

/// Which snapshot a package resolves against: core and cask use their own
/// repos; a tap uses its clone and the tap's state in `state.toml`. Only the
/// state's `cutoff_sha` is ever the cutoff; a stale ref in the clone is not.
pub(super) fn resolve_pkg_blobs(
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

pub(super) fn cutoff_blob_exists(
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

pub(super) fn natural_kind(
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

pub(super) fn write_cutoff_blob(
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

pub(super) fn tap_repo(cache: &Path, kind: PkgKind) -> PathBuf {
    match kind {
        PkgKind::Formula => cache.join("core.git"),
        PkgKind::Cask => cache.join("cask.git"),
    }
}

/// `host` with the languages `brew upgrade` evaluates a cask with, so every
/// side of that cask picks the `language` block it would: a `--language` flag
/// first (brew's args-explicit config), then the installed cask's saved
/// `explicit` languages, then the live run-level list (`Cask::Config.new`:
/// `HOMEBREW_CASK_OPTS` plus the system default), deduplicated, with those
/// `Locale` cannot parse dropped. Saved `env` and `default` are not used:
/// `Cask::Config#merge` discards them. Formulae keep the run-level host.
pub(super) fn host_for_pkg(
    brew: &impl Brew,
    kind: PkgKind,
    name: &str,
    host: Option<&HostPlatform>,
) -> Option<HostPlatform> {
    let mut host = host.cloned();
    if kind == PkgKind::Cask
        && let Some(h) = host.as_mut()
    {
        let mut languages: Vec<String> = Vec::new();
        for lang in brew
            .cli_languages()
            .into_iter()
            .chain(brew.cask_languages(name).unwrap_or_default())
            .chain(std::mem::take(&mut h.languages))
        {
            if identity::is_valid_language(&lang) && !languages.contains(&lang) {
                languages.push(lang);
            }
        }
        h.languages = languages;
    }
    host
}

/// `host` resolves platform blocks for both kinds; `None` leaves them opaque.
/// A formula version derived from its url is brew's `Version.detect` of it.
pub(super) fn parse_pkg(
    brew: &impl Brew,
    kind: PkgKind,
    rb: &str,
    host: Option<&HostPlatform>,
) -> Result<PkgIdentity, Error> {
    match kind {
        PkgKind::Formula => Ok(PkgIdentity::Formula(parse_formula_detected(
            brew, rb, host,
        )?)),
        PkgKind::Cask => Ok(PkgIdentity::Cask(identity::parse_cask(rb, host)?)),
    }
}

/// `identity::parse_formula`, with a derived version replaced by brew's
/// answer. A failed or empty answer keeps the local guess.
pub(super) fn parse_formula_detected(
    brew: &impl Brew,
    rb: &str,
    host: Option<&HostPlatform>,
) -> Result<identity::FormulaIdentity, Error> {
    let mut id = identity::parse_formula(rb, host)?;
    detect_versions_of(brew, &mut [&mut id]);
    Ok(id)
}

/// Parses several packages of one kind, in order, asking brew for all their
/// url-derived formula versions in one call. When that call fails, every
/// formula keeps its local guess, so no set of related identities mixes
/// brew's version with a guess.
pub(super) fn parse_pkgs(
    brew: &impl Brew,
    kind: PkgKind,
    rbs: &[&str],
    host: Option<&HostPlatform>,
) -> Vec<Result<PkgIdentity, Error>> {
    match kind {
        PkgKind::Cask => rbs
            .iter()
            .map(|rb| identity::parse_cask(rb, host).map(PkgIdentity::Cask))
            .collect(),
        PkgKind::Formula => {
            let mut parsed: Vec<Result<identity::FormulaIdentity, Error>> = rbs
                .iter()
                .map(|rb| identity::parse_formula(rb, host))
                .collect();
            let mut ids: Vec<&mut identity::FormulaIdentity> =
                parsed.iter_mut().filter_map(|r| r.as_mut().ok()).collect();
            detect_versions_of(brew, &mut ids);
            parsed
                .into_iter()
                .map(|r| r.map(PkgIdentity::Formula))
                .collect()
        }
    }
}

/// Replaces the local guess of each url-derived version with brew's answer,
/// from one `detect_versions` call. An error leaves every guess alone; a
/// source brew cannot tell leaves only its own.
pub(super) fn detect_versions_of(brew: &impl Brew, ids: &mut [&mut identity::FormulaIdentity]) {
    let mut sources: Vec<identity::VersionSource> = Vec::new();
    for src in ids.iter().filter_map(|id| id.derived_from.as_ref()) {
        if !sources.contains(src) {
            sources.push(src.clone());
        }
    }
    if sources.is_empty() {
        return;
    }
    let Ok(answers) = brew.detect_versions(&sources) else {
        return;
    };
    for id in ids.iter_mut() {
        let Some(src) = &id.derived_from else {
            continue;
        };
        if let Some(i) = sources.iter().position(|s| s == src)
            && let Some(Some(v)) = answers.get(i)
            && !v.is_empty()
        {
            id.version = v.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::brew::MockBrew;

    use crate::eligibility::DesiredAction;
    use crate::git::InMemoryGit;

    use crate::resolve::PkgKind;

    use crate::cmd::apply::reinstall;
    use crate::cmd::test_support::{
        OUTDATED_HEADER, cask_rb, cfg24, core_snaps, formula_pkg, formula_rb, greedy_outdated,
        greedy_upgrade, greedy_world, inv_from, lock_runs, many_detect_world, openssl_is_newer,
        openssl_repoint_world, run_is_soaked_install, section_lines, spawns, unused_cache,
        upgrade_names,
    };
    use crate::cmd::upgrade::upgrade;

    fn cask_view(installed: &str, cutoff: &str) -> ResolvedView {
        let id = |v: &str| {
            PkgIdentity::Cask(crate::identity::CaskIdentity {
                version: v.into(),
                sha256: Some("abc".into()),
                url: None,
            })
        };
        ResolvedView {
            installed: Some(id(installed)),
            cutoff: Some(id(cutoff)),
            head: None,
            action: DesiredAction::InstallCutoff,
            cutoff_blob: None,
            warnings: Vec::new(),
            auto_updates: false,
            latest: false,
        }
    }

    #[test]
    fn cask_build_tokens_after_the_first_comma_are_not_ordered() {
        // `Version.new` compares `dy27whJwwmb` and `8QQF2ciKqxM` lexically, so
        // a same-version re-release would look older or newer by token.
        let a = "3.39.11,dy27whJwwmb,a";
        let b = "3.39.11,8QQF2ciKqxM,a";
        for (installed, cutoff) in [(a, b), (b, a)] {
            let view = cask_view(installed, cutoff);
            assert!(order_pair(PkgKind::Cask, &view).is_none(), "{installed}");
            let mut view = view;
            let brew = MockBrew::new();
            settle_order(&brew, PkgKind::Cask, &mut view);
            assert_eq!(view.action, DesiredAction::InstallCutoff);
            assert!(brew.compares.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn cask_versions_differing_in_the_first_field_are_still_ordered() {
        let view = cask_view("3.39.11,x,a", "3.40.0,y,a");
        let pair = order_pair(PkgKind::Cask, &view).expect("ordered");
        assert_eq!((pair.1.as_str(), pair.2.as_str()), ("3.39.11", "3.40.0"));
    }

    const OPENSSL_NOTE: &str = "openssl@3: installed 3.6.5 is newer than cutoff 3.5.9; leaving it (upstream may have re-pointed it)";

    #[test]
    fn bare_upgrade_never_downgrades_a_repointed_formula() {
        let (mut brew, git, snaps) = openssl_repoint_world();
        openssl_is_newer(&mut brew);
        let text = greedy_upgrade(&brew, &git, &snaps, &[]);
        let runs = lock_runs(&brew);
        assert!(!run_is_soaked_install(&runs, "openssl@3"), "{text}");
        assert!(text.contains("upgrading 0 of 1 packages"), "{text}");
        assert!(text.contains(OPENSSL_NOTE), "{text}");
        assert!(text.contains("ahead 1"), "{text}");
        assert!(text.contains("upgraded 0"), "{text}");
    }

    #[test]
    fn named_upgrade_and_reinstall_of_a_repointed_formula_do_not_install() {
        let (mut brew, git, snaps) = openssl_repoint_world();
        openssl_is_newer(&mut brew);
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let names = vec!["openssl@3".to_string()];
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &names,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !r.refused,
            "named upgrade is left like ahead-of-soak: {text}"
        );
        assert!(text.contains(OPENSSL_NOTE), "{text}");
        let mut out = Vec::new();
        let r = reinstall(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &names,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused, "{text}");
        assert!(text.contains("reinstall would downgrade"), "{text}");
        assert!(
            !run_is_soaked_install(&lock_runs(&brew), "openssl@3"),
            "{text}"
        );
    }

    #[test]
    fn a_failed_version_comparison_holds_the_package() {
        let (mut brew, git, snaps) = openssl_repoint_world();
        brew.compare_error = Some("brew ruby exploded".into());
        let text = greedy_upgrade(&brew, &git, &snaps, &[]);
        assert!(
            !run_is_soaked_install(&lock_runs(&brew), "openssl@3"),
            "{text}"
        );
        assert!(
            text.contains(
                "openssl@3: brew could not compare installed 3.6.5 with cutoff 3.5.9; holding it"
            ),
            "{text}"
        );
        assert!(text.contains("held 1"), "{text}");
        let text = greedy_outdated(&brew, &git, &snaps, &[]);
        assert!(section_lines(&text, OUTDATED_HEADER).is_empty(), "{text}");
        assert_eq!(section_lines(&text, "==> Held").len(), 1, "{text}");
    }

    #[test]
    fn installed_between_cutoff_and_head_is_left_alone() {
        let (mut brew, git, snaps) = openssl_repoint_world();
        git.insert_blob(
            "headsha",
            "Formula/o/openssl@3.rb",
            formula_rb("openssl@3", "3.7.0", "sha370"),
        );
        openssl_is_newer(&mut brew);
        let text = greedy_upgrade(&brew, &git, &snaps, &[]);
        assert!(
            !run_is_soaked_install(&lock_runs(&brew), "openssl@3"),
            "{text}"
        );
        assert!(text.contains(OPENSSL_NOTE), "{text}");
    }

    #[test]
    fn same_version_with_a_different_artifact_still_upgrades_without_asking_brew() {
        let (mut brew, git, snaps) = openssl_repoint_world();
        brew.installed = vec![formula_pkg(
            "openssl@3",
            formula_rb("openssl@3", "3.5.9", "oldsha"),
        )];
        let text = greedy_upgrade(&brew, &git, &snaps, &[]);
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "openssl@3"),
            "{text}"
        );
        assert!(
            brew.compares.lock().unwrap().is_empty(),
            "equal version strings are not compared"
        );
    }

    #[test]
    fn bare_upgrade_asks_brew_once_and_never_about_latest_casks() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy"]);
        let compares = brew.compares.lock().unwrap().clone();
        let first = compares.first().expect("a prefetch call");
        let asked: Vec<&str> = first.iter().map(|(_, a, _)| a.as_str()).collect();
        assert_eq!(asked, ["1.0.0", "7.38.1"], "wget and alt-tab only: {text}");
        assert!(
            compares.iter().skip(1).all(|c| c.len() == 1),
            "later asks are per-package"
        );
    }

    /// stockfish installed at `sf_18`, cutoff and HEAD at `sf_19`; brew's
    /// `Version.detect` calls them 18 and 19. `crafty` is url-derived too.
    fn detect_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let git = InMemoryGit::new();
        let new = formula_rb("stockfish", "sf_19", "sha19");
        git.insert_blob("cutoffsha", "Formula/s/stockfish.rb", new.clone());
        git.insert_blob("headsha", "Formula/s/stockfish.rb", new);
        let crafty = formula_rb("crafty", "25.3", "craftynew");
        git.insert_blob("cutoffsha", "Formula/c/crafty.rb", crafty.clone());
        git.insert_blob("headsha", "Formula/c/crafty.rb", crafty);
        let mut brew = MockBrew {
            installed: vec![
                formula_pkg("stockfish", formula_rb("stockfish", "sf_18", "sha18")),
                formula_pkg("crafty", formula_rb("crafty", "25.2", "craftyold")),
            ],
            ..MockBrew::new()
        };
        for (url, v) in [
            ("stockfish-sf_18", "18"),
            ("stockfish-sf_19", "19"),
            ("crafty-25.2", "25.2"),
            ("crafty-25.3", "25.3"),
        ] {
            brew.detections.insert(
                (format!("https://example.com/{url}.tar.gz"), None),
                v.to_string(),
            );
        }
        (brew, git, core_snaps())
    }

    fn stockfish_pairs(brew: &MockBrew) -> Vec<(String, String)> {
        brew.compares
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .filter(|(_, a, _)| a.contains("18"))
            .map(|(_, a, b)| (a.clone(), b.clone()))
            .collect()
    }

    #[test]
    fn a_url_derived_version_comes_from_brew_not_the_heuristic() {
        let (brew, git, snaps) = detect_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &[]);
        assert!(text.contains("18 -> 19"), "{text}");
        assert!(!text.contains("sf_"), "{text}");
        let pairs = stockfish_pairs(&brew);
        assert!(!pairs.is_empty(), "{text}");
        assert!(
            pairs.iter().all(|p| *p == ("18".into(), "19".into())),
            "{pairs:?}"
        );
    }

    #[test]
    fn a_failed_detection_keeps_the_heuristic_version() {
        let (mut brew, git, snaps) = detect_world();
        brew.detect_error = Some("brew ruby exploded".into());
        let text = greedy_upgrade(&brew, &git, &snaps, &[]);
        assert!(
            text.contains("stockfish-sf_18 -> stockfish-sf_19"),
            "{text}"
        );
        let pairs = stockfish_pairs(&brew);
        assert!(!pairs.is_empty(), "{text}");
        assert!(
            pairs
                .iter()
                .all(|p| *p == ("stockfish-sf_18".into(), "stockfish-sf_19".into())),
            "{pairs:?}"
        );
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "stockfish"),
            "{text}"
        );
    }

    #[test]
    fn a_bare_run_detects_every_derived_version_in_one_call_first() {
        let (brew, git, snaps) = detect_world();
        greedy_upgrade(&brew, &git, &snaps, &[]);
        let detects = brew.detects.lock().unwrap().clone();
        let first = detects.first().expect("a prefetch call");
        let urls: std::collections::BTreeSet<&str> = first.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            urls.len(),
            4,
            "receipts and cutoffs of both formulae in one call: {first:?}"
        );
        assert_eq!(first.len(), 4, "no duplicates: {first:?}");
        assert_eq!(
            spawns(&brew).0,
            1,
            "later asks are per-view memo hits in brew: {detects:?}"
        );
    }

    #[test]
    fn a_named_upgrade_detects_once_per_resolved_view() {
        let (brew, git) = many_detect_world(&["app", "bar", "baz"]);
        let text = upgrade_names(&brew, &git, &["app".to_string()]);
        assert!(text.contains("bar d1.0.0-bar -> d1.1.0-bar"), "{text}");
        // app, bar and baz are three resolved views; each asks brew about
        // its installed, cutoff and HEAD urls in one call.
        let (detects, _) = spawns(&brew);
        assert!(detects <= 3, "{detects} detect spawns: {text}");
    }

    #[test]
    fn a_failed_detection_never_mixes_detected_and_heuristic_sides() {
        for failing in 1..=3usize {
            let (mut brew, git) = many_detect_world(&["app", "bar", "baz"]);
            brew.detect_fail_spawns.insert(failing);
            let text = upgrade_names(&brew, &git, &["app".to_string()]);
            let compares = brew.compares.lock().unwrap().clone();
            for (_, a, b) in compares.iter().flatten() {
                assert_eq!(
                    a.starts_with('d'),
                    b.starts_with('d'),
                    "spawn {failing} failed: {a} vs {b}\n{text}"
                );
            }
            for line in text.lines().filter(|l| l.starts_with("upgrading ")) {
                assert_eq!(
                    line.contains(" d1."),
                    line.contains("-> d1."),
                    "spawn {failing} failed: {line}"
                );
            }
        }
    }

    #[test]
    fn a_head_with_its_own_url_is_detected_in_the_prefetch_call() {
        let (brew, git) = many_detect_world(&["app"]);
        greedy_upgrade(&brew, &git, &core_snaps(), &[]);
        let first = brew.detects.lock().unwrap().first().cloned().unwrap();
        let urls: Vec<&str> = first.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(first.len(), 3, "{urls:?}");
        assert!(
            urls.contains(&"https://example.com/app-1.2.0.tar.gz"),
            "{urls:?}"
        );
        assert_eq!(spawns(&brew).0, 1);
    }

    #[test]
    fn detections_are_keyed_by_url_and_tag() {
        let rb = |tag: &str| {
            format!(
                "class X < Formula\n  url \"https://example.com/x.git\", tag: \"{tag}\", revision: \"r{tag}\"\nend\n"
            )
        };
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/x/x.rb", rb("rel-2"));
        git.insert_blob("headsha", "Formula/x/x.rb", rb("rel-2"));
        let mut brew = MockBrew {
            installed: vec![formula_pkg("x", rb("rel-1"))],
            ..MockBrew::new()
        };
        for (tag, v) in [("rel-1", "A1"), ("rel-2", "A2")] {
            brew.detections.insert(
                ("https://example.com/x.git".into(), Some(tag.into())),
                v.into(),
            );
        }
        // A detection for the bare url must not answer a tagged source.
        brew.detections
            .insert(("https://example.com/x.git".into(), None), "WRONG".into());
        let text = greedy_upgrade(&brew, &git, &core_snaps(), &[]);
        assert!(text.contains("A1 -> A2"), "{text}");
    }

    #[test]
    fn resolve_view_resolves_cask_platform_blocks_with_the_hosts_from_brew() {
        let ngrok = include_str!("../../tests/fixtures/ngrok.rb");
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/n/ngrok.rb", ngrok);
        git.insert_blob("caskhead", "Casks/n/ngrok.rb", ngrok);
        let resolve = |brew: &MockBrew| {
            resolve_view(
                brew,
                &git,
                &core_snaps(),
                unused_cache(),
                "homebrew/cask",
                "ngrok",
                PkgKind::Cask,
                None,
            )
            .unwrap()
        };
        let mut brew = MockBrew::new();
        assert!(resolve(&brew).is_none(), "no host: blocks stay opaque");
        brew.host = HostPlatform::parse("sequoia 15\nHOST sequoia arm64\n");
        let view = resolve(&brew).expect("parseable on a known host");
        assert_eq!(
            report::identity_version(view.cutoff.as_ref().unwrap()),
            "3.39.11,dy27whJwwmb,a"
        );
    }

    #[test]
    fn resolve_view_evaluates_a_cask_with_its_saved_languages() {
        let cave = include_str!("../../tests/fixtures/cave-story.rb");
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/c/cave-story.rb", cave);
        git.insert_blob("caskhead", "Casks/c/cave-story.rb", cave);
        let receipt = crate::brew::version_only_cask_receipt("cave-story", "0.2.0");
        let mut brew = MockBrew::new();
        brew.host = HostPlatform::parse("sequoia 15\nHOST sequoia arm64\nLANG en-US\n");
        let resolve = |brew: &MockBrew| {
            resolve_view(
                brew,
                &git,
                &core_snaps(),
                unused_cache(),
                "homebrew/cask",
                "cave-story",
                PkgKind::Cask,
                Some(&receipt),
            )
            .unwrap()
            .expect("resolvable")
        };
        let en = resolve(&brew);
        assert_eq!(
            report::identity_version(en.cutoff.as_ref().unwrap()),
            "0.1.0,2"
        );
        assert_ne!(en.action, DesiredAction::NoOpAlreadySoaked);
        brew.cask_languages
            .insert("cave-story".into(), vec!["ja".into()]);
        let ja = resolve(&brew);
        assert_eq!(
            report::identity_version(ja.cutoff.as_ref().unwrap()),
            "0.2.0"
        );
        assert_eq!(report::identity_version(ja.head.as_ref().unwrap()), "0.2.0");
        assert_eq!(ja.action, DesiredAction::NoOpAlreadySoaked);
    }

    fn host_langs(brew: &MockBrew, name: &str, kind: PkgKind) -> Vec<String> {
        host_for_pkg(brew, kind, name, brew.host_platform().as_ref())
            .expect("host")
            .languages
    }

    #[test]
    fn host_for_pkg_puts_saved_explicit_before_the_live_run_languages() {
        let mut brew = MockBrew::new();
        brew.host = HostPlatform::parse("sequoia 15\nHOST sequoia arm64\nLANG en-US fr\n");
        // Nothing saved for the cask: the run-level list.
        assert_eq!(host_langs(&brew, "firefox", PkgKind::Cask), ["en-US", "fr"]);
        // Saved `explicit` (all `read_cask_languages` returns) leads, then the
        // live list, deduplicated, invalid dropped.
        brew.cask_languages
            .insert("firefox".into(), vec!["de".into(), "fr".into()]);
        assert_eq!(
            host_langs(&brew, "firefox", PkgKind::Cask),
            ["de", "fr", "en-US"]
        );
        // A formula never takes cask languages.
        brew.cask_languages.insert("wget".into(), vec!["de".into()]);
        assert_eq!(host_langs(&brew, "wget", PkgKind::Formula), ["en-US", "fr"]);
    }

    #[test]
    fn host_for_pkg_puts_the_language_flag_before_everything() {
        let mut brew = MockBrew::new();
        brew.host = HostPlatform::parse("sequoia 15\nHOST sequoia arm64\nLANG en-US\n");
        brew.cli_languages = vec!["de".into(), "bad_one".into(), "fr".into()];
        brew.cask_languages
            .insert("firefox".into(), vec!["fr".into(), "ja".into()]);
        assert_eq!(
            host_langs(&brew, "firefox", PkgKind::Cask),
            ["de", "fr", "ja", "en-US"]
        );
        // The flag applies to a cask with nothing saved too.
        assert_eq!(
            host_langs(&brew, "other", PkgKind::Cask),
            ["de", "fr", "en-US"]
        );
    }

    #[test]
    fn a_saved_default_language_does_not_pick_the_block() {
        // Saved config {"default":{"languages":["en-US"... fr]}} reads as no
        // explicit languages, so the live run-level en-US picks the block.
        let cave = include_str!("../../tests/fixtures/cave-story.rb");
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/c/cave-story.rb", cave);
        git.insert_blob("caskhead", "Casks/c/cave-story.rb", cave);
        let receipt = crate::brew::version_only_cask_receipt("cave-story", "0.2.0");
        let mut brew = MockBrew::new();
        brew.host = HostPlatform::parse("sequoia 15\nHOST sequoia arm64\nLANG en-US\n");
        let version = |brew: &MockBrew| {
            let v = resolve_view(
                brew,
                &git,
                &core_snaps(),
                unused_cache(),
                "homebrew/cask",
                "cave-story",
                PkgKind::Cask,
                Some(&receipt),
            )
            .unwrap()
            .expect("resolvable");
            report::identity_version(v.cutoff.as_ref().unwrap())
        };
        assert_eq!(version(&brew), "0.1.0,2");
        brew.cli_languages = vec!["ja".into()];
        assert_eq!(version(&brew), "0.2.0");
    }

    #[test]
    fn resolve_view_reads_latest_from_the_cutoff_cask() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "caskcut",
            "Casks/n/nightly.rb",
            "cask \"nightly\" do\n  version :latest\n  sha256 :no_check\n  url \"https://example.com/nightly.dmg\"\nend\n",
        );
        git.insert_blob("caskhead", "Casks/n/nightly.rb", cask_rb("nightly", "2.0"));
        let receipt = crate::brew::version_only_cask_receipt("nightly", "latest");
        let view = resolve_view(
            &MockBrew::new(),
            &git,
            &core_snaps(),
            unused_cache(),
            "homebrew/cask",
            "nightly",
            PkgKind::Cask,
            Some(&receipt),
        )
        .unwrap()
        .expect("parseable");
        assert!(view.latest, "cutoff is :latest");
        assert_eq!(view.action, DesiredAction::NoOpAlreadySoaked);
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/n/nightly.rb", cask_rb("nightly", "2.0"));
        git.insert_blob("caskhead", "Casks/n/nightly.rb", cask_rb("nightly", "2.0"));
        let view = resolve_view(
            &MockBrew::new(),
            &git,
            &core_snaps(),
            unused_cache(),
            "homebrew/cask",
            "nightly",
            PkgKind::Cask,
            Some(&receipt),
        )
        .unwrap()
        .expect("parseable");
        assert!(!view.latest, "a cutoff that moved to a number is soakable");
        assert_eq!(view.action, DesiredAction::InstallCutoff);
    }
}
