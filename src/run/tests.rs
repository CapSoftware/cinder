//! Unit tests for private runtime boundaries shared across the focused modules.

use super::{
    DISABLE_FAST_BUILD, LaunchPolicy, StateKind,
    cache::{
        directory_logical_bytes, project_namespace, prune_global_history_at, prune_run_artifacts,
        prune_stale_artifact_staging, prune_stale_global_staging, prune_stale_history_staging,
        prune_stale_project_staging, record_artifact_root, state_directory,
        state_project_directory,
    },
    capture::rustc_list_options,
    cargo::{
        cargo_config_contents_may_change_runner_or_target,
        cargo_config_contents_may_set_rustc_wrapper,
        manifest_contents_have_standard_library_test_harness, toml_string, unsupported_argument,
    },
    cargo_subcommand,
    context::{
        cargo_fingerprint_value, cargo_fingerprint_value_index, environment_affects_context,
        parse_encoded_dependency_environment,
    },
    diagnostics::{
        DiagnosticsReplay, hidden_pass_arguments, read_diagnostics, replay_region, strip_ansi,
        write_diagnostics,
    },
    inputs::{
        PROJECT_TOPOLOGY_MAGIC, add_build_script_inputs, artifact_file_identity, input_identity,
        primary_dependency_file, project_topology, project_topology_is_unchanged,
        read_cargo_outputs, read_inputs, read_project_topology, read_sibling_roots,
        read_source_paths, write_cargo_outputs, write_inputs, write_project_topology,
        write_sibling_roots, write_source_paths,
    },
    patch::{
        CodeSignatureContract, PatchMode, changed_format_segment, code_signature_contract,
        patch_artifact,
    },
    run_context,
    source::{LiteralChange, changed_plain_literal, source_literal_candidates},
    state::{
        ArtifactFileIdentity, ArtifactReceipt, BuildScriptOutput, CargoOutputEntry, CargoOutputs,
        InputEntry, LiteralIndexEntry, ProjectTopology, SiblingRoot, SourceRecord, State,
        StatePublication, read_artifact_receipt, write_artifact_receipt, write_state_directory,
    },
};
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[test]
fn artifact_receipt_round_trips_the_complete_build_script_graph_and_rejects_trailing_data() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-artifact-receipt-{unique}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("artifact.receipt");
    let receipt = ArtifactReceipt {
        artifact: root.join("target/debug/deps/libapp.rlib"),
        dependency_file: root.join("target/debug/deps/app.d"),
        public_file_name: OsString::from("libapp.rlib"),
        crate_type: "rlib".to_owned(),
        manifest_directory: Some(root.join("app")),
        out_directory: None,
        package_manifests: vec![
            root.join("app/Cargo.toml"),
            root.join("dependency-a/Cargo.toml"),
        ],
        build_script_outputs: vec![
            BuildScriptOutput {
                manifest_directory: root.join("dependency-a"),
                out_directory: root.join("target/debug/build/dependency-a/out"),
            },
            BuildScriptOutput {
                manifest_directory: root.join("dependency-b"),
                out_directory: root.join("target/debug/build/dependency-b/out"),
            },
        ],
        compiler_recipe: None,
    };

    write_artifact_receipt(&path, &receipt).unwrap();
    let read = read_artifact_receipt(&path).unwrap();
    assert_eq!(read.artifact, receipt.artifact);
    assert_eq!(read.dependency_file, receipt.dependency_file);
    assert_eq!(read.public_file_name, receipt.public_file_name);
    assert_eq!(read.crate_type, receipt.crate_type);
    assert_eq!(read.manifest_directory, receipt.manifest_directory);
    assert_eq!(read.out_directory, receipt.out_directory);
    assert_eq!(read.package_manifests, receipt.package_manifests);
    assert_eq!(read.build_script_outputs, receipt.build_script_outputs);

    let no_build_script = ArtifactReceipt {
        artifact: receipt.artifact.clone(),
        dependency_file: receipt.dependency_file.clone(),
        public_file_name: receipt.public_file_name.clone(),
        crate_type: receipt.crate_type.clone(),
        manifest_directory: receipt.manifest_directory.clone(),
        out_directory: None,
        package_manifests: receipt.package_manifests.clone(),
        build_script_outputs: Vec::new(),
        compiler_recipe: None,
    };
    let mut inputs = BTreeSet::new();
    add_build_script_inputs(&no_build_script, &mut inputs).unwrap();
    assert!(inputs.is_empty());

    use std::io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"unexpected")
        .unwrap();
    let error = read_artifact_receipt(&path)
        .err()
        .expect("receipt with trailing data must be rejected");
    assert!(error.contains("trailing data"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sibling_root_state_round_trips_and_rejects_trailing_or_relative_data() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-sibling-roots-{unique}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("sibling-roots");
    let sibling = SiblingRoot {
        artifact: root.join("target/debug/tool"),
        artifact_file_identity: ArtifactFileIdentity {
            size: 16,
            modified_ns: 42,
            device: 1,
            inode: 2,
            changed_seconds: 3,
            changed_nanoseconds: 4,
        },
        artifact_digest: [7; 32],
        dependency_file: root.join("target/debug/deps/tool-abc.d"),
        hashed_artifact: root.join("target/debug/deps/tool-abc"),
        fingerprint: root.join("target/debug/.fingerprint/tool-abc"),
    };

    write_sibling_roots(&path, std::slice::from_ref(&sibling)).unwrap();
    let read = read_sibling_roots(&path).unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].artifact, sibling.artifact);
    assert_eq!(
        read[0].artifact_file_identity,
        sibling.artifact_file_identity
    );
    assert_eq!(read[0].artifact_digest, sibling.artifact_digest);
    assert_eq!(read[0].dependency_file, sibling.dependency_file);
    assert_eq!(read[0].hashed_artifact, sibling.hashed_artifact);
    assert_eq!(read[0].fingerprint, sibling.fingerprint);

    write_sibling_roots(&path, &[]).unwrap();
    assert!(read_sibling_roots(&path).unwrap().is_empty());

    let mut relative = sibling.clone();
    relative.dependency_file = PathBuf::from("deps/tool-abc.d");
    assert!(
        write_sibling_roots(&path, std::slice::from_ref(&relative))
            .unwrap_err()
            .contains("absolute")
    );

    write_sibling_roots(&path, std::slice::from_ref(&sibling)).unwrap();
    use std::io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"unexpected")
        .unwrap();
    assert!(
        read_sibling_roots(&path)
            .unwrap_err()
            .contains("trailing data")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_state_round_trips_records_and_rejects_trailing_or_mismatched_data() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("cinder-sources-{unique}-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("sources");
    let sources = vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/main.rs")];
    let records = vec![
        SourceRecord {
            identity: ArtifactFileIdentity {
                size: 16,
                modified_ns: 42,
                device: 1,
                inode: 2,
                changed_seconds: 3,
                changed_nanoseconds: 4,
            },
            digest: [7; 32],
        },
        SourceRecord {
            identity: ArtifactFileIdentity {
                size: 61,
                modified_ns: 43,
                device: 1,
                inode: 5,
                changed_seconds: 6,
                changed_nanoseconds: 7,
            },
            digest: [8; 32],
        },
    ];

    write_source_paths(&path, &sources, &records).unwrap();
    let (read_sources, read_records) = read_source_paths(&path).unwrap();
    assert_eq!(read_sources, sources);
    assert_eq!(read_records, records);

    assert!(
        write_source_paths(&path, &sources, &records[..1])
            .unwrap_err()
            .contains("do not describe")
    );

    write_source_paths(&path, &sources, &records).unwrap();
    use std::io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"unexpected")
        .unwrap();
    assert!(
        read_source_paths(&path)
            .unwrap_err()
            .contains("trailing data")
    );

    // The previous format has a different magic and is an ordinary miss.
    let mut stale = b"CNDS0001".to_vec();
    stale.extend_from_slice(&0_u64.to_le_bytes());
    fs::write(&path, stale).unwrap();
    assert!(
        read_source_paths(&path)
            .unwrap_err()
            .contains("unsupported format")
    );
    fs::remove_dir_all(root).unwrap();
}

fn minimal_state_for(root: &Path) -> State {
    State {
        snapshot: root.join("snapshot"),
        source_digest: [0; 32],
        artifact: root.join("artifact"),
        public_artifact: root.join("artifact"),
        program_name: OsString::from("artifact"),
        artifact_file_identity: ArtifactFileIdentity {
            size: 0,
            modified_ns: 0,
            device: 0,
            inode: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        },
        artifact_digest: None,
        literal_index: Vec::new(),
        run_context: Vec::new(),
        observes_underscore: false,
        inputs: Vec::new(),
        project_topology: ProjectTopology {
            digest: [0; 32],
            directories: Vec::new(),
        },
        sources: Vec::new(),
        source_records: Vec::new(),
        cargo_outputs: CargoOutputs {
            dependency_file: PathBuf::new(),
            artifact: PathBuf::new(),
            fingerprint: PathBuf::new(),
            unit_fingerprints: Vec::new(),
            unit_dependency_files: Vec::new(),
            unit_artifacts: Vec::new(),
            unit_fingerprint_files: Vec::new(),
        },
        cargo_fingerprints_current: false,
        runtime_environment: Vec::new(),
        runtime_directory: None,
        compiler_recipe: None,
        diagnostics: DiagnosticsReplay::None,
        sibling_roots: Vec::new(),
    }
}

#[test]
fn source_revision_identities_trust_unchanged_files_and_reject_changed_content() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-source-revision-{unique}-{}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    let live = root.join("src/lib.rs");
    fs::write(&live, b"pub fn touched() -> u32 { 7 }\n").unwrap();
    let (identity, digest) = input_identity(&live).unwrap();
    let mut state = minimal_state_for(&root);
    state.sources = vec![PathBuf::from("src/lib.rs")];
    state.source_records = vec![SourceRecord { identity, digest }];

    // Unchanged identity is trusted without reading the file.
    assert!(state.sources_match_revision(&root).unwrap());

    // A touched file with identical bytes still matches the revision.
    let restored = fs::read(&live).unwrap();
    fs::write(&live, &restored).unwrap();
    assert!(state.sources_match_revision(&root).unwrap());

    // A same-length content change misses even with a restored timestamp.
    let earlier = fs::metadata(&live).unwrap().modified().unwrap();
    fs::write(&live, b"pub fn touched() -> u32 { 9 }\n").unwrap();
    let file = fs::OpenOptions::new().write(true).open(&live).unwrap();
    file.set_modified(earlier).unwrap();
    drop(file);
    assert!(!state.sources_match_revision(&root).unwrap());

    // A record list that does not describe the sources is a miss.
    state.source_records.clear();
    assert!(!state.sources_match_revision(&root).unwrap());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cargo_output_state_round_trips_the_complete_unit_graph_and_rejects_trailing_data() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-cargo-outputs-{unique}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("cargo-outputs");
    let outputs = CargoOutputs {
        dependency_file: root.join("target/debug/deps/app.d"),
        artifact: root.join("target/debug/deps/libapp.rmeta"),
        fingerprint: root.join("target/debug/.fingerprint/app-selected"),
        unit_fingerprints: vec![
            root.join("target/debug/.fingerprint/app-selected"),
            root.join("target/debug/.fingerprint/dependency-unit"),
        ],
        unit_dependency_files: vec![
            root.join("target/debug/deps/app.d"),
            root.join("target/debug/deps/dependency.d"),
        ],
        unit_artifacts: vec![
            root.join("target/debug/deps/libapp.rmeta"),
            root.join("target/debug/deps/libdependency.rmeta"),
        ],
        unit_fingerprint_files: vec![CargoOutputEntry {
            path: root.join("target/debug/.fingerprint/app-selected/lib-app"),
            identity: ArtifactFileIdentity {
                size: 16,
                modified_ns: 42,
                device: 1,
                inode: 2,
                changed_seconds: 3,
                changed_nanoseconds: 4,
            },
        }],
    };

    write_cargo_outputs(&path, &outputs).unwrap();
    let valid_state = fs::read(&path).unwrap();
    let read = read_cargo_outputs(&path).unwrap();
    assert_eq!(read.dependency_file, outputs.dependency_file);
    assert_eq!(read.artifact, outputs.artifact);
    assert_eq!(read.fingerprint, outputs.fingerprint);
    assert_eq!(read.unit_fingerprints, outputs.unit_fingerprints);
    assert_eq!(read.unit_dependency_files, outputs.unit_dependency_files);
    assert_eq!(read.unit_artifacts, outputs.unit_artifacts);
    assert_eq!(
        read.unit_fingerprint_files[0].path,
        outputs.unit_fingerprint_files[0].path
    );
    assert_eq!(
        read.unit_fingerprint_files[0].identity,
        outputs.unit_fingerprint_files[0].identity
    );

    use std::io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"unexpected")
        .unwrap();
    let error = read_cargo_outputs(&path)
        .err()
        .expect("Cargo output state with trailing data must be rejected");
    assert!(error.contains("trailing data"));
    fs::write(&path, &valid_state[..valid_state.len() - 1]).unwrap();
    let error = read_cargo_outputs(&path)
        .err()
        .expect("truncated Cargo output state must be rejected");
    assert!(error.contains("truncated"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_topology_state_round_trips_and_rejects_trailing_data() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-project-topology-{unique}-{}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    let topology = project_topology(&root, &root.join("target/debug/app")).unwrap();
    let path = root.join("project-topology");

    write_project_topology(&path, &topology).unwrap();
    let valid_state = fs::read(&path).unwrap();
    let read = read_project_topology(&path).unwrap();
    assert_eq!(read.digest, topology.digest);
    assert_eq!(read.directories.len(), topology.directories.len());
    for (read, written) in read.directories.iter().zip(&topology.directories) {
        assert_eq!(read.path, written.path);
        assert_eq!(read.identity, written.identity);
        assert_eq!(read.subtree_digest, written.subtree_digest);
    }

    let mut obsolete = valid_state.clone();
    obsolete[..PROJECT_TOPOLOGY_MAGIC.len()].copy_from_slice(b"CNDT0002");
    fs::write(&path, obsolete).unwrap();
    let error = read_project_topology(&path)
        .err()
        .expect("topology state without subtree evidence must be rejected");
    assert!(error.contains("unsupported format"));
    fs::write(&path, &valid_state).unwrap();

    use std::io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"unexpected")
        .unwrap();
    let error = read_project_topology(&path)
        .err()
        .expect("topology state with trailing data must be rejected");
    assert!(error.contains("trailing data"));
    fs::write(&path, &valid_state[..valid_state.len() - 1]).unwrap();
    let error = read_project_topology(&path)
        .err()
        .expect("truncated topology state must be rejected");
    assert!(error.contains("truncated"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_topology_rescans_only_a_changed_subtree_and_rejects_new_rust_paths() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-project-subtree-{unique}-{}",
        std::process::id()
    ));
    let source = root.join("src");
    fs::create_dir_all(&source).unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(source.join("lib.rs"), "pub fn value() {}\n").unwrap();
    let artifact = root.join("target/debug/app");
    let topology = project_topology(&root, &artifact).unwrap();

    let editor_staging = source.join(".editor-staging");
    fs::write(&editor_staging, b"temporary").unwrap();
    fs::remove_file(editor_staging).unwrap();
    assert!(project_topology_is_unchanged(&topology, &root, &artifact).unwrap());

    fs::write(source.join("new_module.rs"), "pub fn added() {}\n").unwrap();
    assert!(!project_topology_is_unchanged(&topology, &root, &artifact).unwrap());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_topology_ignores_unrelated_ancestor_churn_but_detects_cargo_controls() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let container = std::env::temp_dir().join(format!(
        "cinder-project-controls-{unique}-{}",
        std::process::id()
    ));
    let root = container.join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    let artifact = root.join("target/debug/app");
    let topology = project_topology(&root, &artifact).unwrap();

    let unrelated = container.join("unrelated-editor-file");
    fs::write(&unrelated, b"temporary").unwrap();
    fs::remove_file(unrelated).unwrap();
    assert!(project_topology_is_unchanged(&topology, &root, &artifact).unwrap());

    fs::create_dir(container.join(".cargo")).unwrap();
    fs::write(container.join(".cargo/config.toml"), "[build]\n").unwrap();
    assert!(!project_topology_is_unchanged(&topology, &root, &artifact).unwrap());
    fs::remove_dir_all(container).unwrap();
}

#[test]
fn input_state_round_trips_ordered_absolute_entries_and_rejects_trailing_data() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-input-state-{unique}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let first = root.join("first");
    let second = root.join("second");
    fs::write(&first, b"one").unwrap();
    fs::write(&second, b"two").unwrap();
    let inputs = [&first, &second]
        .into_iter()
        .map(|path| {
            let (identity, digest) = input_identity(path).unwrap();
            InputEntry {
                path: path.clone(),
                identity,
                digest,
            }
        })
        .collect::<Vec<_>>();
    let path = root.join("inputs");

    write_inputs(&path, &inputs).unwrap();
    let valid_state = fs::read(&path).unwrap();
    let read = read_inputs(&path).unwrap();
    assert_eq!(read.len(), inputs.len());
    for (read, written) in read.iter().zip(&inputs) {
        assert_eq!(read.path, written.path);
        assert_eq!(read.identity, written.identity);
        assert_eq!(read.digest, written.digest);
    }

    use std::io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"unexpected")
        .unwrap();
    let error = read_inputs(&path)
        .err()
        .expect("input state with trailing data must be rejected");
    assert!(error.contains("trailing data"));
    fs::write(&path, &valid_state[..valid_state.len() - 1]).unwrap();
    let error = read_inputs(&path)
        .err()
        .expect("truncated input state must be rejected");
    assert!(error.contains("truncated"));
    fs::remove_dir_all(root).unwrap();
}

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
fn public_artifact_mapping_requires_the_exact_cargo_fingerprint() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-artifact-mapping-{}-{unique}",
        std::process::id()
    ));
    let profile = root.join("target/debug");
    let dependencies = profile.join("deps");
    fs::create_dir_all(&dependencies).unwrap();
    let public = profile.join("app");
    let correct = dependencies.join("app-a1b2c3d4");
    let stale = dependencies.join("app-deadbeef");
    fs::write(&public, b"correct!").unwrap();
    fs::write(&correct, b"correct!").unwrap();
    fs::write(&stale, b"stale!!!").unwrap();
    fs::write(correct.with_extension("d"), b"app: src/main.rs\n").unwrap();
    fs::write(stale.with_extension("d"), b"app: src/main.rs\n").unwrap();
    fs::create_dir_all(profile.join(".fingerprint/app-a1b2c3d4")).unwrap();
    let modified = fs::metadata(&public).unwrap().modified().unwrap();
    for artifact in [&correct, &stale] {
        fs::File::open(artifact)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }

    assert_eq!(
        primary_dependency_file(&public).unwrap(),
        correct.with_extension("d")
    );
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
        unit_fingerprints: Vec::new(),
        unit_dependency_files: Vec::new(),
        unit_artifacts: Vec::new(),
        unit_fingerprint_files: Vec::new(),
    };
    let topology = project_topology(&source_root, &cached).unwrap();
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
            project_topology: &topology,
            sources: &sources,
            source_records: &[SourceRecord {
                identity,
                digest: [9_u8; 32],
            }],
            cargo_outputs: &cargo_outputs,
            cargo_fingerprints_current: true,
            runtime_environment: &[],
            runtime_directory: None,
            compiler_recipe: None,
            diagnostics: &DiagnosticsReplay::None,
            sibling_roots: &[],
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
        "check.capture-2147483647",
        "test.capture-2147483647",
        "run.patch-2147483647",
        "build.patch-2147483647",
        "run.tmp-2147483647-1",
        "build.tmp-2147483647-1",
        "check.tmp-2147483647-1",
        "test.tmp-2147483647-1",
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
        project_topology: ProjectTopology {
            digest: [0; 32],
            directories: Vec::new(),
        },
        sources: Vec::new(),
        source_records: Vec::new(),
        cargo_outputs: CargoOutputs {
            dependency_file: PathBuf::new(),
            artifact: PathBuf::new(),
            fingerprint: PathBuf::new(),
            unit_fingerprints: Vec::new(),
            unit_dependency_files: Vec::new(),
            unit_artifacts: Vec::new(),
            unit_fingerprint_files: Vec::new(),
        },
        cargo_fingerprints_current: false,
        runtime_environment: Vec::new(),
        runtime_directory: None,
        compiler_recipe: None,
        diagnostics: DiagnosticsReplay::None,
        sibling_roots: Vec::new(),
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

    let CodeSignatureContract::AdHoc(metadata) = code_signature_contract(&artifact).unwrap() else {
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
fn finds_commands_after_supported_global_cargo_options() {
    let command = |arguments: &[&str]| {
        let arguments: Vec<_> = arguments.iter().map(OsString::from).collect();
        cargo_subcommand(&arguments).map(str::to_owned)
    };

    assert_eq!(
        command(&["+stable", "--locked", "check"]),
        Some("check".into())
    );
    assert_eq!(command(&["--color", "always", "b"]), Some("b".into()));
    assert_eq!(
        command(&["--config=net.offline=true", "clean"]),
        Some("clean".into())
    );
    assert_eq!(command(&["-vv", "run", "--", "--help"]), Some("run".into()));
    assert_eq!(command(&["--version"]), None);
    assert_eq!(command(&["--explain", "E0001", "build"]), None);
    assert_eq!(command(&["--explain=E0001", "build"]), None);
    assert_eq!(command(&["--future-global", "build"]), None);
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
    assert!(!environment_affects_context(OsStr::new(super::TRACE_RUN)));
    assert!(!environment_affects_context(OsStr::new(
        super::SYNCHRONOUS_STATE_RECORDING
    )));
    assert!(!environment_affects_context(OsStr::new(
        "CINDER_REAL_CARGO"
    )));
    assert!(!environment_affects_context(OsStr::new(
        "CINDER_COALESCE_RUN_EVENTS"
    )));
    assert!(environment_affects_context(OsStr::new("CINDER_UNKNOWN")));
    assert!(environment_affects_context(OsStr::new("RUSTFLAGS")));
    assert!(environment_affects_context(OsStr::new("BUN_CODEGEN_DIR")));
    assert!(!environment_affects_context(OsStr::new(
        "CINDER_RUN_CONTEXT_FILE"
    )));
    assert!(!environment_affects_context(OsStr::new(DISABLE_FAST_BUILD)));
    assert!(!environment_affects_context(OsStr::new(
        crate::usage::USAGE_ENVIRONMENT
    )));
    assert!(!environment_affects_context(OsStr::new(
        super::EXPERIMENTAL_DIRECT_CHECK
    )));
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
fn reads_environment_keys_from_cargos_encoded_dependency_info() {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(&1_u32.to_le_bytes());
    encoded.push(u8::MAX);
    encoded.push(1);
    encoded.extend_from_slice(&0_u32.to_le_bytes());
    encoded.extend_from_slice(&2_u32.to_le_bytes());
    for (key, value) in [
        (b"_".as_slice(), None),
        (b"RUSTFLAGS".as_slice(), Some(b"-Copt".as_slice())),
    ] {
        encoded.extend_from_slice(&(key.len() as u32).to_le_bytes());
        encoded.extend_from_slice(key);
        match value {
            Some(value) => {
                encoded.push(1);
                encoded.extend_from_slice(&(value.len() as u32).to_le_bytes());
                encoded.extend_from_slice(value);
            }
            None => encoded.push(0),
        }
    }

    assert_eq!(
        parse_encoded_dependency_environment(&encoded, b"_"),
        Some(true)
    );
    assert_eq!(
        parse_encoded_dependency_environment(&encoded, b"CINDER_UNKNOWN"),
        Some(false)
    );
    assert_eq!(
        parse_encoded_dependency_environment(&encoded[..5], b"_"),
        None
    );
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
    assert!(unsupported_argument("-C"));
    assert!(unsupported_argument("-C../other"));
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
    assert!(!cargo_config_contents_may_set_rustc_wrapper(zed_style).unwrap());
    assert!(
        cargo_config_contents_may_set_rustc_wrapper("[build]\nrustc-wrapper = \"sccache\"\n")
            .unwrap()
    );
    assert!(
        cargo_config_contents_may_set_rustc_wrapper(
            "[build]\nrustc-workspace-wrapper = \"workspace-cache\"\n"
        )
        .unwrap()
    );
}

#[test]
fn direct_test_execution_requires_the_standard_library_harness() {
    assert!(
        manifest_contents_have_standard_library_test_harness(
            "[package]\nname='app'\nversion='0.1.0'\n"
        )
        .unwrap()
    );
    assert!(
        manifest_contents_have_standard_library_test_harness(
            "[package]\nname='app'\nversion='0.1.0'\n[lib]\nharness=true\n"
        )
        .unwrap()
    );
    assert!(
        !manifest_contents_have_standard_library_test_harness(
            "[package]\nname='app'\nversion='0.1.0'\n[lib]\nharness=false\n"
        )
        .unwrap()
    );
    assert!(
        !manifest_contents_have_standard_library_test_harness("[workspace]\nmembers=['app']\n")
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
fn rejects_literal_data_rustc_refuses_to_compile() {
    // U+202E (right-to-left override) is three UTF-8 bytes, the same length as
    // the three ASCII bytes it replaces, but rustc denies it in literals.
    let old = "fn label() { show(\"Camera abc Preview\"); }".as_bytes();
    let new = "fn label() { show(\"Camera \u{202E} Preview\"); }".as_bytes();
    assert_eq!(old.len(), new.len());
    assert!(changed_plain_literal(old, new).is_none());

    // U+2066 (left-to-right isolate) inside a format-literal suffix.
    let old = "fn value() { format!(\"{} cinder-abc-one\", value); }".as_bytes();
    let new = "fn value() { format!(\"{} cinder-\u{2066}-one\", value); }".as_bytes();
    assert_eq!(old.len(), new.len());
    assert!(changed_plain_literal(old, new).is_none());

    // A bare carriage return in a string literal is a hard rustc error.
    let old = b"fn label() { show(\"Camera a Preview\"); }";
    let new = b"fn label() { show(\"Camera \r Preview\"); }";
    assert_eq!(old.len(), new.len());
    assert!(changed_plain_literal(old, new).is_none());
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

#[test]
fn replay_region_extracts_diagnostics_before_the_finished_line() {
    let stderr = b"warning: unused variable: `unused`\n --> src/lib.rs:2:9\n\nwarning: `probe` (lib) generated 1 warning\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.01s\n";
    let region = replay_region(stderr).unwrap();
    assert_eq!(
        region,
        b"warning: unused variable: `unused`\n --> src/lib.rs:2:9\n\nwarning: `probe` (lib) generated 1 warning\n"
    );

    let quiet = b"    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.00s\n";
    assert_eq!(replay_region(quiet).unwrap(), b"");
}

#[test]
fn replay_region_rejects_non_quiet_passes() {
    let compiling = b"   Compiling probe v0.1.0 (/tmp/probe)\n    Finished `dev` profile in 0.4s\n";
    assert!(replay_region(compiling).is_err());
    assert!(replay_region(b"warning: something\n").is_err());
    let twice = b"    Finished `dev` profile in 0.1s\n    Finished `dev` profile in 0.1s\n";
    assert!(replay_region(twice).is_err());
    let trailing = b"    Finished `dev` profile in 0.1s\nerror: unexpected\n";
    assert!(replay_region(trailing).is_err());
}

#[test]
fn replay_region_strips_transient_lock_wait_lines() {
    // A concurrent Cargo briefly holding a shared lock only delays the pass;
    // an uncontended no-change command prints the same bytes without the
    // `Blocking` status lines, so they are removed rather than failing
    // recording on a busy machine.
    let blocking = b"    Blocking waiting for file lock on package cache\nwarning: unused\n    Blocking waiting for file lock on build directory\n    Finished `dev` profile in 0.1s\n";
    assert_eq!(replay_region(blocking).unwrap(), b"warning: unused\n");
    let only_blocking =
        b"    Blocking waiting for file lock on package cache\n    Finished `dev` profile in 0.1s\n";
    assert_eq!(replay_region(only_blocking).unwrap(), b"");
}

#[test]
fn replay_region_accepts_test_executable_lines_and_ansi_status() {
    let no_run = b"warning: unused\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.01s\n  Executable unittests src/lib.rs (target/debug/deps/probe-1234)\n";
    assert_eq!(replay_region(no_run).unwrap(), b"warning: unused\n");

    let ansi = b"\x1b[1m\x1b[33mwarning\x1b[0m: unused\n\x1b[1m\x1b[32m    Finished\x1b[0m `dev` profile in 0.01s\n";
    assert_eq!(
        replay_region(ansi).unwrap(),
        b"\x1b[1m\x1b[33mwarning\x1b[0m: unused\n"
    );
}

#[test]
fn strip_ansi_removes_escape_sequences_for_classification_only() {
    assert_eq!(
        strip_ansi(b"\x1b[1m\x1b[32m    Finished\x1b[0m `dev` profile"),
        b"    Finished `dev` profile"
    );
    assert_eq!(strip_ansi(b"plain text"), b"plain text");
}

#[test]
fn hidden_pass_arguments_derive_no_change_command_shapes() {
    let arguments =
        |values: &[&str]| -> Vec<OsString> { values.iter().map(OsString::from).collect() };
    let artifact = Path::new("/tmp/target/debug/app-name");

    let check =
        hidden_pass_arguments(&arguments(&["check", "--lib"]), StateKind::Check, artifact).unwrap();
    assert_eq!(check, arguments(&["check", "--lib"]));

    let test = hidden_pass_arguments(
        &arguments(&["test", "--lib", "--", "--nocapture"]),
        StateKind::Test,
        artifact,
    )
    .unwrap();
    assert_eq!(test, arguments(&["test", "--lib", "--no-run"]));

    let no_run = hidden_pass_arguments(
        &arguments(&["test", "--no-run", "--lib"]),
        StateKind::Test,
        artifact,
    )
    .unwrap();
    assert_eq!(no_run, arguments(&["test", "--no-run", "--lib"]));

    let run = hidden_pass_arguments(
        &arguments(&["run", "-p", "pkg", "--", "value"]),
        StateKind::Run,
        artifact,
    )
    .unwrap();
    assert_eq!(run, arguments(&["build", "-p", "pkg", "--bin", "app-name"]));

    let example = hidden_pass_arguments(
        &arguments(&["run", "--example", "demo"]),
        StateKind::Run,
        artifact,
    )
    .unwrap();
    assert_eq!(example, arguments(&["build", "--example", "demo"]));
}

#[test]
fn diagnostic_replay_state_round_trips_and_rejects_malformed_records() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-diagnostics-state-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("diagnostics");

    for diagnostics in [
        DiagnosticsReplay::None,
        DiagnosticsReplay::Pinned(b"warning: pinned\n".to_vec()),
        DiagnosticsReplay::Both {
            plain: b"warning: plain\n".to_vec(),
            ansi: b"\x1b[33mwarning\x1b[0m: ansi\n".to_vec(),
        },
    ] {
        write_diagnostics(&path, &diagnostics).unwrap();
        assert_eq!(read_diagnostics(&path).unwrap(), diagnostics);
    }

    let valid = fs::read(&path).unwrap();
    let mut trailing = valid.clone();
    trailing.push(0);
    fs::write(&path, &trailing).unwrap();
    assert!(read_diagnostics(&path).is_err());

    fs::write(&path, b"BADMAGIC").unwrap();
    assert!(read_diagnostics(&path).is_err());

    let mut truncated = valid;
    truncated.truncate(10);
    fs::write(&path, &truncated).unwrap();
    assert!(read_diagnostics(&path).is_err());

    assert!(write_diagnostics(&path, &DiagnosticsReplay::Pinned(Vec::new())).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn diagnostic_replay_variant_follows_the_stderr_terminal() {
    let both = DiagnosticsReplay::Both {
        plain: b"plain\n".to_vec(),
        ansi: b"ansi\n".to_vec(),
    };
    assert_eq!(both.replay_bytes(true).unwrap(), b"ansi\n");
    assert_eq!(both.replay_bytes(false).unwrap(), b"plain\n");
    let pinned = DiagnosticsReplay::Pinned(b"pinned\n".to_vec());
    assert_eq!(pinned.replay_bytes(true).unwrap(), b"pinned\n");
    assert_eq!(pinned.replay_bytes(false).unwrap(), b"pinned\n");
    assert!(DiagnosticsReplay::None.replay_bytes(true).is_none());
}
