//! Validated artifact patching, cloning, and macOS code-signature preservation.

use super::{
    BTreeSet, Command, Instant, LiteralChange, OsStr, Path, PathBuf, PermissionsExt, Read, Seek,
    SeekFrom, State, Stdio, SystemTime, UNIX_EPOCH, Write, env, fs, project_namespace,
    record_artifact_root,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PatchMode {
    RunSibling,
    BuildInPlace,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct AdHocSignatureMetadata {
    pub(super) identifier: Option<Vec<u8>>,
    pub(super) flags: BTreeSet<Vec<u8>>,
    pub(super) runtime_version: Option<Vec<u8>>,
    pub(super) internal_requirements: Option<Vec<u8>>,
    pub(super) entitlements: Vec<u8>,
    pub(super) linker_signed: bool,
}

pub(super) enum CodeSignatureContract {
    Unsigned,
    AdHoc(AdHocSignatureMetadata),
    Unsupported,
}

pub(super) fn patch_artifact(
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

pub(super) fn code_signature_contract(path: &Path) -> Result<CodeSignatureContract, String> {
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

pub(super) fn signature_value(lines: &[&[u8]], prefix: &[u8]) -> Option<Vec<u8>> {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(prefix))
        .map(<[u8]>::to_vec)
}

pub(super) fn code_signature_metadata_matches(
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

pub(super) fn trace_run(stage: &str, started: Instant) {
    if env::var_os(super::TRACE_RUN).is_some() {
        eprintln!(
            "    Cinder trace: {stage} {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
}

pub(super) fn changed_format_segment<'a>(
    old: &'a [u8],
    new: &'a [u8],
) -> Option<(&'a [u8], &'a [u8])> {
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

pub(super) fn clone_file(source: &Path, destination: &Path) -> Result<(), String> {
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

pub(super) fn make_cached_artifact_read_only(path: &Path) -> Result<(), String> {
    let mode = fs::metadata(path)
        .map_err(|error| format!("could not inspect cached artifact: {error}"))?
        .permissions()
        .mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode & !0o222))
        .map_err(|error| format!("could not protect cached artifact: {error}"))
}

pub(super) fn make_owner_writable(path: &Path) -> Result<(), String> {
    let mode = fs::metadata(path)
        .map_err(|error| format!("could not inspect restored artifact: {error}"))?
        .permissions()
        .mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))
        .map_err(|error| format!("could not make restored artifact writable: {error}"))
}

pub(super) fn remove_launch_xattrs(path: &Path) {
    for attribute in ["com.apple.provenance", "com.apple.quarantine"] {
        let _ = Command::new("/usr/bin/xattr")
            .args([OsStr::new("-d"), OsStr::new(attribute), path.as_os_str()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
