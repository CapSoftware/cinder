//! Build inputs, exact Cargo outputs, and persisted validation formats.

use super::{
    ArtifactFileIdentity, ArtifactReceipt, BTreeSet, CargoOutputs, Digest, InputEntry, MetadataExt,
    OsStr, OsStrExt, OsString, OsStringExt, Path, PathBuf, Read, Sha256, UNIX_EPOCH, Write,
    absolute_path, append_context_value, env, fs, io,
};

pub(super) fn artifact_metadata(path: &Path) -> Result<(u64, u128), String> {
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

pub(super) fn artifact_file_identity(path: &Path) -> Result<ArtifactFileIdentity, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect artifact {}: {error}", path.display()))?;
    artifact_file_identity_from_metadata(&metadata)
}

pub(super) fn artifact_file_identity_from_metadata(
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

pub(super) fn artifact_identity(path: &Path) -> Result<(ArtifactFileIdentity, [u8; 32]), String> {
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

pub(super) fn sha256_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open artifact {}: {error}", path.display()))?;
    sha256_reader(&mut file, path)
}

pub(super) fn sha256_reader(file: &mut fs::File, path: &Path) -> Result<[u8; 32], String> {
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

pub(super) fn input_entries_are_unchanged(inputs: &[InputEntry]) -> bool {
    inputs.iter().all(|input| {
        artifact_file_identity(&input.path).is_ok_and(|identity| identity == input.identity)
    })
}

pub(super) fn input_entries_match_revision(inputs: &[InputEntry]) -> Result<bool, String> {
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

pub(super) fn input_identity(path: &Path) -> Result<(ArtifactFileIdentity, [u8; 32]), String> {
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

pub(super) fn input_digest(path: &Path) -> Result<[u8; 32], String> {
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

pub(super) fn project_may_have_build_script(
    directory: &Path,
    sources: &[PathBuf],
) -> Result<bool, String> {
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

pub(super) fn toml_key_is(line: &str, expected: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(key, _)| key.trim().trim_matches(['\'', '"']) == expected)
}

pub(super) const INPUTS_MAGIC: &[u8; 8] = b"CNDI0003";

pub(super) fn build_inputs(
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

pub(super) fn add_cargo_control_inputs(directory: &Path, paths: &mut BTreeSet<PathBuf>) {
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

pub(super) fn add_project_rust_inputs(
    directory: &Path,
    artifact: &Path,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    let target = cargo_target_directory(artifact);
    collect_project_rust_inputs(directory, target, paths)
}

pub(super) fn collect_project_rust_inputs(
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

pub(super) fn add_build_script_inputs(
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
        let target = cargo_target_directory(&receipt.artifact)
            .and_then(|target| fs::canonicalize(target).ok());
        collect_default_build_script_tree(manifest_directory, target.as_deref(), paths, 100_000)?;
        return Ok(());
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

pub(super) fn collect_default_build_script_tree(
    directory: &Path,
    target: Option<&Path>,
    paths: &mut BTreeSet<PathBuf>,
    remaining: usize,
) -> Result<usize, String> {
    if remaining == 0 {
        return Err(
            "build-script package contains more than 100,000 filesystem entries".to_owned(),
        );
    }
    let directory = fs::canonicalize(directory).map_err(|error| {
        format!(
            "could not resolve build-script package {}: {error}",
            directory.display()
        )
    })?;
    if target.is_some_and(|target| directory.starts_with(target)) {
        return Ok(remaining);
    }
    paths.insert(directory.clone());
    let mut remaining = remaining - 1;
    for entry in fs::read_dir(&directory).map_err(|error| {
        format!(
            "could not inspect build-script package {}: {error}",
            directory.display()
        )
    })? {
        if remaining == 0 {
            return Err(
                "build-script package contains more than 100,000 filesystem entries".to_owned(),
            );
        }
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect build-script package {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        if target.is_some_and(|target| path.starts_with(target))
            || entry.file_name() == OsStr::new(".git")
        {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "could not inspect build-script input {}: {error}",
                path.display()
            )
        })?;
        if file_type.is_dir() {
            remaining = collect_default_build_script_tree(&path, target, paths, remaining)?;
        } else {
            paths.insert(path);
            remaining -= 1;
        }
    }
    Ok(remaining)
}

pub(super) fn collect_watched_tree(
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

pub(super) fn build_source_paths(
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

pub(super) fn primary_dependency_file(artifact: &Path) -> Result<PathBuf, String> {
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

pub(super) fn cargo_outputs_for_artifact(
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
        unit_fingerprints: Vec::new(),
    };
    outputs
        .are_available()
        .then_some(outputs)
        .ok_or_else(|| "Cargo's exact hashed outputs are incomplete".to_owned())
}

pub(super) fn cargo_fingerprint_directory(profile: &Path, hash: &str) -> Result<PathBuf, String> {
    let profile = profile
        .ancestors()
        .find(|directory| directory.join(".fingerprint").is_dir())
        .ok_or_else(|| {
            format!(
                "Cargo profile has no fingerprint directory: {}",
                profile.display()
            )
        })?;
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

pub(super) fn cargo_profile_directory(artifact: &Path) -> Option<&Path> {
    artifact
        .parent()?
        .ancestors()
        .find(|directory| directory.join(".fingerprint").is_dir())
}

impl CargoOutputs {
    pub(super) fn are_available(&self) -> bool {
        self.dependency_file.is_file()
            && self.artifact.is_file()
            && self.fingerprint.is_dir()
            && self.unit_fingerprints.iter().all(|path| path.is_dir())
    }
}

pub(super) fn cargo_target_directory(artifact: &Path) -> Option<&Path> {
    artifact.ancestors().find(|ancestor| {
        ancestor.join(".rustc_info.json").is_file() || ancestor.join("CACHEDIR.TAG").is_file()
    })
}

pub(super) fn dependency_paths(
    directory: &Path,
    dependency_file: &Path,
) -> Result<BTreeSet<PathBuf>, String> {
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

pub(super) fn resolve_dependency_path(directory: &Path, path: &Path) -> Result<PathBuf, String> {
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

pub(super) fn makefile_words(bytes: &[u8]) -> Vec<Vec<u8>> {
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

pub(super) fn write_inputs(path: &Path, inputs: &[InputEntry]) -> Result<(), String> {
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

pub(super) fn read_inputs(path: &Path) -> Result<Vec<InputEntry>, String> {
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

pub(super) const SOURCES_MAGIC: &[u8; 8] = b"CNDS0001";

pub(super) fn write_source_paths(path: &Path, sources: &[PathBuf]) -> Result<(), String> {
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

pub(super) fn read_source_paths(path: &Path) -> Result<Vec<PathBuf>, String> {
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

pub(super) const CARGO_OUTPUTS_MAGIC: &[u8; 8] = b"CNDO0002";

pub(super) fn write_cargo_outputs(path: &Path, outputs: &CargoOutputs) -> Result<(), String> {
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
    file.write_all(&(outputs.unit_fingerprints.len() as u64).to_le_bytes())
        .map_err(|error| format!("could not write Cargo output count: {error}"))?;
    for fingerprint in &outputs.unit_fingerprints {
        write_state_bytes(
            &mut file,
            fingerprint.as_os_str().as_bytes(),
            "Cargo fingerprint path",
        )?;
    }
    Ok(())
}

pub(super) fn read_cargo_outputs(path: &Path) -> Result<CargoOutputs, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open Cargo output state: {error}"))?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic)
        .map_err(|error| format!("could not read Cargo output state: {error}"))?;
    if &magic != CARGO_OUTPUTS_MAGIC {
        return Err("Cargo output state has an unsupported format".to_owned());
    }
    let dependency_file = read_cargo_output_path(&mut file)?;
    let artifact = read_cargo_output_path(&mut file)?;
    let fingerprint = read_cargo_output_path(&mut file)?;
    let mut count = [0; 8];
    file.read_exact(&mut count)
        .map_err(|error| format!("could not read Cargo fingerprint count: {error}"))?;
    let count = u64::from_le_bytes(count);
    if count > 100_000 {
        return Err("Cargo output state contains too many fingerprints".to_owned());
    }
    let mut unit_fingerprints = Vec::with_capacity(count as usize);
    for _ in 0..count {
        unit_fingerprints.push(read_cargo_output_path(&mut file)?);
    }
    Ok(CargoOutputs {
        dependency_file,
        artifact,
        fingerprint,
        unit_fingerprints,
    })
}

pub(super) fn read_cargo_output_path(file: &mut fs::File) -> Result<PathBuf, String> {
    let path = PathBuf::from(OsString::from_vec(read_state_bytes(
        file,
        "Cargo output path",
    )?));
    path.is_absolute()
        .then_some(path)
        .ok_or_else(|| "Cargo output state contains a relative path".to_owned())
}

pub(super) const RUNTIME_ENVIRONMENT_MAGIC: &[u8; 8] = b"CNDE0001";
pub(super) const RUNTIME_LINKER_ENVIRONMENT_KEYS: [&str; 3] =
    ["DYLD_FALLBACK_LIBRARY_PATH", "LD_LIBRARY_PATH", "LIBPATH"];

pub(super) fn runtime_linker_environment() -> Vec<(OsString, OsString)> {
    RUNTIME_LINKER_ENVIRONMENT_KEYS
        .iter()
        .filter_map(|key| env::var_os(key).map(|value| (OsString::from(key), value)))
        .collect()
}

pub(super) fn write_runtime_environment(
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

pub(super) fn write_state_bytes(
    file: &mut fs::File,
    value: &[u8],
    label: &str,
) -> Result<(), String> {
    let length = u32::try_from(value.len()).map_err(|_| format!("Cinder {label} is too long"))?;
    file.write_all(&length.to_le_bytes())
        .and_then(|()| file.write_all(value))
        .map_err(|error| format!("could not write Cinder {label}: {error}"))
}

pub(super) fn read_runtime_environment(path: &Path) -> Result<Vec<(OsString, OsString)>, String> {
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

pub(super) fn read_state_bytes(file: &mut fs::File, label: &str) -> Result<Vec<u8>, String> {
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
