use crate::Error;
use crate::resolve::PkgKind;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPkg {
    pub name: String,
    pub kind: PkgKind,
    pub receipt_rb: String,
}

pub trait Brew {
    fn brew_bin(&self) -> &Path;
    fn run(&self, args: &[String]) -> Result<Output, Error>;
    fn installed_core(&self) -> Result<Vec<InstalledPkg>, Error>;
    fn tap_new_soaked(&self) -> Result<(), Error>;
    fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error>;
}

pub struct ProcessBrew {
    pub bin: PathBuf,
}

pub struct MockBrew {
    pub installed: Vec<InstalledPkg>,
    pub deps: BTreeMap<String, Vec<String>>,
    pub runs: Mutex<Vec<Vec<String>>>,
    pub next_status: i32,
}

impl Default for MockBrew {
    fn default() -> Self {
        Self {
            installed: Vec::new(),
            deps: BTreeMap::new(),
            runs: Mutex::new(Vec::new()),
            next_status: 0,
        }
    }
}

impl MockBrew {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, args: &[String]) {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(args.to_vec());
    }
}

impl Brew for ProcessBrew {
    fn brew_bin(&self) -> &Path {
        &self.bin
    }

    fn run(&self, args: &[String]) -> Result<Output, Error> {
        match Command::new(&self.bin).args(args).output() {
            Ok(output) => Ok(output),
            Err(e) if e.kind() == ErrorKind::NotFound => Err(Error::Usage("brew not found".into())),
            Err(e) => Err(Error::from(e)),
        }
    }

    fn installed_core(&self) -> Result<Vec<InstalledPkg>, Error> {
        let output = self.run(&["info".into(), "--json=v2".into(), "--installed".into()])?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        let json = String::from_utf8_lossy(&output.stdout);
        let cellar = self.brew_dir("--cellar");
        let caskroom = self.brew_dir("--caskroom");
        parse_installed_json(&json, |name, kind| match kind {
            PkgKind::Formula => cellar
                .as_deref()
                .and_then(|dir| read_formula_receipt(dir, name)),
            PkgKind::Cask => caskroom
                .as_deref()
                .and_then(|dir| find_named_rb(&dir.join(name), name)),
        })
    }

    fn tap_new_soaked(&self) -> Result<(), Error> {
        let output = self.run(&[
            "tap-new".into(),
            "brewsoakr/soaked".into(),
            "--no-git".into(),
        ])?;
        if output.status.success() || tap_already_exists(&output) {
            Ok(())
        } else {
            Err(brew_fail(&output))
        }
    }

    fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error> {
        let flag = match kind {
            PkgKind::Formula => "--formula",
            PkgKind::Cask => "--cask",
        };
        let output = self.run(&["deps".into(), flag.into(), token.into()])?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        Ok(parse_deps_stdout(&output.stdout))
    }
}

impl ProcessBrew {
    fn brew_dir(&self, flag: &str) -> Option<PathBuf> {
        let output = self.run(&[flag.into()]).ok()?;
        if !output.status.success() {
            return None;
        }
        let dir = String::from_utf8_lossy(&output.stdout);
        let dir = dir.trim();
        if dir.is_empty() {
            None
        } else {
            Some(PathBuf::from(dir))
        }
    }
}

impl Brew for MockBrew {
    fn brew_bin(&self) -> &Path {
        Path::new("brew")
    }

    fn run(&self, args: &[String]) -> Result<Output, Error> {
        self.record(args);
        Ok(Output {
            status: std::process::ExitStatus::from_raw(exit_status_raw(self.next_status)),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    fn installed_core(&self) -> Result<Vec<InstalledPkg>, Error> {
        Ok(self.installed.clone())
    }

    fn tap_new_soaked(&self) -> Result<(), Error> {
        let _ = self.run(&[
            "tap-new".into(),
            "brewsoakr/soaked".into(),
            "--no-git".into(),
        ])?;
        Ok(())
    }

    fn deps(&self, _kind: PkgKind, token: &str) -> Result<Vec<String>, Error> {
        Ok(self.deps.get(token).cloned().unwrap_or_default())
    }
}

/// Replaces the current process with `bin`. Returns only if `exec` fails.
pub fn passthrough_exec(bin: &Path, args: &[String]) -> Error {
    let err = Command::new(bin).args(args).exec();
    if err.kind() == ErrorKind::NotFound {
        Error::Usage("brew not found".into())
    } else {
        Error::from(err)
    }
}

fn exit_status_raw(code: i32) -> i32 {
    if code == 0 { 0 } else { code << 8 }
}

fn brew_fail(output: &Output) -> Error {
    let status = output.status.code().unwrap_or(1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let message = if stderr.is_empty() {
        format!("brew exited {status}")
    } else {
        stderr.to_string()
    };
    Error::Brew { status, message }
}

fn tap_already_exists(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stderr).contains("already exists")
}

fn parse_deps_stdout(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn read_formula_receipt(cellar: &Path, name: &str) -> Option<String> {
    let pkg_dir = cellar.join(name);
    let entries = std::fs::read_dir(pkg_dir).ok()?;
    for entry in entries.flatten() {
        let rb = entry.path().join(".brew").join(format!("{name}.rb"));
        if let Ok(text) = std::fs::read_to_string(rb) {
            return Some(text);
        }
    }
    None
}

fn find_named_rb(dir: &Path, name: &str) -> Option<String> {
    let target = format!("{name}.rb");
    walk_for_file(dir, &target)
}

fn walk_for_file(dir: &Path, target: &str) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(text) = walk_for_file(&path, target) {
                return Some(text);
            }
        } else if path.file_name().and_then(|s| s.to_str()) == Some(target)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            return Some(text);
        }
    }
    None
}

fn parse_installed_json(
    v: &str,
    read_receipt: impl Fn(&str, PkgKind) -> Option<String>,
) -> Result<Vec<InstalledPkg>, Error> {
    let mut out = Vec::new();
    for obj in json_objects_in_array(v, "formulae")? {
        let Some(name) = json_string_value(obj, "name").filter(|n| !n.is_empty()) else {
            continue;
        };
        if !keep_tap(json_string_value(obj, "tap").as_deref()) {
            continue;
        }
        if let Some(receipt_rb) = read_receipt(&name, PkgKind::Formula) {
            out.push(InstalledPkg {
                name,
                kind: PkgKind::Formula,
                receipt_rb,
            });
        }
    }
    for obj in json_objects_in_array(v, "casks")? {
        let Some(name) = json_string_value(obj, "token").filter(|n| !n.is_empty()) else {
            continue;
        };
        if !keep_tap(json_string_value(obj, "tap").as_deref()) {
            continue;
        }
        if let Some(receipt_rb) = read_receipt(&name, PkgKind::Cask) {
            out.push(InstalledPkg {
                name,
                kind: PkgKind::Cask,
                receipt_rb,
            });
        }
    }
    Ok(out)
}

fn keep_tap(tap: Option<&str>) -> bool {
    matches!(
        tap,
        None | Some("") | Some("homebrew/core") | Some("homebrew/cask")
    )
}

fn json_objects_in_array<'a>(json: &'a str, key: &str) -> Result<Vec<&'a str>, Error> {
    let Some(after_key) = find_json_key(json, key) else {
        return Ok(Vec::new());
    };
    let after = after_key.trim_start();
    if after.starts_with("null") {
        return Ok(Vec::new());
    }
    let Some(rest) = after.strip_prefix('[') else {
        return Err(Error::Other(format!("json {key} is not an array")));
    };
    scan_array_objects(rest).ok_or_else(|| Error::Other(format!("json {key} array is malformed")))
}

fn find_json_key<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let mut search = json;
    loop {
        let (_, after) = search.split_once(&pat)?;
        let after = after.trim_start();
        if let Some(rest) = after.strip_prefix(':') {
            return Some(rest);
        }
        search = after;
    }
}

fn scan_array_objects(s: &str) -> Option<Vec<&str>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        i = skip_ws(b, i);
        if i >= b.len() {
            return None;
        }
        match b[i] {
            b']' => return Some(out),
            b',' => i += 1,
            b'{' => {
                let start = i;
                i = skip_delimited(b, i, b'{', b'}')?;
                out.push(s.get(start..i)?);
            }
            _ => return None,
        }
    }
    None
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn skip_delimited(b: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0;
    let mut i = start;
    let mut in_str = false;
    let mut esc = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn json_string_value(obj: &str, key: &str) -> Option<String> {
    let after = find_json_key(obj, key)?;
    let after = after.trim_start();
    if after.starts_with("null") {
        return None;
    }
    parse_json_string(after)
}

fn parse_json_string(s: &str) -> Option<String> {
    let s = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = s.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() != 4 {
                        return None;
                    }
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code)?);
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_with(installed: Vec<InstalledPkg>, deps: BTreeMap<String, Vec<String>>) -> MockBrew {
        MockBrew {
            installed,
            deps,
            runs: Mutex::new(Vec::new()),
            next_status: 0,
        }
    }

    #[test]
    fn mock_deps_returns_configured_names() {
        let mut deps = BTreeMap::new();
        deps.insert("wget".into(), vec!["libidn2".into(), "openssl@3".into()]);
        let brew = mock_with(Vec::new(), deps);
        let got = brew.deps(PkgKind::Formula, "wget").expect("mock deps");
        assert_eq!(got, vec!["libidn2", "openssl@3"]);
    }

    #[test]
    fn mock_installed_core_returns_the_vec() {
        let installed = vec![
            InstalledPkg {
                name: "wget".into(),
                kind: PkgKind::Formula,
                receipt_rb: "class Wget; end".into(),
            },
            InstalledPkg {
                name: "firefox".into(),
                kind: PkgKind::Cask,
                receipt_rb: "cask \"firefox\"".into(),
            },
        ];
        let brew = mock_with(installed.clone(), BTreeMap::new());
        let got = brew.installed_core().expect("mock installed");
        assert_eq!(got, installed);
    }

    #[test]
    fn mock_run_records_args_and_status() {
        let brew = MockBrew {
            next_status: 3,
            ..MockBrew::new()
        };
        let output = brew
            .run(&["install".into(), "wget".into()])
            .expect("mock run");
        assert_eq!(output.status.code(), Some(3));
        let runs = brew.runs.lock().expect("runs");
        assert_eq!(
            runs.as_slice(),
            [vec!["install".to_string(), "wget".into()]]
        );
    }

    #[test]
    fn parse_installed_json_keeps_core_and_cask_drops_third_party() {
        let json = r#"{
          "formulae": [
            {
              "name": "wget",
              "tap": "homebrew/core",
              "installed": [{"version": "1.21.4"}]
            },
            {
              "name": "foo",
              "full_name": "acme/tools/foo",
              "tap": "acme/tools",
              "installed": [{"version": "0.1.0"}]
            }
          ],
          "casks": [
            {
              "token": "firefox",
              "tap": "homebrew/cask",
              "installed": "128.0"
            }
          ]
        }"#;
        let got = parse_installed_json(json, |name, kind| match (name, kind) {
            ("wget", PkgKind::Formula) => Some("class Wget; end".into()),
            ("firefox", PkgKind::Cask) => Some("cask \"firefox\"".into()),
            ("foo", _) => panic!("third-party tap should be dropped before receipt read"),
            (other, _) => panic!("unexpected receipt read for {other}"),
        })
        .expect("parse fixture");
        assert_eq!(
            got,
            vec![
                InstalledPkg {
                    name: "wget".into(),
                    kind: PkgKind::Formula,
                    receipt_rb: "class Wget; end".into(),
                },
                InstalledPkg {
                    name: "firefox".into(),
                    kind: PkgKind::Cask,
                    receipt_rb: "cask \"firefox\"".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_installed_json_keeps_empty_tap_as_api_mode_core() {
        let json = r#"{
          "formulae": [{"name": "ca-certificates", "tap": ""}],
          "casks": []
        }"#;
        let got = parse_installed_json(json, |name, kind| {
            assert_eq!(name, "ca-certificates");
            assert_eq!(kind, PkgKind::Formula);
            Some("class CaCertificates; end".into())
        })
        .expect("parse empty tap");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "ca-certificates");
        assert_eq!(got[0].kind, PkgKind::Formula);
    }
}
