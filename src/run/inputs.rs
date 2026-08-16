//! Build inputs, exact Cargo outputs, and persisted validation formats.

use super::{
    ArtifactFileIdentity, ArtifactReceipt, BTreeSet, BuildScriptOutput, CargoOutputEntry,
    CargoOutputs, Digest, InputEntry, MetadataExt, OsStr, OsStrExt, OsString, OsStringExt, Path,
    PathBuf, ProjectTopology, Read, Sha256, TopologyDirectory, UNIX_EPOCH, Write, absolute_path,
    append_context_value, env, fs, io,
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
        if package_may_have_build_script(package)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn package_may_have_build_script(package: &Path) -> Result<bool, String> {
    if package.join("build.rs").is_file() {
        return Ok(true);
    }
    let manifest = package.join("Cargo.toml");
    let contents = fs::read_to_string(&manifest)
        .map_err(|error| format!("could not inspect {}: {error}", manifest.display()))?;
    let mut in_package = false;
    for line in contents.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if (in_package && toml_key_is(line, "build")) || toml_key_is(line, "package.build") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn toml_key_is(line: &str, expected: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(key, _)| key.trim().trim_matches(['\'', '"']) == expected)
}

pub(super) const INPUTS_MAGIC: &[u8; 8] = b"CNDI0004";
const MAX_INPUT_STATE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_INPUT_ENTRIES: usize = 1_000_000;
const MAX_INPUT_PATH_BYTES: usize = 1024 * 1024;

pub(super) struct StateReader<'a> {
    remaining: &'a [u8],
}

impl<'a> StateReader<'a> {
    pub(super) const fn new(contents: &'a [u8]) -> Self {
        Self {
            remaining: contents,
        }
    }

    pub(super) fn take(&mut self, length: usize, label: &str) -> Result<&'a [u8], String> {
        if self.remaining.len() < length {
            return Err(format!("could not read {label}: state is truncated"));
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    pub(super) fn array<const N: usize>(&mut self, label: &str) -> Result<[u8; N], String> {
        self.take(N, label)?
            .try_into()
            .map_err(|_| format!("could not read {label}: state is truncated"))
    }

    pub(super) fn length_prefixed(
        &mut self,
        maximum: usize,
        label: &str,
    ) -> Result<&'a [u8], String> {
        let length = u32::from_le_bytes(self.array(label)?) as usize;
        if length > maximum {
            return Err(format!("Cinder {label} is too long"));
        }
        self.take(length, label)
    }

    pub(super) fn finish(self, label: &str) -> Result<(), String> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(format!("{label} contains trailing data"))
        }
    }
}

pub(super) fn read_bounded_state(
    path: &Path,
    maximum: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|error| format!("could not open {label}: {error}"))?;
    let size = file
        .metadata()
        .map_err(|error| format!("could not inspect {label}: {error}"))?
        .len();
    if size > maximum {
        return Err(format!("{label} is too large"));
    }
    let mut contents = Vec::with_capacity(size as usize);
    file.take(maximum + 1)
        .read_to_end(&mut contents)
        .map_err(|error| format!("could not read {label}: {error}"))?;
    if contents.len() as u64 > maximum {
        return Err(format!("{label} is too large"));
    }
    Ok(contents)
}

fn read_file_identity(
    reader: &mut StateReader<'_>,
    label: &str,
) -> Result<ArtifactFileIdentity, String> {
    Ok(ArtifactFileIdentity {
        size: u64::from_le_bytes(reader.array(label)?),
        modified_ns: u128::from_le_bytes(reader.array(label)?),
        device: u64::from_le_bytes(reader.array(label)?),
        inode: u64::from_le_bytes(reader.array(label)?),
        changed_seconds: i64::from_le_bytes(reader.array(label)?),
        changed_nanoseconds: i64::from_le_bytes(reader.array(label)?),
    })
}

/// Serializes the six identity fields in exactly the order `read_file_identity`
/// decodes them, followed by nothing; callers append their digest separately.
pub(super) fn write_file_identity(
    writer: &mut impl Write,
    identity: &ArtifactFileIdentity,
    label: &str,
) -> Result<(), String> {
    writer
        .write_all(&identity.size.to_le_bytes())
        .and_then(|()| writer.write_all(&identity.modified_ns.to_le_bytes()))
        .and_then(|()| writer.write_all(&identity.device.to_le_bytes()))
        .and_then(|()| writer.write_all(&identity.inode.to_le_bytes()))
        .and_then(|()| writer.write_all(&identity.changed_seconds.to_le_bytes()))
        .and_then(|()| writer.write_all(&identity.changed_nanoseconds.to_le_bytes()))
        .map_err(|error| format!("could not write Cinder {label}: {error}"))
}

pub(super) struct BuildInputGraph<'a> {
    pub(super) dependency_file: &'a Path,
    pub(super) unit_dependency_files: &'a [PathBuf],
    pub(super) encoded_dependency_paths: &'a [PathBuf],
    pub(super) receipt: Option<&'a ArtifactReceipt>,
    pub(super) project_inputs: Option<&'a BTreeSet<PathBuf>>,
}

pub(super) fn build_inputs(
    directory: &Path,
    artifact: &Path,
    sources: &[PathBuf],
    graph: BuildInputGraph<'_>,
) -> Result<Vec<InputEntry>, String> {
    let mut dependency_files = BTreeSet::from([graph.dependency_file.to_owned()]);
    dependency_files.extend(graph.unit_dependency_files.iter().cloned());
    let mut paths = BTreeSet::new();
    for dependency_file in dependency_files {
        paths.extend(dependency_paths(directory, &dependency_file)?);
    }
    for path in graph.encoded_dependency_paths {
        paths.insert(resolve_dependency_path(directory, path)?);
    }
    if let Some(target) = cargo_target_directory(artifact) {
        paths.retain(|path| !path.starts_with(target));
    }
    add_dependency_manifests(&mut paths);
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

    if let Some(receipt) = graph.receipt {
        let target = cargo_target_directory(artifact);
        for manifest in &receipt.package_manifests {
            let manifest = fs::canonicalize(manifest).map_err(|error| {
                format!(
                    "could not resolve Cargo package manifest {}: {error}",
                    manifest.display()
                )
            })?;
            paths.insert(manifest.clone());
            let package = manifest.parent().ok_or_else(|| {
                format!(
                    "Cargo package manifest has no parent: {}",
                    manifest.display()
                )
            })?;
            if package.starts_with(directory) {
                continue;
            }
            let compiler_input_recorded = paths
                .range(package.to_owned()..)
                .take_while(|path| path.starts_with(package))
                .any(|path| path != &manifest);
            // Cargo's dep-info (or its encoded fingerprint fallback) is the
            // authoritative list of files read by each reachable compiler
            // unit. Rewalking every registry package duplicates that graph
            // and turns a hot decision into thousands of unnecessary stats.
            // Keep the package directory itself so a new default build.rs is
            // still visible. If Cargo supplied no input inside the package,
            // retain the conservative recursive fallback.
            paths.insert(package.to_owned());
            if !compiler_input_recorded {
                collect_project_rust_inputs(package, target, &mut paths)?;
            }
        }
        if let Some(project_inputs) = graph.project_inputs {
            // The topology graph records the names of every project Rust and
            // Cargo file so additions/removals still invalidate. Content is
            // authoritative only when Cargo's dep-info/fingerprint graph says
            // the compiler or a build script consumed it. Retain directory
            // symlink aliases as inputs because their target mapping is not
            // represented by the topology path set alone.
            paths.extend(
                project_inputs
                    .iter()
                    .filter(|path| {
                        fs::symlink_metadata(path)
                            .is_ok_and(|metadata| metadata.file_type().is_symlink())
                    })
                    .cloned(),
            );
        } else {
            add_project_rust_inputs(directory, artifact, &mut paths)?;
        }
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

fn add_dependency_manifests(paths: &mut BTreeSet<PathBuf>) {
    let dependencies = paths.iter().cloned().collect::<Vec<_>>();
    for dependency in dependencies {
        for ancestor in dependency.ancestors().skip(1) {
            let manifest = ancestor.join("Cargo.toml");
            if manifest.is_file() {
                paths.insert(manifest);
            }
        }
    }
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

pub(super) fn project_topology(
    directory: &Path,
    artifact: &Path,
) -> Result<ProjectTopology, String> {
    project_topology_and_inputs(directory, artifact).map(|(topology, _)| topology)
}

pub(super) fn project_topology_and_inputs(
    directory: &Path,
    artifact: &Path,
) -> Result<(ProjectTopology, BTreeSet<PathBuf>), String> {
    let directory = fs::canonicalize(directory).map_err(|error| {
        format!(
            "could not resolve project topology root {}: {error}",
            directory.display()
        )
    })?;
    let target = cargo_target_directory(artifact).and_then(|target| fs::canonicalize(target).ok());
    let mut directories = Vec::new();
    let mut paths = BTreeSet::new();
    collect_project_topology(
        &directory,
        &directory,
        target.as_deref(),
        &mut BTreeSet::new(),
        &mut directories,
        &mut paths,
    )?;
    add_control_topology_directories(&directory, &mut directories)?;
    add_cargo_control_inputs(&directory, &mut paths);
    let mut control_paths = BTreeSet::new();
    add_cargo_control_inputs(&directory, &mut control_paths);
    directories.sort_by(|left, right| left.path.cmp(&right.path));
    directories.dedup_by(|left, right| left.path == right.path);
    for recorded in &mut directories {
        let relevant_paths = if recorded.path.starts_with(&directory) {
            &paths
        } else {
            // Ancestors outside the project are watched only because Cargo may
            // discover configuration or toolchain files there. Hashing that
            // exact name set prevents unrelated sibling churn (for example in
            // a shared temporary directory) from forcing a project-wide walk.
            &control_paths
        };
        recorded.subtree_digest = Some(topology_subtree_digest(&recorded.path, relevant_paths));
    }

    let mut digest = Sha256::new();
    digest.update(b"CINDER-PROJECT-TOPOLOGY-2");
    for path in &paths {
        append_context_value(
            &mut digest,
            path.strip_prefix(&directory)
                .unwrap_or(path)
                .as_os_str()
                .as_bytes(),
        );
    }
    Ok((
        ProjectTopology {
            digest: digest.finalize().into(),
            directories,
        },
        paths,
    ))
}

fn collect_project_topology(
    directory: &Path,
    project_root: &Path,
    target: Option<&Path>,
    visited: &mut BTreeSet<PathBuf>,
    directories: &mut Vec<TopologyDirectory>,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    let directory = fs::canonicalize(directory).map_err(|error| {
        format!(
            "could not resolve project topology {}: {error}",
            directory.display()
        )
    })?;
    if !directory.starts_with(project_root) {
        return Err(format!(
            "symlinked project source directory escapes the workspace: {}",
            directory.display()
        ));
    }
    if target.is_some_and(|target| directory.starts_with(target))
        || !visited.insert(directory.clone())
    {
        return Ok(());
    }
    if directories.len() >= MAX_PROJECT_TOPOLOGY_DIRECTORIES || paths.len() >= MAX_INPUT_ENTRIES {
        return Err("project topology contains too many filesystem entries".to_owned());
    }
    let before = artifact_file_identity(&directory)?;
    let mut entries: Vec<_> = fs::read_dir(&directory)
        .map_err(|error| {
            format!(
                "could not inspect project topology {}: {error}",
                directory.display()
            )
        })?
        .collect::<Result<_, _>>()
        .map_err(|error| format!("could not inspect project topology entry: {error}"))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
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
                "could not inspect project topology entry {}: {error}",
                path.display()
            )
        })?;
        let is_directory_symlink =
            file_type.is_symlink() && fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir());
        if file_type.is_dir() || is_directory_symlink {
            if is_directory_symlink {
                // Preserve the logical alias as topology evidence as well as
                // traversing its canonical target. Otherwise removing an
                // alias whose target is also reachable by its real path could
                // leave the canonical file-set digest unchanged.
                paths.insert(path.clone());
            }
            let resolved = fs::canonicalize(&path).map_err(|error| {
                format!(
                    "could not resolve project source directory {}: {error}",
                    path.display()
                )
            })?;
            if resolved.join(".rustc_info.json").is_file()
                || resolved.join("CACHEDIR.TAG").is_file()
            {
                continue;
            }
            collect_project_topology(&resolved, project_root, target, visited, directories, paths)?;
        } else if path.extension() == Some("rs".as_ref())
            || matches!(
                path.file_name().and_then(OsStr::to_str),
                Some("Cargo.toml" | "Cargo.lock")
            )
        {
            paths.insert(path);
        }
    }
    let after = artifact_file_identity(&directory)?;
    if before != after {
        return Err(format!(
            "project topology changed while Cinder inspected {}",
            directory.display()
        ));
    }
    directories.push(TopologyDirectory {
        path: directory,
        identity: after,
        subtree_digest: None,
    });
    Ok(())
}

fn add_control_topology_directories(
    directory: &Path,
    directories: &mut Vec<TopologyDirectory>,
) -> Result<(), String> {
    let mut ancestor = Some(directory);
    while let Some(path) = ancestor {
        add_topology_directory(path, directories)?;
        let cargo = path.join(".cargo");
        if cargo.is_dir() {
            add_topology_directory(&cargo, directories)?;
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
        let existing = cargo_home
            .ancestors()
            .find(|ancestor| ancestor.is_dir())
            .ok_or_else(|| "Cargo home has no existing ancestor".to_owned())?;
        add_topology_directory(existing, directories)?;
    }
    Ok(())
}

fn add_topology_directory(
    directory: &Path,
    directories: &mut Vec<TopologyDirectory>,
) -> Result<(), String> {
    directories.push(TopologyDirectory {
        path: directory.to_owned(),
        identity: artifact_file_identity(directory)?,
        subtree_digest: None,
    });
    Ok(())
}

fn topology_subtree_digest(root: &Path, paths: &BTreeSet<PathBuf>) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"CINDER-PROJECT-SUBTREE-1");
    for path in paths.iter().filter(|path| path.starts_with(root)) {
        append_context_value(
            &mut digest,
            path.strip_prefix(root)
                .unwrap_or(path)
                .as_os_str()
                .as_bytes(),
        );
    }
    digest.finalize().into()
}

fn current_topology_subtree_digest(
    project_root: &Path,
    subtree: &Path,
    artifact: &Path,
) -> Result<[u8; 32], String> {
    if !subtree.starts_with(project_root) {
        let mut control_paths = BTreeSet::new();
        add_cargo_control_inputs(project_root, &mut control_paths);
        return Ok(topology_subtree_digest(subtree, &control_paths));
    }
    let target = cargo_target_directory(artifact).and_then(|target| fs::canonicalize(target).ok());
    let mut paths = BTreeSet::new();
    collect_project_topology(
        subtree,
        project_root,
        target.as_deref(),
        &mut BTreeSet::new(),
        &mut Vec::new(),
        &mut paths,
    )?;
    add_cargo_control_inputs(project_root, &mut paths);
    Ok(topology_subtree_digest(subtree, &paths))
}

fn trace_topology_validation(
    changed: usize,
    project_rescans: usize,
    control_checks: usize,
    full_rescan: bool,
) {
    if env::var_os(super::TRACE_RUN).is_some() {
        eprintln!(
            "    Cinder trace: topology changed-directories={changed} project-rescans={project_rescans} control-checks={control_checks} full-rescan={full_rescan}"
        );
    }
}

pub(super) fn project_topology_is_unchanged(
    topology: &ProjectTopology,
    directory: &Path,
    artifact: &Path,
) -> Result<bool, String> {
    let directory = fs::canonicalize(directory).map_err(|error| {
        format!(
            "could not resolve project topology root {}: {error}",
            directory.display()
        )
    })?;
    if !topology
        .directories
        .iter()
        .any(|recorded| recorded.path == directory)
    {
        return Ok(false);
    }
    let mut changed = topology
        .directories
        .iter()
        .filter(|recorded| {
            !artifact_file_identity(&recorded.path)
                .is_ok_and(|identity| identity == recorded.identity)
        })
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return Ok(true);
    }
    // Validate the project domain before its external Cargo-control
    // ancestors. An external ancestor's digest deliberately covers only
    // Cargo-relevant control names, so it must never suppress a stricter
    // project-subtree validation merely because it is a path prefix.
    changed.sort_by_key(|recorded| {
        (
            !recorded.path.starts_with(&directory),
            recorded.path.components().count(),
        )
    });
    let changed_count = changed.len();
    let mut validated_subtrees = Vec::<PathBuf>::new();
    let mut project_rescans = 0usize;
    let mut control_checks = 0usize;
    for recorded in changed {
        if validated_subtrees
            .iter()
            .any(|validated| recorded.path.starts_with(validated))
        {
            continue;
        }
        let Some(expected) = recorded.subtree_digest else {
            let unchanged = project_topology(&directory, artifact)?.digest == topology.digest;
            trace_topology_validation(changed_count, project_rescans, control_checks, true);
            return Ok(unchanged);
        };
        let current = match current_topology_subtree_digest(&directory, &recorded.path, artifact) {
            Ok(current) => current,
            Err(_) => {
                let unchanged = project_topology(&directory, artifact)?.digest == topology.digest;
                trace_topology_validation(changed_count, project_rescans, control_checks, true);
                return Ok(unchanged);
            }
        };
        if recorded.path.starts_with(&directory) {
            project_rescans += 1;
        } else {
            control_checks += 1;
        }
        if current != expected {
            trace_topology_validation(changed_count, project_rescans, control_checks, false);
            return Ok(false);
        }
        validated_subtrees.push(recorded.path.clone());
    }
    trace_topology_validation(changed_count, project_rescans, control_checks, false);
    Ok(true)
}

pub(super) fn collect_project_rust_inputs(
    directory: &Path,
    target: Option<&Path>,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    let root = fs::canonicalize(directory).map_err(|error| {
        format!(
            "could not resolve project input root {}: {error}",
            directory.display()
        )
    })?;
    let target = target.and_then(|target| fs::canonicalize(target).ok());
    collect_project_rust_inputs_inner(&root, &root, target.as_deref(), paths, &mut BTreeSet::new())
}

fn collect_project_rust_inputs_inner(
    directory: &Path,
    root: &Path,
    target: Option<&Path>,
    paths: &mut BTreeSet<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    let directory = fs::canonicalize(directory).map_err(|error| {
        format!(
            "could not resolve project inputs {}: {error}",
            directory.display()
        )
    })?;
    if !directory.starts_with(root) {
        return Err(format!(
            "symlinked project input directory escapes its package: {}",
            directory.display()
        ));
    }
    if target.is_some_and(|target| directory.starts_with(target))
        || !visited.insert(directory.clone())
    {
        return Ok(());
    }
    if paths.len() >= MAX_INPUT_ENTRIES {
        return Err("project input graph contains too many filesystem entries".to_owned());
    }
    paths.insert(directory.clone());
    for entry in fs::read_dir(&directory).map_err(|error| {
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
        let is_directory_symlink =
            file_type.is_symlink() && fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir());
        if file_type.is_dir() || is_directory_symlink {
            if is_directory_symlink {
                paths.insert(path.clone());
            }
            let resolved = fs::canonicalize(&path).map_err(|error| {
                format!(
                    "could not resolve project input directory {}: {error}",
                    path.display()
                )
            })?;
            if resolved.join(".rustc_info.json").is_file()
                || resolved.join("CACHEDIR.TAG").is_file()
            {
                continue;
            }
            collect_project_rust_inputs_inner(&resolved, root, target, paths, visited)?;
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
    let mut outputs = receipt.build_script_outputs.clone();
    if outputs.is_empty() {
        let Some(out_directory) = receipt.out_directory.as_deref() else {
            return Ok(());
        };
        let manifest_directory = receipt
            .manifest_directory
            .as_deref()
            .ok_or_else(|| "build script state has no manifest directory".to_owned())?;
        outputs.push(BuildScriptOutput {
            manifest_directory: manifest_directory.to_owned(),
            out_directory: out_directory.to_owned(),
        });
    }
    for output in outputs {
        add_one_build_script_inputs(
            &receipt.artifact,
            &output.manifest_directory,
            &output.out_directory,
            paths,
        )?;
    }
    Ok(())
}

fn add_one_build_script_inputs(
    artifact: &Path,
    manifest_directory: &Path,
    out_directory: &Path,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
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
        let target =
            cargo_target_directory(artifact).and_then(|target| fs::canonicalize(target).ok());
        collect_default_build_script_tree(manifest_directory, target.as_deref(), paths, 100_000)?;
        return Ok(());
    }
    let target = cargo_target_directory(artifact);
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
        match fs::metadata(&watched) {
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
    collect_default_build_script_tree_inner(
        directory,
        target,
        paths,
        &mut BTreeSet::new(),
        remaining,
    )
}

fn collect_default_build_script_tree_inner(
    directory: &Path,
    target: Option<&Path>,
    paths: &mut BTreeSet<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
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
    if !visited.insert(directory.clone()) {
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
        let metadata = fs::metadata(&path).map_err(|error| {
            format!(
                "could not inspect build-script input {}: {error}",
                path.display()
            )
        })?;
        if metadata.is_dir() {
            remaining =
                collect_default_build_script_tree_inner(&path, target, paths, visited, remaining)?;
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
    collect_watched_tree_inner(directory, paths, &mut BTreeSet::new(), remaining)
}

fn collect_watched_tree_inner(
    directory: &Path,
    paths: &mut BTreeSet<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
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
    if !visited.insert(directory.clone()) {
        return Ok(remaining);
    }
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
        let metadata = fs::metadata(&path).map_err(|error| {
            format!("could not inspect build input {}: {error}", path.display())
        })?;
        if metadata.is_dir() {
            remaining = collect_watched_tree_inner(&path, paths, visited, remaining)?;
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
            let hash = path
                .file_stem()
                .and_then(OsStr::to_str)
                .and_then(|stem| stem.strip_prefix(&prefix));
            if executable.is_file()
                && artifact_metadata(&executable).ok() == Some(expected_metadata)
                && hash.is_some_and(|hash| cargo_fingerprint_directory(parent, hash).is_ok())
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
        unit_dependency_files: Vec::new(),
        unit_artifacts: Vec::new(),
        unit_fingerprint_files: Vec::new(),
    };
    outputs
        .are_present()
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
    pub(super) fn are_present(&self) -> bool {
        self.dependency_file.is_file()
            && self.artifact.is_file()
            && self.fingerprint.is_dir()
            && self.unit_fingerprints.iter().all(|path| path.is_dir())
            && self.unit_dependency_files.iter().all(|path| path.is_file())
            && self.unit_artifacts.iter().all(|path| path.is_file())
            && self
                .unit_fingerprint_files
                .iter()
                .all(|entry| entry.path.is_file())
    }

    pub(super) fn are_unchanged(&self) -> bool {
        self.are_present()
            && self.unit_fingerprint_files.iter().all(|entry| {
                artifact_file_identity(&entry.path).is_ok_and(|identity| identity == entry.identity)
            })
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

pub(super) fn dependency_output_paths(
    dependency_file: &Path,
    profile: &Path,
) -> Result<BTreeSet<PathBuf>, String> {
    let dependency_bytes = fs::read(dependency_file).map_err(|error| {
        format!(
            "could not read Cargo dependency file {}: {error}",
            dependency_file.display()
        )
    })?;
    let dependency_file = fs::canonicalize(dependency_file).map_err(|error| {
        format!(
            "could not resolve Cargo dependency file {}: {error}",
            dependency_file.display()
        )
    })?;
    let profile = fs::canonicalize(profile).map_err(|error| {
        format!(
            "could not resolve Cargo profile directory {}: {error}",
            profile.display()
        )
    })?;
    let mut outputs = BTreeSet::new();
    for line in dependency_bytes.split(|byte| *byte == b'\n') {
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        for output in makefile_words(&line[..colon]) {
            if output.is_empty() {
                continue;
            }
            let output = PathBuf::from(OsString::from_vec(output));
            if !output.is_absolute() {
                continue;
            }
            let output = fs::canonicalize(&output).map_err(|error| {
                format!(
                    "could not resolve Cargo compiler output {}: {error}",
                    output.display()
                )
            })?;
            if output != dependency_file && output.starts_with(&profile) {
                outputs.insert(output);
            }
        }
    }
    Ok(outputs)
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
    if inputs.len() > MAX_INPUT_ENTRIES {
        return Err("Cinder input state contains too many entries".to_owned());
    }
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create Cinder input state: {error}"))?;
    file.write_all(INPUTS_MAGIC)
        .and_then(|()| file.write_all(&(inputs.len() as u64).to_le_bytes()))
        .map_err(|error| format!("could not write Cinder input state: {error}"))?;
    let mut previous = None;
    for input in inputs {
        if !input.path.is_absolute()
            || previous.is_some_and(|path: &Path| path >= input.path.as_path())
        {
            return Err("Cinder input paths are not absolute and ordered".to_owned());
        }
        previous = Some(input.path.as_path());
        let path = input.path.as_os_str().as_bytes();
        if path.len() > MAX_INPUT_PATH_BYTES {
            return Err("Cinder input path is too long".to_owned());
        }
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
    let contents = read_bounded_state(path, MAX_INPUT_STATE_BYTES, "Cinder input state")?;
    let mut reader = StateReader::new(&contents);
    let magic = reader.array("Cinder input state")?;
    if &magic != INPUTS_MAGIC {
        return Err("Cinder input state has an unsupported format".to_owned());
    }
    let count = usize::try_from(u64::from_le_bytes(reader.array("Cinder input count")?))
        .map_err(|_| "Cinder input state is too large".to_owned())?;
    if count > MAX_INPUT_ENTRIES {
        return Err("Cinder input state contains too many entries".to_owned());
    }
    let mut inputs = Vec::with_capacity(count);
    for _ in 0..count {
        let path = PathBuf::from(OsString::from_vec(
            reader
                .length_prefixed(MAX_INPUT_PATH_BYTES, "input path")?
                .to_vec(),
        ));
        if !path.is_absolute()
            || inputs
                .last()
                .is_some_and(|previous: &InputEntry| previous.path >= path)
        {
            return Err("Cinder input state paths are not absolute and ordered".to_owned());
        }
        inputs.push(InputEntry {
            path,
            identity: read_file_identity(&mut reader, "Cinder input identity")?,
            digest: reader.array("Cinder input digest")?,
        });
    }
    reader.finish("Cinder input state")?;
    Ok(inputs)
}

pub(super) const PROJECT_TOPOLOGY_MAGIC: &[u8; 8] = b"CNDT0003";
const MAX_PROJECT_TOPOLOGY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROJECT_TOPOLOGY_DIRECTORIES: usize = 100_000;
const MAX_PROJECT_TOPOLOGY_PATH_BYTES: usize = 1024 * 1024;

pub(super) fn write_project_topology(
    path: &Path,
    topology: &ProjectTopology,
) -> Result<(), String> {
    if topology.directories.is_empty()
        || topology.directories.len() > MAX_PROJECT_TOPOLOGY_DIRECTORIES
    {
        return Err("Cinder topology state has an invalid directory count".to_owned());
    }
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create Cinder topology state: {error}"))?;
    file.write_all(PROJECT_TOPOLOGY_MAGIC)
        .and_then(|()| file.write_all(&topology.digest))
        .and_then(|()| file.write_all(&(topology.directories.len() as u64).to_le_bytes()))
        .map_err(|error| format!("could not write Cinder topology state: {error}"))?;
    let mut previous = None;
    for directory in &topology.directories {
        if !directory.path.is_absolute()
            || previous.is_some_and(|path: &Path| path >= directory.path.as_path())
        {
            return Err("Cinder topology paths are not absolute and ordered".to_owned());
        }
        previous = Some(directory.path.as_path());
        let path = directory.path.as_os_str().as_bytes();
        if path.len() > MAX_PROJECT_TOPOLOGY_PATH_BYTES {
            return Err("Cinder topology path is too long".to_owned());
        }
        let length =
            u32::try_from(path.len()).map_err(|_| "Cinder topology path is too long".to_owned())?;
        file.write_all(&length.to_le_bytes())
            .and_then(|()| file.write_all(path))
            .and_then(|()| file.write_all(&directory.identity.size.to_le_bytes()))
            .and_then(|()| file.write_all(&directory.identity.modified_ns.to_le_bytes()))
            .and_then(|()| file.write_all(&directory.identity.device.to_le_bytes()))
            .and_then(|()| file.write_all(&directory.identity.inode.to_le_bytes()))
            .and_then(|()| file.write_all(&directory.identity.changed_seconds.to_le_bytes()))
            .and_then(|()| file.write_all(&directory.identity.changed_nanoseconds.to_le_bytes()))
            .map_err(|error| format!("could not write Cinder topology state: {error}"))?;
        match directory.subtree_digest {
            Some(digest) => file.write_all(&[1]).and_then(|()| file.write_all(&digest)),
            None => file.write_all(&[0]),
        }
        .map_err(|error| format!("could not write Cinder topology state: {error}"))?;
    }
    Ok(())
}

pub(super) fn read_project_topology(path: &Path) -> Result<ProjectTopology, String> {
    let contents = read_bounded_state(path, MAX_PROJECT_TOPOLOGY_BYTES, "Cinder topology state")?;
    let mut reader = StateReader::new(&contents);
    let magic = reader.array("Cinder topology state")?;
    if &magic != PROJECT_TOPOLOGY_MAGIC {
        return Err("Cinder topology state has an unsupported format".to_owned());
    }
    let digest = reader.array("Cinder topology digest")?;
    let count = usize::try_from(u64::from_le_bytes(
        reader.array("Cinder topology directory count")?,
    ))
    .map_err(|_| "Cinder topology state is too large".to_owned())?;
    if count == 0 || count > MAX_PROJECT_TOPOLOGY_DIRECTORIES {
        return Err("Cinder topology state has an invalid directory count".to_owned());
    }
    let mut directories = Vec::with_capacity(count);
    for _ in 0..count {
        let path = PathBuf::from(OsString::from_vec(
            reader
                .length_prefixed(MAX_PROJECT_TOPOLOGY_PATH_BYTES, "topology path")?
                .to_vec(),
        ));
        if !path.is_absolute() {
            return Err("Cinder topology state contains a relative path".to_owned());
        }
        if directories
            .last()
            .is_some_and(|previous: &TopologyDirectory| previous.path >= path)
        {
            return Err("Cinder topology state paths are not ordered".to_owned());
        }
        let identity = read_file_identity(&mut reader, "Cinder topology identity")?;
        let subtree_digest = match reader.array::<1>("Cinder topology subtree marker")?[0] {
            0 => None,
            1 => Some(reader.array("Cinder topology subtree digest")?),
            _ => return Err("Cinder topology state has an invalid subtree marker".to_owned()),
        };
        directories.push(TopologyDirectory {
            path,
            identity,
            subtree_digest,
        });
    }
    reader.finish("Cinder topology state")?;
    Ok(ProjectTopology {
        digest,
        directories,
    })
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

pub(super) const CARGO_OUTPUTS_MAGIC: &[u8; 8] = b"CNDO0005";
const MAX_CARGO_GRAPH_OUTPUTS: usize = 100_000;
const MAX_CARGO_OUTPUT_STATE_BYTES: u64 = 256 * 1024 * 1024;

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
    if outputs.unit_fingerprints.len() > MAX_CARGO_GRAPH_OUTPUTS
        || outputs.unit_dependency_files.len() > MAX_CARGO_GRAPH_OUTPUTS
        || outputs.unit_artifacts.len() > MAX_CARGO_GRAPH_OUTPUTS
        || outputs.unit_fingerprint_files.len() > MAX_CARGO_GRAPH_OUTPUTS
    {
        return Err("Cargo output state contains too many graph outputs".to_owned());
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
    file.write_all(&(outputs.unit_dependency_files.len() as u64).to_le_bytes())
        .map_err(|error| format!("could not write Cargo dependency count: {error}"))?;
    for dependency_file in &outputs.unit_dependency_files {
        write_state_bytes(
            &mut file,
            dependency_file.as_os_str().as_bytes(),
            "Cargo dependency path",
        )?;
    }
    file.write_all(&(outputs.unit_artifacts.len() as u64).to_le_bytes())
        .map_err(|error| format!("could not write Cargo artifact count: {error}"))?;
    for artifact in &outputs.unit_artifacts {
        write_state_bytes(
            &mut file,
            artifact.as_os_str().as_bytes(),
            "Cargo artifact path",
        )?;
    }
    file.write_all(&(outputs.unit_fingerprint_files.len() as u64).to_le_bytes())
        .map_err(|error| format!("could not write Cargo fingerprint file count: {error}"))?;
    for entry in &outputs.unit_fingerprint_files {
        write_state_bytes(
            &mut file,
            entry.path.as_os_str().as_bytes(),
            "Cargo fingerprint file path",
        )?;
        file.write_all(&entry.identity.size.to_le_bytes())
            .and_then(|()| file.write_all(&entry.identity.modified_ns.to_le_bytes()))
            .and_then(|()| file.write_all(&entry.identity.device.to_le_bytes()))
            .and_then(|()| file.write_all(&entry.identity.inode.to_le_bytes()))
            .and_then(|()| file.write_all(&entry.identity.changed_seconds.to_le_bytes()))
            .and_then(|()| file.write_all(&entry.identity.changed_nanoseconds.to_le_bytes()))
            .map_err(|error| format!("could not write Cargo fingerprint file state: {error}"))?;
    }
    Ok(())
}

pub(super) fn read_cargo_outputs(path: &Path) -> Result<CargoOutputs, String> {
    let contents = read_bounded_state(path, MAX_CARGO_OUTPUT_STATE_BYTES, "Cargo output state")?;
    let mut reader = StateReader::new(&contents);
    let magic = reader.array("Cargo output state")?;
    if &magic != CARGO_OUTPUTS_MAGIC {
        return Err("Cargo output state has an unsupported format".to_owned());
    }
    let dependency_file = read_cargo_output_path(&mut reader)?;
    let artifact = read_cargo_output_path(&mut reader)?;
    let fingerprint = read_cargo_output_path(&mut reader)?;
    let unit_fingerprints = read_cargo_output_paths(&mut reader, "fingerprints")?;
    let unit_dependency_files = read_cargo_output_paths(&mut reader, "dependencies")?;
    let unit_artifacts = read_cargo_output_paths(&mut reader, "artifacts")?;
    let count = read_cargo_output_count(&mut reader, "fingerprint files")?;
    let mut unit_fingerprint_files = Vec::with_capacity(count);
    for _ in 0..count {
        unit_fingerprint_files.push(CargoOutputEntry {
            path: read_cargo_output_path(&mut reader)?,
            identity: read_file_identity(&mut reader, "Cargo fingerprint file identity")?,
        });
    }
    reader.finish("Cargo output state")?;
    Ok(CargoOutputs {
        dependency_file,
        artifact,
        fingerprint,
        unit_fingerprints,
        unit_dependency_files,
        unit_artifacts,
        unit_fingerprint_files,
    })
}

fn read_cargo_output_count(reader: &mut StateReader<'_>, label: &str) -> Result<usize, String> {
    let count = u64::from_le_bytes(reader.array("Cargo output count")?);
    if count > MAX_CARGO_GRAPH_OUTPUTS as u64 {
        return Err(format!("Cargo output state contains too many {label}"));
    }
    Ok(count as usize)
}

fn read_cargo_output_paths(
    reader: &mut StateReader<'_>,
    label: &str,
) -> Result<Vec<PathBuf>, String> {
    let count = read_cargo_output_count(reader, label)?;
    (0..count).map(|_| read_cargo_output_path(reader)).collect()
}

fn read_cargo_output_path(reader: &mut StateReader<'_>) -> Result<PathBuf, String> {
    let path = PathBuf::from(OsString::from_vec(
        reader
            .length_prefixed(1_048_576, "Cargo output path")?
            .to_vec(),
    ));
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
    file: &mut impl Write,
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
