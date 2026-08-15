//! Cache layout, revision retention, and stale artifact collection.

use super::{
    BTreeSet, Command, DefaultHasher, Digest, Hash, Hasher, OsStr, OsStrExt, OsString, OsStringExt,
    Path, PathBuf, PermissionsExt, REVISION_HISTORY_LIMIT, REVISION_HISTORY_MAX_AGE,
    REVISION_HISTORY_MAX_BYTES, STAGING_MAX_AGE, Sha256, State, StateKind, Stdio, SystemTime,
    UNIX_EPOCH, env, fs, io, restored_run_artifact_path, restored_run_artifact_receipt_path,
};

pub(super) fn history_directory(directory: &Path, kind: StateKind) -> PathBuf {
    state_directory(directory, kind).with_file_name(kind.history_directory_name())
}

pub(super) fn touch_history_entry(entry: &Path) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    fs::write(entry.join("last-used"), now.to_string())
        .map_err(|error| format!("could not update Cinder revision history: {error}"))
}

pub(super) fn history_recency(entry: &Path) -> SystemTime {
    fs::metadata(entry.join("last-used"))
        .and_then(|metadata| metadata.modified())
        .or_else(|_| fs::metadata(entry).and_then(|metadata| metadata.modified()))
        .unwrap_or(UNIX_EPOCH)
}

pub(super) fn staging_process_id(path: &Path, prefix: &[u8]) -> Option<u32> {
    let name = path.file_name()?.as_bytes();
    let suffix = name.strip_prefix(prefix)?;
    let process = suffix.split(|byte| *byte == b'-').next()?;
    std::str::from_utf8(process).ok()?.parse().ok()
}

pub(super) fn process_is_running(process: u32) -> bool {
    process == std::process::id()
        || Command::new("/bin/kill")
            .args([OsStr::new("-0"), OsStr::new(&process.to_string())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

pub(super) fn staging_path_is_stale(path: &Path, prefix: &[u8], cutoff: SystemTime) -> bool {
    let Some(process) = staging_process_id(path, prefix) else {
        return false;
    };
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified < cutoff)
        && !process_is_running(process)
}

pub(super) fn prune_stale_history_staging(
    history: &Path,
    cutoff: SystemTime,
) -> Result<(), String> {
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

pub(super) fn prune_stale_artifact_staging(root: &Path, cutoff: SystemTime) -> Result<(), String> {
    prune_stale_named_staging(root, &[b".cinder-restore-", b".cinder-patch-"], cutoff)
}

pub(super) fn prune_stale_project_staging(
    project: &Path,
    cutoff: SystemTime,
) -> Result<(), String> {
    prune_stale_named_staging(
        project,
        &[
            b"run.capture-",
            b"build.capture-",
            b"check.capture-",
            b"test.capture-",
            b"run.patch-",
            b"build.patch-",
            b"run.tmp-",
            b"build.tmp-",
            b"check.tmp-",
            b"test.tmp-",
            b"run-context-",
        ],
        cutoff,
    )
}

pub(super) fn prune_stale_named_staging(
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

pub(super) fn prune_stale_global_staging(
    state_root: &Path,
    cutoff: SystemTime,
) -> Result<(), String> {
    let Some(root) = state_root.parent() else {
        return Ok(());
    };
    prune_stale_named_staging(&root.join("receipts"), &[b""], cutoff)?;
    prune_stale_named_staging(&root.join("recordings"), &[b"run-"], cutoff)
}

pub(super) fn prune_history(history: &Path) -> Result<(), String> {
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

pub(super) fn artifact_roots_directory(directory: &Path) -> PathBuf {
    state_project_directory(directory).join("run-artifact-roots")
}

pub(super) fn record_artifact_root(directory: &Path, root: &Path) -> Result<(), String> {
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

pub(super) fn prune_run_artifacts(directory: &Path) -> Result<(), String> {
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

pub(super) fn prune_run_artifact_receipts(
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

pub(super) fn cinder_run_artifact_suffix<'a>(directory: &Path, path: &'a Path) -> Option<&'a str> {
    let name = path.file_name()?.to_str()?;
    let prefix = format!(".cinder-fast-{}-", project_namespace(directory));
    name.strip_prefix(&prefix)
}

pub(super) fn is_revision_run_artifact(directory: &Path, path: &Path) -> bool {
    let Some(digest) =
        cinder_run_artifact_suffix(directory, path).and_then(|suffix| suffix.split('-').next())
    else {
        return false;
    };
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn is_patch_run_artifact(directory: &Path, path: &Path) -> bool {
    cinder_run_artifact_suffix(directory, path)
        .and_then(|suffix| suffix.strip_prefix("patch-"))
        .is_some_and(|suffix| suffix.split('-').count() >= 3)
}

pub(super) fn is_cinder_run_artifact(directory: &Path, path: &Path) -> bool {
    is_revision_run_artifact(directory, path) || is_patch_run_artifact(directory, path)
}

pub(super) fn prune_deleted_workspace_artifacts(
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

pub(super) fn prune_global_history() -> Result<(), String> {
    let state_root = cinder_state_root();
    let now = SystemTime::now();
    let cutoff = now
        .checked_sub(REVISION_HISTORY_MAX_AGE)
        .unwrap_or(UNIX_EPOCH);
    prune_global_history_at(&state_root, REVISION_HISTORY_MAX_BYTES, cutoff)
}

pub(super) fn prune_global_history_at(
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

pub(super) fn directory_logical_bytes(path: &Path) -> Result<u64, String> {
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

pub(super) fn remove_directory_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not prune Cinder state {}: {error}",
            path.display()
        )),
    }
}

pub(super) fn parse_state_number<T>(value: Option<&str>, label: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .ok_or_else(|| format!("Cinder state is missing {label}"))?
        .parse()
        .map_err(|_| format!("Cinder state has an invalid {label}"))
}

pub(super) fn state_directory(directory: &Path, kind: StateKind) -> PathBuf {
    state_project_directory(directory).join(kind.directory_name())
}

pub(super) fn state_project_directory(directory: &Path) -> PathBuf {
    cinder_state_root().join(project_namespace(directory))
}

pub(super) fn project_namespace(directory: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    directory.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(super) fn cinder_state_root() -> PathBuf {
    env::temp_dir().join("cinder").join("state")
}

pub(super) fn make_private_directory(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "could not protect Cinder state directory {}: {error}",
            path.display()
        )
    })
}
