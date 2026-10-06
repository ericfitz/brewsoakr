//! Which user flags each brew verb accepts. The lists come from
//! `brew <verb> --help`; a flag brew would reject is dropped (and reported)
//! instead of making the whole brew run fail.

/// Flags a verb takes without a value, as `--long` names.
struct Table {
    bool_long: &'static [&'static str],
    /// Options that take a value (besides the cask ones): `--name=value` or
    /// `--name value`.
    value_long: &'static [&'static str],
    /// Single-letter flags, which may be clustered (`-vd`).
    shorts: &'static str,
}

/// Cask options common to install, reinstall and upgrade; all take a value.
const CASK_VALUE_OPTIONS: &[&str] = &[
    "--appdir",
    "--appimagedir",
    "--keyboard-layoutdir",
    "--colorpickerdir",
    "--prefpanedir",
    "--qlplugindir",
    "--mdimporterdir",
    "--dictionarydir",
    "--fontdir",
    "--servicedir",
    "--input-methoddir",
    "--internet-plugindir",
    "--audio-unit-plugindir",
    "--vst-plugindir",
    "--screen-saverdir",
    "--language",
];

const INSTALL: Table = Table {
    bool_long: &[
        "--debug",
        "--display-times",
        "--force",
        "--verbose",
        "--dry-run",
        "--no-ask",
        "--yes",
        "--formula",
        "--ignore-dependencies",
        "--only-dependencies",
        "--build-from-source",
        "--force-bottle",
        "--include-test",
        "--HEAD",
        "--fetch-HEAD",
        "--keep-tmp",
        "--debug-symbols",
        "--build-bottle",
        "--skip-post-install",
        "--skip-link",
        "--as-dependency",
        "--interactive",
        "--git",
        "--overwrite",
        "--cask",
        "--require-sha",
        "--adopt",
        "--skip-cask-deps",
        "--zap",
        "--vst",
        "--quiet",
    ],
    value_long: &["--cc", "--bottle-arch"],
    shorts: "dfvnysigq",
};

const REINSTALL: Table = Table {
    bool_long: &[
        "--debug",
        "--display-times",
        "--force",
        "--verbose",
        "--no-ask",
        "--yes",
        "--formula",
        "--build-from-source",
        "--interactive",
        "--force-bottle",
        "--keep-tmp",
        "--debug-symbols",
        "--git",
        "--cask",
        "--require-sha",
        "--adopt",
        "--skip-cask-deps",
        "--zap",
        "--vst",
        "--quiet",
    ],
    value_long: &[],
    shorts: "dfvysigq",
};

const UPGRADE: Table = Table {
    bool_long: &[
        "--debug",
        "--display-times",
        "--force",
        "--verbose",
        "--dry-run",
        "--no-ask",
        "--yes",
        "--formula",
        "--build-from-source",
        "--interactive",
        "--force-bottle",
        "--fetch-HEAD",
        "--keep-tmp",
        "--debug-symbols",
        "--overwrite",
        "--cask",
        "--skip-cask-deps",
        "--no-quit",
        "--greedy",
        "--greedy-latest",
        "--greedy-auto-updates",
        "--require-sha",
        "--vst",
        "--quiet",
    ],
    value_long: &["--minimum-version"],
    shorts: "dfvnysigq",
};

fn table(verb: &str) -> Option<&'static Table> {
    match verb {
        "install" => Some(&INSTALL),
        "reinstall" => Some(&REINSTALL),
        "upgrade" => Some(&UPGRADE),
        _ => None,
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filtered {
    pub kept: Vec<String>,
    /// Flags (as typed, a cluster's letters as `-x`) the verb does not accept.
    pub dropped: Vec<String>,
}

/// Split `flags` into what `brew <verb>` accepts and what it would reject.
/// A value option given as `--name value` keeps or drops its value with it. A
/// verb with no table keeps everything.
pub fn filter_for_verb(verb: &str, flags: &[String]) -> Filtered {
    let mut out = Filtered::default();
    let Some(t) = table(verb) else {
        out.kept = flags.to_vec();
        return out;
    };
    let mut i = 0;
    while i < flags.len() {
        let f = &flags[i];
        i += 1;
        if let Some(long) = f.strip_prefix("--") {
            let (name, has_value) = match long.split_once('=') {
                Some((n, _)) => (format!("--{n}"), true),
                None => (f.clone(), false),
            };
            let takes_value = is_value_option_of(t, &name);
            let known = takes_value || (!has_value && t.bool_long.contains(&name.as_str()));
            // `--name value`: the next word is this option's value.
            let spaced = !has_value
                && i < flags.len()
                && !flags[i].starts_with('-')
                && (takes_value || is_value_option(&name));
            if known {
                out.kept.push(f.clone());
                if spaced && takes_value {
                    out.kept.push(flags[i].clone());
                    i += 1;
                }
            } else {
                out.dropped.push(f.clone());
                if spaced {
                    out.dropped.push(flags[i].clone());
                    i += 1;
                }
            }
        } else if f.len() > 1 && f.starts_with('-') {
            let (ok, bad): (String, String) = f[1..].chars().partition(|c| t.shorts.contains(*c));
            if !ok.is_empty() {
                out.kept.push(format!("-{ok}"));
            }
            out.dropped.extend(bad.chars().map(|c| format!("-{c}")));
        } else {
            // A bare word such as a leftover subcommand: not ours to judge.
            out.kept.push(f.clone());
        }
    }
    out
}

/// Value options of any verb, so a dropped `--cc gcc` takes `gcc` with it.
fn is_value_option(name: &str) -> bool {
    [&INSTALL, &REINSTALL, &UPGRADE]
        .iter()
        .any(|t| is_value_option_of(t, name))
}

fn is_value_option_of(t: &Table, name: &str) -> bool {
    t.value_long.contains(&name) || CASK_VALUE_OPTIONS.contains(&name)
}

/// `brew reinstall does not accept --foo, -x; dropped from the tap switch`.
pub fn dropped_note(verb: &str, dropped: &[String], context: &str) -> Option<String> {
    (!dropped.is_empty()).then(|| {
        format!(
            "brew {verb} does not accept {}; dropped from {context}",
            dropped.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reinstall_keeps_yes_and_no_ask() {
        let got = filter_for_verb("reinstall", &f(&["-y", "--yes", "--no-ask"]));
        assert_eq!(got.kept, f(&["-y", "--yes", "--no-ask"]));
        assert!(got.dropped.is_empty());
    }

    #[test]
    fn short_cluster_is_filtered_per_letter() {
        let got = filter_for_verb("reinstall", &f(&["-vyn"]));
        assert_eq!(got.kept, f(&["-vy"]));
        assert_eq!(got.dropped, f(&["-n"]));
    }

    #[test]
    fn value_options_keep_their_value_in_both_forms() {
        let got = filter_for_verb(
            "reinstall",
            &f(&[
                "--appdir=/x",
                "--language=en",
                "--fontdir",
                "/f",
                "--greedy",
            ]),
        );
        assert_eq!(
            got.kept,
            f(&["--appdir=/x", "--language=en", "--fontdir", "/f"])
        );
        assert_eq!(got.dropped, f(&["--greedy"]));
    }

    #[test]
    fn a_dropped_value_option_takes_its_value_along() {
        let got = filter_for_verb("reinstall", &f(&["--cc", "gcc", "-v"]));
        assert_eq!(got.kept, f(&["-v"]));
        assert_eq!(got.dropped, f(&["--cc", "gcc"]));
    }

    #[test]
    fn a_boolean_flag_with_a_value_is_dropped() {
        let got = filter_for_verb("install", &f(&["--force=yes"]));
        assert_eq!(got.dropped, f(&["--force=yes"]));
    }

    #[test]
    fn install_rejects_upgrade_only_flags() {
        let got = filter_for_verb(
            "install",
            &f(&[
                "--greedy",
                "--greedy-latest",
                "--greedy-auto-updates",
                "--no-quit",
                "--verbose",
            ]),
        );
        assert_eq!(got.kept, f(&["--verbose"]));
        assert_eq!(got.dropped.len(), 4);
    }

    #[test]
    fn upgrade_keeps_greedy_and_g_means_greedy_there() {
        let got = filter_for_verb("upgrade", &f(&["--greedy", "-g", "--minimum-version=1"]));
        assert_eq!(got.dropped, Vec::<String>::new());
    }

    #[test]
    fn unknown_verb_keeps_everything() {
        let got = filter_for_verb("outdated", &f(&["--whatever"]));
        assert_eq!(got.kept, f(&["--whatever"]));
    }

    #[test]
    fn note_names_the_flags_and_the_context() {
        assert_eq!(
            dropped_note("reinstall", &f(&["--foo"]), "the tap switch").as_deref(),
            Some("brew reinstall does not accept --foo; dropped from the tap switch")
        );
        assert_eq!(dropped_note("reinstall", &[], "x"), None);
    }
}
