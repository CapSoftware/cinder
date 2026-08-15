//! Invocation, environment, compiler, and Cargo fingerprint identity.

use super::{
    BTreeMap, BTreeSet, CargoOutputs, DISABLE_FAST_BUILD, DISABLE_FAST_CHECK, DISABLE_FAST_RUN,
    DISABLE_FAST_TEST, Digest, LaunchPolicy, OsStr, OsStrExt, OsString, OsStringExt, Path, PathBuf,
    RUN_CONTEXT_FILE, Sha256, absolute_path, artifact_file_identity, env, fs, io,
};

#[cfg(test)]
pub fn run_context(arguments: &[OsString], launch_policy: LaunchPolicy) -> Vec<u8> {
    run_context_with_cargo(
        arguments,
        launch_policy,
        env::var_os("CINDER_REAL_CARGO")
            .as_deref()
            .unwrap_or(OsStr::new("cargo")),
    )
}

pub fn run_context_with_cargo(
    arguments: &[OsString],
    launch_policy: LaunchPolicy,
    cargo: &OsStr,
) -> Vec<u8> {
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
    append_tool_identity(&mut context, cargo);
    append_tool_identity(
        &mut context,
        env::var_os("RUSTC")
            .as_deref()
            .unwrap_or(OsStr::new("rustc")),
    );
    append_rustup_identity(&mut context);
    context.finalize().to_vec()
}

pub(super) fn environment_affects_context(key: &OsStr) -> bool {
    !matches!(
        key.to_str(),
        Some(
            "_" | RUN_CONTEXT_FILE
                | DISABLE_FAST_RUN
                | DISABLE_FAST_BUILD
                | DISABLE_FAST_CHECK
                | DISABLE_FAST_TEST
        )
    )
}

pub(super) fn bind_observed_shell_environment(
    context: &[u8],
    observes_underscore: bool,
) -> Vec<u8> {
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

pub(super) fn dependency_observes_environment(path: &Path, key: &[u8]) -> Result<bool, String> {
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

pub(super) fn encoded_dependency_observes_environment(
    path: &Path,
    key: &[u8],
) -> Result<bool, String> {
    let contents = fs::read(path).map_err(|error| {
        format!(
            "could not inspect Cargo environment dependencies {}: {error}",
            path.display()
        )
    })?;
    parse_encoded_dependency_environment(&contents, key).ok_or_else(|| {
        format!(
            "Cargo environment dependency data is invalid or unsupported: {}",
            path.display()
        )
    })
}

pub(super) fn parse_encoded_dependency_environment(
    mut contents: &[u8],
    key: &[u8],
) -> Option<bool> {
    let _marker_length = take_encoded_u32(&mut contents)?;
    if take_encoded_u8(&mut contents)? != u8::MAX || take_encoded_u8(&mut contents)? != 1 {
        return None;
    }
    let file_count = take_encoded_u32(&mut contents)? as usize;
    if file_count > contents.len() {
        return None;
    }
    for _ in 0..file_count {
        if !matches!(take_encoded_u8(&mut contents)?, 0 | 1) {
            return None;
        }
        take_encoded_bytes(&mut contents)?;
        if take_encoded_u8(&mut contents)? != 0 {
            take_encoded_u64(&mut contents)?;
            take_encoded_bytes(&mut contents)?;
        }
    }
    let environment_count = take_encoded_u32(&mut contents)? as usize;
    if environment_count > contents.len() {
        return None;
    }
    let mut observed = false;
    for _ in 0..environment_count {
        observed |= take_encoded_bytes(&mut contents)? == key;
        match take_encoded_u8(&mut contents)? {
            0 => {}
            1 => {
                take_encoded_bytes(&mut contents)?;
            }
            _ => return None,
        }
    }
    Some(observed)
}

pub(super) fn take_encoded_u8(contents: &mut &[u8]) -> Option<u8> {
    let (value, remaining) = contents.split_first()?;
    *contents = remaining;
    Some(*value)
}

pub(super) fn take_encoded_u32(contents: &mut &[u8]) -> Option<u32> {
    let (value, remaining) = contents.split_at_checked(4)?;
    *contents = remaining;
    Some(u32::from_le_bytes(value.try_into().ok()?))
}

pub(super) fn take_encoded_u64(contents: &mut &[u8]) -> Option<u64> {
    let (value, remaining) = contents.split_at_checked(8)?;
    *contents = remaining;
    Some(u64::from_le_bytes(value.try_into().ok()?))
}

pub(super) fn take_encoded_bytes<'a>(contents: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = take_encoded_u32(contents)? as usize;
    let (value, remaining) = contents.split_at_checked(length)?;
    *contents = remaining;
    Some(value)
}

pub(super) fn fingerprint_dependency_observes_environment(
    fingerprint: &Path,
    key: &[u8],
) -> Result<Option<bool>, String> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(fingerprint).map_err(|error| {
        format!(
            "could not inspect Cargo fingerprint {}: {error}",
            fingerprint.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect Cargo fingerprint {}: {error}",
                fingerprint.display()
            )
        })?;
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry.file_name().as_bytes().starts_with(b"dep-")
        {
            candidates.push(entry.path());
        }
    }
    match candidates.as_slice() {
        [] => Ok(None),
        [path] => encoded_dependency_observes_environment(path, key).map(Some),
        _ => Err(format!(
            "Cargo fingerprint has ambiguous dependency data: {}",
            fingerprint.display()
        )),
    }
}

pub(super) fn compiler_unit_graph(
    outputs: &CargoOutputs,
    key: &[u8],
) -> Result<(bool, Vec<PathBuf>), String> {
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
    let mut observed = false;

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
            observed |= build_script_output_observes_environment(&output, key)?;
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
            observed |= dependency_observes_environment(dependency_file, key)?;
        } else if !runs_build_script {
            match fingerprint_dependency_observes_environment(&fingerprint, key)? {
                Some(value) => observed |= value,
                None => {
                    return Err(format!(
                        "Cargo unit has no dependency data: {}",
                        fingerprint.display()
                    ));
                }
            }
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
    Ok((observed, visited.into_iter().collect()))
}

pub(super) fn build_script_output_observes_environment(
    path: &Path,
    key: &[u8],
) -> Result<bool, String> {
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

pub(super) fn cargo_fingerprint_value_index(
    root: &Path,
) -> Result<BTreeMap<u64, Vec<PathBuf>>, String> {
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

pub(super) fn cargo_fingerprint_value(contents: &[u8]) -> Option<u64> {
    if contents.len() != 16 {
        return None;
    }
    let mut bytes = [0_u8; 8];
    for (destination, pair) in bytes.iter_mut().zip(contents.chunks_exact(2)) {
        *destination = (hexadecimal_nibble(pair[0])? << 4) | hexadecimal_nibble(pair[1])?;
    }
    Some(u64::from_le_bytes(bytes))
}

pub(super) fn hexadecimal_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(super) fn cargo_dependency_file_index(
    root: &Path,
) -> Result<BTreeMap<String, PathBuf>, String> {
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

pub(super) fn index_cargo_dependency_files(
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

pub(super) fn cargo_fingerprint_descriptor(root: &Path) -> Result<PathBuf, String> {
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

pub(super) fn append_tool_identity(context: &mut Sha256, executable: &OsStr) {
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

pub(super) fn resolve_executable(executable: &OsStr) -> Option<PathBuf> {
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

pub(super) fn append_rustup_identity(context: &mut Sha256) {
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

pub(super) fn append_path_identity(context: &mut Sha256, path: &Path, include_contents: bool) {
    append_context_value(context, path.as_os_str().as_bytes());
    match artifact_file_identity(path) {
        Ok(identity) => {
            context.update([1]);
            context.update(identity.size.to_le_bytes());
            context.update(identity.modified_ns.to_le_bytes());
            context.update(identity.device.to_le_bytes());
            context.update(identity.inode.to_le_bytes());
            context.update(identity.changed_seconds.to_le_bytes());
            context.update(identity.changed_nanoseconds.to_le_bytes());
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

pub(super) fn append_context_value(context: &mut Sha256, value: &[u8]) {
    context.update((value.len() as u64).to_le_bytes());
    context.update(value);
}
