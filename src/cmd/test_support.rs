use crate::Error;
use crate::SoakHours;
use crate::brew::{Brew, InstalledPkg, MockBrew};
use crate::cmd::RunResult;
use crate::cmd::apply::reinstall;
use crate::cmd::outdated::outdated;
use crate::cmd::upgrade::upgrade;
use crate::config::Config;
use crate::flags;
use crate::git::InMemoryGit;
use crate::inventory::Inventory;
use crate::origin::OriginRecords;
use crate::resolve::PkgKind;
use crate::snapshot::Snapshots;
use crate::snapshot::{TapSnapshot, TapState};
use crate::taps::TapInfo;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// True when the Summary has a `label  value` row (value ignoring any suffix).
pub(super) fn summary_has(text: &str, label: &str, value: &str) -> bool {
    text.lines().any(|l| {
        l.strip_prefix("  ")
            .and_then(|l| l.strip_prefix(label))
            .is_some_and(|rest| {
                rest.starts_with(' ') && rest.split_whitespace().next() == Some(value)
            })
    })
}

pub(super) fn cfg24() -> Config {
    Config::uniform(SoakHours::new(24).expect("hours >= 1"))
}

pub(super) fn cfg_with(file: &str) -> Config {
    let p = crate::config::parse_file(file);
    Config {
        taps: p.taps,
        no_soak: p.no_soak,
        ..cfg24()
    }
}

pub(super) fn inv_from(brew: &MockBrew, cfg: &Config) -> Inventory {
    let taps = brew.tap_info().expect("mock taps");
    let origins = OriginRecords::default();
    Inventory::build(brew.installed.clone(), &taps, &origins, cfg).with_hidden(
        &brew.hidden_kegs(&brew.installed),
        &taps,
        &origins,
        None,
    )
}

pub(super) fn tapped(name: &str, remote: Option<&str>) -> TapInfo {
    TapInfo::new(name, remote)
}

pub(super) fn formula_pkg_from(name: &str, tap: &str, receipt_rb: String) -> InstalledPkg {
    InstalledPkg {
        tap: Some(tap.into()),
        ..formula_pkg(name, receipt_rb)
    }
}

/// Snapshots with core/cask plus one soaked tap whose cutoff/head trees
/// hold `Formula/<name>.rb` blobs.
pub(super) fn tap_snaps(
    git: &InMemoryGit,
    tap: &str,
    cutoff: Option<&str>,
    head: &str,
) -> Snapshots {
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

pub(super) fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("fixed now")
}

/// A writable cache directory, the same for every call on one test's thread
/// and removed when that thread ends.
pub(crate) fn test_cache() -> &'static Path {
    thread_local! {
        static CACHE: (tempfile::TempDir, &'static Path) = {
            let dir = tempfile::tempdir().expect("test cache");
            let path: &'static Path = Box::leak(dir.path().to_path_buf().into_boxed_path());
            (dir, path)
        };
    }
    CACHE.with(|c| c.1)
}

pub(super) fn formula_rb(name: &str, version: &str, sha: &str) -> String {
    format!(
        "class X < Formula\n  url \"https://example.com/{name}-{version}.tar.gz\"\n  sha256 \"{sha}\"\nend\n"
    )
}

pub(super) fn view_world() -> (MockBrew, InMemoryGit, Snapshots) {
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

pub(super) const PACKER_REFUSAL: &[u8] = b"Error: packer: Refusing to load formula hashicorp/tap/packer from untrusted tap hashicorp/tap.\nRun `brew trust --formula hashicorp/tap/packer` or `brew trust hashicorp/tap` to trust it.\n";

pub(super) fn packer_world() -> (InMemoryGit, Snapshots) {
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

pub(super) fn hashicorp_tap() -> Vec<TapInfo> {
    vec![tapped(
        "hashicorp/tap",
        Some("https://github.com/hashicorp/homebrew-tap"),
    )]
}

pub(super) fn core_snaps() -> Snapshots {
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

pub(super) fn formula_pkg(name: &str, receipt_rb: String) -> InstalledPkg {
    InstalledPkg {
        own_tap: None,
        name: name.into(),
        kind: PkgKind::Formula,
        keg_version: crate::soaktap::file_version(PkgKind::Formula, receipt_rb.as_bytes())
            .unwrap_or_default(),
        receipt_rb,
        pinned: false,
        tap: None,
        staged_path: None,
    }
}

pub(super) fn formula_pkg_pinned(name: &str, receipt_rb: String) -> InstalledPkg {
    InstalledPkg {
        pinned: true,
        ..formula_pkg(name, receipt_rb)
    }
}

pub(super) fn lock_runs(brew: &MockBrew) -> Vec<Vec<String>> {
    brew.runs.lock().expect("runs").clone()
}

pub(super) fn run_has_token(runs: &[Vec<String>], token: &str) -> bool {
    runs.iter().any(|args| args.iter().any(|a| a == token))
}

pub(super) fn run_is_name_install(runs: &[Vec<String>], name: &str) -> bool {
    runs.iter().any(|args| {
        let last = args.last().map(String::as_str);
        // `upgrade` is how brewsoak installs over a soak-tap keg; a bare name
        // under `upgrade` is something else.
        let verb_ok = match args.first().map(String::as_str) {
            Some("install") => true,
            Some("upgrade") => last.is_some_and(|a| a.starts_with("ericfitz/brewsoak/")),
            _ => false,
        };
        verb_ok
            && last.map(strip_soak_prefix) == Some(name)
            && !args.iter().any(|a| a == "--ignore-dependencies")
    })
}

/// A command argument without the soak tap's `ericfitz/brewsoak/` prefix.
pub(super) fn strip_soak_prefix(arg: &str) -> &str {
    arg.strip_prefix("ericfitz/brewsoak/").unwrap_or(arg)
}

/// No argument is a path; no command is `trust` or `untrust`; no argument is
/// `--ignore-dependencies` or contains `brewsoakr/soaked`; every argument that
/// starts with `ericfitz/brewsoak/` names one of `soak_names`. Third-party
/// installs and `brew deps` questions keep bare names, and no `by_name`
/// query carries a full token.
pub(super) fn assert_soak_tap_commands(brew: &MockBrew, soak_names: &[&str]) {
    let prefix = "ericfitz/brewsoak/";
    let soak_ok = |a: &str| {
        a.strip_prefix(prefix)
            .is_some_and(|n| soak_names.contains(&n))
    };
    let mut all = lock_runs(brew);
    all.extend(brew.visible_runs.lock().expect("visible").clone());
    for args in &all {
        let verb = args.first().map(String::as_str);
        assert!(
            !matches!(verb, Some("trust" | "untrust")),
            "trust command in {args:?}"
        );
        let named = matches!(verb, Some("install" | "deps"));
        for a in args {
            assert!(
                !a.starts_with('/') && !a.ends_with(".rb"),
                "path argument {a:?} in {args:?}"
            );
            assert!(
                a != "--ignore-dependencies" && !a.contains("brewsoakr/soaked"),
                "forbidden argument {a:?} in {args:?}"
            );
            if a.starts_with(prefix) {
                assert!(
                    soak_ok(a),
                    "soak token {a:?} not among {soak_names:?} in {args:?}"
                );
            } else {
                assert!(
                    !(named && !a.starts_with('-') && a.contains('/')),
                    "full token {a:?} in {args:?}"
                );
            }
        }
    }
    for (kind, token) in brew.deps_calls.lock().expect("deps calls").iter() {
        let fine = if token.starts_with(prefix) {
            soak_ok(token)
        } else {
            !token.contains('/')
        };
        assert!(
            fine && !token.starts_with('/') && !token.ends_with(".rb"),
            "path or unexpected token {token:?} handed to deps ({kind:?})"
        );
    }
    for q in brew.by_name_queries.lock().expect("queries").iter() {
        assert!(!q.name.contains('/'), "full token in by_name query {q:?}");
    }
}

pub(super) fn upgrade_names(brew: &MockBrew, git: &InMemoryGit, names: &[String]) -> String {
    upgrade_names_flags(brew, git, names, &[])
}

pub(super) fn upgrade_names_flags(
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
        &crate::lock::RunLock::for_tests(),
        brew,
        git,
        &core_snaps(),
        test_cache(),
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

pub(super) fn cask_rb(name: &str, version: &str) -> String {
    format!(
        "cask \"{name}\"\n  version \"{version}\"\n  sha256 \"cafebabe\"\n  url \"https://example.com/{name}-{version}.zip\"\n"
    )
}

pub(super) fn call_reinstall(
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
        &crate::lock::RunLock::for_tests(),
        brew,
        git,
        snaps,
        test_cache(),
        tap,
        &inv,
        &cfg,
        names,
        &[],
        out,
    )
}

pub(super) fn local_tap_world() -> (MockBrew, InMemoryGit) {
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

pub(super) fn cask_pkg_from(name: &str, tap: &str, receipt_rb: String) -> InstalledPkg {
    InstalledPkg {
        own_tap: None,
        name: name.into(),
        kind: PkgKind::Cask,
        keg_version: crate::soaktap::file_version(PkgKind::Cask, receipt_rb.as_bytes())
            .unwrap_or_default(),
        receipt_rb,
        pinned: false,
        tap: Some(tap.into()),
        staged_path: None,
    }
}

/// Installed cask `ant` from anthropics/tap (version-only receipt, as
/// Homebrew >= 4 leaves it) while core also carries a formula `ant`.
pub(super) fn tap_cask_world() -> (MockBrew, InMemoryGit, Snapshots) {
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

/// Installed `alt-tab` 7.38.1 (self-updating app) behind a cask cutoff
/// of 11.8.0 that says `auto_updates true`, like brew's own skip.
pub(super) fn auto_updates_world() -> (MockBrew, InMemoryGit, Snapshots) {
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

pub(super) fn greedy(argv: &[&str]) -> flags::Greedy {
    flags::greedy_mode(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

/// A `version :latest` cask with `sha256 :no_check`; `url` is the only
/// thing that can differ between two of them.
pub(super) fn latest_cask_rb(name: &str, url: &str, auto_updates: bool) -> String {
    let auto = if auto_updates {
        "  auto_updates true\n"
    } else {
        ""
    };
    format!(
        "cask \"{name}\" do\n  version :latest\n  sha256 :no_check\n  url \"{url}\"\n{auto}end\n"
    )
}

/// Five installed packages, one per greedy row: `wget` (formula behind
/// cutoff), `alt-tab` (self-updating, behind cutoff), `nightly`
/// (`:latest`, version-only receipt, cutoff matches), `moved` (`:latest`,
/// full receipt, cutoff url moved) and `self-latest` (`:latest` and
/// self-updating, cutoff url moved).
///
/// Expected plan sizes: bare 2 (wget, moved); `--greedy-latest` 2;
/// `--greedy-auto-updates` 4 (+ alt-tab, self-latest); `--greedy` 4.
pub(super) fn greedy_world() -> (MockBrew, InMemoryGit, Snapshots) {
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
    let casks = [
        (
            "alt-tab",
            format!("{}  auto_updates true\n", cask_rb("alt-tab", "11.8.0")),
        ),
        (
            "nightly",
            latest_cask_rb("nightly", "https://example.com/nightly.dmg", false),
        ),
        (
            "moved",
            latest_cask_rb("moved", "https://example.com/moved-new.dmg", false),
        ),
        (
            "self-latest",
            latest_cask_rb("self-latest", "https://example.com/self-new.dmg", true),
        ),
    ];
    for (name, rb) in &casks {
        let path = format!("Casks/{}/{name}.rb", &name[..1]);
        git.insert_blob("caskcut", &path, rb.clone());
        git.insert_blob("caskhead", &path, rb.clone());
    }
    // brew wrote soak-tap receipts for what it installed.
    let mut receipt_taps = BTreeMap::new();
    receipt_taps.insert(
        (PkgKind::Formula, "wget".to_string(), "1.1.0".to_string()),
        "ericfitz/brewsoak".to_string(),
    );
    for (name, rb) in &casks {
        let version = crate::soaktap::file_version(PkgKind::Cask, rb.as_bytes()).unwrap();
        // brew's receipt spells a `:latest` cask `latest`.
        let version = if version == ":latest" {
            "latest".to_string()
        } else {
            version
        };
        receipt_taps.insert(
            (PkgKind::Cask, name.to_string(), version),
            "ericfitz/brewsoak".to_string(),
        );
    }
    let brew = MockBrew {
        receipt_taps,
        installed: vec![
            formula_pkg("wget", formula_rb("wget", "1.0.0", "oldsha")),
            cask_pkg_from(
                "alt-tab",
                "homebrew/cask",
                crate::brew::version_only_cask_receipt("alt-tab", "7.38.1"),
            ),
            cask_pkg_from(
                "nightly",
                "homebrew/cask",
                crate::brew::version_only_cask_receipt("nightly", "latest"),
            ),
            cask_pkg_from(
                "moved",
                "homebrew/cask",
                latest_cask_rb("moved", "https://example.com/moved-old.dmg", false),
            ),
            cask_pkg_from(
                "self-latest",
                "homebrew/cask",
                latest_cask_rb("self-latest", "https://example.com/self-old.dmg", true),
            ),
        ],
        ..MockBrew::new()
    };
    (brew, git, core_snaps())
}

/// Bare `upgrade` over `greedy_world` with `flags`; returns the output.
pub(super) fn greedy_upgrade(
    brew: &MockBrew,
    git: &InMemoryGit,
    snaps: &Snapshots,
    argv: &[&str],
) -> String {
    let cfg = cfg24();
    let inv = inv_from(brew, &cfg);
    let cache = tempfile::tempdir().unwrap();
    let tap = tempfile::tempdir().unwrap();
    let flags: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    upgrade(
        &crate::lock::RunLock::for_tests(),
        brew,
        git,
        snaps,
        cache.path(),
        tap.path(),
        &inv,
        &cfg,
        &[],
        &flags,
        &mut out,
    )
    .unwrap();
    String::from_utf8(out).unwrap()
}

/// Bare `outdated` over `greedy_world` with `argv`; returns the output.
pub(super) fn greedy_outdated(
    brew: &MockBrew,
    git: &InMemoryGit,
    snaps: &Snapshots,
    argv: &[&str],
) -> String {
    let cfg = cfg24();
    let inv = inv_from(brew, &cfg);
    let flags: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    outdated(brew, git, snaps, test_cache(), &inv, &cfg, &flags, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

/// Lines under `header` up to the next `==> ` header, `(none)` excluded.
pub(super) fn section_lines(text: &str, header: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| *l != header)
        .skip(1)
        .take_while(|l| !l.starts_with("==> "))
        .filter(|l| *l != "(none)")
        .map(str::to_string)
        .collect()
}

pub(super) const OUTDATED_HEADER: &str = "==> Outdated (will upgrade)";

pub(super) const AUTO_UPDATES_HEADER: &str =
    "==> Auto-updates (left to the app; name it to upgrade)";

/// `openssl@3` installed at 3.6.5 while upstream re-pointed the formula
/// at the 3.5 line: cutoff and HEAD are both 3.5.9.
pub(super) fn openssl_repoint_world() -> (MockBrew, InMemoryGit, Snapshots) {
    let git = InMemoryGit::new();
    let rb = formula_rb("openssl@3", "3.5.9", "sha359");
    git.insert_blob("cutoffsha", "Formula/o/openssl@3.rb", rb.clone());
    git.insert_blob("headsha", "Formula/o/openssl@3.rb", rb);
    let brew = MockBrew {
        installed: vec![formula_pkg(
            "openssl@3",
            formula_rb("openssl@3", "3.6.5", "sha365"),
        )],
        ..MockBrew::new()
    };
    (brew, git, core_snaps())
}

/// brew orders 3.6.5 after 3.5.9.
pub(super) fn openssl_is_newer(brew: &mut MockBrew) {
    brew.orderings.insert(
        ("3.6.5".to_string(), "3.5.9".to_string()),
        std::cmp::Ordering::Greater,
    );
}

/// Every name installed at 1.0.0, cutoff 1.1.0 and HEAD 1.2.0 (so the
/// three sides have three different urls), all url-derived. `app`
/// depends on every other name. Brew's `Version.detect` says `d<version>-<name>`
/// for each url, so a detected version is never the heuristic one.
pub(super) fn many_detect_world(names: &[&str]) -> (MockBrew, InMemoryGit) {
    let git = InMemoryGit::new();
    let mut brew = MockBrew::new();
    for name in names {
        let dir = &name[..1];
        for (sha, ver, hash) in [
            ("cutoffsha", "1.1.0", "midsha"),
            ("headsha", "1.2.0", "newsha"),
        ] {
            git.insert_blob(
                sha,
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, ver, hash),
            );
        }
        brew.installed
            .push(formula_pkg(name, formula_rb(name, "1.0.0", "oldsha")));
        for ver in ["1.0.0", "1.1.0", "1.2.0"] {
            brew.detections.insert(
                (format!("https://example.com/{name}-{ver}.tar.gz"), None),
                format!("d{ver}-{name}"),
            );
        }
    }
    brew.deps.insert(
        "app".to_string(),
        names[1..].iter().map(|n| n.to_string()).collect(),
    );
    (brew, git)
}

pub(super) fn spawns(brew: &MockBrew) -> (usize, usize) {
    (
        *brew.detect_spawns.lock().unwrap(),
        *brew.compare_spawns.lock().unwrap(),
    )
}

pub(super) fn git_empty() -> InMemoryGit {
    InMemoryGit::new()
}

/// The soak tap as `brew tap-info` reports a local one: at `path`, with the
/// given trust.
pub(crate) fn soak_tap_at(path: &Path, trusted: bool) -> TapInfo {
    let mut tap = TapInfo::new(crate::origin::SOAK_TAP, None);
    tap.path = Some(path.to_path_buf());
    tap.trusted = trusted;
    tap
}

/// `local_tap_world` with the soak tap local, trusted and at `tap`.
pub(super) fn soak_world(tap: &Path) -> (MockBrew, InMemoryGit) {
    let (mut brew, git) = local_tap_world();
    brew.soak_tap = Some(soak_tap_at(tap, true));
    (brew, git)
}

/// `soak_world` (formula `alpha` 1.0.0 behind cutoff 1.1.0, unsoakable formula
/// `thing`) plus cask `docker` 4.0 (version-only receipt, tap homebrew/cask)
/// behind `cask_rb("docker", "4.1")` at `caskcut` and `caskhead`,
/// path `Casks/d/docker.rb`.
pub(super) fn formula_and_cask_world(tap: &Path) -> (MockBrew, InMemoryGit) {
    let (mut brew, git) = soak_world(tap);
    for sha in ["caskcut", "caskhead"] {
        git.insert_blob(sha, "Casks/d/docker.rb", cask_rb("docker", "4.1"));
    }
    brew.installed.push(cask_pkg_from(
        "docker",
        "homebrew/cask",
        crate::brew::version_only_cask_receipt("docker", "4.0"),
    ));
    (brew, git)
}

/// `soak_world` plus a formula `widget` brew hides because its tap,
/// `acme/tools` (HTTPS remote), is untrusted.
pub(super) fn hidden_world(tap: &Path) -> (MockBrew, InMemoryGit) {
    let (mut brew, git) = soak_world(tap);
    brew.hidden = vec![crate::brew::HiddenKeg {
        name: "widget".into(),
        tap: Some("acme/tools".into()),
        staged_path: None,
        own_tap: false,
    }];
    brew.taps.push(TapInfo {
        trusted: false,
        ..tapped("acme/tools", Some("https://github.com/acme/homebrew-tools"))
    });
    (brew, git)
}

/// `soak_world` plus the third-party `packer` of `packer_world`, installed
/// behind its cutoff, so a run has a package the soak tap never touches.
pub(super) fn soak_and_packer_world(tap: &Path) -> (MockBrew, InMemoryGit, Snapshots) {
    let (mut brew, git) = soak_world(tap);
    let (_packer_git, snaps) = packer_world();
    for (sha, path, bytes) in [
        (
            "tapcut",
            "Formula/packer.rb",
            formula_rb("packer", "1.16.1", "midsha"),
        ),
        (
            "taphead",
            "Formula/packer.rb",
            formula_rb("packer", "1.17.0", "newsha"),
        ),
    ] {
        git.insert_blob(sha, path, bytes);
    }
    git.insert_tree("tapcut", &["Formula/packer.rb"]);
    git.insert_tree("taphead", &["Formula/packer.rb"]);
    brew.installed.push(formula_pkg_from(
        "packer",
        "hashicorp/tap",
        formula_rb("packer", "1.15.0", "oldsha"),
    ));
    brew.taps.extend(hashicorp_tap());
    (brew, git, snaps)
}

/// `soak_and_packer_world` with `hello` installed from the soak tap (receipt
/// names it) at `installed`, soak cutoff 1.1.0 and HEAD 1.2.0, and the soak
/// tap local at `tap` with the given trust.
pub(super) fn hello_world(
    tap: &Path,
    trusted: bool,
    installed: &str,
) -> (MockBrew, InMemoryGit, Snapshots) {
    let (mut brew, git, snaps) = soak_and_packer_world(tap);
    brew.soak_tap = Some(soak_tap_at(tap, trusted));
    git.insert_blob(
        "cutoffsha",
        "Formula/h/hello.rb",
        formula_rb("hello", "1.1.0", "midsha"),
    );
    git.insert_blob(
        "headsha",
        "Formula/h/hello.rb",
        formula_rb("hello", "1.2.0", "newsha"),
    );
    let sha = match installed {
        "1.1.0" => "midsha",
        "1.2.0" => "newsha",
        _ => "oldsha",
    };
    brew.installed.push(InstalledPkg {
        own_tap: Some(crate::origin::OwnTap::Soak),
        ..formula_pkg("hello", formula_rb("hello", installed, sha))
    });
    (brew, git, snaps)
}

/// The bytes of `rel` below `tap`, if the file exists.
pub(super) fn tap_file(tap: &Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(tap.join(rel)).ok()
}

/// Run `upgrade` with `tap` as the tap root and the per-thread test cache.
pub(super) fn run_upgrade(
    brew: &MockBrew,
    git: &InMemoryGit,
    snaps: &Snapshots,
    tap: &Path,
    names: &[&str],
    user_flags: &[&str],
) -> (RunResult, String) {
    let cfg = cfg24();
    let inv = inv_from(brew, &cfg);
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    let user_flags: Vec<String> = user_flags.iter().map(|n| n.to_string()).collect();
    let mut out = Vec::new();
    let r = upgrade(
        &crate::lock::RunLock::for_tests(),
        brew,
        git,
        snaps,
        test_cache(),
        tap,
        &inv,
        &cfg,
        &names,
        &user_flags,
        &mut out,
    )
    .expect("upgrade");
    (r, String::from_utf8(out).expect("utf8"))
}

/// Leave a tap file no package needs (`Formula/gone.rb`) and an unfinished
/// undo entry (`soak-tap-undo/Formula/back.absent`, whose tap file
/// `Formula/back.rb` should not outlive it), so maintenance has work to do.
pub(crate) fn seed_stale_tap(tap: &Path, cache: &Path) {
    let body = formula_rb("gone", "1.0.0", "x");
    for (dir, file, bytes) in [
        (tap.join("Formula"), "gone.rb", body.as_bytes()),
        (tap.join("Formula"), "back.rb", body.as_bytes()),
        (
            cache.join(crate::soaktap::UNDO_DIR).join("Formula"),
            "back.absent",
            b"".as_slice(),
        ),
    ] {
        std::fs::create_dir_all(&dir).expect("seed dir");
        std::fs::write(dir.join(file), bytes).expect("seed file");
    }
}

/// Every file under `dir` with its bytes; empty when `dir` is missing.
pub(crate) fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for ent in entries.flatten() {
            let path = ent.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                let bytes = std::fs::read(&path).expect("read");
                out.insert(path, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

#[cfg(test)]
mod assert_soak_tap_commands_tests {
    use super::*;
    use crate::brew::NameQuery;

    fn visible(brew: &MockBrew, args: &[&str]) {
        brew.visible_runs
            .lock()
            .unwrap()
            .push(args.iter().map(|a| a.to_string()).collect());
    }

    #[test]
    #[should_panic(expected = "path argument")]
    fn flags_a_slash_prefixed_argument() {
        let brew = MockBrew::new();
        visible(&brew, &["install", "/tmp/staging/x"]);
        assert_soak_tap_commands(&brew, &[]);
    }

    #[test]
    #[should_panic(expected = "path argument")]
    fn flags_an_rb_argument() {
        let brew = MockBrew::new();
        visible(&brew, &["install", "x.rb"]);
        assert_soak_tap_commands(&brew, &[]);
    }

    #[test]
    #[should_panic(expected = "trust command")]
    fn flags_trust_and_untrust() {
        let brew = MockBrew::new();
        visible(&brew, &["untrust", "ericfitz/brewsoak"]);
        assert_soak_tap_commands(&brew, &[]);
    }

    #[test]
    #[should_panic(expected = "forbidden argument")]
    fn flags_ignore_dependencies() {
        let brew = MockBrew::new();
        visible(&brew, &["install", "--ignore-dependencies", "x"]);
        assert_soak_tap_commands(&brew, &[]);
    }

    #[test]
    #[should_panic(expected = "forbidden argument")]
    fn flags_the_legacy_tap() {
        let brew = MockBrew::new();
        visible(&brew, &["install", "brewsoakr/soaked/x"]);
        assert_soak_tap_commands(&brew, &[]);
    }

    #[test]
    #[should_panic(expected = "not among")]
    fn flags_a_soak_token_for_an_unlisted_name() {
        let brew = MockBrew::new();
        visible(&brew, &["install", "--formula", "ericfitz/brewsoak/other"]);
        assert_soak_tap_commands(&brew, &["name"]);
    }

    #[test]
    #[should_panic(expected = "full token")]
    fn flags_a_third_party_full_token_on_an_install_run() {
        let brew = MockBrew::new();
        visible(&brew, &["install", "--formula", "user/repo/name"]);
        assert_soak_tap_commands(&brew, &["name"]);
    }

    #[test]
    #[should_panic(expected = "handed to deps")]
    fn flags_a_third_party_full_token_in_deps_calls() {
        let brew = MockBrew::new();
        brew.deps_calls
            .lock()
            .unwrap()
            .push((PkgKind::Formula, "user/repo/name".to_string()));
        assert_soak_tap_commands(&brew, &[]);
    }

    #[test]
    #[should_panic(expected = "handed to deps")]
    fn flags_a_soak_token_for_an_unlisted_name_in_deps_calls() {
        let brew = MockBrew::new();
        brew.deps_calls
            .lock()
            .unwrap()
            .push((PkgKind::Formula, "ericfitz/brewsoak/other".to_string()));
        assert_soak_tap_commands(&brew, &["name"]);
    }

    #[test]
    #[should_panic(expected = "by_name query")]
    fn flags_a_full_token_in_by_name_queries() {
        let brew = MockBrew::new();
        brew.by_name_queries.lock().unwrap().push(NameQuery {
            kind: PkgKind::Formula,
            name: "ericfitz/brewsoak/name".to_string(),
            origin: "homebrew/core".to_string(),
            cutoff_sha256: None,
        });
        assert_soak_tap_commands(&brew, &["name"]);
    }

    #[test]
    fn accepts_listed_soak_tokens_bare_third_party_names_and_repair_tokens() {
        let brew = MockBrew::new();
        visible(&brew, &["upgrade", "--formula", "ericfitz/brewsoak/name"]);
        visible(&brew, &["install", "--cask", "ericfitz/brewsoak/name"]);
        visible(&brew, &["upgrade", "--formula", "user/repo/other"]);
        visible(&brew, &["reinstall", "user/repo/other"]);
        visible(&brew, &["install", "--formula", "packer"]);
        brew.deps_calls
            .lock()
            .unwrap()
            .push((PkgKind::Formula, "ericfitz/brewsoak/name".to_string()));
        brew.deps_calls
            .lock()
            .unwrap()
            .push((PkgKind::Formula, "packer".to_string()));
        assert_soak_tap_commands(&brew, &["name"]);
    }

    #[test]
    fn run_is_name_install_rejects_an_upgrade_of_a_bare_name() {
        let runs = |args: &[&str]| vec![args.iter().map(|a| a.to_string()).collect::<Vec<_>>()];
        assert!(!run_is_name_install(
            &runs(&["upgrade", "--formula", "packer"]),
            "packer"
        ));
        assert!(run_is_name_install(
            &runs(&["upgrade", "--formula", "ericfitz/brewsoak/alpha"]),
            "alpha"
        ));
        assert!(run_is_name_install(
            &runs(&["install", "--formula", "packer"]),
            "packer"
        ));
    }
}
