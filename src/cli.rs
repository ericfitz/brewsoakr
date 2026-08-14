use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub soak_hours: Option<u32>,
    pub command: Command,
    pub brew_args: Vec<String>,
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

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn version_line() -> String {
    format!("brewsoak {VERSION}")
}

pub fn help_text() -> &'static str {
    "\
Usage: brewsoak [options] <command> [args...]

A Homebrew wrapper that delays core/cask updates for a soak window.

Soaked commands:
  update, upgrade, install, reinstall, outdated, info
Other brew commands are passed through unchanged.

Options:
  --soak-hours <N>   soak window in hours (default 24; also BREWSOAK_SOAK_HOURS)
  -v, --verbose      show soak window, cutoff, and every package evaluated
  -V, --version      print brewsoak version and exit
  -h, --help         show this help
  help <command>     soak-aware help for a soaked command; else brew help

Examples:
  brewsoak update
  brewsoak outdated
  brewsoak upgrade
  brewsoak info wget
  brewsoak --version
"
}

pub fn parse_argv(args: &[String]) -> Result<Invocation, Error> {
    if args.is_empty() {
        return Err(Error::Usage(help_text().to_string()));
    }

    let (soak_hours, remaining) = extract_soak_hours(args)?;
    if remaining.iter().any(|a| a == "--version" || a == "-V") {
        return Ok(Invocation {
            soak_hours,
            command: Command::Version,
            brew_args: Vec::new(),
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
        });
    }

    let Some(sub_idx) = remaining.iter().position(|a| !a.starts_with('-')) else {
        return Ok(passthrough(soak_hours, remaining));
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
        });
    }
    let before = &remaining[..sub_idx];
    let after = &remaining[sub_idx + 1..];

    let invocation = match subcommand {
        "update" => Invocation {
            soak_hours,
            command: Command::Update,
            brew_args: chain_args(before, after),
        },
        "outdated" => Invocation {
            soak_hours,
            command: Command::Outdated,
            brew_args: chain_args(before, after),
        },
        "upgrade" => {
            let (names, brew_args) = split_names_and_flags(before, after);
            Invocation {
                soak_hours,
                command: Command::Upgrade { names },
                brew_args,
            }
        }
        "reinstall" => {
            let (names, brew_args) = split_names_and_flags(before, after);
            Invocation {
                soak_hours,
                command: Command::Reinstall { names },
                brew_args,
            }
        }
        "info" => {
            let (names, brew_args) = split_names_and_flags(before, after);
            Invocation {
                soak_hours,
                command: Command::Info { names },
                brew_args,
            }
        }
        "install" => {
            let (names, brew_args, force_cask, force_formula) = split_install_args(before, after);
            Invocation {
                soak_hours,
                command: Command::Install {
                    names,
                    force_cask,
                    force_formula,
                },
                brew_args,
            }
        }
        _ => passthrough(soak_hours, remaining),
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

fn passthrough(soak_hours: Option<u32>, remaining: Vec<String>) -> Invocation {
    Invocation {
        soak_hours,
        command: Command::Passthrough {
            args: remaining.clone(),
        },
        brew_args: remaining,
    }
}

fn chain_args(before: &[String], after: &[String]) -> Vec<String> {
    [before, after].concat()
}

fn split_names_and_flags(before: &[String], after: &[String]) -> (Vec<String>, Vec<String>) {
    let mut brew_args = before.to_vec();
    let mut names = Vec::new();
    for arg in after {
        if arg.starts_with('-') {
            brew_args.push(arg.clone());
        } else {
            names.push(arg.clone());
        }
    }
    (names, brew_args)
}

pub fn command_help(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "update" => {
            "\
Usage: brewsoak update

Refresh soak snapshots for homebrew-core and homebrew-cask.
Does not update the Homebrew tool itself.

Prints soak hours, cutoff/HEAD SHAs (with cutoff time), fetch progress,
and a summary of installed packages that became eligible, are still
soaking, or are gone at HEAD.

  -v, --verbose   print every installed package and why it classified that way
"
        }
        "upgrade" => {
            "\
Usage: brewsoak upgrade [formula|cask ...]

Upgrade installed core/cask packages to the soaked (cutoff) artifact.
Packages born inside the soak window are held. Ahead-of-soak installs
are left unchanged. Pinned packages are skipped.

With no names, considers every installed core formula and cask.
Third-party tap tokens are passed through to brew.

  -v, --verbose   print soak window and a line for every package evaluated
"
        }
        "install" => {
            "\
Usage: brewsoak install [--formula|--cask] <name> ...

Install the soaked cutoff artifact if it is eligible.
Too-new / yanked / deprecated names are refused; use brew to bypass.

  -v, --verbose   print soak window and a line for every package evaluated
"
        }
        "reinstall" => {
            "\
Usage: brewsoak reinstall <name> ...

If the installed identity equals HEAD, runs brew reinstall (true repair).
Otherwise installs the soaked cutoff artifact. Ahead-of-soak is refused.

  -v, --verbose   print soak window and a line for every package evaluated
"
        }
        "outdated" => {
            "\
Usage: brewsoak outdated

List installed core/cask packages that upgrade would change, plus
held, ahead-of-soak, and pinned sections.

  -v, --verbose   print soak window and a line for every package evaluated
"
        }
        "info" => {
            "\
Usage: brewsoak info [formula|cask ...]

Show installed, cutoff, and HEAD identities and what brewsoak would do.
With no names, prints one compact line per installed core/cask package.
Named packages (or --verbose) print the long form.

  -v, --verbose   long form for every package plus soak window
"
        }
        _ => return None,
    })
}

fn split_install_args(
    before: &[String],
    after: &[String],
) -> (Vec<String>, Vec<String>, bool, bool) {
    let (names, brew_args) = split_names_and_flags(before, after);
    let force_cask = brew_args.iter().any(|a| a == "--cask");
    let force_formula = brew_args.iter().any(|a| a == "--formula");
    (names, brew_args, force_cask, force_formula)
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
}
