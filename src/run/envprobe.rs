//! Probe-verified reconstruction of Cargo's compiler environment.
//!
//! Once per toolchain identity, Cinder compiles a tiny offline probe
//! workspace with `RUSTC_WRAPPER` pointed at itself and records, for every
//! compiler process Cargo spawns, the exact argv and environment. Every
//! injected variable must classify against a closed derivation table —
//! manifest fields, unit paths, toolchain-launch constants, or the session
//! jobserver — and the process observer's KERN_PROCARGS2 environment parse
//! must match the wrapper-recorded environment byte for byte, proving the
//! parser on this machine before it is ever trusted on a real build. A
//! witness lets recipe binding validate a real unit's complete observed
//! environment variable by variable; only then does a recipe carry the full
//! injected set, which is what makes untracked procedural-macro environment
//! reads observe Cargo-identical values at replay. Anything unwitnessed —
//! an unknown variable, an unprobed Cargo version, a parser mismatch — fails
//! closed to exactly today's behavior.

use super::replay::{
    CompilerRecipe, EnvironmentKind, JOBSERVER_ENVIRONMENT, WITNESS_CONSTANT_ENVIRONMENT,
};
use super::{OsStr, OsString, Path, PathBuf, TRACE_RUN, env, fs};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

const WITNESS_MAGIC: &[u8; 8] = b"CNDW0002";
const DUMP_MAGIC: &[u8; 8] = b"CNDPRB01";
const WITNESS_DIRECTORY: &str = "env-witness";
const PROBE_ENVIRONMENT: &str = "CINDER_ENV_PROBE_OUT";
const WRAPPER_ENVIRONMENT: &str = "RUSTC_WRAPPER";
const MAX_WITNESS_BYTES: u64 = 256 * 1024;
const MAX_PIN_FILE_BYTES: u64 = 64 * 1024;
const MAX_DUMP_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DUMPS: usize = 256;
const MAX_ENVIRONMENT_VALUES: usize = 4_096;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const FAILED_RETRY_SECONDS: u64 = 7 * 24 * 60 * 60;
const PROBE_VERSION: &str = "9.7.5-cinderpre+cinderbuild7";
const PROBE_DESCRIPTION: &str = "CINDER-PROBE-DESCRIPTION-3f6c1a";
const PROBE_HOMEPAGE: &str = "https://cinder-probe.invalid/homepage-77d2";
const PROBE_REPOSITORY: &str = "https://cinder-probe.invalid/repo-91aa";
const PROBE_LICENSE: &str = "Zlib";
const PROBE_AUTHORS: [&str; 2] = [
    "Cinder Probe One <one@cinder-probe.invalid>",
    "Cinder Probe Two <two@cinder-probe.invalid>",
];
const PROBE_RUST_VERSION: &str = "1.85";
const PROBE_README: &str = "cinder-probe-readme.md";

/// How one injected variable's value derives from validated inputs. The
/// witness stores exactly one derivation per variable name; a name observed
/// with values matching zero or several derivations is unsupported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Derivation {
    /// A manifest field of the compiled package (including version parts and
    /// target names): Cargo's own observed bytes are accepted because the
    /// manifest is part of the validated input set.
    PackageField,
    /// Must equal the `--crate-name` argument of the invocation.
    CrateName,
    /// Must equal the recorded manifest directory of the unit.
    ManifestDirectory,
    /// Must equal `<manifest directory>/Cargo.toml`.
    ManifestPath,
    /// Must equal the witnessed real Cargo executable path.
    CargoPath,
    /// Must equal the witnessed toolchain-launch constant.
    ToolchainConstant,
    /// Must be exactly `1` (the primary-package flag).
    PrimaryFlag,
    /// Must be the empty string.
    EmptyValue,
    /// Must equal the unit's recorded build-script output directory.
    OutDirectory,
    /// The session jobserver: witnessed by name, never restored.
    Jobserver,
    /// The dynamic-loader path Cargo builds for dylib-producing units: the
    /// unit's own output directory, a colon, then the witnessed toolchain
    /// library tail.
    LoaderPath,
    /// `PATH` with the effective Cargo home's `bin` directory prepended by
    /// the launch layer when it was not already present.
    PathPrepend,
}

impl Derivation {
    fn code(self) -> u8 {
        match self {
            Self::PackageField => 0,
            Self::CrateName => 1,
            Self::ManifestDirectory => 2,
            Self::ManifestPath => 3,
            Self::CargoPath => 4,
            Self::ToolchainConstant => 5,
            Self::PrimaryFlag => 6,
            Self::EmptyValue => 7,
            Self::OutDirectory => 8,
            Self::Jobserver => 9,
            Self::LoaderPath => 10,
            Self::PathPrepend => 11,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::PackageField,
            1 => Self::CrateName,
            2 => Self::ManifestDirectory,
            3 => Self::ManifestPath,
            4 => Self::CargoPath,
            5 => Self::ToolchainConstant,
            6 => Self::PrimaryFlag,
            7 => Self::EmptyValue,
            8 => Self::OutDirectory,
            9 => Self::Jobserver,
            10 => Self::LoaderPath,
            11 => Self::PathPrepend,
            _ => return None,
        })
    }
}

/// The per-toolchain environment witness: the union derivation table over
/// every probed unit shape, plus the constants a replay restores verbatim.
pub struct EnvironmentWitness {
    pub(super) cargo_path: PathBuf,
    pub(super) constants: BTreeMap<String, OsString>,
    pub(super) derivations: BTreeMap<String, Derivation>,
    /// The toolchain library tail of the dynamic-loader path, after the
    /// per-unit output-directory prefix.
    pub(super) loader_tail: Option<OsString>,
}

/// The `PATH` value after the launch layer prepends the effective Cargo
/// home's `bin` directory to the base value. `None` when the base carries
/// neither a Cargo home nor a home directory to derive one from.
fn prepended_path(base: &BTreeMap<OsString, OsString>) -> Option<OsString> {
    let cargo_home = base
        .get(OsStr::new("CARGO_HOME"))
        .map(PathBuf::from)
        .or_else(|| {
            base.get(OsStr::new("HOME"))
                .map(|home| PathBuf::from(home).join(".cargo"))
        })?;
    let mut expected = cargo_home.join("bin").into_os_string();
    expected.push(":");
    expected.push(base.get(OsStr::new("PATH"))?);
    Some(expected)
}

/// The first segment of Cargo's dynamic-loader path for a unit: the profile
/// `deps` directory. Ordinary units emit straight into it; build-script
/// compile units emit into `<profile>/build/<package>-<hash>` while their
/// loader path still leads with the deps directory.
fn loader_prefix_for(out_directory: &Path) -> Option<PathBuf> {
    if out_directory.file_name().is_some() {
        let parent = out_directory.parent()?;
        if parent.file_name() == Some(OsStr::new("build")) {
            return Some(parent.parent()?.join("deps"));
        }
    }
    Some(out_directory.to_owned())
}

fn write_private_file_bytes(path: &Path, contents: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    file.write_all(contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn trace(message: &str) {
    if env::var_os(TRACE_RUN).is_some() {
        eprintln!("    Cinder trace: {message}");
    }
}

// ---------------------------------------------------------------------------
// Wrapper mode
// ---------------------------------------------------------------------------

/// The hidden `RUSTC_WRAPPER` dump-then-exec mode. Returns `None` unless the
/// probe environment variable is present AND the first argument is an
/// absolute path named `rustc` — the exact shape Cargo uses for wrapper
/// invocations during witness generation. Recording is best-effort: any
/// failure still execs the real compiler transparently.
pub fn wrapper_main(arguments: &[OsString]) -> Option<u8> {
    let output_directory = env::var_os(PROBE_ENVIRONMENT)?;
    let compiler = Path::new(arguments.first()?);
    if !compiler.is_absolute() || compiler.file_name() != Some(OsStr::new("rustc")) {
        return None;
    }
    let _ = record_wrapper_dump(Path::new(&output_directory), arguments);
    Some(execute_compiler(arguments))
}

fn record_wrapper_dump(directory: &Path, arguments: &[OsString]) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let path = directory.join(format!("dump-{}-{nanos}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("could not create a probe dump: {error}"))?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(DUMP_MAGIC);
    let working_directory = env::current_dir()
        .map_err(|error| format!("could not read the working directory: {error}"))?;
    append_value(&mut bytes, working_directory.as_os_str());
    append_count(&mut bytes, arguments.len());
    for argument in arguments {
        append_value(&mut bytes, argument);
    }
    let environment: Vec<(OsString, OsString)> = env::vars_os().collect();
    append_count(&mut bytes, environment.len());
    for (key, value) in &environment {
        append_value(&mut bytes, key);
        append_value(&mut bytes, value);
    }
    file.write_all(&bytes)
        .map_err(|error| format!("could not write a probe dump: {error}"))
}

fn execute_compiler(arguments: &[OsString]) -> u8 {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let mut command = Command::new(&arguments[0]);
    command.args(&arguments[1..]);
    let error = command.exec();
    eprintln!("cinder: could not execute the probed compiler: {error}");
    1
}

// ---------------------------------------------------------------------------
// Witness store
// ---------------------------------------------------------------------------

fn witness_root() -> PathBuf {
    super::cache::cinder_state_root().join(WITNESS_DIRECTORY)
}

pub(super) fn clear_witnesses() -> Result<(), String> {
    match fs::remove_dir_all(witness_root()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not clear the Cinder environment witnesses: {error}"
        )),
    }
}

/// Everything the witness key derives from and everything generation needs,
/// captured in one snapshot so the probe can never resolve a different
/// toolchain than the invocation it is keyed by.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct ProbeContext {
    key: String,
    /// SHA-256 over the cargo and rustc `-vV` reports; stored inside the
    /// witness so a file can never serve a context it was not built from.
    report_digest: [u8; 32],
    cargo_report: Vec<u8>,
    rustc_report: Vec<u8>,
    /// The sanitized environment the probe cargo runs under.
    base: BTreeMap<OsString, OsString>,
    /// The nearest toolchain pin files above the invocation directory,
    /// mirrored into the probe workspace so the rustup shim resolves the
    /// probe exactly as it resolves the real command.
    pin_files: Vec<(String, Vec<u8>)>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn probe_context(cargo: &Path) -> Option<ProbeContext> {
    let snapshot: BTreeMap<OsString, OsString> = env::vars_os().collect();
    let cargo_report = versioned_report(cargo.as_os_str())?;
    let rustc = snapshot
        .get(OsStr::new("RUSTC"))
        .cloned()
        .unwrap_or_else(|| OsString::from("rustc"));
    let rustc_report = versioned_report(&rustc)?;
    let mut report = Sha256::new();
    report.update((cargo_report.len() as u64).to_le_bytes());
    report.update(&cargo_report);
    report.update((rustc_report.len() as u64).to_le_bytes());
    report.update(&rustc_report);
    let report_digest: [u8; 32] = report.finalize().into();
    let mut digest = Sha256::new();
    digest.update(WITNESS_MAGIC);
    digest.update(report_digest);
    // Whether a toolchain-launch constant is injected depends on whether the
    // base environment already carries it, so the presence set is part of
    // the witness identity: a witness generated under one launch context
    // never validates another.
    for name in WITNESS_CONSTANT_ENVIRONMENT {
        digest.update([u8::from(snapshot.contains_key(OsStr::new(name)))]);
    }
    digest.update([u8::from(snapshot.contains_key(OsStr::new("CARGO")))]);
    let key = hex(&digest.finalize());

    let mut base = snapshot;
    // The probe must not inherit an outer wrapper, jobserver, or redirected
    // target directory; classification is relative to exactly this snapshot.
    base.remove(OsStr::new("CARGO_TARGET_DIR"));
    base.remove(OsStr::new("CARGO_BUILD_TARGET_DIR"));
    base.remove(OsStr::new(JOBSERVER_ENVIRONMENT));
    base.remove(OsStr::new("RUSTC_WORKSPACE_WRAPPER"));
    // The loader-path tail is extracted from a clean slate; a pre-existing
    // value would be appended by Cargo and poison the witnessed shape.
    base.remove(OsStr::new("DYLD_FALLBACK_LIBRARY_PATH"));
    base.remove(OsStr::new("LD_LIBRARY_PATH"));
    // Every Cargo-injected name is stripped from the probe base so the probe
    // observes each one as injected no matter how Cinder itself was
    // launched (`cargo test` and Cargo-spawned shells already carry them).
    let cargo_injected: Vec<OsString> = base
        .keys()
        .filter(|key| {
            key.to_str().is_some_and(|key| {
                key == "CARGO"
                    || key == "OUT_DIR"
                    || key == JOBSERVER_ENVIRONMENT
                    || key.starts_with("CARGO_PKG_")
                    || key.starts_with("CARGO_BIN_EXE_")
                    || matches!(
                        key,
                        "CARGO_CRATE_NAME"
                            | "CARGO_MANIFEST_DIR"
                            | "CARGO_MANIFEST_PATH"
                            | "CARGO_PRIMARY_PACKAGE"
                            | "CARGO_SBOM_PATH"
                            | "CARGO_BIN_NAME"
                            | "CARGO_TARGET_TMPDIR"
                    )
            })
        })
        .cloned()
        .collect();
    for key in cargo_injected {
        base.remove(&key);
    }

    Some(ProbeContext {
        key,
        report_digest,
        cargo_report,
        rustc_report,
        base,
        pin_files: nearest_toolchain_pins(),
    })
}

/// The toolchain pin files of the nearest ancestor directory that has any,
/// exactly as the rustup shim would discover them from the invocation
/// directory. An unreadable or oversized pin yields none, which leaves the
/// probe resolving differently and the post-generation report validation
/// failing closed.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn nearest_toolchain_pins() -> Vec<(String, Vec<u8>)> {
    let Ok(directory) = env::current_dir().and_then(fs::canonicalize) else {
        return Vec::new();
    };
    let mut candidate = directory.as_path();
    for _ in 0..16 {
        let mut found = Vec::new();
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            let path = candidate.join(name);
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_PIN_FILE_BYTES {
                return Vec::new();
            }
            match fs::read(&path) {
                Ok(bytes) => found.push((name.to_owned(), bytes)),
                Err(_) => return Vec::new(),
            }
        }
        if !found.is_empty() {
            return found;
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => break,
        }
    }
    Vec::new()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn versioned_report(program: &OsStr) -> Option<Vec<u8>> {
    let output = std::process::Command::new(program)
        .arg("-vV")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}

fn hex(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        rendered.push_str(&format!("{byte:02x}"));
    }
    rendered
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn write_witness(
    path: &Path,
    witness: &EnvironmentWitness,
    report_digest: &[u8; 32],
) -> Result<(), String> {
    // Concurrent generations of the same key must never tear the file.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(WITNESS_MAGIC);
    bytes.extend_from_slice(report_digest);
    append_value(&mut bytes, witness.cargo_path.as_os_str());
    append_count(&mut bytes, witness.constants.len());
    for (name, value) in &witness.constants {
        append_value(&mut bytes, OsStr::new(name));
        append_value(&mut bytes, value);
    }
    append_count(&mut bytes, witness.derivations.len());
    for (name, derivation) in &witness.derivations {
        append_value(&mut bytes, OsStr::new(name));
        bytes.push(derivation.code());
    }
    match &witness.loader_tail {
        Some(tail) => {
            bytes.push(1);
            append_value(&mut bytes, tail);
        }
        None => bytes.push(0),
    }
    let staging = path.with_extension(format!("tmp-{}", std::process::id()));
    write_private_file_bytes(&staging, &bytes)?;
    fs::rename(&staging, path).map_err(|error| {
        let _ = fs::remove_file(&staging);
        format!("could not publish the environment witness: {error}")
    })
}

/// Reads a stored witness, requiring both the format magic and the exact
/// toolchain-report digest of the current invocation context: a witness
/// whose stored toolchain evidence does not match the context that is asking
/// is treated as absent and regenerates.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn read_witness(path: &Path, expected_digest: &[u8; 32]) -> Option<EnvironmentWitness> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_WITNESS_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let mut cursor = &bytes[..];
    let mut magic = [0_u8; 8];
    cursor.read_exact(&mut magic).ok()?;
    if &magic != WITNESS_MAGIC {
        return None;
    }
    let mut stored_digest = [0_u8; 32];
    cursor.read_exact(&mut stored_digest).ok()?;
    if &stored_digest != expected_digest {
        return None;
    }
    let cargo_path = PathBuf::from(read_value(&mut cursor)?);
    if !cargo_path.is_absolute() {
        return None;
    }
    let constant_count = read_count(&mut cursor)?;
    let mut constants = BTreeMap::new();
    for _ in 0..constant_count {
        let name = read_value(&mut cursor)?.into_string().ok()?;
        if !WITNESS_CONSTANT_ENVIRONMENT.contains(&name.as_str()) {
            return None;
        }
        let value = read_value(&mut cursor)?;
        if constants.insert(name, value).is_some() {
            return None;
        }
    }
    let derivation_count = read_count(&mut cursor)?;
    let mut derivations = BTreeMap::new();
    for _ in 0..derivation_count {
        let name = read_value(&mut cursor)?.into_string().ok()?;
        let mut code = [0_u8; 1];
        cursor.read_exact(&mut code).ok()?;
        let derivation = Derivation::from_code(code[0])?;
        if name.is_empty() || derivations.insert(name, derivation).is_some() {
            return None;
        }
    }
    let mut has_tail = [0_u8; 1];
    cursor.read_exact(&mut has_tail).ok()?;
    let loader_tail = match has_tail[0] {
        0 => None,
        1 => Some(read_value(&mut cursor)?),
        _ => return None,
    };
    if !cursor.is_empty() {
        return None;
    }
    Some(EnvironmentWitness {
        cargo_path,
        constants,
        derivations,
        loader_tail,
    })
}

fn append_count(bytes: &mut Vec<u8>, count: usize) {
    bytes.extend_from_slice(&(count.min(u32::MAX as usize) as u32).to_le_bytes());
}

fn append_value(bytes: &mut Vec<u8>, value: &OsStr) {
    let value = value.as_bytes();
    bytes.extend_from_slice(&(value.len().min(u32::MAX as usize) as u32).to_le_bytes());
    bytes.extend_from_slice(value);
}

fn read_count(cursor: &mut &[u8]) -> Option<usize> {
    let mut raw = [0_u8; 4];
    cursor.read_exact(&mut raw).ok()?;
    let count = u32::from_le_bytes(raw) as usize;
    (count <= MAX_ENVIRONMENT_VALUES).then_some(count)
}

fn read_value(cursor: &mut &[u8]) -> Option<OsString> {
    let mut raw = [0_u8; 4];
    cursor.read_exact(&mut raw).ok()?;
    let length = u32::from_le_bytes(raw) as usize;
    if length > MAX_VALUE_BYTES || cursor.len() < length {
        return None;
    }
    let (value, rest) = cursor.split_at(length);
    let value = OsString::from_vec(value.to_vec());
    *cursor = rest;
    Some(value)
}

// ---------------------------------------------------------------------------
// Full-environment recipe binding
// ---------------------------------------------------------------------------

/// Validates a recipe's observed environment variable by variable against
/// the witness and the receipt's recorded facts, and on success installs the
/// complete injected set as the recipe environment. Every failure leaves the
/// recipe in its tracked-subset form.
pub(super) fn bind_full_environment(
    recipe: &mut CompilerRecipe,
    witness: &EnvironmentWitness,
    base_environment: &BTreeMap<OsString, OsString>,
    manifest_directory: Option<&Path>,
    out_directory: Option<&Path>,
) -> Result<(), String> {
    let observed = recipe
        .observed_environment
        .as_ref()
        .ok_or_else(|| "the compiler environment was not observed".to_owned())?;
    if observed.len() > MAX_ENVIRONMENT_VALUES {
        return Err("the observed compiler environment is too large".to_owned());
    }
    let crate_name = super::capture::rustc_option(&recipe.arguments, "--crate-name")
        .ok_or_else(|| "the compiler invocation has no crate name".to_owned())?
        .to_os_string();
    let mut environment = Vec::new();
    let mut seen = BTreeSet::new();
    for (key, value) in observed {
        if !seen.insert(key.clone()) {
            return Err("the observed compiler environment has a duplicate key".to_owned());
        }
        if base_environment.get(key).map(OsString::as_os_str) == Some(value.as_os_str()) {
            // Inherited byte-for-byte from the environment Cinder gave
            // Cargo; the replay inherits it through the bound context.
            continue;
        }
        let Some(name) = key.to_str() else {
            return Err("an injected compiler variable has a non-UTF-8 name".to_owned());
        };
        let Some(derivation) = witness.derivations.get(name) else {
            return Err(format!("compiler variable {name} is not witnessed"));
        };
        let valid = match derivation {
            Derivation::PackageField => true,
            Derivation::CrateName => *value == crate_name,
            Derivation::ManifestDirectory => {
                manifest_directory.is_some_and(|directory| value.as_os_str() == directory)
            }
            Derivation::ManifestPath => manifest_directory
                .is_some_and(|directory| Path::new(value) == directory.join("Cargo.toml")),
            Derivation::CargoPath => Path::new(value) == witness.cargo_path,
            Derivation::ToolchainConstant => witness
                .constants
                .get(name)
                .is_some_and(|constant| constant == value),
            Derivation::PrimaryFlag => value.as_os_str() == OsStr::new("1"),
            Derivation::EmptyValue => value.is_empty(),
            Derivation::OutDirectory => {
                out_directory.is_some_and(|directory| value.as_os_str() == directory)
            }
            Derivation::Jobserver => {
                // Witnessed by name; never restored.
                continue;
            }
            Derivation::PathPrepend => {
                prepended_path(base_environment).is_some_and(|expected| *value == expected)
            }
            Derivation::LoaderPath => witness.loader_tail.as_ref().is_some_and(|tail| {
                super::capture::rustc_option(&recipe.arguments, "--out-dir")
                    .and_then(|out_directory| loader_prefix_for(Path::new(out_directory)))
                    .is_some_and(|deps_directory| {
                        // With no pre-existing value Cargo appends the
                        // witnessed toolchain tail (ending in the loader's
                        // default fallback list); with one it appends the
                        // existing value instead.
                        let mut prefix = deps_directory.into_os_string();
                        prefix.push(":");
                        let mut unset_form = prefix.clone();
                        unset_form.push(tail);
                        if *value == unset_form {
                            return true;
                        }
                        base_environment.get(key).is_some_and(|existing| {
                            let mut set_form = prefix;
                            set_form.push(existing);
                            *value == set_form
                        })
                    })
            }),
        };
        if !valid {
            let shown: String = value.to_string_lossy().chars().take(160).collect();
            return Err(format!(
                "compiler variable {name} does not match its witnessed derivation ({shown:?})"
            ));
        }
        environment.push((key.clone(), value.clone()));
    }
    environment.sort();
    recipe.environment = environment;
    recipe.environment_kind = EnvironmentKind::FullWitnessed;
    Ok(())
}

// ---------------------------------------------------------------------------
// Witness generation (macOS: requires the process observer)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) fn witness_for(cargo: &Path) -> Option<EnvironmentWitness> {
    let context = probe_context(cargo)?;
    let directory = witness_root().join(&context.key);
    let witness_path = directory.join("witness");
    if let Some(witness) = read_witness(&witness_path, &context.report_digest) {
        return Some(witness);
    }
    let failed_path = directory.join("failed");
    if let Ok(metadata) = fs::metadata(&failed_path) {
        let fresh = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() < FAILED_RETRY_SECONDS);
        if fresh {
            return None;
        }
        let _ = fs::remove_file(&failed_path);
    }
    let mut last_error = None;
    for _attempt in 0..3 {
        match generate(cargo, &directory, &context) {
            Ok(witness) => {
                fs::create_dir_all(&directory).ok()?;
                super::make_private_directory(&directory).ok()?;
                if write_witness(&witness_path, &witness, &context.report_digest).is_err() {
                    return None;
                }
                trace("environment witness generated");
                return Some(witness);
            }
            Err(error) => {
                let transient = matches!(error, GenerationError::Transient(_));
                last_error = Some(error);
                if !transient {
                    break;
                }
            }
        }
    }
    let error = last_error?;
    trace(&format!(
        "environment witness unavailable ({})",
        error.reason()
    ));
    if let GenerationError::Structural(reason) = &error {
        let _ = super::cache::remove_directory_if_present(&directory);
        let _ = fs::create_dir_all(&directory);
        let _ = super::make_private_directory(&directory);
        let _ = write_private_file_bytes(&failed_path, reason.as_bytes());
    }
    None
}

/// Distinguishes failures a later invocation may not repeat (an observer
/// race, a filesystem hiccup) from structural verdicts about the toolchain
/// (an unclassifiable variable, a parser mismatch); only the latter write
/// the durable `failed` marker.
#[cfg(target_os = "macos")]
enum GenerationError {
    Transient(String),
    Structural(String),
}

#[cfg(target_os = "macos")]
impl GenerationError {
    fn reason(&self) -> &str {
        match self {
            Self::Transient(reason) | Self::Structural(reason) => reason,
        }
    }
}

#[cfg(target_os = "macos")]
struct ProbeDump {
    working_directory: PathBuf,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
}

#[cfg(target_os = "macos")]
fn generate(
    cargo: &Path,
    directory: &Path,
    context: &ProbeContext,
) -> Result<EnvironmentWitness, GenerationError> {
    let staging = directory.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| GenerationError::Transient(
                "system clock predates the Unix epoch".to_owned()
            ))?
            .as_nanos()
    ));
    let result = generate_in(cargo, &staging, context);
    let _ = fs::remove_dir_all(&staging);
    result
}

#[cfg(target_os = "macos")]
fn generate_in(
    cargo: &Path,
    staging: &Path,
    context: &ProbeContext,
) -> Result<EnvironmentWitness, GenerationError> {
    let transient = GenerationError::Transient;
    fs::create_dir_all(staging).map_err(|error| {
        transient(format!(
            "could not create the probe staging directory: {error}"
        ))
    })?;
    super::make_private_directory(staging).map_err(GenerationError::Transient)?;
    let workspace = staging.join("workspace");
    write_probe_workspace(&workspace).map_err(GenerationError::Transient)?;
    // The invocation directory's toolchain pins are mirrored into the probe
    // workspace so the rustup shim resolves the probe's cargo and rustc
    // exactly as it resolves the real command the witness is keyed by.
    for (name, bytes) in &context.pin_files {
        fs::write(workspace.join(name), bytes)
            .map_err(|error| transient(format!("could not mirror a toolchain pin: {error}")))?;
    }
    // Cargo reports canonical manifest paths; the temp root usually reaches
    // the workspace through a symlink (`/var` -> `/private/var`).
    let workspace = fs::canonicalize(&workspace)
        .map_err(|error| transient(format!("could not resolve the probe workspace: {error}")))?;
    let wrapper = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| transient(format!("could not identify the Cinder executable: {error}")))?;

    let mut base = context.base.clone();
    base.insert(
        OsString::from(WRAPPER_ENVIRONMENT),
        wrapper.clone().into_os_string(),
    );

    let mut runs = Vec::new();
    for run in 0..2 {
        let dumps = staging.join(format!("dumps-{run}"));
        fs::create_dir_all(&dumps).map_err(|error| {
            transient(format!(
                "could not create the probe dump directory: {error}"
            ))
        })?;
        super::make_private_directory(&dumps).map_err(GenerationError::Transient)?;
        let mut environment = base.clone();
        environment.insert(
            OsString::from(PROBE_ENVIRONMENT),
            dumps.clone().into_os_string(),
        );
        if run == 1 {
            run_probe_cargo(cargo, &workspace, &environment, &["clean"], None)
                .map_err(GenerationError::Transient)?;
        }
        let recipes = run_probe_cargo(
            cargo,
            &workspace,
            &environment,
            &["build", "--offline"],
            Some(()),
        )
        .map_err(GenerationError::Transient)?;
        let dumps = read_dumps(&dumps).map_err(GenerationError::Transient)?;
        prove_parser(&dumps, &recipes)?;
        runs.push(dumps);
    }
    let witness = classify(cargo, &workspace, &base, &runs[0], &runs[1])
        .map_err(GenerationError::Structural)?;
    validate_resolution(context, &witness, &runs[0])?;
    Ok(witness)
}

/// Proves the probe resolved the same toolchain the witness is keyed by: the
/// observed `CARGO` binary and the compiler every unit dump invoked must
/// report the exact `-vV` bytes of the invocation context. This is what
/// catches any launch-context input the probe cannot reproduce (for example
/// a rustup directory override) before a mismatched witness could be stored.
#[cfg(target_os = "macos")]
fn validate_resolution(
    context: &ProbeContext,
    witness: &EnvironmentWitness,
    dumps: &[ProbeDump],
) -> Result<(), GenerationError> {
    if versioned_report(witness.cargo_path.as_os_str()).as_deref() != Some(&context.cargo_report) {
        return Err(GenerationError::Structural(
            "the probe resolved a different Cargo than the invocation context".to_owned(),
        ));
    }
    let mut compiler: Option<&OsString> = None;
    for dump in dumps {
        if !is_unit_invocation(normalized_compiler_arguments(&dump.arguments)) {
            continue;
        }
        // The dump records the wrapper's arguments after its own argv[0], so
        // the invoked compiler path is the first entry.
        let Some(first) = dump.arguments.first() else {
            continue;
        };
        if !(Path::new(first).is_absolute()
            && Path::new(first).file_name() == Some(OsStr::new("rustc")))
        {
            continue;
        }
        match compiler {
            None => compiler = Some(first),
            Some(existing) if existing == first => {}
            Some(_) => {
                return Err(GenerationError::Structural(
                    "the probe invoked more than one compiler".to_owned(),
                ));
            }
        }
    }
    let compiler = compiler.ok_or_else(|| {
        GenerationError::Transient("the probe revealed no compiler path".to_owned())
    })?;
    if versioned_report(compiler).as_deref() != Some(&context.rustc_report) {
        return Err(GenerationError::Structural(
            "the probe resolved a different compiler than the invocation context".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_probe_cargo(
    cargo: &Path,
    workspace: &Path,
    environment: &BTreeMap<OsString, OsString>,
    arguments: &[&str],
    observe: Option<()>,
) -> Result<Vec<CompilerRecipe>, String> {
    use std::process::{Command, Stdio};

    let mut command = Command::new(cargo);
    command
        .args(arguments)
        .current_dir(workspace)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run the probe Cargo: {error}"))?;
    let observer = observe.map(|()| super::CompilerObserver::start(child.id(), true));
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for the probe Cargo: {error}"))?;
    let recipes = observer
        .map(super::CompilerObserver::finish)
        .unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "the probe build failed: {}",
            String::from_utf8_lossy(&stderr)
                .lines()
                .last()
                .unwrap_or("unknown error")
        ));
    }
    Ok(recipes)
}

#[cfg(target_os = "macos")]
fn write_probe_workspace(root: &Path) -> Result<(), String> {
    let write = |relative: &str, contents: &str| -> Result<(), String> {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create the probe workspace: {error}"))?;
        }
        fs::write(&path, contents)
            .map_err(|error| format!("could not write the probe workspace: {error}"))
    };
    write(
        "Cargo.toml",
        &format!(
            "[workspace]\nmembers = [\"dep\", \"macro\"]\n\n\
             [package]\nname = \"cinder-probe\"\nversion = \"{PROBE_VERSION}\"\n\
             edition = \"2021\"\ndescription = \"{PROBE_DESCRIPTION}\"\n\
             homepage = \"{PROBE_HOMEPAGE}\"\nrepository = \"{PROBE_REPOSITORY}\"\n\
             license = \"{PROBE_LICENSE}\"\nauthors = [\"{}\", \"{}\"]\n\
             rust-version = \"{PROBE_RUST_VERSION}\"\nreadme = \"{PROBE_README}\"\n\n\
             [dependencies]\ncinder_probe_dep = {{ path = \"dep\" }}\n\
             cinder_probe_macro = {{ path = \"macro\" }}\n",
            PROBE_AUTHORS[0], PROBE_AUTHORS[1]
        ),
    )?;
    write(PROBE_README, "cinder probe readme\n")?;
    write("build.rs", "fn main() {}\n")?;
    write(
        "src/lib.rs",
        "pub fn probe() -> u32 { cinder_probe_macro::probe_value!() + cinder_probe_dep::dep_value() }\n",
    )?;
    write(
        "src/main.rs",
        "fn main() { println!(\"{}\", cinder_probe::probe()); }\n",
    )?;
    write(
        "dep/Cargo.toml",
        "[package]\nname = \"cinder_probe_dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    write("dep/src/lib.rs", "pub fn dep_value() -> u32 { 35 }\n")?;
    write(
        "macro/Cargo.toml",
        "[package]\nname = \"cinder_probe_macro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [lib]\nproc-macro = true\n",
    )?;
    write(
        "macro/src/lib.rs",
        "use proc_macro::TokenStream;\n\n\
         #[proc_macro]\npub fn probe_value(_input: TokenStream) -> TokenStream {\n    \
         \"7u32\".parse().unwrap()\n}\n",
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn read_dumps(directory: &Path) -> Result<Vec<ProbeDump>, String> {
    let mut dumps = Vec::new();
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("could not read the probe dumps: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read the probe dumps: {error}"))?;
        if dumps.len() >= MAX_DUMPS {
            return Err("the probe produced too many compiler invocations".to_owned());
        }
        let path = entry.path();
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("could not inspect a probe dump: {error}"))?;
        if !metadata.is_file() || metadata.len() > MAX_DUMP_BYTES {
            return Err("a probe dump has an unexpected shape".to_owned());
        }
        let bytes =
            fs::read(&path).map_err(|error| format!("could not read a probe dump: {error}"))?;
        dumps.push(parse_dump(&bytes).ok_or_else(|| "a probe dump is malformed".to_owned())?);
    }
    if dumps.is_empty() {
        return Err("the probe recorded no compiler invocations".to_owned());
    }
    Ok(dumps)
}

#[cfg(target_os = "macos")]
fn parse_dump(bytes: &[u8]) -> Option<ProbeDump> {
    let mut cursor = bytes;
    let mut magic = [0_u8; 8];
    cursor.read_exact(&mut magic).ok()?;
    if &magic != DUMP_MAGIC {
        return None;
    }
    let working_directory = PathBuf::from(read_value(&mut cursor)?);
    let argument_count = read_count(&mut cursor)?;
    let mut arguments = Vec::with_capacity(argument_count);
    for _ in 0..argument_count {
        arguments.push(read_value(&mut cursor)?);
    }
    let environment_count = read_count(&mut cursor)?;
    let mut environment = Vec::with_capacity(environment_count);
    for _ in 0..environment_count {
        let key = read_value(&mut cursor)?;
        let value = read_value(&mut cursor)?;
        environment.push((key, value));
    }
    cursor.is_empty().then_some(ProbeDump {
        working_directory,
        arguments,
        environment,
    })
}

/// Strips a leading `rustc` executable path so wrapper argv, pre-exec
/// observations, and post-exec observations all compare in the same shape.
#[cfg(target_os = "macos")]
fn normalized_compiler_arguments(arguments: &[OsString]) -> &[OsString] {
    match arguments.first() {
        Some(first)
            if Path::new(first).is_absolute()
                && Path::new(first).file_name() == Some(OsStr::new("rustc")) =>
        {
            &arguments[1..]
        }
        _ => arguments,
    }
}

#[cfg(target_os = "macos")]
fn is_unit_invocation(arguments: &[OsString]) -> bool {
    arguments
        .windows(2)
        .any(|values| values[0] == "--crate-name")
        && super::capture::rustc_list_options(arguments, "--emit")
            .iter()
            .any(|values| {
                values
                    .split(',')
                    .any(|value| matches!(value, "link" | "metadata"))
            })
}

/// The parser proof: every observed compiler process must have a dump whose
/// normalized argv matches, with a byte-identical environment; every
/// unit-shaped dump must have been observed. Only then is the KERN_PROCARGS2
/// environment parse trusted.
#[cfg(target_os = "macos")]
fn prove_parser(dumps: &[ProbeDump], recipes: &[CompilerRecipe]) -> Result<(), GenerationError> {
    let dump_index: Vec<(Vec<OsString>, &ProbeDump)> = dumps
        .iter()
        .map(|dump| {
            (
                normalized_compiler_arguments(&dump.arguments).to_vec(),
                dump,
            )
        })
        .collect();
    let mut matched_dumps = BTreeSet::new();
    for recipe in recipes {
        let arguments = normalized_compiler_arguments(&recipe.arguments).to_vec();
        let matches: Vec<usize> = dump_index
            .iter()
            .enumerate()
            .filter(|(_, (dump_arguments, _))| *dump_arguments == arguments)
            .map(|(index, _)| index)
            .collect();
        let index = match matches.as_slice() {
            [index] => *index,
            [] => {
                return Err(GenerationError::Transient(
                    "an observed compiler invocation has no probe dump".to_owned(),
                ));
            }
            _ => {
                return Err(GenerationError::Structural(
                    "an observed compiler invocation matches several probe dumps".to_owned(),
                ));
            }
        };
        matched_dumps.insert(index);
        let dump = dump_index[index].1;
        let Some(observed) = recipe.observed_environment.as_ref() else {
            return Err(GenerationError::Transient(
                "an observed compiler process had no parseable environment".to_owned(),
            ));
        };
        let observed: BTreeMap<&OsString, &OsString> =
            observed.iter().map(|(key, value)| (key, value)).collect();
        let dumped: BTreeMap<&OsString, &OsString> = dump
            .environment
            .iter()
            .map(|(key, value)| (key, value))
            .collect();
        if observed != dumped {
            let mut differing = BTreeSet::new();
            for key in observed.keys().chain(dumped.keys()) {
                if observed.get(*key) != dumped.get(*key) {
                    differing.insert(key.to_string_lossy().into_owned());
                }
            }
            let differing: Vec<String> = differing.into_iter().take(4).collect();
            return Err(GenerationError::Structural(format!(
                "the observed compiler environment does not match the wrapper record ({})",
                differing.join(", ")
            )));
        }
        if recipe.working_directory != dump.working_directory {
            return Err(GenerationError::Structural(
                "the observed working directory does not match the wrapper record".to_owned(),
            ));
        }
    }
    for (index, (arguments, _)) in dump_index.iter().enumerate() {
        if is_unit_invocation(arguments) && !matched_dumps.contains(&index) {
            return Err(GenerationError::Transient(
                "a probe compiler invocation was not observed".to_owned(),
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
struct ProbeUnit {
    crate_name: String,
    manifest_directory: PathBuf,
    package_name_value: String,
}

#[cfg(target_os = "macos")]
fn probe_unit(workspace: &Path, arguments: &[OsString]) -> Option<ProbeUnit> {
    let crate_name = super::capture::rustc_option(arguments, "--crate-name")?.to_str()?;
    let (manifest_directory, package_name_value) = match crate_name {
        "cinder_probe" | "build_script_build" => (workspace.to_owned(), "cinder-probe"),
        "cinder_probe_dep" => (workspace.join("dep"), "cinder_probe_dep"),
        "cinder_probe_macro" => (workspace.join("macro"), "cinder_probe_macro"),
        _ => return None,
    };
    Some(ProbeUnit {
        crate_name: crate_name.to_owned(),
        manifest_directory,
        package_name_value: package_name_value.to_owned(),
    })
}

#[cfg(target_os = "macos")]
fn classify(
    cargo: &Path,
    workspace: &Path,
    base: &BTreeMap<OsString, OsString>,
    first_run: &[ProbeDump],
    second_run: &[ProbeDump],
) -> Result<EnvironmentWitness, String> {
    let _ = cargo;
    let mut constants: BTreeMap<String, OsString> = BTreeMap::new();
    let mut derivations: BTreeMap<String, Derivation> = BTreeMap::new();
    let mut cargo_path: Option<PathBuf> = None;
    let mut loader_tail: Option<OsString> = None;
    let mut shape_names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for (run_index, dumps) in [first_run, second_run].into_iter().enumerate() {
        let mut saw_macro_unit = false;
        for dump in dumps {
            let arguments = normalized_compiler_arguments(&dump.arguments);
            if !is_unit_invocation(arguments) {
                continue;
            }
            let Some(unit) = probe_unit(workspace, arguments) else {
                return Err("the probe compiled an unexpected crate".to_owned());
            };
            if unit.crate_name == "cinder_probe_macro" {
                saw_macro_unit = true;
            }
            let names = shape_names
                .entry(shape_label(&unit, arguments))
                .or_default();
            for (key, value) in &dump.environment {
                if key == OsStr::new(WRAPPER_ENVIRONMENT) || key == OsStr::new(PROBE_ENVIRONMENT) {
                    continue;
                }
                if base.get(key).map(OsString::as_os_str) == Some(value.as_os_str()) {
                    continue;
                }
                let Some(name) = key.to_str() else {
                    return Err("an injected probe variable has a non-UTF-8 name".to_owned());
                };
                names.insert(name.to_owned());
                let derivation = classify_variable(
                    name,
                    value,
                    &unit,
                    arguments,
                    workspace,
                    base,
                    &mut constants,
                    &mut cargo_path,
                    &mut loader_tail,
                    run_index,
                )?;
                match derivations.get(name) {
                    None => {
                        derivations.insert(name.to_owned(), derivation);
                    }
                    Some(existing) if *existing == derivation => {}
                    Some(_) => {
                        return Err(format!(
                            "probe variable {name} classifies inconsistently across units"
                        ));
                    }
                }
            }
        }
        if !saw_macro_unit {
            return Err("the probe did not compile the procedural macro".to_owned());
        }
    }
    let cargo_path =
        cargo_path.ok_or_else(|| "the probe never revealed the Cargo path".to_owned())?;
    // Name sets must be stable across runs; a run-varying set means the
    // toolchain injects session-dependent variables this table cannot carry.
    let mut per_run_names: [BTreeMap<String, BTreeSet<String>>; 2] =
        [BTreeMap::new(), BTreeMap::new()];
    for (run_index, dumps) in [first_run, second_run].into_iter().enumerate() {
        for dump in dumps {
            let arguments = normalized_compiler_arguments(&dump.arguments);
            if !is_unit_invocation(arguments) {
                continue;
            }
            let Some(unit) = probe_unit(workspace, arguments) else {
                continue;
            };
            let entry = per_run_names[run_index]
                .entry(shape_label(&unit, arguments))
                .or_default();
            for (key, value) in &dump.environment {
                if key == OsStr::new(WRAPPER_ENVIRONMENT)
                    || key == OsStr::new(PROBE_ENVIRONMENT)
                    || base.get(key).map(OsString::as_os_str) == Some(value.as_os_str())
                {
                    continue;
                }
                if let Some(name) = key.to_str() {
                    entry.insert(name.to_owned());
                }
            }
        }
    }
    if per_run_names[0] != per_run_names[1] {
        return Err("the probe environment sets vary between identical runs".to_owned());
    }
    // The launch layer prepends the Cargo home's bin directory to PATH only
    // when it is not already there, so whether the probe observes it depends
    // on the launch context. The rule itself is context-free — binding
    // validates the exact prepend shape against the record-time base — so
    // every witness carries it.
    match derivations.get("PATH") {
        None => {
            derivations.insert("PATH".to_owned(), Derivation::PathPrepend);
        }
        Some(Derivation::PathPrepend) => {}
        Some(_) => return Err("PATH classified inconsistently".to_owned()),
    }
    Ok(EnvironmentWitness {
        cargo_path,
        constants,
        derivations,
        loader_tail,
    })
}

#[cfg(target_os = "macos")]
fn shape_label(unit: &ProbeUnit, arguments: &[OsString]) -> String {
    let crate_types = super::capture::rustc_list_options(arguments, "--crate-type");
    let kind = if unit.crate_name == "build_script_build" {
        "build-script"
    } else if crate_types
        .iter()
        .any(|values| values.split(',').any(|v| v == "proc-macro"))
    {
        "proc-macro"
    } else if crate_types
        .iter()
        .any(|values| values.split(',').any(|v| v == "bin"))
    {
        "bin"
    } else {
        "lib"
    };
    format!("{kind}:{}", unit.crate_name)
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn classify_variable(
    name: &str,
    value: &OsString,
    unit: &ProbeUnit,
    arguments: &[OsString],
    workspace: &Path,
    base: &BTreeMap<OsString, OsString>,
    constants: &mut BTreeMap<String, OsString>,
    cargo_path: &mut Option<PathBuf>,
    loader_tail: &mut Option<OsString>,
    run_index: usize,
) -> Result<Derivation, String> {
    let text = value.to_str();
    if name == "PATH" {
        return if prepended_path(base).is_some_and(|expected| *value == expected) {
            Ok(Derivation::PathPrepend)
        } else {
            Err("PATH does not match the launch-layer prepend shape".to_owned())
        };
    }
    if name == "DYLD_FALLBACK_LIBRARY_PATH" || name == "LD_LIBRARY_PATH" {
        let out_directory = super::capture::rustc_option(arguments, "--out-dir")
            .ok_or_else(|| "a loader-path unit has no output directory".to_owned())?;
        let deps_directory = loader_prefix_for(Path::new(out_directory))
            .ok_or_else(|| "a loader-path unit has no deps directory".to_owned())?;
        let mut prefix = deps_directory.into_os_string();
        prefix.push(":");
        let value_bytes = value.as_bytes();
        let prefix_bytes = prefix.as_bytes();
        if value_bytes.len() <= prefix_bytes.len()
            || &value_bytes[..prefix_bytes.len()] != prefix_bytes
        {
            return Err(format!(
                "loader path {name} does not start with the unit output directory \
                 (unit {}, out {:?}, value {:?})",
                unit.crate_name,
                out_directory,
                value
                    .to_string_lossy()
                    .chars()
                    .take(120)
                    .collect::<String>()
            ));
        }
        let tail = OsString::from_vec(value_bytes[prefix_bytes.len()..].to_vec());
        if tail
            .as_bytes()
            .windows(workspace.as_os_str().as_bytes().len())
            .any(|window| window == workspace.as_os_str().as_bytes())
        {
            return Err(format!(
                "loader path {name} tail mentions the probe workspace"
            ));
        }
        match loader_tail {
            None => *loader_tail = Some(tail),
            Some(existing) if *existing == tail => {}
            Some(_) => return Err(format!("loader path {name} tail is not stable")),
        }
        return Ok(Derivation::LoaderPath);
    }
    if name == JOBSERVER_ENVIRONMENT {
        let plausible = text.is_some_and(|text| text.starts_with("-j"));
        return if plausible {
            Ok(Derivation::Jobserver)
        } else {
            Err("the jobserver variable has an unexpected shape".to_owned())
        };
    }
    if WITNESS_CONSTANT_ENVIRONMENT.contains(&name) {
        match constants.get(name) {
            None if run_index == 0 => {
                constants.insert(name.to_owned(), value.clone());
                return Ok(Derivation::ToolchainConstant);
            }
            Some(existing) if existing == value => return Ok(Derivation::ToolchainConstant),
            _ => {
                return Err(format!(
                    "toolchain constant {name} is not stable across probe runs"
                ));
            }
        }
    }
    if name == "CARGO" {
        let observed = PathBuf::from(value);
        if !observed.is_absolute() || !observed.is_file() {
            return Err("the CARGO variable is not an executable path".to_owned());
        }
        match cargo_path {
            None => *cargo_path = Some(observed),
            Some(existing) if *existing == observed => {}
            Some(_) => return Err("the CARGO variable is not stable".to_owned()),
        }
        return Ok(Derivation::CargoPath);
    }
    if name == "CARGO_CRATE_NAME" {
        return if text == Some(unit.crate_name.as_str()) {
            Ok(Derivation::CrateName)
        } else {
            Err("CARGO_CRATE_NAME does not match the invocation".to_owned())
        };
    }
    if name == "CARGO_MANIFEST_DIR" {
        return if Path::new(value) == unit.manifest_directory {
            Ok(Derivation::ManifestDirectory)
        } else {
            Err("CARGO_MANIFEST_DIR does not match the probe layout".to_owned())
        };
    }
    if name == "CARGO_MANIFEST_PATH" {
        return if Path::new(value) == unit.manifest_directory.join("Cargo.toml") {
            Ok(Derivation::ManifestPath)
        } else {
            Err("CARGO_MANIFEST_PATH does not match the probe layout".to_owned())
        };
    }
    if name == "CARGO_PRIMARY_PACKAGE" {
        return if text == Some("1") {
            Ok(Derivation::PrimaryFlag)
        } else {
            Err("CARGO_PRIMARY_PACKAGE has an unexpected value".to_owned())
        };
    }
    if name == "OUT_DIR" {
        let path = Path::new(value);
        return if path.is_absolute() && path.starts_with(workspace.join("target")) {
            Ok(Derivation::OutDirectory)
        } else {
            Err("OUT_DIR does not point into the probe target".to_owned())
        };
    }
    if name == "CARGO_BIN_NAME" {
        return if text == Some("cinder-probe") {
            Ok(Derivation::PackageField)
        } else {
            Err("CARGO_BIN_NAME does not match the probe target".to_owned())
        };
    }
    if let Some(field) = name.strip_prefix("CARGO_PKG_") {
        let is_root = unit.package_name_value == "cinder-probe";
        let expectation: Option<String> = match field {
            "NAME" => Some(unit.package_name_value.clone()),
            "VERSION" => Some(if is_root {
                PROBE_VERSION.to_owned()
            } else {
                "0.1.0".to_owned()
            }),
            "VERSION_MAJOR" => Some(if is_root { "9" } else { "0" }.to_owned()),
            "VERSION_MINOR" => Some(if is_root { "7" } else { "1" }.to_owned()),
            "VERSION_PATCH" => Some(if is_root { "5" } else { "0" }.to_owned()),
            "VERSION_PRE" => Some(if is_root { "cinderpre" } else { "" }.to_owned()),
            "DESCRIPTION" => Some(if is_root { PROBE_DESCRIPTION } else { "" }.to_owned()),
            "HOMEPAGE" => Some(if is_root { PROBE_HOMEPAGE } else { "" }.to_owned()),
            "REPOSITORY" => Some(if is_root { PROBE_REPOSITORY } else { "" }.to_owned()),
            "LICENSE" => Some(if is_root { PROBE_LICENSE } else { "" }.to_owned()),
            "LICENSE_FILE" => Some(String::new()),
            "AUTHORS" => Some(if is_root {
                PROBE_AUTHORS.join(":")
            } else {
                String::new()
            }),
            "RUST_VERSION" => Some(if is_root { PROBE_RUST_VERSION } else { "" }.to_owned()),
            "README" => Some(if is_root { PROBE_README } else { "" }.to_owned()),
            _ => None,
        };
        let Some(expected) = expectation else {
            return Err(format!("probe package variable {name} is not modeled"));
        };
        return if text == Some(expected.as_str()) {
            Ok(Derivation::PackageField)
        } else {
            Err(format!(
                "probe package variable {name} has an unexpected value"
            ))
        };
    }
    if name == "CARGO_SBOM_PATH" {
        return if value.is_empty() {
            Ok(Derivation::EmptyValue)
        } else {
            Err("CARGO_SBOM_PATH is unexpectedly populated".to_owned())
        };
    }
    Err(format!(
        "probe variable {name} has no witnessed derivation (unit {}, value {:?})",
        unit.crate_name,
        value.to_string_lossy()
    ))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn witness_for(_cargo: &Path) -> Option<EnvironmentWitness> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_witness() -> EnvironmentWitness {
        let mut constants = BTreeMap::new();
        constants.insert(
            "RUSTUP_TOOLCHAIN".to_owned(),
            OsString::from("stable-aarch64-apple-darwin"),
        );
        let mut derivations = BTreeMap::new();
        for (name, derivation) in [
            ("CARGO", Derivation::CargoPath),
            ("CARGO_CRATE_NAME", Derivation::CrateName),
            ("CARGO_MANIFEST_DIR", Derivation::ManifestDirectory),
            ("CARGO_MANIFEST_PATH", Derivation::ManifestPath),
            ("CARGO_PKG_NAME", Derivation::PackageField),
            ("CARGO_PKG_VERSION", Derivation::PackageField),
            ("CARGO_PRIMARY_PACKAGE", Derivation::PrimaryFlag),
            ("CARGO_SBOM_PATH", Derivation::EmptyValue),
            ("OUT_DIR", Derivation::OutDirectory),
            ("RUSTUP_TOOLCHAIN", Derivation::ToolchainConstant),
            (JOBSERVER_ENVIRONMENT, Derivation::Jobserver),
        ] {
            derivations.insert(name.to_owned(), derivation);
        }
        EnvironmentWitness {
            cargo_path: PathBuf::from("/toolchain/bin/cargo"),
            constants,
            derivations,
            loader_tail: Some(OsString::from("/toolchain/lib")),
        }
    }

    fn sample_recipe(observed: Vec<(OsString, OsString)>) -> CompilerRecipe {
        CompilerRecipe::from_observed(
            PathBuf::from("/toolchain/bin/rustc"),
            PathBuf::from("/workspace"),
            [
                "--crate-name",
                "app",
                "src/lib.rs",
                "--crate-type",
                "lib",
                "--emit",
                "metadata",
                "--out-dir",
                "/workspace/target/debug/deps",
            ]
            .map(OsString::from)
            .into(),
            Some(observed),
        )
        .unwrap()
    }

    #[test]
    fn witness_round_trip_rejects_tampering() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("cinder-witness-{}-{unique}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("witness");
        let witness = sample_witness();
        let digest = [7_u8; 32];
        write_witness(&path, &witness, &digest).unwrap();
        let decoded = read_witness(&path, &digest).unwrap();
        assert_eq!(decoded.cargo_path, witness.cargo_path);
        assert_eq!(decoded.constants, witness.constants);
        assert_eq!(decoded.derivations, witness.derivations);

        // A different invocation context never accepts the stored file.
        assert!(read_witness(&path, &[8_u8; 32]).is_none());

        let mut bytes = fs::read(&path).unwrap();
        bytes[3] ^= 0x01;
        fs::write(&path, &bytes).unwrap();
        assert!(read_witness(&path, &digest).is_none());

        write_witness(&path, &witness, &digest).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(0);
        fs::write(&path, &bytes).unwrap();
        assert!(read_witness(&path, &digest).is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn full_binding_installs_only_validated_injections() {
        let witness = sample_witness();
        let mut base = BTreeMap::new();
        base.insert(OsString::from("PATH"), OsString::from("/usr/bin"));
        base.insert(OsString::from("HOME"), OsString::from("/home/user"));
        let observed = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("HOME"), OsString::from("/home/user")),
            (
                OsString::from("CARGO"),
                OsString::from("/toolchain/bin/cargo"),
            ),
            (OsString::from("CARGO_CRATE_NAME"), OsString::from("app")),
            (
                OsString::from("CARGO_MANIFEST_DIR"),
                OsString::from("/workspace/member"),
            ),
            (
                OsString::from("CARGO_MANIFEST_PATH"),
                OsString::from("/workspace/member/Cargo.toml"),
            ),
            (OsString::from("CARGO_PKG_NAME"), OsString::from("app")),
            (OsString::from("CARGO_PKG_VERSION"), OsString::from("1.2.3")),
            (OsString::from("CARGO_PRIMARY_PACKAGE"), OsString::from("1")),
            (OsString::from("CARGO_SBOM_PATH"), OsString::from("")),
            (
                OsString::from("RUSTUP_TOOLCHAIN"),
                OsString::from("stable-aarch64-apple-darwin"),
            ),
            (
                OsString::from(JOBSERVER_ENVIRONMENT),
                OsString::from("-j --jobserver-fds=8,9 --jobserver-auth=8,9"),
            ),
        ];
        let mut recipe = sample_recipe(observed);
        bind_full_environment(
            &mut recipe,
            &witness,
            &base,
            Some(Path::new("/workspace/member")),
            None,
        )
        .unwrap();
        assert_eq!(recipe.environment_kind, EnvironmentKind::FullWitnessed);
        let keys: Vec<&OsString> = recipe.environment.iter().map(|(key, _)| key).collect();
        // Inherited variables and the jobserver are never installed.
        assert!(!keys.iter().any(|key| *key == "PATH"));
        assert!(!keys.iter().any(|key| *key == JOBSERVER_ENVIRONMENT));
        assert!(keys.iter().any(|key| *key == "CARGO_PKG_VERSION"));
        assert!(keys.iter().any(|key| *key == "RUSTUP_TOOLCHAIN"));
    }

    #[test]
    fn full_binding_refuses_unwitnessed_or_mismatched_variables() {
        let witness = sample_witness();
        let base = BTreeMap::new();
        // An unwitnessed variable.
        let mut recipe = sample_recipe(vec![(
            OsString::from("CINDER_SURPRISE"),
            OsString::from("value"),
        )]);
        assert!(
            bind_full_environment(&mut recipe, &witness, &base, None, None)
                .unwrap_err()
                .contains("not witnessed")
        );
        assert_eq!(recipe.environment_kind, EnvironmentKind::Tracked);
        // A crate-name mismatch.
        let mut recipe = sample_recipe(vec![(
            OsString::from("CARGO_CRATE_NAME"),
            OsString::from("other"),
        )]);
        assert!(bind_full_environment(&mut recipe, &witness, &base, None, None).is_err());
        // A manifest variable without a recorded manifest directory.
        let mut recipe = sample_recipe(vec![(
            OsString::from("CARGO_MANIFEST_DIR"),
            OsString::from("/workspace/member"),
        )]);
        assert!(bind_full_environment(&mut recipe, &witness, &base, None, None).is_err());
        // A missing observation entirely.
        let mut recipe = sample_recipe(Vec::new());
        recipe.observed_environment = None;
        assert!(bind_full_environment(&mut recipe, &witness, &base, None, None).is_err());
    }

    #[test]
    fn wrapper_mode_requires_both_conditions() {
        // No probe variable in this test process: the wrapper never engages,
        // even for a plausible compiler shape.
        assert!(env::var_os(PROBE_ENVIRONMENT).is_none());
        assert!(wrapper_main(&[OsString::from("/toolchain/bin/rustc")]).is_none());
    }

    #[test]
    fn dump_round_trip_and_bounds() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(DUMP_MAGIC);
        append_value(&mut bytes, OsStr::new("/workspace"));
        append_count(&mut bytes, 2);
        append_value(&mut bytes, OsStr::new("/toolchain/bin/rustc"));
        append_value(&mut bytes, OsStr::new("--crate-name"));
        append_count(&mut bytes, 1);
        append_value(&mut bytes, OsStr::new("CARGO_PKG_NAME"));
        append_value(&mut bytes, OsStr::new("value with\nnewline"));
        #[cfg(target_os = "macos")]
        {
            let dump = parse_dump(&bytes).unwrap();
            assert_eq!(dump.working_directory, PathBuf::from("/workspace"));
            assert_eq!(dump.arguments.len(), 2);
            assert_eq!(
                dump.environment,
                vec![(
                    OsString::from("CARGO_PKG_NAME"),
                    OsString::from("value with\nnewline")
                )]
            );
            let mut trailing = bytes.clone();
            trailing.push(0);
            assert!(parse_dump(&trailing).is_none());
        }
        let _ = bytes;
    }
}
