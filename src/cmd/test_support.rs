use crate::Error;
use crate::SoakHours;
use crate::brew::{InstalledPkg, MockBrew};
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
use std::path::Path;
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
    Inventory::build(
        brew.installed.clone(),
        &brew.taps,
        &OriginRecords::default(),
        cfg,
    )
}

pub(super) fn tapped(name: &str, remote: Option<&str>) -> TapInfo {
    TapInfo {
        name: name.into(),
        remote: remote.map(str::to_string),
    }
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

pub(super) fn unused_cache() -> &'static Path {
    Path::new("/brewsoak-in-memory-unused")
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
        name: name.into(),
        kind: PkgKind::Formula,
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

pub(super) fn run_is_soaked_install(runs: &[Vec<String>], name: &str) -> bool {
    let suffix = format!("/{name}.rb");
    runs.iter().any(|args| {
        args.first().map(String::as_str) == Some("install")
            && args.iter().any(|a| a.ends_with(&suffix))
            && !args.iter().any(|a| a == "--ignore-dependencies")
    })
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
    let brew = MockBrew {
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
    outdated(
        brew,
        git,
        snaps,
        unused_cache(),
        &inv,
        &cfg,
        &flags,
        &mut out,
    )
    .unwrap();
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
