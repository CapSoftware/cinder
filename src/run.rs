use std::{
    collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher},
    env,
    ffi::{OsStr, OsString},
    fs,
    hash::{Hash, Hasher},
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Range,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::UNIX_EPOCH,
    time::{Duration, Instant, SystemTime},
};

use aho_corasick::AhoCorasick;
use fs2::FileExt;
use memmap2::MmapOptions;
use sha2::{Digest, Sha256};

const DISABLE_FAST_RUN: &str = "CINDER_DISABLE_FAST_RUN";
const DISABLE_FAST_BUILD: &str = "CINDER_DISABLE_FAST_BUILD";
pub const RUN_CONTEXT_FILE: &str = "CINDER_RUN_CONTEXT_FILE";
pub const ARTIFACT_RECEIPT_DIRECTORY: &str = "CINDER_ARTIFACT_RECEIPT_DIRECTORY";
const COALESCE_RUN_EVENTS: &str = "CINDER_COALESCE_RUN_EVENTS";
const SYNCHRONOUS_STATE_RECORDING: &str = "CINDER_SYNCHRONOUS_STATE_RECORDING";
const DUPLICATE_EVENT_SETTLE_TIME: Duration = Duration::from_millis(1_250);
const DUPLICATE_EVENT_MAX_AGE: Duration = Duration::from_secs(2);
const REVISION_HISTORY_LIMIT: usize = 8;
const REVISION_HISTORY_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const REVISION_HISTORY_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const STAGING_MAX_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchPolicy {
    Immediate,
    CoalesceDuplicateEvents,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StateKind {
    Run,
    Build,
}

impl StateKind {
    const fn directory_name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Build => "build",
        }
    }

    fn history_directory_name(self) -> String {
        format!("{}-history", self.directory_name())
    }
}

impl LaunchPolicy {
    pub fn for_invocation(invoked_as_cargo: bool) -> Self {
        if invoked_as_cargo || env::var_os(COALESCE_RUN_EVENTS).is_some() {
            Self::CoalesceDuplicateEvents
        } else {
            Self::Immediate
        }
    }

    const fn context_byte(self) -> u8 {
        match self {
            Self::Immediate => 0,
            Self::CoalesceDuplicateEvents => 1,
        }
    }
}

pub fn run_context(arguments: &[OsString], launch_policy: LaunchPolicy) -> Vec<u8> {
    let mut context = Sha256::new();
    context.update(b"CINDER-RUN-CONTEXT-7");
    context.update([launch_policy.context_byte()]);
    for argument in arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
    {
        append_context_value(&mut context, argument.as_bytes());
    }
    let mut environment: Vec<_> = env::vars_os()
        .filter(|(key, _)| environment_affects_context(key))
        .map(|(key, value)| (key.into_vec(), value.into_vec()))
        .collect();
    environment.sort();
    for (key, value) in environment {
        append_context_value(&mut context, &key);
        append_context_value(&mut context, &value);
    }
    append_tool_identity(
        &mut context,
        env::var_os("CINDER_REAL_CARGO")
            .as_deref()
            .unwrap_or(OsStr::new("cargo")),
    );
    append_tool_identity(
        &mut context,
        env::var_os("RUSTC")
            .as_deref()
            .unwrap_or(OsStr::new("rustc")),
    );
    append_rustup_identity(&mut context);
    context.finalize().to_vec()
}

fn environment_affects_context(key: &OsStr) -> bool {
    !matches!(
        key.to_str(),
        Some("_" | RUN_CONTEXT_FILE | DISABLE_FAST_RUN | DISABLE_FAST_BUILD)
    )
}

fn bind_observed_shell_environment(context: &[u8], observes_underscore: bool) -> Vec<u8> {
    if !observes_underscore {
        return context.to_vec();
    }
    let mut bound = Sha256::new();
    bound.update(b"CINDER-OBSERVED-SHELL-ENVIRONMENT-1");
    append_context_value(&mut bound, context);
    match env::var_os("_") {
        Some(value) => {
            bound.update([1]);
            append_context_value(&mut bound, value.as_bytes());
        }
        None => bound.update([0]),
    }
    bound.finalize().to_vec()
}

fn dependency_observes_environment(path: &Path, key: &[u8]) -> Result<bool, String> {
    let contents = fs::read(path).map_err(|error| {
        format!(
            "could not inspect compiler environment dependencies {}: {error}",
            path.display()
        )
    })?;
    Ok(contents.split(|byte| *byte == b'\n').any(|line| {
        line.strip_suffix(b"\r")
            .unwrap_or(line)
            .strip_prefix(b"# env-dep:")
            .and_then(|dependency| dependency.split(|byte| *byte == b'=').next())
            == Some(key)
    }))
}

fn compiler_unit_graph_observes_environment(
    outputs: &CargoOutputs,
    key: &[u8],
) -> Result<bool, String> {
    let dependency_directory = outputs.dependency_file.parent().ok_or_else(|| {
        format!(
            "compiler dependency file has no parent directory: {}",
            outputs.dependency_file.display()
        )
    })?;
    let fingerprint_root = outputs.fingerprint.parent().ok_or_else(|| {
        format!(
            "Cargo fingerprint has no parent directory: {}",
            outputs.fingerprint.display()
        )
    })?;
    let fingerprint_index = cargo_fingerprint_value_index(fingerprint_root)?;
    let profile = dependency_directory.parent().ok_or_else(|| {
        format!(
            "Cargo dependency directory has no profile parent: {}",
            dependency_directory.display()
        )
    })?;
    let dependency_index = cargo_dependency_file_index(profile)?;
    let mut pending = vec![outputs.fingerprint.clone()];
    let mut visited = BTreeSet::new();

    while let Some(fingerprint) = pending.pop() {
        if !visited.insert(fingerprint.clone()) {
            continue;
        }
        let descriptor = cargo_fingerprint_descriptor(&fingerprint)?;
        let runs_build_script = descriptor
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("run-build-script-"));
        if runs_build_script {
            let output = profile
                .join("build")
                .join(fingerprint.file_name().ok_or_else(|| {
                    format!(
                        "Cargo build-script fingerprint has no name: {}",
                        fingerprint.display()
                    )
                })?)
                .join("output");
            if build_script_output_observes_environment(&output, key)? {
                return Ok(true);
            }
        }
        let dependency_file = if fingerprint == outputs.fingerprint {
            Some(outputs.dependency_file.as_path())
        } else {
            fingerprint
                .file_name()
                .and_then(OsStr::to_str)
                .and_then(|name| name.rsplit_once('-').map(|(_, hash)| hash))
                .and_then(|hash| dependency_index.get(hash).map(PathBuf::as_path))
        };
        if let Some(dependency_file) = dependency_file {
            if dependency_observes_environment(dependency_file, key)? {
                return Ok(true);
            }
        } else if !runs_build_script {
            return Err(format!(
                "Cargo unit has no dependency file: {}",
                fingerprint.display()
            ));
        }

        let contents = fs::read(&descriptor).map_err(|error| {
            format!(
                "could not read Cargo fingerprint {}: {error}",
                descriptor.display()
            )
        })?;
        let descriptor: serde_json::Value = serde_json::from_slice(&contents).map_err(|error| {
            format!(
                "could not parse Cargo fingerprint {}: {error}",
                fingerprint.display()
            )
        })?;
        let dependencies = descriptor
            .get("deps")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                format!(
                    "Cargo fingerprint has no dependency graph: {}",
                    fingerprint.display()
                )
            })?;
        for dependency in dependencies {
            let value = dependency
                .as_array()
                .and_then(|fields| fields.get(3))
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    format!(
                        "Cargo fingerprint has an invalid dependency: {}",
                        fingerprint.display()
                    )
                })?;
            let dependencies = fingerprint_index.get(&value).ok_or_else(|| {
                format!(
                    "Cargo dependency fingerprint {value} is missing for {}",
                    fingerprint.display()
                )
            })?;
            pending.extend(dependencies.iter().cloned());
        }
    }
    Ok(false)
}

fn build_script_output_observes_environment(path: &Path, key: &[u8]) -> Result<bool, String> {
    let contents = fs::read(path).map_err(|error| {
        format!(
            "could not inspect build-script environment dependencies {}: {error}",
            path.display()
        )
    })?;
    Ok(contents.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line.strip_prefix(b"cargo:rerun-if-env-changed=")
            .or_else(|| line.strip_prefix(b"cargo::rerun-if-env-changed="))
            == Some(key)
    }))
}

fn cargo_fingerprint_value_index(root: &Path) -> Result<BTreeMap<u64, Vec<PathBuf>>, String> {
    let mut index: BTreeMap<u64, Vec<PathBuf>> = BTreeMap::new();
    for entry in fs::read_dir(root).map_err(|error| {
        format!(
            "could not inspect Cargo fingerprints {}: {error}",
            root.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect Cargo fingerprints {}: {error}",
                root.display()
            )
        })?;
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for marker in fs::read_dir(entry.path()).map_err(|error| {
            format!(
                "could not inspect Cargo fingerprint {}: {error}",
                entry.path().display()
            )
        })? {
            let marker = marker.map_err(|error| {
                format!(
                    "could not inspect Cargo fingerprint {}: {error}",
                    entry.path().display()
                )
            })?;
            if !marker.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(contents) = fs::read(marker.path()) else {
                continue;
            };
            let Some(value) = cargo_fingerprint_value(&contents) else {
                continue;
            };
            let fingerprints = index.entry(value).or_default();
            if !fingerprints.contains(&entry.path()) {
                fingerprints.push(entry.path());
            }
        }
    }
    Ok(index)
}

fn cargo_fingerprint_value(contents: &[u8]) -> Option<u64> {
    if contents.len() != 16 {
        return None;
    }
    let mut bytes = [0_u8; 8];
    for (destination, pair) in bytes.iter_mut().zip(contents.chunks_exact(2)) {
        *destination = (hexadecimal_nibble(pair[0])? << 4) | hexadecimal_nibble(pair[1])?;
    }
    Some(u64::from_le_bytes(bytes))
}

fn hexadecimal_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn cargo_dependency_file_index(root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut index = BTreeMap::new();
    index_cargo_dependency_files(&root.join("deps"), &mut index)?;
    let build = root.join("build");
    let build_entries = match fs::read_dir(&build) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(index),
        Err(error) => {
            return Err(format!(
                "could not inspect Cargo build dependencies {}: {error}",
                build.display()
            ));
        }
    };
    for entry in build_entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect Cargo build dependencies {}: {error}",
                build.display()
            )
        })?;
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            index_cargo_dependency_files(&entry.path(), &mut index)?;
        }
    }
    Ok(index)
}

fn index_cargo_dependency_files(
    root: &Path,
    index: &mut BTreeMap<String, PathBuf>,
) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|error| {
        format!(
            "could not inspect Cargo dependency files {}: {error}",
            root.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect Cargo dependency files {}: {error}",
                root.display()
            )
        })?;
        if !entry.file_type().is_ok_and(|kind| kind.is_file())
            || entry.path().extension() != Some(OsStr::new("d"))
        {
            continue;
        }
        let Some(hash) = entry
            .path()
            .file_stem()
            .and_then(OsStr::to_str)
            .and_then(|name| name.rsplit_once('-').map(|(_, hash)| hash.to_owned()))
            .filter(|hash| hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        else {
            continue;
        };
        if index.insert(hash.clone(), entry.path()).is_some() {
            return Err(format!("Cargo dependency hash {hash} is ambiguous"));
        }
    }
    Ok(())
}

fn cargo_fingerprint_descriptor(root: &Path) -> Result<PathBuf, String> {
    let mut descriptors = Vec::new();
    for entry in fs::read_dir(root).map_err(|error| {
        format!(
            "could not inspect Cargo fingerprint {}: {error}",
            root.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect Cargo fingerprint {}: {error}",
                root.display()
            )
        })?;
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry.path().extension() == Some(OsStr::new("json"))
        {
            descriptors.push(entry.path());
        }
    }
    match descriptors.as_slice() {
        [descriptor] => Ok(descriptor.clone()),
        [] => Err(format!(
            "Cargo fingerprint descriptor is missing: {}",
            root.display()
        )),
        _ => Err(format!(
            "Cargo fingerprint descriptor is ambiguous: {}",
            root.display()
        )),
    }
}

fn append_tool_identity(context: &mut Sha256, executable: &OsStr) {
    append_context_value(context, executable.as_bytes());
    let Some(path) = resolve_executable(executable) else {
        context.update([0]);
        return;
    };
    context.update([1]);
    append_path_identity(context, &path, false);
    if let Ok(canonical) = fs::canonicalize(&path) {
        append_path_identity(context, &canonical, false);
    }
}

fn resolve_executable(executable: &OsStr) -> Option<PathBuf> {
    let path = Path::new(executable);
    if path.components().count() > 1 {
        return path.is_file().then(|| absolute_path(path).ok()).flatten();
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(executable))
            .find(|candidate| candidate.is_file())
    })
}

fn append_rustup_identity(context: &mut Sha256) {
    let rustup_home = env::var_os("RUSTUP_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")));
    let Some(rustup_home) = rustup_home.filter(|path| path.is_dir()) else {
        context.update([0]);
        return;
    };
    context.update([1]);
    append_path_identity(context, &rustup_home.join("settings.toml"), true);
    let mut paths = Vec::new();
    for directory in ["update-hashes", "toolchains"] {
        let directory = rustup_home.join(directory);
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_file() {
                paths.push((path, true));
            } else if path.is_dir() {
                paths.push((path.join("bin/rustc"), false));
                paths.push((path.join("bin/cargo"), false));
            }
        }
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    for (path, include_contents) in paths {
        append_path_identity(context, &path, include_contents);
    }
}

fn append_path_identity(context: &mut Sha256, path: &Path, include_contents: bool) {
    append_context_value(context, path.as_os_str().as_bytes());
    match artifact_metadata(path) {
        Ok((size, modified_ns)) => {
            context.update([1]);
            context.update(size.to_le_bytes());
            context.update(modified_ns.to_le_bytes());
            if include_contents {
                match fs::read(path) {
                    Ok(contents) => append_context_value(context, &contents),
                    Err(error) => append_context_value(context, error.to_string().as_bytes()),
                }
            }
        }
        Err(_) => context.update([0]),
    }
}

fn append_context_value(context: &mut Sha256, value: &[u8]) {
    context.update((value.len() as u64).to_le_bytes());
    context.update(value);
}

pub fn stage_run_context(context: &[u8]) -> Result<PathBuf, String> {
    let directory = fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))?;
    let root = state_directory(&directory, StateKind::Run);
    let parent = root
        .parent()
        .ok_or_else(|| "Cinder state directory has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create Cinder context directory: {error}"))?;
    let path = parent.join(format!("run-context-{}", std::process::id()));
    fs::write(&path, context)
        .map_err(|error| format!("could not stage Cinder run context: {error}"))?;
    Ok(path)
}

pub fn stage_artifact_receipts() -> Result<PathBuf, String> {
    let root = env::temp_dir().join("cinder").join("receipts");
    fs::create_dir_all(&root)
        .map_err(|error| format!("could not create Cinder receipt directory: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let directory = root.join(format!("{}-{nonce}", std::process::id()));
    fs::create_dir(&directory)
        .map_err(|error| format!("could not stage Cinder artifact receipts: {error}"))?;
    make_private_directory(&directory)?;
    Ok(directory)
}

/// Records a primary binary produced by one wrapped rustc invocation.
///
/// Receipt failures never alter compiler success; the caller reports them as a
/// disabled optimization and the next command continues through Cargo.
pub fn record_artifact_receipt(arguments: &[OsString]) -> Result<(), String> {
    if env::var_os("CARGO_PRIMARY_PACKAGE").as_deref() != Some("1".as_ref()) {
        return Ok(());
    }
    if !rustc_list_options(arguments, "--emit")
        .iter()
        .any(|values| values.split(',').any(|value| value == "link"))
    {
        return Ok(());
    }
    let crate_types: Vec<_> = rustc_list_options(arguments, "--crate-type")
        .into_iter()
        .flat_map(|values| values.split(','))
        .filter(|kind| {
            matches!(
                *kind,
                "bin" | "lib" | "rlib" | "staticlib" | "dylib" | "cdylib"
            )
        })
        .collect();
    let [crate_type] = crate_types.as_slice() else {
        return Ok(());
    };
    if *crate_type == "bin" && env::var_os("CARGO_BIN_NAME").is_none() {
        return Ok(());
    }
    let crate_name = rustc_option(arguments, "--crate-name")
        .ok_or_else(|| "binary rustc invocation has no crate name".to_owned())?;
    let out_directory = rustc_option(arguments, "--out-dir")
        .map(PathBuf::from)
        .ok_or_else(|| "binary rustc invocation has no output directory".to_owned())?;
    let extra_filename = rustc_codegen_option(arguments, "extra-filename").unwrap_or_default();
    let artifact = out_directory.join(linked_artifact_name(
        crate_name,
        extra_filename,
        crate_type,
    )?);
    let mut dependency_name = OsString::from(crate_name);
    dependency_name.push(extra_filename);
    dependency_name.push(".d");
    let dependency_file = out_directory.join(dependency_name);
    if !artifact.is_file() {
        return Err(format!(
            "wrapped compiler did not produce {}",
            artifact.display()
        ));
    }
    let directory = env::var_os(ARTIFACT_RECEIPT_DIRECTORY)
        .map(PathBuf::from)
        .ok_or_else(|| "artifact receipt directory is not configured".to_owned())?;
    let receipt = ArtifactReceipt {
        artifact,
        dependency_file,
        public_file_name: public_artifact_name(crate_name, crate_type)?,
        crate_type: (*crate_type).to_owned(),
        manifest_directory: env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
        out_directory: env::var_os("OUT_DIR").map(PathBuf::from),
    };
    write_artifact_receipt(
        &directory.join(format!("{}.receipt", std::process::id())),
        &receipt,
    )
}

fn linked_artifact_name(
    crate_name: &OsStr,
    extra_filename: &OsStr,
    crate_type: &str,
) -> Result<OsString, String> {
    let mut name = OsString::new();
    match crate_type {
        "bin" => name.push(crate_name),
        "lib" | "rlib" => {
            name.push("lib");
            name.push(crate_name);
        }
        "staticlib" => {
            name.push("lib");
            name.push(crate_name);
        }
        "dylib" | "cdylib" => {
            name.push(env::consts::DLL_PREFIX);
            name.push(crate_name);
        }
        _ => return Err(format!("unsupported Cargo artifact type: {crate_type}")),
    }
    name.push(extra_filename);
    match crate_type {
        "bin" => name.push(env::consts::EXE_SUFFIX),
        "lib" | "rlib" => name.push(".rlib"),
        "staticlib" => name.push(".a"),
        "dylib" | "cdylib" => name.push(env::consts::DLL_SUFFIX),
        _ => return Err(format!("unsupported Cargo artifact type: {crate_type}")),
    }
    Ok(name)
}

fn public_artifact_name(crate_name: &OsStr, crate_type: &str) -> Result<OsString, String> {
    if crate_type == "bin" {
        let mut name = env::var_os("CARGO_BIN_NAME")
            .ok_or_else(|| "binary Cargo target has no public name".to_owned())?;
        name.push(env::consts::EXE_SUFFIX);
        return Ok(name);
    }
    linked_artifact_name(crate_name, OsStr::new(""), crate_type)
}

fn rustc_option<'a>(arguments: &'a [OsString], option: &str) -> Option<&'a OsStr> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == option)
        .map(|pair| pair[1].as_os_str())
        .or_else(|| {
            let prefix = format!("{option}=");
            arguments
                .iter()
                .filter_map(|argument| argument.to_str())
                .find_map(|argument| argument.strip_prefix(&prefix).map(OsStr::new))
        })
}

fn rustc_list_options<'a>(arguments: &'a [OsString], option: &str) -> Vec<&'a str> {
    let joined_prefix = format!("{option}=");
    let mut values = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if argument == option {
            if let Some(value) = arguments.get(index + 1).and_then(|value| value.to_str()) {
                values.push(value);
            }
        } else if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix(&joined_prefix))
        {
            values.push(value);
        }
    }
    values
}

fn rustc_codegen_option<'a>(arguments: &'a [OsString], option: &str) -> Option<&'a OsStr> {
    let prefix = format!("{option}=");
    arguments
        .windows(2)
        .filter(|pair| pair[0] == "-C")
        .filter_map(|pair| pair[1].to_str())
        .find_map(|argument| argument.strip_prefix(&prefix).map(OsStr::new))
        .or_else(|| {
            let joined_prefix = format!("-C{prefix}");
            arguments
                .iter()
                .filter_map(|argument| argument.to_str())
                .find_map(|argument| argument.strip_prefix(&joined_prefix).map(OsStr::new))
        })
}

/// Preserves Cargo's `run` implementation and replaces only its final target
/// runner. Cargo therefore remains responsible for package/target selection,
/// builds, diagnostics, dynamic-library paths, and the application environment.
pub fn cargo_arguments(mut arguments: Vec<OsString>) -> Result<Vec<OsString>, String> {
    if !eligible(&arguments)? {
        return Ok(arguments);
    }

    let target = host_target()?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    let runner = format!(
        "target.{target}.runner=[{},\"__run-artifact\"]",
        toml_string(cinder.as_os_str())?
    );

    arguments.insert(1, OsString::from("--config"));
    arguments.insert(2, OsString::from(runner));
    Ok(arguments)
}

/// Attempts a source-to-artifact transformation before asking Cargo to build.
/// A miss is deliberately silent: normal Cargo remains the compatibility path.
pub fn try_fast_run(
    arguments: &[OsString],
    run_context: &[u8],
    launch_policy: LaunchPolicy,
) -> Result<(), String> {
    if !eligible(arguments)? {
        return Ok(());
    }
    let directory = fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))?;
    let current = State::load(&directory, StateKind::Run)?;
    if let Some(state) = current.as_ref().filter(|state| {
        state.context_matches(run_context)
            && state.artifact_is_unchanged().unwrap_or(false)
            && state.cargo_outputs_are_available()
    }) {
        if let Some(change) = find_literal_change(&directory, &state.snapshot, &state.sources)? {
            if state.inputs_are_unchanged(&directory)? {
                if let Some(patched) =
                    patch_artifact(&directory, state, &change, PatchMode::RunSibling)?
                {
                    state.record_patched(&directory, StateKind::Run, &patched, &change, true)?;
                    eprintln!(
                        "    Cinder patched {} from {} without recompiling",
                        change.relative.display(),
                        state.artifact.display()
                    );
                    return launch_new_fast_run(
                        &directory,
                        &patched,
                        &state.program_name,
                        arguments,
                        &state.runtime_environment,
                        launch_policy,
                    );
                }
            }
        } else if state.sources_match_revision(&directory)?
            && state.inputs_are_unchanged(&directory)?
            && State::consume_fresh_duplicate(&directory)?
        {
            eprintln!("    Cinder reusing {}", state.artifact.display());
            return exec_artifact(
                &state.artifact,
                &state.program_name,
                runtime_arguments(arguments),
                &state.runtime_environment,
            )
            .map(|_| ());
        }
    }

    let Some(historical) = State::matching_history(&directory, StateKind::Run, run_context, None)?
    else {
        return Ok(());
    };
    let restored = restore_cached_run_artifact(&directory, &historical)?;
    historical.promote(&directory, StateKind::Run, &restored, true)?;
    eprintln!(
        "    Cinder restored a validated previous build of {} without recompiling",
        historical.program_name.to_string_lossy()
    );
    launch_new_fast_run(
        &directory,
        &restored,
        &historical.program_name,
        arguments,
        &historical.runtime_environment,
        launch_policy,
    )
}

/// Completes a supported binary build without invoking Cargo when the recorded
/// artifact and every non-source input are still valid.
///
/// Returning `Ok(false)` is a normal cache miss. Cargo remains responsible for
/// every unsupported command shape and for refreshing state after a miss.
pub fn try_fast_build(arguments: &[OsString], build_context: &[u8]) -> Result<bool, String> {
    if !build_eligible(arguments)? {
        return Ok(false);
    }
    let directory = canonical_current_directory()?;
    let current = State::load(&directory, StateKind::Build)?;
    if env::var_os("CINDER_TRACE_RUN").is_some() {
        eprintln!(
            "    Cinder trace: current build state={} context-match={} wanted={} recorded={}",
            current.is_some(),
            current
                .as_ref()
                .is_some_and(|state| state.context_matches(build_context)),
            short_digest(build_context),
            current.as_ref().map_or_else(
                || "none".to_owned(),
                |state| short_digest(&state.run_context)
            )
        );
    }
    let target_lock_path = match current.filter(|state| state.context_matches(build_context)) {
        Some(state) => cargo_target_lock_path(&state.public_artifact)?,
        None => {
            let Some(path) =
                State::historical_target_lock_path(&directory, StateKind::Build, build_context)?
            else {
                return Ok(false);
            };
            path
        }
    };
    let _target_lock = CargoTargetLock::acquire(&target_lock_path)?;
    try_fast_build_locked(&directory, build_context, &target_lock_path)
}

fn short_digest(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn try_fast_build_locked(
    directory: &Path,
    build_context: &[u8],
    target_lock_path: &Path,
) -> Result<bool, String> {
    let current = State::load(directory, StateKind::Build)?;
    if env::var_os("CINDER_TRACE_RUN").is_some() {
        if let Some(state) = current.as_ref() {
            eprintln!(
                "    Cinder trace: locked build context={} target={} artifact={} cargo-outputs={}",
                state.context_matches(build_context),
                cargo_target_lock_path(&state.public_artifact)
                    .is_ok_and(|path| path == target_lock_path),
                state.artifact_is_unchanged().unwrap_or(false),
                state.cargo_outputs_are_available()
            );
        }
    }
    if let Some(state) = current.filter(|state| {
        state.context_matches(build_context)
            && cargo_target_lock_path(&state.public_artifact)
                .is_ok_and(|path| path == target_lock_path)
            && state.artifact_is_unchanged().unwrap_or(false)
            && state.cargo_outputs_are_available()
    }) {
        let change = if artifact_is_executable(&state.public_artifact) {
            find_literal_change(directory, &state.snapshot, &state.sources)?
        } else {
            None
        };
        if let Some(change) = change {
            if state.inputs_are_unchanged(directory)? {
                if let Some(patched) =
                    patch_artifact(directory, &state, &change, PatchMode::BuildInPlace)?
                {
                    state.record_patched(directory, StateKind::Build, &patched, &change, false)?;
                    eprintln!(
                        "    Cinder patched {} into {} without recompiling",
                        change.relative.display(),
                        patched.display()
                    );
                    return Ok(true);
                }
            }
        } else {
            let sources_unchanged = state.sources_match_revision(directory)?;
            let inputs_unchanged = state.inputs_are_unchanged(directory)?;
            if env::var_os("CINDER_TRACE_RUN").is_some() {
                eprintln!(
                    "    Cinder trace: locked build sources={sources_unchanged} inputs={inputs_unchanged}"
                );
            }
            if sources_unchanged && inputs_unchanged {
                eprintln!(
                    "    Cinder reused {} without invoking Cargo",
                    state.artifact.display()
                );
                return Ok(true);
            }
        }
    }

    let Some(historical) = State::matching_history(
        directory,
        StateKind::Build,
        build_context,
        Some(target_lock_path),
    )?
    else {
        return Ok(false);
    };
    let restored = restore_cached_build_artifact(directory, &historical)?;
    historical.promote(directory, StateKind::Build, &restored, false)?;
    eprintln!(
        "    Cinder restored a validated previous build of {} without invoking Cargo",
        restored.display()
    );
    Ok(true)
}

fn artifact_is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

struct CargoTargetLock(fs::File);

impl CargoTargetLock {
    fn acquire(path: &Path) -> Result<Self, String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| {
                format!(
                    "could not open Cargo target lock {}: {error}",
                    path.display()
                )
            })?;
        file.lock_exclusive()
            .map_err(|error| format!("could not acquire Cargo target lock: {error}"))?;
        Ok(Self(file))
    }
}

fn cargo_target_lock_path(public_artifact: &Path) -> Result<PathBuf, String> {
    public_artifact
        .parent()
        .map(|profile| profile.join(".cargo-lock"))
        .ok_or_else(|| {
            format!(
                "public artifact has no profile directory: {}",
                public_artifact.display()
            )
        })
}

impl Drop for CargoTargetLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn launch_new_fast_run(
    directory: &Path,
    artifact: &Path,
    program_name: &OsStr,
    arguments: &[OsString],
    runtime_environment: &[(OsString, OsString)],
    launch_policy: LaunchPolicy,
) -> Result<(), String> {
    if launch_policy == LaunchPolicy::Immediate {
        return exec_artifact(
            artifact,
            program_name,
            runtime_arguments(arguments),
            runtime_environment,
        )
        .map(|_| ());
    }

    // Tauri can deliver a second event for one atomic editor save. Waiting here
    // lets that event cancel this runner; the replacement invocation consumes
    // the token and launches the already prepared artifact.
    thread::sleep(DUPLICATE_EVENT_SETTLE_TIME);
    if !State::fresh_duplicate_is_pending(directory)? {
        std::process::exit(0);
    }
    exec_artifact(
        artifact,
        program_name,
        runtime_arguments(arguments),
        runtime_environment,
    )
    .map(|_| ())
}

fn restore_cached_run_artifact(directory: &Path, historical: &State) -> Result<PathBuf, String> {
    let parent = historical.public_artifact.parent().ok_or_else(|| {
        format!(
            "public artifact has no parent: {}",
            historical.public_artifact.display()
        )
    })?;
    record_artifact_root(directory, parent)?;
    let program_name = historical.program_name.to_string_lossy();
    let artifact_digest = historical
        .artifact_digest
        .ok_or_else(|| "cached run artifact has no digest".to_owned())?;
    let restored = restored_run_artifact_path(directory, historical)?;
    if restored.is_file()
        && restored_run_artifact_is_trusted(directory, &restored, &artifact_digest)?
    {
        prune_run_artifacts(directory)?;
        return Ok(restored);
    }
    if restored.exists() {
        fs::remove_file(&restored)
            .map_err(|error| format!("could not replace restored run artifact: {error}"))?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let temporary = parent.join(format!(
        ".cinder-restore-{}-{nonce}-{program_name}",
        std::process::id(),
    ));
    let result = (|| {
        clone_file(&historical.artifact, &temporary)?;
        if !historical.cached_artifact_is_trusted()? {
            return Err("cached run artifact changed while restoring it".to_owned());
        }
        make_owner_writable(&temporary)?;
        remove_launch_xattrs(&temporary);
        make_cached_artifact_read_only(&temporary)?;
        fs::rename(&temporary, &restored)
            .map_err(|error| format!("could not publish cached run artifact: {error}"))?;
        let (restored_identity, restored_digest) = artifact_identity(&restored)?;
        if restored_digest != artifact_digest {
            return Err("cached run artifact changed while restoring it".to_owned());
        }
        record_restored_run_artifact(directory, &restored, &artifact_digest, &restored_identity)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    prune_run_artifacts(directory)?;
    Ok(restored)
}

fn restored_run_artifact_path(directory: &Path, state: &State) -> Result<PathBuf, String> {
    let parent = state.public_artifact.parent().ok_or_else(|| {
        format!(
            "public artifact has no parent: {}",
            state.public_artifact.display()
        )
    })?;
    let digest = digest_hex(
        &state
            .artifact_digest
            .ok_or_else(|| "cached run artifact has no digest".to_owned())?,
    );
    Ok(parent.join(format!(
        ".cinder-fast-{}-{digest}-{}",
        project_namespace(directory),
        state.program_name.to_string_lossy()
    )))
}

fn restored_run_artifact_receipt_path(directory: &Path, artifact: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(artifact.as_os_str().as_bytes());
    state_project_directory(directory)
        .join("run-artifact-identities")
        .join(format!("{:x}", hasher.finalize()))
}

fn record_restored_run_artifact(
    directory: &Path,
    artifact: &Path,
    digest: &[u8; 32],
    expected_identity: &ArtifactFileIdentity,
) -> Result<(), String> {
    let receipt = restored_run_artifact_receipt_path(directory, artifact);
    let parent = receipt
        .parent()
        .ok_or_else(|| "restored artifact receipt has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create restored artifact receipts: {error}"))?;
    make_private_directory(parent)?;
    let identity = artifact_file_identity(artifact)?;
    if &identity != expected_identity {
        return Err("restored run artifact changed before its receipt was published".to_owned());
    }
    let temporary = receipt.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(
        &temporary,
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            digest_hex(digest),
            identity.size,
            identity.modified_ns,
            identity.device,
            identity.inode,
            identity.changed_seconds,
            identity.changed_nanoseconds,
        ),
    )
    .map_err(|error| format!("could not stage restored artifact receipt: {error}"))?;
    fs::rename(&temporary, &receipt)
        .map_err(|error| format!("could not publish restored artifact receipt: {error}"))
}

fn restored_run_artifact_is_trusted(
    directory: &Path,
    artifact: &Path,
    digest: &[u8; 32],
) -> Result<bool, String> {
    let receipt = restored_run_artifact_receipt_path(directory, artifact);
    let contents = match fs::read_to_string(receipt) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not read restored artifact receipt: {error}")),
    };
    let mut lines = contents.lines();
    let expected_digest = digest_hex(digest);
    if lines.next() != Some(expected_digest.as_str()) {
        return Ok(false);
    }
    let recorded = ArtifactFileIdentity {
        size: parse_state_number(lines.next(), "restored artifact size")?,
        modified_ns: parse_state_number(lines.next(), "restored artifact timestamp")?,
        device: parse_state_number(lines.next(), "restored artifact device")?,
        inode: parse_state_number(lines.next(), "restored artifact inode")?,
        changed_seconds: parse_state_number(lines.next(), "restored artifact change timestamp")?,
        changed_nanoseconds: parse_state_number(
            lines.next(),
            "restored artifact change timestamp nanoseconds",
        )?,
    };
    Ok(artifact_file_identity(artifact)? == recorded
        && fs::metadata(artifact).is_ok_and(|metadata| metadata.permissions().mode() & 0o222 == 0))
}

fn digest_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn restore_cached_build_artifact(directory: &Path, historical: &State) -> Result<PathBuf, String> {
    let parent = historical.public_artifact.parent().ok_or_else(|| {
        format!(
            "public artifact has no parent: {}",
            historical.public_artifact.display()
        )
    })?;
    record_artifact_root(directory, parent)?;
    let program_name = historical.program_name.to_string_lossy();
    let temporary = parent.join(format!(
        ".cinder-restore-{}-{program_name}",
        std::process::id()
    ));
    let result = (|| {
        clone_file(&historical.artifact, &temporary)?;
        if !historical.cached_artifact_is_trusted()? {
            return Err("cached build artifact changed while restoring it".to_owned());
        }
        make_owner_writable(&temporary)?;
        remove_launch_xattrs(&temporary);
        let oldest_source = historical
            .sources
            .iter()
            .map(|source| {
                fs::metadata(directory.join(source))
                    .and_then(|metadata| metadata.modified())
                    .map_err(|error| {
                        format!(
                            "could not inspect source timestamp {}: {error}",
                            source.display()
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .min()
            .ok_or_else(|| "cached build has no source timestamps".to_owned())?;
        let stale_time = oldest_source
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(UNIX_EPOCH);
        fs::File::open(&temporary)
            .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(stale_time)))
            .map_err(|error| format!("could not make cached artifact Cargo-stale: {error}"))?;
        fs::rename(&temporary, &historical.public_artifact)
            .map_err(|error| format!("could not publish cached build artifact: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(historical.public_artifact.clone())
}

pub fn artifact_capture_eligible(arguments: &[OsString]) -> Result<bool, String> {
    match arguments.first().and_then(|argument| argument.to_str()) {
        Some("run") => eligible(arguments),
        Some("build") => build_eligible(arguments),
        _ => Ok(false),
    }
}

fn eligible(arguments: &[OsString]) -> Result<bool, String> {
    if !cfg!(target_os = "macos")
        || env::var_os(DISABLE_FAST_RUN).is_some()
        || arguments.first().and_then(|value| value.to_str()) != Some("run")
    {
        return Ok(false);
    }

    let cargo_arguments = arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
        .filter_map(|argument| argument.to_str());
    if cargo_arguments.clone().any(unsupported_argument) {
        return Ok(false);
    }
    if env::var_os("CARGO_BUILD_TARGET").is_some()
        || env::vars_os().any(|(key, _)| runner_environment_key(&key))
    {
        return Ok(false);
    }

    Ok(!cargo_config_may_change_runner_or_target()?)
}

fn build_eligible(arguments: &[OsString]) -> Result<bool, String> {
    if !cfg!(target_os = "macos")
        || env::var_os(DISABLE_FAST_BUILD).is_some()
        || arguments.first().and_then(|value| value.to_str()) != Some("build")
    {
        return Ok(false);
    }

    let cargo_arguments = arguments.iter().filter_map(|argument| argument.to_str());
    if cargo_arguments
        .clone()
        .any(|argument| unsupported_argument(argument) || unsupported_build_argument(argument))
    {
        return Ok(false);
    }
    if env::var_os("CARGO_BUILD_TARGET").is_some() {
        return Ok(false);
    }
    Ok(!cargo_config_may_change_runner_or_target()?)
}

fn unsupported_build_argument(argument: &str) -> bool {
    matches!(
        argument,
        "--lib"
            | "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--workspace"
            | "--all"
            | "--timings"
            | "--build-plan"
            | "--unit-graph"
    ) || [
        "--example",
        "--test",
        "--bench",
        "--message-format",
        "--artifact-dir",
    ]
    .iter()
    .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
}

fn canonical_current_directory() -> Result<PathBuf, String> {
    fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))
}

fn unsupported_argument(argument: &str) -> bool {
    argument == "--release"
        || argument == "-r"
        || argument == "--target"
        || argument.starts_with("--target=")
        || argument == "--profile"
        || argument.starts_with("--profile=")
        || argument == "--config"
        || argument.starts_with("--config=")
}

fn runner_environment_key(key: &OsStr) -> bool {
    key.to_str()
        .is_some_and(|key| key.starts_with("CARGO_TARGET_") && key.ends_with("_RUNNER"))
}

fn cargo_config_may_change_runner_or_target() -> Result<bool, String> {
    let mut directories = Vec::new();
    let mut directory = env::current_dir()
        .map_err(|error| format!("could not inspect the current directory: {error}"))?;
    loop {
        directories.push(directory.join(".cargo"));
        if !directory.pop() {
            break;
        }
    }
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        directories.push(PathBuf::from(cargo_home));
    } else if let Some(home) = env::var_os("HOME") {
        directories.push(PathBuf::from(home).join(".cargo"));
    }

    for directory in directories {
        for name in ["config.toml", "config"] {
            let path = directory.join(name);
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "could not inspect Cargo configuration {}: {error}",
                        path.display()
                    ));
                }
            };
            if cargo_config_contents_may_change_runner_or_target(&contents).map_err(|error| {
                format!(
                    "could not parse Cargo configuration {}: {error}",
                    path.display()
                )
            })? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn cargo_config_contents_may_change_runner_or_target(contents: &str) -> Result<bool, String> {
    let config = toml::from_str::<toml::Table>(contents).map_err(|error| error.to_string())?;
    let build_target = config
        .get("build")
        .and_then(toml::Value::as_table)
        .is_some_and(|build| build.contains_key("target"));
    let target_runner = config
        .get("target")
        .and_then(toml::Value::as_table)
        .is_some_and(|targets| {
            targets.values().any(|target| {
                target
                    .as_table()
                    .is_some_and(|target| target.contains_key("runner"))
            })
        });
    Ok(build_target || target_runner)
}

fn host_target() -> Result<String, String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|error| format!("could not query the active Rust target: {error}"))?;
    if !output.status.success() {
        return Err("rustc -vV failed while querying the active Rust target".to_owned());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV did not report a host target".to_owned())
}

fn toml_string(value: &OsStr) -> Result<String, String> {
    let value = value
        .to_str()
        .ok_or_else(|| "the Cinder executable path must be valid UTF-8".to_owned())?;
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                return Err("the Cinder executable path contains a control character".to_owned());
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    Ok(escaped)
}

pub fn run_artifact(mut arguments: Vec<OsString>) -> Result<u8, String> {
    if arguments.is_empty() {
        return Err("artifact runner requires an executable path".to_owned());
    }
    let artifact = absolute_path(Path::new(&arguments.remove(0)))?;
    let program_name = artifact
        .file_name()
        .ok_or_else(|| format!("Cargo artifact has no file name: {}", artifact.display()))?
        .to_owned();
    let directory = fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))?;
    let run_context = take_run_context();
    if let Err(error) = run_context.and_then(|run_context| {
        schedule_run_state(&directory, &artifact, &program_name, &run_context)
    }) {
        eprintln!("cinder: could not prepare the next fast run: {error}");
    }
    eprintln!("     Running `{}`", artifact.display());
    exec_artifact(&artifact, &program_name, arguments.iter(), &[])
}

fn schedule_run_state(
    directory: &Path,
    artifact: &Path,
    program_name: &OsStr,
    run_context: &[u8],
) -> Result<(), String> {
    let receipt_directory = env::var_os(ARTIFACT_RECEIPT_DIRECTORY).map(PathBuf::from);
    if env::var_os(SYNCHRONOUS_STATE_RECORDING).is_some() {
        let receipt = receipt_directory
            .as_deref()
            .map(|directory| matching_artifact_receipt(directory, artifact))
            .transpose()?
            .flatten();
        let result = State::record_fresh(
            directory,
            StateKind::Run,
            artifact,
            program_name,
            run_context,
            receipt.as_ref(),
        );
        if let Some(directory) = receipt_directory {
            let _ = fs::remove_dir_all(directory);
        }
        return result;
    }
    let root = env::temp_dir().join("cinder").join("recordings");
    fs::create_dir_all(&root)
        .map_err(|error| format!("could not create state recording directory: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let context_path = root.join(format!("run-{}-{nonce}", std::process::id()));
    fs::write(&context_path, run_context)
        .map_err(|error| format!("could not stage run state context: {error}"))?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    let mut command = Command::new(cinder);
    command
        .arg("__record-run")
        .arg(artifact)
        .arg(program_name)
        .arg(&context_path)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(receipt_directory) = receipt_directory {
        command.arg(receipt_directory);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start the run state recorder: {error}"))
}

pub fn record_run_state_command(arguments: &[OsString]) -> Result<u8, String> {
    if !(3..=4).contains(&arguments.len()) {
        return Err(
            "run state recorder requires artifact, program, context, and optional receipt paths"
                .to_owned(),
        );
    }
    let artifact = &arguments[0];
    let program_name = &arguments[1];
    let context_path = &arguments[2];
    let receipt_directory = arguments.get(3).map(PathBuf::from);
    let context = fs::read(context_path)
        .map_err(|error| format!("could not read run state context: {error}"))?;
    let directory = canonical_current_directory()?;
    let receipt = receipt_directory
        .as_deref()
        .map(|directory| matching_artifact_receipt(directory, Path::new(artifact)))
        .transpose()?
        .flatten();
    let result = State::record_fresh(
        &directory,
        StateKind::Run,
        Path::new(artifact),
        program_name,
        &context,
        receipt.as_ref(),
    );
    let _ = fs::remove_file(context_path);
    if let Some(directory) = receipt_directory {
        let _ = fs::remove_dir_all(directory);
    }
    result.map(|()| 0)
}

fn matching_artifact_receipt(
    directory: &Path,
    artifact: &Path,
) -> Result<Option<ArtifactReceipt>, String> {
    let artifact = absolute_path(artifact)?;
    let mut matching = Vec::new();
    for receipt in read_artifact_receipts(directory)? {
        if public_artifact(&receipt).ok().as_deref() == Some(artifact.as_path()) {
            matching.push(receipt);
        }
    }
    match matching.len() {
        0 => Ok(None),
        1 => Ok(matching.pop()),
        _ => Err(format!(
            "compiler artifact receipt is ambiguous for {}",
            artifact.display()
        )),
    }
}

pub fn record_completed_build(
    receipt_directory: &Path,
    build_context: &[u8],
    selects_binary: bool,
) -> Result<bool, String> {
    let result = (|| {
        let receipts = read_artifact_receipts(receipt_directory)?;
        let mut artifacts = BTreeMap::<PathBuf, ArtifactReceipt>::new();
        for receipt in receipts {
            if selects_binary && receipt.crate_type != "bin" {
                continue;
            }
            let artifact = public_artifact(&receipt)?;
            artifacts.entry(artifact).or_insert(receipt);
        }
        if artifacts.len() != 1 {
            if env::var_os("CINDER_TRACE_RUN").is_some() {
                eprintln!(
                    "    Cinder trace: expected one executable receipt, found {}",
                    artifacts.len()
                );
            }
            return Ok(false);
        }
        let (artifact, receipt) = artifacts
            .into_iter()
            .next()
            .ok_or_else(|| "primary executable disappeared from build state".to_owned())?;
        let directory = canonical_current_directory()?;
        let program_name = artifact
            .file_name()
            .ok_or_else(|| format!("Cargo artifact has no file name: {}", artifact.display()))?;
        State::record_fresh(
            &directory,
            StateKind::Build,
            &artifact,
            program_name,
            build_context,
            Some(&receipt),
        )?;
        Ok(true)
    })();
    let _ = fs::remove_dir_all(receipt_directory);
    result
}

pub fn schedule_completed_build(
    receipt_directory: &Path,
    build_context: &[u8],
    selects_binary: bool,
) -> Result<(), String> {
    if env::var_os(SYNCHRONOUS_STATE_RECORDING).is_some() {
        let _ = record_completed_build(receipt_directory, build_context, selects_binary)?;
        return Ok(());
    }
    let context_path = receipt_directory.join("build-context");
    fs::write(&context_path, build_context)
        .map_err(|error| format!("could not stage build state context: {error}"))?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    Command::new(cinder)
        .arg("__record-build")
        .arg(receipt_directory)
        .arg(&context_path)
        .arg(if selects_binary { "bin" } else { "single" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start the build state recorder: {error}"))
}

pub fn record_build_state_command(arguments: Vec<OsString>) -> Result<u8, String> {
    let [receipt_directory, context_path, selection] = <[OsString; 3]>::try_from(arguments)
        .map_err(|_| "build state recorder requires receipt, context, and selection".to_owned())?;
    let selects_binary = match selection.to_str() {
        Some("bin") => true,
        Some("single") => false,
        _ => return Err("build state recorder has an invalid selection".to_owned()),
    };
    let context = fs::read(&context_path)
        .map_err(|error| format!("could not read build state context: {error}"))?;
    let _ = record_completed_build(Path::new(&receipt_directory), &context, selects_binary)?;
    Ok(0)
}

fn read_artifact_receipts(directory: &Path) -> Result<Vec<ArtifactReceipt>, String> {
    let mut receipts = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("could not inspect artifact receipts: {error}"))?
    {
        let path = entry
            .map_err(|error| format!("could not inspect artifact receipt: {error}"))?
            .path();
        if path.extension() == Some("receipt".as_ref()) {
            receipts.push(read_artifact_receipt(&path)?);
        }
    }
    Ok(receipts)
}

fn public_artifact(receipt: &ArtifactReceipt) -> Result<PathBuf, String> {
    let artifact = absolute_path(&receipt.artifact)?;
    let parent = artifact
        .parent()
        .ok_or_else(|| format!("artifact has no parent: {}", artifact.display()))?;
    let file_name = &receipt.public_file_name;
    let mut candidates = vec![parent.join(file_name)];
    if let Some(profile_directory) = parent.parent() {
        candidates.push(profile_directory.join(file_name));
    }
    candidates.sort();
    candidates.dedup();
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| candidate.is_file())
        .collect();
    match candidates.as_slice() {
        [candidate] => absolute_path(candidate),
        [] => Err(format!(
            "could not find Cargo's public artifact for {}",
            artifact.display()
        )),
        _ => {
            let metadata = artifact_metadata(&artifact)?;
            let matching: Vec<_> = candidates
                .into_iter()
                .filter(|candidate| artifact_metadata(candidate).ok() == Some(metadata))
                .collect();
            match matching.as_slice() {
                [candidate] => absolute_path(candidate),
                _ => Err(format!(
                    "Cargo's public artifact is ambiguous for {}",
                    artifact.display()
                )),
            }
        }
    }
}

fn take_run_context() -> Result<Vec<u8>, String> {
    let path = env::var_os(RUN_CONTEXT_FILE)
        .map(PathBuf::from)
        .ok_or_else(|| "Cargo did not provide a Cinder run context".to_owned())?;
    let context =
        fs::read(&path).map_err(|error| format!("could not read Cinder run context: {error}"))?;
    let _ = fs::remove_file(path);
    Ok(context)
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()
            .map_err(|error| format!("could not inspect the current directory: {error}"))?
            .join(path)
    };
    fs::canonicalize(&path).map_err(|error| {
        format!(
            "could not resolve Cargo artifact {}: {error}",
            path.display()
        )
    })
}

fn runtime_arguments(arguments: &[OsString]) -> impl Iterator<Item = &OsString> {
    arguments
        .iter()
        .skip_while(|argument| argument.as_os_str() != "--")
        .skip(1)
}

fn exec_artifact<'a>(
    artifact: &Path,
    program_name: &OsStr,
    arguments: impl IntoIterator<Item = &'a OsString>,
    runtime_environment: &[(OsString, OsString)],
) -> Result<u8, String> {
    let mut command = Command::new(artifact);
    command.args(arguments);
    command.arg0(program_name);
    command.env(DISABLE_FAST_RUN, "1");
    command.envs(runtime_environment.iter().map(|(key, value)| (key, value)));
    crate::command::restore_runtime_environment(&mut command);
    let error = command.exec();
    Err(format!(
        "could not execute artifact {}: {error}",
        artifact.display()
    ))
}

struct State {
    snapshot: PathBuf,
    source_digest: [u8; 32],
    artifact: PathBuf,
    public_artifact: PathBuf,
    program_name: OsString,
    artifact_file_identity: ArtifactFileIdentity,
    artifact_digest: Option<[u8; 32]>,
    literal_index: Vec<LiteralIndexEntry>,
    run_context: Vec<u8>,
    observes_underscore: bool,
    inputs: Vec<InputEntry>,
    sources: Vec<PathBuf>,
    cargo_outputs: CargoOutputs,
    runtime_environment: Vec<(OsString, OsString)>,
}

#[derive(Clone)]
struct CargoOutputs {
    dependency_file: PathBuf,
    artifact: PathBuf,
    fingerprint: PathBuf,
}

struct ArtifactReceipt {
    artifact: PathBuf,
    dependency_file: PathBuf,
    public_file_name: OsString,
    crate_type: String,
    manifest_directory: Option<PathBuf>,
    out_directory: Option<PathBuf>,
}

const ARTIFACT_RECEIPT_MAGIC: &[u8; 8] = b"CNDR0003";

fn write_artifact_receipt(path: &Path, receipt: &ArtifactReceipt) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create artifact receipt: {error}"))?;
    file.write_all(ARTIFACT_RECEIPT_MAGIC)
        .map_err(|error| format!("could not write artifact receipt: {error}"))?;
    write_optional_receipt_value(&mut file, Some(receipt.artifact.as_os_str()))?;
    write_optional_receipt_value(&mut file, Some(receipt.dependency_file.as_os_str()))?;
    write_optional_receipt_value(&mut file, Some(&receipt.public_file_name))?;
    write_optional_receipt_value(&mut file, Some(OsStr::new(&receipt.crate_type)))?;
    write_optional_receipt_value(
        &mut file,
        receipt.manifest_directory.as_deref().map(Path::as_os_str),
    )?;
    write_optional_receipt_value(
        &mut file,
        receipt.out_directory.as_deref().map(Path::as_os_str),
    )
}

fn write_optional_receipt_value(file: &mut fs::File, value: Option<&OsStr>) -> Result<(), String> {
    let Some(value) = value else {
        return file
            .write_all(&u32::MAX.to_le_bytes())
            .map_err(|error| format!("could not write artifact receipt: {error}"));
    };
    let value = value.as_bytes();
    let length =
        u32::try_from(value.len()).map_err(|_| "artifact receipt value is too long".to_owned())?;
    file.write_all(&length.to_le_bytes())
        .and_then(|()| file.write_all(value))
        .map_err(|error| format!("could not write artifact receipt: {error}"))
}

fn read_artifact_receipt(path: &Path) -> Result<ArtifactReceipt, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open artifact receipt: {error}"))?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic)
        .map_err(|error| format!("could not read artifact receipt: {error}"))?;
    if &magic != ARTIFACT_RECEIPT_MAGIC {
        return Err("artifact receipt has an unsupported format".to_owned());
    }
    let artifact = read_receipt_value(&mut file)?
        .map(PathBuf::from)
        .ok_or_else(|| "artifact receipt has no artifact path".to_owned())?;
    let dependency_file = read_receipt_value(&mut file)?
        .map(PathBuf::from)
        .ok_or_else(|| "artifact receipt has no dependency path".to_owned())?;
    let public_file_name = read_receipt_value(&mut file)?
        .ok_or_else(|| "artifact receipt has no public file name".to_owned())?;
    let crate_type = read_receipt_value(&mut file)?
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| "artifact receipt has no valid crate type".to_owned())?;
    let manifest_directory = read_receipt_value(&mut file)?.map(PathBuf::from);
    let out_directory = read_receipt_value(&mut file)?.map(PathBuf::from);
    Ok(ArtifactReceipt {
        artifact,
        dependency_file,
        public_file_name,
        crate_type,
        manifest_directory,
        out_directory,
    })
}

fn read_receipt_value(file: &mut fs::File) -> Result<Option<OsString>, String> {
    let mut length = [0; 4];
    file.read_exact(&mut length)
        .map_err(|error| format!("could not read artifact receipt: {error}"))?;
    let length = u32::from_le_bytes(length);
    if length == u32::MAX {
        return Ok(None);
    }
    if length > 1_048_576 {
        return Err("artifact receipt value is too long".to_owned());
    }
    let mut value = vec![0; length as usize];
    file.read_exact(&mut value)
        .map_err(|error| format!("could not read artifact receipt: {error}"))?;
    Ok(Some(OsString::from_vec(value)))
}

#[derive(Clone)]
struct LiteralIndexEntry {
    bytes: Vec<u8>,
    offset: u64,
}

#[derive(Clone)]
struct InputEntry {
    path: PathBuf,
    identity: ArtifactFileIdentity,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArtifactFileIdentity {
    size: u64,
    modified_ns: u128,
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Clone, Copy)]
struct StatePublication<'a> {
    source_root: &'a Path,
    source_digest: &'a [u8; 32],
    artifact: &'a Path,
    artifact_file_identity: &'a ArtifactFileIdentity,
    artifact_digest: Option<&'a [u8; 32]>,
    public_artifact: &'a Path,
    program_name: &'a OsStr,
    literal_index: &'a [LiteralIndexEntry],
    run_context: &'a [u8],
    observes_underscore: bool,
    inputs: &'a [InputEntry],
    sources: &'a [PathBuf],
    cargo_outputs: &'a CargoOutputs,
    runtime_environment: &'a [(OsString, OsString)],
    duplicate_ready: bool,
}

impl State {
    fn load(directory: &Path, kind: StateKind) -> Result<Option<Self>, String> {
        let root = state_directory(directory, kind);
        Self::load_from(&root)
    }

    fn load_from(root: &Path) -> Result<Option<Self>, String> {
        let artifact_bytes = match fs::read(root.join("artifact")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("could not read Cinder run state: {error}")),
        };
        let metadata_text = fs::read_to_string(root.join("artifact-metadata"))
            .map_err(|error| format!("could not read Cinder artifact metadata: {error}"))?;
        let mut metadata = metadata_text.lines();
        let artifact_file_identity = ArtifactFileIdentity {
            size: parse_state_number(metadata.next(), "artifact size")?,
            modified_ns: parse_state_number(metadata.next(), "artifact timestamp")?,
            device: parse_state_number(metadata.next(), "artifact device")?,
            inode: parse_state_number(metadata.next(), "artifact inode")?,
            changed_seconds: parse_state_number(metadata.next(), "artifact change timestamp")?,
            changed_nanoseconds: parse_state_number(
                metadata.next(),
                "artifact change timestamp nanoseconds",
            )?,
        };
        let artifact_digest = match fs::read(root.join("artifact-digest")) {
            Ok(value) => Some(
                <[u8; 32]>::try_from(value)
                    .map_err(|_| "Cinder artifact digest has an invalid length".to_owned())?,
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!("could not read Cinder artifact digest: {error}"));
            }
        };
        let program_name = fs::read(root.join("program-name"))
            .map(OsString::from_vec)
            .map_err(|error| format!("could not read Cinder program name: {error}"))?;
        let literal_index_path = root.join("literal-index");
        if !literal_index_path.is_file() {
            return Ok(None);
        }
        let literal_index = read_literal_index(&literal_index_path)?;
        let run_context_path = root.join("run-context");
        if !run_context_path.is_file() {
            return Ok(None);
        }
        let run_context = fs::read(run_context_path)
            .map_err(|error| format!("could not read Cinder run context: {error}"))?;
        let observes_underscore = match fs::read(root.join("observes-underscore")) {
            Ok(value) if value == b"0" => false,
            Ok(value) if value == b"1" => true,
            Ok(_) => {
                return Err("Cinder observed-environment state is invalid".to_owned());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "could not read Cinder observed-environment state: {error}"
                ));
            }
        };
        let inputs_path = root.join("inputs");
        if !inputs_path.is_file() {
            return Ok(None);
        }
        let inputs = read_inputs(&inputs_path)?;
        let sources_path = root.join("sources");
        if !sources_path.is_file() {
            return Ok(None);
        }
        let sources = read_source_paths(&sources_path)?;
        let source_digest = match fs::read(root.join("source-digest")) {
            Ok(value) => <[u8; 32]>::try_from(value)
                .map_err(|_| "Cinder source digest has an invalid length".to_owned())?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("could not read Cinder source digest: {error}"));
            }
        };
        let cargo_outputs_path = root.join("cargo-outputs");
        if !cargo_outputs_path.is_file() {
            return Ok(None);
        }
        let cargo_outputs = read_cargo_outputs(&cargo_outputs_path)?;
        let runtime_environment_path = root.join("runtime-environment");
        if !runtime_environment_path.is_file() {
            return Ok(None);
        }
        let runtime_environment = read_runtime_environment(&runtime_environment_path)?;
        let artifact = PathBuf::from(OsString::from_vec(artifact_bytes));
        let public_artifact = match fs::read(root.join("public-artifact")) {
            Ok(value) => PathBuf::from(OsString::from_vec(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => artifact.clone(),
            Err(error) => {
                return Err(format!(
                    "could not read Cinder public artifact path: {error}"
                ));
            }
        };
        Ok(Some(Self {
            snapshot: root.join("snapshot"),
            source_digest,
            artifact,
            public_artifact,
            program_name,
            artifact_file_identity,
            artifact_digest,
            literal_index,
            run_context,
            observes_underscore,
            inputs,
            sources,
            cargo_outputs,
            runtime_environment,
        }))
    }

    fn record_fresh(
        directory: &Path,
        kind: StateKind,
        artifact: &Path,
        program_name: &OsStr,
        run_context: &[u8],
        receipt: Option<&ArtifactReceipt>,
    ) -> Result<(), String> {
        let cargo_outputs = cargo_outputs_for_artifact(artifact, receipt)?;
        let observes_underscore = compiler_unit_graph_observes_environment(&cargo_outputs, b"_")?;
        let recorded_context = bind_observed_shell_environment(run_context, observes_underscore);
        let source_dependency_file = if artifact_is_executable(artifact) {
            let public_dependency_file = artifact.with_extension("d");
            if public_dependency_file.is_file() {
                public_dependency_file
            } else {
                cargo_outputs.dependency_file.clone()
            }
        } else {
            cargo_outputs.dependency_file.clone()
        };
        let sources = build_source_paths(directory, artifact, &source_dependency_file)?;
        let inherited_inputs = if receipt.is_none() {
            Self::load(directory, kind)
                .ok()
                .flatten()
                .and_then(|state| {
                    (state.artifact == artifact
                        && state.sources == sources
                        && state.artifact_is_unchanged().unwrap_or(false)
                        && state.inputs_are_unchanged(directory).unwrap_or(false))
                    .then_some(state.inputs)
                })
        } else {
            None
        };
        let runtime_environment = if kind == StateKind::Run {
            runtime_linker_environment()
        } else {
            Vec::new()
        };
        if receipt.is_none()
            && inherited_inputs.is_none()
            && project_may_have_build_script(directory, &sources)?
        {
            return Err(
                "Cargo produced no compiler receipt for a package with a build script; using Cargo for safety"
                    .to_owned(),
            );
        }
        let capture = state_directory(directory, kind)
            .with_extension(format!("capture-{}", std::process::id()));
        if capture.exists() {
            fs::remove_dir_all(&capture)
                .map_err(|error| format!("could not reset source capture: {error}"))?;
        }
        fs::create_dir_all(&capture)
            .map_err(|error| format!("could not create source capture: {error}"))?;
        make_private_directory(&capture)?;
        let result = (|| {
            snapshot_sources(directory, &capture, &sources)?;
            if !sources_are_unchanged(directory, &capture, &sources)? {
                return Err("project sources changed while recording build state".to_owned());
            }
            let (_, artifact_modified_ns) = artifact_metadata(artifact)?;
            for source in &sources {
                let (_, modified_ns) = artifact_metadata(&directory.join(source))?;
                if modified_ns > artifact_modified_ns {
                    return Err(format!(
                        "{} changed after the Cargo artifact was produced",
                        source.display()
                    ));
                }
            }
            let literal_index = build_literal_index(&capture, &sources, artifact)?;
            let inputs = match &inherited_inputs {
                Some(inputs) => inputs.clone(),
                None => build_inputs(
                    directory,
                    artifact,
                    &cargo_outputs.dependency_file,
                    &sources,
                    receipt,
                )?,
            };
            if inputs
                .iter()
                .any(|input| input.identity.modified_ns > artifact_modified_ns)
                || !input_entries_are_unchanged(&inputs)
                || !sources_are_unchanged(directory, &capture, &sources)?
            {
                return Err("build inputs changed while recording build state".to_owned());
            }
            let (artifact_file_identity, artifact_digest) = artifact_identity(artifact)?;
            if !input_entries_are_unchanged(&inputs)
                || !sources_are_unchanged(directory, &capture, &sources)?
            {
                return Err("build inputs changed while recording build state".to_owned());
            }
            let source_digest = source_revision_digest(&capture, &sources)?;
            Self::publish(
                directory,
                kind,
                StatePublication {
                    source_root: &capture,
                    source_digest: &source_digest,
                    artifact,
                    artifact_file_identity: &artifact_file_identity,
                    artifact_digest: Some(&artifact_digest),
                    public_artifact: artifact,
                    program_name,
                    literal_index: &literal_index,
                    run_context: &recorded_context,
                    observes_underscore,
                    inputs: &inputs,
                    sources: &sources,
                    cargo_outputs: &cargo_outputs,
                    runtime_environment: &runtime_environment,
                    duplicate_ready: false,
                },
            )?;
            Self::cache_current(directory, kind)
        })();
        let _ = fs::remove_dir_all(capture);
        result
    }

    fn record_patched(
        &self,
        directory: &Path,
        kind: StateKind,
        artifact: &Path,
        change: &LiteralChange,
        duplicate_ready: bool,
    ) -> Result<(), String> {
        let mut literal_index = self.literal_index.clone();
        if let Some((old, new, _)) = self.indexed_patch(change) {
            if literal_index.iter().any(|entry| entry.bytes == new) {
                literal_index.retain(|entry| entry.bytes != old);
            } else if let Some(entry) = literal_index.iter_mut().find(|entry| entry.bytes == old) {
                entry.bytes = new.to_vec();
            }
        }
        let capture = state_directory(directory, kind)
            .with_extension(format!("patch-{}", std::process::id()));
        if capture.exists() {
            fs::remove_dir_all(&capture)
                .map_err(|error| format!("could not reset patched source capture: {error}"))?;
        }
        fs::create_dir_all(&capture)
            .map_err(|error| format!("could not create patched source capture: {error}"))?;
        make_private_directory(&capture)?;
        let result = (|| {
            snapshot_sources(&self.snapshot, &capture, &self.sources)?;
            fs::write(capture.join(&change.relative), &change.new_source).map_err(|error| {
                format!(
                    "could not capture patched source {}: {error}",
                    change.relative.display()
                )
            })?;
            let source_digest = source_revision_digest(&capture, &self.sources)?;
            let artifact_file_identity = artifact_file_identity(artifact)?;
            Self::publish(
                directory,
                kind,
                StatePublication {
                    source_root: &capture,
                    source_digest: &source_digest,
                    artifact,
                    artifact_file_identity: &artifact_file_identity,
                    artifact_digest: None,
                    public_artifact: &self.public_artifact,
                    program_name: &self.program_name,
                    literal_index: &literal_index,
                    run_context: &self.run_context,
                    observes_underscore: self.observes_underscore,
                    inputs: &self.inputs,
                    sources: &self.sources,
                    cargo_outputs: &self.cargo_outputs,
                    runtime_environment: &self.runtime_environment,
                    duplicate_ready,
                },
            )
        })();
        let _ = fs::remove_dir_all(capture);
        result
    }

    fn publish(
        directory: &Path,
        kind: StateKind,
        publication: StatePublication<'_>,
    ) -> Result<(), String> {
        let project_root = state_project_directory(directory);
        fs::create_dir_all(&project_root)
            .map_err(|error| format!("could not create Cinder project state: {error}"))?;
        make_private_directory(&project_root)?;
        fs::write(
            project_root.join("workspace"),
            directory.as_os_str().as_bytes(),
        )
        .map_err(|error| format!("could not record Cinder workspace: {error}"))?;
        let root = state_directory(directory, kind);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock predates the Unix epoch".to_owned())?
            .as_nanos();
        let temporary = root.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
        if temporary.exists() {
            fs::remove_dir_all(&temporary)
                .map_err(|error| format!("could not reset temporary Cinder state: {error}"))?;
        }
        write_state_directory(
            &temporary,
            publication,
            publication.artifact,
            publication.artifact,
        )?;
        if root.exists() {
            fs::remove_dir_all(&root)
                .map_err(|error| format!("could not replace Cinder run state: {error}"))?;
        }
        fs::rename(&temporary, &root)
            .map_err(|error| format!("could not publish Cinder run state: {error}"))?;
        Ok(())
    }

    fn cache_current(directory: &Path, kind: StateKind) -> Result<(), String> {
        let Some(state) = Self::load(directory, kind)? else {
            return Ok(());
        };
        let Some(artifact_digest) = state.artifact_digest else {
            return Ok(());
        };
        if !state.artifact_is_unchanged()? || !state.source_snapshot_matches_revision()? {
            return Ok(());
        }

        let history = history_directory(directory, kind);
        fs::create_dir_all(&history)
            .map_err(|error| format!("could not create Cinder revision history: {error}"))?;
        make_private_directory(&history)?;
        let key = state.history_key()?;
        let entry = history.join(&key);
        if entry.is_dir() {
            let valid = Self::load_from(&entry)
                .ok()
                .flatten()
                .is_some_and(|cached| {
                    cached.cached_artifact_is_trusted().unwrap_or(false)
                        && cached.artifact_digest == Some(artifact_digest)
                        && cached.source_snapshot_matches_revision().unwrap_or(false)
                        && cached.history_key().as_deref() == Ok(key.as_str())
                });
            if valid {
                touch_history_entry(&entry)?;
                prune_history(&history)?;
                return prune_global_history();
            }
            fs::remove_dir_all(&entry)
                .map_err(|error| format!("could not replace invalid revision history: {error}"))?;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock predates the Unix epoch".to_owned())?
            .as_nanos();
        let temporary = history.join(format!(".tmp-{}-{nonce}", std::process::id()));
        fs::create_dir(&temporary)
            .map_err(|error| format!("could not stage Cinder revision history: {error}"))?;
        make_private_directory(&temporary)?;
        let temporary_artifact = temporary.join("cached-artifact");
        let cached_artifact = entry.join("cached-artifact");
        let result = (|| {
            clone_file(&state.artifact, &temporary_artifact)?;
            if !state.artifact_is_unchanged()? {
                return Err("Cargo artifact changed while retaining revision history".to_owned());
            }
            // Content is proven before publication. Removing write bits lets
            // later hits use the recorded full file identity instead of an
            // O(artifact size) hash on every hot reload.
            make_cached_artifact_read_only(&temporary_artifact)?;
            let (cached_artifact_identity, cached_digest) = artifact_identity(&temporary_artifact)?;
            if cached_digest != artifact_digest {
                return Err("Cargo artifact changed while retaining revision history".to_owned());
            }
            write_state_directory(
                &temporary,
                StatePublication {
                    source_root: &state.snapshot,
                    source_digest: &state.source_digest,
                    artifact: &temporary_artifact,
                    artifact_file_identity: &cached_artifact_identity,
                    artifact_digest: Some(&artifact_digest),
                    public_artifact: &state.public_artifact,
                    program_name: &state.program_name,
                    literal_index: &state.literal_index,
                    run_context: &state.run_context,
                    observes_underscore: state.observes_underscore,
                    inputs: &state.inputs,
                    sources: &state.sources,
                    cargo_outputs: &state.cargo_outputs,
                    runtime_environment: &state.runtime_environment,
                    duplicate_ready: false,
                },
                &temporary_artifact,
                &cached_artifact,
            )?;
            touch_history_entry(&temporary)?;
            match fs::rename(&temporary, &entry) {
                Ok(()) => Ok(()),
                Err(_error) if entry.is_dir() => {
                    let _ = fs::remove_dir_all(&temporary);
                    touch_history_entry(&entry)
                }
                Err(error) => Err(format!(
                    "could not publish Cinder revision history: {error}"
                )),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result?;
        prune_history(&history)?;
        prune_global_history()
    }

    fn history_key(&self) -> Result<String, String> {
        let artifact_digest = self
            .artifact_digest
            .ok_or_else(|| "Cinder state has no artifact digest".to_owned())?;
        let mut hasher = Sha256::new();
        hasher.update(b"CINDER-BUILD-HISTORY-3");
        append_context_value(&mut hasher, &self.run_context);
        append_context_value(&mut hasher, self.program_name.as_bytes());
        append_context_value(&mut hasher, self.public_artifact.as_os_str().as_bytes());
        for output in [
            &self.cargo_outputs.dependency_file,
            &self.cargo_outputs.artifact,
            &self.cargo_outputs.fingerprint,
        ] {
            append_context_value(&mut hasher, output.as_os_str().as_bytes());
        }
        hasher.update(artifact_digest);
        hasher.update(self.source_digest);
        for source in &self.sources {
            append_context_value(&mut hasher, source.as_os_str().as_bytes());
        }
        for input in &self.inputs {
            append_context_value(&mut hasher, input.path.as_os_str().as_bytes());
            hasher.update(input.identity.size.to_le_bytes());
            hasher.update(input.digest);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    fn historical_target_lock_path(
        directory: &Path,
        kind: StateKind,
        context: &[u8],
    ) -> Result<Option<PathBuf>, String> {
        let history = history_directory(directory, kind);
        let entries = match fs::read_dir(&history) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "could not inspect Cinder revision history: {error}"
                ));
            }
        };
        let mut entries: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && !path
                        .file_name()
                        .is_some_and(|name| name.as_bytes().starts_with(b".tmp-"))
            })
            .collect();
        entries.sort_by_key(|path| std::cmp::Reverse(history_recency(path)));
        for entry in entries {
            let recorded_context = match fs::read(entry.join("run-context")) {
                Ok(context) => context,
                Err(_) => continue,
            };
            let observes_underscore = match fs::read(entry.join("observes-underscore")) {
                Ok(value) if value == b"0" => false,
                Ok(value) if value == b"1" => true,
                Ok(_) | Err(_) => continue,
            };
            if recorded_context != bind_observed_shell_environment(context, observes_underscore) {
                continue;
            }
            let public_artifact = match fs::read(entry.join("public-artifact")) {
                Ok(path) => PathBuf::from(OsString::from_vec(path)),
                Err(_) => continue,
            };
            if let Ok(path) = cargo_target_lock_path(&public_artifact) {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    fn matching_history(
        directory: &Path,
        kind: StateKind,
        context: &[u8],
        target_lock_path: Option<&Path>,
    ) -> Result<Option<Self>, String> {
        let history = history_directory(directory, kind);
        let entries = match fs::read_dir(&history) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "could not inspect Cinder revision history: {error}"
                ));
            }
        };
        let mut entries: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && !path
                        .file_name()
                        .is_some_and(|name| name.as_bytes().starts_with(b".tmp-"))
            })
            .collect();
        entries.sort_by_key(|path| std::cmp::Reverse(history_recency(path)));
        let mut probes = HistoryProbeCache::default();

        for entry in entries {
            let state = match Self::load_from(&entry) {
                Ok(Some(state)) => state,
                Ok(None) | Err(_) => continue,
            };
            if !state.context_matches(context)
                || target_lock_path.is_some_and(|expected| {
                    cargo_target_lock_path(&state.public_artifact)
                        .ok()
                        .as_deref()
                        != Some(expected)
                })
                || !state.artifact_is_unchanged().unwrap_or(false)
                || !state.cargo_outputs_are_available()
            {
                continue;
            }
            let sources_match =
                if probes.source_digest(directory, &state.sources)? != state.source_digest {
                    false
                } else if probes.source_probe_is_current(directory, &state.sources) {
                    true
                } else {
                    probes.refresh_source_digest(directory, &state.sources)? == state.source_digest
                };
            if !sources_match
                || !state.inputs_match_revision(directory)?
                || entry.file_name().and_then(OsStr::to_str) != state.history_key().ok().as_deref()
                || !state.source_snapshot_matches_revision()?
                || !state.cached_artifact_is_trusted()?
            {
                continue;
            }
            touch_history_entry(&entry)?;
            if env::var_os("CINDER_TRACE_RUN").is_some() {
                eprintln!(
                    "    Cinder trace: history source-probes={}",
                    probes.source_probes
                );
            }
            return Ok(Some(state));
        }
        if env::var_os("CINDER_TRACE_RUN").is_some() {
            eprintln!(
                "    Cinder trace: history source-probes={}",
                probes.source_probes
            );
        }
        Ok(None)
    }

    fn promote(
        &self,
        directory: &Path,
        kind: StateKind,
        artifact: &Path,
        duplicate_ready: bool,
    ) -> Result<(), String> {
        let artifact_file_identity = artifact_file_identity(artifact)?;
        Self::publish(
            directory,
            kind,
            StatePublication {
                source_root: &self.snapshot,
                source_digest: &self.source_digest,
                artifact,
                artifact_file_identity: &artifact_file_identity,
                artifact_digest: self.artifact_digest.as_ref(),
                public_artifact: &self.public_artifact,
                program_name: &self.program_name,
                literal_index: &self.literal_index,
                run_context: &self.run_context,
                observes_underscore: self.observes_underscore,
                inputs: &self.inputs,
                sources: &self.sources,
                cargo_outputs: &self.cargo_outputs,
                runtime_environment: &self.runtime_environment,
                duplicate_ready,
            },
        )
    }

    fn artifact_is_unchanged(&self) -> Result<bool, String> {
        let identity = match artifact_file_identity(&self.artifact) {
            Ok(identity) => identity,
            Err(_) if !self.artifact.exists() => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(identity == self.artifact_file_identity)
    }

    fn context_matches(&self, context: &[u8]) -> bool {
        self.run_context == bind_observed_shell_environment(context, self.observes_underscore)
    }

    fn sources_match_revision(&self, directory: &Path) -> Result<bool, String> {
        Ok(source_revision_digest(directory, &self.sources)? == self.source_digest)
    }

    fn source_snapshot_matches_revision(&self) -> Result<bool, String> {
        Ok(source_revision_digest(&self.snapshot, &self.sources)? == self.source_digest)
    }

    fn cached_artifact_is_trusted(&self) -> Result<bool, String> {
        Ok(self.artifact_digest.is_some()
            && self.artifact_is_unchanged()?
            && fs::metadata(&self.artifact)
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o222 == 0))
    }

    fn cargo_outputs_are_available(&self) -> bool {
        self.public_artifact.is_file() && self.cargo_outputs.are_available()
    }

    fn literal_offset(&self, bytes: &[u8]) -> Option<u64> {
        self.literal_index
            .iter()
            .find(|entry| entry.bytes == bytes)
            .map(|entry| entry.offset)
    }

    fn inputs_are_unchanged(&self, directory: &Path) -> Result<bool, String> {
        if !input_entries_are_unchanged(&self.inputs) {
            return Ok(false);
        }
        self.input_topology_is_unchanged(directory)
    }

    fn inputs_match_revision(&self, directory: &Path) -> Result<bool, String> {
        if !input_entries_match_revision(&self.inputs)? {
            return Ok(false);
        }
        self.input_topology_is_unchanged(directory)
    }

    fn input_topology_is_unchanged(&self, directory: &Path) -> Result<bool, String> {
        let mut current = BTreeSet::new();
        add_project_rust_inputs(directory, &self.public_artifact, &mut current)?;
        add_cargo_control_inputs(directory, &mut current);
        let recorded: BTreeSet<_> = self
            .sources
            .iter()
            .map(|source| directory.join(source))
            .chain(self.inputs.iter().map(|input| input.path.clone()))
            .collect();
        Ok(current.is_subset(&recorded))
    }

    fn indexed_patch<'a>(&self, change: &'a LiteralChange) -> Option<(&'a [u8], &'a [u8], u64)> {
        self.literal_offset(&change.old)
            .map(|offset| (change.old.as_slice(), change.new.as_slice(), offset))
    }

    fn consume_fresh_duplicate(directory: &Path) -> Result<bool, String> {
        let path = state_directory(directory, StateKind::Run).join("duplicate-ready");
        let fresh = Self::fresh_duplicate_is_pending(directory)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(fresh),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("could not consume duplicate-run state: {error}")),
        }
    }

    fn fresh_duplicate_is_pending(directory: &Path) -> Result<bool, String> {
        let path = state_directory(directory, StateKind::Run).join("duplicate-ready");
        let ready_ns: u128 = match fs::read_to_string(&path) {
            Ok(value) => value
                .parse()
                .map_err(|_| "Cinder duplicate-run state is invalid".to_owned())?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("could not read duplicate-run state: {error}")),
        };
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock predates the Unix epoch".to_owned())?
            .as_nanos();
        Ok(now_ns.saturating_sub(ready_ns) <= DUPLICATE_EVENT_MAX_AGE.as_nanos())
    }
}

fn write_state_directory(
    destination: &Path,
    publication: StatePublication<'_>,
    artifact_metadata_path: &Path,
    artifact_record_path: &Path,
) -> Result<(), String> {
    let StatePublication {
        source_root,
        source_digest,
        artifact_file_identity: expected_artifact_identity,
        artifact_digest,
        public_artifact,
        program_name,
        literal_index,
        run_context,
        observes_underscore,
        inputs,
        sources,
        cargo_outputs,
        runtime_environment,
        duplicate_ready,
        ..
    } = publication;
    fs::create_dir_all(destination.join("snapshot"))
        .map_err(|error| format!("could not create Cinder state: {error}"))?;
    make_private_directory(destination)?;
    snapshot_sources(source_root, &destination.join("snapshot"), sources)?;
    let identity = artifact_file_identity(artifact_metadata_path)?;
    if &identity != expected_artifact_identity {
        return Err(format!(
            "artifact changed while Cinder published state: {}",
            artifact_metadata_path.display()
        ));
    }
    fs::write(
        destination.join("artifact"),
        artifact_record_path.as_os_str().as_bytes(),
    )
    .map_err(|error| format!("could not record Cargo artifact: {error}"))?;
    fs::write(
        destination.join("public-artifact"),
        public_artifact.as_os_str().as_bytes(),
    )
    .map_err(|error| format!("could not record public Cargo artifact: {error}"))?;
    fs::write(destination.join("program-name"), program_name.as_bytes())
        .map_err(|error| format!("could not record Cargo program name: {error}"))?;
    fs::write(
        destination.join("artifact-metadata"),
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            identity.size,
            identity.modified_ns,
            identity.device,
            identity.inode,
            identity.changed_seconds,
            identity.changed_nanoseconds,
        ),
    )
    .map_err(|error| format!("could not record Cargo artifact metadata: {error}"))?;
    if let Some(digest) = artifact_digest {
        fs::write(destination.join("artifact-digest"), digest)
            .map_err(|error| format!("could not record Cargo artifact digest: {error}"))?;
    }
    fs::write(destination.join("source-digest"), source_digest)
        .map_err(|error| format!("could not record Cinder source digest: {error}"))?;
    write_literal_index(&destination.join("literal-index"), literal_index)?;
    fs::write(destination.join("run-context"), run_context)
        .map_err(|error| format!("could not record Cinder run context: {error}"))?;
    fs::write(
        destination.join("observes-underscore"),
        if observes_underscore { b"1" } else { b"0" },
    )
    .map_err(|error| format!("could not record observed compiler environment: {error}"))?;
    write_inputs(&destination.join("inputs"), inputs)?;
    write_source_paths(&destination.join("sources"), sources)?;
    write_cargo_outputs(&destination.join("cargo-outputs"), cargo_outputs)?;
    write_runtime_environment(
        &destination.join("runtime-environment"),
        runtime_environment,
    )?;
    if duplicate_ready {
        let ready_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock predates the Unix epoch".to_owned())?
            .as_nanos();
        fs::write(destination.join("duplicate-ready"), ready_ns.to_string())
            .map_err(|error| format!("could not record duplicate-run state: {error}"))?;
    }
    Ok(())
}

fn history_directory(directory: &Path, kind: StateKind) -> PathBuf {
    state_directory(directory, kind).with_file_name(kind.history_directory_name())
}

fn touch_history_entry(entry: &Path) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    fs::write(entry.join("last-used"), now.to_string())
        .map_err(|error| format!("could not update Cinder revision history: {error}"))
}

fn history_recency(entry: &Path) -> SystemTime {
    fs::metadata(entry.join("last-used"))
        .and_then(|metadata| metadata.modified())
        .or_else(|_| fs::metadata(entry).and_then(|metadata| metadata.modified()))
        .unwrap_or(UNIX_EPOCH)
}

fn staging_process_id(path: &Path, prefix: &[u8]) -> Option<u32> {
    let name = path.file_name()?.as_bytes();
    let suffix = name.strip_prefix(prefix)?;
    let process = suffix.split(|byte| *byte == b'-').next()?;
    std::str::from_utf8(process).ok()?.parse().ok()
}

fn process_is_running(process: u32) -> bool {
    process == std::process::id()
        || Command::new("/bin/kill")
            .args([OsStr::new("-0"), OsStr::new(&process.to_string())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

fn staging_path_is_stale(path: &Path, prefix: &[u8], cutoff: SystemTime) -> bool {
    let Some(process) = staging_process_id(path, prefix) else {
        return false;
    };
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified < cutoff)
        && !process_is_running(process)
}

fn prune_stale_history_staging(history: &Path, cutoff: SystemTime) -> Result<(), String> {
    let entries = match fs::read_dir(history) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not inspect Cinder revision staging {}: {error}",
                history.display()
            ));
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() && staging_path_is_stale(&path, b".tmp-", cutoff) {
            remove_directory_if_present(&path)?;
        }
    }
    Ok(())
}

fn prune_stale_artifact_staging(root: &Path, cutoff: SystemTime) -> Result<(), String> {
    prune_stale_named_staging(root, &[b".cinder-restore-", b".cinder-patch-"], cutoff)
}

fn prune_stale_project_staging(project: &Path, cutoff: SystemTime) -> Result<(), String> {
    prune_stale_named_staging(
        project,
        &[
            b"run.capture-",
            b"build.capture-",
            b"run.patch-",
            b"build.patch-",
            b"run.tmp-",
            b"build.tmp-",
            b"run-context-",
        ],
        cutoff,
    )
}

fn prune_stale_named_staging(
    root: &Path,
    prefixes: &[&[u8]],
    cutoff: SystemTime,
) -> Result<(), String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not inspect Cinder artifact staging {}: {error}",
                root.display()
            ));
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let stale = prefixes
            .iter()
            .any(|prefix| staging_path_is_stale(&path, prefix, cutoff));
        if !stale {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "could not inspect Cinder artifact staging {}: {error}",
                    path.display()
                ));
            }
        };
        let result = if metadata.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::NotFound {
                return Err(format!(
                    "could not prune Cinder artifact staging {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn prune_stale_global_staging(state_root: &Path, cutoff: SystemTime) -> Result<(), String> {
    let Some(root) = state_root.parent() else {
        return Ok(());
    };
    prune_stale_named_staging(&root.join("receipts"), &[b""], cutoff)?;
    prune_stale_named_staging(&root.join("recordings"), &[b"run-"], cutoff)
}

fn prune_history(history: &Path) -> Result<(), String> {
    let cutoff = SystemTime::now()
        .checked_sub(STAGING_MAX_AGE)
        .unwrap_or(UNIX_EPOCH);
    prune_stale_history_staging(history, cutoff)?;
    let mut entries: Vec<_> = fs::read_dir(history)
        .map_err(|error| format!("could not inspect Cinder revision history: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && !path
                    .file_name()
                    .is_some_and(|name| name.as_bytes().starts_with(b".tmp-"))
        })
        .collect();
    if entries.len() <= REVISION_HISTORY_LIMIT {
        return Ok(());
    }
    entries.sort_by_key(|path| history_recency(path));
    let remove_count = entries.len() - REVISION_HISTORY_LIMIT;
    for entry in entries.into_iter().take(remove_count) {
        fs::remove_dir_all(&entry).map_err(|error| {
            format!(
                "could not prune Cinder revision history {}: {error}",
                entry.display()
            )
        })?;
    }
    Ok(())
}

fn artifact_roots_directory(directory: &Path) -> PathBuf {
    state_project_directory(directory).join("run-artifact-roots")
}

fn record_artifact_root(directory: &Path, root: &Path) -> Result<(), String> {
    let registry = artifact_roots_directory(directory);
    fs::create_dir_all(&registry)
        .map_err(|error| format!("could not create run artifact registry: {error}"))?;
    make_private_directory(&registry)?;
    let mut hasher = Sha256::new();
    hasher.update(root.as_os_str().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    fs::write(registry.join(key), root.as_os_str().as_bytes())
        .map_err(|error| format!("could not record run artifact directory: {error}"))
}

fn prune_run_artifacts(directory: &Path) -> Result<(), String> {
    let mut retained = BTreeSet::new();
    let mut artifact_roots = BTreeSet::new();
    if let Ok(Some(state)) = State::load(directory, StateKind::Run) {
        if let Some(parent) = state.artifact.parent() {
            artifact_roots.insert(parent.to_path_buf());
        }
        if let Some(parent) = state.public_artifact.parent() {
            artifact_roots.insert(parent.to_path_buf());
        }
        if is_cinder_run_artifact(directory, &state.artifact) {
            retained.insert(state.artifact.clone());
        }
        if let Ok(path) = restored_run_artifact_path(directory, &state) {
            retained.insert(path);
        }
    }
    let history = history_directory(directory, StateKind::Run);
    if let Ok(entries) = fs::read_dir(history) {
        for entry in entries.filter_map(Result::ok) {
            if let Ok(Some(state)) = State::load_from(&entry.path()) {
                if let Some(parent) = state.public_artifact.parent() {
                    artifact_roots.insert(parent.to_path_buf());
                }
                if let Ok(path) = restored_run_artifact_path(directory, &state) {
                    retained.insert(path);
                }
            }
        }
    }
    prune_run_artifact_receipts(directory, &retained)?;

    let registry = artifact_roots_directory(directory);
    let roots = match fs::read_dir(registry) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("could not inspect run artifact registry: {error}")),
    };
    if let Some(roots) = roots {
        for marker in roots.filter_map(Result::ok) {
            let Ok(value) = fs::read(marker.path()) else {
                continue;
            };
            let root = PathBuf::from(OsString::from_vec(value));
            if root.is_absolute() {
                artifact_roots.insert(root);
            }
        }
    }
    for root in artifact_roots {
        let cutoff = SystemTime::now()
            .checked_sub(STAGING_MAX_AGE)
            .unwrap_or(UNIX_EPOCH);
        prune_stale_artifact_staging(&root, cutoff)?;
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "could not inspect restored run artifacts {}: {error}",
                    root.display()
                ));
            }
        };
        let stale_cutoff = SystemTime::now()
            .checked_sub(STAGING_MAX_AGE)
            .unwrap_or(UNIX_EPOCH);
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let unretained_revision =
                is_revision_run_artifact(directory, &path) && !retained.contains(&path);
            let patch_prefix = format!(".cinder-fast-{}-patch-", project_namespace(directory));
            let stale_patch = is_patch_run_artifact(directory, &path)
                && !retained.contains(&path)
                && staging_path_is_stale(&path, patch_prefix.as_bytes(), stale_cutoff);
            if unretained_revision || stale_patch {
                match fs::remove_file(&path) {
                    Ok(()) => {
                        if unretained_revision {
                            let receipt = restored_run_artifact_receipt_path(directory, &path);
                            let _ = fs::remove_file(receipt);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "could not prune restored run artifact {}: {error}",
                            path.display()
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn prune_run_artifact_receipts(
    directory: &Path,
    retained_artifacts: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    let retained: BTreeSet<_> = retained_artifacts
        .iter()
        .map(|artifact| restored_run_artifact_receipt_path(directory, artifact))
        .collect();
    let receipts = state_project_directory(directory).join("run-artifact-identities");
    let entries = match fs::read_dir(&receipts) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not inspect restored artifact receipts: {error}"
            ));
        }
    };
    for entry in entries.filter_map(Result::ok) {
        if retained.contains(&entry.path()) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not prune restored artifact receipt: {error}"
                ));
            }
        }
    }
    Ok(())
}

fn cinder_run_artifact_suffix<'a>(directory: &Path, path: &'a Path) -> Option<&'a str> {
    let name = path.file_name()?.to_str()?;
    let prefix = format!(".cinder-fast-{}-", project_namespace(directory));
    name.strip_prefix(&prefix)
}

fn is_revision_run_artifact(directory: &Path, path: &Path) -> bool {
    let Some(digest) =
        cinder_run_artifact_suffix(directory, path).and_then(|suffix| suffix.split('-').next())
    else {
        return false;
    };
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_patch_run_artifact(directory: &Path, path: &Path) -> bool {
    cinder_run_artifact_suffix(directory, path)
        .and_then(|suffix| suffix.strip_prefix("patch-"))
        .is_some_and(|suffix| suffix.split('-').count() >= 3)
}

fn is_cinder_run_artifact(directory: &Path, path: &Path) -> bool {
    is_revision_run_artifact(directory, path) || is_patch_run_artifact(directory, path)
}

fn prune_deleted_workspace_artifacts(
    project: &Path,
    workspace: &Path,
    staging_cutoff: SystemTime,
) -> Result<bool, String> {
    let registry = project.join("run-artifact-roots");
    let roots = match fs::read_dir(registry) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(format!("could not inspect artifact registry: {error}")),
    };
    let patch_prefix = format!(".cinder-fast-{}-patch-", project_namespace(workspace));
    let mut pending = false;
    for marker in roots {
        let marker = marker.map_err(|error| format!("could not inspect artifact root: {error}"))?;
        let value = fs::read(marker.path())
            .map_err(|error| format!("could not read artifact root: {error}"))?;
        let root = PathBuf::from(OsString::from_vec(value));
        if !root.is_absolute() {
            pending = true;
            continue;
        }
        prune_stale_artifact_staging(&root, staging_cutoff)?;
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "could not inspect deleted-workspace artifacts {}: {error}",
                    root.display()
                ));
            }
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let remove = is_revision_run_artifact(workspace, &path)
                || (is_patch_run_artifact(workspace, &path)
                    && staging_path_is_stale(&path, patch_prefix.as_bytes(), staging_cutoff));
            if remove {
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "could not prune deleted-workspace artifact {}: {error}",
                            path.display()
                        ));
                    }
                }
            } else if is_patch_run_artifact(workspace, &path)
                || staging_process_id(&path, b".cinder-restore-").is_some()
                || staging_process_id(&path, b".cinder-patch-").is_some()
            {
                pending = true;
            }
        }
    }
    Ok(!pending)
}

fn prune_global_history() -> Result<(), String> {
    let state_root = cinder_state_root();
    let now = SystemTime::now();
    let cutoff = now
        .checked_sub(REVISION_HISTORY_MAX_AGE)
        .unwrap_or(UNIX_EPOCH);
    prune_global_history_at(&state_root, REVISION_HISTORY_MAX_BYTES, cutoff)
}

fn prune_global_history_at(
    state_root: &Path,
    max_bytes: u64,
    cutoff: SystemTime,
) -> Result<(), String> {
    let mut entries = Vec::new();
    let mut workspaces = BTreeSet::new();
    let staging_cutoff = SystemTime::now()
        .checked_sub(STAGING_MAX_AGE)
        .unwrap_or(UNIX_EPOCH);
    prune_stale_global_staging(state_root, staging_cutoff)?;

    let projects = match fs::read_dir(state_root) {
        Ok(projects) => projects,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not inspect Cinder state cache: {error}")),
    };
    for project in projects.filter_map(Result::ok) {
        let project = project.path();
        if !project.is_dir() {
            continue;
        }
        prune_stale_project_staging(&project, staging_cutoff)?;
        let workspace = fs::read(project.join("workspace"))
            .ok()
            .map(|workspace| PathBuf::from(OsString::from_vec(workspace)));
        if let Some(workspace) = workspace.as_ref() {
            if !workspace.is_dir() {
                if prune_deleted_workspace_artifacts(&project, workspace, staging_cutoff)? {
                    remove_directory_if_present(&project)?;
                }
                continue;
            }
            workspaces.insert(workspace.clone());
        }
        for name in ["run-history", "build-history"] {
            let history = project.join(name);
            prune_stale_history_staging(&history, staging_cutoff)?;
            let cached = match fs::read_dir(&history) {
                Ok(cached) => cached,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "could not inspect Cinder revision cache {}: {error}",
                        history.display()
                    ));
                }
            };
            for entry in cached.filter_map(Result::ok) {
                let path = entry.path();
                if !path.is_dir()
                    || path
                        .file_name()
                        .is_some_and(|name| name.as_bytes().starts_with(b".tmp-"))
                {
                    continue;
                }
                let recency = history_recency(&path);
                if recency < cutoff {
                    remove_directory_if_present(&path)?;
                    continue;
                }
                let mut bytes = directory_logical_bytes(&path)?;
                if name == "run-history" {
                    if let Ok(Some(state)) = State::load_from(&path) {
                        if let Some(artifact) = workspace.as_deref().and_then(|workspace| {
                            restored_run_artifact_path(workspace, &state).ok()
                        }) {
                            bytes = bytes.saturating_add(
                                fs::symlink_metadata(artifact)
                                    .map(|metadata| metadata.len())
                                    .unwrap_or(0),
                            );
                        }
                    }
                }
                entries.push((recency, bytes, path));
            }
        }
    }

    let mut total: u64 = entries.iter().map(|(_, bytes, _)| bytes).sum();
    if total > max_bytes {
        entries.sort_by_key(|(recency, _, _)| *recency);
        for (_, bytes, path) in entries {
            remove_directory_if_present(&path)?;
            total = total.saturating_sub(bytes);
            if total <= max_bytes {
                break;
            }
        }
    }
    for workspace in workspaces {
        prune_run_artifacts(&workspace)?;
    }
    Ok(())
}

fn directory_logical_bytes(path: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "could not measure Cinder revision cache {}: {error}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let entries = fs::read_dir(path).map_err(|error| {
        format!(
            "could not measure Cinder revision cache {}: {error}",
            path.display()
        )
    })?;
    let mut bytes = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not measure Cinder revision cache {}: {error}",
                path.display()
            )
        })?;
        bytes = bytes.saturating_add(directory_logical_bytes(&entry.path())?);
    }
    Ok(bytes)
}

fn remove_directory_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not prune Cinder state {}: {error}",
            path.display()
        )),
    }
}

fn parse_state_number<T>(value: Option<&str>, label: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .ok_or_else(|| format!("Cinder state is missing {label}"))?
        .parse()
        .map_err(|_| format!("Cinder state has an invalid {label}"))
}

fn state_directory(directory: &Path, kind: StateKind) -> PathBuf {
    state_project_directory(directory).join(kind.directory_name())
}

fn state_project_directory(directory: &Path) -> PathBuf {
    cinder_state_root().join(project_namespace(directory))
}

fn project_namespace(directory: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    directory.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn cinder_state_root() -> PathBuf {
    env::temp_dir().join("cinder").join("state")
}

fn make_private_directory(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "could not protect Cinder state directory {}: {error}",
            path.display()
        )
    })
}

fn artifact_metadata(path: &Path) -> Result<(u64, u128), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect artifact {}: {error}", path.display()))?;
    let modified_ns = metadata
        .modified()
        .map_err(|error| format!("could not inspect artifact timestamp: {error}"))?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "artifact timestamp predates the Unix epoch".to_owned())?
        .as_nanos();
    Ok((metadata.len(), modified_ns))
}

fn artifact_file_identity(path: &Path) -> Result<ArtifactFileIdentity, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect artifact {}: {error}", path.display()))?;
    artifact_file_identity_from_metadata(&metadata)
}

fn artifact_file_identity_from_metadata(
    metadata: &fs::Metadata,
) -> Result<ArtifactFileIdentity, String> {
    let modified_ns = metadata
        .modified()
        .map_err(|error| format!("could not inspect artifact timestamp: {error}"))?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "artifact timestamp predates the Unix epoch".to_owned())?
        .as_nanos();
    Ok(ArtifactFileIdentity {
        size: metadata.len(),
        modified_ns,
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

fn artifact_identity(path: &Path) -> Result<(ArtifactFileIdentity, [u8; 32]), String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open artifact {}: {error}", path.display()))?;
    let before = artifact_file_identity_from_metadata(
        &file
            .metadata()
            .map_err(|error| format!("could not inspect artifact {}: {error}", path.display()))?,
    )?;
    let digest = sha256_reader(&mut file, path)?;
    let after = artifact_file_identity_from_metadata(
        &file
            .metadata()
            .map_err(|error| format!("could not inspect artifact {}: {error}", path.display()))?,
    )?;
    if before != after || after != artifact_file_identity(path)? {
        return Err(format!(
            "artifact changed while Cinder inspected it: {}",
            path.display()
        ));
    }
    Ok((before, digest))
}

fn sha256_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open artifact {}: {error}", path.display()))?;
    sha256_reader(&mut file, path)
}

fn sha256_reader(file: &mut fs::File, path: &Path) -> Result<[u8; 32], String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("could not hash artifact {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn input_entries_are_unchanged(inputs: &[InputEntry]) -> bool {
    inputs.iter().all(|input| {
        artifact_file_identity(&input.path).is_ok_and(|identity| identity == input.identity)
    })
}

fn input_entries_match_revision(inputs: &[InputEntry]) -> Result<bool, String> {
    for input in inputs {
        if artifact_file_identity(&input.path).is_ok_and(|identity| identity == input.identity) {
            continue;
        }
        if input_identity(&input.path)?.1 != input.digest {
            return Ok(false);
        }
    }
    Ok(true)
}

fn input_identity(path: &Path) -> Result<(ArtifactFileIdentity, [u8; 32]), String> {
    let before = artifact_file_identity(path)?;
    if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return artifact_identity(path);
    }
    let digest = input_digest(path)?;
    let after = artifact_file_identity(path)?;
    if before != after {
        return Err(format!(
            "build input changed while Cinder inspected it: {}",
            path.display()
        ));
    }
    Ok((before, digest))
}

fn input_digest(path: &Path) -> Result<[u8; 32], String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect build input {}: {error}", path.display()))?;
    if metadata.is_file() {
        return sha256_file(path);
    }
    if !metadata.is_dir() {
        return Err(format!(
            "build input is neither a file nor directory: {}",
            path.display()
        ));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)
        .map_err(|error| format!("could not inspect build input {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| {
            format!("could not inspect build input {}: {error}", path.display())
        })?;
        let kind = entry.file_type().map_err(|error| {
            format!(
                "could not inspect build input {}: {error}",
                entry.path().display()
            )
        })?;
        let kind = if kind.is_dir() {
            1
        } else if kind.is_file() {
            2
        } else if kind.is_symlink() {
            3
        } else {
            4
        };
        entries.push((entry.file_name().into_vec(), kind));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    hasher.update(b"CINDER-INPUT-DIRECTORY-1");
    for (name, kind) in entries {
        append_context_value(&mut hasher, &name);
        hasher.update([kind]);
    }
    Ok(hasher.finalize().into())
}

fn project_may_have_build_script(directory: &Path, sources: &[PathBuf]) -> Result<bool, String> {
    let mut manifests = BTreeSet::new();
    for source in sources {
        for ancestor in directory.join(source).ancestors().skip(1) {
            let manifest = ancestor.join("Cargo.toml");
            if manifest.is_file() {
                manifests.insert(manifest);
            }
            if ancestor == directory {
                break;
            }
        }
    }
    for manifest in manifests {
        let package = manifest
            .parent()
            .ok_or_else(|| format!("manifest has no parent: {}", manifest.display()))?;
        if package.join("build.rs").is_file() {
            return Ok(true);
        }
        let contents = fs::read_to_string(&manifest)
            .map_err(|error| format!("could not inspect {}: {error}", manifest.display()))?;
        let mut in_package = false;
        for line in contents.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.starts_with('[') {
                in_package = line == "[package]";
            } else if (in_package && toml_key_is(line, "build"))
                || toml_key_is(line, "package.build")
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn toml_key_is(line: &str, expected: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(key, _)| key.trim().trim_matches(['\'', '"']) == expected)
}

const INPUTS_MAGIC: &[u8; 8] = b"CNDI0003";

fn build_inputs(
    directory: &Path,
    artifact: &Path,
    dependency_file: &Path,
    sources: &[PathBuf],
    receipt: Option<&ArtifactReceipt>,
) -> Result<Vec<InputEntry>, String> {
    let mut paths = dependency_paths(directory, dependency_file)?;
    if let Some(target) = cargo_target_directory(artifact) {
        paths.retain(|path| !path.starts_with(target));
    }
    add_cargo_control_inputs(directory, &mut paths);
    if !paths
        .iter()
        .any(|path| path.file_name() == Some(OsStr::new("Cargo.lock")))
    {
        return Err("Cargo.lock is required for reproducible accelerated state".to_owned());
    }

    for source in sources {
        let source = directory.join(source);
        for ancestor in source.ancestors().skip(1) {
            let manifest = ancestor.join("Cargo.toml");
            if manifest.is_file() {
                paths.insert(manifest);
            }
            if ancestor == directory {
                break;
            }
        }
    }

    if let Some(receipt) = receipt {
        add_project_rust_inputs(directory, artifact, &mut paths)?;
        add_build_script_inputs(receipt, &mut paths)?;
    }

    let source_paths: BTreeSet<_> = sources
        .iter()
        .map(|relative| directory.join(relative))
        .collect();
    paths.retain(|path| !source_paths.contains(path));
    paths
        .into_iter()
        .map(|path| {
            let (identity, digest) = input_identity(&path)?;
            Ok(InputEntry {
                path,
                identity,
                digest,
            })
        })
        .collect()
}

fn add_cargo_control_inputs(directory: &Path, paths: &mut BTreeSet<PathBuf>) {
    let mut ancestor = Some(directory);
    while let Some(path) = ancestor {
        for relative in [
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/config",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
        ] {
            let input = path.join(relative);
            if input.is_file() {
                paths.insert(input);
            }
        }
        if path.join(".git").exists() {
            break;
        }
        ancestor = path.parent();
    }
    let cargo_home = env::var_os("CARGO_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    if let Some(cargo_home) = cargo_home {
        for name in ["config", "config.toml"] {
            let input = cargo_home.join(name);
            if input.is_file() {
                paths.insert(input);
            }
        }
    }
}

fn add_project_rust_inputs(
    directory: &Path,
    artifact: &Path,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    let target = cargo_target_directory(artifact);
    collect_project_rust_inputs(directory, target, paths)
}

fn collect_project_rust_inputs(
    directory: &Path,
    target: Option<&Path>,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|error| {
        format!(
            "could not inspect project inputs {}: {error}",
            directory.display()
        )
    })? {
        let entry = entry.map_err(|error| format!("could not inspect project input: {error}"))?;
        let path = entry.path();
        if target.is_some_and(|target| path == target)
            || path.file_name().is_some_and(|name| {
                matches!(
                    name.to_str(),
                    Some(".git" | "node_modules" | ".pnpm" | ".yarn")
                )
            })
        {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "could not inspect project input {}: {error}",
                path.display()
            )
        })?;
        if file_type.is_dir() {
            if path.join(".rustc_info.json").is_file() || path.join("CACHEDIR.TAG").is_file() {
                continue;
            }
            collect_project_rust_inputs(&path, target, paths)?;
        } else if path.extension() == Some("rs".as_ref())
            || matches!(
                path.file_name().and_then(OsStr::to_str),
                Some("Cargo.toml" | "Cargo.lock")
            )
        {
            paths.insert(path);
        }
    }
    Ok(())
}

fn add_build_script_inputs(
    receipt: &ArtifactReceipt,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    let Some(out_directory) = receipt.out_directory.as_deref() else {
        return Ok(());
    };
    let manifest_directory = receipt
        .manifest_directory
        .as_deref()
        .ok_or_else(|| "build script state has no manifest directory".to_owned())?;
    let output = out_directory
        .parent()
        .ok_or_else(|| format!("build output has no parent: {}", out_directory.display()))?
        .join("output");
    let output = fs::read_to_string(&output).map_err(|error| {
        format!(
            "could not read build script output {}: {error}",
            output.display()
        )
    })?;
    let watched: Vec<_> = output
        .lines()
        .filter_map(|line| {
            line.strip_prefix("cargo:rerun-if-changed=")
                .or_else(|| line.strip_prefix("cargo::rerun-if-changed="))
        })
        .filter(|path| !path.is_empty())
        .collect();
    if watched.is_empty() {
        return Err(
            "build script does not enumerate rerun-if-changed inputs; using Cargo for safety"
                .to_owned(),
        );
    }
    let target = cargo_target_directory(&receipt.artifact);
    for watched in watched {
        let watched = Path::new(watched);
        let watched = if watched.is_absolute() {
            watched.to_owned()
        } else {
            manifest_directory.join(watched)
        };
        if target.is_some_and(|target| watched.starts_with(target)) {
            return Err(format!(
                "build script watches Cargo's target directory: {}",
                watched.display()
            ));
        }
        match fs::symlink_metadata(&watched) {
            Ok(metadata) if metadata.is_dir() => {
                collect_watched_tree(&watched, paths, 100_000)?;
            }
            Ok(_) => {
                paths.insert(fs::canonicalize(&watched).map_err(|error| {
                    format!(
                        "could not resolve build input {}: {error}",
                        watched.display()
                    )
                })?);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = watched
                    .ancestors()
                    .skip(1)
                    .find(|ancestor| ancestor.is_dir())
                    .ok_or_else(|| {
                        format!("build input has no existing parent: {}", watched.display())
                    })?;
                paths.insert(fs::canonicalize(parent).map_err(|error| {
                    format!(
                        "could not resolve build input parent {}: {error}",
                        parent.display()
                    )
                })?);
            }
            Err(error) => {
                return Err(format!(
                    "could not inspect build input {}: {error}",
                    watched.display()
                ));
            }
        }
    }
    Ok(())
}

fn collect_watched_tree(
    directory: &Path,
    paths: &mut BTreeSet<PathBuf>,
    remaining: usize,
) -> Result<usize, String> {
    if remaining == 0 {
        return Err("build script watches more than 100,000 filesystem entries".to_owned());
    }
    let directory = fs::canonicalize(directory).map_err(|error| {
        format!(
            "could not resolve build input {}: {error}",
            directory.display()
        )
    })?;
    paths.insert(directory.clone());
    let mut remaining = remaining - 1;
    for entry in fs::read_dir(&directory).map_err(|error| {
        format!(
            "could not inspect build input {}: {error}",
            directory.display()
        )
    })? {
        if remaining == 0 {
            return Err("build script watches more than 100,000 filesystem entries".to_owned());
        }
        let entry = entry.map_err(|error| format!("could not inspect build input: {error}"))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!("could not inspect build input {}: {error}", path.display())
        })?;
        if file_type.is_dir() {
            remaining = collect_watched_tree(&path, paths, remaining)?;
        } else {
            paths.insert(path);
            remaining -= 1;
        }
    }
    Ok(remaining)
}

fn build_source_paths(
    directory: &Path,
    artifact: &Path,
    dependency_file: &Path,
) -> Result<Vec<PathBuf>, String> {
    let dependencies = dependency_paths(directory, dependency_file)?;
    let target = cargo_target_directory(artifact);
    let sources: Vec<_> = dependencies
        .into_iter()
        .filter(|path| {
            path.extension() == Some("rs".as_ref())
                && path.file_name() != Some("build.rs".as_ref())
                && path.starts_with(directory)
                && target.is_none_or(|target| !path.starts_with(target))
        })
        .filter_map(|path| path.strip_prefix(directory).ok().map(Path::to_owned))
        .collect();
    if sources.is_empty() {
        return Err(format!(
            "Cargo dependency file {} contains no project Rust sources",
            dependency_file.display()
        ));
    }
    Ok(sources)
}

fn primary_dependency_file(artifact: &Path) -> Result<PathBuf, String> {
    let parent = artifact
        .parent()
        .ok_or_else(|| format!("artifact has no parent: {}", artifact.display()))?;
    let artifact_name = artifact
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format!("artifact name is not valid UTF-8: {}", artifact.display()))?;
    let prefix = format!("{}-", artifact_name.replace('-', "_"));
    let expected_metadata = artifact_metadata(artifact)?;
    let mut candidates = Vec::new();
    for dependency_directory in [parent.join("deps"), parent.to_owned()] {
        if !dependency_directory.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dependency_directory).map_err(|error| {
            format!(
                "could not inspect Cargo dependency directory {}: {error}",
                dependency_directory.display()
            )
        })? {
            let entry =
                entry.map_err(|error| format!("could not inspect Cargo dependency: {error}"))?;
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if path.extension() != Some("d".as_ref()) || !file_name.starts_with(&prefix) {
                continue;
            }
            let executable = path.with_extension("");
            if executable.is_file()
                && artifact_metadata(&executable).ok() == Some(expected_metadata)
            {
                candidates.push(path);
            }
        }
    }
    match candidates.as_slice() {
        [dependency_file] => Ok(dependency_file.clone()),
        [] => Err(format!(
            "could not identify crate dependency data for {}",
            artifact.display()
        )),
        _ => Err(format!(
            "Cargo dependency data is ambiguous for {}",
            artifact.display()
        )),
    }
}

fn cargo_outputs_for_artifact(
    artifact: &Path,
    receipt: Option<&ArtifactReceipt>,
) -> Result<CargoOutputs, String> {
    let (dependency_file, hashed_artifact) = if let Some(receipt) = receipt {
        let hashed_artifact = absolute_path(&receipt.artifact)?;
        (absolute_path(&receipt.dependency_file)?, hashed_artifact)
    } else {
        let dependency_file = primary_dependency_file(artifact)?;
        let hashed_artifact = dependency_file.with_extension("");
        (dependency_file, hashed_artifact)
    };
    let Some(parent) = artifact.parent() else {
        return Err(format!("artifact has no parent: {}", artifact.display()));
    };
    let dependency_name = dependency_file
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| {
            format!(
                "Cargo dependency name is not valid UTF-8: {}",
                dependency_file.display()
            )
        })?;
    let hash = dependency_name
        .strip_suffix(".d")
        .and_then(|name| name.rsplit_once('-').map(|(_, hash)| hash))
        .filter(|hash| !hash.is_empty() && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| {
            format!(
                "Cargo dependency name has no unit hash: {}",
                dependency_file.display()
            )
        })?;
    let fingerprint = cargo_fingerprint_directory(parent, hash)?;
    let outputs = CargoOutputs {
        dependency_file,
        artifact: hashed_artifact,
        fingerprint,
    };
    outputs
        .are_available()
        .then_some(outputs)
        .ok_or_else(|| "Cargo's exact hashed outputs are incomplete".to_owned())
}

fn cargo_fingerprint_directory(profile: &Path, hash: &str) -> Result<PathBuf, String> {
    let fingerprints = profile.join(".fingerprint");
    let entries = fs::read_dir(&fingerprints).map_err(|error| {
        format!(
            "could not inspect Cargo fingerprints {}: {error}",
            fingerprints.display()
        )
    })?;
    let suffix = format!("-{hash}");
    let mut matches = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("could not inspect Cargo fingerprint: {error}"))?
            .path();
        if path.is_dir()
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.ends_with(&suffix))
        {
            matches.push(path);
        }
    }
    match matches.as_slice() {
        [fingerprint] => Ok(fingerprint.clone()),
        [] => Err(format!("Cargo fingerprint is missing for hash {hash}")),
        _ => Err(format!("Cargo fingerprint is ambiguous for hash {hash}")),
    }
}

impl CargoOutputs {
    fn are_available(&self) -> bool {
        self.dependency_file.is_file() && self.artifact.is_file() && self.fingerprint.is_dir()
    }
}

fn cargo_target_directory(artifact: &Path) -> Option<&Path> {
    artifact.ancestors().find(|ancestor| {
        ancestor.join(".rustc_info.json").is_file() || ancestor.join("CACHEDIR.TAG").is_file()
    })
}

fn dependency_paths(directory: &Path, dependency_file: &Path) -> Result<BTreeSet<PathBuf>, String> {
    let dependency_bytes = fs::read(dependency_file).map_err(|error| {
        format!(
            "could not read Cargo dependency file {}: {error}",
            dependency_file.display()
        )
    })?;
    let colon = dependency_bytes
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(|| "Cargo dependency file has no target separator".to_owned())?;
    let prerequisites = &dependency_bytes[colon + 1..];
    let mut end = prerequisites.len();
    let mut cursor = 0;
    while cursor < prerequisites.len() {
        if prerequisites[cursor] == b'\\' && prerequisites.get(cursor + 1) == Some(&b'\n') {
            cursor += 2;
        } else if prerequisites[cursor] == b'\n' {
            end = cursor;
            break;
        } else {
            cursor += 1;
        }
    }
    makefile_words(&prerequisites[..end])
        .into_iter()
        .filter(|word| !word.is_empty())
        .map(|word| {
            let path = PathBuf::from(OsString::from_vec(word));
            resolve_dependency_path(directory, &path)
        })
        .collect()
}

fn resolve_dependency_path(directory: &Path, path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return fs::canonicalize(path)
            .map_err(|error| format!("could not resolve dependency {}: {error}", path.display()));
    }
    for ancestor in directory.ancestors() {
        let candidate = ancestor.join(path);
        if candidate.exists() {
            return fs::canonicalize(&candidate).map_err(|error| {
                format!(
                    "could not resolve dependency {}: {error}",
                    candidate.display()
                )
            });
        }
    }
    Err(format!(
        "could not resolve relative Cargo dependency {}",
        path.display()
    ))
}

fn makefile_words(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' if bytes.get(cursor + 1) == Some(&b'\n') => cursor += 2,
            b'\\' if cursor + 1 < bytes.len() => {
                word.push(bytes[cursor + 1]);
                cursor += 2;
            }
            byte if byte.is_ascii_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                cursor += 1;
            }
            byte => {
                word.push(byte);
                cursor += 1;
            }
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn write_inputs(path: &Path, inputs: &[InputEntry]) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create Cinder input state: {error}"))?;
    file.write_all(INPUTS_MAGIC)
        .and_then(|()| file.write_all(&(inputs.len() as u64).to_le_bytes()))
        .map_err(|error| format!("could not write Cinder input state: {error}"))?;
    for input in inputs {
        let path = input.path.as_os_str().as_bytes();
        let length =
            u32::try_from(path.len()).map_err(|_| "Cinder input path is too long".to_owned())?;
        file.write_all(&length.to_le_bytes())
            .and_then(|()| file.write_all(path))
            .and_then(|()| file.write_all(&input.identity.size.to_le_bytes()))
            .and_then(|()| file.write_all(&input.identity.modified_ns.to_le_bytes()))
            .and_then(|()| file.write_all(&input.identity.device.to_le_bytes()))
            .and_then(|()| file.write_all(&input.identity.inode.to_le_bytes()))
            .and_then(|()| file.write_all(&input.identity.changed_seconds.to_le_bytes()))
            .and_then(|()| file.write_all(&input.identity.changed_nanoseconds.to_le_bytes()))
            .and_then(|()| file.write_all(&input.digest))
            .map_err(|error| format!("could not write Cinder input state: {error}"))?;
    }
    Ok(())
}

fn read_inputs(path: &Path) -> Result<Vec<InputEntry>, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open Cinder input state: {error}"))?;
    let mut magic = [0u8; 8];
    let mut count = [0u8; 8];
    file.read_exact(&mut magic)
        .and_then(|()| file.read_exact(&mut count))
        .map_err(|error| format!("could not read Cinder input state: {error}"))?;
    if &magic != INPUTS_MAGIC {
        return Err("Cinder input state has an unsupported format".to_owned());
    }
    let count = usize::try_from(u64::from_le_bytes(count))
        .map_err(|_| "Cinder input state is too large".to_owned())?;
    let mut inputs = Vec::with_capacity(count.min(10_000));
    for _ in 0..count {
        let mut length = [0u8; 4];
        file.read_exact(&mut length)
            .map_err(|error| format!("could not read Cinder input path: {error}"))?;
        let length = u32::from_le_bytes(length) as usize;
        if length > 1_048_576 {
            return Err("Cinder input path is too long".to_owned());
        }
        let mut path = vec![0; length];
        let mut size = [0u8; 8];
        let mut modified_ns = [0u8; 16];
        let mut device = [0u8; 8];
        let mut inode = [0u8; 8];
        let mut changed_seconds = [0u8; 8];
        let mut changed_nanoseconds = [0u8; 8];
        let mut digest = [0u8; 32];
        file.read_exact(&mut path)
            .and_then(|()| file.read_exact(&mut size))
            .and_then(|()| file.read_exact(&mut modified_ns))
            .and_then(|()| file.read_exact(&mut device))
            .and_then(|()| file.read_exact(&mut inode))
            .and_then(|()| file.read_exact(&mut changed_seconds))
            .and_then(|()| file.read_exact(&mut changed_nanoseconds))
            .and_then(|()| file.read_exact(&mut digest))
            .map_err(|error| format!("could not read Cinder input state: {error}"))?;
        inputs.push(InputEntry {
            path: PathBuf::from(OsString::from_vec(path)),
            identity: ArtifactFileIdentity {
                size: u64::from_le_bytes(size),
                modified_ns: u128::from_le_bytes(modified_ns),
                device: u64::from_le_bytes(device),
                inode: u64::from_le_bytes(inode),
                changed_seconds: i64::from_le_bytes(changed_seconds),
                changed_nanoseconds: i64::from_le_bytes(changed_nanoseconds),
            },
            digest,
        });
    }
    Ok(inputs)
}

const SOURCES_MAGIC: &[u8; 8] = b"CNDS0001";

fn write_source_paths(path: &Path, sources: &[PathBuf]) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create Cinder source state: {error}"))?;
    file.write_all(SOURCES_MAGIC)
        .and_then(|()| file.write_all(&(sources.len() as u64).to_le_bytes()))
        .map_err(|error| format!("could not write Cinder source state: {error}"))?;
    for source in sources {
        let source = source.as_os_str().as_bytes();
        let length =
            u32::try_from(source.len()).map_err(|_| "Cinder source path is too long".to_owned())?;
        file.write_all(&length.to_le_bytes())
            .and_then(|()| file.write_all(source))
            .map_err(|error| format!("could not write Cinder source state: {error}"))?;
    }
    Ok(())
}

fn read_source_paths(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("could not open source state: {error}"))?;
    let mut magic = [0u8; 8];
    let mut count = [0u8; 8];
    file.read_exact(&mut magic)
        .and_then(|()| file.read_exact(&mut count))
        .map_err(|error| format!("could not read Cinder source state: {error}"))?;
    if &magic != SOURCES_MAGIC {
        return Err("Cinder source state has an unsupported format".to_owned());
    }
    let count = usize::try_from(u64::from_le_bytes(count))
        .map_err(|_| "Cinder source state is too large".to_owned())?;
    let mut sources = Vec::with_capacity(count.min(10_000));
    for _ in 0..count {
        let mut length = [0u8; 4];
        file.read_exact(&mut length)
            .map_err(|error| format!("could not read Cinder source path: {error}"))?;
        let length = u32::from_le_bytes(length) as usize;
        if length > 1_048_576 {
            return Err("Cinder source path is too long".to_owned());
        }
        let mut source = vec![0; length];
        file.read_exact(&mut source)
            .map_err(|error| format!("could not read Cinder source path: {error}"))?;
        let source = PathBuf::from(OsString::from_vec(source));
        if source.is_absolute()
            || source
                .components()
                .any(|part| part.as_os_str() == OsStr::new(".."))
        {
            return Err("Cinder source path escapes the project directory".to_owned());
        }
        sources.push(source);
    }
    Ok(sources)
}

const CARGO_OUTPUTS_MAGIC: &[u8; 8] = b"CNDO0001";

fn write_cargo_outputs(path: &Path, outputs: &CargoOutputs) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create Cargo output state: {error}"))?;
    file.write_all(CARGO_OUTPUTS_MAGIC)
        .map_err(|error| format!("could not write Cargo output state: {error}"))?;
    for output in [
        &outputs.dependency_file,
        &outputs.artifact,
        &outputs.fingerprint,
    ] {
        write_state_bytes(
            &mut file,
            output.as_os_str().as_bytes(),
            "Cargo output path",
        )?;
    }
    Ok(())
}

fn read_cargo_outputs(path: &Path) -> Result<CargoOutputs, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open Cargo output state: {error}"))?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic)
        .map_err(|error| format!("could not read Cargo output state: {error}"))?;
    if &magic != CARGO_OUTPUTS_MAGIC {
        return Err("Cargo output state has an unsupported format".to_owned());
    }
    let mut read_path = || {
        let path = PathBuf::from(OsString::from_vec(read_state_bytes(
            &mut file,
            "Cargo output path",
        )?));
        path.is_absolute()
            .then_some(path)
            .ok_or_else(|| "Cargo output state contains a relative path".to_owned())
    };
    Ok(CargoOutputs {
        dependency_file: read_path()?,
        artifact: read_path()?,
        fingerprint: read_path()?,
    })
}

const RUNTIME_ENVIRONMENT_MAGIC: &[u8; 8] = b"CNDE0001";
const RUNTIME_LINKER_ENVIRONMENT_KEYS: [&str; 3] =
    ["DYLD_FALLBACK_LIBRARY_PATH", "LD_LIBRARY_PATH", "LIBPATH"];

fn runtime_linker_environment() -> Vec<(OsString, OsString)> {
    RUNTIME_LINKER_ENVIRONMENT_KEYS
        .iter()
        .filter_map(|key| env::var_os(key).map(|value| (OsString::from(key), value)))
        .collect()
}

fn write_runtime_environment(
    path: &Path,
    environment: &[(OsString, OsString)],
) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create runtime environment state: {error}"))?;
    file.write_all(RUNTIME_ENVIRONMENT_MAGIC)
        .and_then(|()| file.write_all(&(environment.len() as u64).to_le_bytes()))
        .map_err(|error| format!("could not write runtime environment state: {error}"))?;
    for (key, value) in environment {
        write_state_bytes(&mut file, key.as_bytes(), "runtime environment key")?;
        write_state_bytes(&mut file, value.as_bytes(), "runtime environment value")?;
    }
    Ok(())
}

fn write_state_bytes(file: &mut fs::File, value: &[u8], label: &str) -> Result<(), String> {
    let length = u32::try_from(value.len()).map_err(|_| format!("Cinder {label} is too long"))?;
    file.write_all(&length.to_le_bytes())
        .and_then(|()| file.write_all(value))
        .map_err(|error| format!("could not write Cinder {label}: {error}"))
}

fn read_runtime_environment(path: &Path) -> Result<Vec<(OsString, OsString)>, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open runtime environment state: {error}"))?;
    let mut magic = [0; 8];
    let mut count = [0; 8];
    file.read_exact(&mut magic)
        .and_then(|()| file.read_exact(&mut count))
        .map_err(|error| format!("could not read runtime environment state: {error}"))?;
    if &magic != RUNTIME_ENVIRONMENT_MAGIC {
        return Err("runtime environment state has an unsupported format".to_owned());
    }
    let count = usize::try_from(u64::from_le_bytes(count))
        .map_err(|_| "runtime environment state is too large".to_owned())?;
    if count > RUNTIME_LINKER_ENVIRONMENT_KEYS.len() {
        return Err("runtime environment state has too many entries".to_owned());
    }
    let mut environment = Vec::with_capacity(count);
    for _ in 0..count {
        let key = read_state_bytes(&mut file, "runtime environment key")?;
        let key = OsString::from_vec(key);
        if !RUNTIME_LINKER_ENVIRONMENT_KEYS
            .iter()
            .any(|allowed| key == OsStr::new(allowed))
        {
            return Err("runtime environment state contains an unsupported key".to_owned());
        }
        let value = OsString::from_vec(read_state_bytes(&mut file, "runtime environment value")?);
        environment.push((key, value));
    }
    Ok(environment)
}

fn read_state_bytes(file: &mut fs::File, label: &str) -> Result<Vec<u8>, String> {
    let mut length = [0; 4];
    file.read_exact(&mut length)
        .map_err(|error| format!("could not read Cinder {label}: {error}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length > 1_048_576 {
        return Err(format!("Cinder {label} is too long"));
    }
    let mut value = vec![0; length];
    file.read_exact(&mut value)
        .map_err(|error| format!("could not read Cinder {label}: {error}"))?;
    Ok(value)
}

const LITERAL_INDEX_MAGIC: &[u8; 8] = b"CNDX0001";

fn build_literal_index(
    directory: &Path,
    sources: &[PathBuf],
    artifact: &Path,
) -> Result<Vec<LiteralIndexEntry>, String> {
    let mut occurrences = BTreeMap::<Vec<u8>, usize>::new();
    for relative in sources {
        let contents = fs::read(directory.join(relative))
            .map_err(|error| format!("could not index {}: {error}", relative.display()))?;
        for candidate in source_literal_candidates(&contents) {
            *occurrences.entry(candidate).or_default() += 1;
        }
    }
    let patterns: Vec<Vec<u8>> = occurrences
        .into_iter()
        .filter_map(|(bytes, count)| (count == 1).then_some(bytes))
        .collect();
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let matcher = AhoCorasick::new(&patterns)
        .map_err(|error| format!("could not build the literal index: {error}"))?;
    let file = fs::File::open(artifact)
        .map_err(|error| format!("could not index artifact {}: {error}", artifact.display()))?;
    // SAFETY: Cinder owns no writable handle to the executable during this scan.
    // Its metadata is captured before the mapped bytes become visible in state,
    // and later fast-path use rejects artifacts whose metadata has changed.
    let bytes = unsafe { MmapOptions::new().map(&file) }
        .map_err(|error| format!("could not map artifact {}: {error}", artifact.display()))?;
    let mut offsets = vec![None; patterns.len()];
    let mut repeated = vec![false; patterns.len()];
    for found in matcher.find_overlapping_iter(&bytes) {
        let pattern = found.pattern().as_usize();
        if offsets[pattern].is_some() {
            repeated[pattern] = true;
        } else {
            offsets[pattern] = Some(found.start() as u64);
        }
    }
    Ok(patterns
        .into_iter()
        .enumerate()
        .filter_map(|(index, bytes)| {
            if repeated[index] {
                None
            } else {
                offsets[index].map(|offset| LiteralIndexEntry { bytes, offset })
            }
        })
        .collect())
}

fn source_literal_candidates(source: &[u8]) -> Vec<Vec<u8>> {
    source_string_literals(source)
        .into_iter()
        .filter_map(|literal| patchable_literal_data(literal.bytes).map(<[u8]>::to_vec))
        .collect()
}

struct SourceString<'a> {
    bytes: &'a [u8],
    range: Range<usize>,
}

/// Finds unescaped UTF-8 string tokens while ignoring comments, character
/// literals, byte/C strings, and raw strings. Cinder deliberately indexes only
/// literals whose source bytes are identical to their compiled bytes.
fn source_string_literals(source: &[u8]) -> Vec<SourceString<'_>> {
    let mut literals = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        if source[cursor..].starts_with(b"//") {
            cursor = source[cursor..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(source.len(), |offset| cursor + offset + 1);
            continue;
        }
        if source[cursor..].starts_with(b"/*") {
            cursor = block_comment_end(source, cursor);
            continue;
        }
        if let Some(end) = raw_string_end(source, cursor) {
            cursor = end;
            continue;
        }
        if matches!(source[cursor], b'b' | b'c') && source.get(cursor + 1) == Some(&b'"') {
            cursor = cooked_string_end(source, cursor + 1).map_or(source.len(), |(end, _)| end + 1);
            continue;
        }
        if source[cursor] == b'\'' {
            let Some(end) = char_literal_end(source, cursor) else {
                cursor += 1;
                continue;
            };
            cursor = end;
            continue;
        }
        if source[cursor] != b'"' {
            cursor += 1;
            continue;
        }

        let quote = cursor;
        let Some((end, has_escape)) = cooked_string_end(source, quote) else {
            break;
        };
        let range = quote + 1..end;
        let bytes = &source[range.clone()];
        if !has_escape && !bytes.contains(&b'\n') {
            literals.push(SourceString { bytes, range });
        }
        cursor = end + 1;
    }
    literals
}

fn cooked_string_end(source: &[u8], quote: usize) -> Option<(usize, bool)> {
    let mut cursor = quote + 1;
    let mut has_escape = false;
    while cursor < source.len() {
        match source[cursor] {
            b'"' => return Some((cursor, has_escape)),
            b'\\' => {
                has_escape = true;
                cursor += 2;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn block_comment_end(source: &[u8], start: usize) -> usize {
    let mut depth = 1usize;
    let mut cursor = start + 2;
    while cursor < source.len() && depth > 0 {
        if source[cursor..].starts_with(b"/*") {
            depth += 1;
            cursor += 2;
        } else if source[cursor..].starts_with(b"*/") {
            depth -= 1;
            cursor += 2;
        } else {
            cursor += 1;
        }
    }
    cursor
}

fn raw_string_end(source: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start;
    if matches!(source.get(cursor), Some(b'b' | b'c')) {
        cursor += 1;
    }
    if source.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let hashes_start = cursor;
    while source.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if source.get(cursor) != Some(&b'"') {
        return None;
    }
    let hashes = cursor - hashes_start;
    cursor += 1;
    while cursor < source.len() {
        let Some(relative_quote) = source[cursor..].iter().position(|byte| *byte == b'"') else {
            return Some(source.len());
        };
        let quote = cursor + relative_quote;
        let end = quote + 1 + hashes;
        if end <= source.len() && source[quote + 1..end].iter().all(|byte| *byte == b'#') {
            return Some(end);
        }
        cursor = quote + 1;
    }
    Some(source.len())
}

fn char_literal_end(source: &[u8], quote: usize) -> Option<usize> {
    let content = quote + 1;
    let first = *source.get(content)?;
    let closing = if first == b'\\' {
        match source.get(content + 1)? {
            b'x' => content + 4,
            b'u' if source.get(content + 2) == Some(&b'{') => {
                content
                    + 3
                    + source[content + 3..]
                        .iter()
                        .position(|byte| *byte == b'}')?
                    + 1
            }
            _ => content + 2,
        }
    } else {
        content + utf8_char_width(first)?
    };
    (source.get(closing) == Some(&b'\'')).then_some(closing + 1)
}

const fn utf8_char_width(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn patchable_literal_data(literal: &[u8]) -> Option<&[u8]> {
    if literal.len() < 8 || std::str::from_utf8(literal).is_err() {
        return None;
    }
    if literal.contains(&b'{') || literal.contains(&b'}') {
        format_literal_suffix(literal)
    } else {
        Some(literal)
    }
}

fn format_literal_suffix(literal: &[u8]) -> Option<&[u8]> {
    let open = literal.iter().position(|byte| *byte == b'{')?;
    if literal[open + 1..].contains(&b'{') {
        return None;
    }
    let suffix = literal.get(open + 1..)?.strip_prefix(b"}")?;
    (suffix.len() >= 8).then_some(suffix)
}

fn write_literal_index(path: &Path, entries: &[LiteralIndexEntry]) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create literal index: {error}"))?;
    file.write_all(LITERAL_INDEX_MAGIC)
        .and_then(|()| file.write_all(&(entries.len() as u64).to_le_bytes()))
        .map_err(|error| format!("could not write literal index: {error}"))?;
    for entry in entries {
        let length = u32::try_from(entry.bytes.len())
            .map_err(|_| "literal index entry is too long".to_owned())?;
        file.write_all(&length.to_le_bytes())
            .and_then(|()| file.write_all(&entry.offset.to_le_bytes()))
            .and_then(|()| file.write_all(&entry.bytes))
            .map_err(|error| format!("could not write literal index: {error}"))?;
    }
    Ok(())
}

fn read_literal_index(path: &Path) -> Result<Vec<LiteralIndexEntry>, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("could not open literal index: {error}"))?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)
        .map_err(|error| format!("could not read literal index: {error}"))?;
    if &magic != LITERAL_INDEX_MAGIC {
        return Err("Cinder literal index has an unsupported format".to_owned());
    }
    let mut count = [0u8; 8];
    file.read_exact(&mut count)
        .map_err(|error| format!("could not read literal index: {error}"))?;
    let count = usize::try_from(u64::from_le_bytes(count))
        .map_err(|_| "Cinder literal index is too large".to_owned())?;
    let mut entries = Vec::with_capacity(count.min(100_000));
    for _ in 0..count {
        let mut length = [0u8; 4];
        let mut offset = [0u8; 8];
        file.read_exact(&mut length)
            .and_then(|()| file.read_exact(&mut offset))
            .map_err(|error| format!("could not read literal index entry: {error}"))?;
        let length = u32::from_le_bytes(length) as usize;
        if length > 1_048_576 {
            return Err("Cinder literal index entry is too large".to_owned());
        }
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)
            .map_err(|error| format!("could not read literal index entry: {error}"))?;
        entries.push(LiteralIndexEntry {
            bytes,
            offset: u64::from_le_bytes(offset),
        });
    }
    Ok(entries)
}

fn snapshot_sources(directory: &Path, snapshot: &Path, sources: &[PathBuf]) -> Result<(), String> {
    for relative in sources {
        let destination = snapshot.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create source snapshot: {error}"))?;
        }
        fs::copy(directory.join(relative), &destination)
            .map_err(|error| format!("could not snapshot {}: {error}", relative.display()))?;
    }
    Ok(())
}

#[derive(Clone)]
struct SourceRevisionProbe {
    digest: [u8; 32],
    metadata: Vec<ArtifactFileIdentity>,
}

impl SourceRevisionProbe {
    fn is_current(&self, root: &Path, sources: &[PathBuf]) -> bool {
        self.metadata.len() == sources.len()
            && sources
                .iter()
                .zip(&self.metadata)
                .all(|(relative, expected)| {
                    artifact_file_identity(&root.join(relative)).ok().as_ref() == Some(expected)
                })
    }
}

#[derive(Default)]
struct HistoryProbeCache {
    sources: BTreeMap<Vec<PathBuf>, SourceRevisionProbe>,
    source_probes: usize,
}

impl HistoryProbeCache {
    fn source_digest(&mut self, root: &Path, sources: &[PathBuf]) -> Result<[u8; 32], String> {
        if let Some(probe) = self.sources.get(sources) {
            return Ok(probe.digest);
        }
        self.refresh_source_digest(root, sources)
    }

    fn refresh_source_digest(
        &mut self,
        root: &Path,
        sources: &[PathBuf],
    ) -> Result<[u8; 32], String> {
        let probe = source_revision_probe(root, sources)?;
        let digest = probe.digest;
        self.sources.insert(sources.to_vec(), probe);
        self.source_probes += 1;
        Ok(digest)
    }

    fn source_probe_is_current(&self, root: &Path, sources: &[PathBuf]) -> bool {
        self.sources
            .get(sources)
            .is_some_and(|probe| probe.is_current(root, sources))
    }
}

fn source_revision_probe(root: &Path, sources: &[PathBuf]) -> Result<SourceRevisionProbe, String> {
    let mut hasher = Sha256::new();
    let mut metadata = Vec::with_capacity(sources.len());
    hasher.update(b"CINDER-SOURCE-REVISION-1");
    for relative in sources {
        let path = root.join(relative);
        let mut file = fs::File::open(&path)
            .map_err(|error| format!("could not open source {}: {error}", path.display()))?;
        let before =
            artifact_file_identity_from_metadata(&file.metadata().map_err(|error| {
                format!("could not inspect source {}: {error}", path.display())
            })?)?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .map_err(|error| format!("could not read source {}: {error}", path.display()))?;
        let after =
            artifact_file_identity_from_metadata(&file.metadata().map_err(|error| {
                format!("could not inspect source {}: {error}", path.display())
            })?)?;
        if before != after || after != artifact_file_identity(&path)? {
            return Err(format!(
                "source changed while Cinder inspected it: {}",
                path.display()
            ));
        }
        append_context_value(&mut hasher, relative.as_os_str().as_bytes());
        append_context_value(&mut hasher, &contents);
        metadata.push(after);
    }
    Ok(SourceRevisionProbe {
        digest: hasher.finalize().into(),
        metadata,
    })
}

fn source_revision_digest(root: &Path, sources: &[PathBuf]) -> Result<[u8; 32], String> {
    source_revision_probe(root, sources).map(|probe| probe.digest)
}

struct LiteralChange {
    relative: PathBuf,
    old: Vec<u8>,
    new: Vec<u8>,
    new_source: Vec<u8>,
}

fn find_literal_change(
    directory: &Path,
    snapshot: &Path,
    sources: &[PathBuf],
) -> Result<Option<LiteralChange>, String> {
    let mut change = None;
    for relative in sources {
        let old = fs::read(snapshot.join(relative)).map_err(|error| {
            format!(
                "could not read source snapshot {}: {error}",
                relative.display()
            )
        })?;
        let new = fs::read(directory.join(relative))
            .map_err(|error| format!("could not read source {}: {error}", relative.display()))?;
        if old == new {
            continue;
        }
        if change.is_some() {
            return Ok(None);
        }
        let Some((old_literal, new_literal)) = changed_plain_literal(&old, &new) else {
            return Ok(None);
        };
        change = Some(LiteralChange {
            relative: relative.clone(),
            old: old_literal,
            new: new_literal,
            new_source: new,
        });
    }
    Ok(change)
}

fn sources_are_unchanged(
    directory: &Path,
    snapshot: &Path,
    sources: &[PathBuf],
) -> Result<bool, String> {
    for relative in sources {
        if fs::read(directory.join(relative))
            .map_err(|error| format!("could not read {}: {error}", relative.display()))?
            != fs::read(snapshot.join(relative)).map_err(|error| {
                format!(
                    "could not read source snapshot {}: {error}",
                    relative.display()
                )
            })?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn changed_plain_literal(old: &[u8], new: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if old.len() != new.len() || old == new {
        return None;
    }
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let changed_end = old.len() - suffix;
    let old_literal = source_string_literals(old)
        .into_iter()
        .find(|literal| literal.range.start <= prefix && literal.range.end >= changed_end)?;
    let new_literal = source_string_literals(new)
        .into_iter()
        .find(|literal| literal.range == old_literal.range)?;
    changed_literal_data(old_literal.bytes, new_literal.bytes)
}

fn changed_literal_data(old: &[u8], new: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if old.len() != new.len()
        || old == new
        || std::str::from_utf8(old).is_err()
        || std::str::from_utf8(new).is_err()
        || old.len() < 8
    {
        return None;
    }
    if old.contains(&b'{') || old.contains(&b'}') || new.contains(&b'{') || new.contains(&b'}') {
        let (old, new) = changed_format_segment(old, new)?;
        Some((old.to_vec(), new.to_vec()))
    } else {
        Some((old.to_vec(), new.to_vec()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PatchMode {
    RunSibling,
    BuildInPlace,
}

#[derive(Debug, Eq, PartialEq)]
struct AdHocSignatureMetadata {
    identifier: Option<Vec<u8>>,
    flags: BTreeSet<Vec<u8>>,
    runtime_version: Option<Vec<u8>>,
    internal_requirements: Option<Vec<u8>>,
    entitlements: Vec<u8>,
    linker_signed: bool,
}

enum CodeSignatureContract {
    Unsigned,
    AdHoc(AdHocSignatureMetadata),
    Unsupported,
}

fn patch_artifact(
    directory: &Path,
    state: &State,
    change: &LiteralChange,
    mode: PatchMode,
) -> Result<Option<PathBuf>, String> {
    let started = Instant::now();
    let Some((old_patch, new_patch, offset)) = state.indexed_patch(change) else {
        return Ok(None);
    };
    trace_run("locate indexed literal", started);

    let signature_contract = code_signature_contract(&state.artifact)?;
    if matches!(signature_contract, CodeSignatureContract::Unsupported) {
        return Ok(None);
    }

    let parent = state
        .artifact
        .parent()
        .ok_or_else(|| format!("artifact has no parent: {}", state.artifact.display()))?;
    record_artifact_root(directory, parent)?;
    let program_name = state.program_name.to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let patched = match mode {
        PatchMode::RunSibling => parent.join(format!(
            ".cinder-fast-{}-patch-{}-{nonce}-{program_name}",
            project_namespace(directory),
            std::process::id()
        )),
        PatchMode::BuildInPlace => state.artifact.clone(),
    };
    let temporary = parent.join(format!(
        ".cinder-patch-{}-{nonce}-{program_name}",
        std::process::id()
    ));
    let clone_started = Instant::now();
    clone_file(&state.artifact, &temporary)?;
    make_owner_writable(&temporary)?;
    trace_run("clone artifact", clone_started);
    let result = (|| {
        let write_started = Instant::now();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("could not open staged artifact: {error}"))?;
        let mut existing = vec![0; old_patch.len()];
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(&mut existing))
            .map_err(|error| format!("could not validate staged artifact: {error}"))?;
        if existing != old_patch {
            return Err("indexed literal no longer matches the staged artifact".to_owned());
        }
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.write_all(new_patch))
            .map_err(|error| format!("could not patch staged artifact: {error}"))?;
        debug_assert_eq!(old_patch.len(), new_patch.len());
        drop(file);
        trace_run("write artifact", write_started);
        let sign_started = Instant::now();
        let status = Command::new("/usr/bin/codesign")
            .args([
                OsStr::new("--force"),
                OsStr::new("--sign"),
                OsStr::new("-"),
                OsStr::new(
                    "--preserve-metadata=identifier,entitlements,requirements,flags,runtime,launch-constraints,library-constraints",
                ),
                temporary.as_os_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| format!("could not sign staged artifact: {error}"))?;
        if !status.success() {
            return Err("ad-hoc artifact signing failed".to_owned());
        }
        if let CodeSignatureContract::AdHoc(expected) = &signature_contract {
            let CodeSignatureContract::AdHoc(actual) = code_signature_contract(&temporary)? else {
                return Err("patched artifact lost its ad-hoc signature contract".to_owned());
            };
            if !code_signature_metadata_matches(expected, &actual) {
                return Err("patched artifact changed its code-signature metadata".to_owned());
            }
        }
        trace_run("sign artifact", sign_started);
        remove_launch_xattrs(&temporary);
        if mode == PatchMode::BuildInPlace {
            let modified = fs::metadata(&state.artifact)
                .and_then(|metadata| metadata.modified())
                .map_err(|error| format!("could not preserve Cargo artifact timestamp: {error}"))?;
            fs::File::open(&temporary)
                .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
                .map_err(|error| format!("could not restore Cargo artifact timestamp: {error}"))?;
        }
        fs::rename(&temporary, &patched)
            .map_err(|error| format!("could not publish staged artifact: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    trace_run("patch artifact total", started);
    Ok(Some(patched))
}

fn code_signature_contract(path: &Path) -> Result<CodeSignatureContract, String> {
    let display = Command::new("/usr/bin/codesign")
        .args([
            OsStr::new("-d"),
            OsStr::new("--verbose=4"),
            OsStr::new("--entitlements"),
            OsStr::new("-"),
            path.as_os_str(),
        ])
        .output()
        .map_err(|error| format!("could not inspect artifact signature: {error}"))?;
    if !display.status.success() {
        return if display
            .stderr
            .windows(b"is not signed at all".len())
            .any(|window| window == b"is not signed at all")
        {
            Ok(CodeSignatureContract::Unsigned)
        } else {
            Ok(CodeSignatureContract::Unsupported)
        };
    }
    let lines: Vec<_> = display.stderr.split(|byte| *byte == b'\n').collect();
    if !lines.contains(&b"Signature=adhoc".as_slice()) {
        return Ok(CodeSignatureContract::Unsupported);
    }
    let flags_line = lines
        .iter()
        .find_map(|line| line.strip_prefix(b"CodeDirectory "));
    let flags: BTreeSet<Vec<u8>> = flags_line
        .and_then(|line| {
            line.windows(b"flags=".len())
                .position(|window| window == b"flags=")
                .map(|offset| &line[offset + b"flags=".len()..])
        })
        .and_then(|value| value.split(|byte| *byte == b' ').next())
        .and_then(|value| {
            let start = value.iter().position(|byte| *byte == b'(')? + 1;
            let end = value.iter().position(|byte| *byte == b')')?;
            Some(&value[start..end])
        })
        .map(|flags| {
            flags
                .split(|byte| *byte == b',')
                .filter(|flag| !flag.is_empty())
                .map(<[u8]>::to_vec)
                .collect()
        })
        .unwrap_or_default();
    Ok(CodeSignatureContract::AdHoc(AdHocSignatureMetadata {
        identifier: signature_value(&lines, b"Identifier="),
        runtime_version: signature_value(&lines, b"Runtime Version="),
        internal_requirements: lines
            .iter()
            .find(|line| line.starts_with(b"Internal requirements"))
            .map(|line| line.to_vec()),
        linker_signed: flags.contains(b"linker-signed".as_slice()),
        flags,
        entitlements: display.stdout,
    }))
}

fn signature_value(lines: &[&[u8]], prefix: &[u8]) -> Option<Vec<u8>> {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(prefix))
        .map(<[u8]>::to_vec)
}

fn code_signature_metadata_matches(
    expected: &AdHocSignatureMetadata,
    actual: &AdHocSignatureMetadata,
) -> bool {
    let mut expected_flags = expected.flags.clone();
    expected_flags.remove(b"linker-signed".as_slice());
    expected.entitlements == actual.entitlements
        && expected_flags == actual.flags
        && expected.runtime_version == actual.runtime_version
        && (expected.linker_signed
            || (expected.identifier == actual.identifier
                && expected.internal_requirements == actual.internal_requirements))
}

fn trace_run(stage: &str, started: Instant) {
    if env::var_os("CINDER_TRACE_RUN").is_some() {
        eprintln!(
            "    Cinder trace: {stage} {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
}

fn changed_format_segment<'a>(old: &'a [u8], new: &'a [u8]) -> Option<(&'a [u8], &'a [u8])> {
    let old_open = old.iter().position(|byte| *byte == b'{')?;
    let new_open = new.iter().position(|byte| *byte == b'{')?;
    if old[..old_open] != new[..new_open]
        || old[old_open + 1..].contains(&b'{')
        || new[new_open + 1..].contains(&b'{')
    {
        return None;
    }
    let old_changed = old.get(old_open + 1..)?.strip_prefix(b"}")?;
    let new_changed = new.get(new_open + 1..)?.strip_prefix(b"}")?;
    (old_changed.len() >= 8 && old_changed.len() == new_changed.len())
        .then_some((old_changed, new_changed))
}

fn clone_file(source: &Path, destination: &Path) -> Result<(), String> {
    let status = Command::new("/bin/cp")
        .args([
            OsStr::new("-c"),
            source.as_os_str(),
            destination.as_os_str(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !status.is_ok_and(|status| status.success()) {
        fs::copy(source, destination)
            .map_err(|error| format!("could not clone artifact {}: {error}", source.display()))?;
    }
    let mode = fs::metadata(source)
        .map_err(|error| format!("could not inspect {}: {error}", source.display()))?
        .permissions()
        .mode();
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("could not preserve artifact permissions: {error}"))
}

fn make_cached_artifact_read_only(path: &Path) -> Result<(), String> {
    let mode = fs::metadata(path)
        .map_err(|error| format!("could not inspect cached artifact: {error}"))?
        .permissions()
        .mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode & !0o222))
        .map_err(|error| format!("could not protect cached artifact: {error}"))
}

fn make_owner_writable(path: &Path) -> Result<(), String> {
    let mode = fs::metadata(path)
        .map_err(|error| format!("could not inspect restored artifact: {error}"))?
        .permissions()
        .mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))
        .map_err(|error| format!("could not make restored artifact writable: {error}"))
}

fn remove_launch_xattrs(path: &Path) {
    for attribute in ["com.apple.provenance", "com.apple.quarantine"] {
        let _ = Command::new("/usr/bin/xattr")
            .args([OsStr::new("-d"), OsStr::new(attribute), path.as_os_str()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CargoOutputs, CodeSignatureContract, DISABLE_FAST_BUILD, LaunchPolicy, LiteralChange,
        LiteralIndexEntry, PatchMode, RUN_CONTEXT_FILE, State, StateKind, StatePublication,
        artifact_file_identity, cargo_config_contents_may_change_runner_or_target,
        cargo_fingerprint_value, cargo_fingerprint_value_index, changed_format_segment,
        changed_plain_literal, code_signature_contract, directory_logical_bytes,
        environment_affects_context, patch_artifact, project_namespace, prune_global_history_at,
        prune_run_artifacts, prune_stale_artifact_staging, prune_stale_global_staging,
        prune_stale_history_staging, prune_stale_project_staging, record_artifact_root,
        run_context, rustc_list_options, source_literal_candidates, state_directory,
        state_project_directory, toml_string, unsupported_argument, write_state_directory,
    };
    use std::{
        ffi::{OsStr, OsString},
        fs,
        path::{Path, PathBuf},
        process::Command,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn global_revision_cache_prunes_oldest_bytes_and_deleted_workspaces() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cinder-global-prune-{}-{unique}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("workspace"),
            workspace.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        let history = project.join("build-history");
        write_cache_entry(
            &history.join("old"),
            b"old!",
            UNIX_EPOCH + Duration::from_secs(10),
        );
        write_cache_entry(
            &history.join("new"),
            b"new!",
            UNIX_EPOCH + Duration::from_secs(20),
        );
        let deleted = root.join("deleted-project");
        fs::create_dir_all(deleted.join("run-history/entry")).unwrap();
        fs::write(
            deleted.join("workspace"),
            root.join("missing-workspace")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();

        let newest_entry_bytes = directory_logical_bytes(&history.join("new")).unwrap();
        prune_global_history_at(&root, newest_entry_bytes, UNIX_EPOCH).unwrap();

        assert!(!history.join("old").exists());
        assert!(history.join("new").is_dir());
        assert!(!deleted.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn global_revision_budget_includes_restored_run_artifacts() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cinder-run-artifact-budget-{}-{unique}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let project = root.join("project");
        let entry = project.join("run-history/entry");
        let source_root = root.join("source");
        let target = workspace.join("target/debug");
        fs::create_dir_all(&entry).unwrap();
        fs::create_dir_all(source_root.join("src")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source_root.join("src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(
            project.join("workspace"),
            workspace.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        let cached = entry.join("cached-artifact");
        fs::write(&cached, [0_u8; 64]).unwrap();
        let identity = artifact_file_identity(&cached).unwrap();
        let digest = [7_u8; 32];
        let public = target.join("app");
        let sources = vec![PathBuf::from("src/main.rs")];
        let cargo_outputs = CargoOutputs {
            dependency_file: target.join("deps/app.d"),
            artifact: target.join("deps/app"),
            fingerprint: target.join(".fingerprint/app"),
        };
        write_state_directory(
            &entry,
            StatePublication {
                source_root: &source_root,
                source_digest: &[0; 32],
                artifact: &cached,
                artifact_file_identity: &identity,
                artifact_digest: Some(&digest),
                public_artifact: &public,
                program_name: OsStr::new("app"),
                literal_index: &[],
                run_context: &[],
                observes_underscore: false,
                inputs: &[],
                sources: &sources,
                cargo_outputs: &cargo_outputs,
                runtime_environment: &[],
                duplicate_ready: false,
            },
            &cached,
            &cached,
        )
        .unwrap();
        let restored = target.join(format!(
            ".cinder-fast-{}-{}-app",
            project_namespace(&workspace),
            "07".repeat(32)
        ));
        fs::write(&restored, [0_u8; 128]).unwrap();
        let entry_bytes = directory_logical_bytes(&entry).unwrap();

        prune_global_history_at(&root, entry_bytes, UNIX_EPOCH).unwrap();

        assert!(
            !entry.exists(),
            "restored run artifact bytes were excluded from the global budget"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_targets_are_project_namespaced_and_deleted_workspaces_clean_their_artifacts() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cinder-shared-target-prune-{}-{unique}",
            std::process::id()
        ));
        let workspace_a = root.join("workspace-a");
        let workspace_b = root.join("workspace-b");
        let shared_target = root.join("shared-target/debug");
        fs::create_dir_all(&workspace_a).unwrap();
        fs::create_dir_all(&workspace_b).unwrap();
        fs::create_dir_all(&shared_target).unwrap();
        record_artifact_root(&workspace_a, &shared_target).unwrap();
        record_artifact_root(&workspace_b, &shared_target).unwrap();
        let artifact_a = shared_target.join(format!(
            ".cinder-fast-{}-{}-app",
            project_namespace(&workspace_a),
            "0a".repeat(32)
        ));
        let artifact_b = shared_target.join(format!(
            ".cinder-fast-{}-{}-app",
            project_namespace(&workspace_b),
            "0b".repeat(32)
        ));
        fs::write(&artifact_a, [0_u8; 64]).unwrap();
        fs::write(&artifact_b, [0_u8; 64]).unwrap();

        prune_run_artifacts(&workspace_a).unwrap();

        assert!(!artifact_a.exists());
        assert!(
            artifact_b.is_file(),
            "one workspace pruned another workspace's shared-target artifact"
        );

        let state_root = root.join("state");
        let deleted_workspace = root.join("deleted-workspace");
        let deleted_project = state_root.join("deleted-project");
        let registry = deleted_project.join("run-artifact-roots");
        fs::create_dir_all(&registry).unwrap();
        fs::write(
            deleted_project.join("workspace"),
            deleted_workspace.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        fs::write(
            registry.join("root"),
            shared_target.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        let deleted_artifact = shared_target.join(format!(
            ".cinder-fast-{}-{}-app",
            project_namespace(&deleted_workspace),
            "0c".repeat(32)
        ));
        fs::write(&deleted_artifact, [0_u8; 64]).unwrap();

        prune_global_history_at(&state_root, u64::MAX, UNIX_EPOCH).unwrap();

        assert!(!deleted_artifact.exists());
        assert!(!deleted_project.exists());
        assert!(artifact_b.is_file());
        let _ = fs::remove_dir_all(state_project_directory(&workspace_a));
        let _ = fs::remove_dir_all(state_project_directory(&workspace_b));
        fs::remove_dir_all(root).unwrap();
    }

    fn write_cache_entry(path: &Path, artifact: &[u8], modified: SystemTime) {
        fs::create_dir_all(path.join("snapshot")).unwrap();
        fs::write(path.join("cached-artifact"), artifact).unwrap();
        fs::write(path.join("snapshot/source.rs"), [b's'; 32]).unwrap();
        fs::write(path.join("last-used"), b"used").unwrap();
        fs::File::open(path.join("last-used"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }

    #[test]
    fn stale_crash_staging_is_pruned_without_touching_live_publishers() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cinder-staging-prune-{}-{unique}",
            std::process::id()
        ));
        let history = root.join("history");
        let artifacts = root.join("target");
        let project = root.join("project");
        fs::create_dir_all(&history).unwrap();
        fs::create_dir_all(&artifacts).unwrap();
        fs::create_dir_all(&project).unwrap();
        let stale_history = history.join(".tmp-2147483647-1");
        let live_history = history.join(format!(".tmp-{}-2", std::process::id()));
        fs::create_dir(&stale_history).unwrap();
        fs::create_dir(&live_history).unwrap();
        fs::write(stale_history.join("cached-artifact"), [0_u8; 32]).unwrap();
        fs::write(live_history.join("cached-artifact"), [0_u8; 32]).unwrap();
        let stale_restore = artifacts.join(".cinder-restore-2147483647-1-app");
        let stale_patch = artifacts.join(".cinder-patch-2147483647-app");
        let live_restore = artifacts.join(format!(".cinder-restore-{}-2-app", std::process::id()));
        fs::write(&stale_restore, [0_u8; 32]).unwrap();
        fs::write(&stale_patch, [0_u8; 32]).unwrap();
        fs::write(&live_restore, [0_u8; 32]).unwrap();
        let project_stages = [
            "run.capture-2147483647",
            "build.capture-2147483647",
            "run.patch-2147483647",
            "build.patch-2147483647",
            "run.tmp-2147483647-1",
            "build.tmp-2147483647-1",
        ]
        .map(|name| project.join(name));
        for path in &project_stages {
            fs::create_dir(path).unwrap();
            fs::write(path.join("snapshot"), [0_u8; 32]).unwrap();
        }
        let stale_context = project.join("run-context-2147483647");
        fs::write(&stale_context, [0_u8; 32]).unwrap();
        let live_project_stage = project.join(format!("run.tmp-{}-2", std::process::id()));
        fs::create_dir(&live_project_stage).unwrap();
        for path in [
            &stale_history,
            &live_history,
            &stale_restore,
            &stale_patch,
            &live_restore,
        ] {
            fs::File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
                .unwrap();
        }
        for path in project_stages
            .iter()
            .chain([&stale_context, &live_project_stage])
        {
            fs::File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
                .unwrap();
        }

        let cinder_root = root.join("cinder");
        let receipt = cinder_root.join("receipts/2147483647-1");
        let recording = cinder_root.join("recordings/run-2147483647-1");
        fs::create_dir_all(&receipt).unwrap();
        fs::create_dir_all(recording.parent().unwrap()).unwrap();
        fs::write(&recording, [0_u8; 32]).unwrap();
        for path in [&receipt, &recording] {
            fs::File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
                .unwrap();
        }

        prune_stale_history_staging(&history, SystemTime::now()).unwrap();
        prune_stale_artifact_staging(&artifacts, SystemTime::now()).unwrap();
        prune_stale_project_staging(&project, SystemTime::now()).unwrap();
        prune_stale_global_staging(&cinder_root.join("state"), SystemTime::now()).unwrap();

        assert!(!stale_history.exists());
        assert!(!stale_restore.exists());
        assert!(!stale_patch.exists());
        assert!(project_stages.iter().all(|path| !path.exists()));
        assert!(!stale_context.exists());
        assert!(!receipt.exists());
        assert!(!recording.exists());
        assert!(live_history.is_dir());
        assert!(live_restore.is_file());
        assert!(live_project_stage.is_dir());

        let workspace = root.join("slot-workspace");
        let slot_root = workspace.join("target/debug");
        fs::create_dir_all(&slot_root).unwrap();
        let old_slot = slot_root.join(format!(
            ".cinder-fast-{}-patch-2147483647-1-old-program",
            project_namespace(&workspace)
        ));
        fs::write(&old_slot, [0_u8; 64]).unwrap();
        fs::File::open(&old_slot)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
        record_artifact_root(&workspace, &slot_root).unwrap();
        prune_run_artifacts(&workspace).unwrap();
        assert!(!old_slot.exists());
        let _ = fs::remove_dir_all(state_project_directory(&workspace));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patching_preserves_ad_hoc_entitlements_and_hardened_runtime() {
        let old = b"cinder-signature-metadata-alpha";
        let new = b"cinder-signature-metadata-bravo";
        std::hint::black_box(old);
        assert_eq!(old.len(), new.len());
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cinder-signature-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let artifact = root.join("signed-artifact");
        fs::copy(std::env::current_exe().unwrap(), &artifact).unwrap();
        let entitlements = root.join("entitlements.plist");
        fs::write(
            &entitlements,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>com.apple.security.get-task-allow</key><true/></dict></plist>\n",
        )
        .unwrap();
        let signed = Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--sign",
                "-",
                "--identifier",
                "dev.cinder.signature-fixture",
                "--options",
                "runtime",
                "--entitlements",
            ])
            .arg(&entitlements)
            .arg(&artifact)
            .output()
            .unwrap();
        assert!(
            signed.status.success(),
            "{}",
            String::from_utf8_lossy(&signed.stderr)
        );

        let contents = fs::read(&artifact).unwrap();
        let offset = contents
            .windows(old.len())
            .position(|window| window == old)
            .expect("test literal is absent from the test executable") as u64;
        let identity = artifact_file_identity(&artifact).unwrap();
        let state = State {
            snapshot: root.join("snapshot"),
            source_digest: [0; 32],
            artifact: artifact.clone(),
            public_artifact: artifact.clone(),
            program_name: OsString::from("signed-artifact"),
            artifact_file_identity: identity,
            artifact_digest: None,
            literal_index: vec![LiteralIndexEntry {
                bytes: old.to_vec(),
                offset,
            }],
            run_context: Vec::new(),
            observes_underscore: false,
            inputs: Vec::new(),
            sources: Vec::new(),
            cargo_outputs: CargoOutputs {
                dependency_file: PathBuf::new(),
                artifact: PathBuf::new(),
                fingerprint: PathBuf::new(),
            },
            runtime_environment: Vec::new(),
        };
        let change = LiteralChange {
            relative: PathBuf::from("src/main.rs"),
            old: old.to_vec(),
            new: new.to_vec(),
            new_source: Vec::new(),
        };
        let first_run = patch_artifact(&root, &state, &change, PatchMode::RunSibling)
            .unwrap()
            .unwrap();
        let second_run = patch_artifact(&root, &state, &change, PatchMode::RunSibling)
            .unwrap()
            .unwrap();
        assert_ne!(
            first_run, second_run,
            "concurrent run patches must have immutable publication paths"
        );
        assert_eq!(
            patch_artifact(&root, &state, &change, PatchMode::BuildInPlace)
                .unwrap()
                .as_deref(),
            Some(artifact.as_path())
        );

        let CodeSignatureContract::AdHoc(metadata) = code_signature_contract(&artifact).unwrap()
        else {
            panic!("patched artifact lost its ad-hoc signature");
        };
        assert!(metadata.flags.contains(b"runtime".as_slice()));
        assert!(
            metadata
                .entitlements
                .windows(b"com.apple.security.get-task-allow".len())
                .any(|window| window == b"com.apple.security.get-task-allow")
        );
        let verified = Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(&artifact)
            .output()
            .unwrap();
        assert!(
            verified.status.success(),
            "{}",
            String::from_utf8_lossy(&verified.stderr)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_duplicate_token_available_for_a_late_watcher_event() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "cinder-duplicate-token-{}-{unique}",
            std::process::id()
        ));
        let state = state_directory(&directory, StateKind::Run);
        fs::create_dir_all(&state).unwrap();
        let token = state.join("duplicate-ready");
        fs::write(&token, unique.to_string()).unwrap();

        assert!(State::fresh_duplicate_is_pending(&directory).unwrap());
        assert!(token.is_file());
        assert!(State::consume_fresh_duplicate(&directory).unwrap());
        assert!(!token.exists());

        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn separates_immediate_and_watcher_run_state() {
        let arguments = [OsString::from("run")];
        assert_ne!(
            run_context(&arguments, LaunchPolicy::Immediate),
            run_context(&arguments, LaunchPolicy::CoalesceDuplicateEvents)
        );
    }

    #[test]
    fn hashes_context_instead_of_persisting_arguments_or_environment() {
        let secret = OsString::from("--features=do-not-persist-this-value");
        let context = run_context(&[OsString::from("run"), secret], LaunchPolicy::Immediate);

        assert_eq!(context.len(), 32);
        assert!(
            !context
                .windows(14)
                .any(|window| window == b"do-not-persist")
        );
    }

    #[test]
    fn preserves_compiler_observable_environment_in_build_contexts() {
        assert!(!environment_affects_context(OsStr::new("_")));
        assert!(environment_affects_context(OsStr::new("SHLVL")));
        assert!(environment_affects_context(OsStr::new("CINDER_TRACE_RUN")));
        assert!(environment_affects_context(OsStr::new("RUSTFLAGS")));
        assert!(environment_affects_context(OsStr::new("BUN_CODEGEN_DIR")));
        assert!(!environment_affects_context(OsStr::new(RUN_CONTEXT_FILE)));
        assert!(!environment_affects_context(OsStr::new(DISABLE_FAST_BUILD)));
    }

    #[test]
    fn decodes_cargo_fingerprint_values_as_little_endian_hex() {
        assert_eq!(
            cargo_fingerprint_value(b"e92cb941684b739f"),
            Some(11_489_609_985_503_603_945)
        );
        assert_eq!(cargo_fingerprint_value(b"not-a-fingerprint"), None);
    }

    #[test]
    fn retains_duplicate_cargo_fingerprint_values_conservatively() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cinder-fingerprint-index-{}-{unique}",
            std::process::id()
        ));
        for directory in ["dependency-one", "dependency-two"] {
            let directory = root.join(directory);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("lib-dependency"), b"e92cb941684b739f").unwrap();
        }

        let index = cargo_fingerprint_value_index(&root).unwrap();
        assert_eq!(index[&11_489_609_985_503_603_945].len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collects_every_rustc_crate_type_occurrence() {
        let arguments = [
            OsString::from("--crate-type"),
            OsString::from("rlib"),
            OsString::from("--crate-type=staticlib"),
        ];
        assert_eq!(
            rustc_list_options(&arguments, "--crate-type"),
            ["rlib", "staticlib"]
        );
    }

    #[test]
    fn quotes_runner_paths_for_inline_toml() {
        assert_eq!(
            toml_string(OsStr::new("/tmp/Cinder \"dev\"/cinder")).unwrap(),
            r#""/tmp/Cinder \"dev\"/cinder""#
        );
    }

    #[test]
    fn keeps_release_and_custom_target_runs_on_standard_cargo() {
        assert!(unsupported_argument("--release"));
        assert!(unsupported_argument("--profile=fast"));
        assert!(unsupported_argument("--target"));
        assert!(!unsupported_argument("--bin"));
        assert!(!unsupported_argument("--features"));
    }

    #[test]
    fn cargo_config_only_disables_overrides_that_change_execution() {
        let zed_style = r#"
            [build]
            rustflags = ["--cfg", "tokio_unstable"]

            [target.'cfg(target_os = "windows")']
            rustflags = ["-C", "target-feature=+crt-static"]
        "#;
        assert!(!cargo_config_contents_may_change_runner_or_target(zed_style).unwrap());
        assert!(
            cargo_config_contents_may_change_runner_or_target(
                "[build]\ntarget = \"wasm32-unknown-unknown\"\n"
            )
            .unwrap()
        );
        assert!(
            cargo_config_contents_may_change_runner_or_target(
                "[target.aarch64-apple-darwin]\nrunner = \"tool\"\n"
            )
            .unwrap()
        );
    }

    #[test]
    fn accepts_equal_length_format_and_ordinary_string_changes() {
        let old = b"fn value() { format!(\"{} cinder-one\", value); }";
        let new = b"fn value() { format!(\"{} cinder-two\", value); }";
        assert_eq!(
            changed_plain_literal(old, new),
            Some((b" cinder-one".to_vec(), b" cinder-two".to_vec()))
        );
        assert_eq!(
            changed_plain_literal(
                b"fn label() { show(\"Camera Preview\"); }",
                b"fn label() { show(\"Camera Review \"); }"
            ),
            Some((b"Camera Preview".to_vec(), b"Camera Review ".to_vec()))
        );
        assert_eq!(
            changed_plain_literal(
                "fn label() { show(\"Status: 🟢\"); }".as_bytes(),
                "fn label() { show(\"Status: 🔴\"); }".as_bytes()
            ),
            Some((
                "Status: 🟢".as_bytes().to_vec(),
                "Status: 🔴".as_bytes().to_vec()
            ))
        );
        assert!(
            changed_plain_literal(old, b"fn value() { format!(\"{} longer-value\", value); }")
                .is_none()
        );
        assert!(
            changed_plain_literal(
                b"fn value() { show(r#\"cinder-one\"#); }",
                b"fn value() { show(r#\"cinder-two\"#); }"
            )
            .is_none()
        );
    }

    #[test]
    fn extracts_one_changed_format_segment() {
        assert_eq!(
            changed_format_segment(
                b"window.FLAGS = {};/* cinder-bench-1 */",
                b"window.FLAGS = {};/* cinder-bench-2 */"
            ),
            Some((
                b";/* cinder-bench-1 */".as_slice(),
                b";/* cinder-bench-2 */".as_slice()
            ))
        );
        assert!(changed_format_segment(b"{one} {two}", b"{one} {next}").is_none());
    }

    #[test]
    fn indexes_ordinary_strings_and_static_format_segments() {
        assert_eq!(
            source_literal_candidates(
                b"show(\"Camera Preview\"); format!(\"window.FLAGS = {};/* cinder-bench-1 */\")"
            ),
            vec![
                b"Camera Preview".to_vec(),
                b";/* cinder-bench-1 */".to_vec()
            ]
        );
    }

    #[test]
    fn ignores_non_runtime_or_non_verbatim_string_syntax() {
        let source = br##"
            // show("line comment value");
            /* show("block comment value"); /* "nested comment value" */ */
            show(r#"raw string value"#);
            show(b"byte string value");
            show(c"C string value");
            let quote = '"';
            show("escaped\\tstring");
            show("Runtime Label");
        "##;
        assert_eq!(
            source_literal_candidates(source),
            vec![b"Runtime Label".to_vec()]
        );
    }
}
