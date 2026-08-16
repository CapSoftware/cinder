//! Experimental exact compiler-recipe capture and replay.
//!
//! Recipes are local, private state. They contain the exact executable,
//! arguments, working directory, and allowlisted Cargo-generated environment
//! values proven by Cargo's rustc dep-info. A per-recipe salt and one-way
//! fingerprints bind that exact prior dependency set without persisting
//! arbitrary environment names or values. The replayed dep-info must not add or
//! change an environment access; otherwise Cinder discards the result and lets
//! Cargo establish a new baseline.
//! Replays are accepted only when rustc succeeds without user-visible
//! diagnostics; every other result falls back to Cargo.

use super::{OsStr, OsString, OsStringExt, Path, PathBuf, env, fs};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::OsStr as StdOsStr,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

// Recipe admission is part of the format contract. Bump this whenever an older
// Cinder could have persisted a recipe without a proof required by the current
// replay policy. Version 6 adds privacy-safe fingerprints of Cargo's previous
// rustc environment dependencies so newly introduced or changed accesses can
// be rejected after compilation without persisting arbitrary values.
const RECIPE_MAGIC: &[u8; 8] = b"CNDRCP06";
const MAX_VALUES: usize = 16_384;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: u64 = 1024 * 1024;
const MAX_DEPENDENCY_FILE_BYTES: u64 = 64 * 1024 * 1024;
type DependencyEnvironmentEntry = (OsString, Option<Vec<u8>>);
type DependencyEnvironment = Vec<DependencyEnvironmentEntry>;
const FIXED_CARGO_COMPILER_ENVIRONMENT: [&str; 12] = [
    "CARGO",
    "CARGO_BIN_NAME",
    "CARGO_CRATE_NAME",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_SBOM_PATH",
    "CARGO_TARGET_TMPDIR",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "LD_LIBRARY_PATH",
    "LIBPATH",
    "OUT_DIR",
];

#[derive(Clone)]
pub(super) struct CompilerRecipe {
    pub(super) executable: PathBuf,
    pub(super) working_directory: PathBuf,
    pub(super) arguments: Vec<OsString>,
    pub(super) environment: Vec<(OsString, OsString)>,
    environment_fingerprint_salt: [u8; 32],
    dependency_environment_absences: Vec<[u8; 32]>,
    dependency_environment_values: Vec<[u8; 32]>,
}

impl CompilerRecipe {
    #[cfg(target_os = "macos")]
    pub(super) fn from_observed(
        executable: PathBuf,
        working_directory: PathBuf,
        arguments: Vec<OsString>,
    ) -> Option<Self> {
        if !executable.is_absolute()
            || !working_directory.is_absolute()
            || !arguments
                .windows(2)
                .any(|values| values[0] == "--crate-name")
            || !super::capture::rustc_list_options(&arguments, "--emit")
                .iter()
                .any(|values| {
                    values
                        .split(',')
                        .any(|value| matches!(value, "link" | "metadata"))
                })
        {
            return None;
        }
        Some(Self {
            executable,
            working_directory,
            arguments,
            environment: Vec::new(),
            environment_fingerprint_salt: environment_fingerprint_salt()?,
            dependency_environment_absences: Vec::new(),
            dependency_environment_values: Vec::new(),
        })
    }

    /// Restores only Cargo-generated values that rustc proved the selected
    /// unit observed. User-provided environment remains inherited from Cinder
    /// and is already bound into the command context.
    pub(super) fn bind_dependency_environment(
        &mut self,
        dependency_file: &Path,
        out_directory: Option<&Path>,
    ) -> Result<(), String> {
        let dependencies = dependency_environment(dependency_file)?;
        if dependencies.len() > MAX_VALUES {
            return Err("compiler dependency file has too many environment values".to_owned());
        }
        let mut dependency_keys = BTreeSet::new();
        let mut dependency_environment_absences = Vec::new();
        let mut dependency_environment_values = Vec::new();
        let mut environment = std::collections::BTreeMap::new();
        for (key, value) in self.environment.drain(..) {
            environment.insert(key, value);
        }
        for (key, value) in dependencies {
            if !dependency_keys.insert(key.clone()) {
                return Err("compiler dependency environment contains a duplicate key".to_owned());
            }
            match value.as_deref() {
                Some(value) => dependency_environment_values.push(environment_value_fingerprint(
                    &self.environment_fingerprint_salt,
                    key.as_os_str(),
                    value,
                )),
                None => dependency_environment_absences.push(environment_key_fingerprint(
                    &self.environment_fingerprint_salt,
                    key.as_os_str(),
                )),
            }
            if cargo_compiler_environment(&key) {
                match value {
                    Some(value) => {
                        environment.insert(key, OsString::from_vec(value));
                    }
                    None => {
                        environment.remove(&key);
                    }
                }
            } else if !inherited_environment_matches(&key, value.as_deref()) {
                return Err(format!(
                    "compiler dependency environment {} cannot be reconstructed exactly",
                    key.to_string_lossy()
                ));
            }
        }
        if let Some(out_directory) = out_directory {
            environment.insert(
                OsString::from("OUT_DIR"),
                out_directory.as_os_str().to_owned(),
            );
        }
        if environment.len() > MAX_VALUES {
            return Err("compiler recipe has too many environment values".to_owned());
        }
        if environment.iter().any(|(key, value)| {
            key.as_bytes().len() > MAX_VALUE_BYTES || value.as_bytes().len() > MAX_VALUE_BYTES
        }) {
            return Err("compiler recipe environment value is too long".to_owned());
        }
        dependency_environment_absences.sort_unstable();
        dependency_environment_values.sort_unstable();
        self.environment = environment.into_iter().collect();
        self.dependency_environment_absences = dependency_environment_absences;
        self.dependency_environment_values = dependency_environment_values;
        Ok(())
    }

    /// Confirms that replayed source made only environment accesses present in
    /// Cargo's previous rustc dependency file and observed the same values.
    ///
    /// A newly added `env!` or `option_env!` can compile successfully with a
    /// value Cargo would not have supplied to a direct rustc replay. Requiring
    /// an exact prior fingerprint makes that edit a one-time Cargo fallback.
    pub(super) fn replayed_dependency_environment_matches(
        &self,
        dependency_file: &Path,
    ) -> Result<bool, String> {
        let dependencies = dependency_environment(dependency_file)?;
        if env::var_os(super::TRACE_RUN).is_some() {
            eprintln!(
                "    Cinder trace: compiler replay environment-dependencies={}",
                dependencies.len()
            );
        }
        if dependencies.len() > MAX_VALUES {
            return Ok(false);
        }
        let mut dependency_keys = BTreeSet::new();
        for (key, value) in dependencies {
            if !dependency_keys.insert(key.clone()) {
                return Ok(false);
            }
            let matches = match value {
                Some(value) => self
                    .dependency_environment_values
                    .binary_search(&environment_value_fingerprint(
                        &self.environment_fingerprint_salt,
                        key.as_os_str(),
                        &value,
                    ))
                    .is_ok(),
                None => self
                    .dependency_environment_absences
                    .binary_search(&environment_key_fingerprint(
                        &self.environment_fingerprint_salt,
                        key.as_os_str(),
                    ))
                    .is_ok(),
            };
            if !matches {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn artifact_path(&self) -> Option<PathBuf> {
        let emits_link = super::capture::rustc_list_options(&self.arguments, "--emit")
            .iter()
            .any(|values| values.split(',').any(|value| value == "link"));
        let emits_metadata = super::capture::rustc_list_options(&self.arguments, "--emit")
            .iter()
            .any(|values| values.split(',').any(|value| value == "metadata"));
        if !emits_link && !emits_metadata {
            return None;
        }
        let is_test_harness = self.arguments.iter().any(|argument| argument == "--test");
        let mut crate_types = super::capture::rustc_list_options(&self.arguments, "--crate-type")
            .into_iter()
            .flat_map(|values| values.split(','))
            .filter(|kind| {
                matches!(
                    *kind,
                    "bin" | "lib" | "rlib" | "staticlib" | "dylib" | "cdylib"
                )
            });
        let crate_type = crate_types
            .next()
            .or_else(|| is_test_harness.then_some("bin"))?;
        if crate_types.next().is_some() {
            return None;
        }
        let crate_name = super::capture::rustc_option(&self.arguments, "--crate-name")?;
        let out_directory =
            PathBuf::from(super::capture::rustc_option(&self.arguments, "--out-dir")?);
        let extra_filename =
            super::capture::rustc_codegen_option(&self.arguments, "extra-filename")
                .unwrap_or_default();
        if emits_link {
            super::capture::linked_artifact_name(crate_name, extra_filename, crate_type)
                .ok()
                .map(|name| out_directory.join(name))
        } else {
            Some(out_directory.join(super::capture::metadata_artifact_name(
                crate_name,
                extra_filename,
            )))
        }
    }
}

#[cfg(target_os = "macos")]
fn environment_fingerprint_salt() -> Option<[u8; 32]> {
    let mut salt = [0_u8; 32];
    fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut salt)
        .ok()?;
    Some(salt)
}

fn environment_key_fingerprint(salt: &[u8; 32], key: &OsStr) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"CINDER-COMPILER-ENVIRONMENT-KEY-1");
    digest.update(salt);
    digest.update((key.as_bytes().len() as u64).to_le_bytes());
    digest.update(key.as_bytes());
    digest.finalize().into()
}

fn environment_value_fingerprint(salt: &[u8; 32], key: &OsStr, value: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"CINDER-COMPILER-ENVIRONMENT-VALUE-1");
    digest.update(salt);
    digest.update((key.as_bytes().len() as u64).to_le_bytes());
    digest.update(key.as_bytes());
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
    digest.finalize().into()
}

fn dependency_environment(path: &Path) -> Result<DependencyEnvironment, String> {
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "could not inspect compiler dependency file {}: {error}",
            path.display()
        )
    })?;
    if metadata.len() > MAX_DEPENDENCY_FILE_BYTES {
        return Err("compiler dependency file is too large".to_owned());
    }
    let contents = fs::read(path).map_err(|error| {
        format!(
            "could not read compiler dependency file {}: {error}",
            path.display()
        )
    })?;
    contents
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            line.strip_suffix(b"\r")
                .unwrap_or(line)
                .strip_prefix(b"# env-dep:")
        })
        .map(|dependency| {
            let (key, value) = match dependency.iter().position(|byte| *byte == b'=') {
                Some(separator) => (&dependency[..separator], Some(&dependency[separator + 1..])),
                None => (dependency, None),
            };
            Ok((
                OsString::from_vec(unescape_dependency_environment(key)?),
                value.map(unescape_dependency_environment).transpose()?,
            ))
        })
        .collect()
}

fn unescape_dependency_environment(value: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'\\' {
            decoded.push(value[index]);
            index += 1;
            continue;
        }
        let escaped = value
            .get(index + 1)
            .ok_or_else(|| "compiler dependency environment has a trailing escape".to_owned())?;
        decoded.push(match escaped {
            b'n' => b'\n',
            b'r' => b'\r',
            b'\\' => b'\\',
            _ => {
                return Err("compiler dependency environment has an unsupported escape".to_owned());
            }
        });
        index += 2;
    }
    Ok(decoded)
}

fn cargo_compiler_environment(key: &OsStr) -> bool {
    key.to_str().is_some_and(|key| {
        FIXED_CARGO_COMPILER_ENVIRONMENT.contains(&key)
            || key.starts_with("CARGO_BIN_EXE_")
            || key.starts_with("CARGO_PKG_")
    })
}

fn inherited_environment_matches(key: &OsStr, value: Option<&[u8]>) -> bool {
    if key.is_empty() || key.as_bytes().contains(&0) || key.as_bytes().contains(&b'=') {
        return false;
    }
    match (env::var_os(key), value) {
        (Some(inherited), Some(value)) => inherited.as_bytes() == value,
        (None, None) => true,
        _ => false,
    }
}

pub(super) fn write_compiler_recipe(path: &Path, recipe: &CompilerRecipe) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create compiler recipe: {error}"))?;
    file.write_all(RECIPE_MAGIC)
        .map_err(|error| format!("could not write compiler recipe: {error}"))?;
    write_value(&mut file, recipe.executable.as_os_str())?;
    write_value(&mut file, recipe.working_directory.as_os_str())?;
    write_count(&mut file, recipe.arguments.len())?;
    for argument in &recipe.arguments {
        write_value(&mut file, argument)?;
    }
    write_count(&mut file, recipe.environment.len())?;
    for (key, value) in &recipe.environment {
        write_value(&mut file, key)?;
        write_value(&mut file, value)?;
    }
    file.write_all(&recipe.environment_fingerprint_salt)
        .map_err(|error| format!("could not write compiler recipe: {error}"))?;
    write_fingerprints(&mut file, &recipe.dependency_environment_absences)?;
    write_fingerprints(&mut file, &recipe.dependency_environment_values)?;
    Ok(())
}

pub(super) fn read_compiler_recipe(path: &Path) -> Result<CompilerRecipe, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("could not open compiler recipe: {error}"))?;
    let mut magic = [0_u8; 8];
    file.read_exact(&mut magic)
        .map_err(|error| format!("could not read compiler recipe: {error}"))?;
    if &magic != RECIPE_MAGIC {
        return Err("compiler recipe has an unsupported format".to_owned());
    }
    let executable = PathBuf::from(read_value(&mut file)?);
    let working_directory = PathBuf::from(read_value(&mut file)?);
    if !executable.is_absolute() || !working_directory.is_absolute() {
        return Err("compiler recipe contains a relative execution path".to_owned());
    }
    let argument_count = read_count(&mut file)?;
    let mut arguments = Vec::with_capacity(argument_count);
    for _ in 0..argument_count {
        arguments.push(read_value(&mut file)?);
    }
    let environment_count = read_count(&mut file)?;
    let mut environment = Vec::with_capacity(environment_count);
    let mut environment_keys = BTreeSet::new();
    for _ in 0..environment_count {
        let key = read_value(&mut file)?;
        if !cargo_compiler_environment(&key) || !environment_keys.insert(key.clone()) {
            return Err(
                "compiler recipe contains an unsupported or duplicate environment key".to_owned(),
            );
        }
        environment.push((key, read_value(&mut file)?));
    }
    let mut environment_fingerprint_salt = [0_u8; 32];
    file.read_exact(&mut environment_fingerprint_salt)
        .map_err(|error| format!("could not read compiler recipe: {error}"))?;
    let dependency_environment_absences = read_fingerprints(&mut file)?;
    let dependency_environment_values = read_fingerprints(&mut file)?;
    if dependency_environment_absences
        .len()
        .checked_add(dependency_environment_values.len())
        .is_none_or(|count| count > MAX_VALUES)
    {
        return Err("compiler recipe has too many environment fingerprints".to_owned());
    }
    let recipe = CompilerRecipe {
        executable,
        working_directory,
        arguments,
        environment,
        environment_fingerprint_salt,
        dependency_environment_absences,
        dependency_environment_values,
    };
    if recipe
        .artifact_path()
        .is_none_or(|artifact| !artifact.is_absolute())
    {
        return Err("compiler recipe has an unsupported output layout".to_owned());
    }
    let mut trailing = [0_u8; 1];
    if file
        .read(&mut trailing)
        .map_err(|error| format!("could not finish reading compiler recipe: {error}"))?
        != 0
    {
        return Err("compiler recipe contains trailing data".to_owned());
    }
    Ok(recipe)
}

fn write_fingerprints(file: &mut fs::File, values: &[[u8; 32]]) -> Result<(), String> {
    write_count(file, values.len())?;
    for value in values {
        file.write_all(value)
            .map_err(|error| format!("could not write compiler recipe: {error}"))?;
    }
    Ok(())
}

fn read_fingerprints(file: &mut fs::File) -> Result<Vec<[u8; 32]>, String> {
    let count = read_count(file)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let mut value = [0_u8; 32];
        file.read_exact(&mut value)
            .map_err(|error| format!("could not read compiler recipe: {error}"))?;
        if values.last().is_some_and(|previous| previous >= &value) {
            return Err("compiler recipe fingerprints are not ordered".to_owned());
        }
        values.push(value);
    }
    Ok(values)
}

fn write_count(file: &mut fs::File, count: usize) -> Result<(), String> {
    let count = u32::try_from(count).map_err(|_| "compiler recipe has too many values")?;
    file.write_all(&count.to_le_bytes())
        .map_err(|error| format!("could not write compiler recipe: {error}"))
}

fn read_count(file: &mut fs::File) -> Result<usize, String> {
    let mut count = [0_u8; 4];
    file.read_exact(&mut count)
        .map_err(|error| format!("could not read compiler recipe: {error}"))?;
    let count = u32::from_le_bytes(count) as usize;
    if count > MAX_VALUES {
        return Err("compiler recipe has too many values".to_owned());
    }
    Ok(count)
}

fn write_value(file: &mut fs::File, value: &StdOsStr) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;

    let value = value.as_bytes();
    let length = u32::try_from(value.len()).map_err(|_| "compiler recipe value is too long")?;
    file.write_all(&length.to_le_bytes())
        .and_then(|()| file.write_all(value))
        .map_err(|error| format!("could not write compiler recipe: {error}"))
}

fn read_value(file: &mut fs::File) -> Result<OsString, String> {
    let mut length = [0_u8; 4];
    file.read_exact(&mut length)
        .map_err(|error| format!("could not read compiler recipe: {error}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_VALUE_BYTES {
        return Err("compiler recipe value is too long".to_owned());
    }
    let mut value = vec![0_u8; length];
    file.read_exact(&mut value)
        .map_err(|error| format!("could not read compiler recipe: {error}"))?;
    Ok(OsString::from_vec(value))
}

/// Runs an exact captured compiler recipe and accepts only a silent success.
///
/// Cargo requests internal JSON artifact notifications from rustc. Those are
/// safe to suppress. Warnings, diagnostics, malformed output, and failures are
/// discarded and reported as a normal miss so Cargo can produce canonical
/// user-facing output.
pub(super) fn replay_compiler(
    recipe: &CompilerRecipe,
    private_directory: &Path,
) -> Result<bool, String> {
    if !recipe.executable.is_file() || !recipe.working_directory.is_dir() {
        return Ok(false);
    }
    fs::create_dir_all(private_directory)
        .map_err(|error| format!("could not create compiler replay directory: {error}"))?;
    super::make_private_directory(private_directory)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let stem = format!("compiler-output-{}-{nonce}", std::process::id());
    let stdout_path = private_directory.join(format!("{stem}.stdout"));
    let stderr_path = private_directory.join(format!("{stem}.stderr"));
    let result = (|| {
        let stdout = private_output(&stdout_path)?;
        let stderr = private_output(&stderr_path)?;
        let mut command = Command::new(&recipe.executable);
        command
            .current_dir(&recipe.working_directory)
            .args(&recipe.arguments)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        for key in FIXED_CARGO_COMPILER_ENVIRONMENT {
            command.env_remove(key);
        }
        for (key, _) in env::vars_os().filter(|(key, _)| {
            key.to_str().is_some_and(|key| {
                key.starts_with("CARGO_BIN_EXE_") || key.starts_with("CARGO_PKG_")
            })
        }) {
            command.env_remove(key);
        }
        command.envs(
            recipe
                .environment
                .iter()
                .map(|(key, value)| (key.as_os_str(), value.as_os_str())),
        );
        crate::command::restore_runtime_environment(&mut command);
        let status = command
            .status()
            .map_err(|error| format!("could not replay Cargo compiler recipe: {error}"))?;
        if !status.success() {
            if env::var_os(super::TRACE_RUN).is_some() {
                let _ = compiler_output_is_internal(&stdout_path, "failed-stdout");
                let _ = compiler_output_is_internal(&stderr_path, "failed-stderr");
                eprintln!(
                    "    Cinder trace: compiler replay exit-code={}",
                    status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |code| code.to_string())
                );
            }
            return Ok(false);
        }
        Ok(compiler_output_is_internal(&stdout_path, "stdout")?
            && compiler_output_is_internal(&stderr_path, "stderr")?)
    })();
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
    result
}

fn private_output(path: &Path) -> Result<fs::File, String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("could not create compiler replay output: {error}"))
}

fn compiler_output_is_internal(path: &Path, stream: &str) -> Result<bool, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect compiler replay output: {error}"))?;
    if metadata.len() > MAX_DIAGNOSTIC_BYTES {
        trace_output(stream, metadata.len(), ["oversized"]);
        return Ok(false);
    }
    let mut output = String::new();
    fs::File::open(path)
        .and_then(|mut file| file.read_to_string(&mut output))
        .map_err(|error| format!("could not read compiler replay output: {error}"))?;
    let mut kinds = BTreeSet::new();
    let accepted =
        output.lines().all(
            |line| match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => {
                    let kind = value
                        .get("$message_type")
                        .and_then(|value| value.as_str())
                        .unwrap_or("json-without-message-type");
                    kinds.insert(kind.to_owned());
                    kind == "artifact"
                }
                Err(_) => {
                    kinds.insert("non-json".to_owned());
                    false
                }
            },
        );
    trace_output(stream, metadata.len(), kinds.iter().map(String::as_str));
    Ok(accepted)
}

fn trace_output<'a>(stream: &str, bytes: u64, kinds: impl IntoIterator<Item = &'a str>) {
    if env::var_os(super::TRACE_RUN).is_some() {
        let kinds = kinds.into_iter().collect::<Vec<_>>().join(",");
        eprintln!("    Cinder trace: compiler replay {stream} bytes={bytes} message-types={kinds}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn recipe_fingerprints_do_not_persist_arbitrary_environment() {
        let arguments = [
            "--crate-name",
            "app",
            "src/lib.rs",
            "--crate-type",
            "lib",
            "--emit",
            "metadata",
            "--out-dir",
            "/workspace/target",
        ]
        .map(OsString::from)
        .into();
        let mut recipe = CompilerRecipe::from_observed(
            PathBuf::from("/toolchain/bin/rustc"),
            PathBuf::from("/workspace"),
            arguments,
        )
        .unwrap();
        assert!(recipe.environment.is_empty());
        recipe
            .dependency_environment_values
            .push(environment_value_fingerprint(
                &recipe.environment_fingerprint_salt,
                OsStr::new("PRIVATE_TOKEN"),
                b"discarded",
            ));

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "cinder-observed-environment-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let state = root.join("recipe");
        write_compiler_recipe(&state, &recipe).unwrap();
        let persisted = fs::read(&state).unwrap();
        assert!(!persisted.windows(13).any(|bytes| bytes == b"PRIVATE_TOKEN"));
        assert!(!persisted.windows(9).any(|bytes| bytes == b"discarded"));

        let dependency_file = root.join("selected.d");
        fs::write(
            &dependency_file,
            b"artifact: source.rs\n# env-dep:PRIVATE_TOKEN=discarded\n",
        )
        .unwrap();
        let decoded = read_compiler_recipe(&state).unwrap();
        assert!(
            decoded
                .replayed_dependency_environment_matches(&dependency_file)
                .unwrap()
        );
        fs::write(
            &dependency_file,
            b"artifact: source.rs\n# env-dep:PRIVATE_TOKEN=different\n",
        )
        .unwrap();
        assert!(
            !recipe
                .replayed_dependency_environment_matches(&dependency_file)
                .unwrap()
        );
        fs::write(
            &dependency_file,
            b"artifact: source.rs\n# env-dep:NEW_UNSET_VARIABLE\n",
        )
        .unwrap();
        assert!(
            !recipe
                .replayed_dependency_environment_matches(&dependency_file)
                .unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recipe_round_trip_rejects_uncontrolled_environment() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("cinder-recipe-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let path = root.join("recipe");
        let recipe = CompilerRecipe {
            executable: PathBuf::from("/usr/bin/rustc"),
            working_directory: PathBuf::from("/workspace"),
            arguments: [
                "--crate-name",
                "app",
                "--crate-type",
                "lib",
                "--emit",
                "metadata",
                "--out-dir",
                "/tmp",
            ]
            .map(OsString::from)
            .into(),
            environment: vec![(OsString::from("CARGO_CRATE_NAME"), OsString::from("app"))],
            environment_fingerprint_salt: [1; 32],
            dependency_environment_absences: Vec::new(),
            dependency_environment_values: Vec::new(),
        };
        write_compiler_recipe(&path, &recipe).unwrap();
        let decoded = read_compiler_recipe(&path).unwrap();
        assert_eq!(decoded.executable, recipe.executable);
        assert_eq!(decoded.working_directory, recipe.working_directory);
        assert_eq!(decoded.arguments, recipe.arguments);
        assert_eq!(decoded.environment, recipe.environment);

        let mut obsolete = fs::read(&path).unwrap();
        obsolete[..RECIPE_MAGIC.len()].copy_from_slice(b"CNDRCP05");
        fs::write(&path, obsolete).unwrap();
        assert!(read_compiler_recipe(&path).is_err());

        write_compiler_recipe(&path, &recipe).unwrap();
        let mut trailing = fs::read(&path).unwrap();
        trailing.push(0);
        fs::write(&path, trailing).unwrap();
        assert!(read_compiler_recipe(&path).is_err());

        write_compiler_recipe(&path, &recipe).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        let offset = bytes
            .windows("CARGO_CRATE_NAME".len())
            .position(|window| window == b"CARGO_CRATE_NAME")
            .unwrap();
        bytes[offset..offset + "CARGO_CRATE_NAME".len()].copy_from_slice(b"CINDER_BAD_VALUE");
        fs::write(&path, bytes).unwrap();
        assert!(read_compiler_recipe(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_environment_restores_only_cargo_generated_values() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("cinder-recipe-env-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let dependency_file = root.join("selected.d");
        fs::write(
            &dependency_file,
            b"artifact: source.rs\n# env-dep:CARGO_PKG_VERSION=1.2.3\n# env-dep:CINDER_REPLAY_TEST_UNSET\n",
        )
        .unwrap();
        let mut recipe = CompilerRecipe {
            executable: PathBuf::from("/usr/bin/rustc"),
            working_directory: PathBuf::from("/workspace"),
            arguments: vec![OsString::from("--crate-name"), OsString::from("app")],
            environment: Vec::new(),
            environment_fingerprint_salt: [1; 32],
            dependency_environment_absences: Vec::new(),
            dependency_environment_values: Vec::new(),
        };
        recipe
            .bind_dependency_environment(&dependency_file, None)
            .unwrap();
        assert_eq!(
            recipe.environment,
            vec![(OsString::from("CARGO_PKG_VERSION"), OsString::from("1.2.3"))]
        );
        assert!(
            recipe
                .replayed_dependency_environment_matches(&dependency_file)
                .unwrap()
        );
        fs::write(
            &dependency_file,
            b"artifact: source.rs\n# env-dep:CARGO_PKG_VERSION=1.2.3\n# env-dep:CARGO_PKG_VERSION=1.2.3\n",
        )
        .unwrap();
        assert!(
            !recipe
                .replayed_dependency_environment_matches(&dependency_file)
                .unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_environment_restores_cargo_target_tmpdir() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "cinder-recipe-target-tmpdir-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let dependency_file = root.join("selected.d");
        let target_tmpdir = root.join("target/tmp");
        fs::write(
            &dependency_file,
            format!(
                "artifact: source.rs\n# env-dep:CARGO_TARGET_TMPDIR={}\n",
                target_tmpdir.display()
            ),
        )
        .unwrap();
        let mut recipe = CompilerRecipe {
            executable: PathBuf::from("/usr/bin/rustc"),
            working_directory: PathBuf::from("/workspace"),
            arguments: vec![OsString::from("--crate-name"), OsString::from("app")],
            environment: Vec::new(),
            environment_fingerprint_salt: [1; 32],
            dependency_environment_absences: Vec::new(),
            dependency_environment_values: Vec::new(),
        };
        recipe
            .bind_dependency_environment(&dependency_file, None)
            .unwrap();
        assert_eq!(
            recipe.environment,
            vec![(
                OsString::from("CARGO_TARGET_TMPDIR"),
                target_tmpdir.into_os_string()
            )]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_environment_rejects_a_value_cinder_cannot_reconstruct() {
        let key = "CINDER_REPLAY_TEST_UNINHERITED_VALUE";
        assert!(env::var_os(key).is_none());
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "cinder-recipe-unreconstructable-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let dependency_file = root.join("selected.d");
        fs::write(
            &dependency_file,
            format!("artifact: source.rs\n# env-dep:{key}=cargo-only\n"),
        )
        .unwrap();
        let mut recipe = CompilerRecipe {
            executable: PathBuf::from("/usr/bin/rustc"),
            working_directory: PathBuf::from("/workspace"),
            arguments: vec![OsString::from("--crate-name"), OsString::from("app")],
            environment: Vec::new(),
            environment_fingerprint_salt: [1; 32],
            dependency_environment_absences: Vec::new(),
            dependency_environment_values: Vec::new(),
        };
        assert!(
            recipe
                .bind_dependency_environment(&dependency_file, None)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_environment_unescapes_rustc_values_and_rejects_unknown_escapes() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "cinder-recipe-escaped-environment-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let dependency_file = root.join("selected.d");
        fs::write(
            &dependency_file,
            b"artifact: source.rs\n# env-dep:CARGO_PKG_DESCRIPTION=line\\\\path\\nnext\\rend\n",
        )
        .unwrap();
        let mut recipe = CompilerRecipe {
            executable: PathBuf::from("/usr/bin/rustc"),
            working_directory: PathBuf::from("/workspace"),
            arguments: vec![OsString::from("--crate-name"), OsString::from("app")],
            environment: Vec::new(),
            environment_fingerprint_salt: [1; 32],
            dependency_environment_absences: Vec::new(),
            dependency_environment_values: Vec::new(),
        };
        recipe
            .bind_dependency_environment(&dependency_file, None)
            .unwrap();
        assert_eq!(
            recipe.environment,
            vec![(
                OsString::from("CARGO_PKG_DESCRIPTION"),
                OsString::from("line\\path\nnext\rend")
            )]
        );

        fs::write(
            &dependency_file,
            b"artifact: source.rs\n# env-dep:CARGO_PKG_DESCRIPTION=bad\\q\n",
        )
        .unwrap();
        assert!(
            recipe
                .bind_dependency_environment(&dependency_file, None)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_environment_binds_selected_build_script_output() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("cinder-recipe-out-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let dependency_file = root.join("selected.d");
        fs::write(&dependency_file, b"artifact: source.rs\n").unwrap();
        let out_directory = root.join("out");
        let mut recipe = CompilerRecipe {
            executable: PathBuf::from("/usr/bin/rustc"),
            working_directory: PathBuf::from("/workspace"),
            arguments: vec![OsString::from("--crate-name"), OsString::from("app")],
            environment: Vec::new(),
            environment_fingerprint_salt: [1; 32],
            dependency_environment_absences: Vec::new(),
            dependency_environment_values: Vec::new(),
        };
        recipe
            .bind_dependency_environment(&dependency_file, Some(&out_directory))
            .unwrap();
        assert_eq!(
            recipe.environment,
            vec![(OsString::from("OUT_DIR"), out_directory.into_os_string())]
        );
        fs::remove_dir_all(root).unwrap();
    }
}
