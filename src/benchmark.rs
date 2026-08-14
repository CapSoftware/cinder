use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ORIGINAL_EDIT_TOKEN: &str = "\"window.FLAGS = {}\"";
const BENCHMARK_EDIT_PREFIX: &str = "\"window.FLAGS = {};/* cinder-bench-";
const DEFAULT_EDIT_FILE: &str = "apps/desktop/src-tauri/src/flags.rs";
const DEFAULT_APP_BINARY: &str = "target/debug/cap-desktop";
const WINDOW_PROBE_SOURCE: &str = include_str!("../benchmarks/cap/window_probe.swift");

pub fn run(arguments: Vec<OsString>) -> Result<u8, String> {
    let options = Options::parse(arguments)?;
    let before = GitSnapshot::capture(&options.repository)?;
    let result = match options.mode {
        Mode::Warm => run_warm(&options, &before),
        Mode::Startup => run_startup(&options, &before),
        Mode::Clean => run_clean(&options, &before),
    }?;
    let after = GitSnapshot::capture(&options.repository)?;

    if before != after {
        return Err(
            "Cap working state changed during the benchmark; results were not written".to_owned(),
        );
    }

    result.write(&options, &before)?;
    Ok(0)
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Warm,
    Startup,
    Clean,
}

impl Mode {
    fn parse(value: &OsStr) -> Result<Self, String> {
        match value.to_str() {
            Some("warm") => Ok(Self::Warm),
            Some("startup") => Ok(Self::Startup),
            Some("clean") => Ok(Self::Clean),
            _ => Err("benchmark mode must be one of: warm, startup, clean".to_owned()),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Startup => "startup",
            Self::Clean => "clean",
        }
    }
}

struct Options {
    mode: Mode,
    repository: PathBuf,
    trials: usize,
    timeout: Duration,
    runtime_lease_owner: Option<String>,
    edit_file: PathBuf,
    app_binary: PathBuf,
    output: PathBuf,
    command: Vec<OsString>,
}

struct OptionsBuilder {
    mode: Mode,
    repository: Option<PathBuf>,
    trials: usize,
    timeout: Duration,
    runtime_lease_owner: Option<String>,
    edit_file: PathBuf,
    app_binary: PathBuf,
    output: Option<PathBuf>,
    command: Vec<OsString>,
}

impl OptionsBuilder {
    fn new(mode: Mode) -> Self {
        let trials = match mode {
            Mode::Clean => 3,
            Mode::Warm | Mode::Startup => 7,
        };
        let timeout = Duration::from_secs(match mode {
            Mode::Clean => 3_600,
            Mode::Warm | Mode::Startup => 600,
        });

        Self {
            mode,
            repository: None,
            trials,
            timeout,
            runtime_lease_owner: None,
            edit_file: PathBuf::from(DEFAULT_EDIT_FILE),
            app_binary: PathBuf::from(DEFAULT_APP_BINARY),
            output: None,
            command: Vec::new(),
        }
    }

    fn parse(mut self, mut arguments: impl Iterator<Item = OsString>) -> Result<Self, String> {
        while let Some(argument) = arguments.next() {
            if argument == "--" {
                self.command.extend(arguments);
                break;
            }

            let flag = argument
                .to_str()
                .ok_or_else(|| "benchmark option names must be valid UTF-8".to_owned())?;
            if !matches!(
                flag,
                "--repo"
                    | "--trials"
                    | "--timeout-seconds"
                    | "--runtime-lease-owner"
                    | "--edit-file"
                    | "--app-binary"
                    | "--output"
            ) {
                return Err(format!("unknown benchmark option: {flag}"));
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("{flag} requires a value"))?;

            match flag {
                "--repo" => self.repository = Some(PathBuf::from(value)),
                "--trials" => {
                    self.trials = parse_number(&value, "--trials")?;
                    if self.trials < 2 {
                        return Err("--trials must be at least 2".to_owned());
                    }
                }
                "--timeout-seconds" => {
                    self.timeout = Duration::from_secs(parse_number(&value, "--timeout-seconds")?);
                }
                "--runtime-lease-owner" => {
                    self.runtime_lease_owner = Some(
                        value
                            .to_str()
                            .ok_or_else(|| "--runtime-lease-owner must be valid UTF-8".to_owned())?
                            .to_owned(),
                    );
                }
                "--edit-file" => self.edit_file = PathBuf::from(value),
                "--app-binary" => self.app_binary = PathBuf::from(value),
                "--output" => self.output = Some(PathBuf::from(value)),
                _ => unreachable!("internal error: known benchmark options are not exhaustive"),
            }
        }
        Ok(self)
    }

    fn build(mut self) -> Result<Options, String> {
        let repository = fs::canonicalize(
            self.repository
                .ok_or_else(|| "--repo is required".to_owned())?,
        )
        .map_err(|error| format!("could not resolve --repo: {error}"))?;
        ensure_git_root(&repository)?;

        if self.command.is_empty() {
            self.command = match self.mode {
                Mode::Warm | Mode::Startup => os_arguments(&["pnpm", "dev:desktop"]),
                Mode::Clean => os_arguments(&[
                    "cargo",
                    "build",
                    "--locked",
                    "-p",
                    "cap-desktop",
                    "--timings",
                ]),
            };
        }

        if matches!(self.mode, Mode::Warm | Mode::Startup) && self.runtime_lease_owner.is_none() {
            return Err(
                "warm and startup modes require --runtime-lease-owner to protect the shared desktop runtime"
                    .to_owned(),
            );
        }

        let epoch = epoch_seconds()?;
        let output = match self.output {
            Some(output) => output,
            None => env::current_dir()
                .map_err(|error| format!("could not inspect invocation directory: {error}"))?
                .join("target/cinder-bench/results")
                .join(format!("{epoch}-{}.json", self.mode.label())),
        };

        Ok(Options {
            mode: self.mode,
            repository,
            trials: self.trials,
            timeout: self.timeout,
            runtime_lease_owner: self.runtime_lease_owner,
            edit_file: self.edit_file,
            app_binary: self.app_binary,
            output,
            command: self.command,
        })
    }
}

impl Options {
    fn parse(arguments: Vec<OsString>) -> Result<Self, String> {
        let mut arguments = arguments.into_iter();
        let mode = Mode::parse(
            &arguments
                .next()
                .ok_or_else(|| "missing benchmark mode".to_owned())?,
        )?;
        OptionsBuilder::new(mode).parse(arguments)?.build()
    }

    fn absolute_app_binary(&self) -> PathBuf {
        self.repository.join(&self.app_binary)
    }

    fn log_path(&self) -> PathBuf {
        self.output.with_extension("log")
    }
}

fn parse_number<T>(value: &OsStr, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .to_str()
        .ok_or_else(|| format!("{name} must be valid UTF-8"))?
        .parse()
        .map_err(|_| format!("{name} must be a valid number"))
}

fn os_arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn run_warm(options: &Options, before: &GitSnapshot) -> Result<BenchmarkResult, String> {
    let owner = options.runtime_lease_owner.as_deref().unwrap();
    verify_runtime_lease(&options.repository, owner)?;
    verify_path_lease(&options.repository, &options.edit_file, owner)?;
    ensure_edit_file_clean(&options.repository, &options.edit_file)?;
    ensure_no_existing_app(&options.absolute_app_binary())?;
    let window_probe = compile_window_probe()?;
    prepare_output(options)?;

    let edit_path = options.repository.join(&options.edit_file);
    let mut source = SourceSwap::new(&edit_path)?;
    let mut process = ManagedChild::spawn(options, &[])?;
    let initial_ready = process.wait_for_ready(
        &options.absolute_app_binary(),
        &window_probe,
        None,
        options.timeout,
    )?;
    println!(
        "cinder benchmark: warm-up ready in {:.3}s",
        initial_ready.elapsed.as_secs_f64()
    );

    let mut previous_pid = initial_ready.pid;
    let mut trials = Vec::with_capacity(options.trials);
    let measured = (|| -> Result<(), String> {
        for number in 1..=options.trials {
            let started = source.swap()?;
            let ready = process.wait_for_ready(
                &options.absolute_app_binary(),
                &window_probe,
                Some(previous_pid),
                options.timeout,
            )?;
            let result = process.trial_result(number, started, ready);
            println!(
                "cinder benchmark: warm trial {number}/{} {:.3}s",
                options.trials,
                result.total_ms / 1_000.0
            );
            previous_pid = result.ready_pid;
            trials.push(result);
        }
        Ok(())
    })();

    let restore_started = source.restore()?;
    if let Some(started) = restore_started {
        let ready = process.wait_for_ready(
            &options.absolute_app_binary(),
            &window_probe,
            Some(previous_pid),
            options.timeout,
        )?;
        println!(
            "cinder benchmark: original source restored and running in {:.3}s",
            ready.ready_at.duration_since(started).as_secs_f64()
        );
    }
    process.stop()?;
    measured?;

    let after_restore = GitSnapshot::capture(&options.repository)?;
    if before != &after_restore {
        return Err("source restoration did not recover the exact initial Cap diff".to_owned());
    }

    BenchmarkResult::new(Mode::Warm, trials)
}

fn run_startup(options: &Options, _before: &GitSnapshot) -> Result<BenchmarkResult, String> {
    verify_runtime_lease(
        &options.repository,
        options.runtime_lease_owner.as_deref().unwrap(),
    )?;
    ensure_no_existing_app(&options.absolute_app_binary())?;
    let window_probe = compile_window_probe()?;
    prepare_output(options)?;
    let mut trials = Vec::with_capacity(options.trials);

    for number in 1..=options.trials {
        let mut process = ManagedChild::spawn(options, &[])?;
        let ready = process.wait_for_ready(
            &options.absolute_app_binary(),
            &window_probe,
            None,
            options.timeout,
        )?;
        let result = process.trial_result(number, process.started, ready);
        println!(
            "cinder benchmark: startup trial {number}/{} {:.3}s",
            options.trials,
            result.total_ms / 1_000.0
        );
        trials.push(result);
        process.stop()?;
        thread::sleep(Duration::from_millis(750));
    }

    BenchmarkResult::new(Mode::Startup, trials)
}

fn run_clean(options: &Options, _before: &GitSnapshot) -> Result<BenchmarkResult, String> {
    prepare_output(options)?;
    let mut trials = Vec::with_capacity(options.trials);

    for number in 1..=options.trials {
        let build = IsolatedTarget::new(&options.repository, number)?;
        let environment = vec![(
            OsString::from("CARGO_TARGET_DIR"),
            build.target.as_os_str().to_owned(),
        )];
        let mut process = ManagedChild::spawn(options, &environment)?;
        let status = process.wait_for_exit(options.timeout)?;
        let ended = Instant::now();
        if !status.success() {
            return Err(format!(
                "clean trial {number} failed with status {}",
                display_status(status)
            ));
        }
        build.preserve_timings(&options.output, number)?;
        let result = process.trial_result(
            number,
            process.started,
            ReadyEvent {
                pid: 0,
                elapsed: ended.duration_since(process.started),
                ready_at: ended,
            },
        );
        println!(
            "cinder benchmark: clean trial {number}/{} {:.3}s",
            options.trials,
            result.total_ms / 1_000.0
        );
        trials.push(result);
    }

    BenchmarkResult::new(Mode::Clean, trials)
}

fn prepare_output(options: &Options) -> Result<(), String> {
    if let Some(parent) = options.output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create benchmark output directory: {error}"))?;
    }
    if options.output.exists() || options.log_path().exists() {
        return Err(format!(
            "benchmark output already exists: {}",
            public_output_path(&options.output).display()
        ));
    }
    Ok(())
}

struct SourceSwap {
    path: PathBuf,
    recovery_backup: PathBuf,
    original: Vec<u8>,
    sequence: usize,
    is_modified: bool,
    restored: bool,
}

impl SourceSwap {
    fn new(path: &Path) -> Result<Self, String> {
        let original =
            fs::read(path).map_err(|error| format!("could not read benchmark source: {error}"))?;
        let original_text = std::str::from_utf8(&original)
            .map_err(|_| "benchmark source is not valid UTF-8".to_owned())?;
        let occurrences = original_text.matches(ORIGINAL_EDIT_TOKEN).count();
        if occurrences != 1 || original_text.contains(BENCHMARK_EDIT_PREFIX) {
            return Err(
                "benchmark source must contain exactly one expected token and no alternate token"
                    .to_owned(),
            );
        }
        let recovery_backup = write_private_recovery_backup(&original)?;
        Ok(Self {
            path: path.to_owned(),
            recovery_backup,
            original,
            sequence: 0,
            is_modified: false,
            restored: false,
        })
    }

    fn swap(&mut self) -> Result<Instant, String> {
        self.sequence += 1;
        let original_text = std::str::from_utf8(&self.original).unwrap();
        let replacement = format!("{BENCHMARK_EDIT_PREFIX}{} */\"", self.sequence);
        let contents = original_text
            .replacen(ORIGINAL_EDIT_TOKEN, &replacement, 1)
            .into_bytes();
        write_source(&self.path, &contents)?;
        self.is_modified = true;
        Ok(Instant::now())
    }

    fn restore(&mut self) -> Result<Option<Instant>, String> {
        if self.restored {
            return Ok(None);
        }
        let restored_at = if self.is_modified {
            write_source(&self.path, &self.original)?;
            self.is_modified = false;
            Some(Instant::now())
        } else {
            None
        };
        fs::remove_file(&self.recovery_backup)
            .map_err(|error| format!("could not remove private source recovery backup: {error}"))?;
        self.restored = true;
        Ok(restored_at)
    }
}

impl Drop for SourceSwap {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        if fs::write(&self.path, &self.original).is_ok() {
            self.is_modified = false;
            let _ = fs::remove_file(&self.recovery_backup);
        }
    }
}

fn write_private_recovery_backup(contents: &[u8]) -> Result<PathBuf, String> {
    let root = env::temp_dir().join("cinder").join("benchmark-recovery");
    fs::create_dir_all(&root)
        .map_err(|error| format!("could not create private recovery directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure private recovery directory: {error}"))?;
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_nanos();
    let path = root.join(format!("source-{}-{nonce}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("could not create private source recovery backup: {error}"))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write private source recovery backup: {error}"))?;
    Ok(path)
}

fn write_source(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| format!("could not open benchmark source for editing: {error}"))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not save benchmark source edit: {error}"))
}

struct ManagedChild {
    child: Child,
    root_pid: u32,
    receiver: Receiver<LogEvent>,
    lines: Vec<LogEvent>,
    log: File,
    repository: PathBuf,
    started: Instant,
    stopped: bool,
}

impl ManagedChild {
    fn spawn(options: &Options, environment: &[(OsString, OsString)]) -> Result<Self, String> {
        let executable = options
            .command
            .first()
            .ok_or_else(|| "benchmark command cannot be empty".to_owned())?;
        let mut command = Command::new(executable);
        command
            .args(&options.command[1..])
            .current_dir(&options.repository)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in environment {
            command.env(name, value);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let started = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start benchmark command: {error}"))?;
        let root_pid = child.id();
        let (sender, receiver) = mpsc::channel();
        spawn_log_reader(
            child
                .stdout
                .take()
                .ok_or_else(|| "could not capture child stdout".to_owned())?,
            "stdout",
            sender.clone(),
        );
        spawn_log_reader(
            child
                .stderr
                .take()
                .ok_or_else(|| "could not capture child stderr".to_owned())?,
            "stderr",
            sender,
        );
        let log = open_private_log(&options.log_path())?;

        Ok(Self {
            child,
            root_pid,
            receiver,
            lines: Vec::new(),
            log,
            repository: options.repository.clone(),
            started,
            stopped: false,
        })
    }

    fn wait_for_ready(
        &mut self,
        app_binary: &Path,
        window_probe: &Path,
        previous_pid: Option<u32>,
        timeout: Duration,
    ) -> Result<ReadyEvent, String> {
        let wait_started = Instant::now();
        loop {
            self.drain_logs()?;
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| format!("could not inspect benchmark command: {error}"))?
            {
                self.drain_logs()?;
                return Err(format!(
                    "benchmark command exited before the app was ready: {}",
                    display_status(status)
                ));
            }

            for pid in descendant_app_pids(self.root_pid, app_binary)? {
                if previous_pid == Some(pid) {
                    continue;
                }
                if window_is_visible(window_probe, pid)? {
                    thread::sleep(Duration::from_millis(150));
                    if process_exists(pid) && window_is_visible(window_probe, pid)? {
                        let ready_at = Instant::now();
                        self.drain_logs()?;
                        return Ok(ReadyEvent {
                            pid,
                            elapsed: ready_at.duration_since(self.started),
                            ready_at,
                        });
                    }
                }
            }

            if wait_started.elapsed() > timeout {
                return Err(format!(
                    "timed out after {:.1}s waiting for a new stable cap-desktop window",
                    timeout.as_secs_f64()
                ));
            }
            thread::sleep(Duration::from_millis(75));
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let started = Instant::now();
        loop {
            self.drain_logs()?;
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| format!("could not inspect benchmark command: {error}"))?
            {
                self.drain_logs()?;
                self.stopped = true;
                return Ok(status);
            }
            if started.elapsed() > timeout {
                return Err(format!(
                    "benchmark command timed out after {:.1}s",
                    timeout.as_secs_f64()
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn trial_result(&self, number: usize, started: Instant, ready: ReadyEvent) -> TrialResult {
        let lines: Vec<&LogEvent> = self
            .lines
            .iter()
            .filter(|line| line.at >= started && line.at <= ready.ready_at)
            .collect();
        TrialResult {
            number,
            total_ms: ready.ready_at.duration_since(started).as_secs_f64() * 1_000.0,
            ready_pid: ready.pid,
            cargo_finished_ms: marker_elapsed(&lines, started, cargo_finished_marker),
            app_spawned_ms: marker_elapsed(&lines, started, app_spawned_marker),
            frontend_ready_ms: marker_elapsed(&lines, started, frontend_ready_marker),
        }
    }

    fn drain_logs(&mut self) -> Result<(), String> {
        while let Ok(event) = self.receiver.try_recv() {
            let elapsed = event.at.duration_since(self.started).as_secs_f64();
            let safe_text = sanitize_log_line(&event.text, &self.repository);
            println!("[{elapsed:9.3}] {:>6} | {safe_text}", event.stream);
            writeln!(
                self.log,
                "[{elapsed:9.3}] {:>6} | {safe_text}",
                event.stream
            )
            .map_err(|error| format!("could not write benchmark log: {error}"))?;
            self.lines.push(event);
        }
        self.log
            .flush()
            .map_err(|error| format!("could not flush benchmark log: {error}"))
    }

    fn stop(&mut self) -> Result<(), String> {
        if self.stopped {
            return Ok(());
        }
        let descendants = descendant_pids(self.root_pid)?;
        signal_process_group(self.root_pid, "INT");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            self.drain_logs()?;
            if self
                .child
                .try_wait()
                .map_err(|error| format!("could not inspect benchmark command: {error}"))?
                .is_some()
            {
                terminate_residual_children(&descendants);
                self.stopped = true;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        signal_process_group(self.root_pid, "TERM");
        thread::sleep(Duration::from_secs(2));
        if self
            .child
            .try_wait()
            .map_err(|error| format!("could not inspect benchmark command: {error}"))?
            .is_none()
        {
            signal_process_group(self.root_pid, "KILL");
            self.child
                .wait()
                .map_err(|error| format!("could not reap benchmark command: {error}"))?;
        }
        terminate_residual_children(&descendants);
        self.drain_logs()?;
        self.stopped = true;
        Ok(())
    }
}

fn open_private_log(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("could not open benchmark log: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("could not secure benchmark log: {error}"))?;
    }
    Ok(file)
}

fn sanitize_log_line(line: &str, repository: &Path) -> String {
    let mut safe = line.replace(&*repository.to_string_lossy(), "<repository>");
    if let Some(home) = env::var_os("HOME") {
        safe = safe.replace(&*home.to_string_lossy(), "<home>");
    }
    safe = redact_user_directory(&safe, "/Users/", '/');
    safe = redact_user_directory(&safe, "/home/", '/');

    let lowercase = safe.to_ascii_lowercase();
    let sensitive = [
        "authorization",
        "bearer ",
        "cookie",
        "credential",
        "password",
        "passwd",
        "secret",
        "token",
        "_key",
        "-key",
        "private_key",
        "private-key",
        "api_key",
        "api-key",
        "access_token",
        "access-token",
        "refresh_token",
        "refresh-token",
        "client_secret",
        "client-secret",
    ]
    .iter()
    .any(|needle| lowercase.contains(needle));
    if sensitive || lowercase.contains("://") || contains_email_address(&safe) {
        "[redacted]".to_owned()
    } else {
        safe
    }
}

fn redact_user_directory(value: &str, prefix: &str, separator: char) -> String {
    let mut redacted = value.to_owned();
    while let Some(start) = redacted.find(prefix) {
        let user_start = start + prefix.len();
        let user_length = redacted[user_start..]
            .find(|character: char| character == separator || character.is_whitespace())
            .unwrap_or(redacted.len() - user_start);
        redacted.replace_range(start..user_start + user_length, "<home>");
    }
    redacted
}

fn contains_email_address(value: &str) -> bool {
    value.split_whitespace().any(|word| {
        let candidate = word.trim_matches(|character: char| {
            matches!(
                character,
                '<' | '>' | '(' | ')' | '[' | ']' | ',' | ';' | '"' | '\''
            )
        });
        candidate
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
    })
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct LogEvent {
    at: Instant,
    stream: &'static str,
    text: String,
}

fn spawn_log_reader<R>(reader: R, stream: &'static str, sender: Sender<LogEvent>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(text) => {
                    if sender
                        .send(LogEvent {
                            at: Instant::now(),
                            stream,
                            text,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

#[derive(Clone, Copy)]
struct ReadyEvent {
    pid: u32,
    elapsed: Duration,
    ready_at: Instant,
}

struct TrialResult {
    number: usize,
    total_ms: f64,
    ready_pid: u32,
    cargo_finished_ms: Option<f64>,
    app_spawned_ms: Option<f64>,
    frontend_ready_ms: Option<f64>,
}

struct BenchmarkResult {
    mode: Mode,
    trials: Vec<TrialResult>,
    statistics: Statistics,
}

impl BenchmarkResult {
    fn new(mode: Mode, trials: Vec<TrialResult>) -> Result<Self, String> {
        let statistics = Statistics::from_values(
            &trials
                .iter()
                .map(|trial| trial.total_ms)
                .collect::<Vec<_>>(),
        )?;
        Ok(Self {
            mode,
            trials,
            statistics,
        })
    }

    fn write(&self, options: &Options, snapshot: &GitSnapshot) -> Result<(), String> {
        let environment = environment_facts(&options.repository)?;
        let json = self.render(options, snapshot, &environment);
        write_private_result(&options.output, json.as_bytes()).map_err(|error| {
            format!(
                "could not write benchmark result {}: {error}",
                public_output_path(&options.output).display()
            )
        })?;
        println!(
            "cinder benchmark: median {:.3}s, sample variance {:.6}s², result {}",
            self.statistics.median / 1_000.0,
            self.statistics.sample_variance / 1_000_000.0,
            public_output_path(&options.output).display()
        );
        Ok(())
    }

    fn render(
        &self,
        options: &Options,
        snapshot: &GitSnapshot,
        environment: &[(String, String)],
    ) -> String {
        let mut json = String::new();
        json.push_str("{\n");
        push_json_field(&mut json, "schemaVersion", "2", false);
        push_json_string_field(&mut json, "mode", self.mode.label(), false);
        push_json_bool_field(
            &mut json,
            "gitDirty",
            !snapshot.status.trim().is_empty(),
            false,
        );
        push_json_string_field(
            &mut json,
            "commandClass",
            command_class(&options.command),
            false,
        );
        push_json_field(
            &mut json,
            "commandArgumentCount",
            &options.command.len().saturating_sub(1).to_string(),
            false,
        );
        json.push_str("  \"environment\": {\n");
        for (index, (name, value)) in environment.iter().enumerate() {
            json.push_str("    ");
            push_json_string(&mut json, name);
            json.push_str(": ");
            push_json_string(&mut json, value);
            json.push_str(if index + 1 == environment.len() {
                "\n"
            } else {
                ",\n"
            });
        }
        json.push_str("  },\n");
        json.push_str("  \"trials\": [\n");
        for (index, trial) in self.trials.iter().enumerate() {
            json.push_str("    {");
            write!(
                json,
                "\"number\": {}, \"totalMs\": {:.3}",
                trial.number, trial.total_ms
            )
            .expect("writing JSON to a String cannot fail");
            push_optional_number(&mut json, "cargoFinishedMs", trial.cargo_finished_ms);
            push_optional_number(&mut json, "appSpawnedMs", trial.app_spawned_ms);
            push_optional_number(&mut json, "frontendReadyMs", trial.frontend_ready_ms);
            json.push_str(if index + 1 == self.trials.len() {
                "}\n"
            } else {
                "},\n"
            });
        }
        json.push_str("  ],\n");
        writeln!(
            json,
            "  \"statisticsMs\": {{\"median\": {:.3}, \"mean\": {:.3}, \"sampleVariance\": {:.3}, \"standardDeviation\": {:.3}, \"min\": {:.3}, \"max\": {:.3}}},",
            self.statistics.median,
            self.statistics.mean,
            self.statistics.sample_variance,
            self.statistics.standard_deviation,
            self.statistics.min,
            self.statistics.max
        )
        .expect("writing JSON to a String cannot fail");
        push_json_string_field(
            &mut json,
            "readiness",
            match self.mode {
                Mode::Warm | Mode::Startup => {
                    "selected desktop child process owns a layer-0 macOS application window of at least 100x100 points for two probes 150ms apart, including windows on other Spaces"
                }
                Mode::Clean => {
                    "Cargo command exits successfully in a fresh isolated target directory"
                }
            },
            true,
        );
        json.push_str("}\n");
        json
    }
}

fn command_class(command: &[OsString]) -> &'static str {
    let Some(name) = command
        .first()
        .and_then(|path| Path::new(path).file_name())
        .and_then(OsStr::to_str)
    else {
        return "custom";
    };
    match name {
        "cargo" => "cargo",
        "cinder" => "cinder",
        "pnpm" => "pnpm",
        _ => "custom",
    }
}

fn write_private_result(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())
}

fn public_output_path(path: &Path) -> PathBuf {
    env::current_dir()
        .ok()
        .and_then(|directory| path.strip_prefix(directory).ok().map(Path::to_owned))
        .or_else(|| path.file_name().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("<benchmark-result>"))
}

struct Statistics {
    median: f64,
    mean: f64,
    sample_variance: f64,
    standard_deviation: f64,
    min: f64,
    max: f64,
}

impl Statistics {
    fn from_values(values: &[f64]) -> Result<Self, String> {
        if values.len() < 2 || values.iter().any(|value| !value.is_finite()) {
            return Err("statistics require at least two finite samples".to_owned());
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let median = if sorted.len() % 2 == 0 {
            let middle = sorted.len() / 2;
            f64::midpoint(sorted[middle - 1], sorted[middle])
        } else {
            sorted[sorted.len() / 2]
        };
        let sample_count = u32::try_from(values.len())
            .map_err(|_| "statistics support at most 4,294,967,295 samples".to_owned())?;
        let mean = values.iter().sum::<f64>() / f64::from(sample_count);
        let sample_variance = values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / f64::from(sample_count - 1);
        Ok(Self {
            median,
            mean,
            sample_variance,
            standard_deviation: sample_variance.sqrt(),
            min: sorted[0],
            max: sorted[sorted.len() - 1],
        })
    }
}

#[derive(PartialEq, Eq)]
struct GitSnapshot {
    head: String,
    status: String,
    diff_object: String,
}

impl GitSnapshot {
    fn capture(repository: &Path) -> Result<Self, String> {
        Ok(Self {
            head: command_output(repository, "git", &["rev-parse", "HEAD"])?
                .trim()
                .to_owned(),
            status: command_output(
                repository,
                "git",
                &["status", "--porcelain=v2", "--untracked-files=all"],
            )?,
            diff_object: working_diff_object(repository)?,
        })
    }
}

fn working_diff_object(repository: &Path) -> Result<String, String> {
    let diff = Command::new("git")
        .args(["diff", "--binary", "--no-ext-diff"])
        .current_dir(repository)
        .output()
        .map_err(|error| format!("could not read the working diff: {error}"))?;
    if !diff.status.success() {
        return Err("git diff failed".to_owned());
    }
    let mut hash = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not hash the working diff: {error}"))?;
    hash.stdin
        .take()
        .unwrap()
        .write_all(&diff.stdout)
        .map_err(|error| format!("could not send the working diff to git: {error}"))?;
    let output = hash
        .wait_with_output()
        .map_err(|error| format!("could not finish hashing the working diff: {error}"))?;
    if !output.status.success() {
        return Err("git hash-object failed".to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

struct IsolatedTarget {
    root: PathBuf,
    target: PathBuf,
}

impl IsolatedTarget {
    fn new(repository: &Path, trial: usize) -> Result<Self, String> {
        let root = env::temp_dir().join(format!(
            "cinder-cap-clean-{}-{}-{trial}",
            std::process::id(),
            epoch_seconds()?
        ));
        let target = root.join("target");
        fs::create_dir_all(&target)
            .map_err(|error| format!("could not create isolated target: {error}"))?;
        let native_dependencies = repository.join("target/native-deps");
        if native_dependencies.is_dir() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&native_dependencies, target.join("native-deps"))
                .map_err(|error| format!("could not link prepared native dependencies: {error}"))?;
        }
        Ok(Self { root, target })
    }

    fn preserve_timings(&self, output: &Path, trial: usize) -> Result<(), String> {
        let timings = self.target.join("cargo-timings/cargo-timing.html");
        if !timings.is_file() {
            return Ok(());
        }
        let destination = output.with_file_name(format!(
            "{}-timing-{trial}.html",
            output.file_stem().unwrap_or_default().to_string_lossy()
        ));
        fs::copy(&timings, &destination)
            .map_err(|error| format!("could not preserve Cargo timings: {error}"))?;
        Ok(())
    }
}

impl Drop for IsolatedTarget {
    fn drop(&mut self) {
        let expected_prefix = env::temp_dir().join("cinder-cap-clean-");
        if self.root.parent() == expected_prefix.parent()
            && self
                .root
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("cinder-cap-clean-"))
        {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn ensure_git_root(repository: &Path) -> Result<(), String> {
    let root = command_output(repository, "git", &["rev-parse", "--show-toplevel"])?;
    let root = fs::canonicalize(root.trim())
        .map_err(|error| format!("could not resolve Git root: {error}"))?;
    if root != repository {
        return Err("--repo must name the Git root".to_owned());
    }
    Ok(())
}

fn ensure_edit_file_clean(repository: &Path, relative: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--"])
        .arg(relative)
        .current_dir(repository)
        .output()
        .map_err(|error| format!("could not inspect benchmark edit file: {error}"))?;
    if !output.status.success() {
        return Err("git status failed for benchmark edit file".to_owned());
    }
    if !output.stdout.is_empty() {
        return Err("benchmark edit file is already dirty".to_owned());
    }
    Ok(())
}

fn verify_runtime_lease(repository: &Path, expected_owner: &str) -> Result<(), String> {
    let common = git_common_directory(repository)?;
    let owner_path = common.join("cinder/leases/global/desktop-macos-runtime/owner");
    let owner = fs::read_to_string(&owner_path)
        .map_err(|error| format!("could not verify desktop runtime lease: {error}"))?;
    if owner.trim() != expected_owner {
        return Err("desktop runtime lease belongs to another session".to_owned());
    }
    Ok(())
}

fn verify_path_lease(
    repository: &Path,
    relative: &Path,
    expected_owner: &str,
) -> Result<(), String> {
    let relative_text = relative
        .to_str()
        .ok_or_else(|| "benchmark edit path must be valid UTF-8".to_owned())?;
    let hash = sha256_text(relative_text)?;
    let owner_path = git_common_directory(repository)?
        .join("cinder/leases/paths")
        .join(hash)
        .join("owner");
    let owner = fs::read_to_string(&owner_path)
        .map_err(|error| format!("could not verify benchmark source path lease: {error}"))?;
    if owner.trim() != expected_owner {
        return Err("benchmark source path lease belongs to another session".to_owned());
    }
    Ok(())
}

fn sha256_text(text: &str) -> Result<String, String> {
    let mut child = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start shasum: {error}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .map_err(|error| format!("could not write to shasum: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("could not finish shasum: {error}"))?;
    if !output.status.success() {
        return Err("shasum failed".to_owned());
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "shasum returned no hash".to_owned())
}

fn git_common_directory(repository: &Path) -> Result<PathBuf, String> {
    let common = command_output(repository, "git", &["rev-parse", "--git-common-dir"])?;
    let common = PathBuf::from(common.trim());
    let common = if common.is_absolute() {
        common
    } else {
        repository.join(common)
    };
    fs::canonicalize(&common)
        .map_err(|error| format!("could not resolve Git common directory: {error}"))
}

fn compile_window_probe() -> Result<PathBuf, String> {
    if !cfg!(target_os = "macos") {
        return Err("visible-window readiness is currently supported only on macOS".to_owned());
    }
    let root = env::temp_dir().join("cinder").join("window-probe-v1");
    fs::create_dir_all(&root)
        .map_err(|error| format!("could not create window probe directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure window probe directory: {error}"))?;
    }
    let source = root.join("window_probe.swift");
    let output = root.join("window-probe");
    let rebuild = fs::read_to_string(&source).ok().as_deref() != Some(WINDOW_PROBE_SOURCE)
        || !output.is_file();
    if rebuild {
        fs::write(&source, WINDOW_PROBE_SOURCE)
            .map_err(|error| format!("could not stage window probe source: {error}"))?;
        let status = Command::new("xcrun")
            .args(["swiftc", "-O"])
            .arg(&source)
            .arg("-o")
            .arg(&output)
            .status()
            .map_err(|error| format!("could not compile the window probe: {error}"))?;
        if !status.success() {
            return Err(format!(
                "window probe compilation failed with {}",
                display_status(status)
            ));
        }
    }
    Ok(output)
}

fn window_is_visible(probe: &Path, pid: u32) -> Result<bool, String> {
    let status = Command::new(probe)
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("could not run window probe: {error}"))?;
    Ok(status.success())
}

fn ensure_no_existing_app(app_binary: &Path) -> Result<(), String> {
    let expected = app_binary.to_string_lossy();
    let matches: Vec<_> = process_table()?
        .into_iter()
        .filter(|process| {
            process.pid != std::process::id() && process.command.contains(expected.as_ref())
        })
        .collect();
    if matches.is_empty() {
        return Ok(());
    }
    Err(format!(
        "benchmark app process is already running ({} match(es))",
        matches.len()
    ))
}

struct ProcessInfo {
    pid: u32,
    parent: u32,
    command: String,
}

fn process_table() -> Result<Vec<ProcessInfo>, String> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .map_err(|error| format!("could not inspect processes: {error}"))?;
    if !output.status.success() {
        return Err("ps failed".to_owned());
    }
    let mut processes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let Some(pid) = fields.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        let Some(parent) = fields.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        processes.push(ProcessInfo {
            pid,
            parent,
            command: fields.collect::<Vec<_>>().join(" "),
        });
    }
    Ok(processes)
}

fn descendant_pids(root: u32) -> Result<Vec<u32>, String> {
    let processes = process_table()?;
    let mut descendants = HashSet::from([root]);
    loop {
        let before = descendants.len();
        for process in &processes {
            if descendants.contains(&process.parent) {
                descendants.insert(process.pid);
            }
        }
        if before == descendants.len() {
            break;
        }
    }
    descendants.remove(&root);
    Ok(descendants.into_iter().collect())
}

fn descendant_app_pids(root: u32, app_binary: &Path) -> Result<Vec<u32>, String> {
    let processes = process_table()?;
    let by_parent: HashMap<u32, Vec<u32>> = processes.iter().fold(
        HashMap::new(),
        |mut parents: HashMap<u32, Vec<u32>>, process| {
            parents.entry(process.parent).or_default().push(process.pid);
            parents
        },
    );
    let mut descendants = HashSet::from([root]);
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        if let Some(children) = by_parent.get(&parent) {
            for child in children {
                if descendants.insert(*child) {
                    pending.push(*child);
                }
            }
        }
    }
    let expected = app_binary.to_string_lossy();
    let file_name = app_binary.file_name().unwrap_or_default().to_string_lossy();
    let mut matches: Vec<u32> = processes
        .iter()
        .filter(|process| {
            descendants.contains(&process.pid)
                && (process.command.contains(expected.as_ref())
                    || process.command.ends_with(file_name.as_ref()))
        })
        .map(|process| process.pid)
        .collect();
    matches.sort_unstable();
    matches.reverse();
    Ok(matches)
}

fn process_exists(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn signal_process_group(root: u32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([format!("-{signal}"), format!("-{root}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn terminate_residual_children(pids: &[u32]) {
    for pid in pids {
        if process_exists(*pid) {
            let _ = Command::new("/bin/kill")
                .args(["-TERM", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn marker_elapsed(
    lines: &[&LogEvent],
    started: Instant,
    predicate: fn(&str) -> bool,
) -> Option<f64> {
    lines
        .iter()
        .find(|line| predicate(&line.text))
        .map(|line| line.at.duration_since(started).as_secs_f64() * 1_000.0)
}

fn cargo_finished_marker(line: &str) -> bool {
    line.contains("Finished") && line.contains("`dev` profile")
}

fn app_spawned_marker(line: &str) -> bool {
    line.contains("target/debug/cap-desktop`")
}

fn frontend_ready_marker(line: &str) -> bool {
    (line.contains("ready in") || line.contains("Local:")) && line.contains("3002")
}

fn environment_facts(repository: &Path) -> Result<Vec<(String, String)>, String> {
    let rustc = first_line(&command_output(repository, "rustc", &["-Vv"])?);
    let cargo = first_line(&command_output(repository, "cargo", &["-Vv"])?);
    let os_version = command_output(repository, "sw_vers", &["-productVersion"])?;
    let os_build = command_output(repository, "sw_vers", &["-buildVersion"])?;
    let architecture = command_output(repository, "uname", &["-m"])?;
    let power = command_output(repository, "pmset", &["-g", "batt"])?;
    let thermal = command_output(repository, "pmset", &["-g", "therm"])?;
    let date = command_output(repository, "date", &["-u", "+%Y-%m-%d"])?;
    Ok(vec![
        ("rustc".to_owned(), rustc),
        ("cargo".to_owned(), cargo),
        (
            "operatingSystem".to_owned(),
            format!("macOS {} ({})", os_version.trim(), os_build.trim()),
        ),
        ("architecture".to_owned(), architecture.trim().to_owned()),
        ("powerSource".to_owned(), power_source(&power).to_owned()),
        (
            "thermalState".to_owned(),
            thermal_state(&thermal).to_owned(),
        ),
        ("date".to_owned(), date.trim().to_owned()),
    ])
}

fn first_line(value: &str) -> String {
    value.lines().next().unwrap_or_default().trim().to_owned()
}

fn power_source(value: &str) -> &'static str {
    let lowercase = value.to_ascii_lowercase();
    if lowercase.contains("ac power") {
        "AC"
    } else if lowercase.contains("battery power") {
        "battery"
    } else {
        "unknown"
    }
}

fn thermal_state(value: &str) -> &'static str {
    if value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| line.to_ascii_lowercase().contains("no "))
    {
        "normal"
    } else {
        "warning"
    }
}

fn command_output(repository: &Path, program: &str, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(repository)
        .output()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed with {}",
            display_status(output.status)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn display_status(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "signal".to_owned(), |code| code.to_string())
}

fn epoch_seconds() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                write!(output, "\\u{:04x}", character as u32)
                    .expect("writing JSON to a String cannot fail");
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

fn push_json_field(output: &mut String, name: &str, value: &str, last: bool) {
    output.push_str("  ");
    push_json_string(output, name);
    output.push_str(": ");
    output.push_str(value);
    output.push_str(if last { "\n" } else { ",\n" });
}

fn push_json_string_field(output: &mut String, name: &str, value: &str, last: bool) {
    output.push_str("  ");
    push_json_string(output, name);
    output.push_str(": ");
    push_json_string(output, value);
    output.push_str(if last { "\n" } else { ",\n" });
}

fn push_json_bool_field(output: &mut String, name: &str, value: bool, last: bool) {
    push_json_field(output, name, if value { "true" } else { "false" }, last);
}

fn push_optional_number(output: &mut String, name: &str, value: Option<f64>) {
    output.push_str(", \"");
    output.push_str(name);
    output.push_str("\": ");
    match value {
        Some(value) => {
            write!(output, "{value:.3}").expect("writing JSON to a String cannot fail");
        }
        None => output.push_str("null"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BenchmarkResult, GitSnapshot, Mode, ORIGINAL_EDIT_TOKEN, Options, SourceSwap, Statistics,
        TrialResult, power_source, sanitize_log_line, thermal_state,
    };
    use std::{
        ffi::OsString,
        fs,
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn calculates_median_and_sample_variance() {
        let statistics = Statistics::from_values(&[1.0, 2.0, 3.0, 10.0]).unwrap();
        assert!((statistics.median - 2.5).abs() < f64::EPSILON);
        assert!((statistics.mean - 4.0).abs() < f64::EPSILON);
        assert!((statistics.sample_variance - 16.666_666_666_7).abs() < 0.000_001);
        assert!((statistics.min - 1.0).abs() < f64::EPSILON);
        assert!((statistics.max - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn publishable_results_exclude_private_benchmark_context() {
        let separator = std::path::MAIN_SEPARATOR;
        let private_root = PathBuf::from(format!(
            "{separator}Users{separator}example{separator}private-project"
        ));
        let example_email = format!("customer{}example.com", '@');
        let options = Options {
            mode: Mode::Warm,
            repository: private_root.clone(),
            trials: 2,
            timeout: Duration::from_secs(1),
            runtime_lease_owner: Some("private-session-owner".to_owned()),
            edit_file: PathBuf::from("src/private-module.rs"),
            app_binary: PathBuf::from("target/debug/private-app"),
            output: PathBuf::from("result.json"),
            command: ["cargo", "run", "--access-token", "DO_NOT_PERSIST"]
                .map(OsString::from)
                .to_vec(),
        };
        let snapshot = GitSnapshot {
            head: "aaaaaaaaaaaaaaaa".to_owned(),
            status: format!("1 .M src/customer-name.rs\n? {example_email}\n"),
            diff_object: "bbbbbbbbbbbbbbbb".to_owned(),
        };
        let result = BenchmarkResult::new(
            Mode::Warm,
            vec![
                TrialResult {
                    number: 1,
                    total_ms: 10.0,
                    ready_pid: 41_007,
                    cargo_finished_ms: Some(4.0),
                    app_spawned_ms: None,
                    frontend_ready_ms: None,
                },
                TrialResult {
                    number: 2,
                    total_ms: 20.0,
                    ready_pid: 52_008,
                    cargo_finished_ms: Some(8.0),
                    app_spawned_ms: None,
                    frontend_ready_ms: None,
                },
            ],
        )
        .unwrap();
        let json = result.render(
            &options,
            &snapshot,
            &[("powerSource".to_owned(), "AC".to_owned())],
        );

        for private_value in [
            private_root.to_string_lossy().as_ref(),
            "private-session-owner",
            "src/private-module.rs",
            "target/debug/private-app",
            "DO_NOT_PERSIST",
            &example_email,
            "src/customer-name.rs",
            "aaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbb",
        ] {
            assert!(!json.contains(private_value), "leaked {private_value:?}");
        }
        assert!(json.contains("\"schemaVersion\": 2"));
        assert!(json.contains("\"commandClass\": \"cargo\""));
        assert!(json.contains("\"commandArgumentCount\": 3"));
        assert!(json.contains("\"gitDirty\": true"));
        assert!(!json.contains("repository"));
        assert!(!json.contains("\"head\""));
        assert!(!json.contains("workingDiffObject"));
        assert!(!json.contains("gitStatus"));
        assert!(!json.contains("readyPid"));
    }

    #[test]
    fn benchmark_logs_redact_paths_credentials_urls_and_email_addresses() {
        let separator = std::path::MAIN_SEPARATOR;
        let repository = PathBuf::from(format!(
            "{separator}Users{separator}example{separator}private-project"
        ));
        let other_home = format!("{separator}home{separator}other{separator}cache");
        let path_line = format!("Finished {}/target and {other_home}", repository.display());
        assert_eq!(
            sanitize_log_line(&path_line, &repository),
            "Finished <repository>/target and <home>/cache"
        );
        assert_eq!(
            sanitize_log_line("Authorization: Bearer DO_NOT_PERSIST", &repository),
            "[redacted]"
        );
        let local_url = ["Local: https", "://", "localhost:3002"].concat();
        assert_eq!(sanitize_log_line(&local_url, &repository), "[redacted]");
        let example_email = format!("customer{}example.com", '@');
        assert_eq!(
            sanitize_log_line(&format!("contact {example_email}"), &repository),
            "[redacted]"
        );
    }

    #[test]
    fn environment_metadata_reduces_machine_state_to_allowlisted_categories() {
        assert_eq!(
            power_source("Now drawing from 'AC Power'\n battery id 42"),
            "AC"
        );
        assert_eq!(power_source("Now drawing from 'Battery Power'"), "battery");
        assert_eq!(
            thermal_state("Note: No thermal warning level has been recorded"),
            "normal"
        );
        assert_eq!(thermal_state("CPU_Speed_Limit = 50"), "warning");
    }

    #[test]
    fn source_recovery_backup_is_private_and_removed_after_restoration() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cinder-benchmark-recovery-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.rs");
        let original = format!("const SCRIPT: &str = {ORIGINAL_EDIT_TOKEN};\n");
        fs::write(&source, &original).unwrap();

        let mut swap = SourceSwap::new(&source).unwrap();
        let backup = swap.recovery_backup.clone();
        assert!(backup.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        swap.swap().unwrap();
        assert_ne!(fs::read_to_string(&source).unwrap(), original);
        assert!(swap.restore().unwrap().is_some());
        drop(swap);

        assert_eq!(fs::read_to_string(&source).unwrap(), original);
        assert!(!backup.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
