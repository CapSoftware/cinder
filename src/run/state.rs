//! Validated state publication, loading, promotion, and revision matching.

use super::{
    BTreeSet, BuildInputGraph, CompilerRecipe, DUPLICATE_EVENT_MAX_AGE, Digest, HistoryProbeCache,
    Instant, LiteralChange, OsStr, OsStrExt, OsString, OsStringExt, Path, PathBuf, PermissionsExt,
    Read, Sha256, StateKind, SystemTime, UNIX_EPOCH, Write, append_context_value,
    artifact_file_identity, artifact_identity, artifact_is_executable, artifact_metadata,
    bind_observed_shell_environment, build_inputs, build_literal_index, build_source_paths,
    cargo_outputs_for_artifact, cargo_target_lock_path, clone_file, compiler_unit_graph, env, fs,
    history_directory, history_recency, input_entries_are_unchanged, input_entries_match_revision,
    io, make_cached_artifact_read_only, make_private_directory,
    manifest_has_standard_library_test_harness_at, package_may_have_build_script,
    parse_state_number, project_may_have_build_script, project_topology_and_inputs,
    project_topology_is_unchanged, prune_global_history, prune_history, read_cargo_outputs,
    read_compiler_recipe, read_inputs, read_literal_index, read_project_topology,
    read_runtime_environment, read_source_paths, runtime_linker_environment, snapshot_sources,
    source_revision_digest, sources_are_unchanged, state_directory, state_project_directory,
    touch_history_entry, write_cargo_outputs, write_compiler_recipe, write_inputs,
    write_literal_index, write_project_topology, write_runtime_environment, write_source_paths,
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
    pub(super) project_topology: ProjectTopology,
    pub(super) sources: Vec<PathBuf>,
    pub(super) cargo_outputs: CargoOutputs,
    pub(super) cargo_fingerprints_current: bool,
    pub(super) runtime_environment: Vec<(OsString, OsString)>,
    pub(super) runtime_directory: Option<PathBuf>,
    pub(super) compiler_recipe: Option<CompilerRecipe>,
}

pub(super) struct StateLockProbe {
    pub(super) public_artifact: PathBuf,
    run_context: Vec<u8>,
    observes_underscore: bool,
}

impl StateLockProbe {
    pub(super) fn context_matches(&self, context: &[u8]) -> bool {
        self.run_context == bind_observed_shell_environment(context, self.observes_underscore)
    }
}

#[derive(Clone)]
pub(super) struct CargoOutputs {
    pub(super) dependency_file: PathBuf,
    pub(super) artifact: PathBuf,
    pub(super) fingerprint: PathBuf,
    pub(super) unit_fingerprints: Vec<PathBuf>,
    pub(super) unit_dependency_files: Vec<PathBuf>,
    pub(super) unit_artifacts: Vec<PathBuf>,
    pub(super) unit_fingerprint_files: Vec<CargoOutputEntry>,
}

#[derive(Clone)]
pub(super) struct CargoOutputEntry {
    pub(super) path: PathBuf,
    pub(super) identity: ArtifactFileIdentity,
}

pub(super) struct ArtifactReceipt {
    pub(super) artifact: PathBuf,
    pub(super) dependency_file: PathBuf,
    pub(super) public_file_name: OsString,
    pub(super) crate_type: String,
    pub(super) manifest_directory: Option<PathBuf>,
    pub(super) out_directory: Option<PathBuf>,
    pub(super) package_manifests: Vec<PathBuf>,
    pub(super) build_script_outputs: Vec<BuildScriptOutput>,
    pub(super) compiler_recipe: Option<CompilerRecipe>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct BuildScriptOutput {
    pub(super) manifest_directory: PathBuf,
    pub(super) out_directory: PathBuf,
}

pub(super) const ARTIFACT_RECEIPT_MAGIC: &[u8; 8] = b"CNDR0005";
const MAX_PACKAGE_MANIFESTS: usize = 16_384;
const MAX_BUILD_SCRIPT_OUTPUTS: usize = 4_096;

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
    )?;
    if receipt.package_manifests.len() > MAX_PACKAGE_MANIFESTS {
        return Err("artifact receipt has too many package manifests".to_owned());
    }
    let count = u32::try_from(receipt.package_manifests.len())
        .map_err(|_| "artifact receipt has too many package manifests".to_owned())?;
    file.write_all(&count.to_le_bytes())
        .map_err(|error| format!("could not write artifact receipt: {error}"))?;
    let mut previous = None;
    for manifest in &receipt.package_manifests {
        if !manifest.is_absolute() || previous.is_some_and(|path: &Path| path >= manifest.as_path())
        {
            return Err(
                "artifact receipt package manifests are not absolute and ordered".to_owned(),
            );
        }
        previous = Some(manifest.as_path());
        write_optional_receipt_value(&mut file, Some(manifest.as_os_str()))?;
    }
    if receipt.build_script_outputs.len() > MAX_BUILD_SCRIPT_OUTPUTS {
        return Err("artifact receipt has too many build-script outputs".to_owned());
    }
    let count = u32::try_from(receipt.build_script_outputs.len())
        .map_err(|_| "artifact receipt has too many build-script outputs".to_owned())?;
    file.write_all(&count.to_le_bytes())
        .map_err(|error| format!("could not write artifact receipt: {error}"))?;
    for output in &receipt.build_script_outputs {
        write_optional_receipt_value(&mut file, Some(output.manifest_directory.as_os_str()))?;
        write_optional_receipt_value(&mut file, Some(output.out_directory.as_os_str()))?;
    }
    Ok(())
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
    let mut count = [0_u8; 4];
    file.read_exact(&mut count)
        .map_err(|error| format!("could not read artifact receipt: {error}"))?;
    let count = u32::from_le_bytes(count) as usize;
    if count > MAX_PACKAGE_MANIFESTS {
        return Err("artifact receipt has too many package manifests".to_owned());
    }
    let mut package_manifests = Vec::with_capacity(count);
    for _ in 0..count {
        let manifest = read_receipt_value(&mut file)?
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| "artifact receipt has no absolute package manifest".to_owned())?;
        if package_manifests
            .last()
            .is_some_and(|previous: &PathBuf| previous >= &manifest)
        {
            return Err("artifact receipt package manifests are not ordered".to_owned());
        }
        package_manifests.push(manifest);
    }
    let mut count = [0_u8; 4];
    file.read_exact(&mut count)
        .map_err(|error| format!("could not read artifact receipt: {error}"))?;
    let count = u32::from_le_bytes(count) as usize;
    if count > MAX_BUILD_SCRIPT_OUTPUTS {
        return Err("artifact receipt has too many build-script outputs".to_owned());
    }
    let mut build_script_outputs = Vec::with_capacity(count);
    for _ in 0..count {
        let manifest_directory = read_receipt_value(&mut file)?
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| "build-script receipt has no absolute manifest directory".to_owned())?;
        let out_directory = read_receipt_value(&mut file)?
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| "build-script receipt has no absolute output directory".to_owned())?;
        build_script_outputs.push(BuildScriptOutput {
            manifest_directory,
            out_directory,
        });
    }
    let mut trailing = [0_u8; 1];
    if file
        .read(&mut trailing)
        .map_err(|error| format!("could not finish reading artifact receipt: {error}"))?
        != 0
    {
        return Err("artifact receipt contains trailing data".to_owned());
    }
    Ok(ArtifactReceipt {
        artifact,
        dependency_file,
        public_file_name,
        crate_type,
        manifest_directory,
        out_directory,
        package_manifests,
        build_script_outputs,
        compiler_recipe: None,
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

#[derive(Clone)]
pub(super) struct ProjectTopology {
    pub(super) digest: [u8; 32],
    pub(super) directories: Vec<TopologyDirectory>,
}

#[derive(Clone)]
pub(super) struct TopologyDirectory {
    pub(super) path: PathBuf,
    pub(super) identity: ArtifactFileIdentity,
    pub(super) subtree_digest: Option<[u8; 32]>,
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
    pub(super) project_topology: &'a ProjectTopology,
    pub(super) sources: &'a [PathBuf],
    pub(super) cargo_outputs: &'a CargoOutputs,
    pub(super) cargo_fingerprints_current: bool,
    pub(super) runtime_environment: &'a [(OsString, OsString)],
    pub(super) runtime_directory: Option<&'a Path>,
    pub(super) compiler_recipe: Option<&'a CompilerRecipe>,
    pub(super) duplicate_ready: bool,
}

fn history_entry_may_match(
    entry: &Path,
    directory: &Path,
    context: &[u8],
    target_lock_path: Option<&Path>,
    probes: &mut HistoryProbeCache,
) -> Result<bool, String> {
    let recorded_context = match fs::read(entry.join("run-context")) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let observes_underscore = match fs::read(entry.join("observes-underscore")) {
        Ok(value) if value == b"0" => false,
        Ok(value) if value == b"1" => true,
        Ok(_) | Err(_) => return Ok(false),
    };
    if recorded_context != bind_observed_shell_environment(context, observes_underscore) {
        return Ok(false);
    }
    if let Some(expected) = target_lock_path {
        let artifact = match fs::read(entry.join("public-artifact")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::read(entry.join("artifact")) {
                    Ok(value) => value,
                    Err(_) => return Ok(false),
                }
            }
            Err(_) => return Ok(false),
        };
        let artifact = PathBuf::from(OsString::from_vec(artifact));
        if cargo_target_lock_path(&artifact).ok().as_deref() != Some(expected) {
            return Ok(false);
        }
    }
    let sources = match read_source_paths(&entry.join("sources")) {
        Ok(sources) => sources,
        Err(_) => return Ok(false),
    };
    let source_digest = match fs::read(entry.join("source-digest")) {
        Ok(value) => match <[u8; 32]>::try_from(value) {
            Ok(digest) => digest,
            Err(_) => return Ok(false),
        },
        Err(_) => return Ok(false),
    };
    source_revision_matches(probes, directory, &sources, source_digest)
}

fn source_revision_matches(
    probes: &mut HistoryProbeCache,
    directory: &Path,
    sources: &[PathBuf],
    expected: [u8; 32],
) -> Result<bool, String> {
    if probes.source_digest(directory, sources)? != expected {
        return Ok(false);
    }
    if probes.source_probe_is_current(directory, sources) {
        return Ok(true);
    }
    Ok(probes.refresh_source_digest(directory, sources)? == expected)
}

impl State {
    pub(super) fn load(directory: &Path, kind: StateKind) -> Result<Option<Self>, String> {
        let root = state_directory(directory, kind);
        Self::load_from(&root)
    }

    pub(super) fn load_lock_probe(
        directory: &Path,
        kind: StateKind,
    ) -> Result<Option<StateLockProbe>, String> {
        let root = state_directory(directory, kind);
        let artifact = match fs::read(root.join("artifact")) {
            Ok(value) => PathBuf::from(OsString::from_vec(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("could not read Cinder run state: {error}")),
        };
        let run_context = match fs::read(root.join("run-context")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("could not read Cinder run context: {error}")),
        };
        let observes_underscore = match fs::read(root.join("observes-underscore")) {
            Ok(value) if value == b"0" => false,
            Ok(value) if value == b"1" => true,
            Ok(_) => return Err("Cinder observed-environment state is invalid".to_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "could not read Cinder observed-environment state: {error}"
                ));
            }
        };
        let public_artifact = match fs::read(root.join("public-artifact")) {
            Ok(value) => PathBuf::from(OsString::from_vec(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => artifact,
            Err(error) => {
                return Err(format!(
                    "could not read public Cargo artifact path: {error}"
                ));
            }
        };
        Ok(Some(StateLockProbe {
            public_artifact,
            run_context,
            observes_underscore,
        }))
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
        let stage_started = Instant::now();
        let inputs = read_inputs(&inputs_path)?;
        super::patch::trace_run("read state inputs", stage_started);
        let project_topology_path = root.join("project-topology");
        if !project_topology_path.is_file() {
            return Ok(None);
        }
        let stage_started = Instant::now();
        let project_topology = read_project_topology(&project_topology_path)?;
        super::patch::trace_run("read state topology", stage_started);
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
        let stage_started = Instant::now();
        let cargo_outputs = read_cargo_outputs(&cargo_outputs_path)?;
        super::patch::trace_run("read state Cargo graph", stage_started);
        let cargo_fingerprints_current = match fs::read(root.join("cargo-fingerprints-current")) {
            Ok(value) if value == b"0" => false,
            Ok(value) if value == b"1" => true,
            Ok(_) => return Err("Cinder Cargo fingerprint state is invalid".to_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "could not read Cinder Cargo fingerprint state: {error}"
                ));
            }
        };
        let runtime_environment_path = root.join("runtime-environment");
        if !runtime_environment_path.is_file() {
            return Ok(None);
        }
        let runtime_environment = read_runtime_environment(&runtime_environment_path)?;
        let runtime_directory = match fs::read(root.join("runtime-directory")) {
            Ok(value) => {
                if value.len() <= 32 || value.len() > 32 + 1_048_576 {
                    return Err("Cinder runtime directory record has an invalid length".to_owned());
                }
                let (recorded_digest, path) = value.split_at(32);
                let actual_digest: [u8; 32] = Sha256::digest(path).into();
                if recorded_digest != actual_digest {
                    return Err("Cinder runtime directory integrity check failed".to_owned());
                }
                let directory = PathBuf::from(OsString::from_vec(path.to_vec()));
                if !directory.is_absolute() {
                    return Err("Cinder runtime directory is not absolute".to_owned());
                }
                Some(directory)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!("could not read Cinder runtime directory: {error}"));
            }
        };
        let compiler_recipe_path = root.join("compiler-recipe");
        let compiler_recipe = compiler_recipe_path
            .is_file()
            .then(|| read_compiler_recipe(&compiler_recipe_path).ok())
            .flatten();
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
            project_topology,
            sources,
            cargo_outputs,
            cargo_fingerprints_current,
            runtime_environment,
            runtime_directory,
            compiler_recipe,
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
        Self::record_fresh_with_runtime_directory(
            directory,
            kind,
            artifact,
            program_name,
            run_context,
            receipt,
            None,
        )
    }

    pub(super) fn record_fresh_test_execution(
        directory: &Path,
        artifact: &Path,
        program_name: &OsStr,
        run_context: &[u8],
        receipt: &ArtifactReceipt,
        runtime_directory: &Path,
    ) -> Result<(), String> {
        let runtime_directory = fs::canonicalize(runtime_directory)
            .map_err(|error| format!("could not resolve Cargo test working directory: {error}"))?;
        let manifest_directory = receipt
            .manifest_directory
            .as_deref()
            .ok_or_else(|| "Cargo test receipt has no package manifest".to_owned())?;
        let manifest_directory = fs::canonicalize(manifest_directory)
            .map_err(|error| format!("could not resolve Cargo test package directory: {error}"))?;
        if runtime_directory != manifest_directory {
            return Err(
                "Cargo test working directory did not match its artifact receipt".to_owned(),
            );
        }
        if !manifest_has_standard_library_test_harness_at(&runtime_directory)? {
            return Err("Cargo selected a nonstandard library test harness".to_owned());
        }
        Self::record_fresh_with_runtime_directory(
            directory,
            StateKind::Test,
            artifact,
            program_name,
            run_context,
            Some(receipt),
            Some(&runtime_directory),
        )
    }

    fn record_fresh_with_runtime_directory(
        directory: &Path,
        kind: StateKind,
        artifact: &Path,
        program_name: &OsStr,
        run_context: &[u8],
        receipt: Option<&ArtifactReceipt>,
        runtime_directory: Option<&Path>,
    ) -> Result<(), String> {
        let runtime_directory = runtime_directory
            .map(fs::canonicalize)
            .transpose()
            .map_err(|error| format!("could not resolve Cargo runtime directory: {error}"))?;
        let mut cargo_outputs = cargo_outputs_for_artifact(artifact, receipt)?;
        let unit_graph = compiler_unit_graph(&cargo_outputs, b"_")?;
        let observes_underscore = unit_graph.observes_environment;
        let build_script_directories = unit_graph.build_script_directories;
        let encoded_dependency_paths = unit_graph.encoded_dependency_paths;
        cargo_outputs.unit_fingerprints = unit_graph.fingerprints;
        cargo_outputs.unit_dependency_files = unit_graph.dependency_files;
        cargo_outputs.unit_artifacts = unit_graph.artifacts;
        cargo_outputs.unit_fingerprint_files = unit_graph.fingerprint_files;
        if env::var_os(super::TRACE_RUN).is_some() {
            eprintln!(
                "    Cinder trace: Cargo graph build-scripts={} receipt-build-scripts={}",
                build_script_directories.len(),
                receipt.map_or(0, |receipt| receipt.build_script_outputs.len()),
            );
        }
        let recorded_context = bind_observed_shell_environment(run_context, observes_underscore);
        if let Some(receipt) = receipt {
            let mut receipt_directories = BTreeSet::new();
            for output in &receipt.build_script_outputs {
                let directory = output.out_directory.parent().ok_or_else(|| {
                    format!(
                        "Cargo build-script output has no parent: {}",
                        output.out_directory.display()
                    )
                })?;
                receipt_directories.insert(fs::canonicalize(directory).map_err(|error| {
                    format!(
                        "could not resolve Cargo build-script receipt {}: {error}",
                        directory.display()
                    )
                })?);
            }
            if receipt_directories.into_iter().collect::<Vec<_>>() != build_script_directories {
                return Err(
                    "Cargo build-script message graph does not match its fingerprint graph"
                        .to_owned(),
                );
            }
        }
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
        let inherited_state = if receipt.is_none() {
            Self::load(directory, kind)
                .ok()
                .flatten()
                .and_then(|state| {
                    (state.artifact == artifact
                        && state.sources == sources
                        && state.artifact_is_unchanged().unwrap_or(false)
                        && state.inputs_are_unchanged(directory).unwrap_or(false))
                    .then_some((state.inputs, state.project_topology))
                })
        } else {
            None
        };
        let runtime_environment = if matches!(kind, StateKind::Run | StateKind::Test) {
            runtime_linker_environment()
        } else {
            Vec::new()
        };
        let selected_package_may_have_build_script =
            match receipt.and_then(|receipt| receipt.manifest_directory.as_deref()) {
                Some(manifest_directory) => package_may_have_build_script(manifest_directory)?,
                None => project_may_have_build_script(directory, &sources)?,
            };
        if inherited_state.is_none()
            && ((!build_script_directories.is_empty() && receipt.is_none())
                || (selected_package_may_have_build_script
                    && receipt
                        .and_then(|receipt| receipt.out_directory.as_ref())
                        .is_none()))
        {
            return Err(
                "Cargo produced no build-script output receipt for a package with a build script; using Cargo for safety"
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
            let (inputs, project_topology) = match &inherited_state {
                Some((inputs, project_topology)) => (inputs.clone(), project_topology.clone()),
                None => {
                    let (project_topology, project_inputs) =
                        project_topology_and_inputs(directory, artifact)?;
                    (
                        build_inputs(
                            directory,
                            artifact,
                            &sources,
                            BuildInputGraph {
                                dependency_file: &cargo_outputs.dependency_file,
                                unit_dependency_files: &cargo_outputs.unit_dependency_files,
                                encoded_dependency_paths: &encoded_dependency_paths,
                                receipt,
                                project_inputs: Some(&project_inputs),
                            },
                        )?,
                        project_topology,
                    )
                }
            };
            let newer_input = inputs
                .iter()
                .find(|input| input.identity.modified_ns > artifact_modified_ns);
            let inputs_unchanged = input_entries_are_unchanged(&inputs);
            let topology_unchanged =
                project_topology_is_unchanged(&project_topology, directory, artifact)?;
            let sources_unchanged = sources_are_unchanged(directory, &capture, &sources)?;
            if env::var_os(super::TRACE_RUN).is_some()
                && (newer_input.is_some()
                    || !inputs_unchanged
                    || !topology_unchanged
                    || !sources_unchanged)
            {
                eprintln!(
                    "    Cinder trace: recording race newer-input={} inputs={} topology={} sources={}",
                    newer_input
                        .map(|input| input.path.display().to_string())
                        .unwrap_or_else(|| "none".to_owned()),
                    inputs_unchanged,
                    topology_unchanged,
                    sources_unchanged,
                );
            }
            if newer_input.is_some()
                || !inputs_unchanged
                || !topology_unchanged
                || !sources_unchanged
            {
                return Err("build inputs changed while recording build state".to_owned());
            }
            let (artifact_file_identity, artifact_digest) = artifact_identity(artifact)?;
            if !input_entries_are_unchanged(&inputs)
                || !project_topology_is_unchanged(&project_topology, directory, artifact)?
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
                    project_topology: &project_topology,
                    sources: &sources,
                    cargo_outputs: &cargo_outputs,
                    cargo_fingerprints_current: true,
                    runtime_environment: &runtime_environment,
                    runtime_directory: runtime_directory.as_deref(),
                    compiler_recipe: receipt.and_then(|receipt| receipt.compiler_recipe.as_ref()),
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
                    project_topology: &self.project_topology,
                    sources: &self.sources,
                    cargo_outputs: &self.cargo_outputs,
                    cargo_fingerprints_current: false,
                    runtime_environment: &self.runtime_environment,
                    runtime_directory: self.runtime_directory.as_deref(),
                    compiler_recipe: self.compiler_recipe.as_ref(),
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
                    project_topology: &state.project_topology,
                    sources: &state.sources,
                    cargo_outputs: &state.cargo_outputs,
                    cargo_fingerprints_current: state.cargo_fingerprints_current,
                    runtime_environment: &state.runtime_environment,
                    runtime_directory: state.runtime_directory.as_deref(),
                    compiler_recipe: state.compiler_recipe.as_ref(),
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
        hasher.update(b"CINDER-BUILD-HISTORY-4");
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
        for output in self
            .cargo_outputs
            .unit_fingerprints
            .iter()
            .chain(&self.cargo_outputs.unit_dependency_files)
            .chain(&self.cargo_outputs.unit_artifacts)
        {
            append_context_value(&mut hasher, output.as_os_str().as_bytes());
        }
        for output in &self.cargo_outputs.unit_fingerprint_files {
            append_context_value(&mut hasher, output.path.as_os_str().as_bytes());
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
        probes: &mut HistoryProbeCache,
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
        let mut loaded_candidates = 0usize;
        for entry in entries {
            if !history_entry_may_match(&entry, directory, context, target_lock_path, probes)? {
                continue;
            }
            let state = match Self::load_from(&entry) {
                Ok(Some(state)) => {
                    loaded_candidates += 1;
                    state
                }
                Ok(None) | Err(_) => continue,
            };
            if !state.context_matches(context)
                || target_lock_path.is_some_and(|expected| {
                    cargo_target_lock_path(&state.public_artifact)
                        .ok()
                        .as_deref()
                        != Some(expected)
                })
            {
                continue;
            }
            if !source_revision_matches(probes, directory, &state.sources, state.source_digest)?
                || !state.artifact_is_unchanged().unwrap_or(false)
                || !state.cargo_outputs_are_present()
                || !state.inputs_match_revision(directory)?
                || entry.file_name().and_then(OsStr::to_str) != state.history_key().ok().as_deref()
                || !state.source_snapshot_matches_revision()?
                || !state.cached_artifact_is_trusted()?
            {
                continue;
            }
            touch_history_entry(&entry)?;
            if env::var_os(super::TRACE_RUN).is_some() {
                eprintln!(
                    "    Cinder trace: history source-probes={} loaded-candidates={loaded_candidates}",
                    probes.source_probes,
                );
            }
            return Ok(Some(state));
        }
        if env::var_os(super::TRACE_RUN).is_some() {
            eprintln!(
                "    Cinder trace: history source-probes={} loaded-candidates={loaded_candidates}",
                probes.source_probes,
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
                project_topology: &self.project_topology,
                sources: &self.sources,
                cargo_outputs: &self.cargo_outputs,
                cargo_fingerprints_current: false,
                runtime_environment: &self.runtime_environment,
                runtime_directory: self.runtime_directory.as_deref(),
                compiler_recipe: self.compiler_recipe.as_ref(),
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

    pub(super) fn compiler_recipe_supports_direct_check(&self) -> bool {
        self.compiler_recipe.as_ref().is_some_and(|recipe| {
            !recipe
                .environment
                .iter()
                .any(|(key, _)| key == OsStr::new("OUT_DIR"))
        })
    }

    pub(super) fn record_replayed_check(&self, directory: &Path) -> Result<(), String> {
        let sources = build_source_paths(
            directory,
            &self.artifact,
            &self.cargo_outputs.dependency_file,
        )?;
        if sources != self.sources {
            return Err("compiler replay changed the selected unit's source topology".to_owned());
        }
        if !self.inputs_are_unchanged(directory)? {
            return Err("build inputs changed while replaying the compiler".to_owned());
        }
        let capture = state_directory(directory, StateKind::Check)
            .with_extension(format!("replay-{}", std::process::id()));
        if capture.exists() {
            fs::remove_dir_all(&capture)
                .map_err(|error| format!("could not reset replayed source capture: {error}"))?;
        }
        fs::create_dir_all(&capture)
            .map_err(|error| format!("could not create replayed source capture: {error}"))?;
        make_private_directory(&capture)?;
        let result = (|| {
            snapshot_sources(directory, &capture, &sources)?;
            if !sources_are_unchanged(directory, &capture, &sources)?
                || !self.inputs_are_unchanged(directory)?
            {
                return Err("sources changed while recording compiler replay state".to_owned());
            }
            let source_digest = source_revision_digest(&capture, &sources)?;
            let (artifact_file_identity, artifact_digest) = artifact_identity(&self.artifact)?;
            if !sources_are_unchanged(directory, &capture, &sources)?
                || !self.inputs_are_unchanged(directory)?
            {
                return Err("sources changed while recording compiler replay state".to_owned());
            }
            Self::publish(
                directory,
                StateKind::Check,
                StatePublication {
                    source_root: &capture,
                    source_digest: &source_digest,
                    artifact: &self.artifact,
                    artifact_file_identity: &artifact_file_identity,
                    artifact_digest: Some(&artifact_digest),
                    public_artifact: &self.public_artifact,
                    program_name: &self.program_name,
                    literal_index: &[],
                    run_context: &self.run_context,
                    observes_underscore: self.observes_underscore,
                    inputs: &self.inputs,
                    project_topology: &self.project_topology,
                    sources: &sources,
                    cargo_outputs: &self.cargo_outputs,
                    cargo_fingerprints_current: false,
                    runtime_environment: &self.runtime_environment,
                    runtime_directory: self.runtime_directory.as_deref(),
                    compiler_recipe: self.compiler_recipe.as_ref(),
                    duplicate_ready: false,
                },
            )
        })();
        let _ = fs::remove_dir_all(capture);
        result
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
        self.public_artifact.is_file()
            && if self.cargo_fingerprints_current {
                self.cargo_outputs.are_unchanged()
            } else {
                self.cargo_outputs.are_present()
            }
    }

    pub(super) fn cargo_outputs_are_present(&self) -> bool {
        self.public_artifact.is_file() && self.cargo_outputs.are_present()
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
        project_topology_is_unchanged(&self.project_topology, directory, &self.public_artifact)
    }

    pub(super) fn inputs_match_revision(&self, directory: &Path) -> Result<bool, String> {
        if !input_entries_match_revision(&self.inputs)? {
            return Ok(false);
        }
        project_topology_is_unchanged(&self.project_topology, directory, &self.public_artifact)
    }

    pub(super) fn validated_test_runtime_directory(&self) -> Result<Option<PathBuf>, String> {
        let Some(runtime_directory) = self.runtime_directory.as_deref() else {
            return Ok(None);
        };
        let runtime_directory = match fs::canonicalize(runtime_directory) {
            Ok(resolved) if resolved == runtime_directory => resolved,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("could not validate Cargo test directory: {error}"));
            }
        };
        let manifest = match fs::canonicalize(runtime_directory.join("Cargo.toml")) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("could not validate Cargo test manifest: {error}"));
            }
        };
        if !self.inputs.iter().any(|input| input.path == manifest) {
            return Ok(None);
        }
        if !manifest_has_standard_library_test_harness_at(&runtime_directory)? {
            return Ok(None);
        }
        Ok(Some(runtime_directory))
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
        project_topology,
        sources,
        cargo_outputs,
        cargo_fingerprints_current,
        runtime_environment,
        runtime_directory,
        compiler_recipe,
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
    write_project_topology(&destination.join("project-topology"), project_topology)?;
    write_source_paths(&destination.join("sources"), sources)?;
    write_cargo_outputs(&destination.join("cargo-outputs"), cargo_outputs)?;
    fs::write(
        destination.join("cargo-fingerprints-current"),
        if cargo_fingerprints_current {
            b"1"
        } else {
            b"0"
        },
    )
    .map_err(|error| format!("could not record Cargo fingerprint state: {error}"))?;
    write_runtime_environment(
        &destination.join("runtime-environment"),
        runtime_environment,
    )?;
    if let Some(runtime_directory) = runtime_directory {
        let path = runtime_directory.as_os_str().as_bytes();
        let digest: [u8; 32] = Sha256::digest(path).into();
        let mut record = Vec::with_capacity(digest.len() + path.len());
        record.extend_from_slice(&digest);
        record.extend_from_slice(path);
        fs::write(destination.join("runtime-directory"), record)
            .map_err(|error| format!("could not record Cargo runtime directory: {error}"))?;
    }
    if let Some(compiler_recipe) = compiler_recipe {
        write_compiler_recipe(&destination.join("compiler-recipe"), compiler_recipe)?;
    }
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
