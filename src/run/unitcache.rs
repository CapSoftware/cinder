//! Content-addressed, cross-project cache of registry dependency units.
//!
//! Every stored byte is one of Cargo's own build outputs — an rlib, rmeta,
//! loose codegen object, dep-info file, fingerprint directory file,
//! build-script executable, or build-script output tree — captured verbatim
//! after a successful real Cargo build and restored only into a target
//! directory whose expected compilation context (toolchain `-vV` identity,
//! profile, lockfile package set) matches the recorded one. Cargo's own
//! planning-time fingerprint comparison remains the final authority: a
//! restored unit is linked only when Cargo independently recomputes the
//! exact fingerprint the restored bytes carry, so a mis-keyed or stale
//! restore can at worst cause the normal recompile, never a wrong reuse.
//! Library units qualify only when their entire dependency closure is made
//! of registry-style immutable-source units — no proc-macro dylibs, no
//! workspace paths, no tracked source files inside the encoded dep-info. A
//! build-scripted registry package qualifies as an atomic three-entry group
//! (build-script compile unit, build-script run unit with its OUT_DIR tree,
//! library unit) when every group file is provably relocatable: donor-path
//! occurrences may appear only in UTF-8 text where a reversible prefix
//! substitution reproduces exactly what a native run would have written.
//! Every unrecognized layout, schema, or parse surprise disqualifies the
//! unit rather than widening the contract; a non-relocatable package writes
//! a persistent exclusion marker with its reason.
//!
//! The store is bounded, versioned, owner-only, and cleared by `cinder
//! clean`. A key that ever observes two different byte sets is permanently
//! marked unstable and never cached again.

#![cfg(target_os = "macos")]

use super::{OsStr, OsString, Path, PathBuf, TRACE_RUN, env, fs};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::time::{SystemTime, UNIX_EPOCH};

const META_MAGIC: &str = "CNDU0002";
const STORE_VERSION: &str = "v1";
const UNIT_CACHE_ROOT: &str = "CINDER_UNIT_CACHE";
const UNIT_CACHE_BYTES: &str = "CINDER_UNIT_CACHE_BYTES";
const DEFAULT_STORE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_UNIT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_UNIT_FILES: usize = 64;
const MAX_ENTRY_FILES: usize = 4_200;
const MAX_OUT_FILES: usize = 4_096;
const MAX_OUT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_OUT_COMPONENTS: usize = 24;
const MAX_META_BYTES: u64 = 1024 * 1024;
const MAX_LOCK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LOCK_PACKAGES: usize = 8_192;
const MAX_FINGERPRINT_DIRS: usize = 16_384;
const MAX_DEP_INFO_BYTES: u64 = 1024 * 1024;
const MAX_DEPENDENCY_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LOCK_ANCESTOR_DEPTH: usize = 16;

pub(crate) struct RestoreCounts {
    pub(crate) candidates: usize,
    pub(crate) restored: usize,
    pub(crate) skipped_existing: usize,
    pub(crate) skipped_conflict: usize,
    pub(crate) skipped_group: usize,
    pub(crate) digest_failures: usize,
}

/// The resolved location a restore or record operates on. Resolution is
/// deliberately narrow: any argument or configuration shape that could move
/// Cargo's output elsewhere leaves the cache inert for the invocation.
pub(crate) struct CacheLocation {
    pub(crate) profile_directory: PathBuf,
    pub(crate) profile_name: &'static str,
    pub(crate) workspace_root: PathBuf,
    pub(crate) lock_file: PathBuf,
}

fn trace(message: &str) {
    if env::var_os(TRACE_RUN).is_some() {
        eprintln!("    Cinder trace: {message}");
    }
}

pub(crate) fn store_root() -> Option<PathBuf> {
    if let Some(root) = env::var_os(UNIT_CACHE_ROOT) {
        if root.is_empty() {
            return None;
        }
        return Some(PathBuf::from(root).join(STORE_VERSION));
    }
    let home = env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join("Library/Caches/cinder/unit-cache")
            .join(STORE_VERSION),
    )
}

pub(crate) fn clear() -> Result<(), String> {
    let Some(root) = store_root() else {
        return Ok(());
    };
    let Some(base) = root.parent() else {
        return Ok(());
    };
    match fs::remove_dir_all(base) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not clear the Cinder unit cache {}: {error}",
            base.display()
        )),
    }
}

/// Resolves where Cargo will (or did) place this invocation's units. Any
/// shape this resolution does not model exactly returns `None` and keeps the
/// cache out of the command entirely.
pub(crate) fn resolve_location(arguments: &[OsString]) -> Option<CacheLocation> {
    let mut profile: Option<&'static str> = None;
    let mut pending_profile_value = false;
    for argument in arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
    {
        let argument = argument.to_str()?;
        if pending_profile_value {
            pending_profile_value = false;
            match argument {
                "release" => profile = Some("release"),
                "dev" => profile = Some("debug"),
                _ => return None,
            }
            continue;
        }
        if argument.starts_with('+')
            || argument == "--target"
            || argument.starts_with("--target=")
            || argument == "--target-dir"
            || argument.starts_with("--target-dir=")
            || argument == "--manifest-path"
            || argument.starts_with("--manifest-path=")
            || argument == "-C"
            || argument.starts_with("-C")
            || argument == "--config"
            || argument.starts_with("--config=")
            || argument == "-Z"
            || argument.starts_with("-Z")
        {
            return None;
        }
        if argument == "--release" || argument == "-r" {
            profile = Some("release");
        } else if argument == "--profile" {
            pending_profile_value = true;
        } else if let Some(value) = argument.strip_prefix("--profile=") {
            match value {
                "release" => profile = Some("release"),
                "dev" => profile = Some("debug"),
                _ => return None,
            }
        }
    }
    if pending_profile_value {
        return None;
    }
    let profile_name = profile.unwrap_or("debug");
    let directory = env::current_dir().ok()?;
    let directory = fs::canonicalize(directory).ok()?;
    let mut workspace_root = None;
    let mut candidate = directory.as_path();
    for _ in 0..MAX_LOCK_ANCESTOR_DEPTH {
        if candidate.join("Cargo.lock").is_file() {
            workspace_root = Some(candidate.to_owned());
            break;
        }
        candidate = candidate.parent()?;
    }
    let workspace_root = workspace_root?;
    // A configuration-selected build target or target directory moves output
    // somewhere this resolution does not model; stay out entirely.
    if super::cargo::cargo_config_may_move_build_output().unwrap_or(true) {
        return None;
    }
    let target_root = match env::var_os("CARGO_TARGET_DIR") {
        Some(configured) if !configured.is_empty() => {
            let configured = PathBuf::from(configured);
            if configured.is_absolute() {
                configured
            } else {
                directory.join(configured)
            }
        }
        Some(_) => return None,
        None => workspace_root.join("target"),
    };
    Some(CacheLocation {
        profile_directory: target_root.join(profile_name),
        profile_name,
        lock_file: workspace_root.join("Cargo.lock"),
        workspace_root,
    })
}

/// Hashes the verbose version report of the compiler Cargo will launch. The
/// tuned-toolchain routing has already exported `RUSTC` when it applies, so
/// this identity follows the same compiler Cargo resolves.
pub(crate) fn rustc_identity_digest() -> Option<String> {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = std::process::Command::new(rustc)
        .arg("-vV")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(&output.stdout);
    Some(hex(&digest.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        rendered.push_str(&format!("{byte:02x}"));
    }
    rendered
}

fn is_hex16(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalized(name: &str) -> String {
    name.replace('-', "_")
}

/// Splits a registry package directory name `<name>-<version>` at the
/// leftmost hyphen followed by a digit. A crate name segment that itself
/// starts with a digit mis-splits here; the resulting lockfile mismatch is a
/// silent miss, never a wrong entry.
fn split_package_directory_name(basename: &str) -> Option<(&str, &str)> {
    let bytes = basename.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'-'
            && bytes
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_digit())
        {
            let name = &basename[..index];
            let version = &basename[index + 1..];
            if !name.is_empty() && version_is_plausible(version) {
                return Some((name, version));
            }
            return None;
        }
    }
    None
}

fn version_is_plausible(version: &str) -> bool {
    version.contains('.')
        && version.len() <= 64
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

// ---------------------------------------------------------------------------
// Lockfile
// ---------------------------------------------------------------------------

/// Extracts `[[package]]` name/version pairs from a Cargo lockfile with a
/// deliberately minimal reader: any line shape outside the small expected
/// grammar abandons the whole parse, which quietly disables restoring.
pub(crate) fn lock_packages(path: &Path) -> Option<Vec<(String, String)>> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_LOCK_BYTES {
        return None;
    }
    let contents = fs::read_to_string(path).ok()?;
    let mut packages = Vec::new();
    let mut in_package = false;
    let mut in_array = false;
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let finish =
        |name: &mut Option<String>, version: &mut Option<String>, packages: &mut Vec<_>| {
            if let (Some(name), Some(version)) = (name.take(), version.take()) {
                packages.push((name, version));
            }
        };
    for line in contents.lines() {
        let line = line.trim_end();
        if in_array {
            let trimmed = line.trim();
            if trimmed == "]" {
                in_array = false;
                continue;
            }
            if quoted_array_entry(trimmed) {
                continue;
            }
            return None;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[package]]" {
            finish(&mut name, &mut version, &mut packages);
            if packages.len() >= MAX_LOCK_PACKAGES {
                return None;
            }
            in_package = true;
            continue;
        }
        if line.starts_with("[[") || line.starts_with('[') {
            finish(&mut name, &mut version, &mut packages);
            in_package = false;
            continue;
        }
        let (key, value) = line.split_once(" = ")?;
        if !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return None;
        }
        if value == "[" {
            in_array = true;
            continue;
        }
        if value == "[]" {
            continue;
        }
        let quoted = plain_quoted(value);
        if quoted.is_none() && !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        if in_package {
            match key {
                "name" => name = Some(quoted?.to_owned()),
                "version" => version = Some(quoted?.to_owned()),
                _ => {}
            }
        }
    }
    finish(&mut name, &mut version, &mut packages);
    Some(packages)
}

fn plain_quoted(value: &str) -> Option<&str> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"') && !inner.contains('\\')).then_some(inner)
}

fn quoted_array_entry(line: &str) -> bool {
    let line = line.strip_suffix(',').unwrap_or(line);
    plain_quoted(line).is_some()
}

// ---------------------------------------------------------------------------
// Encoded dep-info
// ---------------------------------------------------------------------------

/// A file Cargo's encoded dep-info tracks: the path-type byte and the path.
/// Registry-immutable sources are untracked; the only tracked shape a
/// cacheable unit may carry is a target-root-relative generated input.
struct EncodedDepInfoFile {
    // The decoded fields document the exact on-disk format and stay
    // available for later phases; today's policy only distinguishes an
    // empty tracked set from a non-empty one.
    #[allow(dead_code)]
    target_root_relative: bool,
    #[allow(dead_code)]
    path: String,
}

/// Parses Cargo's encoded dep-info. The format is matched byte-exactly
/// against the shape the running Cargo writes; any deviation disqualifies
/// the unit. Environment-value dependencies are not returned: Cargo itself
/// revalidates them against the live environment on every build.
fn parse_encoded_dep_info(bytes: &[u8]) -> Option<Vec<EncodedDepInfoFile>> {
    fn read_u32(bytes: &mut &[u8]) -> Option<u32> {
        let (head, tail) = bytes.split_at_checked(4)?;
        *bytes = tail;
        Some(u32::from_le_bytes(head.try_into().ok()?))
    }
    fn read_u8(bytes: &mut &[u8]) -> Option<u8> {
        let (head, tail) = bytes.split_at_checked(1)?;
        *bytes = tail;
        Some(head[0])
    }
    fn read_bytes<'a>(bytes: &mut &'a [u8], count: usize) -> Option<&'a [u8]> {
        let (head, tail) = bytes.split_at_checked(count)?;
        *bytes = tail;
        Some(head)
    }
    let mut bytes = bytes;
    let header = read_bytes(&mut bytes, 6)?;
    if header != [0x01, 0x00, 0x00, 0x00, 0xff, 0x01] {
        return None;
    }
    let file_count = read_u32(&mut bytes)?;
    if file_count > MAX_OUT_FILES as u32 {
        return None;
    }
    let mut files = Vec::new();
    for _ in 0..file_count {
        let path_type = read_u8(&mut bytes)?;
        let target_root_relative = match path_type {
            0 => false,
            1 => true,
            _ => return None,
        };
        let path_length = read_u32(&mut bytes)?;
        if path_length > 64 * 1024 {
            return None;
        }
        let path = read_bytes(&mut bytes, path_length as usize)?;
        let path = std::str::from_utf8(path).ok()?.to_owned();
        // The observed shape carries a trailing checksum-presence byte per
        // file; only the no-checksum form is supported.
        if read_u8(&mut bytes)? != 0 {
            return None;
        }
        files.push(EncodedDepInfoFile {
            target_root_relative,
            path,
        });
    }
    let environment_entries = read_u32(&mut bytes)?;
    if environment_entries > 4_096 {
        return None;
    }
    for _ in 0..environment_entries {
        let key_length = read_u32(&mut bytes)?;
        if key_length > 64 * 1024 {
            return None;
        }
        read_bytes(&mut bytes, key_length as usize)?;
        match read_u8(&mut bytes)? {
            0 => {}
            1 => {
                let value_length = read_u32(&mut bytes)?;
                if value_length > 1024 * 1024 {
                    return None;
                }
                read_bytes(&mut bytes, value_length as usize)?;
            }
            _ => return None,
        }
    }
    bytes.is_empty().then_some(files)
}

fn encoded_dep_info_tracks_no_files(bytes: &[u8]) -> bool {
    parse_encoded_dep_info(bytes).is_some_and(|files| files.is_empty())
}

// ---------------------------------------------------------------------------
// Fingerprint JSON
// ---------------------------------------------------------------------------

struct FingerprintFacts {
    dependency_edges: Vec<(String, u64)>,
    local: Vec<serde_json::Value>,
}

/// Accepts only the exact fingerprint envelope the proof characterized: the
/// known key set and host compilation. `declared_features` is tolerated as
/// absent because supported older toolchains predate it. The `local` entries
/// are returned for the unit-kind-specific validation.
fn parse_fingerprint_json(bytes: &[u8]) -> Option<FingerprintFacts> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    let mut keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    for required in [
        "rustc",
        "features",
        "target",
        "profile",
        "path",
        "deps",
        "local",
        "rustflags",
        "config",
        "compile_kind",
    ] {
        if !keys.remove(required) {
            return None;
        }
    }
    keys.remove("declared_features");
    if !keys.is_empty() {
        return None;
    }
    for numeric in ["rustc", "target", "profile", "path", "config"] {
        object.get(numeric)?.as_u64()?;
    }
    object.get("features")?.as_str()?;
    if let Some(declared) = object.get("declared_features") {
        declared.as_str()?;
    }
    if object.get("compile_kind")?.as_u64()? != 0 {
        return None;
    }
    let rustflags = object.get("rustflags")?.as_array()?;
    if !rustflags.iter().all(serde_json::Value::is_string) {
        return None;
    }
    let mut dependency_edges = Vec::new();
    for edge in object.get("deps")?.as_array()? {
        let edge = edge.as_array()?;
        let [_, name, public, fingerprint] = edge.as_slice() else {
            return None;
        };
        edge[0].as_u64()?;
        public.as_bool()?;
        dependency_edges.push((name.as_str()?.to_owned(), fingerprint.as_u64()?));
    }
    Some(FingerprintFacts {
        dependency_edges,
        local: object.get("local")?.as_array()?.clone(),
    })
}

/// Validates the single mtime-mode `CheckDepInfo` local entry a compiled
/// unit (library or build-script compile) must carry.
fn check_dep_info_local(
    local: &[serde_json::Value],
    profile_name: &str,
    fingerprint_directory_name: &str,
    hash_file_name: &str,
) -> Option<()> {
    let [local_entry] = local else {
        return None;
    };
    let check = local_entry.as_object()?;
    if check.len() != 1 {
        return None;
    }
    let dep_info = check.get("CheckDepInfo")?.as_object()?;
    if dep_info.len() != 2 || dep_info.get("checksum")?.as_bool()? {
        return None;
    }
    let recorded_path = dep_info.get("dep_info")?.as_str()?;
    let expected_path =
        format!("{profile_name}/.fingerprint/{fingerprint_directory_name}/dep-{hash_file_name}");
    (recorded_path == expected_path).then_some(())
}

/// Validates a build-script run unit's local entries. Both observed
/// renderings are admissible on cargo 1.88 and 1.93: a silent script gets
/// `Precalculated(<package version>)`; a directive-emitting script gets one
/// `RerunIfChanged` over package-relative paths plus any number of
/// `RerunIfEnvChanged` entries, which Cargo revalidates against the live
/// environment on every build. Anything else disqualifies the unit.
fn run_unit_local_is_supported(
    local: &[serde_json::Value],
    profile_name: &str,
    fingerprint_directory_name: &str,
    package_directory: &Path,
    package_version: &str,
) -> bool {
    if local.is_empty() {
        return false;
    }
    if let [only] = local {
        if let Some(object) = only.as_object() {
            if object.len() == 1 {
                if let Some(precalculated) = object.get("Precalculated") {
                    return precalculated.as_str() == Some(package_version);
                }
            }
        }
    }
    let mut saw_rerun_paths = false;
    for entry in local {
        let Some(object) = entry.as_object() else {
            return false;
        };
        if object.len() != 1 {
            return false;
        }
        if let Some(rerun) = object.get("RerunIfChanged") {
            let Some(rerun) = rerun.as_object() else {
                return false;
            };
            if saw_rerun_paths || rerun.len() != 2 {
                return false;
            }
            let expected_output =
                format!("{profile_name}/build/{fingerprint_directory_name}/output");
            if rerun.get("output").and_then(serde_json::Value::as_str) != Some(&expected_output) {
                return false;
            }
            let Some(paths) = rerun.get("paths").and_then(serde_json::Value::as_array) else {
                return false;
            };
            for path in paths {
                let Some(path) = path.as_str() else {
                    return false;
                };
                let path = Path::new(path);
                if path.is_absolute() {
                    return false;
                }
                // A relative rerun path resolves against the immutable
                // package directory and must stay inside it.
                let Some(resolved) = lexically_normalized(&package_directory.join(path)) else {
                    return false;
                };
                if !resolved.starts_with(package_directory) {
                    return false;
                }
            }
            saw_rerun_paths = true;
        } else if let Some(env) = object.get("RerunIfEnvChanged") {
            let Some(env) = env.as_object() else {
                return false;
            };
            if env.len() != 2
                || env.get("var").and_then(serde_json::Value::as_str).is_none()
                || !env
                    .get("val")
                    .is_some_and(|value| value.is_string() || value.is_null())
            {
                return false;
            }
        } else {
            return false;
        }
    }
    saw_rerun_paths
}

/// Renders a fingerprint hash the way Cargo names its hash files: the
/// little-endian bytes of the value, hex-encoded.
fn fingerprint_hash_text(value: u64) -> String {
    hex(&value.to_le_bytes())
}

// ---------------------------------------------------------------------------
// Dep-info (.d) files
// ---------------------------------------------------------------------------

struct DependencyFileFacts {
    package_directory: PathBuf,
    /// Donor-relative suffixes of target-directory prerequisites (an
    /// `include!` of a build-script-generated file). The caller must prove
    /// each one lives in the unit's own build-script output directory.
    target_sources: Vec<String>,
    /// The donor-relative value of a recorded `env-dep:OUT_DIR` read, which
    /// the caller must prove is the unit's own build-script output
    /// directory. The rewrite reproduces the native rendering on restore;
    /// Cargo's encoded dep-info drops the entry entirely (OUT_DIR is
    /// Cargo's own responsibility), so no absolute donor path survives
    /// anywhere Cargo compares bytes.
    out_dir_env: Option<String>,
}

/// Validates the textual dep-info file for a registry unit: every rule target
/// is either a donor-prefixed output path or a source under the package
/// directory, every prerequisite is a source under one package directory or a
/// donor-prefixed generated input returned for the caller to prove, and no
/// escape sequences appear anywhere, so a later restore can rewrite the donor
/// prefix as pure textual substitution provably equal to what Cargo would
/// have written natively.
fn parse_dependency_file(
    contents: &str,
    donor_prefix: &str,
    unit_ident: &str,
) -> Option<DependencyFileFacts> {
    if contents.contains('\\') {
        return None;
    }
    let mut package_directory: Option<PathBuf> = None;
    let mut target_sources: Vec<String> = Vec::new();
    let mut out_dir_env: Option<String> = None;
    let observe_source = |source: &str,
                          package_directory: &mut Option<PathBuf>,
                          target_sources: &mut Vec<String>| {
        let path = Path::new(source);
        if !path.is_absolute() {
            return None;
        }
        if let Some(relative) = source.strip_prefix(donor_prefix) {
            if !relative.starts_with('/') || relative.contains(donor_prefix) {
                return None;
            }
            if !target_sources.iter().any(|seen| seen == relative) {
                target_sources.push(relative.to_owned());
            }
            return Some(());
        }
        if source.contains(donor_prefix) {
            return None;
        }
        let candidate = package_ancestor(path, unit_ident)?;
        match package_directory {
            Some(existing) if *existing != candidate => None,
            Some(_) => Some(()),
            None => {
                *package_directory = Some(candidate);
                Some(())
            }
        }
    };
    let mut saw_output_rule = false;
    for line in contents.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            if line.contains(donor_prefix) {
                // The one admissible donor-prefixed record: rustc's note
                // that the unit read its own OUT_DIR.
                let value = line.strip_prefix("# env-dep:OUT_DIR=")?;
                let relative = value.strip_prefix(donor_prefix)?;
                if !relative.starts_with('/')
                    || relative.contains(donor_prefix)
                    || out_dir_env.replace(relative.to_owned()).is_some()
                {
                    return None;
                }
            }
            continue;
        }
        let colon = line
            .find(": ")
            .map(|index| index + 1)
            .or_else(|| line.ends_with(':').then_some(line.len().checked_sub(1)?))?;
        let target = &line[..colon];
        let prerequisites = line[colon + 1..].trim();
        if let Some(relative) = target.strip_prefix(donor_prefix) {
            if !relative.starts_with('/') {
                return None;
            }
            saw_output_rule = true;
        } else {
            observe_source(target, &mut package_directory, &mut target_sources)?;
        }
        for prerequisite in prerequisites.split(' ') {
            if prerequisite.is_empty() {
                continue;
            }
            observe_source(prerequisite, &mut package_directory, &mut target_sources)?;
        }
    }
    if !saw_output_rule {
        return None;
    }
    Some(DependencyFileFacts {
        package_directory: package_directory?,
        target_sources,
        out_dir_env,
    })
}

/// Finds the ancestor directory whose name is `<unit>-<version>` for this
/// unit, the registry package directory every source must live in. The path
/// is normalized lexically first: registry sources routinely contain `..`
/// segments (`src/../README.md` from `include_str!`), and a path that truly
/// escapes the package directory must attribute to where it lands, not to a
/// directory it merely passes through.
fn package_ancestor(path: &Path, unit_ident: &str) -> Option<PathBuf> {
    let path = lexically_normalized(path)?;
    for ancestor in path.ancestors().skip(1) {
        let name = ancestor.file_name()?.to_str()?;
        if let Some((package, _version)) = split_package_directory_name(name) {
            if normalized(package) == normalized(unit_ident) {
                return Some(ancestor.to_owned());
            }
        }
    }
    None
}

/// Resolves `.` and `..` components without touching the filesystem. A `..`
/// that would climb past the root makes the path unattributable.
fn lexically_normalized(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let mut resolved = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                resolved.pop();
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => {
                resolved.push(component.as_os_str());
            }
            Component::Normal(name) => {
                resolved.push(name);
                depth += 1;
            }
        }
    }
    Some(resolved)
}

// ---------------------------------------------------------------------------
// Record
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnitKind {
    Library,
    BuildScriptCompile,
    BuildScriptRun,
}

impl UnitKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::BuildScriptCompile => "build-script-compile",
            Self::BuildScriptRun => "build-script-run",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "library" => Some(Self::Library),
            "build-script-compile" => Some(Self::BuildScriptCompile),
            "build-script-run" => Some(Self::BuildScriptRun),
            _ => None,
        }
    }
}

struct EntryFile {
    relative: String,
    source: PathBuf,
    executable: bool,
    rewrite: bool,
}

impl EntryFile {
    fn plain(relative: String, source: PathBuf) -> Self {
        Self {
            relative,
            source,
            executable: false,
            rewrite: false,
        }
    }
}

struct QualifiedUnit {
    kind: UnitKind,
    fingerprint_directory_name: String,
    package_name: String,
    package_version: String,
    fingerprint_hash: String,
    unit_hash: String,
    source_root: PathBuf,
    package_directory: PathBuf,
    files: Vec<EntryFile>,
    directories: Vec<String>,
    group: Vec<String>,
    excluded: Option<String>,
}

impl QualifiedUnit {
    fn entry_key(&self) -> String {
        format!("{}-{}", self.unit_hash, self.fingerprint_hash)
    }
}

pub(crate) fn record_units(
    profile_directory: &Path,
    rustc_digest: &str,
    workspace_root: &Path,
) -> Result<usize, String> {
    let Some(store) = store_root() else {
        return Ok(0);
    };
    let profile_name = match profile_directory.file_name().and_then(OsStr::to_str) {
        Some(name @ ("debug" | "release")) => name,
        _ => return Ok(0),
    };
    if rustc_digest.len() != 64 || !rustc_digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("unit cache recording requires a compiler identity digest".to_owned());
    }
    let fingerprint_root = profile_directory.join(".fingerprint");
    let deps_root = profile_directory.join("deps");
    if !fingerprint_root.is_dir() || !deps_root.is_dir() {
        return Ok(0);
    }
    let units = qualified_units(
        &store,
        profile_directory,
        profile_name,
        &fingerprint_root,
        &deps_root,
        workspace_root,
    )?;
    let mut recorded = 0usize;
    for unit in &units {
        match store_unit(&store, unit, rustc_digest, profile_name, profile_directory) {
            Ok(true) => recorded += 1,
            Ok(false) => {}
            Err(error) => trace(&format!(
                "unit cache could not store {}: {error}",
                unit.fingerprint_directory_name
            )),
        }
    }
    if let Err(error) = enforce_store_bound(&store) {
        trace(&format!("unit cache bound enforcement failed: {error}"));
    }
    trace(&format!(
        "unit cache recorded {recorded} of {} qualified units",
        units.len()
    ));
    Ok(recorded)
}

fn qualified_units(
    store: &Path,
    profile_directory: &Path,
    profile_name: &str,
    fingerprint_root: &Path,
    deps_root: &Path,
    workspace_root: &Path,
) -> Result<Vec<QualifiedUnit>, String> {
    let donor_prefix = profile_directory
        .to_str()
        .ok_or_else(|| "target directory path is not UTF-8".to_owned())?;
    // Deps-directory entries grouped by the 16-hex unit hash embedded in
    // their file names; unknown shapes taint the hash they carry.
    let mut deps_by_hash: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in fs::read_dir(deps_root)
        .map_err(|error| format!("could not inspect the dependency directory: {error}"))?
    {
        let entry = entry
            .map_err(|error| format!("could not inspect the dependency directory: {error}"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some(hash) = unit_hash_in_file_name(&name) {
            deps_by_hash.entry(hash).or_default().push(name);
        }
    }
    // Fingerprint directories indexed for dependency-edge resolution.
    let mut directories = Vec::new();
    let mut hash_to_directory: BTreeMap<String, usize> = BTreeMap::new();
    for entry in fs::read_dir(fingerprint_root)
        .map_err(|error| format!("could not inspect the fingerprint directory: {error}"))?
    {
        let entry = entry
            .map_err(|error| format!("could not inspect the fingerprint directory: {error}"))?;
        if directories.len() >= MAX_FINGERPRINT_DIRS {
            return Ok(Vec::new());
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let index = directories.len();
        if let Some(total_hash) = fingerprint_total_hash(&path) {
            // First writer wins; a duplicate total hash across directories
            // leaves the duplicate unresolvable, which fails closed below.
            hash_to_directory.entry(total_hash).or_insert(index);
        }
        directories.push((name, path));
    }
    let context = QualifyContext {
        store,
        profile_directory,
        profile_name,
        donor_prefix,
        workspace_root,
        directories: &directories,
        hash_to_directory: &hash_to_directory,
        deps_by_hash: &deps_by_hash,
    };
    let mut decisions: BTreeMap<usize, Option<QualifiedUnit>> = BTreeMap::new();
    let mut in_progress = BTreeSet::new();
    for index in 0..directories.len() {
        qualify_directory(index, &context, &mut decisions, &mut in_progress);
    }
    Ok(decisions.into_values().flatten().collect())
}

struct QualifyContext<'a> {
    store: &'a Path,
    profile_directory: &'a Path,
    profile_name: &'a str,
    donor_prefix: &'a str,
    workspace_root: &'a Path,
    directories: &'a [(String, PathBuf)],
    hash_to_directory: &'a BTreeMap<String, usize>,
    deps_by_hash: &'a BTreeMap<String, Vec<String>>,
}

/// Qualifies one fingerprint directory, memoizing the decision. Returns true
/// only when the unit is usable by a dependent: an excluded build-script
/// group member resolves (so its marker can be recorded) but is not usable.
fn qualify_directory(
    index: usize,
    context: &QualifyContext<'_>,
    decisions: &mut BTreeMap<usize, Option<QualifiedUnit>>,
    in_progress: &mut BTreeSet<usize>,
) -> bool {
    if let Some(decision) = decisions.get(&index) {
        return decision
            .as_ref()
            .is_some_and(|unit| unit.excluded.is_none());
    }
    if !in_progress.insert(index) {
        return false;
    }
    let unit = qualify_directory_inner(index, context, decisions, in_progress);
    in_progress.remove(&index);
    let usable = unit.as_ref().is_some_and(|unit| unit.excluded.is_none());
    decisions.insert(index, unit);
    usable
}

fn qualify_directory_inner(
    index: usize,
    context: &QualifyContext<'_>,
    decisions: &mut BTreeMap<usize, Option<QualifiedUnit>>,
    in_progress: &mut BTreeSet<usize>,
) -> Option<QualifiedUnit> {
    let (directory_name, directory_path) = &context.directories[index];
    split_unit_directory_name(directory_name)?;
    let mut names = Vec::new();
    for entry in fs::read_dir(directory_path).ok()? {
        let entry = entry.ok()?;
        if !entry.path().is_file() {
            return None;
        }
        names.push(entry.file_name().to_str()?.to_owned());
    }
    names.sort();
    if names
        .iter()
        .any(|name| name == "run-build-script-build-script-build")
    {
        qualify_build_script_run(index, &names, context, decisions, in_progress)
    } else if names
        .iter()
        .any(|name| name == "build-script-build-script-build")
    {
        qualify_build_script_compile(index, &names, context, decisions, in_progress)
    } else {
        qualify_library(index, &names, context, decisions, in_progress)
    }
}

fn qualify_library(
    index: usize,
    names: &[String],
    context: &QualifyContext<'_>,
    decisions: &mut BTreeMap<usize, Option<QualifiedUnit>>,
    in_progress: &mut BTreeSet<usize>,
) -> Option<QualifiedUnit> {
    let (directory_name, directory_path) = &context.directories[index];
    let (unit_name, unit_hash) = split_unit_directory_name(directory_name)?;
    // The fingerprint directory must contain exactly a library unit's file
    // set: the hash file, its JSON rendering, the encoded dep-info, and
    // optionally Cargo's invocation timestamp.
    let mut hash_file: Option<String> = None;
    let mut fingerprint_files = Vec::new();
    for name in names {
        if name == "invoked.timestamp" {
            fingerprint_files.push((name.clone(), directory_path.join(name)));
            continue;
        }
        if let Some(target) = name.strip_prefix("lib-") {
            if name.ends_with(".json") {
                fingerprint_files.push((name.clone(), directory_path.join(name)));
                continue;
            }
            if target.is_empty() {
                return None;
            }
            if hash_file.replace(name.clone()).is_some() {
                return None;
            }
            fingerprint_files.push((name.clone(), directory_path.join(name)));
            continue;
        }
        if name.starts_with("dep-lib-") {
            fingerprint_files.push((name.clone(), directory_path.join(name)));
            continue;
        }
        return None;
    }
    let hash_file_name = hash_file?;
    let target_name = hash_file_name.strip_prefix("lib-")?;
    if normalized(target_name) != normalized(unit_name) {
        return None;
    }
    let json_name = format!("{hash_file_name}.json");
    let dep_info_name = format!("dep-{hash_file_name}");
    let json_path = directory_path.join(&json_name);
    let dep_info_path = directory_path.join(&dep_info_name);
    if !fingerprint_files.iter().any(|(name, _)| *name == json_name)
        || !fingerprint_files
            .iter()
            .any(|(name, _)| *name == dep_info_name)
    {
        return None;
    }
    let total_hash = fingerprint_total_hash(directory_path)?;
    let json_bytes = read_bounded(&json_path, MAX_META_BYTES)?;
    let facts = parse_fingerprint_json(&json_bytes)?;
    check_dep_info_local(
        &facts.local,
        context.profile_name,
        directory_name,
        &hash_file_name,
    )?;
    let dep_info_bytes = read_bounded(&dep_info_path, MAX_DEP_INFO_BYTES)?;
    let tracked_files = parse_encoded_dep_info(&dep_info_bytes)?;
    // Classify this unit's dependency-directory files by exact name shape.
    let deps_files = context.deps_by_hash.get(unit_hash)?;
    if deps_files.len() > MAX_UNIT_FILES {
        return None;
    }
    let mut unit_ident: Option<String> = None;
    let assign_ident = |candidate: &str, unit_ident: &mut Option<String>| match unit_ident {
        Some(existing) => existing == candidate,
        None => {
            *unit_ident = Some(candidate.to_owned());
            true
        }
    };
    let mut rlib = false;
    let mut rmeta = false;
    let mut dependency_file_name: Option<String> = None;
    let mut objects = Vec::new();
    for name in deps_files {
        if let Some(stem) = name
            .strip_prefix("lib")
            .and_then(|name| name.strip_suffix(&format!("-{unit_hash}.rlib")))
        {
            if !assign_ident(stem, &mut unit_ident) {
                return None;
            }
            rlib = true;
            continue;
        }
        if let Some(stem) = name
            .strip_prefix("lib")
            .and_then(|name| name.strip_suffix(&format!("-{unit_hash}.rmeta")))
        {
            if !assign_ident(stem, &mut unit_ident) {
                return None;
            }
            rmeta = true;
            continue;
        }
        if let Some(stem) = name.strip_suffix(&format!("-{unit_hash}.d")) {
            if !assign_ident(stem, &mut unit_ident)
                || dependency_file_name.replace(name.clone()).is_some()
            {
                return None;
            }
            continue;
        }
        if name.ends_with(".rcgu.o") {
            if let Some((stem, _)) = name.split_once(&format!("-{unit_hash}.")) {
                if !assign_ident(stem, &mut unit_ident) {
                    return None;
                }
                objects.push(name.clone());
                continue;
            }
        }
        // Any other artifact carrying this unit hash — a dylib, staticlib,
        // executable, or unknown layout — disqualifies the unit.
        return None;
    }
    let unit_ident = unit_ident?;
    if normalized(&unit_ident) != normalized(unit_name) || !rmeta || dependency_file_name.is_none()
    {
        return None;
    }
    let dependency_file_name = dependency_file_name?;
    let dependency_path = context
        .profile_directory
        .join("deps")
        .join(&dependency_file_name);
    let dependency_contents = read_bounded(&dependency_path, MAX_DEPENDENCY_FILE_BYTES)?;
    let dependency_contents = std::str::from_utf8(&dependency_contents).ok()?;
    let dependency_facts =
        parse_dependency_file(dependency_contents, context.donor_prefix, &unit_ident)?;
    let package_directory = dependency_facts.package_directory.clone();
    if package_directory.starts_with(context.workspace_root)
        || context.workspace_root.starts_with(&package_directory)
    {
        return None;
    }
    let package_basename = package_directory.file_name()?.to_str()?;
    let (package_name, package_version) = split_package_directory_name(package_basename)?;
    if normalized(package_name) != normalized(unit_name) {
        return None;
    }
    let source_root = package_directory.parent()?.to_owned();
    // Dependency closure: every edge must resolve to another usable unit. A
    // `build_script_build` edge is admissible exactly when it resolves to
    // this package's qualified build-script run unit, whose own edge
    // resolves to the qualified compile unit; the three entries then restore
    // as one atomic group.
    let mut group = Vec::new();
    for (edge_name, edge_hash) in &facts.dependency_edges {
        let rendered = fingerprint_hash_text(*edge_hash);
        let &dependency_index = context.hash_to_directory.get(&rendered)?;
        if !qualify_directory(dependency_index, context, decisions, in_progress) {
            return None;
        }
        let dependency = decisions.get(&dependency_index)?.as_ref()?;
        if edge_name == "build_script_build" {
            if dependency.kind != UnitKind::BuildScriptRun
                || dependency.package_directory != package_directory
                || !group.is_empty()
            {
                return None;
            }
            group.push(dependency.entry_key());
            group.extend(dependency.group.iter().cloned());
        } else if dependency.kind != UnitKind::Library {
            return None;
        }
    }
    // A library that reads its build-script output directory — a tracked
    // generated source, a target-directory prerequisite, or an OUT_DIR env
    // read — compiles the generated file's absolute path into its own debug
    // information, so its rlib is not project-independent and never caches.
    // The package's build-script group still does: the run and compile
    // entries restore on their own and only the cheap library compile
    // remains. A groupless library must carry no tracked state at all.
    if !tracked_files.is_empty()
        || !dependency_facts.target_sources.is_empty()
        || dependency_facts.out_dir_env.is_some()
    {
        return None;
    }
    let deps_root = context.profile_directory.join("deps");
    let mut files = Vec::new();
    for (name, path) in fingerprint_files {
        files.push(EntryFile::plain(format!("fingerprint/{name}"), path));
    }
    files.push(EntryFile {
        relative: format!("deps/{dependency_file_name}"),
        source: dependency_path,
        executable: false,
        rewrite: true,
    });
    if rlib {
        let name = format!("lib{unit_ident}-{unit_hash}.rlib");
        files.push(EntryFile::plain(
            format!("deps/{name}"),
            deps_root.join(name),
        ));
    }
    let rmeta_name = format!("lib{unit_ident}-{unit_hash}.rmeta");
    files.push(EntryFile::plain(
        format!("deps/{rmeta_name}"),
        deps_root.join(rmeta_name),
    ));
    for object in objects {
        files.push(EntryFile::plain(
            format!("deps/{object}"),
            deps_root.join(object),
        ));
    }
    Some(QualifiedUnit {
        kind: UnitKind::Library,
        fingerprint_directory_name: directory_name.clone(),
        package_name: package_name.to_owned(),
        package_version: package_version.to_owned(),
        fingerprint_hash: total_hash,
        unit_hash: unit_hash.to_owned(),
        source_root,
        package_directory,
        files,
        directories: Vec::new(),
        group,
        excluded: None,
    })
}

/// Qualifies a build-script compile unit: the same immutable class as a
/// library unit, with its executable outputs under `build/<dir>/`.
fn qualify_build_script_compile(
    index: usize,
    names: &[String],
    context: &QualifyContext<'_>,
    decisions: &mut BTreeMap<usize, Option<QualifiedUnit>>,
    in_progress: &mut BTreeSet<usize>,
) -> Option<QualifiedUnit> {
    let (directory_name, directory_path) = &context.directories[index];
    let (unit_name, unit_hash) = split_unit_directory_name(directory_name)?;
    let hash_file_name = "build-script-build-script-build";
    let json_name = format!("{hash_file_name}.json");
    let dep_info_name = format!("dep-{hash_file_name}");
    for name in names {
        if name != hash_file_name
            && *name != json_name
            && *name != dep_info_name
            && name != "invoked.timestamp"
        {
            return None;
        }
    }
    if !names.contains(&json_name) || !names.contains(&dep_info_name) {
        return None;
    }
    let total_hash = fingerprint_total_hash(directory_path)?;
    let json_bytes = read_bounded(&directory_path.join(&json_name), MAX_META_BYTES)?;
    let facts = parse_fingerprint_json(&json_bytes)?;
    check_dep_info_local(
        &facts.local,
        context.profile_name,
        directory_name,
        hash_file_name,
    )?;
    let dep_info_bytes = read_bounded(&directory_path.join(&dep_info_name), MAX_DEP_INFO_BYTES)?;
    if !encoded_dep_info_tracks_no_files(&dep_info_bytes) {
        return None;
    }
    // No dependency-directory artifact may carry this unit's hash.
    if context.deps_by_hash.contains_key(unit_hash) {
        return None;
    }
    // Build-dependency edges must resolve to usable library units.
    for (edge_name, edge_hash) in &facts.dependency_edges {
        if edge_name == "build_script_build" {
            return None;
        }
        let rendered = fingerprint_hash_text(*edge_hash);
        let &dependency_index = context.hash_to_directory.get(&rendered)?;
        if !qualify_directory(dependency_index, context, decisions, in_progress) {
            return None;
        }
        if decisions.get(&dependency_index)?.as_ref()?.kind != UnitKind::Library {
            return None;
        }
    }
    // The executable directory must hold exactly the compiled script pair
    // and its dep-info file.
    let build_directory = context
        .profile_directory
        .join("build")
        .join(directory_name.as_str());
    let executable_name = format!("build_script_build-{unit_hash}");
    let dependency_name = format!("{executable_name}.d");
    let mut files = Vec::new();
    for entry in fs::read_dir(&build_directory).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_str()?.to_owned();
        let path = entry.path();
        let metadata = entry.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        let executable = metadata.permissions().mode() & 0o111 != 0;
        if name == executable_name || name == "build-script-build" {
            if !executable {
                return None;
            }
            files.push(EntryFile {
                relative: format!("build/{name}"),
                source: path,
                executable: true,
                rewrite: false,
            });
        } else if name == dependency_name {
            files.push(EntryFile {
                relative: format!("build/{name}"),
                source: path,
                executable: false,
                rewrite: true,
            });
        } else {
            return None;
        }
    }
    if files.len() != 3 {
        return None;
    }
    let dependency_contents = read_bounded(
        &build_directory.join(&dependency_name),
        MAX_DEPENDENCY_FILE_BYTES,
    )?;
    let dependency_contents = std::str::from_utf8(&dependency_contents).ok()?;
    let dependency_facts =
        parse_dependency_file(dependency_contents, context.donor_prefix, unit_name)?;
    if !dependency_facts.target_sources.is_empty() || dependency_facts.out_dir_env.is_some() {
        return None;
    }
    let package_directory = dependency_facts.package_directory;
    if package_directory.starts_with(context.workspace_root)
        || context.workspace_root.starts_with(&package_directory)
    {
        return None;
    }
    let package_basename = package_directory.file_name()?.to_str()?;
    let (package_name, package_version) = split_package_directory_name(package_basename)?;
    if normalized(package_name) != normalized(unit_name) {
        return None;
    }
    let source_root = package_directory.parent()?.to_owned();
    for name in [hash_file_name, &json_name, &dep_info_name] {
        files.push(EntryFile::plain(
            format!("fingerprint/{name}"),
            directory_path.join(name),
        ));
    }
    if names.iter().any(|name| name == "invoked.timestamp") {
        files.push(EntryFile::plain(
            "fingerprint/invoked.timestamp".to_owned(),
            directory_path.join("invoked.timestamp"),
        ));
    }
    Some(QualifiedUnit {
        kind: UnitKind::BuildScriptCompile,
        fingerprint_directory_name: directory_name.clone(),
        package_name: package_name.to_owned(),
        package_version: package_version.to_owned(),
        fingerprint_hash: total_hash,
        unit_hash: unit_hash.to_owned(),
        source_root,
        package_directory,
        files,
        directories: Vec::new(),
        group: Vec::new(),
        excluded: None,
    })
}

/// Qualifies a build-script run unit and its output directory. A package
/// whose outputs are not provably relocatable resolves as excluded, which
/// records a persistent marker and keeps the whole group out of the cache.
fn qualify_build_script_run(
    index: usize,
    names: &[String],
    context: &QualifyContext<'_>,
    decisions: &mut BTreeMap<usize, Option<QualifiedUnit>>,
    in_progress: &mut BTreeSet<usize>,
) -> Option<QualifiedUnit> {
    let (directory_name, directory_path) = &context.directories[index];
    let (unit_name, unit_hash) = split_unit_directory_name(directory_name)?;
    let hash_file_name = "run-build-script-build-script-build";
    let json_name = format!("{hash_file_name}.json");
    if names.len() != 2 || names[0] != *hash_file_name || names[1] != json_name {
        return None;
    }
    let total_hash = fingerprint_total_hash(directory_path)?;
    let json_bytes = read_bounded(&directory_path.join(&json_name), MAX_META_BYTES)?;
    let facts = parse_fingerprint_json(&json_bytes)?;
    if context.deps_by_hash.contains_key(unit_hash) {
        return None;
    }
    // Exactly one edge: the compiled build script of the same package.
    let [(edge_name, edge_hash)] = facts.dependency_edges.as_slice() else {
        return None;
    };
    if edge_name != "build_script_build" {
        return None;
    }
    let rendered = fingerprint_hash_text(*edge_hash);
    let &dependency_index = context.hash_to_directory.get(&rendered)?;
    if !qualify_directory(dependency_index, context, decisions, in_progress) {
        return None;
    }
    let compile = decisions.get(&dependency_index)?.as_ref()?;
    if compile.kind != UnitKind::BuildScriptCompile
        || normalized(&compile.package_name) != normalized(unit_name)
    {
        return None;
    }
    let package_directory = compile.package_directory.clone();
    let package_name = compile.package_name.clone();
    let package_version = compile.package_version.clone();
    let source_root = compile.source_root.clone();
    let compile_key = compile.entry_key();
    if !run_unit_local_is_supported(
        &facts.local,
        context.profile_name,
        directory_name,
        &package_directory,
        &package_version,
    ) {
        return None;
    }
    let mut unit = QualifiedUnit {
        kind: UnitKind::BuildScriptRun,
        fingerprint_directory_name: directory_name.clone(),
        package_name,
        package_version,
        fingerprint_hash: total_hash,
        unit_hash: unit_hash.to_owned(),
        source_root,
        package_directory,
        files: vec![
            EntryFile::plain(
                format!("fingerprint/{hash_file_name}"),
                directory_path.join(hash_file_name),
            ),
            EntryFile::plain(
                format!("fingerprint/{json_name}"),
                directory_path.join(&json_name),
            ),
        ],
        directories: Vec::new(),
        group: vec![compile_key],
        excluded: None,
    };
    // A previously stored or excluded entry skips the output scan; the
    // stored metadata already carries the flags, and an exclusion is
    // permanent.
    let entry = entry_directory(context.store, &unit);
    if entry.join("excluded").is_file() {
        unit.excluded = Some("recorded".to_owned());
        return Some(unit);
    }
    let scanned = scan_run_output_directory(
        &context
            .profile_directory
            .join("build")
            .join(directory_name.as_str()),
        context.donor_prefix,
        context.workspace_root,
        entry.is_dir(),
    )?;
    match scanned {
        RunOutputScan::Excluded(reason) => unit.excluded = Some(reason),
        RunOutputScan::Relocatable { files, directories } => {
            unit.files.extend(files);
            unit.directories = directories;
        }
    }
    Some(unit)
}

enum RunOutputScan {
    Relocatable {
        files: Vec<EntryFile>,
        directories: Vec<String>,
    },
    Excluded(String),
}

/// Walks a build-script run directory, classifying every file for
/// relocatability. With `skip_content` (an already-stored entry) only the
/// name layout is validated; digests and flags then come from the stored
/// metadata.
fn scan_run_output_directory(
    build_directory: &Path,
    donor_prefix: &str,
    workspace_root: &Path,
    skip_content: bool,
) -> Option<RunOutputScan> {
    let workspace_prefix = workspace_root.to_str()?;
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut saw_output = false;
    let mut saw_root_output = false;
    let mut total_bytes = 0u64;
    let mut pending: Vec<(PathBuf, String)> = Vec::new();
    for entry in fs::read_dir(build_directory).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_str()?.to_owned();
        let path = entry.path();
        let metadata = entry.path().symlink_metadata().ok()?;
        match name.as_str() {
            "output" | "root-output" | "stderr" | "invoked.timestamp" => {
                if !metadata.is_file() {
                    return Some(RunOutputScan::Excluded(format!(
                        "{name} is not a regular file"
                    )));
                }
                saw_output |= name == "output";
                saw_root_output |= name == "root-output";
                pending.push((path, format!("build/{name}")));
            }
            "out" => {
                if !metadata.is_dir() {
                    return Some(RunOutputScan::Excluded("out is not a directory".to_owned()));
                }
                directories.push("build/out".to_owned());
                let mut stack = vec![(path, "build/out".to_owned())];
                while let Some((current, relative)) = stack.pop() {
                    for child in fs::read_dir(&current).ok()? {
                        let child = child.ok()?;
                        let child_name = child.file_name().to_str()?.to_owned();
                        if child_name.is_empty() || child_name == "." || child_name == ".." {
                            return None;
                        }
                        let child_relative = format!("{relative}/{child_name}");
                        if child_relative.matches('/').count() >= MAX_OUT_COMPONENTS {
                            return Some(RunOutputScan::Excluded(
                                "output tree is too deep".to_owned(),
                            ));
                        }
                        let child_metadata = child.path().symlink_metadata().ok()?;
                        if child_metadata.is_dir() {
                            directories.push(child_relative.clone());
                            stack.push((child.path(), child_relative));
                        } else if child_metadata.is_file() {
                            if pending.len() >= MAX_OUT_FILES {
                                return Some(RunOutputScan::Excluded(
                                    "output tree has too many files".to_owned(),
                                ));
                            }
                            total_bytes = total_bytes.saturating_add(child_metadata.len());
                            if total_bytes > MAX_OUT_BYTES {
                                return Some(RunOutputScan::Excluded(
                                    "output tree is too large".to_owned(),
                                ));
                            }
                            pending.push((child.path(), child_relative));
                        } else {
                            return Some(RunOutputScan::Excluded(format!(
                                "{child_relative} is not a regular file"
                            )));
                        }
                    }
                }
            }
            _ => {
                return Some(RunOutputScan::Excluded(format!(
                    "unexpected build output {name}"
                )));
            }
        }
    }
    if !saw_output || !saw_root_output {
        return None;
    }
    for (path, relative) in pending {
        if skip_content {
            files.push(EntryFile::plain(relative, path));
            continue;
        }
        let contents = read_bounded(&path, MAX_OUT_BYTES)?;
        match classify_relocatability(&contents, donor_prefix, workspace_prefix) {
            Relocatability::Verbatim => files.push(EntryFile::plain(relative, path)),
            Relocatability::Rewrite => files.push(EntryFile {
                relative,
                source: path,
                executable: false,
                rewrite: true,
            }),
            Relocatability::Excluded(reason) => {
                return Some(RunOutputScan::Excluded(format!("{relative}: {reason}")));
            }
        }
    }
    directories.sort();
    Some(RunOutputScan::Relocatable { files, directories })
}

enum Relocatability {
    Verbatim,
    Rewrite,
    Excluded(String),
}

/// Classifies one build output file. Donor-prefix occurrences are admissible
/// only in UTF-8 text (a reversible substitution then reproduces what a
/// native run would have written); a workspace-prefix occurrence outside a
/// donor occurrence, or a donor occurrence inside a binary file, can never
/// be restored faithfully.
fn classify_relocatability(
    contents: &[u8],
    donor_prefix: &str,
    workspace_prefix: &str,
) -> Relocatability {
    let donor_ranges = occurrence_ranges(contents, donor_prefix.as_bytes());
    for workspace_start in occurrence_starts(contents, workspace_prefix.as_bytes()) {
        let inside_donor = donor_ranges
            .iter()
            .any(|(start, end)| workspace_start >= *start && workspace_start < *end);
        if !inside_donor {
            return Relocatability::Excluded("mentions the donor workspace".to_owned());
        }
    }
    if donor_ranges.is_empty() {
        return Relocatability::Verbatim;
    }
    if std::str::from_utf8(contents).is_err() {
        return Relocatability::Excluded("donor path inside a binary file".to_owned());
    }
    Relocatability::Rewrite
}

fn occurrence_starts(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    let mut starts = Vec::new();
    for start in 0..=haystack.len() - needle.len() {
        if &haystack[start..start + needle.len()] == needle {
            starts.push(start);
        }
    }
    starts
}

fn occurrence_ranges(haystack: &[u8], needle: &[u8]) -> Vec<(usize, usize)> {
    occurrence_starts(haystack, needle)
        .into_iter()
        .map(|start| (start, start + needle.len()))
        .collect()
}

fn split_unit_directory_name(name: &str) -> Option<(&str, &str)> {
    let (unit_name, hash) = name.rsplit_once('-')?;
    (is_hex16(hash) && !unit_name.is_empty()).then_some((unit_name, hash))
}

fn unit_hash_in_file_name(name: &str) -> Option<String> {
    // The unit hash is the 16-hex run immediately after the last `-` that
    // precedes either the extension or a `.`-separated tail.
    let dash = name.rfind('-')?;
    let tail = &name[dash + 1..];
    let hash_end = tail.find('.').unwrap_or(tail.len());
    let hash = &tail[..hash_end];
    is_hex16(hash).then(|| hash.to_owned())
}

fn fingerprint_total_hash(directory: &Path) -> Option<String> {
    let mut hash = None;
    for entry in fs::read_dir(directory).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_str()?;
        if name.contains('.') || name.starts_with("dep-") {
            continue;
        }
        let contents = read_bounded(&entry.path(), 64)?;
        let contents = std::str::from_utf8(&contents).ok()?;
        if !is_hex16(contents) {
            return None;
        }
        if hash.replace(contents.to_owned()).is_some() {
            return None;
        }
    }
    hash
}

fn read_bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    fs::read(path).ok()
}

// ---------------------------------------------------------------------------
// Store entries
// ---------------------------------------------------------------------------

struct MetaFileEntry {
    relative: String,
    size: u64,
    digest: String,
    executable: bool,
    rewrite: bool,
}

struct MetaFile {
    kind: UnitKind,
    package_name: String,
    package_version: String,
    fingerprint_directory_name: String,
    rustc_digest: String,
    profile_name: String,
    source_root: PathBuf,
    donor_prefix: String,
    group: Vec<String>,
    directories: Vec<String>,
    files: Vec<MetaFileEntry>,
}

fn entry_directory(store: &Path, unit: &QualifiedUnit) -> PathBuf {
    store
        .join("index")
        .join(format!("{}-{}", unit.package_name, unit.package_version))
        .join(format!("{}-{}", unit.unit_hash, unit.fingerprint_hash))
}

fn store_unit(
    store: &Path,
    unit: &QualifiedUnit,
    rustc_digest: &str,
    profile_name: &str,
    profile_directory: &Path,
) -> Result<bool, String> {
    let entry = entry_directory(store, unit);
    if entry.join("unstable").is_file() || entry.join("excluded").is_file() {
        return Ok(false);
    }
    let donor_prefix = profile_directory
        .to_str()
        .ok_or_else(|| "target directory path is not UTF-8".to_owned())?;
    if let Some(reason) = &unit.excluded {
        if entry.is_dir() {
            // A previously restorable entry that stopped qualifying is a
            // divergence: retire it before recording the exclusion.
            mark_unstable(&entry)?;
            return Ok(false);
        }
        create_private_directories(&entry)?;
        write_private_file(&entry.join("excluded"), reason.as_bytes())?;
        trace(&format!(
            "unit cache excluded {}: {reason}",
            unit.fingerprint_directory_name
        ));
        return Ok(false);
    }
    // Hash the live unit files once; the digests both populate the metadata
    // and detect divergence from an existing entry.
    let mut files: Vec<(String, u64, String, PathBuf)> = Vec::new();
    let mut total_bytes = 0u64;
    for file in &unit.files {
        files.push(described_file(&file.relative, &file.source)?);
    }
    for (_, size, _, _) in &files {
        total_bytes = total_bytes.saturating_add(*size);
    }
    let (file_cap, byte_cap) = match unit.kind {
        UnitKind::BuildScriptRun => (MAX_ENTRY_FILES, MAX_OUT_BYTES),
        _ => (MAX_UNIT_FILES, MAX_UNIT_BYTES),
    };
    if files.len() > file_cap || total_bytes > byte_cap {
        return Ok(false);
    }
    if entry.is_dir() {
        return match compare_existing_entry(
            &entry,
            &files,
            unit,
            rustc_digest,
            profile_name,
            donor_prefix,
        ) {
            ExistingEntry::Identical => {
                touch_entry(&entry);
                Ok(false)
            }
            ExistingEntry::ContextMismatch => Ok(false),
            ExistingEntry::Diverged => {
                mark_unstable(&entry)?;
                trace(&format!(
                    "unit cache marked {} unstable after divergent bytes",
                    unit.fingerprint_directory_name
                ));
                Ok(false)
            }
        };
    }
    let meta = MetaFile {
        kind: unit.kind,
        package_name: unit.package_name.clone(),
        package_version: unit.package_version.clone(),
        fingerprint_directory_name: unit.fingerprint_directory_name.clone(),
        rustc_digest: rustc_digest.to_owned(),
        profile_name: profile_name.to_owned(),
        source_root: unit.source_root.clone(),
        donor_prefix: donor_prefix.to_owned(),
        group: unit.group.clone(),
        directories: unit.directories.clone(),
        files: files
            .iter()
            .zip(&unit.files)
            .map(|((relative, size, digest, _), file)| MetaFileEntry {
                relative: relative.clone(),
                size: *size,
                digest: digest.clone(),
                executable: file.executable,
                rewrite: file.rewrite,
            })
            .collect(),
    };
    let staging = store.join(format!(
        "staging-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock predates the Unix epoch".to_owned())?
            .as_nanos()
    ));
    let result = (|| {
        create_private_directories(&staging)?;
        for (relative, _, _, source) in &files {
            let destination = staging.join("files").join(relative);
            let parent = destination
                .parent()
                .ok_or_else(|| "unit cache staging path has no parent".to_owned())?;
            create_private_directories(parent)?;
            super::patch::clone_file(source, &destination)?;
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("could not protect a unit cache file: {error}"))?;
        }
        write_meta(&staging.join("meta"), &meta)?;
        write_private_file(&staging.join("touched"), b"1")?;
        if let Some(parent) = entry.parent() {
            create_private_directories(parent)?;
        }
        match fs::rename(&staging, &entry) {
            Ok(()) => Ok(true),
            Err(_) if entry.exists() => Ok(false),
            Err(error) => Err(format!("could not publish a unit cache entry: {error}")),
        }
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

enum ExistingEntry {
    Identical,
    Diverged,
    ContextMismatch,
}

fn compare_existing_entry(
    entry: &Path,
    files: &[(String, u64, String, PathBuf)],
    unit: &QualifiedUnit,
    rustc_digest: &str,
    profile_name: &str,
    live_prefix: &str,
) -> ExistingEntry {
    let Some(meta) = read_meta(&entry.join("meta")) else {
        return ExistingEntry::Diverged;
    };
    if meta.rustc_digest != rustc_digest || meta.profile_name != profile_name {
        // The same fingerprint hash under a different recorded context would
        // be a Cargo hash collision; leave the stored entry alone.
        return ExistingEntry::ContextMismatch;
    }
    if meta.kind != unit.kind
        || meta.group != unit.group
        || (unit.kind == UnitKind::BuildScriptRun && meta.directories != unit.directories)
    {
        return ExistingEntry::Diverged;
    }
    let recorded: BTreeMap<&str, &MetaFileEntry> = meta
        .files
        .iter()
        .map(|file| (file.relative.as_str(), file))
        .collect();
    if recorded.len() != files.len() {
        return ExistingEntry::Diverged;
    }
    for (relative, size, digest, path) in files {
        let Some(record) = recorded.get(relative.as_str()) else {
            return ExistingEntry::Diverged;
        };
        // A rewrite-flagged file legitimately carries this project's target
        // prefix; compare it as the bytes the donor would have written.
        if record.rewrite {
            let Some(contents) = read_bounded(path, MAX_DEPENDENCY_FILE_BYTES.max(MAX_OUT_BYTES))
            else {
                trace(&format!("unit cache divergence at unreadable {relative}"));
                return ExistingEntry::Diverged;
            };
            let Some(as_donor) =
                rewrite_dependency_file(&contents, live_prefix, &meta.donor_prefix)
            else {
                trace(&format!("unit cache divergence rewriting {relative}"));
                return ExistingEntry::Diverged;
            };
            let mut hasher = Sha256::new();
            hasher.update(&as_donor);
            let rewritten_digest = hex(&hasher.finalize());
            if record.size != as_donor.len() as u64 || record.digest != rewritten_digest {
                trace(&format!("unit cache divergence at rewritten {relative}"));
                return ExistingEntry::Diverged;
            }
            continue;
        }
        if record.size != *size || record.digest != *digest {
            trace(&format!("unit cache divergence at {relative}"));
            return ExistingEntry::Diverged;
        }
    }
    ExistingEntry::Identical
}

fn described_file(relative: &str, path: &Path) -> Result<(String, u64, String, PathBuf), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let contents =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if contents.len() as u64 != metadata.len() {
        return Err(format!("{} changed while being recorded", path.display()));
    }
    let mut digest = Sha256::new();
    digest.update(&contents);
    Ok((
        relative.to_owned(),
        metadata.len(),
        hex(&digest.finalize()),
        path.to_owned(),
    ))
}

fn mark_unstable(entry: &Path) -> Result<(), String> {
    let parent = entry
        .parent()
        .ok_or_else(|| "unit cache entry has no parent".to_owned())?;
    let discard = parent.join(format!(
        "discard-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock predates the Unix epoch".to_owned())?
            .as_nanos()
    ));
    fs::rename(entry, &discard)
        .map_err(|error| format!("could not retire a divergent unit cache entry: {error}"))?;
    let _ = fs::remove_dir_all(&discard);
    create_private_directories(entry)?;
    write_private_file(&entry.join("unstable"), b"1")
}

fn touch_entry(entry: &Path) {
    let _ = write_private_file(&entry.join("touched"), b"1");
}

fn entry_recency(entry: &Path) -> SystemTime {
    for name in ["touched", "meta"] {
        if let Ok(metadata) = fs::metadata(entry.join(name)) {
            if let Ok(modified) = metadata.modified() {
                return modified;
            }
        }
    }
    UNIX_EPOCH
}

fn create_private_directories(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    let Some(root) = store_root() else {
        return Ok(());
    };
    let mut current = path;
    loop {
        super::make_private_directory(current)?;
        if current == root || !current.starts_with(&root) {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    Ok(())
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    file.write_all(contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not protect {}: {error}", path.display()))
}

fn write_meta(path: &Path, meta: &MetaFile) -> Result<(), String> {
    let mut rendered = String::new();
    rendered.push_str(META_MAGIC);
    rendered.push('\n');
    rendered.push_str(&format!("kind {}\n", meta.kind.as_str()));
    rendered.push_str(&format!("package {}\n", meta.package_name));
    rendered.push_str(&format!("version {}\n", meta.package_version));
    rendered.push_str(&format!(
        "fingerprint-directory {}\n",
        meta.fingerprint_directory_name
    ));
    rendered.push_str(&format!("rustc {}\n", meta.rustc_digest));
    rendered.push_str(&format!("profile {}\n", meta.profile_name));
    let source_root = meta
        .source_root
        .to_str()
        .ok_or_else(|| "unit cache source root is not UTF-8".to_owned())?;
    rendered.push_str(&format!("source-root {source_root}\n"));
    rendered.push_str(&format!("donor-target {}\n", meta.donor_prefix));
    for member in &meta.group {
        rendered.push_str(&format!("group {member}\n"));
    }
    for directory in &meta.directories {
        rendered.push_str(&format!("dir {directory}\n"));
    }
    for file in &meta.files {
        let flags = match (file.executable, file.rewrite) {
            (false, false) => "-",
            (true, false) => "x",
            (false, true) => "r",
            (true, true) => "xr",
        };
        rendered.push_str(&format!(
            "file {} {} {flags} {}\n",
            file.digest, file.size, file.relative
        ));
    }
    write_private_file(path, rendered.as_bytes())
}

fn read_meta(path: &Path) -> Option<MetaFile> {
    let contents = read_bounded(path, MAX_META_BYTES)?;
    let contents = std::str::from_utf8(&contents).ok()?;
    let mut lines = contents.lines();
    if lines.next()? != META_MAGIC {
        return None;
    }
    let mut kind = None;
    let mut package_name = None;
    let mut package_version = None;
    let mut fingerprint_directory_name = None;
    let mut rustc_digest = None;
    let mut profile_name = None;
    let mut source_root = None;
    let mut donor_prefix = None;
    let mut group = Vec::new();
    let mut directories = Vec::new();
    let mut files: Vec<MetaFileEntry> = Vec::new();
    for line in lines {
        let (key, value) = line.split_once(' ')?;
        match key {
            "kind" => kind = Some(UnitKind::from_str(value)?),
            "package" => package_name = Some(value.to_owned()),
            "version" => package_version = Some(value.to_owned()),
            "fingerprint-directory" => fingerprint_directory_name = Some(value.to_owned()),
            "rustc" => rustc_digest = Some(value.to_owned()),
            "profile" => profile_name = Some(value.to_owned()),
            "source-root" => source_root = Some(PathBuf::from(value)),
            "donor-target" => donor_prefix = Some(value.to_owned()),
            "group" => {
                if group.len() >= 8 || value.is_empty() || value.contains('/') {
                    return None;
                }
                group.push(value.to_owned());
            }
            "dir" => {
                if directories.len() >= MAX_OUT_FILES || !relative_path_is_safe(value) {
                    return None;
                }
                directories.push(value.to_owned());
            }
            "file" => {
                if files.len() >= MAX_ENTRY_FILES {
                    return None;
                }
                let (digest, rest) = value.split_once(' ')?;
                let (size, rest) = rest.split_once(' ')?;
                let (flags, relative) = rest.split_once(' ')?;
                if digest.len() != 64
                    || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || relative.is_empty()
                {
                    return None;
                }
                if !relative_path_is_safe(relative) {
                    return None;
                }
                let (executable, rewrite) = match flags {
                    "-" => (false, false),
                    "x" => (true, false),
                    "r" => (false, true),
                    "xr" => (true, true),
                    _ => return None,
                };
                files.push(MetaFileEntry {
                    relative: relative.to_owned(),
                    size: size.parse().ok()?,
                    digest: digest.to_owned(),
                    executable,
                    rewrite,
                });
            }
            _ => return None,
        }
    }
    if files.is_empty() {
        return None;
    }
    Some(MetaFile {
        kind: kind?,
        package_name: package_name?,
        package_version: package_version?,
        fingerprint_directory_name: fingerprint_directory_name?,
        rustc_digest: rustc_digest?,
        profile_name: profile_name?,
        source_root: source_root?,
        donor_prefix: donor_prefix?,
        group,
        directories,
        files,
    })
}

/// Store-relative file names live under a known prefix with clean
/// components; anything else is rejected before any path is joined. Nested
/// paths are admitted only for a build-script output tree.
fn relative_path_is_safe(relative: &str) -> bool {
    let Some((prefix, rest)) = relative.split_once('/') else {
        return false;
    };
    let components: Vec<&str> = rest.split('/').collect();
    if components.len() > MAX_OUT_COMPONENTS
        || components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return false;
    }
    match prefix {
        "deps" | "fingerprint" => components.len() == 1,
        "build" => components.len() == 1 || components[0] == "out",
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Store bound
// ---------------------------------------------------------------------------

fn enforce_store_bound(store: &Path) -> Result<(), String> {
    let bound = env::var_os(UNIT_CACHE_BYTES)
        .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
        .unwrap_or(DEFAULT_STORE_BYTES);
    let index = store.join("index");
    if !index.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for package in read_directory(&index)? {
        for entry in read_directory(&package)? {
            if entry.join("unstable").is_file() || entry.join("excluded").is_file() {
                continue;
            }
            let size = directory_bytes(&entry);
            total = total.saturating_add(size);
            entries.push((entry_recency(&entry), size, entry));
        }
    }
    if total <= bound {
        return Ok(());
    }
    entries.sort_by_key(|(recency, _, _)| *recency);
    for (_, size, entry) in entries {
        if total <= bound {
            break;
        }
        if fs::remove_dir_all(&entry).is_ok() {
            total = total.saturating_sub(size);
        }
    }
    Ok(())
}

fn read_directory(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut children = Vec::new();
    for entry in fs::read_dir(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
    {
        let entry =
            entry.map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        if entry.path().is_dir() {
            children.push(entry.path());
        }
    }
    Ok(children)
}

fn directory_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut pending = vec![path.to_owned()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

pub(crate) fn restore_units(
    location: &CacheLocation,
    rustc_digest: &str,
) -> Result<RestoreCounts, String> {
    let mut counts = RestoreCounts {
        candidates: 0,
        restored: 0,
        skipped_existing: 0,
        skipped_conflict: 0,
        skipped_group: 0,
        digest_failures: 0,
    };
    let Some(store) = store_root() else {
        return Ok(counts);
    };
    let index = store.join("index");
    if !index.is_dir() {
        return Ok(counts);
    }
    let Some(packages) = lock_packages(&location.lock_file) else {
        return Ok(counts);
    };
    let destination_profile = &location.profile_directory;
    let Some(destination_prefix) = destination_profile.to_str() else {
        return Ok(counts);
    };
    // Gather surviving candidates first so cross-entry destination conflicts
    // can drop every claimant of a contested file name. A candidate is a
    // library entry together with its build-script group members, restored
    // all-or-nothing.
    struct Candidate {
        entries: Vec<(PathBuf, MetaFile)>,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    for (name, version) in &packages {
        let package_directory = index.join(format!("{name}-{version}"));
        let Ok(entries) = fs::read_dir(&package_directory) else {
            continue;
        };
        let mut roots: Vec<(PathBuf, MetaFile)> = Vec::new();
        for entry in entries.flatten() {
            let entry = entry.path();
            if !entry.is_dir()
                || entry.join("unstable").is_file()
                || entry.join("excluded").is_file()
            {
                continue;
            }
            let Some(meta) = read_meta(&entry.join("meta")) else {
                continue;
            };
            if meta.rustc_digest != rustc_digest
                || meta.profile_name != location.profile_name
                || meta.package_name != *name
                || meta.package_version != *version
                || !meta.source_root.is_dir()
            {
                continue;
            }
            // Library entries restore with their whole build-script group. A
            // run entry whose library cannot cache (an OUT_DIR-reading
            // library) restores standalone below, with its compile unit.
            if meta.kind != UnitKind::BuildScriptCompile {
                roots.push((entry, meta));
            }
        }
        let claimed_by_library: BTreeSet<String> = roots
            .iter()
            .filter(|(_, meta)| meta.kind == UnitKind::Library)
            .flat_map(|(_, meta)| meta.group.iter().cloned())
            .collect();
        for (entry, meta) in roots {
            if meta.kind == UnitKind::BuildScriptRun {
                let key = entry
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or_default()
                    .to_owned();
                if claimed_by_library.contains(&key) {
                    continue;
                }
            }
            let group = meta.group.clone();
            let mut members = vec![(entry, meta)];
            let mut group_available = true;
            for key in &group {
                let member = package_directory.join(key);
                if member.join("unstable").is_file() || member.join("excluded").is_file() {
                    group_available = false;
                    break;
                }
                let Some(member_meta) = read_meta(&member.join("meta")) else {
                    group_available = false;
                    break;
                };
                if member_meta.kind == UnitKind::Library
                    || member_meta.rustc_digest != rustc_digest
                    || member_meta.profile_name != location.profile_name
                    || member_meta.package_name != *name
                    || member_meta.package_version != *version
                {
                    group_available = false;
                    break;
                }
                members.push((member, member_meta));
            }
            if !group_available {
                counts.skipped_group += 1;
                continue;
            }
            candidates.push(Candidate { entries: members });
        }
    }
    counts.candidates = candidates.len();
    let mut claims: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut contested: BTreeSet<usize> = BTreeSet::new();
    for (index, candidate) in candidates.iter().enumerate() {
        for (_, meta) in &candidate.entries {
            for file in &meta.files {
                let destination = destination_for(destination_profile, meta, &file.relative);
                if let Some(previous) = claims.insert(destination, index) {
                    contested.insert(previous);
                    contested.insert(index);
                }
            }
        }
    }
    counts.skipped_conflict = contested.len();
    let restored_at = SystemTime::now();
    'candidates: for (candidate_index, candidate) in candidates.iter().enumerate() {
        if contested.contains(&candidate_index) {
            continue;
        }
        let mut plan: Vec<(PathBuf, Vec<u8>, bool)> = Vec::new();
        let mut plan_directories: Vec<PathBuf> = Vec::new();
        for (entry, meta) in &candidate.entries {
            for directory in &meta.directories {
                plan_directories.push(destination_for(destination_profile, meta, directory));
            }
            for file in &meta.files {
                let destination = destination_for(destination_profile, meta, &file.relative);
                if destination.symlink_metadata().is_ok() {
                    counts.skipped_existing += 1;
                    continue 'candidates;
                }
                let source = entry.join("files").join(&file.relative);
                let Some(contents) = read_bounded(&source, MAX_UNIT_BYTES.max(MAX_OUT_BYTES))
                else {
                    counts.digest_failures += 1;
                    let _ = fs::remove_dir_all(entry);
                    continue 'candidates;
                };
                let mut hasher = Sha256::new();
                hasher.update(&contents);
                if contents.len() as u64 != file.size || hex(&hasher.finalize()) != file.digest {
                    counts.digest_failures += 1;
                    let _ = fs::remove_dir_all(entry);
                    continue 'candidates;
                }
                let contents = if file.rewrite {
                    let Some(rewritten) =
                        rewrite_dependency_file(&contents, &meta.donor_prefix, destination_prefix)
                    else {
                        continue 'candidates;
                    };
                    rewritten
                } else {
                    contents
                };
                plan.push((destination, contents, file.executable));
            }
        }
        let mut wrote = Vec::new();
        let mut failed = false;
        for directory in &plan_directories {
            if fs::create_dir_all(directory).is_err() {
                failed = true;
                break;
            }
        }
        if !failed {
            for (destination, contents, executable) in &plan {
                let Some(parent) = destination.parent() else {
                    failed = true;
                    break;
                };
                if fs::create_dir_all(parent).is_err() {
                    failed = true;
                    break;
                }
                // Every restored file across every unit of this pass carries
                // one shared modification time. Cargo marks a unit stale
                // when a dependency output is strictly newer than the unit's
                // own dep-info file, so write-order timestamps would
                // spuriously recompile every restored unit whose dependency
                // was written after it.
                let created = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(destination)
                    .and_then(|mut file| {
                        file.write_all(contents)?;
                        file.set_modified(restored_at)
                    });
                match created {
                    Ok(()) => {
                        wrote.push(destination.clone());
                        if *executable
                            && fs::set_permissions(destination, fs::Permissions::from_mode(0o755))
                                .is_err()
                        {
                            failed = true;
                            break;
                        }
                    }
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
        }
        if failed {
            for path in wrote {
                let _ = fs::remove_file(path);
            }
            counts.skipped_existing += 1;
            continue;
        }
        for (entry, _) in &candidate.entries {
            touch_entry(entry);
        }
        counts.restored += 1;
    }
    Ok(counts)
}

fn destination_for(destination_profile: &Path, meta: &MetaFile, relative: &str) -> PathBuf {
    match relative.split_once('/') {
        Some(("deps", name)) => destination_profile.join("deps").join(name),
        Some(("fingerprint", name)) => destination_profile
            .join(".fingerprint")
            .join(&meta.fingerprint_directory_name)
            .join(name),
        Some(("build", rest)) => destination_profile
            .join("build")
            .join(&meta.fingerprint_directory_name)
            .join(rest),
        _ => destination_profile.join("invalid").join(relative),
    }
}

/// Rewrites the donor target prefix to the destination prefix and proves the
/// substitution exact by reversing it: only prefix occurrences may differ.
fn rewrite_dependency_file(
    contents: &[u8],
    donor_prefix: &str,
    destination_prefix: &str,
) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(contents).ok()?;
    if donor_prefix.is_empty() || text.contains(destination_prefix) {
        // A dependency file that already mentions the destination cannot be
        // rewritten reversibly; skip the unit.
        return (donor_prefix == destination_prefix).then(|| contents.to_vec());
    }
    let rewritten = text.replace(donor_prefix, destination_prefix);
    let reversed = rewritten.replace(destination_prefix, donor_prefix);
    (reversed == text).then(|| rewritten.into_bytes())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "cinder-unitcache-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn lockfile_parser_extracts_packages_and_rejects_surprises() {
        let root = temp_directory("lock");
        let lock = root.join("Cargo.lock");
        fs::write(
            &lock,
            "# comment\nversion = 4\n\n[[package]]\nname = \"memchr\"\nversion = \"2.7.4\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             checksum = \"78ca9ab1a0babb1e7d5695e3530886289c18cf2f87ec19a575a0abdce112e3a3\"\n\
             dependencies = [\n \"a\",\n \"b 1.0.0\",\n]\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let packages = lock_packages(&lock).unwrap();
        assert_eq!(
            packages,
            vec![
                ("memchr".to_owned(), "2.7.4".to_owned()),
                ("app".to_owned(), "0.1.0".to_owned())
            ]
        );

        fs::write(&lock, "[[package]]\nname = \"x\"\nversion = { evil = 1 }\n").unwrap();
        assert!(lock_packages(&lock).is_none());
        fs::write(
            &lock,
            "[[package]]\nname = \"x\\\"y\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        assert!(lock_packages(&lock).is_none());
        fs::write(&lock, "not a lockfile at all").unwrap();
        assert!(lock_packages(&lock).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn encoded_dep_info_recognizes_only_the_empty_file_list() {
        // Byte shapes captured from Cargo 1.93 fingerprints.
        let registry = [
            0x01, 0x00, 0x00, 0x00, 0xff, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(encoded_dep_info_tracks_no_files(&registry));
        let mut with_environment = registry[..10].to_vec();
        with_environment.extend_from_slice(&1u32.to_le_bytes());
        with_environment.extend_from_slice(&3u32.to_le_bytes());
        with_environment.extend_from_slice(b"KEY");
        with_environment.push(0);
        assert!(encoded_dep_info_tracks_no_files(&with_environment));
        let mut with_present_value = registry[..10].to_vec();
        with_present_value.extend_from_slice(&1u32.to_le_bytes());
        with_present_value.extend_from_slice(&3u32.to_le_bytes());
        with_present_value.extend_from_slice(b"KEY");
        with_present_value.push(1);
        with_present_value.extend_from_slice(&2u32.to_le_bytes());
        with_present_value.extend_from_slice(b"vv");
        assert!(encoded_dep_info_tracks_no_files(&with_present_value));

        let path_dependency: Vec<u8> = {
            let mut bytes = vec![0x01, 0x00, 0x00, 0x00, 0xff, 0x01];
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&10u32.to_le_bytes());
            bytes.extend_from_slice(b"src/lib.rs");
            bytes.push(0);
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes
        };
        assert!(!encoded_dep_info_tracks_no_files(&path_dependency));
        assert!(!encoded_dep_info_tracks_no_files(&registry[..13]));
        let mut trailing = registry.to_vec();
        trailing.push(0);
        assert!(!encoded_dep_info_tracks_no_files(&trailing));
        let mut wrong_header = registry;
        wrong_header[4] = 0xfe;
        assert!(!encoded_dep_info_tracks_no_files(&wrong_header));
    }

    #[test]
    fn fingerprint_hash_text_matches_cargo_rendering() {
        // Observed pair: bstr's JSON edge for memchr vs memchr's hash file.
        assert_eq!(
            fingerprint_hash_text(7869073639887851339),
            "4bcfe788c890346d"
        );
    }

    #[test]
    fn meta_round_trip_rejects_tampering() {
        let root = temp_directory("meta");
        let path = root.join("meta");
        let meta = MetaFile {
            kind: UnitKind::Library,
            package_name: "memchr".to_owned(),
            package_version: "2.7.4".to_owned(),
            fingerprint_directory_name: "memchr-4783d14dd8e34994".to_owned(),
            rustc_digest: "ab".repeat(32),
            profile_name: "debug".to_owned(),
            source_root: PathBuf::from("/registry/src/index"),
            donor_prefix: "/donor/target/debug".to_owned(),
            group: vec!["aaaa-bbbb".to_owned()],
            directories: vec!["build/out".to_owned()],
            files: vec![MetaFileEntry {
                relative: "deps/libmemchr-4783d14dd8e34994.rlib".to_owned(),
                size: 10,
                digest: "cd".repeat(32),
                executable: false,
                rewrite: false,
            }],
        };
        write_meta(&path, &meta).unwrap();
        let decoded = read_meta(&path).unwrap();
        assert_eq!(decoded.package_name, "memchr");
        assert_eq!(decoded.files.len(), 1);
        assert_eq!(decoded.group, vec!["aaaa-bbbb".to_owned()]);
        assert_eq!(decoded.directories, vec!["build/out".to_owned()]);
        assert!(matches!(decoded.kind, UnitKind::Library));

        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, original.replace(META_MAGIC, "CNDU0001")).unwrap();
        assert!(read_meta(&path).is_none());
        fs::write(&path, original.replace("deps/", "../deps/")).unwrap();
        assert!(read_meta(&path).is_none());
        fs::write(&path, format!("{original}unknown value\n")).unwrap();
        assert!(read_meta(&path).is_none());
        fs::write(&path, original.replace(" - deps/", " q deps/")).unwrap();
        assert!(read_meta(&path).is_none());
        fs::write(&path, original.replace("kind library", "kind mystery")).unwrap();
        assert!(read_meta(&path).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn meta_round_trip_preserves_build_script_flags() {
        let root = temp_directory("meta-flags");
        let path = root.join("meta");
        let meta = MetaFile {
            kind: UnitKind::BuildScriptRun,
            package_name: "probedep".to_owned(),
            package_version: "1.0.0".to_owned(),
            fingerprint_directory_name: "probedep-3c9de8e89fc9978b".to_owned(),
            rustc_digest: "ab".repeat(32),
            profile_name: "debug".to_owned(),
            source_root: PathBuf::from("/registry/src/index"),
            donor_prefix: "/donor/target/debug".to_owned(),
            group: vec!["cccc-dddd".to_owned()],
            directories: vec!["build/out".to_owned(), "build/out/nested".to_owned()],
            files: vec![
                MetaFileEntry {
                    relative: "build/root-output".to_owned(),
                    size: 44,
                    digest: "cd".repeat(32),
                    executable: false,
                    rewrite: true,
                },
                MetaFileEntry {
                    relative: "build/out/nested/gen.rs".to_owned(),
                    size: 20,
                    digest: "ef".repeat(32),
                    executable: true,
                    rewrite: true,
                },
            ],
        };
        write_meta(&path, &meta).unwrap();
        let decoded = read_meta(&path).unwrap();
        assert!(matches!(decoded.kind, UnitKind::BuildScriptRun));
        assert!(decoded.files[0].rewrite && !decoded.files[0].executable);
        assert!(decoded.files[1].rewrite && decoded.files[1].executable);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_paths_admit_only_known_shapes() {
        assert!(relative_path_is_safe("deps/libx-0000.rlib"));
        assert!(relative_path_is_safe("fingerprint/lib-x"));
        assert!(relative_path_is_safe("build/output"));
        assert!(relative_path_is_safe("build/out"));
        assert!(relative_path_is_safe("build/out/a/b/c.rs"));
        assert!(!relative_path_is_safe("deps/a/b"));
        assert!(!relative_path_is_safe("build/other/nested"));
        assert!(!relative_path_is_safe("build/out/../x"));
        assert!(!relative_path_is_safe("build/out//x"));
        assert!(!relative_path_is_safe("elsewhere/x"));
        assert!(!relative_path_is_safe("deps"));
        let deep = format!("build/out/{}", vec!["d"; MAX_OUT_COMPONENTS].join("/"));
        assert!(!relative_path_is_safe(&deep));
    }

    #[test]
    fn run_unit_locals_accept_only_observed_shapes() {
        let package = Path::new("/reg/src/probedep-1.0.0");
        let precalculated: Vec<serde_json::Value> =
            vec![serde_json::json!({"Precalculated": "1.0.0"})];
        assert!(run_unit_local_is_supported(
            &precalculated,
            "debug",
            "probedep-0000",
            package,
            "1.0.0"
        ));
        // The precalculated value must be the package version.
        assert!(!run_unit_local_is_supported(
            &precalculated,
            "debug",
            "probedep-0000",
            package,
            "2.0.0"
        ));
        let rerun: Vec<serde_json::Value> = vec![
            serde_json::json!({"RerunIfChanged": {
                "output": "debug/build/probedep-0000/output",
                "paths": ["build.rs"]
            }}),
            serde_json::json!({"RerunIfEnvChanged": {"var": "PROBE", "val": null}}),
            serde_json::json!({"RerunIfEnvChanged": {"var": "OTHER", "val": "set"}}),
        ];
        assert!(run_unit_local_is_supported(
            &rerun,
            "debug",
            "probedep-0000",
            package,
            "1.0.0"
        ));
        // The output must be this unit's own build directory.
        assert!(!run_unit_local_is_supported(
            &rerun,
            "debug",
            "probedep-1111",
            package,
            "1.0.0"
        ));
        // Escaping or absolute rerun paths are rejected.
        let escaping: Vec<serde_json::Value> = vec![serde_json::json!({"RerunIfChanged": {
            "output": "debug/build/probedep-0000/output",
            "paths": ["../outside.rs"]
        }})];
        assert!(!run_unit_local_is_supported(
            &escaping,
            "debug",
            "probedep-0000",
            package,
            "1.0.0"
        ));
        let absolute: Vec<serde_json::Value> = vec![serde_json::json!({"RerunIfChanged": {
            "output": "debug/build/probedep-0000/output",
            "paths": ["/etc/hosts"]
        }})];
        assert!(!run_unit_local_is_supported(
            &absolute,
            "debug",
            "probedep-0000",
            package,
            "1.0.0"
        ));
        // Env-only locals (no RerunIfChanged) and unknown locals fail closed.
        let env_only: Vec<serde_json::Value> =
            vec![serde_json::json!({"RerunIfEnvChanged": {"var": "P", "val": null}})];
        assert!(!run_unit_local_is_supported(
            &env_only,
            "debug",
            "probedep-0000",
            package,
            "1.0.0"
        ));
        let unknown: Vec<serde_json::Value> = vec![serde_json::json!({"SomethingNew": 1})];
        assert!(!run_unit_local_is_supported(
            &unknown,
            "debug",
            "probedep-0000",
            package,
            "1.0.0"
        ));
        assert!(!run_unit_local_is_supported(
            &[],
            "debug",
            "probedep-0000",
            package,
            "1.0.0"
        ));
    }

    #[test]
    fn relocatability_classifier_guards_binary_and_workspace_bytes() {
        let donor = "/ws/target/debug";
        let workspace = "/ws";
        // Path-free content restores verbatim.
        assert!(matches!(
            classify_relocatability(b"plain bytes", donor, workspace),
            Relocatability::Verbatim
        ));
        // A donor occurrence in text is rewritable.
        assert!(matches!(
            classify_relocatability(b"path=/ws/target/debug/build/x/out\n", donor, workspace),
            Relocatability::Rewrite
        ));
        // A donor occurrence in a binary file is excluded.
        let mut binary = b"\x00\x01\xff".to_vec();
        binary.extend_from_slice(donor.as_bytes());
        assert!(matches!(
            classify_relocatability(&binary, donor, workspace),
            Relocatability::Excluded(_)
        ));
        // A workspace occurrence outside a donor occurrence is excluded,
        // even in valid UTF-8.
        assert!(matches!(
            classify_relocatability(b"manifest=/ws/crates/app\n", donor, workspace),
            Relocatability::Excluded(_)
        ));
    }

    #[test]
    fn dependency_file_rewrite_is_reversible_and_exact() {
        let donor = "/donor/target/debug";
        let destination = "/other/target/cinder-tuned/debug";
        let contents = format!(
            "{donor}/deps/libx-0000.rlib: /reg/src/x-1.0.0/src/lib.rs\n\n\
             {donor}/deps/x-0000.d: /reg/src/x-1.0.0/src/lib.rs\n\n\
             /reg/src/x-1.0.0/src/lib.rs:\n"
        );
        let rewritten = rewrite_dependency_file(contents.as_bytes(), donor, destination).unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();
        assert_eq!(rewritten, contents.replace(donor, destination));
        // A file that already mentions the destination cannot be reversed.
        assert!(rewrite_dependency_file(rewritten.as_bytes(), donor, destination).is_none());
        // Identical prefixes restore the identical bytes.
        assert_eq!(
            rewrite_dependency_file(contents.as_bytes(), donor, donor).unwrap(),
            contents.as_bytes()
        );
    }

    #[test]
    fn dependency_file_parser_enforces_registry_shape() {
        let donor = "/donor/target/debug";
        let ok = format!(
            "{donor}/deps/libmemchr-0000.rlib: /reg/src/memchr-2.7.4/src/lib.rs /reg/src/memchr-2.7.4/src/m.rs\n\
             /reg/src/memchr-2.7.4/src/lib.rs:\n/reg/src/memchr-2.7.4/src/m.rs:\n"
        );
        let facts = parse_dependency_file(&ok, donor, "memchr").unwrap();
        assert_eq!(
            facts.package_directory,
            PathBuf::from("/reg/src/memchr-2.7.4")
        );
        // Sources from two package roots are rejected.
        let mixed = format!(
            "{donor}/deps/libmemchr-0000.rlib: /reg/src/memchr-2.7.4/src/lib.rs /reg/src/memchr-2.8.0/src/lib.rs\n"
        );
        assert!(parse_dependency_file(&mixed, donor, "memchr").is_none());
        // Escapes are rejected outright.
        let escaped =
            format!("{donor}/deps/libmemchr-0000.rlib: /reg/src/memchr-2.7.4/src/a\\ b.rs\n");
        assert!(parse_dependency_file(&escaped, donor, "memchr").is_none());
        // A donor-prefixed prerequisite is rejected.
        let self_referential =
            format!("{donor}/deps/libmemchr-0000.rlib: {donor}/deps/other.rmeta\n");
        assert!(parse_dependency_file(&self_referential, donor, "memchr").is_none());
        // A relative source is rejected.
        let relative = format!("{donor}/deps/libmemchr-0000.rlib: src/lib.rs\n");
        assert!(parse_dependency_file(&relative, donor, "memchr").is_none());
        // No donor-prefixed output rule at all is rejected.
        assert!(
            parse_dependency_file("/reg/src/memchr-2.7.4/src/lib.rs:\n", donor, "memchr").is_none()
        );
    }

    #[test]
    fn package_directory_names_split_conservatively() {
        assert_eq!(
            split_package_directory_name("memchr-2.7.4"),
            Some(("memchr", "2.7.4"))
        );
        assert_eq!(
            split_package_directory_name("unicode-width-0.1.11"),
            Some(("unicode-width", "0.1.11"))
        );
        assert_eq!(
            split_package_directory_name("foo-1.0.0-beta.2"),
            Some(("foo", "1.0.0-beta.2"))
        );
        assert_eq!(split_package_directory_name("noversion"), None);
        assert_eq!(split_package_directory_name("-1.0.0"), None);
        assert_eq!(split_package_directory_name("foo-bar"), None);
    }

    #[test]
    fn package_ancestor_normalizes_dot_dot_sources_and_rejects_escapes() {
        // `include_str!("../README.md")`-style sources stay inside the
        // package directory after normalization.
        let inside = Path::new("/registry/src/index/winnow-0.7.15/src/../examples/css/parser.rs");
        assert_eq!(
            package_ancestor(inside, "winnow"),
            Some(PathBuf::from("/registry/src/index/winnow-0.7.15"))
        );
        // A path that climbs out of the package directory attributes to
        // where it lands, never to a directory it merely passes through.
        let escape = Path::new("/registry/src/index/winnow-0.7.15/../other-1.0.0/src/lib.rs");
        assert_eq!(package_ancestor(escape, "winnow"), None);
        // Climbing past the root is unattributable.
        assert_eq!(
            package_ancestor(Path::new("/../winnow-0.7.15/src/lib.rs"), "winnow"),
            None
        );
    }

    #[test]
    fn eviction_removes_least_recent_entries_and_keeps_unstable_markers() {
        let root = temp_directory("evict");
        let store = root.join(STORE_VERSION);
        let index = store.join("index");
        let old_entry = index.join("old-1.0.0/aaaa-bbbb");
        let new_entry = index.join("new-1.0.0/cccc-dddd");
        let unstable_entry = index.join("bad-1.0.0/eeee-ffff");
        for entry in [&old_entry, &new_entry, &unstable_entry] {
            fs::create_dir_all(entry.join("files")).unwrap();
        }
        fs::write(old_entry.join("files/data"), vec![0u8; 4096]).unwrap();
        fs::write(old_entry.join("touched"), b"1").unwrap();
        fs::write(new_entry.join("files/data"), vec![0u8; 4096]).unwrap();
        fs::write(unstable_entry.join("unstable"), b"1").unwrap();
        // Make the old entry's recency clearly older.
        let old_time = SystemTime::now() - std::time::Duration::from_secs(600);
        let file = fs::File::options()
            .write(true)
            .open(old_entry.join("touched"))
            .unwrap();
        file.set_modified(old_time).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(new_entry.join("touched"), b"1").unwrap();

        // SAFETY: test-only environment mutation; the suite runs these
        // helpers in-process without concurrent environment readers.
        unsafe { env::set_var(UNIT_CACHE_ROOT, &root) };
        unsafe { env::set_var(UNIT_CACHE_BYTES, "6000") };
        enforce_store_bound(&store).unwrap();
        unsafe { env::remove_var(UNIT_CACHE_BYTES) };
        unsafe { env::remove_var(UNIT_CACHE_ROOT) };

        assert!(!old_entry.exists(), "least recent entry should be evicted");
        assert!(new_entry.exists(), "most recent entry should survive");
        assert!(unstable_entry.join("unstable").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unstable_marker_blocks_future_stores() {
        let root = temp_directory("unstable");
        let store = root.join(STORE_VERSION);
        let profile = root.join("project/target/debug");
        fs::create_dir_all(&profile).unwrap();
        let unit = QualifiedUnit {
            kind: UnitKind::Library,
            fingerprint_directory_name: "pkg-0123456789abcdef".to_owned(),
            package_name: "pkg".to_owned(),
            package_version: "1.0.0".to_owned(),
            fingerprint_hash: "aaaaaaaaaaaaaaaa".to_owned(),
            unit_hash: "0123456789abcdef".to_owned(),
            source_root: root.clone(),
            package_directory: root.join("pkg-1.0.0"),
            files: vec![EntryFile {
                relative: "deps/pkg-0123456789abcdef.d".to_owned(),
                source: profile.join("deps/pkg-0123456789abcdef.d"),
                executable: false,
                rewrite: true,
            }],
            directories: Vec::new(),
            group: Vec::new(),
            excluded: None,
        };
        let entry = entry_directory(&store, &unit);
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("unstable"), b"1").unwrap();
        let stored = store_unit(&store, &unit, &"ab".repeat(32), "debug", &profile).unwrap();
        assert!(!stored);
        assert!(entry.join("unstable").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exclusion_markers_persist_and_block_storing() {
        let root = temp_directory("excluded");
        let store = root.join(STORE_VERSION);
        let profile = root.join("project/target/debug");
        fs::create_dir_all(&profile).unwrap();
        let unit = QualifiedUnit {
            kind: UnitKind::BuildScriptRun,
            fingerprint_directory_name: "pkg-0123456789abcdef".to_owned(),
            package_name: "pkg".to_owned(),
            package_version: "1.0.0".to_owned(),
            fingerprint_hash: "aaaaaaaaaaaaaaaa".to_owned(),
            unit_hash: "0123456789abcdef".to_owned(),
            source_root: root.clone(),
            package_directory: root.join("pkg-1.0.0"),
            files: Vec::new(),
            directories: Vec::new(),
            group: Vec::new(),
            excluded: Some("output tree is too large".to_owned()),
        };
        let stored = store_unit(&store, &unit, &"ab".repeat(32), "debug", &profile).unwrap();
        assert!(!stored);
        let entry = entry_directory(&store, &unit);
        assert_eq!(
            fs::read_to_string(entry.join("excluded")).unwrap(),
            "output tree is too large"
        );
        // A later record pass with a now-relocatable unit still stays out.
        let mut relocatable = unit;
        relocatable.excluded = None;
        let stored = store_unit(&store, &relocatable, &"ab".repeat(32), "debug", &profile).unwrap();
        assert!(!stored);
        assert!(entry.join("excluded").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_registry_fingerprints_qualify_and_path_dependencies_do_not() {
        let root = temp_directory("qualify");
        let project = root.join("app");
        fs::create_dir_all(project.join("src")).unwrap();
        // A vendored registry-style directory source keeps the test offline.
        let vendor = root.join("vendor");
        let vendored = vendor.join("tinydep-1.0.0");
        fs::create_dir_all(vendored.join("src")).unwrap();
        fs::write(
            vendored.join("Cargo.toml"),
            "[package]\nname = \"tinydep\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(vendored.join("src/lib.rs"), "pub fn v() -> i32 { 41 }\n").unwrap();
        fs::write(
            vendored.join(".cargo-checksum.json"),
            "{\"files\":{},\"package\":\"\"}",
        )
        .unwrap();
        fs::create_dir_all(project.join(".cargo")).unwrap();
        fs::write(
            project.join(".cargo/config.toml"),
            format!(
                "[source.crates-io]\nreplace-with = \"vendored\"\n\n\
                 [source.vendored]\ndirectory = \"{}\"\n",
                vendor.display()
            ),
        )
        .unwrap();
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\ntinydep = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            project.join("src/main.rs"),
            "fn main() { println!(\"{}\", tinydep::v()); }\n",
        )
        .unwrap();
        let output = std::process::Command::new("cargo")
            .current_dir(&project)
            .args(["build", "--offline"])
            .env("CARGO_TARGET_DIR", project.join("target"))
            .env_remove("RUSTC")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "vendored fixture build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let profile = project.join("target/debug");
        let store = root.join("store").join(STORE_VERSION);
        let units = qualified_units(
            &store,
            &profile,
            "debug",
            &profile.join(".fingerprint"),
            &profile.join("deps"),
            &project,
        )
        .unwrap();
        let named: Vec<&str> = units
            .iter()
            .map(|unit| unit.package_name.as_str())
            .collect();
        assert_eq!(
            named,
            vec!["tinydep"],
            "exactly the vendored registry-style dependency should qualify"
        );
        let unit = &units[0];
        assert_eq!(unit.package_version, "1.0.0");
        assert!(unit.source_root.ends_with("vendor"));
        assert!(matches!(unit.kind, UnitKind::Library));
        assert!(unit.group.is_empty());
        assert!(
            unit.files
                .iter()
                .any(|file| file.relative.ends_with(".rlib"))
        );
        assert!(
            unit.files
                .iter()
                .all(|file| file.rewrite == file.relative.ends_with(".d"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_build_script_groups_qualify_with_probe_and_out_dir_shapes() {
        let root = temp_directory("qualify-bs");
        let project = root.join("app");
        fs::create_dir_all(project.join("src")).unwrap();
        let vendor = root.join("vendor");
        let write_dep = |name: &str, build: &str, lib: &str| {
            let package = vendor.join(format!("{name}-1.0.0"));
            fs::create_dir_all(package.join("src")).unwrap();
            fs::write(
                package.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\nedition = \"2021\"\n"),
            )
            .unwrap();
            fs::write(package.join("build.rs"), build).unwrap();
            fs::write(package.join("src/lib.rs"), lib).unwrap();
            fs::write(
                package.join(".cargo-checksum.json"),
                "{\"files\":{},\"package\":\"\"}",
            )
            .unwrap();
        };
        // The three real-world shapes: a silent script (Precalculated), a
        // directive-emitting probe, and an OUT_DIR generator whose output
        // file embeds no paths.
        write_dep("silentdep", "fn main() {}", "pub fn s() -> i32 { 1 }");
        write_dep(
            "probedep",
            "fn main() { println!(\"cargo:rustc-cfg=probed\");\n\
             println!(\"cargo:rerun-if-changed=build.rs\");\n\
             println!(\"cargo:rerun-if-env-changed=PROBE_ENV_X\"); }",
            "pub fn p() -> i32 { if cfg!(probed) { 2 } else { 0 } }",
        );
        write_dep(
            "outdirdep",
            "use std::{env, fs, path::Path};\n\
             fn main() { let out = env::var(\"OUT_DIR\").unwrap();\n\
             fs::write(Path::new(&out).join(\"gen.rs\"), \"pub fn g() -> i32 { 7 }\\n\").unwrap();\n\
             println!(\"cargo:rerun-if-changed=build.rs\"); }",
            "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));",
        );
        fs::create_dir_all(project.join(".cargo")).unwrap();
        fs::write(
            project.join(".cargo/config.toml"),
            format!(
                "[source.crates-io]\nreplace-with = \"vendored\"\n\n\
                 [source.vendored]\ndirectory = \"{}\"\n",
                vendor.display()
            ),
        )
        .unwrap();
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nsilentdep = \"1.0.0\"\nprobedep = \"1.0.0\"\noutdirdep = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            project.join("src/main.rs"),
            "fn main() { println!(\"{} {} {}\", silentdep::s(), probedep::p(), outdirdep::g()); }\n",
        )
        .unwrap();
        let output = std::process::Command::new("cargo")
            .current_dir(&project)
            .args(["build", "--offline"])
            .env("CARGO_TARGET_DIR", project.join("target"))
            .env_remove("RUSTC")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "vendored fixture build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let profile = project.join("target/debug");
        let store = root.join("store").join(STORE_VERSION);
        let units = qualified_units(
            &store,
            &profile,
            "debug",
            &profile.join(".fingerprint"),
            &profile.join("deps"),
            &project,
        )
        .unwrap();
        let mut by_kind: BTreeMap<&str, Vec<&QualifiedUnit>> = BTreeMap::new();
        for unit in &units {
            by_kind.entry(unit.kind.as_str()).or_default().push(unit);
        }
        // Every package yields its build-script pair; only libraries that
        // never read OUT_DIR qualify (an OUT_DIR reader's rlib embeds the
        // generated file's absolute path in its debug information).
        assert_eq!(by_kind.get("library").map_or(0, Vec::len), 2);
        assert_eq!(by_kind.get("build-script-compile").map_or(0, Vec::len), 3);
        assert_eq!(by_kind.get("build-script-run").map_or(0, Vec::len), 3);
        assert!(
            !by_kind["library"]
                .iter()
                .any(|unit| unit.package_name == "outdirdep"),
            "an OUT_DIR-reading library must not cache"
        );
        if let Some(unit) = units.iter().find(|unit| unit.excluded.is_some()) {
            panic!(
                "unexpected exclusion for {}: {:?}",
                unit.fingerprint_directory_name, unit.excluded
            );
        }
        for library in &by_kind["library"] {
            assert_eq!(
                library.group.len(),
                2,
                "{} should carry its run and compile group entries",
                library.package_name
            );
            assert_ne!(library.package_name, "outdirdep");
        }
        for run in &by_kind["build-script-run"] {
            assert!(run.directories.contains(&"build/out".to_owned()));
            assert!(
                run.files
                    .iter()
                    .any(|file| file.relative == "build/root-output" && file.rewrite),
                "root-output must be rewrite-flagged for {}",
                run.package_name
            );
            if run.package_name == "outdirdep" {
                assert!(
                    run.files
                        .iter()
                        .any(|file| file.relative == "build/out/gen.rs" && !file.rewrite),
                    "path-free generated content restores verbatim"
                );
            }
        }
        for compile in &by_kind["build-script-compile"] {
            assert!(
                compile
                    .files
                    .iter()
                    .any(|file| file.relative == "build/build-script-build" && file.executable)
            );
            assert!(
                compile
                    .files
                    .iter()
                    .any(|file| file.relative.ends_with(".d") && file.rewrite)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
