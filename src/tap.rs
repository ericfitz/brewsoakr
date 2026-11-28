//! Writing staged `.rb` files and building `brew install` args.

use crate::Error;
use crate::resolve::{PkgKind, PkgRef};
use std::path::{Path, PathBuf};

pub fn tap_formula_path(tap_root: &Path, name: &str) -> PathBuf {
    tap_root.join("Formula").join(format!("{name}.rb"))
}

pub fn tap_cask_path(tap_root: &Path, name: &str) -> PathBuf {
    tap_root.join("Casks").join(format!("{name}.rb"))
}

/// Writes `sanitize_unofficial(blob)` to `Formula/<name>.rb` or `Casks/<name>.rb`
/// through `.<name>.rb.tmp-<pid>` and a rename. The file is mode 0600
/// (`config::write_temp`); brew runs as the same user, so that is enough.
pub fn write_blob(tap_root: &Path, pkg: &PkgRef, blob: &[u8]) -> Result<PathBuf, Error> {
    let path = match pkg.kind {
        PkgKind::Formula => tap_formula_path(tap_root, &pkg.name),
        PkgKind::Cask => tap_cask_path(tap_root, &pkg.name),
    };
    crate::atomic::write_bytes(&path, &sanitize_unofficial(blob))?;
    Ok(path)
}

/// The cutoff blob with every `no_autobump!` stanza removed (Homebrew allows
/// the stanza only in official taps). Every other byte, line endings
/// included, is unchanged.
///
/// A stanza starts at a line whose first non-blank text is `no_autobump!` and
/// covers:
/// - that line;
/// - each following line while the previous one ends in `,` or `\`;
/// - when the last of those lines ends in a `do` (with or without `|args|`),
///   everything through the line that closes the block: the first later line
///   that has the stanza's own indentation and starts with `end`.
///
/// A `do` block with no such closing line leaves the whole stanza in the file
/// untouched (brew then fails with its known error and the package holds).
/// A trailing Ruby `# comment` is ignored when looking at a line's end. Only
/// a line whose first non-blank text is `no_autobump!` starts a stanza, so
/// the word inside a string on another line is kept; a heredoc body line that
/// itself starts with it is not told apart from code and goes. Eric decided
/// on 2026-11-27 that multi-line stanzas are handled.
pub fn sanitize_unofficial(rb: &[u8]) -> Vec<u8> {
    const STANZA: &[u8] = b"no_autobump!";
    let lines: Vec<&[u8]> = rb.split_inclusive(|b| *b == b'\n').collect();
    let mut out = Vec::with_capacity(rb.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let indent = indent_of(line);
        if !line[indent.len()..].starts_with(STANZA) {
            out.extend_from_slice(line);
            i += 1;
            continue;
        }
        let mut last = i;
        while last + 1 < lines.len() && continues(lines[last]) {
            last += 1;
        }
        if !opens_block(lines[last]) {
            i = last + 1;
            continue;
        }
        match (last + 1..lines.len()).find(|&j| closes_block(lines[j], indent)) {
            Some(e) => i = e + 1,
            None => {
                // Unclosed block: keep the stanza as it is.
                for l in &lines[i..=last] {
                    out.extend_from_slice(l);
                }
                i = last + 1;
            }
        }
    }
    out
}

/// The leading blanks of a line (all of a blank line).
fn indent_of(line: &[u8]) -> &[u8] {
    let n = line
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(line.len());
    &line[..n]
}

/// The line without a trailing Ruby comment (a `#` outside a quoted string),
/// trailing whitespace and line ending.
fn code_of(line: &[u8]) -> &[u8] {
    let mut quote: Option<u8> = None;
    let mut cut = line.len();
    let mut k = 0;
    while k < line.len() {
        let b = line[k];
        match quote {
            Some(_) if b == b'\\' => k += 1,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'#' => {
                cut = k;
                break;
            }
            None => {}
        }
        k += 1;
    }
    let line = &line[..cut];
    let n = line
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |p| p + 1);
    &line[..n]
}

/// The line ends in `,` or `\`, so the next line belongs to the same statement.
fn continues(line: &[u8]) -> bool {
    matches!(code_of(line).last(), Some(b',' | b'\\'))
}

/// The line ends in `do` or `do |args|`.
fn opens_block(line: &[u8]) -> bool {
    let code = code_of(line);
    let code = match code.last() {
        Some(b'|') => match code[..code.len() - 1].iter().rposition(|b| *b == b'|') {
            Some(p) => code_of(&code[..p]),
            None => return false,
        },
        _ => code,
    };
    code.ends_with(b"do")
        && (code.len() == 2
            || !(code[code.len() - 3].is_ascii_alphanumeric() || code[code.len() - 3] == b'_'))
}

/// The line is indented exactly as `indent` and starts with `end` as a whole
/// word: end of line, whitespace or `#` follows (not `end:` or `end.foo`).
fn closes_block(line: &[u8], indent: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(indent) else {
        return false;
    };
    rest.strip_prefix(b"end").is_some_and(|after| {
        after
            .first()
            .is_none_or(|b| b.is_ascii_whitespace() || *b == b'#')
    })
}

/// `ericfitz/brewsoak/<name>`.
pub fn soak_token(name: &str) -> String {
    format!("{}/{name}", crate::origin::SOAK_TAP)
}

/// `upgrade` for an installed formula, else `install`.
pub fn soak_tap_verb(kind: PkgKind, installed: bool) -> &'static str {
    match (kind, installed) {
        (PkgKind::Formula, true) => "upgrade",
        (PkgKind::Formula, false) | (PkgKind::Cask, _) => "install",
    }
}

/// `brew <verb> --formula|--cask [flags] ericfitz/brewsoak/<name>`, flags
/// filtered for that verb.
pub fn soak_tap_args(pkg: &PkgRef, installed: bool, user_flags: &[String]) -> Vec<String> {
    let verb = soak_tap_verb(pkg.kind, installed);
    let mut wanted: Vec<String> = user_flags
        .iter()
        .filter(|f| !is_stripped_flag(f) && !is_kind_flag(f))
        .cloned()
        .collect();
    if verb == "upgrade" {
        wanted = crate::flags::without_greedy(&wanted);
    }
    let mut args = vec![
        verb.to_string(),
        match pkg.kind {
            PkgKind::Formula => "--formula".into(),
            PkgKind::Cask => "--cask".into(),
        },
    ];
    args.extend(crate::flags::filter_for_verb(verb, &wanted).kept);
    args.push(soak_token(&pkg.name));
    args
}

/// `brew install --formula|--cask [flags] <token>`; soaked installs pass the bare name.
pub fn brew_install_args(pkg: &PkgRef, token: &str, user_flags: &[String]) -> Vec<String> {
    let mut args = vec!["install".to_string()];
    args.push(match pkg.kind {
        PkgKind::Formula => "--formula".into(),
        PkgKind::Cask => "--cask".into(),
    });
    // brew install rejects upgrade-only flags such as `--greedy`.
    let wanted: Vec<String> = user_flags
        .iter()
        .filter(|f| !is_stripped_flag(f) && !is_kind_flag(f))
        .cloned()
        .collect();
    args.extend(crate::flags::filter_for_verb("install", &wanted).kept);
    args.push(token.to_string());
    args
}

/// The kind flags brew declares as conflicting with each other. The commands
/// built here carry the flag for the package's own kind, so the user's is dropped.
pub(crate) fn is_kind_flag(s: &str) -> bool {
    matches!(s, "--formula" | "--formulae" | "--cask" | "--casks")
}

/// Flags brewsoak never forwards to brew: subcommand words and the
/// unsupported `--ignore-dependencies` developer option.
pub(crate) fn is_stripped_flag(s: &str) -> bool {
    is_brew_subcommand(s) || s == "--ignore-dependencies"
}

pub(crate) fn is_brew_subcommand(s: &str) -> bool {
    matches!(
        s,
        "install" | "upgrade" | "reinstall" | "update" | "outdated" | "info"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formula(name: &str) -> PkgRef {
        PkgRef {
            name: name.to_string(),
            kind: PkgKind::Formula,
        }
    }

    fn cask(name: &str) -> PkgRef {
        PkgRef {
            name: name.to_string(),
            kind: PkgKind::Cask,
        }
    }

    #[test]
    fn write_blob_creates_formula_wget_rb() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let blob = b"class Wget < Formula; end\n";
        let path = write_blob(tmp.path(), &formula("wget"), blob).expect("write");
        assert_eq!(path, tmp.path().join("Formula/wget.rb"));
        assert_eq!(std::fs::read(&path).expect("read"), blob);
    }

    #[test]
    fn write_blob_creates_cask_firefox_rb() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let blob = b"cask \"firefox\"\n";
        let path = write_blob(tmp.path(), &cask("firefox"), blob).expect("write");
        assert_eq!(path, tmp.path().join("Casks/firefox.rb"));
        assert_eq!(std::fs::read(&path).expect("read"), blob);
    }

    #[test]
    fn brew_install_args_ends_with_the_bare_name() {
        let args = brew_install_args(&formula("wget"), "wget", &[]);
        assert_eq!(args, vec!["install", "--formula", "wget"]);
    }

    #[test]
    fn brew_install_args_strips_user_ignore_dependencies() {
        let flags = ["--ignore-dependencies".to_string(), "--verbose".to_string()];
        let args = brew_install_args(&formula("wget"), "wget", &flags);
        assert_eq!(args, vec!["install", "--formula", "--verbose", "wget"]);
    }

    #[test]
    fn brew_install_args_drops_flags_install_rejects() {
        let flags = [
            "--greedy",
            "--greedy-latest",
            "--greedy-auto-updates",
            "--ignore-pinned",
            "-vg",
            "--verbose",
        ]
        .map(String::from);
        let args = brew_install_args(&formula("wget"), "wget", &flags);
        assert_eq!(
            args,
            vec!["install", "--formula", "-vg", "--verbose", "wget"]
        );
    }

    #[test]
    fn brew_install_args_cask_forwards_user_flags_without_subcommand() {
        let flags = ["install".to_string(), "--appdir=/Apps".to_string()];
        let args = brew_install_args(&cask("firefox"), "firefox", &flags);
        assert_eq!(args, vec!["install", "--cask", "--appdir=/Apps", "firefox"]);
    }

    #[test]
    fn sanitize_unofficial_strips_no_autobump() {
        let rb = b"class Sqlite < Formula\n  url \"https://example.com/s.tgz\"\n  sha256 \"abc\"\n  no_autobump! because: :bumped_by_upstream\nend\n";
        let got = String::from_utf8(sanitize_unofficial(rb)).unwrap();
        assert!(!got.contains("no_autobump!"), "{got}");
        assert!(got.contains("class Sqlite"), "{got}");
    }

    #[test]
    fn sanitize_leaves_a_blob_without_the_stanza_byte_for_byte() {
        for blob in [
            &b"a\r\nb"[..],
            b"class X < Formula\nend",
            b"\xff\xfe raw\n",
            b"",
        ] {
            assert_eq!(sanitize_unofficial(blob), blob);
        }
    }

    #[test]
    fn sanitize_removes_a_stanza_continued_by_a_comma_or_a_backslash() {
        let rb = b"class S < Formula\n  no_autobump! :a,\n    because: :bumped_by_upstream\n  url \"u\"\n  no_autobump! \\\n    because: :x\nend\n";
        assert_eq!(
            sanitize_unofficial(rb),
            b"class S < Formula\n  url \"u\"\nend\n"
        );
        let rb = b"class S < Formula\r\n  no_autobump! :a,\r\n    :b,\r\n    :c\r\n  keep\r\nend";
        assert_eq!(
            sanitize_unofficial(rb),
            b"class S < Formula\r\n  keep\r\nend"
        );
    }

    #[test]
    fn sanitize_removes_a_do_block_stanza_through_its_own_end() {
        let rb = b"class S < Formula\n  no_autobump! because: :x do\n    if true\n      foo\n    end\n  end\n  keep do\n  end\nend\n";
        assert_eq!(
            sanitize_unofficial(rb),
            b"class S < Formula\n  keep do\n  end\nend\n"
        );
        let rb = b"cask \"x\" do\n  no_autobump! do |a|\n    a.b\n  end\n  keep\nend\n";
        assert_eq!(sanitize_unofficial(rb), b"cask \"x\" do\n  keep\nend\n");
    }

    #[test]
    fn sanitize_leaves_an_unclosed_block_stanza_untouched() {
        let rb = b"class S < Formula\n  no_autobump! do\n    x\nend\n";
        // The closing `end` has a different indent: nothing is removed.
        assert_eq!(sanitize_unofficial(rb), rb);
        let rb = b"class S < Formula\n  no_autobump! :a,\n    b do\n    x\nend\n  url \"u\"\n";
        assert_eq!(sanitize_unofficial(rb), rb);
    }

    #[test]
    fn sanitize_ignores_a_trailing_comment_when_looking_for_a_continuation() {
        let rb = b"class S < Formula\n  no_autobump! because: :x # why,\n  url \"u\"\n  no_autobump! :y # more \\\n  keep\nend\n";
        assert_eq!(
            sanitize_unofficial(rb),
            b"class S < Formula\n  url \"u\"\n  keep\nend\n"
        );
        // A `#` inside a string is not a comment.
        let rb =
            b"class S < Formula\n  no_autobump! because: \"a # b\",\n    c: 1\n  url \"u\"\nend\n";
        assert_eq!(
            sanitize_unofficial(rb),
            b"class S < Formula\n  url \"u\"\nend\n"
        );
        let rb = b"class S < Formula\n  no_autobump! do # block\n    x\n  end # done\n  url \"u\"\nend\n";
        assert_eq!(
            sanitize_unofficial(rb),
            b"class S < Formula\n  url \"u\"\nend\n"
        );
    }

    #[test]
    fn sanitize_closes_a_block_only_at_the_word_end() {
        let rb = b"class S < Formula\n  no_autobump! do\n  end: 1\n  end.foo\n  endx\n  end\n  url \"u\"\nend\n";
        assert_eq!(
            sanitize_unofficial(rb),
            b"class S < Formula\n  url \"u\"\nend\n"
        );
        // With no real `end`, the stanza stays.
        let rb = b"class S < Formula\n  no_autobump! do\n  end: 1\n  end.foo\nend\n";
        assert_eq!(sanitize_unofficial(rb), rb);
    }

    #[test]
    fn sanitize_only_a_line_starting_with_the_stanza_counts() {
        let rb = b"class S < Formula\n  desc \"no_autobump! in text\"\n  url \"u\" # no_autobump! x\n  puts <<~EOS\n    see no_autobump! here\n  EOS\nend\n";
        assert_eq!(sanitize_unofficial(rb), rb);
    }

    #[test]
    fn sanitize_does_not_treat_a_word_ending_in_do_as_a_block() {
        let rb = b"  no_autobump! because: :undo\n  keep\n  end\n";
        assert_eq!(sanitize_unofficial(rb), b"  keep\n  end\n");
    }

    #[test]
    fn sanitize_removes_only_the_stanza_line() {
        let rb = b"class S < Formula\r\n  no_autobump! because: :bumped_by_upstream\r\n  # no_autobump! stays\r\nend";
        assert_eq!(
            sanitize_unofficial(rb),
            b"class S < Formula\r\n  # no_autobump! stays\r\nend"
        );
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn write_blob_writes_the_sanitized_bytes_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let blob = b"class Wget < Formula\r\n  no_autobump! because: :x\r\n\xff\r\nend";
        let path = write_blob(tmp.path(), &formula("wget"), blob).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"class Wget < Formula\r\n\xff\r\nend"
        );
        assert_eq!(names(&tmp.path().join("Formula")), vec!["wget.rb"]);
    }

    #[test]
    fn write_blob_replaces_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), &formula("wget"), b"old old old\n").unwrap();
        let path = write_blob(tmp.path(), &formula("wget"), b"new\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(names(&tmp.path().join("Formula")), vec!["wget.rb"]);
    }

    #[test]
    fn write_blob_paths_for_odd_names() {
        let tmp = tempfile::tempdir().unwrap();
        for (pkg, rel) in [
            (formula("python@3.12"), "Formula/python@3.12.rb"),
            (formula("c++filt"), "Formula/c++filt.rb"),
            (cask("font-hack"), "Casks/font-hack.rb"),
            (formula("docker"), "Formula/docker.rb"),
            (cask("docker"), "Casks/docker.rb"),
        ] {
            let path = write_blob(tmp.path(), &pkg, rel.as_bytes()).unwrap();
            assert_eq!(path, tmp.path().join(rel));
            assert_eq!(std::fs::read(&path).unwrap(), rel.as_bytes());
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn soak_token_is_the_tap_and_name() {
        assert_eq!(soak_token("wget"), "ericfitz/brewsoak/wget");
    }

    #[test]
    fn soak_tap_args_follow_the_installed_state() {
        let f = formula("wget");
        assert_eq!(
            soak_tap_args(&f, false, &[]),
            ["install", "--formula", "ericfitz/brewsoak/wget"]
        );
        assert_eq!(
            soak_tap_args(&f, true, &[]),
            ["upgrade", "--formula", "ericfitz/brewsoak/wget"]
        );
        let c = cask("firefox");
        for installed in [false, true] {
            assert_eq!(
                soak_tap_args(&c, installed, &[]),
                ["install", "--cask", "ericfitz/brewsoak/firefox"]
            );
        }
        assert_eq!(soak_tap_verb(PkgKind::Formula, true), "upgrade");
        assert_eq!(soak_tap_verb(PkgKind::Cask, true), "install");
    }

    #[test]
    fn soak_tap_args_filter_flags_for_the_verb_run() {
        let flags = strs(&["--greedy", "--dry-run", "--git", "--ignore-dependencies"]);
        assert_eq!(
            soak_tap_args(&formula("wget"), true, &flags),
            [
                "upgrade",
                "--formula",
                "--dry-run",
                "ericfitz/brewsoak/wget"
            ]
        );
        assert_eq!(
            soak_tap_args(&formula("wget"), false, &flags),
            [
                "install",
                "--formula",
                "--dry-run",
                "--git",
                "ericfitz/brewsoak/wget"
            ]
        );
    }

    #[test]
    fn soak_tap_upgrade_never_receives_g() {
        assert_eq!(
            soak_tap_args(&formula("wget"), true, &strs(&["-g"])),
            ["upgrade", "--formula", "ericfitz/brewsoak/wget"]
        );
        assert_eq!(
            soak_tap_args(&formula("wget"), true, &strs(&["-vg"])),
            ["upgrade", "--formula", "-v", "ericfitz/brewsoak/wget"]
        );
    }

    #[test]
    fn soaked_commands_drop_the_users_kind_flags() {
        let flags = strs(&["--cask", "--formula", "--formulae", "--casks", "--dry-run"]);
        assert_eq!(
            soak_tap_args(&formula("wget"), false, &flags),
            [
                "install",
                "--formula",
                "--dry-run",
                "ericfitz/brewsoak/wget"
            ]
        );
        assert_eq!(
            soak_tap_args(&formula("wget"), true, &flags),
            [
                "upgrade",
                "--formula",
                "--dry-run",
                "ericfitz/brewsoak/wget"
            ]
        );
        assert_eq!(
            soak_tap_args(&cask("firefox"), false, &flags),
            [
                "install",
                "--cask",
                "--dry-run",
                "ericfitz/brewsoak/firefox"
            ]
        );
        assert_eq!(
            brew_install_args(&formula("wget"), "wget", &flags),
            ["install", "--formula", "--dry-run", "wget"]
        );
        assert_eq!(
            brew_install_args(&cask("firefox"), "firefox", &flags),
            ["install", "--cask", "--dry-run", "firefox"]
        );
    }

    #[test]
    fn write_blob_writes_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = write_blob(tmp.path(), &formula("wget"), b"x\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
