use crate::{Error, SoakHours};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub soak_hours: Option<u32>,
    pub command: Command,
    pub brew_args: Vec<String>,
    /// Forward brew's output byte for byte instead of summarizing it.
    pub raw: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Update,
    Upgrade {
        names: Vec<String>,
    },
    Install {
        names: Vec<String>,
        force_cask: bool,
        force_formula: bool,
    },
    Reinstall {
        names: Vec<String>,
    },
    Outdated,
    Info {
        names: Vec<String>,
    },
    Version,
    Settings(SettingsCmd),
    Help {
        topic: Option<String>,
    },
    Passthrough {
        args: Vec<String>,
    },
}

impl Command {
    pub fn is_soaked(&self) -> bool {
        matches!(
            self,
            Command::Update
                | Command::Upgrade { .. }
                | Command::Install { .. }
                | Command::Reinstall { .. }
                | Command::Outdated
                | Command::Info { .. }
        )
    }
}

/// `brewsoak settings`: show, or one edit of the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsCmd {
    Show,
    Edit(SettingsEdit),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEdit {
    /// `None` is `--clear`. 24 is passed through; the editor removes the key.
    SoakHours(Option<SoakHours>),
    /// Raw tokens; `settings::no_soak_add` validates and lowercases them.
    NoSoakAdd(Vec<String>),
    NoSoakRemove(Vec<String>),
    NoSoakRepair,
    /// `tap` is normalized `user/repo`; `hours` `None` is `--clear`.
    TapHours {
        tap: String,
        hours: Option<SoakHours>,
    },
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn version_line() -> String {
    format!("brewsoak {VERSION}")
}

pub fn help_text() -> &'static str {
    "\
Usage: brewsoak [options] <command> [args...]

A Homebrew wrapper that delays core, cask, and third-party tap updates for a
soak window.

Soaked commands:
  update, upgrade, install, reinstall, outdated, info
Other brew commands are passed through unchanged. Packages and taps listed
under NO_SOAK in ~/.config/brewsoak/config.toml skip soaking and go to brew.

Settings (edit ~/.config/brewsoak/config.toml; never runs brew):
  settings [show]                         print the effective config
  settings soak-hours N|--clear           set or remove SOAK_HOURS
  settings no-soak add|remove TOKEN...    edit the NO_SOAK list
  settings tap-hours USER/REPO N|--clear  set or remove a tap's soak_hours
  settings repair                         fix invalid NO_SOAK and [[TAP]] entries

Options:
  --soak-hours <N>   soak window in hours (default 24; also BREWSOAK_SOAK_HOURS)
  -v, --verbose      show soak window, cutoff, and every package evaluated
      --raw          print brew's output unfiltered (also in $TMPDIR log)
  -V, --version      print brewsoak version and exit
  -h, --help         show this help
  help <command>     brewsoak help for a soaked command or settings; else brew help

Examples:
  brewsoak update
  brewsoak outdated
  brewsoak upgrade
  brewsoak info wget
  brewsoak settings no-soak add wget
  brewsoak --version
"
}

pub fn parse_argv(args: &[String]) -> Result<Invocation, Error> {
    if args.is_empty() {
        return Err(Error::Usage(help_text().to_string()));
    }

    let (raw, args) = extract_raw(args);
    let (soak_hours, remaining) = extract_soak_hours(&args)?;
    if remaining.iter().any(|a| a == "--version" || a == "-V") {
        return Ok(Invocation {
            soak_hours,
            command: Command::Version,
            brew_args: Vec::new(),
            raw,
        });
    }
    if remaining.iter().all(|a| a.starts_with('-'))
        && remaining
            .iter()
            .any(|a| a == "--help" || a == "-h" || a == "--verbose" || a == "-v")
    {
        return Ok(Invocation {
            soak_hours,
            command: Command::Help { topic: None },
            brew_args: Vec::new(),
            raw,
        });
    }

    let Some(sub_idx) = remaining.iter().position(|a| !a.starts_with('-')) else {
        return Ok(passthrough(soak_hours, remaining, raw));
    };

    let subcommand = remaining[sub_idx].as_str();
    if subcommand == "help" {
        let topic = remaining[sub_idx + 1..]
            .iter()
            .find(|a| !a.starts_with('-'))
            .cloned();
        return Ok(Invocation {
            soak_hours,
            command: Command::Help { topic },
            brew_args: Vec::new(),
            raw,
        });
    }
    if subcommand == "settings" {
        // Config edits are not a flag: `--soak-hours` is for soaked commands.
        if soak_hours.is_some() {
            return Err(Error::Usage(
                "--soak-hours cannot be combined with settings; use: brewsoak settings soak-hours N"
                    .into(),
            ));
        }
        if sub_idx != 0 {
            return Err(Error::Usage(format!(
                "settings takes no options before it, got: {}",
                remaining[..sub_idx].join(" ")
            )));
        }
        let rest = &remaining[sub_idx + 1..];
        if rest.iter().any(|a| a == "--help" || a == "-h") {
            return Ok(Invocation {
                soak_hours: None,
                command: Command::Help {
                    topic: Some("settings".into()),
                },
                brew_args: Vec::new(),
                raw,
            });
        }
        return Ok(Invocation {
            soak_hours: None,
            command: Command::Settings(parse_settings(rest)?),
            brew_args: Vec::new(),
            raw,
        });
    }
    let before = &remaining[..sub_idx];
    let after = &remaining[sub_idx + 1..];

    let invocation = match subcommand {
        "update" => Invocation {
            soak_hours,
            command: Command::Update,
            brew_args: chain_args(before, after),
            raw,
        },
        "outdated" => Invocation {
            soak_hours,
            command: Command::Outdated,
            brew_args: chain_args(before, after),
            raw,
        },
        "upgrade" => {
            let (names, brew_args) = split_names_and_flags(before, after)?;
            Invocation {
                soak_hours,
                command: Command::Upgrade { names },
                brew_args,
                raw,
            }
        }
        "reinstall" => {
            let (names, brew_args) = split_names_and_flags(before, after)?;
            // brew reinstall has no dry run, and dropping the flag would run
            // the reinstall for real.
            if crate::flags::is_dry_run(&brew_args) {
                return Err(Error::Usage(
                    "brew reinstall has no dry run (--dry-run / -n); not reinstalling".into(),
                ));
            }
            Invocation {
                soak_hours,
                command: Command::Reinstall { names },
                brew_args,
                raw,
            }
        }
        "info" => {
            let (names, brew_args) = split_names_and_flags(before, after)?;
            Invocation {
                soak_hours,
                command: Command::Info { names },
                brew_args,
                raw,
            }
        }
        "install" => {
            let (names, brew_args, force_cask, force_formula) = split_install_args(before, after)?;
            Invocation {
                soak_hours,
                command: Command::Install {
                    names,
                    force_cask,
                    force_formula,
                },
                brew_args,
                raw,
            }
        }
        _ => passthrough(soak_hours, remaining, raw),
    };
    Ok(invocation)
}

fn extract_soak_hours(args: &[String]) -> Result<(Option<u32>, Vec<String>), Error> {
    let mut soak_hours = None;
    let mut remaining = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--soak-hours=") {
            soak_hours = Some(parse_soak_value(value)?);
        } else if arg == "--soak-hours" {
            let value = iter
                .next()
                .ok_or_else(|| Error::Usage("missing value for --soak-hours".into()))?;
            soak_hours = Some(parse_soak_value(value)?);
        } else {
            remaining.push(arg.clone());
        }
    }
    Ok((soak_hours, remaining))
}

fn parse_soak_value(value: &str) -> Result<u32, Error> {
    value
        .parse::<u32>()
        .map_err(|_| Error::Usage(format!("--soak-hours must be an integer, got {value:?}")))
}

fn passthrough(soak_hours: Option<u32>, remaining: Vec<String>, raw: bool) -> Invocation {
    Invocation {
        soak_hours,
        command: Command::Passthrough {
            args: remaining.clone(),
        },
        brew_args: remaining,
        raw,
    }
}

/// `--raw` is ours, not brew's; strip it before anything reaches brew.
fn extract_raw(args: &[String]) -> (bool, Vec<String>) {
    let raw = args.iter().any(|a| a == "--raw");
    if !raw {
        return (false, args.to_vec());
    }
    (
        true,
        args.iter().filter(|a| *a != "--raw").cloned().collect(),
    )
}

fn chain_args(before: &[String], after: &[String]) -> Vec<String> {
    [before, after].concat()
}

/// Words after the subcommand: package names, and brew flags. A value option
/// given as `--opt value` becomes `--opt=value`, so its value is never taken
/// for a package name and a later `brew` cannot read a package as its value.
fn split_names_and_flags(
    before: &[String],
    after: &[String],
) -> Result<(Vec<String>, Vec<String>), Error> {
    let mut brew_args = before.to_vec();
    let mut names = Vec::new();
    let mut iter = after.iter();
    while let Some(arg) = iter.next() {
        if arg.starts_with("--") && !arg.contains('=') && crate::flags::takes_value(arg) {
            let value = iter
                .next()
                .ok_or_else(|| Error::Usage(format!("{arg} needs a value")))?;
            brew_args.push(format!("{arg}={value}"));
        } else if arg.starts_with('-') {
            brew_args.push(arg.clone());
        } else {
            names.push(arg.clone());
        }
    }
    Ok((names, brew_args))
}

/// Strict: every verb takes a fixed shape, and anything else is usage.
fn parse_settings(args: &[String]) -> Result<SettingsCmd, Error> {
    let hint = "see: brewsoak help settings";
    let Some((verb, rest)) = args.split_first() else {
        return Ok(SettingsCmd::Show);
    };
    let edit = match verb.as_str() {
        "show" => {
            if !rest.is_empty() {
                return Err(Error::Usage(format!(
                    "settings show takes no arguments, got: {}; {hint}",
                    rest.join(" ")
                )));
            }
            return Ok(SettingsCmd::Show);
        }
        "soak-hours" => {
            let [arg] = rest else {
                return Err(Error::Usage(format!(
                    "settings soak-hours takes exactly one argument: N or --clear; {hint}"
                )));
            };
            SettingsEdit::SoakHours(parse_hours_or_clear(arg, "soak-hours")?)
        }
        "no-soak" => {
            let Some((op, tokens)) = rest.split_first() else {
                return Err(Error::Usage(format!(
                    "settings no-soak needs add or remove and at least one token; {hint}"
                )));
            };
            if tokens.is_empty() {
                return Err(Error::Usage(format!(
                    "settings no-soak {op} needs at least one token; {hint}"
                )));
            }
            if let Some(flag) = tokens.iter().find(|t| t.starts_with('-')) {
                return Err(Error::Usage(format!(
                    "settings no-soak {op} takes tokens, not options, got {flag:?}; {hint}"
                )));
            }
            match op.as_str() {
                "add" => SettingsEdit::NoSoakAdd(tokens.to_vec()),
                "remove" => SettingsEdit::NoSoakRemove(tokens.to_vec()),
                other => {
                    return Err(Error::Usage(format!(
                        "unknown settings no-soak verb {other:?}; expected add or remove; {hint}"
                    )));
                }
            }
        }
        "repair" => {
            if !rest.is_empty() {
                return Err(Error::Usage(format!(
                    "settings repair takes no arguments, got: {}; {hint}",
                    rest.join(" ")
                )));
            }
            SettingsEdit::NoSoakRepair
        }
        "tap-hours" => {
            let [tap, arg] = rest else {
                return Err(Error::Usage(format!(
                    "settings tap-hours takes exactly two arguments: USER/REPO and N or --clear; {hint}"
                )));
            };
            SettingsEdit::TapHours {
                tap: crate::settings::normalize_tap(tap)?,
                hours: parse_hours_or_clear(arg, "tap-hours")?,
            }
        }
        other => {
            return Err(Error::Usage(format!(
                "unknown settings verb {other:?}; expected show, soak-hours, no-soak, repair, or tap-hours; {hint}"
            )));
        }
    };
    Ok(SettingsCmd::Edit(edit))
}

fn parse_hours_or_clear(arg: &str, verb: &str) -> Result<Option<SoakHours>, Error> {
    if arg == "--clear" {
        return Ok(None);
    }
    arg.parse::<u32>()
        .ok()
        .and_then(SoakHours::new)
        .map(Some)
        .ok_or_else(|| {
            Error::Usage(format!(
                "settings {verb} needs an integer >= 1 or --clear, got {arg:?}"
            ))
        })
}

pub fn command_help(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "update" => {
            "\
Usage: brewsoak update

Refresh soak snapshots for homebrew-core, homebrew-cask, and every soaked tap.
Runs brew update once when any installed package is no-soak.

Prints soak hours, cutoff/HEAD SHAs (with cutoff time), fetch progress,
and a summary of installed packages that became eligible, are still
soaking, or are gone at HEAD.

  -v, --verbose   print every installed package and why it classified that way
      --raw       print brew's output unfiltered (a full log is always
                  written under $TMPDIR; its path is printed at the end)
"
        }
        "upgrade" => {
            "\
Usage: brewsoak upgrade [formula|cask ...]

Upgrade installed packages to the soaked (cutoff) artifact.
Packages born inside the soak window are held. Ahead-of-soak installs
are left unchanged. Pinned packages are skipped.

With no names, considers every installed formula and cask.
Third-party tap packages are soaked from brewsoak's own tap clones.
Packages in NO_SOAK (no-soak) are handed to brew after the soaked work
(one brew update, then one brew upgrade); their outdated dependencies go
with them.
Taps without an HTTPS remote are not soakable and are noted, not upgraded.
Casks that update themselves (auto_updates true) are left to the app on a
bare upgrade, as brew leaves them without --greedy; name one to upgrade it.
  -g, --greedy              also upgrade self-updating casks to their cutoff,
                            and consider version :latest casks
      --greedy-auto-updates self-updating casks only
      --greedy-latest       version :latest casks only
A version :latest cask is reinstalled only when its cutoff definition
differs from the installed one; otherwise it is left with a note that the
contents of a :latest cask cannot be soaked.

  -v, --verbose   print soak window and a line for every package evaluated
      --raw       print brew's output unfiltered (a full log is always
                  written under $TMPDIR; its path is printed at the end)
"
        }
        "install" => {
            "\
Usage: brewsoak install [--formula|--cask] <name> ...

Install the soaked cutoff artifact if it is eligible.
Too-new / yanked / deprecated names are refused; use brew to bypass.
user/repo/name tokens are soaked (or no-soak) like any other package.

  -v, --verbose   print soak window and a line for every package evaluated
      --raw       print brew's output unfiltered (a full log is always
                  written under $TMPDIR; its path is printed at the end)
"
        }
        "reinstall" => {
            "\
Usage: brewsoak reinstall <name> ...

If the installed identity equals HEAD, runs brew reinstall (true repair).
Otherwise installs the soaked cutoff artifact. Ahead-of-soak is refused.
user/repo/name tokens are soaked (or no-soak) like any other package.

  -v, --verbose   print soak window and a line for every package evaluated
      --raw       print brew's output unfiltered (a full log is always
                  written under $TMPDIR; its path is printed at the end)
"
        }
        "outdated" => {
            "\
Usage: brewsoak outdated

List installed packages that upgrade would change, plus held,
ahead-of-soak, auto-updates, and pinned sections.

  -g, --greedy              list self-updating casks behind their cutoff
                            under Outdated instead of Auto-updates, and
                            consider version :latest casks
      --greedy-auto-updates self-updating casks only
      --greedy-latest       version :latest casks only
  With --greedy or --greedy-latest, -v notes each :latest cask left
  because its contents cannot be soaked.
  -v, --verbose   print soak window and a line for every package evaluated
      --raw       print brew's output unfiltered (a full log is always
                  written under $TMPDIR; its path is printed at the end)
"
        }
        "info" => {
            "\
Usage: brewsoak info [formula|cask ...]

Show installed, cutoff, and HEAD identities and what brewsoak would do.
With no names, prints one compact line per installed package.
Named packages (or --verbose) print the long form.
user/repo/name tokens are soaked (or no-soak) like any other package.
Shows origin tap and effective soak hours; no-soak packages are marked.

  -v, --verbose   long form for every package plus soak window
"
        }
        "settings" => {
            "\
Usage: brewsoak settings [show]
       brewsoak settings soak-hours N|--clear
       brewsoak settings no-soak add|remove TOKEN...
       brewsoak settings tap-hours USER/REPO N|--clear
       brewsoak settings repair

Show or edit ~/.config/brewsoak/config.toml. Edits keep comments, key order,
and unknown keys. Before each write the previous file is kept as
config.toml.<UTC time>.bak next to it; the 2 newest backups are kept.

  show                  effective soak hours and their source, NO_SOAK as
                        written, every [[TAP]] with its effective hours, and
                        every parse note and warning
  soak-hours N          set SOAK_HOURS (24, the default, removes the key)
  soak-hours --clear    remove SOAK_HOURS
  no-soak add TOKEN...  append tokens not already listed (case-insensitive)
  no-soak remove TOKEN...
                        remove matching tokens; an absent token is reported
  tap-hours USER/REPO N
                        set that tap's soak_hours, adding its [[TAP]] entry
  tap-hours USER/REPO --clear
                        remove soak_hours; an entry left with only name goes
  repair                make the file what brewsoak reads: wrap a lone
                        NO_SOAK string in an array, drop invalid NO_SOAK
                        entries, move a NO_SOAK written inside [[TAP]] up to
                        the top-level list, drop invalid or duplicate [[TAP]]
                        entries and bad soak_hours. Valid entries keep their
                        text, order, and comments. An unfixable value is
                        refused; nothing to fix writes nothing.

TOKEN is wget, user/repo, or user/repo/name. N is an integer >= 1.
homebrew/core and homebrew/cask are valid USER/REPO values.
BREWSOAK_SOAK_HOURS overrides SOAK_HOURS in the file; soak-hours warns when
it is set. --soak-hours is not accepted with settings. A file that is not
valid TOML is refused, not rewritten.
"
        }
        _ => return None,
    })
}

fn split_install_args(
    before: &[String],
    after: &[String],
) -> Result<(Vec<String>, Vec<String>, bool, bool), Error> {
    let (names, brew_args) = split_names_and_flags(before, after)?;
    let force_cask = brew_args.iter().any(|a| a == "--cask");
    let force_formula = brew_args.iter().any(|a| a == "--formula");
    Ok((names, brew_args, force_cask, force_formula))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn upgrade_with_flag_before_and_after() {
        let i = parse_argv(&s(&["--soak-hours", "48", "upgrade", "-v", "wget"])).unwrap();
        assert_eq!(i.soak_hours, Some(48));
        assert!(matches!(i.command, Command::Upgrade { ref names } if names == &["wget"]));
        assert!(i.brew_args.iter().any(|a| a == "-v"));
    }

    #[test]
    fn soak_hours_after_subcommand() {
        let i = parse_argv(&s(&["upgrade", "--soak-hours=12", "foo"])).unwrap();
        assert_eq!(i.soak_hours, Some(12));
        assert!(matches!(i.command, Command::Upgrade { ref names } if names == &["foo"]));
    }

    #[test]
    fn services_is_passthrough_without_soak_flag() {
        let i = parse_argv(&s(&["--soak-hours", "48", "services", "start", "foo"])).unwrap();
        assert_eq!(i.soak_hours, Some(48));
        match i.command {
            Command::Passthrough { args } => {
                assert_eq!(args, s(&["services", "start", "foo"]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn spaced_value_option_joins_its_value_and_leaves_names_alone() {
        let inv = parse_argv(&s(&["upgrade", "--language", "en", "hashicorp/tap/packer"])).unwrap();
        match inv.command {
            Command::Upgrade { names } => assert_eq!(names, s(&["hashicorp/tap/packer"])),
            other => panic!("{other:?}"),
        }
        assert_eq!(inv.brew_args, s(&["--language=en"]));
        // What the tap switch then hands brew reinstall.
        let kept = crate::flags::filter_for_verb("reinstall", &inv.brew_args).kept;
        assert_eq!(kept, s(&["--language=en"]));
    }

    #[test]
    fn value_option_without_a_value_is_usage() {
        assert!(matches!(
            parse_argv(&s(&["upgrade", "--appdir"])),
            Err(Error::Usage(m)) if m.contains("--appdir")
        ));
    }

    #[test]
    fn reinstall_refuses_a_dry_run_it_cannot_honour() {
        for flag in ["--dry-run", "-n", "-vn"] {
            match parse_argv(&s(&["reinstall", flag, "wget"])) {
                Err(Error::Usage(m)) => assert!(m.contains("no dry run"), "{flag}: {m}"),
                other => panic!("{flag}: {other:?}"),
            }
        }
        assert!(parse_argv(&s(&["reinstall", "-v", "wget"])).is_ok());
    }

    #[test]
    fn missing_soak_value_is_usage() {
        assert!(matches!(
            parse_argv(&s(&["--soak-hours"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn no_args_is_usage() {
        assert!(matches!(parse_argv(&[]), Err(Error::Usage(_))));
    }

    #[test]
    fn help_mentions_no_soak_and_taps() {
        assert!(help_text().contains("NO_SOAK"));
        assert!(command_help("upgrade").unwrap().contains("no-soak"));
        assert!(
            !command_help("upgrade")
                .unwrap()
                .contains("passed through to brew")
        );
        assert!(command_help("update").unwrap().contains("brew update"));
        for verb in ["upgrade", "outdated"] {
            let text = command_help(verb).unwrap();
            assert!(text.contains("--greedy-auto-updates"), "{verb}: {text}");
            assert!(text.contains("--greedy-latest"), "{verb}: {text}");
            assert!(text.contains("cannot be soaked"), "{verb}: {text}");
        }
    }

    #[test]
    fn raw_flag_is_ours_and_never_reaches_brew() {
        let i = parse_argv(&s(&["upgrade", "--raw", "wget"])).unwrap();
        assert!(i.raw);
        assert!(
            !i.brew_args.iter().any(|a| a == "--raw"),
            "{:?}",
            i.brew_args
        );
        assert!(matches!(i.command, Command::Upgrade { ref names } if names == &["wget"]));
    }

    #[test]
    fn raw_flag_survives_passthrough() {
        let i = parse_argv(&s(&["--raw", "services", "list"])).unwrap();
        assert!(i.raw);
        match i.command {
            Command::Passthrough { args } => assert_eq!(args, s(&["services", "list"])),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn install_cask_flag() {
        let i = parse_argv(&s(&["install", "--cask", "firefox"])).unwrap();
        match i.command {
            Command::Install {
                names,
                force_cask,
                force_formula,
            } => {
                assert_eq!(names, ["firefox"]);
                assert!(force_cask);
                assert!(!force_formula);
            }
            other => panic!("{other:?}"),
        }
        assert!(i.brew_args.iter().any(|a| a == "--cask"));
    }

    #[test]
    fn install_formula_flag() {
        let i = parse_argv(&s(&["install", "--formula", "wget"])).unwrap();
        match i.command {
            Command::Install {
                names,
                force_cask,
                force_formula,
            } => {
                assert_eq!(names, ["wget"]);
                assert!(!force_cask);
                assert!(force_formula);
            }
            other => panic!("{other:?}"),
        }
        assert!(i.brew_args.iter().any(|a| a == "--formula"));
    }

    #[test]
    fn flags_before_subcommand_stay_in_brew_args() {
        let i = parse_argv(&s(&["-v", "upgrade", "wget"])).unwrap();
        assert!(matches!(i.command, Command::Upgrade { ref names } if names == &["wget"]));
        assert_eq!(i.brew_args, s(&["-v"]));
    }

    #[test]
    fn unknown_flags_stay_in_brew_args() {
        let i = parse_argv(&s(&["upgrade", "--debug", "--force", "wget"])).unwrap();
        assert!(matches!(i.command, Command::Upgrade { ref names } if names == &["wget"]));
        assert_eq!(i.brew_args, s(&["--debug", "--force"]));
    }

    #[test]
    fn update_leftover_names_go_to_brew_args() {
        let i = parse_argv(&s(&["update", "--force", "extra"])).unwrap();
        assert!(matches!(i.command, Command::Update));
        assert_eq!(i.brew_args, s(&["--force", "extra"]));
    }

    #[test]
    fn outdated_leftover_names_go_to_brew_args() {
        let i = parse_argv(&s(&["outdated", "wget"])).unwrap();
        assert!(matches!(i.command, Command::Outdated));
        assert_eq!(i.brew_args, s(&["wget"]));
    }

    #[test]
    fn non_integer_soak_hours_is_usage() {
        assert!(matches!(
            parse_argv(&s(&["--soak-hours", "nope", "upgrade"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            parse_argv(&s(&["upgrade", "--soak-hours=nope"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn version_flag_is_version_command() {
        for args in [s(&["--version"]), s(&["-V"]), s(&["upgrade", "--version"])] {
            let i = parse_argv(&args).unwrap();
            assert!(matches!(i.command, Command::Version), "{args:?} -> {i:?}");
        }
        let i = parse_argv(&s(&["-v"])).unwrap();
        assert!(
            matches!(i.command, Command::Help { topic: None }),
            "bare -v is brewsoak help, not brew -v: {i:?}"
        );
    }

    #[test]
    fn help_flag_is_help_command() {
        for args in [s(&["--help"]), s(&["-h"]), s(&["help"])] {
            let i = parse_argv(&args).unwrap();
            assert!(
                matches!(i.command, Command::Help { topic: None }),
                "{args:?} -> {i:?}"
            );
        }
        let help_cmd = parse_argv(&s(&["help", "install"])).unwrap();
        match help_cmd.command {
            Command::Help { topic: Some(topic) } => assert_eq!(topic, "install"),
            other => panic!("{other:?}"),
        }
        assert!(command_help("install").unwrap().contains("soak"));
        assert!(command_help("services").is_none());
    }

    #[test]
    fn help_text_documents_verbose_and_version() {
        let text = help_text();
        assert!(text.contains("--verbose"), "{text}");
        assert!(text.contains("--version"), "{text}");
        assert!(text.contains("--soak-hours"), "{text}");
        assert!(text.contains("outdated"), "{text}");
        assert_eq!(version_line(), format!("brewsoak {VERSION}"));
    }

    #[test]
    fn no_args_mentions_available_commands() {
        match parse_argv(&[]) {
            Err(Error::Usage(msg)) => {
                assert!(msg.contains("update"));
                assert!(msg.contains("upgrade"));
                assert!(msg.contains("install"));
                assert!(msg.contains("reinstall"));
                assert!(msg.contains("outdated"));
                assert!(msg.contains("info"));
                assert!(msg.contains("--verbose"));
                assert!(msg.contains("--version"));
            }
            other => panic!("{other:?}"),
        }
    }

    fn settings(args: &[&str]) -> SettingsCmd {
        let i = parse_argv(&s(args)).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        assert!(i.brew_args.is_empty(), "{args:?}: {:?}", i.brew_args);
        assert_eq!(i.soak_hours, None, "{args:?}");
        match i.command {
            Command::Settings(cmd) => cmd,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    fn hours(n: u32) -> Option<SoakHours> {
        Some(SoakHours::new(n).unwrap())
    }

    #[test]
    fn settings_bare_and_show() {
        assert_eq!(settings(&["settings"]), SettingsCmd::Show);
        assert_eq!(settings(&["settings", "show"]), SettingsCmd::Show);
        let i = parse_argv(&s(&["settings", "--raw", "show"])).unwrap();
        assert!(i.raw, "--raw is accepted and ignored");
        assert_eq!(i.command, Command::Settings(SettingsCmd::Show));
        assert!(!Command::Settings(SettingsCmd::Show).is_soaked());
    }

    #[test]
    fn settings_soak_hours_value_and_clear() {
        assert_eq!(
            settings(&["settings", "soak-hours", "48"]),
            SettingsCmd::Edit(SettingsEdit::SoakHours(hours(48)))
        );
        assert_eq!(
            settings(&["settings", "soak-hours", "24"]),
            SettingsCmd::Edit(SettingsEdit::SoakHours(hours(24))),
            "24 is the editor's business (it removes the key)"
        );
        assert_eq!(
            settings(&["settings", "soak-hours", "--clear"]),
            SettingsCmd::Edit(SettingsEdit::SoakHours(None))
        );
    }

    #[test]
    fn settings_no_soak_add_and_remove_keep_tokens_verbatim() {
        assert_eq!(
            settings(&[
                "settings",
                "no-soak",
                "add",
                "WGet",
                "hashicorp/tap/terraform"
            ]),
            SettingsCmd::Edit(SettingsEdit::NoSoakAdd(s(&[
                "WGet",
                "hashicorp/tap/terraform"
            ])))
        );
        assert_eq!(
            settings(&["settings", "no-soak", "remove", "wget"]),
            SettingsCmd::Edit(SettingsEdit::NoSoakRemove(s(&["wget"])))
        );
    }

    #[test]
    fn settings_repair_parses_and_takes_no_arguments() {
        assert_eq!(
            settings(&["settings", "repair"]),
            SettingsCmd::Edit(SettingsEdit::NoSoakRepair)
        );
        match parse_argv(&s(&["settings", "repair", "x", "y"])) {
            Err(Error::Usage(m)) => assert!(m.contains("x y"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn settings_tap_hours_normalizes_the_tap() {
        assert_eq!(
            settings(&["settings", "tap-hours", "HashiCorp/tap", "72"]),
            SettingsCmd::Edit(SettingsEdit::TapHours {
                tap: "hashicorp/tap".into(),
                hours: hours(72)
            })
        );
        assert_eq!(
            settings(&["settings", "tap-hours", "homebrew/core", "--clear"]),
            SettingsCmd::Edit(SettingsEdit::TapHours {
                tap: "homebrew/core".into(),
                hours: None
            })
        );
    }

    #[test]
    fn settings_help_forms() {
        for args in [
            s(&["settings", "--help"]),
            s(&["settings", "-h"]),
            s(&["settings", "no-soak", "-h"]),
            s(&["help", "settings"]),
        ] {
            match parse_argv(&args).unwrap().command {
                Command::Help { topic: Some(t) } => assert_eq!(t, "settings", "{args:?}"),
                other => panic!("{args:?}: {other:?}"),
            }
        }
        let text = command_help("settings").unwrap();
        for word in [
            "show",
            "soak-hours",
            "no-soak add",
            "no-soak remove",
            "tap-hours",
            "repair",
            "--clear",
            ".bak",
        ] {
            assert!(text.contains(word), "{word}: {text}");
        }
        assert!(help_text().contains("settings"), "{}", help_text());
    }

    #[test]
    fn settings_usage_matrix() {
        for args in [
            s(&["settings", "show", "x"]),
            s(&["settings", "soak-hours"]),
            s(&["settings", "soak-hours", "0"]),
            s(&["settings", "soak-hours", "abc"]),
            s(&["settings", "soak-hours", "48", "x"]),
            s(&["settings", "soak-hours", "--clear", "x"]),
            s(&["settings", "no-soak"]),
            s(&["settings", "no-soak", "add"]),
            s(&["settings", "no-soak", "frob", "x"]),
            s(&["settings", "no-soak", "add", "--flag"]),
            s(&["settings", "tap-hours"]),
            s(&["settings", "tap-hours", "a/b"]),
            s(&["settings", "tap-hours", "bad", "5"]),
            s(&["settings", "tap-hours", "a/b/c", "5"]),
            s(&["settings", "tap-hours", "a/b", "0"]),
            s(&["settings", "tap-hours", "a/b", "5", "x"]),
            s(&["settings", "repair", "x"]),
            s(&["settings", "repair", "--clear"]),
            s(&["settings", "bogus"]),
            s(&["settings", "-v"]),
            s(&["-v", "settings"]),
            s(&["--verbose", "settings", "show"]),
        ] {
            match parse_argv(&args) {
                Err(Error::Usage(_)) => {}
                other => panic!("{args:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn settings_rejects_soak_hours_flag_in_every_position() {
        for args in [
            s(&["--soak-hours", "1", "settings", "show"]),
            s(&["settings", "--soak-hours=1"]),
            s(&["settings", "soak-hours", "48", "--soak-hours", "1"]),
        ] {
            match parse_argv(&args) {
                Err(Error::Usage(m)) => assert!(m.contains("settings soak-hours"), "{args:?}: {m}"),
                other => panic!("{args:?}: {other:?}"),
            }
        }
    }
}
