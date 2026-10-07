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
    "--vst3-plugindir",
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
        "--formulae",
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
        "--casks",
        "--require-sha",
        "--adopt",
        "--skip-cask-deps",
        "--zap",
        "--binaries",
        "--no-binaries",
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
        "--formulae",
        "--build-from-source",
        "--interactive",
        "--force-bottle",
        "--keep-tmp",
        "--debug-symbols",
        "--git",
        "--cask",
        "--casks",
        "--require-sha",
        "--adopt",
        "--skip-cask-deps",
        "--zap",
        "--binaries",
        "--no-binaries",
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
        "--formulae",
        "--build-from-source",
        "--interactive",
        "--force-bottle",
        "--fetch-HEAD",
        "--keep-tmp",
        "--debug-symbols",
        "--overwrite",
        "--cask",
        "--casks",
        "--skip-cask-deps",
        "--binaries",
        "--no-binaries",
        "--no-quit",
        "--greedy",
        "--greedy-latest",
        "--greedy-auto-updates",
        "--require-sha",
        "--quiet",
    ],
    value_long: &["--minimum-version", "--min-version"],
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
/// Value options arrive as `--name=value` (the CLI joins `--name value`). A
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
            if known {
                out.kept.push(f.clone());
            } else {
                out.dropped.push(f.clone());
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

/// Whether `--name` takes a value in any of the verbs (`--appdir`, `--cc`).
pub fn takes_value(name: &str) -> bool {
    [&INSTALL, &REINSTALL, &UPGRADE]
        .iter()
        .any(|t| is_value_option_of(t, name))
}

fn is_value_option_of(t: &Table, name: &str) -> bool {
    t.value_long.contains(&name) || CASK_VALUE_OPTIONS.contains(&name)
}

/// `--dry-run`, or `-n` alone or inside a short cluster (`-vn`).
pub fn is_dry_run(flags: &[String]) -> bool {
    flags
        .iter()
        .any(|f| f == "--dry-run" || (is_short_cluster(f) && f[1..].contains('n')))
}

/// Which self-updating casks a bare `upgrade`/`outdated` may touch. brew's
/// `--greedy` covers both kinds; the two long forms cover one each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Greedy {
    /// `--greedy` / `--greedy-auto-updates`: casks with `auto_updates true`.
    pub auto_updates: bool,
    /// `--greedy` / `--greedy-latest`: casks with `version :latest`.
    pub latest: bool,
}

/// Read the greedy flags brew's `upgrade` and `outdated` take. `-g`, alone
/// or in a short cluster (`-vg`), is `--greedy` on those verbs (brew's own
/// short; on `install` and `reinstall` the same letter is `--git`). Callers
/// gate on the verb: `upgrade` and `outdated` read it directly, and
/// `apply_resolved` reads it for every verb but only acts on it for a bare
/// run (`bare_run`).
pub fn greedy_mode(flags: &[String]) -> Greedy {
    let mut mode = Greedy::default();
    for f in flags {
        match f.as_str() {
            "--greedy" => {
                mode.auto_updates = true;
                mode.latest = true;
            }
            "--greedy-auto-updates" => mode.auto_updates = true,
            "--greedy-latest" => mode.latest = true,
            s if is_short_cluster(s) && s[1..].contains('g') => {
                mode.auto_updates = true;
                mode.latest = true;
            }
            _ => {}
        }
    }
    mode
}

/// `flags` without the greedy flags brewsoak consumed on an upgrade run:
/// the three long forms, and `g` removed from short clusters (a cluster
/// that was only `-g` disappears). What is left is what `brew install
/// <file>.rb` may see; the `install` table filter still applies after.
pub fn without_greedy(flags: &[String]) -> Vec<String> {
    flags
        .iter()
        .filter_map(|f| match f.as_str() {
            "--greedy" | "--greedy-auto-updates" | "--greedy-latest" => None,
            s if is_short_cluster(s) => {
                let kept: String = s[1..].chars().filter(|c| *c != 'g').collect();
                (!kept.is_empty()).then(|| format!("-{kept}"))
            }
            _ => Some(f.clone()),
        })
        .collect()
}

/// `-v`, `-vn`: a single dash followed by one or more letters.
pub(crate) fn is_short_cluster(s: &str) -> bool {
    s.len() > 1 && s.starts_with('-') && !s.starts_with("--")
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
    fn value_options_keep_their_value() {
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
    fn a_value_option_the_verb_lacks_is_dropped() {
        let got = filter_for_verb("reinstall", &f(&["--cc=gcc", "-v"]));
        assert_eq!(got.kept, f(&["-v"]));
        assert_eq!(got.dropped, f(&["--cc=gcc"]));
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
    fn lists_match_brew_help_for_aliases_and_vst3() {
        for verb in ["install", "reinstall", "upgrade"] {
            let got = filter_for_verb(
                verb,
                &f(&[
                    "--casks",
                    "--formulae",
                    "--binaries",
                    "--no-binaries",
                    "--vst3-plugindir=/v",
                ]),
            );
            assert!(got.dropped.is_empty(), "{verb}: {:?}", got.dropped);
            // `--vst` is a regex artefact of `--vst3-plugindir`, not an option.
            assert_eq!(filter_for_verb(verb, &f(&["--vst"])).dropped, f(&["--vst"]));
        }
        assert!(
            filter_for_verb("upgrade", &f(&["--min-version=1"]))
                .dropped
                .is_empty()
        );
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

    #[test]
    fn greedy_mode_reads_each_flag_with_brew_meaning() {
        assert_eq!(greedy_mode(&[]), Greedy::default());
        assert_eq!(
            greedy_mode(&f(&["--greedy"])),
            Greedy {
                auto_updates: true,
                latest: true
            }
        );
        assert_eq!(
            greedy_mode(&f(&["--greedy-auto-updates"])),
            Greedy {
                auto_updates: true,
                latest: false
            }
        );
        assert_eq!(
            greedy_mode(&f(&["--greedy-latest"])),
            Greedy {
                auto_updates: false,
                latest: true
            }
        );
        assert_eq!(
            greedy_mode(&f(&["--greedy-latest", "--greedy-auto-updates"])),
            Greedy {
                auto_updates: true,
                latest: true
            }
        );
    }

    #[test]
    fn greedy_mode_reads_g_in_a_cluster() {
        let both = Greedy {
            auto_updates: true,
            latest: true,
        };
        assert_eq!(greedy_mode(&f(&["-g"])), both);
        assert_eq!(greedy_mode(&f(&["-vg"])), both);
        assert_eq!(greedy_mode(&f(&["-v"])), Greedy::default());
    }

    #[test]
    fn greedy_mode_ignores_lookalikes() {
        // `filter_for_verb` drops a boolean flag given a value; so do we.
        assert_eq!(greedy_mode(&f(&["--greedy=yes"])), Greedy::default());
        assert_eq!(greedy_mode(&f(&["--greedy-foo"])), Greedy::default());
        assert_eq!(greedy_mode(&f(&["--g"])), Greedy::default());
        assert_eq!(greedy_mode(&f(&["greedy"])), Greedy::default());
    }

    #[test]
    fn without_greedy_strips_the_flags_and_the_letter() {
        let got = without_greedy(&f(&[
            "--greedy",
            "--verbose",
            "--greedy-latest",
            "-vg",
            "-g",
            "--greedy-auto-updates",
            "--greedy=yes",
        ]));
        assert_eq!(got, f(&["--verbose", "-v", "--greedy=yes"]));
        assert_eq!(
            without_greedy(&f(&["-v", "--force"])),
            f(&["-v", "--force"])
        );
    }
}
