//! Golden test: a real noisy `brewsoak upgrade` transcript through the filter.
//! Set BREWSOAK_BLESS=1 to rewrite the expected file after an intended change.
//!
//! The fixture is a terminal transcript, so it interleaves two sources: brew's
//! output (which the filter sees) and brewsoak's own lines (which it never
//! does). `brew_stream` drops the brewsoak-origin lines and starts a new run
//! at each package, which is how ProcessBrew drives the filter for real.

use brewsoak::quiet::{Filter, RunContext, RunMode, human_size};
use std::path::Path;

fn is_brewsoak_line(line: &str) -> bool {
    line.contains("efitz@macmini %")
        || line.starts_with("fetching Homebrew/")
        || line.starts_with("warning: ")
        || line.starts_with("updating soak snapshots")
        || line.starts_with("core ")
        || line.starts_with("cask ")
        || line.starts_with("no installed packages changed")
        || line.starts_with("snapshots refreshed")
        || line.starts_with("upgraded ")
        || line.ends_with("unparseable identity; skipping")
}

#[test]
fn noisy_upgrade_transcript_collapses() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let input = std::fs::read_to_string(dir.join("noisy_upgrade.txt")).unwrap();

    let mut filter = Filter::new();
    filter.start_run(&RunContext::default());
    let mut lines = Vec::new();
    for line in input.lines() {
        if is_brewsoak_line(line) {
            continue;
        }
        // brew prints this first for each package brewsoak hands it.
        if line.contains("is already installed but outdated") {
            filter.start_run(&RunContext::default());
        }
        if let Some(out) = filter.line(line) {
            lines.push(out);
        }
    }
    lines.extend(filter.caveat_report());
    lines.push(format!(
        "installed {}, freed {}",
        human_size(filter.added_bytes()),
        human_size(filter.freed_bytes())
    ));
    let got = format!("{}\n", lines.join("\n"));

    let expected_path = dir.join("quiet_upgrade.txt");
    if std::env::var_os("BREWSOAK_BLESS").is_some() {
        std::fs::write(&expected_path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&expected_path).unwrap_or_default();
    assert_eq!(got, want, "filtered output changed");

    assert!(
        got.lines().count() * 2 < input.lines().count(),
        "expected at least a 2x reduction: {} -> {}",
        input.lines().count(),
        got.lines().count()
    );
    assert!(!got.contains("==>"), "arrows must be stripped");
    assert!(!got.contains("Cleanup"), "cleanup must be suppressed");
    assert!(
        !got.contains("site-functions"),
        "completion caveats must be dropped"
    );
    assert!(
        !got.contains("brew reinstall"),
        "reinstall hints must be dropped"
    );
    assert!(
        filter.installed().contains_key("simdjson"),
        "transitive deps recorded"
    );
}

/// The fixture is hand-built from reading brew's source, not recorded output;
/// the manual run in Task 13.3/13.4 refreshes it from real brew output.
#[test]
fn soak_tap_transcript_collapses() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let input = std::fs::read_to_string(dir.join("soak_tap_run.txt")).unwrap();

    let mut filter = Filter::new();
    filter.start_run(&RunContext {
        mode: RunMode::Soaked,
        ..RunContext::default()
    });
    let lines: Vec<String> = input.lines().filter_map(|l| filter.line(l)).collect();
    let got = format!("{}\n", lines.join("\n"));

    let expected_path = dir.join("soak_tap_quiet.txt");
    if std::env::var_os("BREWSOAK_BLESS").is_some() {
        std::fs::write(&expected_path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&expected_path).unwrap_or_default();
    assert_eq!(got, want, "filtered output changed");

    assert!(
        !got.contains("ericfitz/brewsoak"),
        "tap prefix must not show"
    );
    assert!(!got.contains("Trusted"), "trust lines are for the log only");
    assert!(
        !got.lines().any(|l| l == "hello 2.12.3"),
        "the dry-run preview must not show"
    );
}
