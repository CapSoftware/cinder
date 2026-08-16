//! Cinder-tuned toolchain routing with automatic stock fallback.
//!
//! When a development command is eligible, Cinder routes the Cargo child
//! through the locally built `cinder-tuned` rustup toolchain — the same
//! rustc source as the active stock toolchain, rebuilt with ThinLTO, PGO,
//! and the parallel frontend — inside a separate artifact namespace
//! (`target/cinder-tuned`). Tuned artifacts are functionally equivalent but
//! not byte-identical to stock Cargo's, so they must never share a target
//! directory with stock builds, and anything that expresses an explicit
//! toolchain or target-directory choice keeps the stock path untouched.
//!
//! Routing is applied by mutating this process's environment before the
//! command context is computed, so context identity, fast-path state, the
//! capture pipeline, detached recorders, and hidden diagnostic passes all
//! observe one consistent world and tuned state can never alias stock state.

use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};

const TUNED_TOOLCHAIN: &str = "cinder-tuned";
const PROBE_MAGIC: &[u8; 8] = b"CNDT0001";
const MAX_PROBE_BYTES: u64 = 4096;
const FLAGS_FILE: &str = "cinder-tuned-flags";
const MAX_FLAGS_BYTES: u64 = 4096;
const MAX_FLAGS_TOKENS: usize = 32;
pub const STOCK_ENVIRONMENT: &str = "CINDER_STOCK";
pub const BACKEND_ENVIRONMENT: &str = "CINDER_TUNED_BACKEND";
const MACHINE_MARKER: &str = "tuned-disabled";
const PROJECT_MARKER: &str = "tuned-ineligible";
const HOST_TRIPLE: &str = "aarch64-apple-darwin";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    Stock,
    Tuned,
}

/// Decides and applies tuned routing for one command invocation. Must run
/// before the command context is computed and before any thread is spawned:
/// it mutates the process environment on the tuned path.
pub fn apply_routing(arguments: &[OsString]) -> Routing {
    if !cfg!(target_os = "macos") {
        return Routing::Stock;
    }
    match eligible_invocation(arguments) {
        Some((directory, pin)) => match tuned_toolchain_for(&directory, pin.as_deref()) {
            Some(tuned) => {
                apply_environment(&tuned, &directory);
                Routing::Tuned
            }
            None => Routing::Stock,
        },
        None => Routing::Stock,
    }
}

/// Returns the canonical invocation directory, plus the project's plain
/// version pin when one applies, when every eligibility gate passes; `None`
/// for any shape that must stay exactly stock.
fn eligible_invocation(arguments: &[OsString]) -> Option<(PathBuf, Option<String>)> {
    if env::var_os(STOCK_ENVIRONMENT).is_some()
        || env::var_os("RUSTUP_TOOLCHAIN").is_some()
        || env::var_os("CARGO_TARGET_DIR").is_some()
        || env::var_os("CARGO_BUILD_TARGET_DIR").is_some()
        || env::var_os("RUSTC").is_some()
        || env::var_os("RUSTDOC").is_some()
        || env::var_os("RUSTC_BOOTSTRAP").is_some()
        || env::var_os("RUSTC_WRAPPER").is_some()
        || env::var_os("RUSTC_WORKSPACE_WRAPPER").is_some()
    {
        return None;
    }
    if !matches!(
        crate::run::cargo_subcommand(arguments),
        Some("build" | "b" | "check" | "c" | "test" | "t" | "run" | "r")
    ) {
        return None;
    }
    let mut before_delimiter = arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--");
    if before_delimiter.any(|argument| {
        let Some(argument) = argument.to_str() else {
            return true;
        };
        argument.starts_with('+')
            || argument == "--release"
            || argument == "-r"
            || argument == "--profile"
            || argument.starts_with("--profile=")
            || argument == "--target"
            || argument.starts_with("--target=")
            || argument == "--target-dir"
            || argument.starts_with("--target-dir=")
            || argument == "--config"
            || argument.starts_with("--config=")
            || argument == "-Z"
            || argument.starts_with("-Z")
    }) {
        return None;
    }
    let directory = env::current_dir().ok()?;
    let directory = fs::canonicalize(directory).ok()?;
    let pin = match project_pin(&directory) {
        ProjectPin::None => None,
        ProjectPin::PlainVersion(version) => Some(version),
        // Channel names that are not plain versions (nightly dates, `stable`,
        // full toolchain names) and unreadable or unrecognized pin files
        // always keep the project's own toolchain choice, exactly stock.
        ProjectPin::Other => return None,
    };
    if machine_marker_path().is_file() || project_marker_path(&directory).is_file() {
        return None;
    }
    Some((directory, pin))
}

enum ProjectPin {
    None,
    PlainVersion(String),
    Other,
}

/// Reads the nearest `rust-toolchain` file above the invocation directory —
/// the same nearest-wins resolution rustup applies — and classifies its
/// channel. Only a plain `major.minor` or `major.minor.patch` version can
/// ever match a per-pin tuned build.
fn project_pin(directory: &Path) -> ProjectPin {
    for ancestor in directory.ancestors() {
        let toml = ancestor.join("rust-toolchain.toml");
        if toml.is_file() {
            return classify_pin(pin_channel_from_toml(&toml));
        }
        let legacy = ancestor.join("rust-toolchain");
        if legacy.is_file() {
            let channel = fs::read_to_string(&legacy)
                .ok()
                .map(|contents| contents.trim().to_owned());
            // A legacy file may itself hold TOML; treat that shape as TOML.
            if let Some(channel) = &channel {
                if channel.contains("[toolchain]") {
                    return classify_pin(pin_channel_from_toml(&legacy));
                }
            }
            return classify_pin(channel);
        }
    }
    ProjectPin::None
}

fn pin_channel_from_toml(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let table = toml::from_str::<toml::Table>(&contents).ok()?;
    table
        .get("toolchain")?
        .as_table()?
        .get("channel")?
        .as_str()
        .map(str::to_owned)
}

fn classify_pin(channel: Option<String>) -> ProjectPin {
    match channel {
        Some(channel) if is_plain_version(&channel) => ProjectPin::PlainVersion(channel),
        _ => ProjectPin::Other,
    }
}

/// `1.88` and `1.88.0` are plain versions; anything else is not.
fn is_plain_version(channel: &str) -> bool {
    let components: Vec<&str> = channel.split('.').collect();
    matches!(components.len(), 2 | 3)
        && components
            .iter()
            .all(|component| !component.is_empty() && component.bytes().all(|b| b.is_ascii_digit()))
}

struct TunedToolchain {
    rustc: PathBuf,
    rustdoc: PathBuf,
    flags: Vec<String>,
}

/// The stock compiler a tuned candidate must extend: the rustup shim's
/// default for unpinned projects, or the installed pinned toolchain's own
/// binary for pinned ones. A pinned toolchain that is not installed keeps
/// the project stock — Cinder never triggers a rustup download.
enum StockReference {
    PathShim,
    Pinned(PathBuf),
}

impl StockReference {
    fn version_rustc(&self) -> &Path {
        match self {
            Self::PathShim => Path::new("rustc"),
            Self::Pinned(rustc) => rustc,
        }
    }

    fn identity_path(&self) -> Option<PathBuf> {
        match self {
            Self::PathShim => which_on_path("rustc"),
            Self::Pinned(rustc) => Some(rustc.clone()),
        }
    }
}

/// Locates and health-checks the tuned toolchain matching the project — the
/// default `cinder-tuned` for unpinned projects, or `cinder-tuned-<version>`
/// for a plain version pin — caching the verdict per tuned toolchain by the
/// filesystem identities of the tuned compiler, the stock reference, and the
/// rustup settings file, so an unchanged installation costs a few stats and
/// one small read per command.
fn tuned_toolchain_for(directory: &Path, pin: Option<&str>) -> Option<TunedToolchain> {
    let toolchains = rustup_home()?.join("toolchains");
    let (name, root, stock) = match pin {
        None => {
            let root = toolchains.join(TUNED_TOOLCHAIN);
            (TUNED_TOOLCHAIN.to_owned(), root, StockReference::PathShim)
        }
        Some(pin) => {
            let (name, root) = tuned_candidate_for_pin(&toolchains, pin)?;
            let stock = installed_pinned_rustc(&toolchains, pin)?;
            (name, root, StockReference::Pinned(stock))
        }
    };
    let rustc = root.join("bin/rustc");
    let rustdoc = root.join("bin/rustdoc");
    if !rustc.is_file() {
        return None;
    }
    let flags = read_flags_file(&root)?;
    let key = probe_key(&rustc, &stock)?;
    let verdict = match read_probe(&name, &key) {
        Some(verdict) => verdict,
        None => {
            let verdict = probe_toolchain(&rustc, &stock);
            let _ = write_probe(&name, &key, verdict);
            verdict
        }
    };
    if verdict {
        Some(TunedToolchain {
            rustc,
            rustdoc,
            flags,
        })
    } else {
        let _ = directory; // project identity reserved for future use
        None
    }
}

/// A pin `1.88.0` matches `cinder-tuned-1.88.0` or `cinder-tuned-1.88`; a
/// pin `1.88` matches `cinder-tuned-1.88`. The first existing candidate wins.
fn tuned_candidate_for_pin(toolchains: &Path, pin: &str) -> Option<(String, PathBuf)> {
    let mut candidates = vec![format!("{TUNED_TOOLCHAIN}-{pin}")];
    let major_minor: Vec<&str> = pin.split('.').take(2).collect();
    if major_minor.len() == 2 {
        let short = format!("{TUNED_TOOLCHAIN}-{}", major_minor.join("."));
        if !candidates.contains(&short) {
            candidates.push(short);
        }
    }
    candidates.into_iter().find_map(|name| {
        let root = toolchains.join(&name);
        root.join("bin/rustc").is_file().then_some((name, root))
    })
}

/// Resolves the installed stock toolchain for a plain version pin without
/// ever installing anything: the exact `<pin>-<host>` directory, or for a
/// two-component pin, the highest installed `<pin>.<patch>-<host>`.
fn installed_pinned_rustc(toolchains: &Path, pin: &str) -> Option<PathBuf> {
    let exact = toolchains.join(format!("{pin}-{HOST_TRIPLE}"));
    if exact.join("bin/rustc").is_file() {
        return Some(exact.join("bin/rustc"));
    }
    if pin.split('.').count() != 2 {
        return None;
    }
    let prefix = format!("{pin}.");
    let suffix = format!("-{HOST_TRIPLE}");
    let mut best: Option<String> = None;
    for entry in fs::read_dir(toolchains).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix)
            && name.ends_with(&suffix)
            && best
                .as_deref()
                .is_none_or(|current| name.as_str() > current)
        {
            best = Some(name);
        }
    }
    let rustc = toolchains.join(best?).join("bin/rustc");
    rustc.is_file().then_some(rustc)
}

/// Reads the tuned toolchain's optional flags file. An absent or empty file
/// means no extra compiler flags; a present file must be small, UTF-8, and
/// hold only plausible flag tokens, or the tuned toolchain is ineligible.
fn read_flags_file(toolchain_root: &Path) -> Option<Vec<String>> {
    let path = toolchain_root.join(FLAGS_FILE);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    if !metadata.is_file() || metadata.len() > MAX_FLAGS_BYTES {
        return None;
    }
    let contents = fs::read_to_string(&path).ok()?;
    parse_flags(&contents)
}

fn parse_flags(contents: &str) -> Option<Vec<String>> {
    let tokens: Vec<String> = contents.split_whitespace().map(str::to_owned).collect();
    if tokens.len() > MAX_FLAGS_TOKENS
        || tokens.iter().any(|token| {
            !token.starts_with('-')
                || token.chars().any(|character| {
                    character.is_control() || character == '"' || character == '\''
                })
        })
    {
        return None;
    }
    Some(tokens)
}

fn rustup_home() -> Option<PathBuf> {
    if let Some(home) = env::var_os("RUSTUP_HOME") {
        return Some(PathBuf::from(home));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup"))
}

/// Identity bytes for the probe cache: any change to the tuned compiler, the
/// stock reference compiler, or rustup's settings (default toolchain) forces
/// a re-probe.
fn probe_key(tuned_rustc: &Path, stock: &StockReference) -> Option<Vec<u8>> {
    let mut key = Vec::new();
    for path in [
        tuned_rustc.to_path_buf(),
        stock.identity_path()?,
        rustup_home()?.join("settings.toml"),
    ] {
        let metadata = fs::metadata(&path).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        key.extend_from_slice(path.as_os_str().as_encoded_bytes());
        key.extend_from_slice(&metadata.len().to_le_bytes());
        key.extend_from_slice(&modified.to_le_bytes());
    }
    Some(key)
}

fn which_on_path(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// The health probe proves the same-source guarantee and a working sysroot:
/// the tuned `--version` line must contain the stock reference's full version
/// identity, and the tuned compiler must compile a trivial crate. A failed
/// probe is cached as a negative verdict rather than retried on every
/// command.
fn probe_toolchain(tuned_rustc: &Path, stock: &StockReference) -> bool {
    let stock = version_output(stock.version_rustc());
    let tuned = version_output(tuned_rustc);
    let (Some(stock), Some(tuned)) = (stock, tuned) else {
        return false;
    };
    let (Some(stock_line), Some(tuned_line)) = (stock.lines().next(), tuned.lines().next()) else {
        return false;
    };
    // The stock line is `rustc 1.88.0 (hash date)`; the tuned line appends
    // its own description as a further parenthesis group, so the stock line
    // appears verbatim inside it.
    if !tuned_line.contains(stock_line.trim()) {
        return false;
    }
    let probe_directory = env::temp_dir().join("cinder").join("toolchain");
    if fs::create_dir_all(&probe_directory).is_err() {
        return false;
    }
    let source = probe_directory.join("probe.rs");
    let output = probe_directory.join(format!("probe-{}.rmeta", std::process::id()));
    if fs::write(&source, "pub fn cinder_probe() {}\n").is_err() {
        return false;
    }
    let compiled = Command::new(tuned_rustc)
        .arg("--edition=2021")
        .arg("--crate-type=lib")
        .arg("--emit=metadata")
        .arg("-o")
        .arg(&output)
        .arg(&source)
        .output()
        .is_ok_and(|result| result.status.success());
    let _ = fs::remove_file(output);
    compiled
}

fn version_output(rustc: &Path) -> Option<String> {
    let output = Command::new(rustc).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// One verdict file per tuned toolchain, so multiple tuned builds coexist
/// without evicting one another's probe results.
fn probe_cache_path(toolchain_name: &str) -> Option<PathBuf> {
    if toolchain_name.is_empty()
        || toolchain_name.len() > 64
        || !toolchain_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '.'))
    {
        return None;
    }
    let directory = env::temp_dir().join("cinder").join("toolchain");
    fs::create_dir_all(&directory).ok()?;
    Some(directory.join(format!("probe-cache-{toolchain_name}")))
}

fn read_probe(toolchain_name: &str, key: &[u8]) -> Option<bool> {
    let path = probe_cache_path(toolchain_name)?;
    let metadata = fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_PROBE_BYTES {
        return None;
    }
    let contents = fs::read(&path).ok()?;
    let rest = contents.strip_prefix(PROBE_MAGIC.as_slice())?;
    let (&verdict, rest) = rest.split_first()?;
    if rest != key {
        return None;
    }
    match verdict {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn write_probe(toolchain_name: &str, key: &[u8], verdict: bool) -> io::Result<()> {
    let Some(path) = probe_cache_path(toolchain_name) else {
        return Ok(());
    };
    let mut contents = Vec::with_capacity(PROBE_MAGIC.len() + 1 + key.len());
    contents.extend_from_slice(PROBE_MAGIC);
    contents.push(u8::from(verdict));
    contents.extend_from_slice(key);
    fs::write(path, contents)
}

fn machine_marker_path() -> PathBuf {
    env::temp_dir()
        .join("cinder")
        .join("toolchain")
        .join(MACHINE_MARKER)
}

fn project_marker_path(directory: &Path) -> PathBuf {
    crate::run::project_state_directory(directory).join(PROJECT_MARKER)
}

/// Applies tuned routing by mutating this process's environment. Called from
/// single-threaded startup only, before the context hash is computed and
/// before any capture, validation, or recorder machinery runs; every later
/// child inherits exactly this world.
fn apply_environment(tuned: &TunedToolchain, directory: &Path) {
    let mut applied: Vec<&str> = tuned.flags.iter().map(String::as_str).collect();
    if env::var_os(BACKEND_ENVIRONMENT).is_some_and(|backend| backend == "cranelift") {
        applied.push("-Zcodegen-backend=cranelift");
    }
    let needs_bootstrap = applied.iter().any(|flag| flag.starts_with("-Z"));
    let rustflags = if applied.is_empty() {
        None
    } else {
        let mut rustflags = env::var_os("RUSTFLAGS").unwrap_or_default();
        for flag in &applied {
            if !rustflags.is_empty() {
                rustflags.push(" ");
            }
            rustflags.push(flag);
        }
        Some(rustflags)
    };
    // SAFETY: `run_cargo` calls this before any thread exists in the
    // process; the environment is read concurrently only after routing has
    // been fully applied.
    unsafe {
        env::set_var("RUSTC", &tuned.rustc);
        env::set_var("RUSTDOC", &tuned.rustdoc);
        if needs_bootstrap {
            env::set_var("RUSTC_BOOTSTRAP", "1");
        }
        if let Some(rustflags) = rustflags {
            env::set_var("RUSTFLAGS", &rustflags);
        }
        env::set_var(
            "CARGO_TARGET_DIR",
            directory.join("target").join(TUNED_TOOLCHAIN),
        );
    }
}

/// Handles a tuned Cargo child that failed to launch or died to a signal:
/// records the appropriate marker and reruns the entire command through the
/// stock path by re-executing Cinder with the stock escape hatch set.
/// Ordinary compile failures never come here — the tuned compiler shares the
/// stock compiler's source, so its diagnostics are trusted as-is.
pub fn fallback_to_stock(
    original_arguments: &[OsString],
    failure: TunedFailure,
) -> Result<u8, String> {
    match failure {
        TunedFailure::Launch => {
            let path = machine_marker_path();
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(path, b"1");
        }
        TunedFailure::Signal => {
            if let Ok(directory) = env::current_dir() {
                if let Ok(directory) = fs::canonicalize(directory) {
                    let path = project_marker_path(&directory);
                    if let Some(parent) = path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::write(path, b"1");
                }
            }
        }
    }
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    let status = Command::new(cinder)
        .args(original_arguments)
        .env(STOCK_ENVIRONMENT, "1")
        .status()
        .map_err(|error| {
            format!("could not rerun the command with the stock toolchain: {error}")
        })?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

#[derive(Clone, Copy)]
pub enum TunedFailure {
    /// The Cargo child could not be spawned or the toolchain failed to
    /// launch at all; tuned mode is disabled machine-wide until
    /// `cinder clean` clears the marker.
    Launch,
    /// The child died to a signal (a crashed compiler); tuned mode is
    /// disabled for this project until `cinder clean` clears the marker.
    Signal,
}

/// Reports whether a finished child indicates a crashed tuned compiler
/// rather than an ordinary failed build.
pub fn died_to_signal(status: std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().is_some()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        false
    }
}

/// Removes the tuned artifact namespace and both fallback markers for a
/// project; wired into `cinder clean`.
pub fn clean_project(directory: &Path) {
    let _ = fs::remove_dir_all(directory.join("target").join(TUNED_TOOLCHAIN));
    let _ = fs::remove_file(project_marker_path(directory));
    let marker = machine_marker_path();
    let _ = fs::remove_file(&marker);
    // Every per-toolchain probe verdict is cleared alongside the markers so
    // `cinder clean` fully resets tuned-mode state.
    if let Some(parent) = marker.parent() {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("probe-cache")
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_cache_round_trips_per_toolchain_and_rejects_key_changes() {
        let key = b"cinder-test-key".to_vec();
        let _ = write_probe("cinder-test-a", &key, true);
        assert_eq!(read_probe("cinder-test-a", &key), Some(true));
        let other = b"cinder-other-key".to_vec();
        assert_eq!(read_probe("cinder-test-a", &other), None);
        // A second toolchain's verdict must not evict the first.
        let _ = write_probe("cinder-test-b", &key, false);
        assert_eq!(read_probe("cinder-test-b", &key), Some(false));
        assert_eq!(read_probe("cinder-test-a", &key), Some(true));
        // Names that cannot form a safe file name are rejected.
        assert!(probe_cache_path("").is_none());
        assert!(probe_cache_path("evil/../name").is_none());
    }

    #[test]
    fn pin_channels_classify_and_name_candidates_conservatively() {
        assert!(is_plain_version("1.88"));
        assert!(is_plain_version("1.88.0"));
        assert!(!is_plain_version("nightly-2026-07-20"));
        assert!(!is_plain_version("stable"));
        assert!(!is_plain_version("beta"));
        assert!(!is_plain_version("1.88.0-aarch64-apple-darwin"));
        assert!(!is_plain_version("1"));
        assert!(!is_plain_version("1..0"));
        assert!(matches!(
            classify_pin(Some("1.88.0".to_owned())),
            ProjectPin::PlainVersion(version) if version == "1.88.0"
        ));
        assert!(matches!(
            classify_pin(Some("nightly-2026-07-20".to_owned())),
            ProjectPin::Other
        ));
        assert!(matches!(classify_pin(None), ProjectPin::Other));
    }

    #[test]
    fn flags_files_parse_bounded_and_fail_closed() {
        assert_eq!(parse_flags(""), Some(Vec::new()));
        assert_eq!(
            parse_flags("-Zthreads=8\n"),
            Some(vec!["-Zthreads=8".to_owned()])
        );
        assert_eq!(
            parse_flags("-Zthreads=8 -Cdebuginfo=1"),
            Some(vec!["-Zthreads=8".to_owned(), "-Cdebuginfo=1".to_owned()])
        );
        // Tokens must look like flags; anything else disables tuned mode.
        assert_eq!(parse_flags("threads=8"), None);
        assert_eq!(parse_flags("-Zthreads=8 rm"), None);
        assert_eq!(parse_flags("-Z\"quoted\""), None);
        let oversized = vec!["-Zflag"; MAX_FLAGS_TOKENS + 1].join(" ");
        assert_eq!(parse_flags(&oversized), None);
    }

    #[test]
    fn selector_shapes_and_environment_overrides_stay_stock() {
        let eligible = |arguments: &[&str]| {
            let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
            eligible_invocation(&arguments).is_some()
        };
        assert!(!eligible(&["build", "--release"]));
        assert!(!eligible(&["build", "--profile=bench"]));
        assert!(!eligible(&["check", "--target", "x86_64-apple-darwin"]));
        assert!(!eligible(&["build", "--target-dir", "elsewhere"]));
        assert!(!eligible(&["+nightly", "build"]));
        assert!(!eligible(&["clean"]));
        assert!(!eligible(&["publish"]));
    }
}
