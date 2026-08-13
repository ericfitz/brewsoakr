use crate::Error;
use crate::resolve::PkgKind;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPkg {
    pub name: String,
    pub kind: PkgKind,
    pub receipt_rb: String,
    pub pinned: bool,
}

pub trait Brew {
    fn brew_bin(&self) -> &Path;
    /// Capturing run for JSON, deps, `--repository`, tap-new, and trust.
    fn run(&self, args: &[String]) -> Result<Output, Error>;
    /// Live stdout/stderr for install/upgrade/reinstall and brew passthrough.
    fn run_visible(&self, args: &[String]) -> Result<Output, Error>;
    fn installed_core(&self) -> Result<Vec<InstalledPkg>, Error>;
    fn tap_new_soaked(&self) -> Result<(), Error>;
    fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error>;
}

pub struct ProcessBrew {
    pub bin: PathBuf,
    skip_tap_trust: Cell<bool>,
}

impl ProcessBrew {
    pub fn new(bin: PathBuf) -> Self {
        Self {
            bin,
            skip_tap_trust: Cell::new(false),
        }
    }
}

pub struct MockBrew {
    pub installed: Vec<InstalledPkg>,
    pub deps: BTreeMap<String, Vec<String>>,
    pub runs: Mutex<Vec<Vec<String>>>,
    pub visible_runs: Mutex<Vec<Vec<String>>>,
    pub next_status: i32,
    pub next_stdout: Vec<u8>,
    pub next_stderr: Vec<u8>,
}

impl Default for MockBrew {
    fn default() -> Self {
        Self {
            installed: Vec::new(),
            deps: BTreeMap::new(),
            runs: Mutex::new(Vec::new()),
            visible_runs: Mutex::new(Vec::new()),
            next_status: 0,
            next_stdout: Vec::new(),
            next_stderr: Vec::new(),
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

    fn record_visible(&self, args: &[String]) {
        self.visible_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(args.to_vec());
    }

    fn mock_output(&self) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(exit_status_raw(self.next_status)),
            stdout: self.next_stdout.clone(),
            stderr: self.next_stderr.clone(),
        }
    }
}

impl Brew for ProcessBrew {
    fn brew_bin(&self) -> &Path {
        &self.bin
    }

    fn run(&self, args: &[String]) -> Result<Output, Error> {
        self.spawn_brew(args, false)
    }

    fn run_visible(&self, args: &[String]) -> Result<Output, Error> {
        self.spawn_brew(args, true)
    }

    fn installed_core(&self) -> Result<Vec<InstalledPkg>, Error> {
        let output = self.run(&["info".into(), "--json=v2".into(), "--installed".into()])?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        let json = String::from_utf8_lossy(&output.stdout);
        let cellar = self.brew_dir("--cellar");
        let caskroom = self.brew_dir("--caskroom");
        parse_installed_json(&json, |name, kind, version| match kind {
            PkgKind::Formula => cellar
                .as_deref()
                .and_then(|dir| read_formula_receipt(dir, name, version)),
            PkgKind::Cask => caskroom
                .as_deref()
                .and_then(|dir| read_cask_receipt(dir, name, version)),
        })
    }

    fn tap_new_soaked(&self) -> Result<(), Error> {
        let output = self.run(&[
            "tap-new".into(),
            "brewsoakr/soaked".into(),
            "--no-git".into(),
        ])?;
        if !(output.status.success() || tap_already_exists(&output)) {
            return Err(brew_fail(&output));
        }
        self.trust_soaked_tap()
    }

    fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error> {
        let flag = match kind {
            PkgKind::Formula => "--formula",
            PkgKind::Cask => "--cask",
        };
        let output = self.run(&["deps".into(), "--1".into(), flag.into(), token.into()])?;
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

    fn spawn_brew(&self, args: &[String], visible: bool) -> Result<Output, Error> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1");
        if self.skip_tap_trust.get() {
            cmd.env("HOMEBREW_NO_REQUIRE_TAP_TRUST", "1");
        }
        if !visible {
            return match cmd.output() {
                Ok(output) => Ok(output),
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    Err(Error::Usage("brew not found".into()))
                }
                Err(e) => Err(Error::from(e)),
            };
        }
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(Error::Usage("brew not found".into()));
            }
            Err(e) => return Err(Error::from(e)),
        };
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let out_thread = std::thread::spawn(move || match stdout_pipe.as_mut() {
            Some(pipe) => copy_and_forward(pipe, &mut std::io::stdout()),
            None => Vec::new(),
        });
        let err_thread = std::thread::spawn(move || match stderr_pipe.as_mut() {
            Some(pipe) => copy_and_forward(pipe, &mut std::io::stderr()),
            None => Vec::new(),
        });
        let status = child.wait()?;
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    fn trust_soaked_tap(&self) -> Result<(), Error> {
        let output = self.run(&["trust".into(), "brewsoakr/soaked".into()])?;
        if output.status.success() {
            return Ok(());
        }
        if trust_command_unavailable(&output) {
            // Homebrew < 6 has no `brew trust`. Disable the Homebrew 6 check
            // for later brew deps/install of brewsoakr/soaked/*.
            self.skip_tap_trust.set(true);
            return Ok(());
        }
        Err(brew_fail(&output))
    }
}

fn copy_and_forward(src: &mut impl Read, dst: &mut impl Write) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match src.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &tmp[..n];
                buf.extend_from_slice(chunk);
                let _ = dst.write_all(chunk);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let _ = dst.flush();
    buf
}

fn trust_command_unavailable(output: &Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    text.contains("unknown command")
        || text.contains("unknown subcommand")
        || text.contains("invalid command")
}

impl Brew for MockBrew {
    fn brew_bin(&self) -> &Path {
        Path::new("brew")
    }

    fn run(&self, args: &[String]) -> Result<Output, Error> {
        self.record(args);
        Ok(self.mock_output())
    }

    fn run_visible(&self, args: &[String]) -> Result<Output, Error> {
        self.record(args);
        self.record_visible(args);
        Ok(self.mock_output())
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
        let _ = self.run(&["trust".into(), "brewsoakr/soaked".into()])?;
        Ok(())
    }

    fn deps(&self, _kind: PkgKind, token: &str) -> Result<Vec<String>, Error> {
        let stem = Path::new(token)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(token);
        Ok(self
            .deps
            .get(token)
            .or_else(|| self.deps.get(stem))
            .cloned()
            .unwrap_or_default())
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

/// `exec` brew; returns an exit code only if replacement fails.
pub fn exec(bin: &Path, args: &[String]) -> i32 {
    let err = passthrough_exec(bin, args);
    eprintln!("brewsoakr: {err}");
    err.exit_code()
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

fn read_formula_receipt(cellar: &Path, name: &str, version: Option<&str>) -> Option<String> {
    let version = version?;
    let rb = cellar
        .join(name)
        .join(version)
        .join(".brew")
        .join(format!("{name}.rb"));
    std::fs::read_to_string(rb).ok()
}

fn read_cask_receipt(caskroom: &Path, name: &str, version: Option<&str>) -> Option<String> {
    let metadata = caskroom.join(name).join(".metadata");
    let target = format!("{name}.rb");
    if let Some(version) = version {
        let versioned = metadata.join(version);
        if let Some(text) = walk_metadata_for_file(&versioned, &target) {
            return Some(text);
        }
    }
    walk_metadata_for_file(&metadata, &target)
}

/// Walk only `dir` (a `.metadata` tree). Unreadable subdirs are skipped.
fn walk_metadata_for_file(dir: &Path, target: &str) -> Option<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir {
            if let Some(text) = walk_metadata_for_file(&path, target) {
                return Some(text);
            }
            continue;
        }
        if path.file_name().and_then(|s| s.to_str()) == Some(target)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            return Some(text);
        }
    }
    None
}

fn parse_installed_json(
    v: &str,
    mut read_receipt: impl FnMut(&str, PkgKind, Option<&str>) -> Option<String>,
) -> Result<Vec<InstalledPkg>, Error> {
    let mut out = Vec::new();
    for obj in json_objects_in_array(v, "formulae")? {
        let Some(name) = json_string_value(obj, "name").filter(|n| !n.is_empty()) else {
            continue;
        };
        if !keep_tap(json_string_value(obj, "tap").as_deref()) {
            continue;
        }
        let version = installed_version(obj);
        if let Some(receipt_rb) = read_receipt(&name, PkgKind::Formula, version.as_deref()) {
            out.push(InstalledPkg {
                name,
                kind: PkgKind::Formula,
                receipt_rb,
                pinned: json_bool_value(obj, "pinned").unwrap_or(false),
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
        let version = installed_version(obj);
        if let Some(receipt_rb) = read_receipt(&name, PkgKind::Cask, version.as_deref()) {
            out.push(InstalledPkg {
                name,
                kind: PkgKind::Cask,
                receipt_rb,
                pinned: json_bool_value(obj, "pinned").unwrap_or(false),
            });
        }
    }
    Ok(out)
}

/// Formula: `installed[0].version`. Cask: string `installed`.
fn installed_version(obj: &str) -> Option<String> {
    let after = find_json_key(obj, "installed")?;
    let after = after.trim_start();
    if after.starts_with("null") {
        return None;
    }
    if after.starts_with('"') {
        return parse_json_string(after).filter(|s| !s.is_empty());
    }
    let rest = after.strip_prefix('[')?;
    let first = scan_array_objects(rest)?.into_iter().next()?;
    json_string_value(first, "version").filter(|s| !s.is_empty())
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

fn json_bool_value(obj: &str, key: &str) -> Option<bool> {
    let after = find_json_key(obj, key)?;
    let after = after.trim_start();
    if after.starts_with("true") {
        Some(true)
    } else if after.starts_with("false") {
        Some(false)
    } else {
        None
    }
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
            ..MockBrew::new()
        }
    }

    fn pkg(name: &str, kind: PkgKind, receipt_rb: &str) -> InstalledPkg {
        InstalledPkg {
            name: name.into(),
            kind,
            receipt_rb: receipt_rb.into(),
            pinned: false,
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
            pkg("wget", PkgKind::Formula, "class Wget; end"),
            pkg("firefox", PkgKind::Cask, "cask \"firefox\""),
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
        let got = parse_installed_json(json, |name, kind, _version| match (name, kind) {
            ("wget", PkgKind::Formula) => Some("class Wget; end".into()),
            ("firefox", PkgKind::Cask) => Some("cask \"firefox\"".into()),
            ("foo", _) => panic!("third-party tap should be dropped before receipt read"),
            (other, _) => panic!("unexpected receipt read for {other}"),
        })
        .expect("parse fixture");
        assert_eq!(
            got,
            vec![
                pkg("wget", PkgKind::Formula, "class Wget; end"),
                pkg("firefox", PkgKind::Cask, "cask \"firefox\""),
            ]
        );
    }

    #[test]
    fn parse_installed_json_keeps_empty_tap_as_api_mode_core() {
        let json = r#"{
          "formulae": [{"name": "ca-certificates", "tap": ""}],
          "casks": []
        }"#;
        let got = parse_installed_json(json, |name, kind, _version| {
            assert_eq!(name, "ca-certificates");
            assert_eq!(kind, PkgKind::Formula);
            Some("class CaCertificates; end".into())
        })
        .expect("parse empty tap");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "ca-certificates");
        assert_eq!(got[0].kind, PkgKind::Formula);
    }

    #[test]
    fn parse_installed_json_passes_installed_version() {
        let json = r#"{
          "formulae": [
            {
              "name": "wget",
              "tap": "homebrew/core",
              "installed": [{"version": "1.21.4"}]
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
        let mut versions = Vec::new();
        parse_installed_json(json, |name, kind, version| {
            versions.push((name.to_string(), kind, version.map(str::to_string)));
            Some("rb".into())
        })
        .expect("parse");
        assert_eq!(
            versions,
            vec![
                ("wget".into(), PkgKind::Formula, Some("1.21.4".into())),
                ("firefox".into(), PkgKind::Cask, Some("128.0".into())),
            ]
        );
    }

    #[test]
    fn read_formula_receipt_uses_installed_version_keg() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cellar = tmp.path();
        let old = cellar.join("wget").join("1.20.0").join(".brew");
        let new = cellar.join("wget").join("1.21.4").join(".brew");
        std::fs::create_dir_all(&old).expect("old keg");
        std::fs::create_dir_all(&new).expect("new keg");
        std::fs::write(old.join("wget.rb"), "old-receipt").expect("old rb");
        std::fs::write(new.join("wget.rb"), "new-receipt").expect("new rb");
        assert_eq!(
            read_formula_receipt(cellar, "wget", Some("1.21.4")).as_deref(),
            Some("new-receipt")
        );
        assert_eq!(
            read_formula_receipt(cellar, "wget", Some("9.9.9")).as_deref(),
            None
        );
        assert_eq!(read_formula_receipt(cellar, "wget", None).as_deref(), None);
    }

    #[test]
    fn parse_installed_json_reads_versioned_formula_keg() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cellar = tmp.path().to_path_buf();
        let old = cellar.join("wget").join("1.20.0").join(".brew");
        let new = cellar.join("wget").join("1.21.4").join(".brew");
        std::fs::create_dir_all(&old).expect("old keg");
        std::fs::create_dir_all(&new).expect("new keg");
        std::fs::write(old.join("wget.rb"), "old-receipt").expect("old rb");
        std::fs::write(new.join("wget.rb"), "new-receipt").expect("new rb");
        let json = r#"{
          "formulae": [
            {
              "name": "wget",
              "tap": "homebrew/core",
              "installed": [{"version": "1.21.4"}]
            }
          ],
          "casks": []
        }"#;
        let got = parse_installed_json(json, |name, kind, version| {
            assert_eq!(kind, PkgKind::Formula);
            read_formula_receipt(&cellar, name, version)
        })
        .expect("parse");
        assert_eq!(got, vec![pkg("wget", PkgKind::Formula, "new-receipt")]);
    }

    #[test]
    fn parse_installed_json_reads_pinned() {
        let json = r#"{
          "formulae": [
            {
              "name": "wget",
              "tap": "homebrew/core",
              "pinned": true,
              "installed": [{"version": "1.21.4"}]
            },
            {
              "name": "curl",
              "tap": "homebrew/core",
              "pinned": false,
              "installed": [{"version": "8.0.0"}]
            }
          ],
          "casks": []
        }"#;
        let got = parse_installed_json(json, |name, _kind, _version| Some(name.to_string()))
            .expect("parse");
        assert!(got.iter().find(|p| p.name == "wget").expect("wget").pinned);
        assert!(!got.iter().find(|p| p.name == "curl").expect("curl").pinned);
    }

    #[test]
    fn mock_run_visible_records_separately() {
        let brew = MockBrew::new();
        brew.run_visible(&["install".into(), "wget".into()])
            .expect("visible");
        brew.run(&["deps".into(), "wget".into()]).expect("capture");
        let visible = brew.visible_runs.lock().expect("visible_runs");
        assert_eq!(
            visible.as_slice(),
            [vec!["install".to_string(), "wget".into()]]
        );
        let runs = brew.runs.lock().expect("runs");
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn process_brew_run_captures_stdout() {
        let brew = ProcessBrew::new(PathBuf::from("/bin/echo"));
        let output = brew.run(&["hello-capture".into()]).expect("run");
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("hello-capture"),
            "{:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn process_brew_run_visible_forwards_and_returns_status() {
        let brew = ProcessBrew::new(PathBuf::from("/bin/echo"));
        let output = brew
            .run_visible(&["hello-visible".into()])
            .expect("run_visible");
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("hello-visible"),
            "{:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn mock_tap_new_soaked_records_trust() {
        let brew = MockBrew::new();
        brew.tap_new_soaked().expect("tap");
        let runs = brew.runs.lock().expect("runs");
        assert!(
            runs.iter()
                .any(|args| args == &["trust".to_string(), "brewsoakr/soaked".into()]),
            "expected brew trust brewsoakr/soaked: {runs:?}"
        );
    }

    #[test]
    fn read_cask_receipt_uses_metadata_not_app_bundle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let caskroom = tmp.path();
        let app = caskroom
            .join("firefox")
            .join("128.0")
            .join("Firefox.app")
            .join("Contents");
        std::fs::create_dir_all(&app).expect("app bundle");
        std::fs::write(app.join("firefox.rb"), "decoy-from-app").expect("decoy");
        assert_eq!(
            read_cask_receipt(caskroom, "firefox", Some("128.0")).as_deref(),
            None
        );
        let meta = caskroom
            .join("firefox")
            .join(".metadata")
            .join("128.0")
            .join("20240101000000.000")
            .join("Casks");
        std::fs::create_dir_all(&meta).expect("metadata");
        std::fs::write(meta.join("firefox.rb"), "cask \"firefox\"").expect("receipt");
        assert_eq!(
            read_cask_receipt(caskroom, "firefox", Some("128.0")).as_deref(),
            Some("cask \"firefox\"")
        );
    }

    #[test]
    fn read_cask_receipt_skips_unreadable_metadata_subdir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let caskroom = tmp.path();
        let metadata = caskroom.join("firefox").join(".metadata");
        let blocked = metadata.join("blocked");
        let good = metadata.join("128.0").join("ts").join("Casks");
        std::fs::create_dir_all(&blocked).expect("blocked");
        std::fs::create_dir_all(&good).expect("good");
        std::fs::write(good.join("firefox.rb"), "from-metadata").expect("receipt");
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 0");
        let got = read_cask_receipt(caskroom, "firefox", None);
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755))
            .expect("chmod restore");
        assert_eq!(got.as_deref(), Some("from-metadata"));
    }
}
