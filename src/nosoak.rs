//! The `NO_SOAK` list: packages and taps brewsoak hands straight to brew.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoSoakEntry {
    /// `wget`: that formula or cask from any origin.
    Name(String),
    /// `ericfitz/tap`: every package whose origin is that tap.
    Tap(String),
    /// `hashicorp/tap/terraform`: that package from that tap only.
    TapPkg { tap: String, name: String },
}

/// Parse one `NO_SOAK` entry. `Err` carries the reason, worded for a `-v` note.
/// Matching is case-insensitive, so entries are lowercased here.
pub fn parse_entry(raw: &str) -> Result<NoSoakEntry, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("NO_SOAK entry is empty; skipped".to_string());
    }
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!(
            "NO_SOAK entry {raw:?} has an empty path segment; skipped"
        ));
    }
    let lower = raw.to_ascii_lowercase();
    match parts.len() {
        1 => Ok(NoSoakEntry::Name(lower)),
        2 => Ok(NoSoakEntry::Tap(lower)),
        3 => {
            let (tap, name) = lower.rsplit_once('/').expect("three segments");
            Ok(NoSoakEntry::TapPkg {
                tap: tap.to_string(),
                name: name.to_string(),
            })
        }
        _ => Err(format!(
            "NO_SOAK entry {raw:?} has more than two slashes; skipped"
        )),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoSoakList {
    entries: Vec<NoSoakEntry>,
}

impl NoSoakList {
    pub fn new(entries: Vec<NoSoakEntry>) -> Self {
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `origin` is `user/repo` (core and cask included); `name` is the formula
    /// name or cask token. Both compare case-insensitively.
    pub fn matches(&self, origin: &str, name: &str) -> bool {
        let origin = origin.to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        self.entries.iter().any(|e| match e {
            NoSoakEntry::Name(n) => *n == name,
            NoSoakEntry::Tap(t) => *t == origin,
            NoSoakEntry::TapPkg { tap, name: n } => *tap == origin && *n == name,
        })
    }
}

use crate::Error;
use crate::brew::Brew;
use crate::cmd::{max_status, merge_status};
use crate::flags::{dropped_note, filter_for_verb, is_dry_run};
use crate::origin;
use crate::quiet;
use crate::report::{Verb, nosoak_header};
use crate::resolve::PkgKind;
use crate::section::Section;
use crate::tap::is_stripped_flag;
use std::collections::BTreeMap;
use std::io::Write;

/// One no-soak package handed to brew. `switch_tap`: the installed keg was
/// staged by brewsoak (receipt tap empty) and the origin is a third-party tap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub origin: String,
    pub name: String,
    pub switch_tap: bool,
    /// Known for an installed package; `None` for a name not installed yet.
    pub kind: Option<PkgKind>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub status: Option<i32>,
    pub notes: Vec<String>,
    /// Targets handed to brew (the `no-soak N` count).
    pub count: usize,
    /// Cellar versions brew reported installing, for the staleness check.
    pub installed: BTreeMap<String, String>,
    /// Tap-switch targets brew replaced; their staged-keg origin record is stale.
    pub switched: Vec<Target>,
    /// Targets brew upgraded, installed or (dry run) listed as upgradable.
    pub upgraded: usize,
    /// Targets brew said were already installed and current.
    pub current: usize,
    /// Targets brew said nothing about.
    pub no_result: usize,
}

/// The targets as `(kind flag, group)` runs: one unflagged group, or when
/// `split` a `--formula` group and a `--cask` group (kind unknown counts as
/// formula, as brew resolves a bare token).
fn kind_groups<'a>(
    targets: &[&'a Target],
    split: bool,
) -> Vec<(Option<&'static str>, Vec<&'a Target>)> {
    if targets.is_empty() {
        return Vec::new();
    }
    if !split {
        return vec![(None, targets.to_vec())];
    }
    let (casks, formulae): (Vec<&Target>, Vec<&Target>) =
        targets.iter().partition(|t| t.kind == Some(PkgKind::Cask));
    [(Some("--formula"), formulae), (Some("--cask"), casks)]
        .into_iter()
        .filter(|(_, g)| !g.is_empty())
        .collect()
}

pub fn brew_token(origin_tap: &str, name: &str) -> String {
    if origin::is_core_or_cask(origin_tap) {
        name.to_string()
    } else {
        format!("{origin_tap}/{name}")
    }
}

/// Spec "No-soak packages": one `brew update`, then one `brew <verb>` with
/// full tokens; tap-switch targets go through `brew reinstall` on `upgrade`.
pub fn run_step(
    brew: &impl Brew,
    verb: &str,
    user_flags: &[String],
    targets: &[Target],
    out: &mut impl Write,
) -> Result<StepResult, Error> {
    let verb_kind = Verb::parse(verb)
        .ok_or_else(|| Error::Usage(format!("no-soak: unknown brew verb {verb:?}")))?;
    let mut result = StepResult::default();
    if targets.is_empty() {
        return Ok(result);
    }
    result.count = targets.len();
    let tokens: Vec<String> = targets
        .iter()
        .map(|t| brew_token(&t.origin, &t.name))
        .collect();
    let token_of = |t: &Target| brew_token(&t.origin, &t.name);

    let mut updating = Section::new(&mut *out, "Updating Homebrew");
    updating.open()?;
    let update = brew.run_visible(&["update".to_string()], &mut updating)?;
    drop(updating);
    if !update.status.success() {
        let code = update.status.code().unwrap_or(1);
        result.notes.push(format!(
            "no-soak: brew update failed (exit {code}); not {verb}d: {}",
            tokens.join(", ")
        ));
        result.status = Some(code);
        result.no_result = targets.len();
        return Ok(result);
    }

    let flags: Vec<String> = user_flags
        .iter()
        .filter(|f| !is_stripped_flag(f))
        .cloned()
        .collect();
    let dry_run = is_dry_run(&flags);
    let ctx = quiet::RunContext {
        mode: quiet::RunMode::NoSoak,
    };
    let mut section = Section::new(&mut *out, nosoak_header(verb_kind, dry_run));
    // Everything brew said across the section's runs, for the per-package verdicts.
    let mut said = String::new();
    let (switch, plain): (Vec<&Target>, Vec<&Target>) = targets
        .iter()
        .partition(|t| t.switch_tap && verb == "upgrade");

    // A formula and a cask can share a name, and a full token cannot tell
    // them apart (`brew upgrade vacuum` takes the formula). brew honours
    // `--formula` and `--cask` on upgrade, install, and reinstall, so when
    // both kinds are present each runs in its own flagged group.
    let split_kinds = targets.iter().any(|t| t.kind == Some(PkgKind::Formula))
        && targets.iter().any(|t| t.kind == Some(PkgKind::Cask));

    let plain_flags = filter_for_verb(verb, &flags);
    if !plain.is_empty()
        && let Some(note) = dropped_note(verb, &plain_flags.dropped, "the no-soak run")
    {
        result.notes.push(note);
    }
    for (kind_flag, group) in kind_groups(&plain, split_kinds) {
        let mut args = vec![verb.to_string()];
        args.extend(plain_flags.kept.iter().cloned());
        args.extend(kind_flag.map(str::to_string));
        args.extend(group.iter().map(|t| token_of(t)));
        let output = brew.run_visible_in(&args, &mut section, &ctx)?;
        said.push_str(&String::from_utf8_lossy(&output.stdout));
        result
            .installed
            .extend(quiet::installed_from_output(&output.stdout));
        merge_status(&mut result.status, output);
    }

    // brew install says "already installed" and leaves a staged keg alone;
    // reinstall replaces it, and the receipt then carries the real tap.
    // reinstall has no dry run, so a dry run must not reach it.
    if dry_run && !switch.is_empty() {
        result.notes.push(format!(
            "no-soak: skipped moving staged kegs to their tap for a dry run: {}",
            switch
                .iter()
                .map(|t| token_of(t))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // On upgrade `-g` is `--greedy`, but on reinstall the same letter is
    // `--git`: the switch must not see it.
    let switch_input = if verb == "upgrade" {
        crate::flags::without_greedy(&flags)
    } else {
        flags.clone()
    };
    let switch_flags = filter_for_verb("reinstall", &switch_input);
    if !dry_run
        && !switch.is_empty()
        && let Some(note) =
            dropped_note("reinstall", &switch_flags.dropped, "the no-soak tap switch")
    {
        result.notes.push(note);
    }
    let switch_groups = if dry_run {
        Vec::new()
    } else {
        kind_groups(&switch, split_kinds)
    };
    for (kind_flag, switch) in switch_groups {
        let mut args = vec!["reinstall".to_string()];
        args.extend(switch_flags.kept.iter().cloned());
        args.extend(kind_flag.map(str::to_string));
        args.extend(switch.iter().map(|t| token_of(t)));
        for t in &switch {
            writeln!(
                section,
                "reinstalling {} (moving staged keg to its tap)",
                token_of(t)
            )?;
        }
        let output = brew.run_visible_in(&args, &mut section, &ctx)?;
        said.push_str(&String::from_utf8_lossy(&output.stdout));
        // Only Cellar lines are evidence of a written keg; casks have none, so
        // for them (and formulae alike) success means exit 0 with no
        // already-installed message for the target.
        let installed = quiet::cellar_installed_from_output(&output.stdout);
        let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        let mut code = output.status.code().unwrap_or(1);
        let mut failed = Vec::new();
        for t in &switch {
            let name = t.name.to_ascii_lowercase();
            let said_installed = text
                .lines()
                .any(|l| crate::cmd::already_installed_line(l) && l.contains(&name));
            if code != 0 || said_installed {
                let cask = if t.kind == Some(PkgKind::Cask) {
                    " --cask"
                } else {
                    ""
                };
                result.notes.push(format!(
                    "{}: brew did not replace the staged keg; run brew uninstall{cask} {} then brew install{cask} {} (--ignore-dependencies will not help)",
                    t.name,
                    t.name,
                    token_of(t)
                ));
                code = code.max(1);
                failed.push(*t);
            }
        }
        // When the run failed as a whole, which kegs it replaced is unknown;
        // their records stay and the next run tries again.
        if code == 0 {
            result.switched.extend(
                switch
                    .iter()
                    .filter(|t| !failed.contains(t))
                    .map(|t| (*t).clone()),
            );
        }
        result.installed.extend(installed);
        max_status(&mut result.status, code);
    }

    // A dry run never reaches the staged-keg targets (reinstall has no dry
    // run); report them as the reinstall they would get.
    if dry_run {
        for t in &switch {
            writeln!(
                section,
                "would reinstall {} (moving staged keg to its tap)",
                token_of(t)
            )?;
            result.upgraded += 1;
        }
    }
    let judged = plain.iter().chain(if dry_run { &[] } else { &switch[..] });
    let beer_lines: String = said
        .lines()
        .filter(|l| l.trim_start().starts_with('\u{1f37a}'))
        .flat_map(|l| [l, "\n"])
        .collect();
    let cellar: Vec<String> = quiet::cellar_installed_from_output(beer_lines.as_bytes())
        .into_keys()
        .map(|k| k.to_ascii_lowercase())
        .collect();
    for t in judged {
        match verdict(&said, &cellar, &token_of(t), dry_run) {
            Verdict::Upgraded => result.upgraded += 1,
            Verdict::Current => result.current += 1,
            Verdict::NoResult => {
                result.no_result += 1;
                writeln!(
                    section,
                    "{}\n  no result from brew; see the full log",
                    token_of(t)
                )?;
            }
        }
    }
    Ok(result)
}

enum Verdict {
    Upgraded,
    Current,
    NoResult,
}

/// What brew's output says about one target, matching its token or its last
/// `/` segment case-insensitively. An upgrade outranks an already-current
/// warning. `cellar` holds the lowercase names of the kegs brew poured. A bare
/// `name a -> b` line counts only on a dry run: in a real run it is brew's
/// plan, and the upgrade can still fail.
fn verdict(said: &str, cellar: &[String], token: &str, dry_run: bool) -> Verdict {
    let token = token.to_ascii_lowercase();
    let name = token.rsplit('/').next().unwrap_or(&token).to_string();
    let is_me = |w: &str| {
        let w = w.trim_matches(|c: char| c == '\'' || c == '"' || c == ',' || c == '.');
        w == token || w == name
    };
    if cellar.iter().any(|c| is_me(c)) {
        return Verdict::Upgraded;
    }
    let mut current = false;
    // Inside a `==> Would install|upgrade ...` target list (dry run).
    let mut in_would = false;
    for line in said.lines() {
        let l = line.to_ascii_lowercase();
        let l = l.trim_end();
        let first = l.split_whitespace().next().unwrap_or("");
        if let Some(rest) = l.strip_prefix("==> ") {
            in_would = dry_run && quiet::would_list_verb(&line["==> ".len()..]).is_some();
            let action = rest
                .strip_prefix("upgrading ")
                .or_else(|| rest.strip_prefix("installing "))
                .or_else(|| rest.strip_prefix("reinstalling "))
                .map(|r| r.strip_prefix("cask ").unwrap_or(r));
            if let Some(r) = action
                && r.split_whitespace().next().is_some_and(is_me)
            {
                return Verdict::Upgraded;
            }
            if let Some(file) = rest.strip_prefix("pouring ")
                && file.split("--").next().is_some_and(is_me)
            {
                return Verdict::Upgraded;
            }
        } else if l.trim().is_empty() {
            in_would = false;
        } else if !l.starts_with(' ')
            && is_me(first)
            && (in_would || (dry_run && l.contains(" -> ")))
        {
            return Verdict::Upgraded;
        } else if crate::cmd::already_installed_line(l) && l.split_whitespace().any(is_me) {
            current = true;
        }
    }
    if current {
        Verdict::Current
    } else {
        Verdict::NoResult
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_matches_any_origin() {
        let list = NoSoakList::new(vec![parse_entry("wget").unwrap()]);
        assert!(list.matches("homebrew/core", "wget"));
        assert!(list.matches("acme/tools", "WGET"));
        assert!(!list.matches("homebrew/core", "wget2"));
    }

    #[test]
    fn one_slash_is_a_whole_tap() {
        let list = NoSoakList::new(vec![parse_entry("EricFitz/Tap").unwrap()]);
        assert!(list.matches("ericfitz/tap", "brewsoak"));
        assert!(list.matches("ericfitz/tap", "anything"));
        assert!(!list.matches("homebrew/core", "brewsoak"));
    }

    #[test]
    fn two_slashes_is_one_package_from_one_tap() {
        let list = NoSoakList::new(vec![parse_entry("hashicorp/tap/terraform").unwrap()]);
        assert!(list.matches("hashicorp/tap", "terraform"));
        assert!(!list.matches("hashicorp/tap", "vault"));
        assert!(!list.matches("homebrew/core", "terraform"));
    }

    #[test]
    fn core_and_cask_are_valid_tap_entries() {
        let list = NoSoakList::new(vec![parse_entry("homebrew/cask").unwrap()]);
        assert!(list.matches("homebrew/cask", "firefox"));
        assert!(!list.matches("homebrew/core", "firefox"));
    }

    #[test]
    fn malformed_entries_are_rejected_with_a_reason() {
        for raw in ["", "  ", "a/b/c/d", "/tap", "tap/", "a//c", "a/b/"] {
            let err = parse_entry(raw).expect_err(raw);
            assert!(
                err.contains(&format!("{raw:?}")) || raw.trim().is_empty(),
                "{raw:?}: {err}"
            );
        }
    }

    #[test]
    fn empty_list_matches_nothing() {
        assert!(NoSoakList::default().is_empty());
        assert!(!NoSoakList::default().matches("homebrew/core", "wget"));
    }

    use crate::brew::MockBrew;
    use std::collections::VecDeque;

    fn t(origin: &str, name: &str) -> Target {
        Target {
            origin: origin.into(),
            name: name.into(),
            switch_tap: false,
            kind: None,
        }
    }

    fn runs(brew: &MockBrew) -> Vec<Vec<String>> {
        brew.visible_runs.lock().unwrap().clone()
    }

    #[test]
    fn brew_token_is_bare_for_core_and_cask() {
        assert_eq!(brew_token("homebrew/core", "wget"), "wget");
        assert_eq!(brew_token("homebrew/cask", "firefox"), "firefox");
        assert_eq!(
            brew_token("ericfitz/tap", "brewsoak"),
            "ericfitz/tap/brewsoak"
        );
    }

    #[test]
    fn no_targets_runs_nothing() {
        let brew = MockBrew::new();
        let r = run_step(&brew, "upgrade", &[], &[], &mut Vec::new()).unwrap();
        assert_eq!(r, StepResult::default());
        assert!(runs(&brew).is_empty());
    }

    #[test]
    fn one_update_then_one_upgrade_with_full_tokens() {
        let brew = MockBrew::new();
        let mut out = Vec::new();
        let r = run_step(
            &brew,
            "upgrade",
            &["--verbose".into()],
            &[t("ericfitz/tap", "brewsoak"), t("homebrew/core", "wget")],
            &mut out,
        )
        .unwrap();
        let got = runs(&brew);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0], vec!["update".to_string()]);
        assert_eq!(
            got[1],
            vec!["upgrade", "--verbose", "ericfitz/tap/brewsoak", "wget"]
        );
        assert_eq!(r.count, 2);
        assert_eq!(r.status, Some(0));
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("Updating Homebrew\n")
        );
    }

    #[test]
    fn install_verb_uses_install_and_strips_subcommand_words_from_flags() {
        let brew = MockBrew::new();
        run_step(
            &brew,
            "install",
            &[
                "install".into(),
                "--cask".into(),
                "--ignore-dependencies".into(),
            ],
            &[t("homebrew/cask", "firefox")],
            &mut Vec::new(),
        )
        .unwrap();
        let got = runs(&brew);
        assert_eq!(got[1], vec!["install", "--cask", "firefox"]);
    }

    #[test]
    fn tap_switch_targets_use_reinstall_in_a_separate_run() {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![
                (0, Vec::new()),
                (0, Vec::new()),
                (
                    0,
                    b"\xf0\x9f\x8d\xba  /opt/homebrew/Cellar/vault/1.2.0: 5 files, 1MB\n".to_vec(),
                ),
            ])),
            ..MockBrew::new()
        };
        let targets = [
            t("homebrew/core", "wget"),
            Target {
                origin: "hashicorp/tap".into(),
                name: "vault".into(),
                switch_tap: true,
                kind: None,
            },
        ];
        let r = run_step(&brew, "upgrade", &[], &targets, &mut Vec::new()).unwrap();
        let got = runs(&brew);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[1], vec!["upgrade", "wget"]);
        assert_eq!(got[2], vec!["reinstall", "hashicorp/tap/vault"]);
        assert_eq!(r.switched, vec![targets[1].clone()]);
        assert_eq!(r.status, Some(0));
        assert_eq!(r.installed.get("vault").map(String::as_str), Some("1.2.0"));
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    #[test]
    fn switch_install_that_did_not_replace_keg_keeps_status_and_notes() {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![
                (0, Vec::new()),
                (
                    1,
                    b"Warning: hashicorp/tap/vault 1.2.0 is already installed and up-to-date.\n"
                        .to_vec(),
                ),
            ])),
            ..MockBrew::new()
        };
        let targets = [Target {
            origin: "hashicorp/tap".into(),
            name: "vault".into(),
            switch_tap: true,
            kind: None,
        }];
        let r = run_step(&brew, "upgrade", &[], &targets, &mut Vec::new()).unwrap();
        assert_eq!(
            r.status,
            Some(1),
            "already-installed masking must not apply to the switch run"
        );
        assert!(
            r.notes.iter().any(|n| n.contains("did not replace")
                && n.contains("brew uninstall vault")
                && n.contains("brew install hashicorp/tap/vault")
                && n.contains("--ignore-dependencies will not help")
                && !n.contains("run brew reinstall")),
            "{:?}",
            r.notes
        );
        assert!(r.switched.is_empty(), "{:?}", r.switched);
    }

    #[test]
    fn switch_run_keeps_clusters_and_value_options_and_notes_what_it_dropped() {
        let brew = MockBrew::new();
        let flags = ["-vyx", "--appdir=/x", "--language=en", "--no-ask", "--foo"].map(String::from);
        let r = run_step(
            &brew,
            "upgrade",
            &flags,
            &switch_target("hashicorp/tap", "packer"),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            runs(&brew)[1],
            vec![
                "reinstall",
                "-vy",
                "--appdir=/x",
                "--language=en",
                "--no-ask",
                "hashicorp/tap/packer"
            ]
        );
        assert!(
            r.notes.iter().any(|n| n
                == "brew reinstall does not accept -x, --foo; dropped from the no-soak tap switch"),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn dry_run_skips_the_switch_with_a_note_and_never_claims_a_failure() {
        for flag in ["--dry-run", "-n"] {
            let brew = MockBrew::new();
            let r = run_step(
                &brew,
                "upgrade",
                &[flag.to_string()],
                &switch_target("hashicorp/tap", "packer"),
                &mut Vec::new(),
            )
            .unwrap();
            let got = runs(&brew);
            assert_eq!(got, vec![vec!["update".to_string()]], "{flag}: {got:?}");
            assert_eq!(r.status, None, "{flag}");
            assert!(
                r.notes.iter().any(|n| n.contains("dry run")
                    && n.contains("hashicorp/tap/packer")
                    && !n.contains("did not replace")),
                "{flag}: {:?}",
                r.notes
            );
        }
    }

    #[test]
    fn switch_line_per_package_and_flags_reinstall_rejects_are_dropped() {
        let brew = MockBrew::new();
        let mut out = Vec::new();
        let flags = [
            "--greedy".to_string(),
            "--fetch-HEAD".to_string(),
            "--overwrite".to_string(),
            "--no-quit".to_string(),
            "--some-future-flag".to_string(),
            "--verbose".to_string(),
        ];
        run_step(
            &brew,
            "upgrade",
            &flags,
            &switch_target("hashicorp/tap", "packer"),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("reinstalling hashicorp/tap/packer (moving staged keg to its tap)"),
            "{text}"
        );
        assert!(!text.contains("no-soak: brew"), "{text}");
        assert_eq!(
            runs(&brew)[1],
            vec!["reinstall", "--verbose", "hashicorp/tap/packer"]
        );
    }

    #[test]
    fn short_g_never_reaches_reinstall_but_plain_upgrade_keeps_greedy() {
        for (flag, want) in [("-g", vec![]), ("-vg", vec!["-v"])] {
            let brew = MockBrew::new();
            run_step(
                &brew,
                "upgrade",
                &[flag.to_string()],
                &switch_target("hashicorp/tap", "packer"),
                &mut Vec::new(),
            )
            .unwrap();
            let got = runs(&brew);
            let mut expect = vec!["reinstall".to_string()];
            expect.extend(want.iter().map(|s| s.to_string()));
            expect.push("hashicorp/tap/packer".into());
            assert_eq!(got[1], expect, "`g` is --git to brew reinstall: {flag}");
        }
        for flag in ["-g", "--greedy"] {
            let brew = MockBrew::new();
            run_step(
                &brew,
                "upgrade",
                &[flag.to_string()],
                &[t("homebrew/cask", "foo")],
                &mut Vec::new(),
            )
            .unwrap();
            assert_eq!(runs(&brew)[1], vec!["upgrade", flag, "foo"]);
        }
    }

    fn mock_with(outs: Vec<(i32, &[u8])>) -> MockBrew {
        MockBrew {
            next_outputs: std::sync::Mutex::new(
                outs.into_iter().map(|(c, o)| (c, o.to_vec())).collect(),
            ),
            ..MockBrew::new()
        }
    }

    #[test]
    fn classifies_upgraded_current_and_no_result() {
        let brew = mock_with(vec![
            (0, b""),
            (
                0,
                b"Warning: ericfitz/tap/brewsoak 1.5.2 already installed\n\
                  ==> Upgrading ericfitz/tap/agentbus\n  1.15.0 -> 1.16.0\n",
            ),
        ]);
        let targets = [
            t("ericfitz/tap", "agentbus"),
            t("ericfitz/tap", "brewsoak"),
            t("ericfitz/tap", "sqdist"),
        ];
        let mut out = Vec::new();
        let r = run_step(&brew, "upgrade", &[], &targets, &mut out).unwrap();
        assert_eq!((r.upgraded, r.current, r.no_result), (1, 1, 1));
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "Updating Homebrew\nUpgrading no-soak packages (brew decides)\n  ericfitz/tap/sqdist\n    no result from brew; see the full log\n"
        );
    }

    #[test]
    fn classifies_by_last_segment_and_ignores_case() {
        let brew = mock_with(vec![
            (0, b""),
            (
                0,
                b"==> Pouring AgentBus--1.16.0.arm64_sequoia.bottle.tar.gz\n",
            ),
        ]);
        let r = run_step(
            &brew,
            "upgrade",
            &[],
            &[t("ericfitz/tap", "agentbus")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!((r.upgraded, r.current, r.no_result), (1, 0, 0));
    }

    #[test]
    fn dry_run_list_counts_as_upgraded_and_header_says_would() {
        let brew = mock_with(vec![
            (0, b""),
            (0, b"==> Would upgrade 1 formula:\ncertifi 1 -> 2\n"),
        ]);
        let mut out = Vec::new();
        let r = run_step(
            &brew,
            "upgrade",
            &["--dry-run".to_string()],
            &[t("homebrew/core", "certifi"), t("homebrew/core", "other")],
            &mut out,
        )
        .unwrap();
        assert_eq!((r.upgraded, r.current, r.no_result), (1, 0, 1));
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\nWould upgrade no-soak packages (brew decides)\n"),
            "{text}"
        );
    }

    #[test]
    fn runs_in_nosoak_mode() {
        let brew = MockBrew::new();
        run_step(
            &brew,
            "upgrade",
            &[],
            &[t("homebrew/core", "wget")],
            &mut Vec::new(),
        )
        .unwrap();
        let ctx = brew.last_ctx.lock().unwrap().clone();
        assert_eq!(
            ctx,
            Some(quiet::RunContext {
                mode: quiet::RunMode::NoSoak
            })
        );
    }

    #[test]
    fn unknown_verb_is_rejected() {
        let brew = MockBrew::new();
        let err = run_step(
            &brew,
            "frobnicate",
            &[],
            &[t("homebrew/core", "wget")],
            &mut Vec::new(),
        )
        .expect_err("unknown verb");
        assert!(err.to_string().contains("frobnicate"), "{err}");
        assert!(runs(&brew).is_empty(), "no brew command may run");
    }

    #[test]
    fn dry_run_switch_target_is_reported_and_counted() {
        let brew = MockBrew::new();
        let mut out = Vec::new();
        let r = run_step(
            &brew,
            "upgrade",
            &["--dry-run".to_string()],
            &switch_target("hashicorp/tap", "packer"),
            &mut out,
        )
        .unwrap();
        assert_eq!((r.upgraded, r.current, r.no_result), (1, 0, 0));
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "Would upgrade no-soak packages (brew decides)\n  would reinstall hashicorp/tap/packer (moving staged keg to its tap)\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn failed_update_counts_every_target_as_no_result() {
        let brew = mock_with(vec![(3, b"Error: no network\n")]);
        let r = run_step(
            &brew,
            "upgrade",
            &[],
            &[t("ericfitz/tap", "a"), t("homebrew/core", "b")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!((r.upgraded, r.current, r.no_result), (0, 0, 2));
    }

    #[test]
    fn real_run_outdated_list_alone_is_not_an_upgrade() {
        let brew = mock_with(vec![
            (0, b""),
            (
                1,
                b"==> Upgrading 1 outdated package:\nfoo 1.0 -> 2.0\nError: boom\n",
            ),
        ]);
        let r = run_step(
            &brew,
            "upgrade",
            &[],
            &[t("homebrew/core", "foo")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!((r.upgraded, r.current, r.no_result), (0, 0, 1));
    }

    #[test]
    fn cellar_path_in_an_error_line_is_not_an_upgrade() {
        let brew = mock_with(vec![
            (0, b""),
            (
                1,
                b"Error: Could not symlink /opt/homebrew/Cellar/foo/1.0/bin/foo\n",
            ),
        ]);
        let r = run_step(
            &brew,
            "upgrade",
            &[],
            &[t("homebrew/core", "foo")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!((r.upgraded, r.current, r.no_result), (0, 0, 1));
    }

    #[test]
    fn install_verdicts_cover_real_and_dry_runs() {
        let brew = mock_with(vec![(0, b""), (0, b"==> Installing cowsay\n")]);
        let r = run_step(
            &brew,
            "install",
            &[],
            &[t("homebrew/core", "cowsay")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!((r.upgraded, r.current, r.no_result), (1, 0, 0));
        let brew = mock_with(vec![
            (0, b""),
            (
                0,
                b"==> Would install 1 formula:\ncowsay 3.8.4\n==> Would install 1 dependency for cowsay:\nperl\n",
            ),
        ]);
        let r = run_step(
            &brew,
            "install",
            &["--dry-run".to_string()],
            &[t("homebrew/core", "cowsay"), t("homebrew/core", "perl")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            (r.upgraded, r.current, r.no_result),
            (1, 0, 1),
            "dependency list is not a target verdict"
        );
    }

    #[test]
    fn output_nests_under_the_header_as_designed() {
        let mut f = quiet::Filter::new();
        f.start_run(&quiet::RunContext {
            mode: quiet::RunMode::NoSoak,
        });
        let mut out = Vec::new();
        {
            let mut s = Section::new(&mut out, nosoak_header(Verb::Upgrade, false));
            for l in [
                "Warning: ericfitz/tap/brewsoak 1.5.2 already installed",
                "==> Upgrading ericfitz/tap/agentbus",
                "  1.15.0 -> 1.16.0",
                "==> Pouring agentbus--1.16.0.arm64_sequoia.bottle.tar.gz",
                "==> Upgrading foo",
            ] {
                if let Some(o) = f.line(l) {
                    writeln!(s, "{o}").unwrap();
                }
            }
            if let Some(o) = f.finish_run() {
                writeln!(s, "{o}").unwrap();
            }
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Upgrading no-soak packages (brew decides)\n  ericfitz/tap/brewsoak 1.5.2\n    Already up-to-date.\n  upgrading ericfitz/tap/agentbus 1.15.0 -> 1.16.0\n    installing agentbus 1.16.0\n  upgrading foo\n"
        );
    }

    fn typed(origin: &str, name: &str, kind: PkgKind) -> Target {
        Target {
            kind: Some(kind),
            ..t(origin, name)
        }
    }

    #[test]
    fn same_name_formula_and_cask_run_in_separate_kind_groups() {
        let brew = MockBrew::new();
        let targets = [
            typed("homebrew/core", "vacuum", PkgKind::Formula),
            typed("homebrew/cask", "vacuum", PkgKind::Cask),
        ];
        run_step(&brew, "upgrade", &[], &targets, &mut Vec::new()).unwrap();
        let got = runs(&brew);
        assert_eq!(got.len(), 3, "one update, one run per kind: {got:?}");
        assert_eq!(got[0], vec!["update"]);
        assert_eq!(got[1], vec!["upgrade", "--formula", "vacuum"]);
        assert_eq!(got[2], vec!["upgrade", "--cask", "vacuum"]);
    }

    #[test]
    fn one_kind_keeps_the_single_unflagged_run() {
        let brew = MockBrew::new();
        let targets = [
            typed("homebrew/core", "wget", PkgKind::Formula),
            typed("homebrew/core", "curl", PkgKind::Formula),
        ];
        run_step(&brew, "upgrade", &[], &targets, &mut Vec::new()).unwrap();
        assert_eq!(runs(&brew)[1], vec!["upgrade", "wget", "curl"]);
    }

    #[test]
    fn switch_only_applies_to_upgrade() {
        let brew = MockBrew::new();
        let targets = [Target {
            origin: "hashicorp/tap".into(),
            name: "vault".into(),
            switch_tap: true,
            kind: None,
        }];
        run_step(&brew, "reinstall", &[], &targets, &mut Vec::new()).unwrap();
        assert_eq!(runs(&brew)[1], vec!["reinstall", "hashicorp/tap/vault"]);
    }

    #[test]
    fn failed_brew_update_fails_the_step_only() {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![(
                3,
                b"Error: no network\n".to_vec(),
            )])),
            ..MockBrew::new()
        };
        let r = run_step(
            &brew,
            "upgrade",
            &[],
            &[t("ericfitz/tap", "brewsoak"), t("homebrew/core", "wget")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(runs(&brew).len(), 1, "no upgrade after a failed update");
        assert_eq!(r.status, Some(3));
        assert_eq!(r.count, 2);
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("brew update failed (exit 3)")
                    && n.contains("ericfitz/tap/brewsoak, wget")),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn plain_run_already_installed_nonzero_is_success() {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![
                (0, Vec::new()),
                (
                    1,
                    b"Warning: wget 1.0 is already installed and up-to-date.\n".to_vec(),
                ),
            ])),
            ..MockBrew::new()
        };
        let r = run_step(
            &brew,
            "upgrade",
            &[],
            &[t("homebrew/core", "wget")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(r.status, Some(0));
    }

    fn switch_target(origin: &str, name: &str) -> [Target; 1] {
        [Target {
            origin: origin.into(),
            name: name.into(),
            switch_tap: true,
            kind: None,
        }]
    }

    fn switch_with(out: (i32, &[u8]), targets: &[Target]) -> StepResult {
        let brew = MockBrew {
            next_outputs: std::sync::Mutex::new(VecDeque::from(vec![
                (0, Vec::new()),
                (out.0, out.1.to_vec()),
            ])),
            ..MockBrew::new()
        };
        run_step(&brew, "upgrade", &[], targets, &mut Vec::new()).unwrap()
    }

    #[test]
    fn switch_exit_zero_already_installed_is_not_a_replacement() {
        let r = switch_with(
            (
                0,
                b"Warning: vault 1.2.0 is already installed and up-to-date.\n",
            ),
            &switch_target("hashicorp/tap", "vault"),
        );
        assert_eq!(r.status, Some(1));
        assert!(r.switched.is_empty());
        assert!(
            r.notes.iter().any(|n| n.contains("did not replace")),
            "{:?}",
            r.notes
        );
        assert!(r.installed.is_empty(), "{:?}", r.installed);
    }

    #[test]
    fn cask_switch_success_is_replaced_and_already_installed_is_not() {
        let ok = switch_with(
            (0, b"==> Installing Cask foo\n"),
            &switch_target("acme/tap", "foo"),
        );
        assert_eq!(ok.status, Some(0));
        assert!(ok.notes.is_empty(), "{:?}", ok.notes);
        let stale = switch_with(
            (0, b"Warning: Cask 'foo' is already installed.\n"),
            &switch_target("acme/tap", "foo"),
        );
        assert_eq!(stale.status, Some(1));
        assert!(
            stale
                .notes
                .iter()
                .any(|n| n.contains("brew install acme/tap/foo")),
            "{:?}",
            stale.notes
        );
    }
}
