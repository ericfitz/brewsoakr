use crate::Error;
use crate::identity::{HostPlatform, VersionSource};
use crate::origin::OwnTap;
use crate::quiet;
use crate::resolve::PkgKind;
use crate::taps::TapInfo;
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, ErrorKind, Read, Write};
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
    /// Receipt tap from `brew info`; `None` for null/empty/brewsoak's own taps.
    pub tap: Option<String>,
    /// Which of brewsoak's own taps the receipt names, if either.
    pub own_tap: Option<OwnTap>,
    /// `source.path` of a formula keg whose receipt tap is null: where the
    /// formula file was installed from. brewsoak's own staging paths name
    /// the tap the keg was staged from.
    pub staged_path: Option<String>,
}

/// What a whole brewsoak session accumulated across every visible brew run:
/// the caveats worth showing, the byte delta, and where the raw log went.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionReport {
    pub caveats: Vec<String>,
    pub added_bytes: u64,
    pub freed_bytes: u64,
    pub log_path: Option<PathBuf>,
}

/// Two version strings to order: `(kind, installed, other)`.
pub type VersionPair = (PkgKind, String, String);

/// Runs under `brew ruby -e`: ARGV holds `kind installed other` triples, and
/// each prints one `<=>` result. The strings arrive as arguments, never as
/// Ruby source. Formulae use `PkgVersion` so a `_N` revision orders.
const COMPARE_VERSIONS_RUBY: &str = r#"ARGV.each_slice(3) do |kind, a, b|
  puts(kind == "cask" ? Version.new(a) <=> Version.new(b) : PkgVersion.parse(a) <=> PkgVersion.parse(b))
end"#;

/// Runs under `brew ruby -e`: ARGV holds `url tag` pairs (an empty tag means
/// none), and each prints brew's `Version.detect` of it, empty when brew
/// cannot tell. The strings arrive as arguments, never as Ruby source.
const DETECT_VERSIONS_RUBY: &str = r#"ARGV.each_slice(2) { |url, tag| puts Version.detect(url, **(tag.empty? ? {} : { tag: tag })).to_s }"#;

/// Runs under `brew ruby -e`: prints every `<codename> <version>` pair brew
/// knows, then `HOST <codename> <arch>` for this machine and `LANG` with the
/// languages a cask load prefers (`Cask::Config#languages`).
const HOST_PLATFORM_RUBY: &str = r##"puts MacOSVersion::SYMBOLS.map { |k, v| "#{k} #{v}" }
puts "HOST #{MacOS.version.to_sym} #{Hardware::CPU.arch}"
puts "LANG #{Cask::Config.new.languages.join(" ")}".strip"##;

/// Runs under `brew ruby -e`: ARGV holds `kind name` pairs, and each prints
/// one tab-separated row: `OK kind name tap version source_sha256
/// installed_current`, or `ERR kind name class message` when brew's own
/// loader refuses the name. The leading `kind name` lets the parser match a
/// row to its query rather than trust its position. Names arrive as
/// arguments, never as Ruby source.
const BY_NAME_RUBY: &str = r#"ARGV.each_slice(2) do |kind, name|
  begin
    if kind == "cask"
      c = Cask::CaskLoader.load(name)
      sum = c.ruby_source_checksum
      puts ["OK", kind, name, c.tap&.name, c.version, sum && sum[:sha256], c.installed_version.to_s == c.version.to_s].join("\t")
    else
      f = Formulary.factory(name)
      puts ["OK", kind, name, f.tap&.name, f.pkg_version, f.ruby_source_checksum&.hexdigest, f.latest_version_installed?].join("\t")
    end
  rescue StandardError, ScriptError => e
    puts ["ERR", kind, name, e.class, e.message.lines.first.to_s.strip].join("\t")
  end
end"#;

/// One question for `Brew::by_name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameQuery {
    pub kind: PkgKind,
    /// Bare name or cask token: what brewsoak would hand to `brew install`.
    pub name: String,
    /// Lowercase `user/repo` brewsoak expects the name to resolve to.
    pub origin: String,
    /// SHA-256 of the raw cutoff blob; `None` when there is no cutoff.
    pub cutoff_sha256: Option<String>,
}

/// What brew's own data says a bare name is right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByName {
    /// Lowercase `user/repo` the name resolves to.
    pub tap: String,
    /// Formula: `pkg_version` (`1.5.2_3`). Cask: `version`.
    pub version: String,
    /// brew's `ruby_source_checksum` equals the query's `cutoff_sha256`.
    pub same_source: bool,
    /// brew's `latest_version_installed?` (cask: installed at `version`).
    pub installed_current: bool,
}

impl ByName {
    /// A by-name install would load exactly the soaked cutoff of `origin`:
    /// `same_source`, and `tap` is `origin` (case-insensitive).
    pub fn installs_cutoff(&self, origin: &str) -> bool {
        self.same_source && self.tap.eq_ignore_ascii_case(origin)
    }
}

/// One parsed `BY_NAME_RUBY` row, before it is judged against a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Loaded {
    tap: String,
    version: String,
    source_sha256: Option<String>,
    installed_current: bool,
}

/// One name's `BY_NAME_RUBY` answer, or brew's reason it has none.
type LoadedAnswer = Result<Loaded, String>;

/// One answer per `asked` `(kind, name)` from `BY_NAME_RUBY` output. The
/// `asked` entries must be unique: a row is consumed by the first entry that
/// asks for it, so a repeated entry gets "no answer". Rows are matched by `kind name`, never by position: a missing or duplicated
/// row is that name's `Err`, and lines that are not a row for an asked name
/// (a stray brew message, a row for another name) are never assigned.
pub(crate) fn parse_by_name_lines(
    stdout: &str,
    asked: &[(PkgKind, String)],
) -> Vec<Result<Loaded, String>> {
    let mut rows: HashMap<(&str, &str), Vec<Result<Loaded, String>>> = HashMap::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 3 && matches!(fields[0], "OK" | "ERR") {
            rows.entry((fields[1], fields[2]))
                .or_default()
                .push(parse_by_name_row(&fields));
        }
    }
    asked
        .iter()
        .map(
            |(kind, name)| match rows.remove(&(kind_tag(*kind), name.as_str())) {
                None => Err("brew ruby gave no answer for this name".to_string()),
                Some(mut found) if found.len() == 1 => found.remove(0),
                Some(_) => Err("brew ruby answered this name more than once".to_string()),
            },
        )
        .collect()
}

fn parse_by_name_row(fields: &[&str]) -> Result<Loaded, String> {
    match fields {
        ["OK", _, _, tap, version, sum, installed] => {
            let tap = tap.trim().to_lowercase();
            if tap.is_empty() {
                return Err("brew reports no tap for this name".to_string());
            }
            let installed_current = match installed.trim() {
                "true" => true,
                "false" => false,
                other => return Err(format!("unparseable installed flag from brew: {other:?}")),
            };
            Ok(Loaded {
                tap,
                version: version.trim().to_string(),
                source_sha256: Some(sum.trim().to_lowercase()).filter(|v| !v.is_empty()),
                installed_current,
            })
        }
        ["ERR", _, _, class, message] => {
            let message = message.trim();
            Err(if message.is_empty() {
                class.trim().to_string()
            } else {
                message.to_string()
            })
        }
        _ => Err(format!(
            "unparseable brew loader row: {}",
            fields.join("\t").trim()
        )),
    }
}

/// Compare one loaded answer with what the query expects.
pub(crate) fn judge(loaded: &Loaded, q: &NameQuery) -> ByName {
    let same_source = match (&loaded.source_sha256, &q.cutoff_sha256) {
        (Some(have), Some(want)) => have.eq_ignore_ascii_case(want),
        _ => false,
    };
    ByName {
        tap: loaded.tap.clone(),
        version: loaded.version.clone(),
        same_source,
        installed_current: loaded.installed_current,
    }
}

pub trait Brew {
    fn brew_bin(&self) -> &Path;
    /// Capturing run for JSON (`info`, `outdated`, `tap-info`), `deps`, `--cellar` and `--caskroom`.
    fn run(&self, args: &[String]) -> Result<Output, Error>;
    /// Live stdout/stderr for install/upgrade/reinstall and brew passthrough.
    /// The summarized lines go to `terminal`: callers pass the writer they
    /// write their own lines to, so the two streams cannot split or reorder.
    fn run_visible_in(
        &self,
        args: &[String],
        terminal: &mut dyn Write,
        ctx: &quiet::RunContext,
    ) -> Result<Output, Error>;
    /// `run_visible_in` for a soaked run (the default `RunContext`).
    fn run_visible(&self, args: &[String], terminal: &mut dyn Write) -> Result<Output, Error> {
        self.run_visible_in(args, terminal, &quiet::RunContext::default())
    }
    fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error>;
    fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error>;
    /// One answer per query, in order. `Err(text)` is brew's own failure for
    /// that name (unknown name, untrusted tap, a script brewsoak could not
    /// read). The outer `Err` is only "brew not found".
    fn by_name(&self, queries: &[NameQuery]) -> Result<Vec<Result<ByName, String>>, Error>;
    /// Every installed tap, as `brew tap-info --json --installed` reports it.
    fn tap_info(&self) -> Result<Vec<TapInfo>, Error>;
    /// Names brew itself considers outdated right now (no `brew update`),
    /// with the same greedy mode `upgrade` passes to brew.
    fn outdated_names(&self, greedy: crate::flags::Greedy) -> Result<Vec<String>, Error>;
    /// Turn the output filter off; brew output is forwarded byte for byte.
    fn set_raw(&self, _raw: bool) {}
    fn session_report(&self) -> SessionReport {
        SessionReport::default()
    }
    /// The formula `.rb` brew stored in keg `<cellar>/<name>/<version>/.brew/`,
    /// or `None` when it cannot be read. This is what the keg really is, as
    /// opposed to what its directory name suggests.
    fn keg_receipt(&self, _name: &str, _version: &str) -> Option<String> {
        None
    }
    /// Homebrew's own order of each pair's first version against its second,
    /// one result per pair, from one `brew ruby` call. `Greater` means the
    /// first is newer. brewsoak does not port Homebrew's version rules. The
    /// default reports every pair as `Less`, as if the cutoff were newer.
    fn compare_versions(&self, pairs: &[VersionPair]) -> Result<Vec<Ordering>, Error> {
        Ok(vec![Ordering::Less; pairs.len()])
    }
    /// The architecture and macOS release brew runs on, asked once per run.
    /// `None` when it cannot be read; cask platform blocks then stay opaque.
    /// The default reports no host.
    fn host_platform(&self) -> Option<HostPlatform> {
        None
    }
    /// The `explicit` languages in the installed cask `token`'s saved config:
    /// the only part `brew upgrade` carries over. `None` when it is not
    /// installed or the config cannot be read. Callers put these before the
    /// run-level `host_platform().languages`. The default reports none.
    fn cask_languages(&self, _token: &str) -> Option<Vec<String>> {
        None
    }
    /// The languages of a `--language=<list>` flag on this run, in order.
    /// Brew treats the flag as explicit config over everything saved or live.
    fn cli_languages(&self) -> Vec<String> {
        Vec::new()
    }
    /// Records the `--language` flag's languages for this run.
    fn set_cli_languages(&self, _languages: Vec<String>) {}
    /// Homebrew's `Version.detect` of each source, one entry per source, from
    /// one `brew ruby` call. `None` when brew cannot tell (or the url holds a
    /// `#{` interpolation, which is never asked). The default answers `None`
    /// for all, so the caller keeps its own guess.
    fn detect_versions(&self, sources: &[VersionSource]) -> Result<Vec<Option<String>>, Error> {
        Ok(vec![None; sources.len()])
    }
}

/// `(kind tag, a, b)` of one `compare_versions` pair.
type OrderKey = (&'static str, String, String);

pub struct ProcessBrew {
    pub bin: PathBuf,
    raw: Cell<bool>,
    /// `brew --cellar`, asked once: it never changes during a run.
    cellar: std::cell::OnceCell<Option<PathBuf>>,
    /// `brew --caskroom`, asked once.
    caskroom: std::cell::OnceCell<Option<PathBuf>>,
    /// `read_cask_languages` answers for this run, keyed by cask token.
    languages: Mutex<HashMap<String, Option<Vec<String>>>>,
    /// The `--language` flag's languages (`set_cli_languages`).
    cli: Mutex<Vec<String>>,
    /// `compare_versions` answers for this run, keyed by `(kind, a, b)`.
    orderings: Mutex<HashMap<OrderKey, Ordering>>,
    /// Pairs whose batch failed, with the failure: a broken `brew ruby` costs
    /// one spawn per run, not one per package.
    ordering_failures: Mutex<HashMap<OrderKey, (i32, String)>>,
    /// `detect_versions` answers for this run.
    detected: Mutex<HashMap<VersionSource, Option<String>>>,
    /// Sources whose batch failed, with the failure.
    detect_failures: Mutex<HashMap<VersionSource, (i32, String)>>,
    /// `host_platform`, asked once: the host never changes during a run.
    host: std::cell::OnceCell<Option<HostPlatform>>,
    /// `BY_NAME_RUBY` answers, failures included, keyed by `(kind, name)`.
    /// Cleared by `brew update`.
    loaded: Mutex<HashMap<(&'static str, String), LoadedAnswer>>,
    /// Installed packages seen by `installed_packages`.
    installed_names: Mutex<Vec<(PkgKind, String)>>,
    /// The installed names went along with an earlier `by_name` spawn.
    installed_asked: Mutex<bool>,
    /// Successful `deps` answers, keyed by `(kind, token)`. Cleared by
    /// `brew update`.
    deps_memo: Mutex<HashMap<(&'static str, String), Vec<String>>>,
    sink: Mutex<Sink>,
}

/// Shared destination for every visible brew run: the summarizing filter plus
/// the lazily created raw log under `$TMPDIR`.
#[derive(Default)]
struct Sink {
    filter: quiet::Filter,
    log: Option<std::fs::File>,
    log_path: Option<PathBuf>,
    log_failed: bool,
}

impl Sink {
    /// Open the log on first use. A log we cannot open is not fatal: brewsoak
    /// keeps going without one and stops retrying.
    fn log(&mut self) -> Option<&mut std::fs::File> {
        if self.log.is_none() && !self.log_failed {
            let path = std::env::temp_dir().join(format!("brewsoak-{}.log", std::process::id()));
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(f) => {
                    self.log = Some(f);
                    self.log_path = Some(path);
                }
                Err(_) => self.log_failed = true,
            }
        }
        self.log.as_mut()
    }
}

impl ProcessBrew {
    /// One `brew ruby` spawn for a batch; `count` answers or an error.
    fn ask_orderings(&self, args: &[String], count: usize) -> Result<Vec<Ordering>, Error> {
        let output = self.run(args)?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .map(|l| match l.trim() {
                "-1" => Some(Ordering::Less),
                "0" => Some(Ordering::Equal),
                "1" => Some(Ordering::Greater),
                _ => None,
            })
            .collect::<Option<_>>()
            .filter(|a: &Vec<Ordering>| a.len() == count)
            .ok_or_else(|| Error::Brew {
                status: 0,
                message: format!("unparseable version comparison output: {}", stdout.trim()),
            })
    }

    fn ask_versions(&self, args: &[String], count: usize) -> Result<Vec<Option<String>>, Error> {
        let output = self.run(args)?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let answers: Vec<Option<String>> = stdout
            .lines()
            .map(|l| Some(l.trim()).filter(|v| !v.is_empty()).map(String::from))
            .collect();
        if answers.len() != count {
            return Err(Error::Brew {
                status: 0,
                message: format!("unparseable version detection output: {}", stdout.trim()),
            });
        }
        Ok(answers)
    }

    pub fn new(bin: PathBuf) -> Self {
        Self {
            bin,
            raw: Cell::new(false),
            cellar: std::cell::OnceCell::new(),
            caskroom: std::cell::OnceCell::new(),
            languages: Mutex::new(HashMap::new()),
            cli: Mutex::new(Vec::new()),
            orderings: Mutex::new(HashMap::new()),
            ordering_failures: Mutex::new(HashMap::new()),
            detected: Mutex::new(HashMap::new()),
            detect_failures: Mutex::new(HashMap::new()),
            host: std::cell::OnceCell::new(),
            loaded: Mutex::new(HashMap::new()),
            installed_names: Mutex::new(Vec::new()),
            installed_asked: Mutex::new(false),
            deps_memo: Mutex::new(HashMap::new()),
            sink: Mutex::new(Sink::default()),
        }
    }
}

pub struct MockBrew {
    pub installed: Vec<InstalledPkg>,
    pub deps: BTreeMap<String, Vec<String>>,
    /// Makes `deps` for this token fail with brew's stderr text.
    pub deps_errors: BTreeMap<String, String>,
    /// Every `deps` call, in order.
    pub deps_calls: Mutex<Vec<(PkgKind, String)>>,
    /// What `by_name` reports per bare name; an unscripted name is the
    /// query's own origin tap, same source, not installed-current.
    pub by_name: BTreeMap<String, Result<ByName, String>>,
    /// Every query `by_name` was asked, in order.
    pub by_name_queries: Mutex<Vec<NameQuery>>,
    pub taps: Vec<TapInfo>,
    pub runs: Mutex<Vec<Vec<String>>>,
    pub visible_runs: Mutex<Vec<Vec<String>>>,
    /// The `RunContext` of the latest `run_visible_in` call.
    pub last_ctx: Mutex<Option<quiet::RunContext>>,
    pub next_status: i32,
    pub next_stdout: Vec<u8>,
    pub next_stderr: Vec<u8>,
    /// Queued `(status, stdout)` results popped by `run_visible` before it
    /// falls back to `next_status`/`next_stdout`.
    pub next_outputs: Mutex<VecDeque<(i32, Vec<u8>)>>,
    /// What `outdated_names` reports.
    pub outdated: Vec<String>,
    /// The greedy mode of each `outdated_names` call.
    pub outdated_modes: Mutex<Vec<crate::flags::Greedy>>,
    /// What `keg_receipt` reports, keyed by `(name, cellar version)`.
    pub kegs: BTreeMap<(String, String), String>,
    /// What `compare_versions` reports for `(installed, other)`; an
    /// unscripted pair is `Less`.
    pub orderings: BTreeMap<(String, String), Ordering>,
    /// Makes every `compare_versions` call fail with this message.
    pub compare_error: Option<String>,
    /// What `host_platform` reports; `None` means the host is unknown.
    pub host: Option<HostPlatform>,
    /// What `cask_languages` reports per installed cask token.
    pub cask_languages: BTreeMap<String, Vec<String>>,
    /// What `cli_languages` reports.
    pub cli_languages: Vec<String>,
    /// The pairs of each `compare_versions` call.
    pub compares: Mutex<Vec<Vec<VersionPair>>>,
    /// Pairs `compare_versions` has answered: like `ProcessBrew`, a pair is
    /// asked of brew once per run.
    pub compared: Mutex<std::collections::HashSet<(String, String)>>,
    /// How many `compare_versions` calls asked about a pair not yet answered
    /// (each is one `brew ruby` spawn in `ProcessBrew`).
    pub compare_spawns: Mutex<usize>,
    /// What `detect_versions` reports for a `(url, tag)`; an unscripted
    /// source is `None`.
    pub detections: BTreeMap<(String, Option<String>), String>,
    /// Makes every `detect_versions` call fail with this message.
    pub detect_error: Option<String>,
    /// The 1-based spawns (calls with a source not yet answered) of
    /// `detect_versions` that fail; the sources of a failed spawn then fail
    /// for the rest of the run, as in `ProcessBrew`.
    pub detect_fail_spawns: std::collections::BTreeSet<usize>,
    /// The sources of each `detect_versions` call.
    pub detects: Mutex<Vec<Vec<VersionSource>>>,
    /// Sources `detect_versions` has answered or failed.
    pub detect_seen: Mutex<std::collections::HashSet<VersionSource>>,
    pub detect_failed: Mutex<std::collections::HashSet<VersionSource>>,
    /// How many `detect_versions` calls asked about a source not yet answered
    /// (each is one `brew ruby` spawn in `ProcessBrew`).
    pub detect_spawns: Mutex<usize>,
}

impl Default for MockBrew {
    fn default() -> Self {
        Self {
            installed: Vec::new(),
            deps: BTreeMap::new(),
            deps_errors: BTreeMap::new(),
            deps_calls: Mutex::new(Vec::new()),
            by_name: BTreeMap::new(),
            by_name_queries: Mutex::new(Vec::new()),
            taps: Vec::new(),
            runs: Mutex::new(Vec::new()),
            visible_runs: Mutex::new(Vec::new()),
            last_ctx: Mutex::new(None),
            next_status: 0,
            next_stdout: Vec::new(),
            next_stderr: Vec::new(),
            next_outputs: Mutex::new(VecDeque::new()),
            outdated: Vec::new(),
            outdated_modes: Mutex::new(Vec::new()),
            kegs: BTreeMap::new(),
            orderings: BTreeMap::new(),
            compare_error: None,
            host: None,
            cask_languages: BTreeMap::new(),
            cli_languages: Vec::new(),
            compares: Mutex::new(Vec::new()),
            compared: Mutex::new(std::collections::HashSet::new()),
            compare_spawns: Mutex::new(0),
            detections: BTreeMap::new(),
            detect_error: None,
            detect_fail_spawns: std::collections::BTreeSet::new(),
            detects: Mutex::new(Vec::new()),
            detect_seen: Mutex::new(std::collections::HashSet::new()),
            detect_failed: Mutex::new(std::collections::HashSet::new()),
            detect_spawns: Mutex::new(0),
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
        self.spawn_brew(args, None)
    }

    fn run_visible_in(
        &self,
        args: &[String],
        terminal: &mut dyn Write,
        ctx: &quiet::RunContext,
    ) -> Result<Output, Error> {
        let output = self.spawn_brew(args, Some((terminal, ctx)));
        if args.first().map(String::as_str) == Some("update") {
            // Whatever the exit status, brew may have moved its taps.
            self.loaded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.deps_memo
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            *self
                .installed_asked
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = false;
        }
        output
    }

    fn set_raw(&self, raw: bool) {
        self.raw.set(raw);
    }

    fn session_report(&self) -> SessionReport {
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        SessionReport {
            caveats: sink.filter.caveat_report(),
            added_bytes: sink.filter.added_bytes(),
            freed_bytes: sink.filter.freed_bytes(),
            log_path: sink.log_path.clone(),
        }
    }

    fn keg_receipt(&self, name: &str, version: &str) -> Option<String> {
        let cellar = self.cellar()?;
        read_formula_receipt(&cellar, name, Some(version))
    }

    fn host_platform(&self) -> Option<HostPlatform> {
        self.host
            .get_or_init(|| {
                let args = ["ruby".into(), "-e".into(), HOST_PLATFORM_RUBY.into()];
                let output = self.run(&args).ok().filter(|o| o.status.success())?;
                HostPlatform::parse(&String::from_utf8_lossy(&output.stdout))
            })
            .clone()
    }

    fn cask_languages(&self, token: &str) -> Option<Vec<String>> {
        let caskroom = self.caskroom()?;
        let mut memo = self.languages.lock().unwrap_or_else(|e| e.into_inner());
        memo.entry(token.to_string())
            .or_insert_with(|| read_cask_languages(&caskroom, token))
            .clone()
    }

    fn cli_languages(&self) -> Vec<String> {
        self.cli.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_cli_languages(&self, languages: Vec<String>) {
        *self.cli.lock().unwrap_or_else(|e| e.into_inner()) = languages;
    }

    fn compare_versions(&self, pairs: &[VersionPair]) -> Result<Vec<Ordering>, Error> {
        let key = |(kind, a, b): &VersionPair| (kind_tag(*kind), a.clone(), b.clone());
        let mut asking: Vec<&VersionPair> = Vec::new();
        {
            let memo = self.orderings.lock().unwrap_or_else(|e| e.into_inner());
            for pair in pairs {
                if !memo.contains_key(&key(pair)) && !asking.iter().any(|q| key(q) == key(pair)) {
                    asking.push(pair);
                }
            }
        }
        {
            let failures = self
                .ordering_failures
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((status, message)) = pairs.iter().find_map(|p| failures.get(&key(p))) {
                return Err(Error::Brew {
                    status: *status,
                    message: message.clone(),
                });
            }
        }
        if !asking.is_empty() {
            let mut args = vec![
                "ruby".to_string(),
                "-e".to_string(),
                COMPARE_VERSIONS_RUBY.to_string(),
                "--".to_string(),
            ];
            for (kind, a, b) in &asking {
                args.extend([kind_tag(*kind).to_string(), a.clone(), b.clone()]);
            }
            let answers = match self.ask_orderings(&args, asking.len()) {
                Ok(answers) => answers,
                Err(e) => {
                    let (status, message) = match &e {
                        Error::Brew { status, message } => (*status, message.clone()),
                        other => (1, other.to_string()),
                    };
                    let mut failures = self
                        .ordering_failures
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    for pair in &asking {
                        failures.insert(key(pair), (status, message.clone()));
                    }
                    return Err(e);
                }
            };
            let mut memo = self.orderings.lock().unwrap_or_else(|e| e.into_inner());
            for (pair, answer) in asking.iter().zip(answers) {
                memo.insert(key(pair), answer);
            }
        }
        let memo = self.orderings.lock().unwrap_or_else(|e| e.into_inner());
        Ok(pairs.iter().map(|p| memo[&key(p)]).collect())
    }

    fn detect_versions(&self, sources: &[VersionSource]) -> Result<Vec<Option<String>>, Error> {
        let askable = |s: &VersionSource| !s.url.contains("#{");
        let mut asking: Vec<&VersionSource> = Vec::new();
        {
            let memo = self.detected.lock().unwrap_or_else(|e| e.into_inner());
            for s in sources.iter().filter(|s| askable(s)) {
                if !memo.contains_key(s) && !asking.contains(&s) {
                    asking.push(s);
                }
            }
        }
        {
            let failures = self
                .detect_failures
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((status, message)) = sources.iter().find_map(|s| failures.get(s)) {
                return Err(Error::Brew {
                    status: *status,
                    message: message.clone(),
                });
            }
        }
        if !asking.is_empty() {
            let mut args = vec![
                "ruby".to_string(),
                "-e".to_string(),
                DETECT_VERSIONS_RUBY.to_string(),
                "--".to_string(),
            ];
            for s in &asking {
                args.extend([s.url.clone(), s.tag.clone().unwrap_or_default()]);
            }
            match self.ask_versions(&args, asking.len()) {
                Ok(answers) => {
                    let mut memo = self.detected.lock().unwrap_or_else(|e| e.into_inner());
                    for (s, answer) in asking.iter().zip(answers) {
                        memo.insert((*s).clone(), answer);
                    }
                }
                Err(e) => {
                    let (status, message) = match &e {
                        Error::Brew { status, message } => (*status, message.clone()),
                        other => (1, other.to_string()),
                    };
                    let mut failures = self
                        .detect_failures
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    for s in &asking {
                        failures.insert((*s).clone(), (status, message.clone()));
                    }
                    return Err(e);
                }
            }
        }
        let memo = self.detected.lock().unwrap_or_else(|e| e.into_inner());
        Ok(sources
            .iter()
            .map(|s| memo.get(s).cloned().flatten())
            .collect())
    }

    fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
        let output = self.run(&["info".into(), "--json=v2".into(), "--installed".into()])?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        let json = String::from_utf8_lossy(&output.stdout);
        let cellar = self.cellar();
        let caskroom = self.caskroom();
        let installed = parse_installed_json(
            &json,
            |name, kind, version| match kind {
                PkgKind::Formula => cellar
                    .as_deref()
                    .and_then(|dir| read_formula_receipt(dir, name, version)),
                PkgKind::Cask => caskroom
                    .as_deref()
                    .and_then(|dir| read_cask_receipt(dir, name, version)),
            },
            |name, version| {
                cellar
                    .as_deref()
                    .map(|dir| read_formula_receipt_source(dir, name, version))
                    .unwrap_or_default()
            },
        )?;
        *self
            .installed_names
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            installed.iter().map(|p| (p.kind, p.name.clone())).collect();
        Ok(installed)
    }

    fn by_name(&self, queries: &[NameQuery]) -> Result<Vec<Result<ByName, String>>, Error> {
        let mut asking: Vec<(PkgKind, String)> = Vec::new();
        let mut with_installed = false;
        {
            let loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
            let asked = *self
                .installed_asked
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut want: Vec<(PkgKind, String)> =
                queries.iter().map(|q| (q.kind, q.name.clone())).collect();
            if !asked {
                let installed = self
                    .installed_names
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                want.extend(installed.iter().cloned());
                with_installed = !installed.is_empty();
            }
            for (kind, name) in want {
                let key = (kind_tag(kind), name.clone());
                if !loaded.contains_key(&key) && !asking.contains(&(kind, name.clone())) {
                    asking.push((kind, name));
                }
            }
        }
        if !asking.is_empty() {
            let mut args = vec![
                "ruby".to_string(),
                "-e".to_string(),
                BY_NAME_RUBY.to_string(),
                "--".to_string(),
            ];
            for (kind, name) in &asking {
                args.extend([kind_tag(*kind).to_string(), name.clone()]);
            }
            let output = self.run(&args)?;
            if output.status.success() && with_installed {
                *self
                    .installed_asked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = true;
            }
            let answers: Vec<Result<Loaded, String>> = if output.status.success() {
                parse_by_name_lines(&String::from_utf8_lossy(&output.stdout), &asking)
            } else {
                let text = match brew_fail(&output) {
                    Error::Brew { message, .. } => message,
                    other => other.to_string(),
                };
                vec![Err(text); asking.len()]
            };
            let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
            for ((kind, name), answer) in asking.into_iter().zip(answers) {
                loaded.insert((kind_tag(kind), name), answer);
            }
        }
        let loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        Ok(queries
            .iter()
            .map(|q| match loaded.get(&(kind_tag(q.kind), q.name.clone())) {
                Some(Ok(l)) => Ok(judge(l, q)),
                Some(Err(e)) => Err(e.clone()),
                None => Err("brew loader gave no answer".to_string()),
            })
            .collect())
    }

    fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
        let output = self.run(&["tap-info".into(), "--json".into(), "--installed".into()])?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        crate::taps::parse_tap_info_json(&String::from_utf8_lossy(&output.stdout))
    }

    fn outdated_names(&self, greedy: crate::flags::Greedy) -> Result<Vec<String>, Error> {
        let mut args = vec!["outdated".to_string(), "--json=v2".to_string()];
        args.extend(greedy_args(greedy));
        let output = self.run(&args)?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        parse_outdated_json(&String::from_utf8_lossy(&output.stdout))
    }

    fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error> {
        let flag = match kind {
            PkgKind::Formula => "--formula",
            PkgKind::Cask => "--cask",
        };
        let key = (kind_tag(kind), token.to_string());
        if let Some(memo) = self
            .deps_memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(memo.clone());
        }
        // `--full-name` makes brew resolve bare dep names itself (core first,
        // then a unique installed tap) and print `user/repo/name` for tap
        // packages. It is accepted for casks too, where it changes nothing.
        let output = self.run(&[
            "deps".into(),
            "--1".into(),
            "--full-name".into(),
            flag.into(),
            token.into(),
        ])?;
        if !output.status.success() {
            return Err(brew_fail(&output));
        }
        let deps = parse_deps_stdout(&output.stdout);
        self.deps_memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, deps.clone());
        Ok(deps)
    }
}

/// brew's spelling of a greedy mode for `brew outdated`/`upgrade`: both kinds
/// is `--greedy`, one kind is its long form, none is no flag.
fn greedy_args(greedy: crate::flags::Greedy) -> Vec<String> {
    match (greedy.auto_updates, greedy.latest) {
        (true, true) => vec!["--greedy".to_string()],
        (true, false) => vec!["--greedy-auto-updates".to_string()],
        (false, true) => vec!["--greedy-latest".to_string()],
        (false, false) => Vec::new(),
    }
}

/// Env for brewsoak-driven `brew` children.
/// `HOMEBREW_DEVELOPER` and the `FORBID_PACKAGES_FROM_PATHS` removal no longer
/// enable path loading (soaked installs go by bare name). `brew ruby` still
/// needs `HOMEBREW_DEVELOPER`. An `update` child must not get it: with it set,
/// `brew update` moves Homebrew to `main` instead of the latest stable tag, so
/// that child inherits the user's own value untouched.
/// `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK` stops brew from upgrading
/// dependents of a cutoff install to HEAD.
fn apply_brewsoak_brew_env(cmd: &mut Command) {
    let verb = cmd
        .get_args()
        .next()
        .map(|a| a.to_string_lossy().into_owned());
    for (key, value) in brewsoak_brew_env_pairs(verb.as_deref().unwrap_or("")) {
        match value {
            Some(v) => {
                cmd.env(key, v);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
}

/// The env changes for a `brew <verb>` child; a key not listed is inherited.
fn brewsoak_brew_env_pairs(verb: &str) -> Vec<(&'static str, Option<&'static str>)> {
    let mut pairs = vec![
        ("HOMEBREW_NO_AUTO_UPDATE", Some("1")),
        ("HOMEBREW_NO_COLOR", Some("1")),
        ("HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK", Some("1")),
        ("HOMEBREW_FORBID_PACKAGES_FROM_PATHS", None),
        // brew's loaders must read the taps as they are, not refetch the API.
        ("HOMEBREW_FORCE_API_AUTO_UPDATE", None),
    ];
    if verb != "update" {
        pairs.push(("HOMEBREW_DEVELOPER", Some("1")));
    }
    pairs
}

impl ProcessBrew {
    fn cellar(&self) -> Option<PathBuf> {
        self.cellar
            .get_or_init(|| self.brew_dir("--cellar"))
            .clone()
    }

    /// `brew --caskroom`, asked once and shared by every reader.
    fn caskroom(&self) -> Option<PathBuf> {
        self.caskroom
            .get_or_init(|| self.brew_dir("--caskroom"))
            .clone()
    }

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

    /// `terminal` is `Some` for a visible run: its summarized lines go there.
    fn spawn_brew(
        &self,
        args: &[String],
        terminal: Option<(&mut dyn Write, &quiet::RunContext)>,
    ) -> Result<Output, Error> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        apply_brewsoak_brew_env(&mut cmd);
        let Some((terminal, ctx)) = terminal else {
            return match cmd.output() {
                Ok(output) => Ok(output),
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    Err(Error::Usage("brew not found".into()))
                }
                Err(e) => Err(Error::from(e)),
            };
        };
        // One pipe for both streams: the summarizer needs brew's stdout and
        // stderr in the order brew wrote them, which two reader threads cannot
        // guarantee. Everything lands in Output.stdout; the only callers that
        // read a visible run's output already concatenate both streams.
        let (reader, writer) = std::io::pipe()?;
        cmd.stdin(Stdio::inherit())
            .stdout(writer.try_clone()?)
            .stderr(writer);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(Error::Usage("brew not found".into()));
            }
            Err(e) => return Err(Error::from(e)),
        };
        // Drop our copies of the write end, or the reader never sees EOF.
        drop(cmd);
        let raw = self.raw.get();
        let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        let captured = forward_run(reader, &mut sink, terminal, args, raw, ctx);
        drop(sink);
        let status = child.wait()?;
        Ok(Output {
            status,
            stdout: captured,
            stderr: Vec::new(),
        })
    }
}

/// Read one brew run to EOF: raw bytes to the log and to the returned capture,
/// one summarized line at a time to the terminal. Returns the raw output.
fn forward_run(
    src: impl Read,
    sink: &mut Sink,
    stdout: &mut dyn Write,
    args: &[String],
    raw: bool,
    ctx: &quiet::RunContext,
) -> Vec<u8> {
    let mut reader = std::io::BufReader::new(src);
    let mut captured = Vec::new();
    if let Some(log) = sink.log() {
        let _ = writeln!(log, "\n$ brew {}", args.join(" "));
    }
    sink.filter.start_run(ctx);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        captured.extend_from_slice(&line);
        if let Some(log) = sink.log() {
            let _ = log.write_all(&line);
        }
        let text = String::from_utf8_lossy(&line);
        if raw {
            let _ = stdout.write_all(&line);
        } else if let Some(out) = sink.filter.line(&text) {
            let _ = writeln!(stdout, "{out}");
        }
        let _ = stdout.flush();
    }
    if !raw && let Some(out) = sink.filter.finish_run() {
        let _ = writeln!(stdout, "{out}");
    }
    let _ = stdout.flush();
    captured
}

impl Brew for MockBrew {
    fn brew_bin(&self) -> &Path {
        Path::new("brew")
    }

    fn run(&self, args: &[String]) -> Result<Output, Error> {
        self.record(args);
        Ok(self.mock_output())
    }

    fn run_visible_in(
        &self,
        args: &[String],
        _terminal: &mut dyn Write,
        ctx: &quiet::RunContext,
    ) -> Result<Output, Error> {
        self.record(args);
        self.record_visible(args);
        *self.last_ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx.clone());
        let queued = self
            .next_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        Ok(match queued {
            Some((status, stdout)) => Output {
                status: std::process::ExitStatus::from_raw(exit_status_raw(status)),
                stdout,
                stderr: Vec::new(),
            },
            None => self.mock_output(),
        })
    }

    fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
        Ok(self.installed.clone())
    }

    fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
        Ok(self.taps.clone())
    }

    fn outdated_names(&self, greedy: crate::flags::Greedy) -> Result<Vec<String>, Error> {
        self.outdated_modes
            .lock()
            .expect("outdated_modes")
            .push(greedy);
        Ok(self.outdated.clone())
    }

    fn keg_receipt(&self, name: &str, version: &str) -> Option<String> {
        self.kegs
            .get(&(name.to_string(), version.to_string()))
            .cloned()
    }

    fn host_platform(&self) -> Option<HostPlatform> {
        self.host.clone()
    }

    fn cask_languages(&self, token: &str) -> Option<Vec<String>> {
        self.cask_languages.get(token).cloned()
    }

    fn cli_languages(&self) -> Vec<String> {
        self.cli_languages.clone()
    }

    fn detect_versions(&self, sources: &[VersionSource]) -> Result<Vec<Option<String>>, Error> {
        self.detects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(sources.to_vec());
        if let Some(message) = &self.detect_error {
            return Err(Error::Brew {
                status: 1,
                message: message.clone(),
            });
        }
        let askable = |s: &&VersionSource| !s.url.contains("#{");
        let failed_before = self.detect_failed.lock().unwrap_or_else(|e| e.into_inner());
        if sources.iter().any(|s| failed_before.contains(s)) {
            return Err(Error::Brew {
                status: 1,
                message: "brew ruby failed earlier in this run".into(),
            });
        }
        drop(failed_before);
        let mut seen = self.detect_seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut new: Vec<&VersionSource> = Vec::new();
        for s in sources.iter().filter(askable) {
            if !seen.contains(s) && !new.contains(&s) {
                new.push(s);
            }
        }
        if !new.is_empty() {
            let mut spawns = self.detect_spawns.lock().unwrap_or_else(|e| e.into_inner());
            *spawns += 1;
            if self.detect_fail_spawns.contains(&*spawns) {
                let mut failed = self.detect_failed.lock().unwrap_or_else(|e| e.into_inner());
                failed.extend(new.into_iter().cloned());
                return Err(Error::Brew {
                    status: 1,
                    message: format!("brew ruby failed (spawn {})", *spawns),
                });
            }
            seen.extend(new.into_iter().cloned());
        }
        Ok(sources
            .iter()
            .map(|s| {
                if s.url.contains("#{") {
                    None
                } else {
                    self.detections
                        .get(&(s.url.clone(), s.tag.clone()))
                        .cloned()
                }
            })
            .collect())
    }

    fn compare_versions(&self, pairs: &[VersionPair]) -> Result<Vec<Ordering>, Error> {
        self.compares
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pairs.to_vec());
        {
            let mut compared = self.compared.lock().unwrap_or_else(|e| e.into_inner());
            let before = compared.len();
            compared.extend(pairs.iter().map(|(_, a, b)| (a.clone(), b.clone())));
            if compared.len() > before {
                *self
                    .compare_spawns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) += 1;
            }
        }
        if let Some(message) = &self.compare_error {
            return Err(Error::Brew {
                status: 1,
                message: message.clone(),
            });
        }
        Ok(pairs
            .iter()
            .map(|(_, a, b)| {
                self.orderings
                    .get(&(a.clone(), b.clone()))
                    .copied()
                    .unwrap_or(Ordering::Less)
            })
            .collect())
    }

    fn by_name(&self, queries: &[NameQuery]) -> Result<Vec<Result<ByName, String>>, Error> {
        let mut seen = self
            .by_name_queries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        seen.extend(queries.iter().cloned());
        Ok(queries
            .iter()
            .map(|q| {
                self.by_name.get(&q.name).cloned().unwrap_or_else(|| {
                    Ok(ByName {
                        tap: q.origin.clone(),
                        version: String::new(),
                        same_source: true,
                        installed_current: false,
                    })
                })
            })
            .collect())
    }

    fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error> {
        self.deps_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((kind, token.to_string()));
        if let Some(message) = self.deps_errors.get(token) {
            return Err(Error::Brew {
                status: 1,
                message: message.clone(),
            });
        }
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

/// `exec` brew; returns an exit code only if replacement fails.
pub fn exec(bin: &Path, args: &[String]) -> i32 {
    let err = passthrough_exec(bin, args);
    eprintln!("brewsoak: {err}");
    err.exit_code()
}

fn exit_status_raw(code: i32) -> i32 {
    if code == 0 { 0 } else { code << 8 }
}

fn kind_tag(kind: PkgKind) -> &'static str {
    match kind {
        PkgKind::Formula => "formula",
        PkgKind::Cask => "cask",
    }
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

/// What a formula keg's receipt says about where it was installed from.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReceiptSource {
    /// `source.tap`; `None` for null, empty, not a string or one of brewsoak's own taps.
    pub tap: Option<String>,
    /// Which of brewsoak's own taps `source.tap` names, if either.
    pub own_tap: Option<OwnTap>,
    /// `source.path`, read only when `tap` is `None`.
    pub path: Option<String>,
}

/// The source a formula keg's own `INSTALL_RECEIPT.json` records
/// (`source.tap`), not where brew resolves the name today: a keg staged by
/// brewsoak has `null`, and `brew info` may still say `homebrew/core` for it
/// when core has a same-name formula.
fn read_formula_receipt_source(cellar: &Path, name: &str, version: Option<&str>) -> ReceiptSource {
    read_formula_receipt_source_opt(cellar, name, version).unwrap_or_default()
}

fn read_formula_receipt_source_opt(
    cellar: &Path,
    name: &str,
    version: Option<&str>,
) -> Option<ReceiptSource> {
    let text = std::fs::read_to_string(
        cellar
            .join(name)
            .join(version?)
            .join("INSTALL_RECEIPT.json"),
    )
    .ok()?;
    let source = find_json_key(&text, "source")?.trim_start();
    let end = skip_delimited(source.as_bytes(), 0, b'{', b'}')?;
    let source = source.get(..end)?;
    let raw_tap = json_string_value(source, "tap");
    let own_tap = raw_tap.as_deref().and_then(crate::origin::own_tap);
    let tap = raw_tap.filter(|t| !t.is_empty() && own_tap.is_none());
    // A staged keg has no tap; the path it was installed from may name one.
    let path = if tap.is_none() {
        json_string_value(source, "path").filter(|p| !p.is_empty())
    } else {
        None
    };
    Some(ReceiptSource { tap, path, own_tap })
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

/// The languages in `explicit` of `<caskroom>/<token>/.metadata/config.json`,
/// deduplicated, with those `Locale` cannot parse dropped. Only `explicit`
/// counts: `brew upgrade` rebuilds the config with `default_config.merge(saved)`,
/// and `Cask::Config#merge` keeps nothing but `explicit`, so `env` and
/// `default` are read live again (the run-level `host_platform().languages`).
/// `None` when the file is missing or unreadable, has no `explicit` object, or
/// its `languages` value is not an array of strings.
fn read_cask_languages(caskroom: &Path, token: &str) -> Option<Vec<String>> {
    let path = caskroom.join(token).join(".metadata").join("config.json");
    let json = std::fs::read_to_string(path).ok()?;
    let b = json.as_bytes();
    let after = find_json_key(&json, "explicit")?;
    let start = skip_ws(b, json.len() - after.len());
    if b.get(start) != Some(&b'{') {
        return None;
    }
    let obj = json.get(start..skip_delimited(b, start, b'{', b'}')?)?;
    let mut out: Vec<String> = Vec::new();
    if let Some(after) = find_json_key(obj, "languages") {
        for lang in json_string_array(after.trim_start())? {
            if crate::identity::is_valid_language(&lang) && !out.contains(&lang) {
                out.push(lang);
            }
        }
    }
    Some(out)
}

/// `["a", "b"]` at the start of `s`.
fn json_string_array(s: &str) -> Option<Vec<String>> {
    let mut rest = s.strip_prefix('[')?.trim_start();
    let mut out = Vec::new();
    loop {
        if rest.starts_with(']') {
            return Some(out);
        }
        let item = parse_json_string(rest)?;
        // The item is the quoted text plus its escapes; find where it ends.
        let mut end = 1;
        let mut esc = false;
        for (i, c) in rest.char_indices().skip(1) {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                end = i + 1;
                break;
            }
        }
        out.push(item);
        rest = rest[end..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
}

/// Homebrew >= 4 saves an installed cask as
/// `.metadata/<version>/<timestamp>/Casks/<token>.json`
/// (`Cask::Installer#save_caskfile`: `to_installed_json_hash`, which holds
/// uninstall artifacts and nothing identifying) and writes `<token>.rb` only
/// for casks with uninstall flight blocks. Without a `.rb`, the installed
/// identity is the version `brew info --json=v2 --installed` reports.
/// `identity::parse_cask` reads this as a version-only `CaskIdentity`.
pub fn version_only_cask_receipt(name: &str, version: &str) -> String {
    let version = if version == "latest" {
        ":latest".to_string()
    } else {
        format!("\"{version}\"")
    };
    format!("cask \"{name}\" do\n  version {version}\nend\n")
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

/// `formula_tap(name, version)` is the tap in that formula keg's own receipt;
/// brew info's top-level `tap` is only trusted for casks.
fn parse_installed_json(
    v: &str,
    mut read_receipt: impl FnMut(&str, PkgKind, Option<&str>) -> Option<String>,
    mut formula_source: impl FnMut(&str, Option<&str>) -> ReceiptSource,
) -> Result<Vec<InstalledPkg>, Error> {
    let mut out = Vec::new();
    for obj in json_objects_in_array(v, "formulae")? {
        let Some(name) = json_string_value(obj, "name").filter(|n| !n.is_empty()) else {
            continue;
        };
        let version = installed_version(obj);
        let source = formula_source(&name, version.as_deref());
        if let Some(receipt_rb) = read_receipt(&name, PkgKind::Formula, version.as_deref()) {
            out.push(InstalledPkg {
                name,
                kind: PkgKind::Formula,
                receipt_rb,
                pinned: json_bool_value(obj, "pinned").unwrap_or(false),
                tap: source.tap,
                own_tap: source.own_tap,
                staged_path: source.path,
            });
        }
    }
    for obj in json_objects_in_array(v, "casks")? {
        let Some(name) = json_string_value(obj, "token").filter(|n| !n.is_empty()) else {
            continue;
        };
        let raw_tap = json_string_value(obj, "tap");
        let own_tap = raw_tap.as_deref().and_then(crate::origin::own_tap);
        let tap = raw_tap.filter(|t| !t.is_empty() && own_tap.is_none());
        let version = installed_version(obj);
        let receipt_rb = read_receipt(&name, PkgKind::Cask, version.as_deref()).or_else(|| {
            version
                .as_deref()
                .map(|v| version_only_cask_receipt(&name, v))
        });
        if let Some(receipt_rb) = receipt_rb {
            out.push(InstalledPkg {
                name,
                kind: PkgKind::Cask,
                receipt_rb,
                pinned: json_bool_value(obj, "pinned").unwrap_or(false),
                tap,
                own_tap,
                staged_path: None,
            });
        }
    }
    Ok(out)
}

/// Formula: the newest keg. Homebrew sorts `installed` oldest-first, and
/// `brew upgrade` compares against the newest keg, so reading any other one
/// makes brewsoak ask brew to upgrade something brew already considers current.
/// Falls back to `linked_keg`. Cask: string `installed`.
fn installed_version(obj: &str) -> Option<String> {
    let after = find_json_key(obj, "installed")?;
    let after = after.trim_start();
    if after.starts_with('"') {
        return parse_json_string(after).filter(|s| !s.is_empty());
    }
    if let Some(rest) = after.strip_prefix('[')
        && let Some(kegs) = scan_array_objects(rest)
        && let Some(newest) = kegs.into_iter().next_back()
        && let Some(version) = json_string_value(newest, "version").filter(|s| !s.is_empty())
    {
        return Some(version);
    }
    json_string_value(obj, "linked_keg").filter(|s| !s.is_empty())
}

/// Formula and cask names from `brew outdated --json=v2`.
pub(crate) fn parse_outdated_json(json: &str) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    for key in ["formulae", "casks"] {
        for obj in json_objects_in_array(json, key)? {
            if let Some(name) = json_string_value(obj, "name").filter(|n| !n.is_empty()) {
                out.push(name);
            }
        }
    }
    Ok(out)
}

pub(crate) fn json_objects_in_array<'a>(json: &'a str, key: &str) -> Result<Vec<&'a str>, Error> {
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

/// The text after `"key":` for a key of the object `json` itself: a key of a
/// nested object or array is skipped, as is `"key":` inside a string value.
/// A text that does not start with `{` is read as the members of an object.
pub(crate) fn find_json_key<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let b = json.as_bytes();
    let top = if b.get(skip_ws(b, 0)) == Some(&b'{') {
        1
    } else {
        0
    };
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            b'"' => {
                let start = i + 1;
                i = start;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                if depth == top && json.get(start..i) == Some(key) {
                    let after = skip_ws(b, i + 1);
                    if b.get(after) == Some(&b':') {
                        return json.get(after + 1..);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

pub(crate) fn scan_array_objects(s: &str) -> Option<Vec<&str>> {
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

pub(crate) fn json_string_value(obj: &str, key: &str) -> Option<String> {
    let after = find_json_key(obj, key)?;
    let after = after.trim_start();
    if after.starts_with("null") {
        return None;
    }
    parse_json_string(after)
}

pub(crate) fn json_bool_value(obj: &str, key: &str) -> Option<bool> {
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

    #[test]
    fn mock_deps_keys_by_exact_token() {
        let mut brew = MockBrew::new();
        brew.deps
            .insert("python@3.13".to_string(), vec!["openssl@3".to_string()]);
        assert_eq!(
            brew.deps(PkgKind::Formula, "python@3.13").unwrap(),
            ["openssl@3"]
        );
        // A different spelling of the same package is a different key.
        assert!(
            brew.deps(PkgKind::Formula, "tap/x/python@3.13")
                .unwrap()
                .is_empty()
        );
        assert!(brew.deps(PkgKind::Formula, "python").unwrap().is_empty());
    }

    #[test]
    fn forward_run_flushes_a_pending_nosoak_upgrade_at_eof() {
        let mut sink = Sink::default();
        let mut terminal = Vec::new();
        let ctx = quiet::RunContext {
            mode: quiet::RunMode::NoSoak,
            ..quiet::RunContext::default()
        };
        let src = std::io::Cursor::new(b"==> Upgrading foo\n".to_vec());
        let captured = forward_run(src, &mut sink, &mut terminal, &[], false, &ctx);
        assert_eq!(captured, b"==> Upgrading foo\n");
        assert_eq!(String::from_utf8(terminal).unwrap(), "upgrading foo\n");
    }

    #[test]
    fn mock_outdated_names() {
        let brew = MockBrew {
            outdated: vec!["brewsoak".into()],
            ..MockBrew::new()
        };
        assert_eq!(
            brew.outdated_names(Default::default()).unwrap(),
            vec!["brewsoak".to_string()]
        );
    }

    #[test]
    fn process_brew_outdated_passes_the_greedy_mode_to_brew() {
        use crate::flags::Greedy;
        let tmp = tempfile::tempdir().expect("tmp");
        let log = tmp.path().join("args");
        let script = tmp.path().join("brew.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\necho '{{\"formulae\":[],\"casks\":[]}}'\n",
                log.display()
            ),
        )
        .expect("script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let brew = ProcessBrew::new(script);
        for greedy in [
            Greedy::default(),
            Greedy {
                auto_updates: true,
                latest: true,
            },
            Greedy {
                auto_updates: true,
                latest: false,
            },
            Greedy {
                auto_updates: false,
                latest: true,
            },
        ] {
            brew.outdated_names(greedy).expect("outdated");
        }
        assert_eq!(
            std::fs::read_to_string(&log).expect("log"),
            "outdated --json=v2\noutdated --json=v2 --greedy\n\
             outdated --json=v2 --greedy-auto-updates\noutdated --json=v2 --greedy-latest\n"
        );
    }

    #[test]
    fn process_brew_deps_asks_brew_for_full_names() {
        let tmp = tempfile::tempdir().expect("tmp");
        let log = tmp.path().join("args");
        let script = tmp.path().join("brew.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\necho hashicorp/tap/terraform\necho wget\n",
                log.display()
            ),
        )
        .expect("script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let brew = ProcessBrew::new(script);
        let got = brew.deps(PkgKind::Formula, "packer").expect("deps");
        assert_eq!(got, ["hashicorp/tap/terraform", "wget"]);
        brew.deps(PkgKind::Cask, "alt-tab").expect("cask deps");
        assert_eq!(
            std::fs::read_to_string(&log).expect("log"),
            "deps --1 --full-name --formula packer\ndeps --1 --full-name --cask alt-tab\n"
        );
    }

    #[test]
    fn parse_outdated_json_reads_formula_and_cask_names() {
        let json = r#"{"formulae": [{"name": "brewsoak", "installed_versions": ["1.0"], "current_version": "1.1"}], "casks": [{"name": "firefox"}]}"#;
        assert_eq!(
            parse_outdated_json(json).unwrap(),
            vec!["brewsoak".to_string(), "firefox".to_string()]
        );
    }

    fn mock_with(installed: Vec<InstalledPkg>, deps: BTreeMap<String, Vec<String>>) -> MockBrew {
        MockBrew {
            installed,
            deps,
            ..MockBrew::new()
        }
    }

    fn pkg(name: &str, kind: PkgKind, receipt_rb: &str, tap: Option<&str>) -> InstalledPkg {
        InstalledPkg {
            own_tap: None,
            name: name.into(),
            kind,
            receipt_rb: receipt_rb.into(),
            pinned: false,
            tap: tap.map(str::to_string),
            staged_path: None,
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
    fn mock_tap_info_returns_the_vec() {
        let brew = MockBrew {
            taps: vec![crate::taps::TapInfo::new("a/b", None)],
            ..MockBrew::new()
        };
        assert_eq!(brew.tap_info().unwrap().len(), 1);
    }

    #[test]
    fn mock_installed_packages_returns_the_vec() {
        let installed = vec![
            pkg("wget", PkgKind::Formula, "class Wget; end", None),
            pkg("firefox", PkgKind::Cask, "cask \"firefox\"", None),
        ];
        let brew = mock_with(installed.clone(), BTreeMap::new());
        let got = brew.installed_packages().expect("mock installed");
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
    fn parse_installed_json_reads_the_newest_keg_receipt() {
        // Homebrew sorts `installed` oldest-first; the old keg's receipt would
        // make brewsoak disagree with `brew upgrade` about what is installed.
        let json = r#"{
          "formulae": [
            {
              "name": "ca-certificates",
              "tap": "homebrew/core",
              "linked_keg": "2026-08-13",
              "installed": [{"version": "2026-07-16"}, {"version": "2026-08-13"}]
            }
          ],
          "casks": []
        }"#;
        let got = parse_installed_json(
            json,
            |name, kind, version| {
                assert_eq!((name, kind), ("ca-certificates", PkgKind::Formula));
                Some(format!("keg {}", version.expect("version")))
            },
            |_, _| core_source(),
        )
        .expect("parse fixture");
        assert_eq!(
            got,
            vec![pkg(
                "ca-certificates",
                PkgKind::Formula,
                "keg 2026-08-13",
                Some("homebrew/core")
            )]
        );
    }

    #[test]
    fn parse_installed_json_falls_back_to_linked_keg_without_installed_kegs() {
        let json = r#"{
          "formulae": [
            {
              "name": "wget",
              "tap": "homebrew/core",
              "linked_keg": "1.21.4",
              "installed": []
            }
          ],
          "casks": []
        }"#;
        let got = parse_installed_json(
            json,
            |_name, _kind, version| Some(format!("keg {}", version.expect("version"))),
            |_, _| core_source(),
        )
        .expect("parse fixture");
        assert_eq!(
            got,
            vec![pkg(
                "wget",
                PkgKind::Formula,
                "keg 1.21.4",
                Some("homebrew/core")
            )]
        );
    }

    #[test]
    fn parse_installed_json_keeps_third_party() {
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
        let got = parse_installed_json(
            json,
            |name, kind, _version| match (name, kind) {
                ("wget", PkgKind::Formula) => Some("class Wget; end".into()),
                ("firefox", PkgKind::Cask) => Some("cask \"firefox\"".into()),
                ("foo", PkgKind::Formula) => Some("class Foo; end".into()),
                (other, _) => panic!("unexpected receipt read for {other}"),
            },
            |name, _| match name {
                "wget" => core_source(),
                "foo" => ReceiptSource {
                    tap: Some("acme/tools".into()),
                    path: None,
                    own_tap: None,
                },
                other => panic!("unexpected tap read for {other}"),
            },
        )
        .expect("parse fixture");
        assert_eq!(
            got,
            vec![
                pkg(
                    "wget",
                    PkgKind::Formula,
                    "class Wget; end",
                    Some("homebrew/core")
                ),
                pkg(
                    "foo",
                    PkgKind::Formula,
                    "class Foo; end",
                    Some("acme/tools")
                ),
                pkg(
                    "firefox",
                    PkgKind::Cask,
                    "cask \"firefox\"",
                    Some("homebrew/cask")
                ),
            ]
        );
    }

    #[test]
    fn parse_installed_json_maps_null_empty_and_staging_tap_to_none() {
        let json = r#"{
          "formulae": [
            {"name": "a", "tap": null, "installed": [{"version": "1"}]},
            {"name": "b", "tap": "", "installed": [{"version": "1"}]},
            {"name": "c", "tap": "brewsoakr/soaked", "installed": [{"version": "1"}]}
          ],
          "casks": []
        }"#;
        let got = parse_installed_json(
            json,
            |_, _, _| Some("rb".into()),
            |_, _| ReceiptSource::default(),
        )
        .expect("parse");
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|p| p.tap.is_none()), "{got:?}");
    }

    #[test]
    fn parse_installed_json_keeps_empty_tap_as_api_mode_core() {
        let json = r#"{
          "formulae": [{"name": "ca-certificates", "tap": ""}],
          "casks": []
        }"#;
        let got = parse_installed_json(
            json,
            |name, kind, _version| {
                assert_eq!(name, "ca-certificates");
                assert_eq!(kind, PkgKind::Formula);
                Some("class CaCertificates; end".into())
            },
            |_, _| ReceiptSource::default(),
        )
        .expect("parse empty tap");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "ca-certificates");
        assert_eq!(got[0].kind, PkgKind::Formula);
    }

    #[test]
    fn parse_installed_json_cask_without_rb_receipt_uses_brew_version() {
        // Homebrew >= 4 saves `.metadata/<v>/<ts>/Casks/<token>.json`, not
        // `<token>.rb`; every installed cask used to be dropped here.
        let json = r#"{
          "formulae": [
            {"name": "vacuum", "tap": "homebrew/core", "installed": [{"version": "0.30.6"}]}
          ],
          "casks": [
            {"token": "vacuum", "full_token": "daveshanley/vacuum/vacuum",
             "tap": "daveshanley/vacuum", "installed": "0.30.6"},
            {"token": "ant", "full_token": "anthropics/tap/ant",
             "tap": "anthropics/tap", "installed": "1.38.0"},
            {"token": "font-open-sans", "tap": "homebrew/cask", "installed": "latest"},
            {"token": "flighty", "tap": "homebrew/cask", "installed": "2.0"}
          ]
        }"#;
        let got = parse_installed_json(
            json,
            |name, kind, _version| match (name, kind) {
                ("vacuum", PkgKind::Formula) => Some("class Vacuum; end".into()),
                ("flighty", PkgKind::Cask) => Some("cask \"flighty\"".into()),
                (_, PkgKind::Cask) => None,
                (other, _) => panic!("unexpected receipt read for {other}"),
            },
            |name, _| match name {
                "vacuum" => core_source(),
                other => panic!("unexpected tap read for {other}"),
            },
        )
        .expect("parse fixture");
        let summary: Vec<(&str, PkgKind, Option<&str>)> = got
            .iter()
            .map(|p| (p.name.as_str(), p.kind, p.tap.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("vacuum", PkgKind::Formula, Some("homebrew/core")),
                ("vacuum", PkgKind::Cask, Some("daveshanley/vacuum")),
                ("ant", PkgKind::Cask, Some("anthropics/tap")),
                ("font-open-sans", PkgKind::Cask, Some("homebrew/cask")),
                ("flighty", PkgKind::Cask, Some("homebrew/cask")),
            ]
        );
        let version = |i: usize| {
            crate::identity::parse_cask(&got[i].receipt_rb, None)
                .unwrap()
                .version
        };
        assert_eq!(version(1), "0.30.6");
        assert_eq!(version(2), "1.38.0");
        assert_eq!(
            version(3),
            ":latest",
            "brew reports `latest`; the cask says `:latest`"
        );
        assert_eq!(
            got[4].receipt_rb, "cask \"flighty\"",
            "a real .rb receipt still wins"
        );
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
        parse_installed_json(
            json,
            |name, kind, version| {
                versions.push((name.to_string(), kind, version.map(str::to_string)));
                Some("rb".into())
            },
            |_, _| ReceiptSource::default(),
        )
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
        let got = parse_installed_json(
            json,
            |name, kind, version| {
                assert_eq!(kind, PkgKind::Formula);
                read_formula_receipt(&cellar, name, version)
            },
            |_, _| core_source(),
        )
        .expect("parse");
        assert_eq!(
            got,
            vec![pkg(
                "wget",
                PkgKind::Formula,
                "new-receipt",
                Some("homebrew/core")
            )]
        );
    }

    fn core_source() -> ReceiptSource {
        ReceiptSource {
            tap: Some("homebrew/core".into()),
            path: None,
            own_tap: None,
        }
    }

    fn write_receipt(cellar: &Path, name: &str, version: &str, body: &str) {
        let keg = cellar.join(name).join(version);
        std::fs::create_dir_all(&keg).expect("keg");
        std::fs::write(keg.join("INSTALL_RECEIPT.json"), body).expect("receipt");
    }

    #[test]
    fn receipt_tap_reads_source_tap_of_the_named_keg() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cellar = tmp.path();
        write_receipt(
            cellar,
            "packer",
            "1.0.0",
            r#"{"source":{"tap":"homebrew/core","spec":"stable"}}"#,
        );
        write_receipt(
            cellar,
            "packer",
            "1.1.0",
            r#"{"used_options":[],"source":{"tap":"hashicorp/tap","path":"/x","spec":"stable"}}"#,
        );
        assert_eq!(
            read_formula_receipt_source(cellar, "packer", Some("1.1.0"))
                .tap
                .as_deref(),
            Some("hashicorp/tap")
        );
    }

    #[test]
    fn receipt_naming_an_own_tap_has_no_tap_and_keeps_the_fact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cellar = tmp.path();
        for (name, tap) in [
            ("a", "ericfitz/brewsoak"),
            ("b", "brewsoakr/soaked"),
            ("c", "homebrew/core"),
        ] {
            write_receipt(
                cellar,
                name,
                "1",
                &format!(r#"{{"source":{{"tap":"{tap}","spec":"stable"}}}}"#),
            );
        }
        let src = |n: &str| read_formula_receipt_source(cellar, n, Some("1"));
        let (a, b, c) = (src("a"), src("b"), src("c"));
        assert_eq!((a.tap, a.own_tap), (None, Some(OwnTap::Soak)));
        assert_eq!((b.tap, b.own_tap), (None, Some(OwnTap::Legacy)));
        assert_eq!((c.tap.as_deref(), c.own_tap), (Some("homebrew/core"), None));
    }

    #[test]
    fn receipt_tap_that_is_not_a_string_is_not_from_the_soak_tap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cellar = tmp.path();
        for (name, value) in [("a", "7"), ("b", "{}"), ("c", "true")] {
            write_receipt(
                cellar,
                name,
                "1",
                &format!(r#"{{"source":{{"tap":{value},"spec":"stable"}}}}"#),
            );
            let got = read_formula_receipt_source(cellar, name, Some("1"));
            assert_eq!((got.tap, got.own_tap), (None, None), "tap {value}");
        }
    }

    #[test]
    fn parse_installed_json_cask_keeps_the_own_tap_fact() {
        let json = r#"{"formulae": [], "casks": [
            {"token": "flighty", "tap": "ericfitz/brewsoak", "installed": "2.0"}
        ]}"#;
        let got = parse_installed_json(
            json,
            |_, _, _| None,
            |name, _| panic!("unexpected tap read for {name}"),
        )
        .expect("parse");
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].tap.clone(), got[0].own_tap),
            (None, Some(OwnTap::Soak))
        );
    }

    #[test]
    fn receipt_source_reads_the_path_only_when_the_tap_is_null() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cellar = tmp.path();
        write_receipt(
            cellar,
            "cats",
            "13.8.0",
            r#"{"source":{"tap":null,"path":"/c/staging/taps/endava/tap/Formula/cats.rb","spec":"stable"}}"#,
        );
        write_receipt(
            cellar,
            "wget",
            "1",
            r#"{"source":{"tap":"homebrew/core","path":"/x/wget.rb"}}"#,
        );
        let cats = read_formula_receipt_source(cellar, "cats", Some("13.8.0"));
        assert_eq!(cats.tap, None);
        assert_eq!(
            cats.path.as_deref(),
            Some("/c/staging/taps/endava/tap/Formula/cats.rb")
        );
        assert_eq!(
            read_formula_receipt_source(cellar, "wget", Some("1")),
            core_source()
        );
    }

    #[test]
    fn receipt_tap_is_none_when_null_missing_or_unreadable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cellar = tmp.path();
        write_receipt(
            cellar,
            "cyclonedx-gomod",
            "1.12.0",
            r#"{"source":{"tap":null,"path":"/s/Formula/cyclonedx-gomod.rb"}}"#,
        );
        write_receipt(
            cellar,
            "staged",
            "1",
            r#"{"source":{"tap":"brewsoakr/soaked"}}"#,
        );
        write_receipt(cellar, "garbled", "1", "not json");
        assert_eq!(
            read_formula_receipt_source(cellar, "cyclonedx-gomod", Some("1.12.0")).tap,
            None
        );
        assert_eq!(
            read_formula_receipt_source(cellar, "staged", Some("1")).tap,
            None
        );
        assert_eq!(
            read_formula_receipt_source(cellar, "garbled", Some("1")).tap,
            None
        );
        assert_eq!(
            read_formula_receipt_source(cellar, "absent", Some("1")).tap,
            None
        );
        assert_eq!(
            read_formula_receipt_source(cellar, "garbled", None).tap,
            None
        );
    }

    #[test]
    fn parse_installed_json_takes_a_formulas_tap_from_its_receipt_not_brew_info() {
        let json = r#"{
          "formulae": [
            {"name": "cyclonedx-gomod", "tap": "homebrew/core", "installed": [{"version": "1.12.0"}]},
            {"name": "packer", "tap": "hashicorp/tap", "installed": [{"version": "1.0"}]}
          ],
          "casks": [{"token": "vacuum", "tap": "daveshanley/vacuum", "installed": "1"}]
        }"#;
        let got = parse_installed_json(
            json,
            |_, _, _| Some("rb".into()),
            |name, _| {
                if name == "packer" {
                    ReceiptSource {
                        tap: Some("hashicorp/tap".into()),
                        path: None,
                        own_tap: None,
                    }
                } else {
                    ReceiptSource::default()
                }
            },
        )
        .expect("parse");
        let tap = |n: &str| got.iter().find(|p| p.name == n).unwrap().tap.clone();
        assert_eq!(tap("cyclonedx-gomod"), None, "staged keg: receipt null");
        assert_eq!(tap("packer").as_deref(), Some("hashicorp/tap"));
        assert_eq!(tap("vacuum").as_deref(), Some("daveshanley/vacuum"));
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
        let got = parse_installed_json(
            json,
            |name, _kind, _version| Some(name.to_string()),
            |_, _| ReceiptSource::default(),
        )
        .expect("parse");
        assert!(got.iter().find(|p| p.name == "wget").expect("wget").pinned);
        assert!(!got.iter().find(|p| p.name == "curl").expect("curl").pinned);
    }

    #[test]
    fn brewsoak_brew_env_keeps_developer_and_forbid_removal_for_install() {
        let pairs = brewsoak_brew_env_pairs("install");
        assert!(
            pairs
                .iter()
                .any(|(k, v)| *k == "HOMEBREW_DEVELOPER" && *v == Some("1"))
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| *k == "HOMEBREW_FORBID_PACKAGES_FROM_PATHS" && v.is_none())
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| *k == "HOMEBREW_NO_AUTO_UPDATE" && *v == Some("1"))
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| *k == "HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK" && *v == Some("1"))
        );
    }

    #[test]
    fn keg_receipt_asks_brew_for_the_cellar_once() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tmp");
        let cellar = tmp.path().join("Cellar");
        let brew_dir = cellar.join("wget").join("1.0").join(".brew");
        std::fs::create_dir_all(&brew_dir).expect("mkdir");
        std::fs::write(brew_dir.join("wget.rb"), "rb").expect("rb");
        let count = tmp.path().join("count");
        let script = tmp.path().join("brew");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho x >> {}\necho {}\n",
                count.display(),
                cellar.display()
            ),
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let brew = ProcessBrew::new(script);
        for _ in 0..3 {
            assert_eq!(brew.keg_receipt("wget", "1.0").as_deref(), Some("rb"));
        }
        let calls = std::fs::read_to_string(count)
            .expect("count")
            .lines()
            .count();
        assert_eq!(calls, 1, "brew --cellar was spawned {calls} times");
    }

    /// A `brew` stand-in that logs its argv (one line per call) and prints `stdout`.
    fn compare_script(tmp: &Path, stdout: &str, status: i32) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = tmp.join("argv");
        let script = tmp.join("brew");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\nprintf '{stdout}'\nexit {status}\n",
                log.display()
            ),
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        (script, log)
    }

    #[test]
    fn compare_versions_batches_one_call_passes_argv_and_memoizes() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (script, log) = compare_script(tmp.path(), "1\\n-1\\n", 0);
        let brew = ProcessBrew::new(script);
        let pairs = [
            (PkgKind::Formula, "3.6.5".to_string(), "3.5.9".to_string()),
            (PkgKind::Cask, "1.0".to_string(), "2.0".to_string()),
        ];
        let want = vec![Ordering::Greater, Ordering::Less];
        assert_eq!(brew.compare_versions(&pairs).expect("compare"), want);
        assert_eq!(brew.compare_versions(&pairs).expect("memo"), want);
        let argv = std::fs::read_to_string(log).expect("log");
        assert_eq!(
            argv.matches("ruby -e").count(),
            1,
            "second ask is memoized: {argv}"
        );
        assert!(argv.starts_with("ruby -e "), "{argv}");
        assert!(
            argv.trim_end()
                .ends_with("-- formula 3.6.5 3.5.9 cask 1.0 2.0"),
            "versions travel as argv, not Ruby source: {argv}"
        );
    }

    fn source(url: &str, tag: Option<&str>) -> VersionSource {
        VersionSource {
            url: url.to_string(),
            tag: tag.map(String::from),
        }
    }

    #[test]
    fn detect_versions_batches_one_call_passes_argv_and_memoizes() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (script, log) = compare_script(tmp.path(), "19\n\n1.2.3\n", 0);
        let brew = ProcessBrew::new(script);
        let sources = [
            source("https://x.test/sf_19.tar.gz", None),
            source("https://x.test/download", None),
            source("https://x.test/foo.git", Some("v1.2.3")),
            source("https://x.test/#{version}.tgz", None),
        ];
        let want = vec![
            Some("19".to_string()),
            None,
            Some("1.2.3".to_string()),
            None,
        ];
        assert_eq!(brew.detect_versions(&sources).expect("detect"), want);
        assert_eq!(brew.detect_versions(&sources).expect("memo"), want);
        let argv = std::fs::read_to_string(log).expect("log");
        assert_eq!(
            argv.matches("ruby -e").count(),
            1,
            "second ask is memoized: {argv}"
        );
        assert!(argv.contains("Version.detect"), "{argv}");
        assert!(
            argv.trim_end().ends_with(
                "-- https://x.test/sf_19.tar.gz  https://x.test/download  https://x.test/foo.git v1.2.3"
            ),
            "strings travel as argv, an empty tag as an empty argument: {argv}"
        );
    }

    #[test]
    fn detect_versions_failure_is_memoized_for_the_run() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (script, log) = compare_script(tmp.path(), "", 1);
        let brew = ProcessBrew::new(script);
        let one = source("https://x.test/a-1.tgz", None);
        assert!(brew.detect_versions(std::slice::from_ref(&one)).is_err());
        assert!(brew.detect_versions(std::slice::from_ref(&one)).is_err());
        let two = source("https://x.test/b-1.tgz", None);
        assert!(brew.detect_versions(&[one, two]).is_err());
        let argv = std::fs::read_to_string(log).expect("log");
        assert_eq!(argv.matches("ruby -e").count(), 1, "{argv}");
    }

    #[test]
    fn detect_versions_errors_on_failure_or_wrong_line_count() {
        for (stdout, status) in [("", 1), ("1\n", 1), ("1\n", 0), ("1\n2\n3\n", 0)] {
            let tmp = tempfile::tempdir().expect("tmp");
            let (script, _) = compare_script(tmp.path(), stdout, status);
            let brew = ProcessBrew::new(script);
            let sources = [
                source("https://x.test/a-1.tgz", None),
                source("https://x.test/b-2.tgz", None),
            ];
            assert!(
                brew.detect_versions(&sources).is_err(),
                "{stdout:?} exit {status} must not parse"
            );
        }
    }

    #[test]
    fn detect_versions_never_asks_for_an_interpolated_url() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (script, log) = compare_script(tmp.path(), "", 0);
        let brew = ProcessBrew::new(script);
        let got = brew
            .detect_versions(&[source("https://x.test/#{version}.tgz", None)])
            .expect("detect");
        assert_eq!(got, vec![None]);
        assert!(!log.exists(), "brew must not be spawned");
    }

    #[test]
    fn compare_versions_failure_is_memoized_for_the_run() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (script, log) = compare_script(tmp.path(), "", 1);
        let brew = ProcessBrew::new(script);
        let pair = (PkgKind::Formula, "1.0".to_string(), "2.0".to_string());
        assert!(brew.compare_versions(std::slice::from_ref(&pair)).is_err());
        assert!(brew.compare_versions(std::slice::from_ref(&pair)).is_err());
        let other = (PkgKind::Formula, "1.0".to_string(), "3.0".to_string());
        assert!(brew.compare_versions(&[pair.clone(), other]).is_err());
        let argv = std::fs::read_to_string(log).expect("log");
        assert_eq!(argv.matches("ruby -e").count(), 1, "{argv}");
    }

    #[test]
    fn host_platform_runs_brew_ruby_once_and_parses_the_output() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (script, log) = compare_script(
            tmp.path(),
            "tahoe 26\\nsequoia 15\\nbig_sur 11\\nHOST sequoia arm64\\n",
            0,
        );
        let brew = ProcessBrew::new(script);
        let host = brew.host_platform().expect("host");
        assert_eq!(
            (host.macos.as_str(), host.arch.as_str()),
            ("sequoia", "arm64")
        );
        assert_eq!(host.macos_versions.len(), 3);
        assert_eq!(brew.host_platform(), Some(host), "memoized");
        let argv = std::fs::read_to_string(log).expect("log");
        assert_eq!(argv.matches("ruby -e").count(), 1, "{argv}");
        assert!(argv.contains("MacOSVersion::SYMBOLS"), "{argv}");
    }

    #[test]
    fn host_platform_is_none_on_garbage_failure_or_no_host_line() {
        for (stdout, status) in [
            ("what\\n", 0),
            ("", 0),
            ("sequoia 15\\n", 0),
            ("sequoia 15\\nHOST sequoia arm64\\n", 1),
        ] {
            let tmp = tempfile::tempdir().expect("tmp");
            let (script, _) = compare_script(tmp.path(), stdout, status);
            let brew = ProcessBrew::new(script);
            assert_eq!(brew.host_platform(), None, "{stdout:?} exit {status}");
        }
    }

    #[test]
    fn compare_versions_errors_on_failure_or_unparseable_output() {
        for (stdout, status) in [("", 1), ("1\\n", 0), ("banana\\nx\\n", 0), ("1\\n2\\n", 0)] {
            let tmp = tempfile::tempdir().expect("tmp");
            let (script, _) = compare_script(tmp.path(), stdout, status);
            let brew = ProcessBrew::new(script);
            let pairs = [
                (PkgKind::Formula, "1.0".to_string(), "2.0".to_string()),
                (PkgKind::Formula, "1.0_1".to_string(), "1.0".to_string()),
            ];
            assert!(
                brew.compare_versions(&pairs).is_err(),
                "{stdout:?} exit {status} must not parse"
            );
        }
    }

    fn env_of(verb: &str) -> Vec<(String, Option<String>)> {
        let mut cmd = Command::new("brew");
        cmd.arg(verb);
        apply_brewsoak_brew_env(&mut cmd);
        cmd.get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn update_child_never_sets_homebrew_developer() {
        // With HOMEBREW_DEVELOPER set, brew update moves Homebrew to main
        // instead of the latest stable tag. The child must inherit the user's
        // own value, not gain one: no entry means the parent env passes
        // through untouched (set stays set, unset stays unset).
        let env = env_of("update");
        assert!(
            !env.iter().any(|(k, _)| k == "HOMEBREW_DEVELOPER"),
            "{env:?}"
        );
        assert!(
            env.contains(&("HOMEBREW_NO_AUTO_UPDATE".into(), Some("1".into()))),
            "{env:?}"
        );
    }

    #[test]
    fn other_children_still_set_homebrew_developer() {
        for verb in ["install", "upgrade", "ruby", "deps", "info", "outdated"] {
            let env = env_of(verb);
            assert!(
                env.contains(&("HOMEBREW_DEVELOPER".into(), Some("1".into()))),
                "{verb}: {env:?}"
            );
        }
    }

    #[test]
    fn brewsoak_brew_env_turns_brew_colour_off() {
        let pairs = brewsoak_brew_env_pairs("install");
        assert!(
            pairs
                .iter()
                .any(|(k, v)| *k == "HOMEBREW_NO_COLOR" && *v == Some("1"))
        );
    }

    #[test]
    fn brewsoak_brew_env_never_disables_tap_trust() {
        // brewsoak never trusts a tap on the user's behalf (2026-10-05 human
        // decision), so it must not switch Homebrew's tap-trust check off
        // either. The removed brewsoakr/soaked fallback used to set this.
        let pairs = brewsoak_brew_env_pairs("install");
        assert!(
            !pairs
                .iter()
                .any(|(k, _)| *k == "HOMEBREW_NO_REQUIRE_TAP_TRUST"),
            "{pairs:?}"
        );
    }

    #[test]
    fn process_brew_run_visible_merges_stderr_summarizes_and_logs_raw() {
        // Cleanup on stdout, its Removing line on stderr: the summarizer only
        // sees them in the right order because both share one pipe.
        let brew = ProcessBrew::new(PathBuf::from("/bin/sh"));
        let script = "echo '==> Cleanup'; echo 'Removing: /x... (1 files, 1KB)' 1>&2";
        let output = brew
            .run_visible(&["-c".into(), script.into()], &mut Vec::new())
            .expect("run_visible");
        assert!(output.status.success());
        let captured = String::from_utf8_lossy(&output.stdout);
        assert!(captured.contains("==> Cleanup"), "{captured}");
        assert!(captured.contains("Removing:"), "{captured}");

        let report = brew.session_report();
        assert_eq!(report.freed_bytes, 1024, "cleanup sizes are tallied");
        let log = std::fs::read_to_string(report.log_path.expect("log path")).expect("log");
        assert!(log.contains("Removing:"), "raw output must reach the log");
    }

    #[test]
    fn mock_run_visible_records_separately() {
        let brew = MockBrew::new();
        brew.run_visible(&["install".into(), "wget".into()], &mut Vec::new())
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
        let mut shown = Vec::new();
        let brew = ProcessBrew::new(PathBuf::from("/bin/echo"));
        let output = brew
            .run_visible(&["hello-visible".into()], &mut shown)
            .expect("run_visible");
        assert!(output.status.success());
        let shown = String::from_utf8(shown).expect("utf8");
        assert!(
            shown.contains("hello-visible"),
            "the summary reaches the injected terminal: {shown:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("hello-visible"),
            "{:?}",
            String::from_utf8_lossy(&output.stdout)
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

    #[test]
    fn mock_run_visible_pops_queued_outputs() {
        let brew = MockBrew {
            next_outputs: Mutex::new(std::collections::VecDeque::from(vec![(
                2,
                b"first\n".to_vec(),
            )])),
            next_status: 0,
            ..MockBrew::new()
        };
        let a = brew.run_visible(&["x".into()], &mut Vec::new()).unwrap();
        assert_eq!(a.status.code(), Some(2));
        assert_eq!(a.stdout, b"first\n");
        let b = brew.run_visible(&["y".into()], &mut Vec::new()).unwrap();
        assert_eq!(
            b.status.code(),
            Some(0),
            "queue empty: falls back to next_status"
        );
    }

    fn write_cask_config(caskroom: &Path, token: &str, json: &str) {
        let meta = caskroom.join(token).join(".metadata");
        std::fs::create_dir_all(&meta).expect("metadata");
        std::fs::write(meta.join("config.json"), json).expect("config");
    }

    #[test]
    fn cask_languages_reads_only_the_saved_explicit_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_cask_config(
            tmp.path(),
            "cave-story",
            r#"{"default":{"languages":["en-US","ja"],"appdir":"/Applications"},"env":{"languages":["fr","ja"]},"explicit":{"languages":["ja","de","ja"]}}"#,
        );
        assert_eq!(
            read_cask_languages(tmp.path(), "cave-story"),
            Some(vec!["ja".into(), "de".into()])
        );
        write_cask_config(
            tmp.path(),
            "fr-default",
            r#"{"default":{"languages":["fr"]},"env":{},"explicit":{}}"#,
        );
        assert_eq!(
            read_cask_languages(tmp.path(), "fr-default"),
            Some(Vec::new())
        );
    }

    #[test]
    fn find_json_key_matches_only_the_immediate_object_level() {
        let obj = r#"{"extra":{"languages":["de"]},"languages":["fr"]}"#;
        assert_eq!(
            find_json_key(obj, "languages").map(str::trim_start),
            Some(r#"["fr"]}"#)
        );
        assert_eq!(find_json_key(obj, "nope"), None);
        // A key text inside a string value is not a key.
        let tricky = r#"{"a":"\"languages\":[1]","languages":["x"]}"#;
        assert_eq!(
            find_json_key(tricky, "languages").map(str::trim_start),
            Some(r#"["x"]}"#)
        );
        let cfg = r#"{"default":{"extra":{"languages":["de"]},"languages":["fr"]},"explicit":{"extra":{"languages":["de"]}}}"#;
        let tmp = tempfile::tempdir().expect("tempdir");
        write_cask_config(tmp.path(), "n", cfg);
        assert_eq!(read_cask_languages(tmp.path(), "n"), Some(Vec::new()));
    }

    #[test]
    fn process_brew_memoizes_cask_languages_per_token() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_cask_config(
            tmp.path(),
            "a",
            r#"{"default":{},"env":{},"explicit":{"languages":["ja"]}}"#,
        );
        let brew = ProcessBrew::new(PathBuf::from("/nonexistent/brew"));
        let _ = brew.caskroom.set(Some(tmp.path().to_path_buf()));
        assert_eq!(brew.cask_languages("a"), Some(vec!["ja".into()]));
        write_cask_config(
            tmp.path(),
            "a",
            r#"{"default":{},"env":{},"explicit":{"languages":["de"]}}"#,
        );
        assert_eq!(
            brew.cask_languages("a"),
            Some(vec!["ja".into()]),
            "second call must not re-read"
        );
        assert_eq!(brew.cask_languages("missing"), None);
        std::fs::remove_dir_all(tmp.path().join("a")).expect("rm");
        assert_eq!(brew.cask_languages("a"), Some(vec!["ja".into()]));
    }

    #[test]
    fn process_brew_shares_one_caskroom_lookup() {
        let brew = ProcessBrew::new(PathBuf::from("/nonexistent/brew"));
        let _ = brew.caskroom.set(Some(PathBuf::from("/cr")));
        assert_eq!(brew.caskroom(), Some(PathBuf::from("/cr")));
        assert_eq!(brew.caskroom(), Some(PathBuf::from("/cr")));
    }

    #[test]
    fn cask_languages_drops_invalid_locales_and_reads_an_empty_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_cask_config(
            tmp.path(),
            "a",
            r#"{"default":{},"env":{},"explicit":{"languages":["en_US","ja-JP-mac","en-US"]}}"#,
        );
        assert_eq!(
            read_cask_languages(tmp.path(), "a"),
            Some(vec!["en-US".into()])
        );
        write_cask_config(tmp.path(), "b", r#"{"default":{},"env":{},"explicit":{}}"#);
        assert_eq!(read_cask_languages(tmp.path(), "b"), Some(Vec::new()));
    }

    #[test]
    fn cask_languages_is_none_when_missing_or_unreadable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_cask_languages(tmp.path(), "nope"), None);
        write_cask_config(tmp.path(), "bad", "not json");
        assert_eq!(read_cask_languages(tmp.path(), "bad"), None);
        write_cask_config(tmp.path(), "arr", r#"{"explicit":{"languages":"en"}}"#);
        assert_eq!(read_cask_languages(tmp.path(), "arr"), None);
    }

    #[test]
    fn mock_cask_languages_reports_the_configured_list() {
        let mut brew = MockBrew::new();
        brew.cask_languages
            .insert("cave-story".into(), vec!["ja".into()]);
        assert_eq!(brew.cask_languages("cave-story"), Some(vec!["ja".into()]));
        assert_eq!(brew.cask_languages("other"), None);
    }

    fn loaded(tap: &str, sum: Option<&str>) -> Loaded {
        Loaded {
            tap: tap.into(),
            version: "1.0".into(),
            source_sha256: sum.map(str::to_string),
            installed_current: false,
        }
    }

    fn query(name: &str, origin: &str, sum: Option<&str>) -> NameQuery {
        NameQuery {
            kind: PkgKind::Formula,
            name: name.into(),
            origin: origin.into(),
            cutoff_sha256: sum.map(str::to_string),
        }
    }

    fn asked(names: &[&str]) -> Vec<(PkgKind, String)> {
        names
            .iter()
            .map(|n| (PkgKind::Formula, n.to_string()))
            .collect()
    }

    #[test]
    fn parse_by_name_lines_reads_ok_and_err_rows() {
        let out = "OK\tformula\ta\thomebrew/core\t1.5.2_3\tAbC123\ttrue\n\
                   OK\tcask\tb\tHomebrew/Cask\t4.0\tdef456\tfalse\n\
                   OK\tformula\tc\thomebrew/core\t1.0\t\tfalse\n\
                   OK\tformula\td\t\t1.0\tabc\tfalse\n\
                   ERR\tformula\te\tFormulaUnavailableError\tNo available formula with the name \"x\".\n\
                   OK\tformula\tf\thomebrew/core\t1.0\tabc\tmaybe\n\
                   OK\tformula\tg\ttoo\tfew\n";
        let q = vec![
            (PkgKind::Formula, "a".to_string()),
            (PkgKind::Cask, "b".to_string()),
            (PkgKind::Formula, "c".to_string()),
            (PkgKind::Formula, "d".to_string()),
            (PkgKind::Formula, "e".to_string()),
            (PkgKind::Formula, "f".to_string()),
            (PkgKind::Formula, "g".to_string()),
        ];
        let got = parse_by_name_lines(out, &q);
        assert_eq!(
            got[0],
            Ok(Loaded {
                tap: "homebrew/core".into(),
                version: "1.5.2_3".into(),
                source_sha256: Some("abc123".into()),
                installed_current: true,
            })
        );
        let cask = got[1].as_ref().expect("cask row");
        assert_eq!(cask.tap, "homebrew/cask");
        assert!(!cask.installed_current);
        assert_eq!(got[2].as_ref().expect("empty sum").source_sha256, None);
        assert!(got[3].is_err(), "an empty tap is not an answer");
        assert!(
            got[4]
                .as_ref()
                .unwrap_err()
                .contains("No available formula")
        );
        assert!(
            got[5].as_ref().unwrap_err().contains("unparseable"),
            "installed must be true or false"
        );
        assert!(got[6].is_err(), "a short row is malformed");
    }

    #[test]
    fn parse_by_name_lines_short_output_is_err_for_the_missing_names() {
        let got = parse_by_name_lines(
            "OK\tformula\ta\thomebrew/core\t1\tabc\tfalse\n",
            &asked(&["a", "b", "c"]),
        );
        assert_eq!(got.len(), 3);
        assert!(got[0].is_ok());
        assert!(got[1].is_err() && got[2].is_err());
    }

    #[test]
    fn parse_by_name_lines_ignores_a_stray_line_before_the_rows() {
        let out = "Warning: something brew printed\n\
                   OK\tformula\ta\thomebrew/core\t1\tabc\tfalse\n\
                   OK\tformula\tb\tother/tap\t2\tdef\tfalse\n";
        let got = parse_by_name_lines(out, &asked(&["a", "b"]));
        assert_eq!(got[0].as_ref().unwrap().tap, "homebrew/core");
        assert_eq!(got[1].as_ref().unwrap().tap, "other/tap");
    }

    #[test]
    fn parse_by_name_lines_matches_rows_by_name_not_position() {
        let out = "OK\tformula\tb\tother/tap\t2\tdef\tfalse\n\
                   OK\tformula\ta\thomebrew/core\t1\tabc\tfalse\n";
        let got = parse_by_name_lines(out, &asked(&["a", "b"]));
        assert_eq!(got[0].as_ref().unwrap().tap, "homebrew/core");
        assert_eq!(got[1].as_ref().unwrap().tap, "other/tap");
    }

    #[test]
    fn parse_by_name_lines_never_assigns_a_row_for_an_unasked_name() {
        let out = "OK\tformula\tzzz\thomebrew/core\t1\tabc\tfalse\n\
                   OK\tcask\ta\thomebrew/cask\t1\tabc\tfalse\n";
        let got = parse_by_name_lines(out, &asked(&["a"]));
        assert!(got[0].is_err(), "a cask row is not the formula's answer");
    }

    #[test]
    fn parse_by_name_lines_duplicate_rows_are_err() {
        let out = "OK\tformula\ta\thomebrew/core\t1\tabc\tfalse\n\
                   OK\tformula\ta\tother/tap\t1\tabc\tfalse\n\
                   OK\tformula\tb\thomebrew/core\t1\tabc\tfalse\n";
        let got = parse_by_name_lines(out, &asked(&["a", "b"]));
        assert!(got[0].is_err());
        assert!(got[1].is_ok());
    }

    #[test]
    fn judge_same_source_needs_equal_checksum() {
        let l = loaded("homebrew/core", Some("abc"));
        assert!(judge(&l, &query("x", "homebrew/core", Some("abc"))).same_source);
        assert!(judge(&l, &query("x", "homebrew/core", Some("ABC"))).same_source);
        assert!(!judge(&l, &query("x", "homebrew/core", Some("abd"))).same_source);
        let none = loaded("homebrew/core", None);
        assert!(!judge(&none, &query("x", "homebrew/core", Some("abc"))).same_source);
    }

    #[test]
    fn judge_no_cutoff_is_never_same_source() {
        let l = loaded("homebrew/core", Some("abc"));
        assert!(!judge(&l, &query("x", "homebrew/core", None)).same_source);
        let none = loaded("homebrew/core", None);
        assert!(!judge(&none, &query("x", "homebrew/core", None)).same_source);
    }

    #[test]
    fn installs_cutoff_needs_the_origin_tap() {
        let l = loaded("homebrew/core", Some("abc"));
        let by = judge(
            &l,
            &query("cyclonedx-gomod", "cyclonedx/cyclonedx", Some("abc")),
        );
        assert!(by.same_source);
        assert!(!by.installs_cutoff("cyclonedx/cyclonedx"));
        assert!(by.installs_cutoff("homebrew/core"));
        assert!(by.installs_cutoff("Homebrew/Core"));
        let diff = judge(&l, &query("x", "homebrew/core", Some("zzz")));
        assert!(!diff.installs_cutoff("homebrew/core"));
    }

    /// A fake brew: `info` lists one installed cask (`alt-tab`), `ruby` logs
    /// its names and answers one OK row each, `deps` logs and prints a name.
    /// `ruby_exit` is the exit status of the `ruby` call.
    fn fake_loader_brew(tmp: &Path, ruby_exit: i32) -> (ProcessBrew, PathBuf) {
        let log = tmp.join("log");
        let script = tmp.join("brew.sh");
        let body = format!(
            r#"#!/bin/sh
case "$1" in
  info)
    echo "$1 $2 $3" >> {log}
    echo '{{"formulae": [], "casks": [{{"token": "alt-tab", "tap": "homebrew/cask", "installed": "1.0"}}]}}'
    ;;
  ruby)
    shift 4
    echo "ruby $*" >> {log}
    if [ {ruby_exit} -ne 0 ]; then echo "boom" >&2; exit {ruby_exit}; fi
    while [ $# -gt 0 ]; do
      printf 'OK\t%s\t%s\thomebrew/core\t1.0\tabc\tfalse\n' "$1" "$2"
      shift 2
    done
    ;;
  deps)
    echo "$@" >> {log}
    echo wget
    ;;
  update)
    echo "$@" >> {log}
    if [ -e {marker} ]; then exit 1; fi
    ;;
  *)
    echo "$@" >> {log}
    ;;
esac
"#,
            log = log.display(),
            marker = tmp.join("update_fails").display(),
            ruby_exit = ruby_exit
        );
        std::fs::write(&script, body).expect("script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        (ProcessBrew::new(script), log)
    }

    fn log_lines(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn process_brew_by_name_asks_about_every_installed_package_in_its_first_call() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 0);
        brew.installed_packages().expect("installed");
        let got = brew
            .by_name(&[query("wget", "homebrew/core", Some("abc"))])
            .expect("by_name");
        assert!(
            got[0]
                .as_ref()
                .expect("wget")
                .installs_cutoff("homebrew/core")
        );
        let rubies: Vec<String> = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("ruby"))
            .collect();
        assert_eq!(rubies, ["ruby formula wget cask alt-tab"]);
        let cask = NameQuery {
            kind: PkgKind::Cask,
            ..query("alt-tab", "homebrew/cask", None)
        };
        brew.by_name(&[cask]).expect("installed name");
        let rubies = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("ruby"))
            .count();
        assert_eq!(rubies, 1, "an installed name was already answered");
    }

    #[test]
    fn process_brew_by_name_asks_once_per_uninstalled_name() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 0);
        let q = query("wget", "homebrew/core", Some("abc"));
        brew.by_name(std::slice::from_ref(&q)).expect("first");
        brew.by_name(std::slice::from_ref(&q)).expect("again");
        brew.by_name(&[q.clone(), query("curl", "homebrew/core", Some("abc"))])
            .expect("one new");
        let rubies: Vec<String> = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("ruby"))
            .collect();
        assert_eq!(rubies, ["ruby formula wget", "ruby formula curl"]);
    }

    #[test]
    fn process_brew_by_name_script_failure_is_err_for_every_query() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 1);
        let qs = [
            query("wget", "homebrew/core", Some("abc")),
            query("curl", "homebrew/core", Some("abc")),
        ];
        let got = brew.by_name(&qs).expect("never an outer Err");
        assert_eq!(got.len(), 2);
        assert!(got[0].as_ref().unwrap_err().contains("boom"));
        assert!(got[1].is_err());
        brew.by_name(&qs).expect("remembered");
        let rubies = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("ruby"))
            .count();
        assert_eq!(rubies, 1, "a failure is remembered like a success");
    }

    #[test]
    fn process_brew_by_name_never_runs_brew_info() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 0);
        brew.installed_packages().expect("installed");
        brew.by_name(&[query("wget", "homebrew/core", Some("abc"))])
            .expect("by_name");
        let infos: Vec<String> = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("info"))
            .collect();
        assert_eq!(infos, ["info --json=v2 --installed"]);
    }

    #[test]
    fn process_brew_deps_asks_once_per_token() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 0);
        assert_eq!(brew.deps(PkgKind::Formula, "wget").expect("1"), ["wget"]);
        assert_eq!(brew.deps(PkgKind::Formula, "wget").expect("2"), ["wget"]);
        let deps = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("deps"))
            .count();
        assert_eq!(deps, 1);
    }

    #[test]
    fn process_brew_forgets_loader_and_deps_answers_after_brew_update() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 0);
        let q = query("wget", "homebrew/core", Some("abc"));
        brew.by_name(std::slice::from_ref(&q)).expect("by_name");
        brew.deps(PkgKind::Formula, "wget").expect("deps");
        let mut term = Vec::new();
        brew.run_visible(&["update".to_string()], &mut term)
            .expect("update");
        brew.by_name(std::slice::from_ref(&q)).expect("by_name 2");
        brew.deps(PkgKind::Formula, "wget").expect("deps 2");
        let lines = log_lines(&log);
        let count = |p: &str| lines.iter().filter(|l| l.starts_with(p)).count();
        assert_eq!((count("ruby"), count("deps")), (2, 2), "{lines:?}");
    }

    #[test]
    fn process_brew_by_name_without_a_brew_binary_is_the_outer_err() {
        let brew = ProcessBrew::new(PathBuf::from("/nonexistent/brew-for-tests"));
        assert!(
            brew.by_name(&[query("wget", "homebrew/core", None)])
                .is_err()
        );
    }

    #[test]
    fn process_brew_by_name_asks_a_duplicate_name_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 0);
        let q = query("wget", "homebrew/core", Some("abc"));
        let got = brew.by_name(&[q.clone(), q]).expect("by_name");
        assert!(got[0].is_ok() && got[1].is_ok());
        let rubies: Vec<String> = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("ruby"))
            .collect();
        assert_eq!(rubies, ["ruby formula wget"]);
    }

    #[test]
    fn process_brew_by_name_before_installed_packages_still_batches_installed_names_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 0);
        brew.by_name(&[query("wget", "homebrew/core", Some("abc"))])
            .expect("early");
        brew.installed_packages().expect("installed");
        brew.by_name(&[query("curl", "homebrew/core", Some("abc"))])
            .expect("later");
        brew.by_name(&[query("jq", "homebrew/core", Some("abc"))])
            .expect("again");
        let rubies: Vec<String> = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("ruby"))
            .collect();
        assert_eq!(
            rubies,
            [
                "ruby formula wget",
                "ruby formula curl cask alt-tab",
                "ruby formula jq"
            ]
        );
    }

    #[test]
    fn process_brew_by_name_failed_spawn_does_not_use_up_the_installed_batch() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, _log) = fake_loader_brew(tmp.path(), 1);
        brew.installed_packages().expect("installed");
        brew.by_name(&[query("wget", "homebrew/core", None)])
            .expect("failed spawn is per-name Err");
        assert!(!*brew.installed_asked.lock().unwrap());
    }

    #[test]
    fn process_brew_failing_update_still_clears_both_memos() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (brew, log) = fake_loader_brew(tmp.path(), 0);
        std::fs::write(tmp.path().join("update_fails"), "").expect("marker");
        let q = query("wget", "homebrew/core", Some("abc"));
        brew.by_name(std::slice::from_ref(&q)).expect("by_name");
        brew.deps(PkgKind::Formula, "wget").expect("deps");
        let mut term = Vec::new();
        let out = brew
            .run_visible(&["update".to_string()], &mut term)
            .expect("update runs");
        assert!(!out.status.success());
        brew.by_name(std::slice::from_ref(&q)).expect("by_name 2");
        brew.deps(PkgKind::Formula, "wget").expect("deps 2");
        let lines = log_lines(&log);
        let count = |p: &str| lines.iter().filter(|l| l.starts_with(p)).count();
        assert_eq!((count("ruby"), count("deps")), (2, 2), "{lines:?}");
    }

    #[test]
    fn brew_children_never_force_api_auto_update() {
        assert!(
            brewsoak_brew_env_pairs("update").contains(&("HOMEBREW_FORCE_API_AUTO_UPDATE", None)),
            "a stale FORCE_API_AUTO_UPDATE would make the loaders refetch"
        );
    }

    #[test]
    fn mock_by_name_defaults_to_same_source() {
        let brew = MockBrew::new();
        let q = query("wget", "homebrew/core", Some("abc"));
        let got = brew.by_name(std::slice::from_ref(&q)).expect("mock");
        assert_eq!(
            got,
            vec![Ok(ByName {
                tap: "homebrew/core".into(),
                version: String::new(),
                same_source: true,
                installed_current: false,
            })]
        );
        assert_eq!(*brew.by_name_queries.lock().unwrap(), vec![q]);
    }

    #[test]
    fn mock_by_name_honors_scripted_answers() {
        let mut brew = MockBrew::new();
        brew.by_name.insert("wget".into(), Err("untrusted".into()));
        brew.by_name.insert(
            "curl".into(),
            Ok(ByName {
                tap: "other/tap".into(),
                version: "2".into(),
                same_source: false,
                installed_current: true,
            }),
        );
        let got = brew
            .by_name(&[
                query("wget", "homebrew/core", None),
                query("curl", "homebrew/core", None),
                query("jq", "homebrew/core", None),
            ])
            .expect("mock");
        assert_eq!(got[0], Err("untrusted".into()));
        assert_eq!(got[1].as_ref().unwrap().tap, "other/tap");
        assert!(got[2].as_ref().unwrap().same_source);
    }
}
