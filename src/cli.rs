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
    Passthrough {
        args: Vec<String>,
    },
}

pub fn parse_argv(args: &[String]) -> Result<Invocation, Error> {
    if args.is_empty() {
        return Err(Error::Usage(
            "Usage: brewsoakr [--soak-hours <HOURS>] <command> [args...]\n\
             Available commands: update, upgrade, install, reinstall, outdated, info"
                .into(),
        ));
    }

    let (soak_hours, remaining) = extract_soak_hours(args)?;
    let Some(sub_idx) = remaining.iter().position(|a| !a.starts_with('-')) else {
        return Ok(passthrough(soak_hours, remaining));
    };

    let subcommand = remaining[sub_idx].as_str();
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
    fn help_and_flags_only_are_passthrough() {
        let help = parse_argv(&s(&["--help"])).unwrap();
        match help.command {
            Command::Passthrough { args } => assert_eq!(args, s(&["--help"])),
            other => panic!("{other:?}"),
        }
        let help_cmd = parse_argv(&s(&["help", "install"])).unwrap();
        match help_cmd.command {
            Command::Passthrough { args } => assert_eq!(args, s(&["help", "install"])),
            other => panic!("{other:?}"),
        }
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
            }
            other => panic!("{other:?}"),
        }
    }
}
