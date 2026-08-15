//! Validated state publication, loading, promotion, and revision matching.

use super::{
    BTreeSet, DUPLICATE_EVENT_MAX_AGE, Digest, HistoryProbeCache, LiteralChange, OsStr, OsStrExt,
    OsString, OsStringExt, Path, PathBuf, PermissionsExt, Read, Sha256, StateKind, SystemTime,
    UNIX_EPOCH, Write, add_cargo_control_inputs, add_project_rust_inputs, append_context_value,
    artifact_file_identity, artifact_identity, artifact_is_executable, artifact_metadata,
    bind_observed_shell_environment, build_inputs, build_literal_index, build_source_paths,
    cargo_outputs_for_artifact, cargo_target_lock_path, clone_file, compiler_unit_graph, env, fs,
    history_directory, history_recency, input_entries_are_unchanged, input_entries_match_revision,
    io, make_cached_artifact_read_only, make_private_directory, parse_state_number,
    project_may_have_build_script, prune_global_history, prune_history, read_cargo_outputs,
    read_inputs, read_literal_index, read_runtime_environment, read_source_paths,
    runtime_linker_environment, snapshot_sources, source_revision_digest, sources_are_unchanged,
    state_directory, state_project_directory, touch_history_entry, write_cargo_outputs,
    write_inputs, write_literal_index, write_runtime_environment, write_source_paths,
};

pub(super) struct State {
    pub(super) snapshot: PathBuf,
    pub(super) source_digest: [u8; 32],
    pub(super) artifact: PathBuf,
    pub(super) public_artifact: PathBuf,
    pub(super) program_name: OsString,
    pub(super) artifact_file_identity: ArtifactFileIdentity,
    pub(super) artifact_digest: Option<[u8; 32]>,
    pub(super) literal_index: Vec<LiteralIndexEntry>,
    pub(super) run_context: Vec<u8>,
    pub(super) observes_underscore: bool,
    pub(super) inputs: Vec<InputEntry>,
    pub(super) sources: Vec<PathBuf>,
    pub(super) cargo_outputs: CargoOutputs,
    pub(super) runtime_environment: Vec<(OsString, OsString)>,
}

#[derive(Clone)]
pub(super) struct CargoOutputs {
    pub(super) dependency_file: PathBuf,
    pub(super) artifact: PathBuf,
    pub(super) fingerprint: PathBuf,
    pub(super) unit_fingerprints: Vec<PathBuf>,
}

pub(super) struct ArtifactReceipt {
    pub(super) artifact: PathBuf,
    pub(super) dependency_file: PathBuf,
    pub(super) public_file_name: OsString,
    pub(super) crate_type: String,
    pub(super) manifest_directory: Option<PathBuf>,
    pub(super) out_directory: Option<PathBuf>,
}

pub(super) const ARTIFACT_RECEIPT_MAGIC: &[u8; 8] = b"CNDR0003";

pub(super) fn write_artifact_receipt(path: &Path, receipt: &ArtifactReceipt) -> Result<(), String> {
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

pub(super) fn write_optional_receipt_value(
    file: &mut fs::File,
    value: Option<&OsStr>,
) -> Result<(), String> {
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

pub(super) fn read_artifact_receipt(path: &Path) -> Result<ArtifactReceipt, String> {
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

pub(super) fn read_receipt_value(file: &mut fs::File) -> Result<Option<OsString>, String> {
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
pub(super) struct LiteralIndexEntry {
    pub(super) bytes: Vec<u8>,
    pub(super) offset: u64,
}

#[derive(Clone)]
pub(super) struct InputEntry {
    pub(super) path: PathBuf,
    pub(super) identity: ArtifactFileIdentity,
    pub(super) digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ArtifactFileIdentity {
    pub(super) size: u64,
    pub(super) modified_ns: u128,
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) changed_seconds: i64,
    pub(super) changed_nanoseconds: i64,
}

#[derive(Clone, Copy)]
pub(super) struct StatePublication<'a> {
    pub(super) source_root: &'a Path,
    pub(super) source_digest: &'a [u8; 32],
    pub(super) artifact: &'a Path,
    pub(super) artifact_file_identity: &'a ArtifactFileIdentity,
    pub(super) artifact_digest: Option<&'a [u8; 32]>,
    pub(super) public_artifact: &'a Path,
    pub(super) program_name: &'a OsStr,
    pub(super) literal_index: &'a [LiteralIndexEntry],
    pub(super) run_context: &'a [u8],
    pub(super) observes_underscore: bool,
    pub(super) inputs: &'a [InputEntry],
    pub(super) sources: &'a [PathBuf],
    pub(super) cargo_outputs: &'a CargoOutputs,
    pub(super) runtime_environment: &'a [(OsString, OsString)],
    pub(super) duplicate_ready: bool,
}

impl State {
    pub(super) fn load(directory: &Path, kind: StateKind) -> Result<Option<Self>, String> {
        let root = state_directory(directory, kind);
        Self::load_from(&root)
    }

    pub(super) fn load_from(root: &Path) -> Result<Option<Self>, String> {
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

    pub(super) fn record_fresh(
        directory: &Path,
        kind: StateKind,
        artifact: &Path,
        program_name: &OsStr,
        run_context: &[u8],
        receipt: Option<&ArtifactReceipt>,
    ) -> Result<(), String> {
        let mut cargo_outputs = cargo_outputs_for_artifact(artifact, receipt)?;
        let (observes_underscore, unit_fingerprints) = compiler_unit_graph(&cargo_outputs, b"_")?;
        cargo_outputs.unit_fingerprints = unit_fingerprints;
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
            let literal_index = if matches!(kind, StateKind::Check | StateKind::Test) {
                Vec::new()
            } else {
                build_literal_index(&capture, &sources, artifact)?
            };
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
            if matches!(kind, StateKind::Check | StateKind::Test) {
                Ok(())
            } else {
                Self::cache_current(directory, kind)
            }
        })();
        let _ = fs::remove_dir_all(capture);
        result
    }

    pub(super) fn record_patched(
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

    pub(super) fn publish(
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

    pub(super) fn cache_current(directory: &Path, kind: StateKind) -> Result<(), String> {
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

    pub(super) fn history_key(&self) -> Result<String, String> {
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

    pub(super) fn historical_target_lock_path(
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

    pub(super) fn matching_history(
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

    pub(super) fn promote(
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

    pub(super) fn artifact_is_unchanged(&self) -> Result<bool, String> {
        let identity = match artifact_file_identity(&self.artifact) {
            Ok(identity) => identity,
            Err(_) if !self.artifact.exists() => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(identity == self.artifact_file_identity)
    }

    pub(super) fn context_matches(&self, context: &[u8]) -> bool {
        self.run_context == bind_observed_shell_environment(context, self.observes_underscore)
    }

    pub(super) fn sources_match_revision(&self, directory: &Path) -> Result<bool, String> {
        Ok(source_revision_digest(directory, &self.sources)? == self.source_digest)
    }

    pub(super) fn source_snapshot_matches_revision(&self) -> Result<bool, String> {
        Ok(source_revision_digest(&self.snapshot, &self.sources)? == self.source_digest)
    }

    pub(super) fn cached_artifact_is_trusted(&self) -> Result<bool, String> {
        Ok(self.artifact_digest.is_some()
            && self.artifact_is_unchanged()?
            && fs::metadata(&self.artifact)
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o222 == 0))
    }

    pub(super) fn cargo_outputs_are_available(&self) -> bool {
        self.public_artifact.is_file() && self.cargo_outputs.are_available()
    }

    pub(super) fn literal_offset(&self, bytes: &[u8]) -> Option<u64> {
        self.literal_index
            .iter()
            .find(|entry| entry.bytes == bytes)
            .map(|entry| entry.offset)
    }

    pub(super) fn inputs_are_unchanged(&self, directory: &Path) -> Result<bool, String> {
        if !input_entries_are_unchanged(&self.inputs) {
            return Ok(false);
        }
        self.input_topology_is_unchanged(directory)
    }

    pub(super) fn inputs_match_revision(&self, directory: &Path) -> Result<bool, String> {
        if !input_entries_match_revision(&self.inputs)? {
            return Ok(false);
        }
        self.input_topology_is_unchanged(directory)
    }

    pub(super) fn input_topology_is_unchanged(&self, directory: &Path) -> Result<bool, String> {
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

    pub(super) fn indexed_patch<'a>(
        &self,
        change: &'a LiteralChange,
    ) -> Option<(&'a [u8], &'a [u8], u64)> {
        self.literal_offset(&change.old)
            .map(|offset| (change.old.as_slice(), change.new.as_slice(), offset))
    }

    pub(super) fn consume_fresh_duplicate(directory: &Path) -> Result<bool, String> {
        let path = state_directory(directory, StateKind::Run).join("duplicate-ready");
        let fresh = Self::fresh_duplicate_is_pending(directory)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(fresh),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("could not consume duplicate-run state: {error}")),
        }
    }

    pub(super) fn fresh_duplicate_is_pending(directory: &Path) -> Result<bool, String> {
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

pub(super) fn write_state_directory(
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
