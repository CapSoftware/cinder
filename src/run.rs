//! Cargo-compatible command acceleration and runtime orchestration.

use std::{
    collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher},
    env,
    ffi::{OsStr, OsString},
    fs,
    hash::{Hash, Hasher},
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Range,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::UNIX_EPOCH,
    time::{Duration, Instant, SystemTime},
};

use aho_corasick::AhoCorasick;
use fs2::FileExt;
use memmap2::MmapOptions;
use sha2::{Digest, Sha256};

const DISABLE_FAST_RUN: &str = "CINDER_DISABLE_FAST_RUN";
const DISABLE_FAST_BUILD: &str = "CINDER_DISABLE_FAST_BUILD";
const DISABLE_FAST_CHECK: &str = "CINDER_DISABLE_FAST_CHECK";
const DISABLE_FAST_TEST: &str = "CINDER_DISABLE_FAST_TEST";
pub const EXPERIMENTAL_DIRECT_CHECK: &str = "CINDER_EXPERIMENTAL_DIRECT_CHECK";
pub const TRACE_RUN: &str = "CINDER_TRACE_RUN";
pub const SYNCHRONOUS_STATE_RECORDING: &str = "CINDER_SYNCHRONOUS_STATE_RECORDING";
const COALESCE_RUN_EVENTS: &str = "CINDER_COALESCE_RUN_EVENTS";
const DUPLICATE_EVENT_SETTLE_TIME: Duration = Duration::from_millis(1_250);
const DUPLICATE_EVENT_MAX_AGE: Duration = Duration::from_secs(2);
const REVISION_HISTORY_LIMIT: usize = 8;
const REVISION_HISTORY_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const REVISION_HISTORY_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const STAGING_MAX_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchPolicy {
    Immediate,
    CoalesceDuplicateEvents,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StateKind {
    Run,
    Build,
    Check,
    Test,
}

impl StateKind {
    const fn directory_name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Build => "build",
            Self::Check => "check",
            Self::Test => "test",
        }
    }

    fn history_directory_name(self) -> String {
        format!("{}-history", self.directory_name())
    }
}

impl LaunchPolicy {
    pub fn for_invocation(invoked_as_cargo: bool) -> Self {
        if invoked_as_cargo || env::var_os(COALESCE_RUN_EVENTS).is_some() {
            Self::CoalesceDuplicateEvents
        } else {
            Self::Immediate
        }
    }

    const fn context_byte(self) -> u8 {
        match self {
            Self::Immediate => 0,
            Self::CoalesceDuplicateEvents => 1,
        }
    }

    const fn coalesces_duplicate_events(self) -> bool {
        matches!(self, Self::CoalesceDuplicateEvents)
    }
}

mod cache;
mod capture;
mod cargo;
mod context;
mod diagnostics;
mod envprobe;
mod inputs;
mod messages;
mod observe;
mod patch;
mod replay;
mod source;
mod state;

pub use capture::{
    cargo_arguments, cargo_subcommand, stage_artifact_receipts, stage_run_context,
    stage_test_context,
};
pub use cargo::{artifact_capture_eligible, clear_project_state, test_execution_eligible};
#[cfg(test)]
pub use context::run_context;
pub use context::run_context_with_cargo;
pub use diagnostics::stage_cargo_invocation;
pub use envprobe::EnvironmentWitness;
pub use messages::{PackageSelection, run_cargo_messages, selected_package};

use cache::{
    history_directory, history_recency, is_cinder_run_artifact, make_private_directory,
    parse_state_number, project_namespace, prune_global_history, prune_history,
    prune_run_artifacts, record_artifact_root, remove_directory_if_present, state_directory,
    state_project_directory, touch_history_entry,
};
use capture::cargo_subcommand_index;
use cargo::{
    build_eligible, canonical_current_directory, cargo_config_pins_term_color, check_eligible,
    eligible, host_target, manifest_has_standard_library_test_harness_at, test_eligible,
    test_executes, toml_string,
};
use context::{append_context_value, bind_observed_shell_environment, compiler_unit_graph};
use diagnostics::{
    DiagnosticsReplay, capture_replay_diagnostics, read_cargo_invocation, read_diagnostics,
    write_diagnostics,
};
use inputs::{
    BuildInputGraph, RUNTIME_LINKER_ENVIRONMENT_KEYS, StateReader, artifact_file_identity,
    artifact_file_identity_from_metadata, artifact_identity, artifact_metadata, build_inputs,
    build_source_paths, cargo_outputs_for_artifact, cargo_profile_directory,
    dependency_output_paths, input_entries_are_unchanged, input_entries_match_revision,
    input_identity, package_may_have_build_script, project_may_have_build_script,
    project_topology_and_inputs, project_topology_is_unchanged, read_bounded_state,
    read_cargo_outputs, read_inputs, read_project_topology, read_runtime_environment,
    read_sibling_roots, read_source_paths, runtime_linker_environment, write_cargo_outputs,
    write_inputs, write_project_topology, write_runtime_environment, write_sibling_roots,
    write_source_paths, write_state_bytes,
};
use observe::CompilerObserver;
use patch::{
    PatchMode, changed_format_segment, clone_file, make_cached_artifact_read_only,
    make_owner_writable, patch_artifact, remove_launch_xattrs,
};
use replay::{CompilerRecipe, read_compiler_recipe, replay_compiler, write_compiler_recipe};
use source::{
    HistoryProbeCache, LiteralChange, build_literal_index, find_literal_change, read_literal_index,
    snapshot_sources, source_revision_digest, sources_are_unchanged, write_literal_index,
};
use state::{
    ArtifactFileIdentity, ArtifactReceipt, BuildScriptOutput, CargoOutputEntry, CargoOutputs,
    InputEntry, LiteralIndexEntry, MAX_STATE_ROOTS, ProjectTopology, SiblingRoot, SourceRecord,
    State, TopologyDirectory, read_artifact_receipt, write_artifact_receipt,
};

/// Attempts a source-to-artifact transformation before asking Cargo to build.
/// A miss is deliberately silent: normal Cargo remains the compatibility path.
pub fn try_fast_run(
    arguments: &[OsString],
    run_context: &[u8],
    launch_policy: LaunchPolicy,
) -> Result<(), String> {
    let decision_started = Instant::now();
    if !eligible(arguments)? {
        return Ok(());
    }
    let directory = fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))?;
    let mut history_probes = HistoryProbeCache::default();
    let current = State::load(&directory, StateKind::Run)?;
    if let Some(state) = current.as_ref().filter(|state| {
        state.context_matches(run_context)
            && state.sibling_roots.is_empty()
            && state.artifact_is_unchanged().unwrap_or(false)
    }) {
        // A patch produces source Cargo never rendered diagnostics for, so
        // recorded warnings disqualify the transformation rather than being
        // replayed inexactly.
        let change = if state.diagnostics.is_none() {
            find_literal_change(&directory, &state.snapshot, &state.sources)?
        } else {
            None
        };
        if let Some(change) = change {
            if cargo_outputs_and_inputs_are_unchanged(state, &directory)? {
                if let Some(patched) =
                    patch_artifact(&directory, state, &change, PatchMode::RunSibling)?
                {
                    state.record_patched(
                        &directory,
                        StateKind::Run,
                        &patched,
                        &change,
                        launch_policy.coalesces_duplicate_events(),
                    )?;
                    crate::usage::record(
                        crate::usage::CommandKind::Run,
                        crate::usage::Outcome::BinaryPatch,
                        decision_started.elapsed(),
                    );
                    eprintln!(
                        "    Cinder patched {} from {} without recompiling",
                        change.relative.display(),
                        state.artifact.display()
                    );
                    return launch_new_fast_run(
                        &directory,
                        &patched,
                        &state.program_name,
                        arguments,
                        &state.runtime_environment,
                        launch_policy,
                    );
                }
            }
        } else if history_probes.source_digest(&directory, &state.sources)? == state.source_digest
            && cargo_outputs_and_inputs_are_unchanged(state, &directory)?
        {
            let can_launch_current = state.artifact != state.public_artifact
                && (!launch_policy.coalesces_duplicate_events()
                    || State::consume_fresh_duplicate(&directory)?);
            if can_launch_current {
                crate::usage::record(
                    crate::usage::CommandKind::Run,
                    crate::usage::Outcome::CurrentReuse,
                    decision_started.elapsed(),
                );
                state.diagnostics.replay_to_stderr();
                eprintln!("    Cinder reusing {}", state.artifact.display());
                return exec_artifact(
                    &state.artifact,
                    &state.program_name,
                    runtime_arguments(arguments),
                    &state.runtime_environment,
                )
                .map(|_| ());
            }
        }
    }

    let Some(historical) = State::matching_history(
        &directory,
        StateKind::Run,
        run_context,
        None,
        &mut history_probes,
    )?
    else {
        return Ok(());
    };
    let restored = restore_cached_run_artifact(&directory, &historical)?;
    historical.promote(
        &directory,
        StateKind::Run,
        &restored,
        launch_policy.coalesces_duplicate_events(),
    )?;
    crate::usage::record(
        crate::usage::CommandKind::Run,
        crate::usage::Outcome::RevisionRestore,
        decision_started.elapsed(),
    );
    historical.diagnostics.replay_to_stderr();
    eprintln!(
        "    Cinder restored a validated previous build of {} without recompiling",
        historical.program_name.to_string_lossy()
    );
    launch_new_fast_run(
        &directory,
        &restored,
        &historical.program_name,
        arguments,
        &historical.runtime_environment,
        launch_policy,
    )
}

/// Completes a supported binary build without invoking Cargo when the recorded
/// artifact and every non-source input are still valid.
///
/// Returning `Ok(false)` is a normal cache miss. Cargo remains responsible for
/// every unsupported command shape and for refreshing state after a miss.
pub fn try_fast_build(arguments: &[OsString], build_context: &[u8]) -> Result<bool, String> {
    let decision_started = Instant::now();
    if !build_eligible(arguments)? {
        return Ok(false);
    }
    let directory = canonical_current_directory()?;
    let stage_started = Instant::now();
    let current = State::load_lock_probe(&directory, StateKind::Build)?;
    patch::trace_run("probe current build state", stage_started);
    if env::var_os(TRACE_RUN).is_some() {
        eprintln!(
            "    Cinder trace: current build state={} context-match={} wanted={}",
            current.is_some(),
            current
                .as_ref()
                .is_some_and(|state| state.context_matches(build_context)),
            short_digest(build_context),
        );
    }
    let target_lock_path = match current.filter(|state| state.context_matches(build_context)) {
        Some(state) => cargo_target_lock_path(&state.public_artifact)?,
        None => {
            let Some(path) =
                State::historical_target_lock_path(&directory, StateKind::Build, build_context)?
            else {
                return Ok(false);
            };
            path
        }
    };
    let stage_started = Instant::now();
    let _target_lock = CargoTargetLock::acquire(&target_lock_path)?;
    patch::trace_run("acquire Cargo target lock", stage_started);
    try_fast_build_locked(
        &directory,
        build_context,
        &target_lock_path,
        decision_started,
    )
}

/// Completes an unchanged, previously validated single-unit check without
/// asking Cargo to walk the package graph again.
///
/// A changed source, input, compiler context, or exact Cargo output is a normal
/// miss. Check state is never restored from revision history because Cinder
/// does not fabricate Cargo fingerprints or dependency metadata.
pub fn try_fast_check(arguments: &[OsString], check_context: &[u8]) -> Result<bool, String> {
    let decision_started = Instant::now();
    if !check_eligible(arguments)? {
        return Ok(false);
    }
    let directory = canonical_current_directory()?;
    let Some(state) = State::load_lock_probe(&directory, StateKind::Check)? else {
        if env::var_os(TRACE_RUN).is_some() {
            eprintln!("    Cinder trace: no recorded check state");
        }
        return Ok(false);
    };
    if !state.context_matches(check_context) {
        if env::var_os(TRACE_RUN).is_some() {
            eprintln!("    Cinder trace: check context changed before target lock");
        }
        return Ok(false);
    }
    let target_lock_path = cargo_target_lock_path(&state.public_artifact)?;
    let _target_lock = CargoTargetLock::acquire(&target_lock_path)?;
    let stage_started = Instant::now();
    let Some(state) = State::load(&directory, StateKind::Check)? else {
        return Ok(false);
    };
    patch::trace_run("load check state", stage_started);
    if !state.context_matches(check_context) {
        return Ok(false);
    }
    let stage_started = Instant::now();
    if !state.artifact_is_unchanged()? || !state.sibling_roots_are_unchanged()? {
        return Ok(false);
    }
    patch::trace_run("validate selected check artifact", stage_started);
    let stage_started = Instant::now();
    let sources_match = state.sources_match_revision(&directory)?;
    patch::trace_run("validate selected check sources", stage_started);
    if !sources_match {
        // A recorded diagnostic replay describes the previous source revision;
        // a changed source could render differently, so replays stay on Cargo.
        if env::var_os(EXPERIMENTAL_DIRECT_CHECK).as_deref() != Some(OsStr::new("1"))
            || !state.compiler_recipe_supports_direct_check()
            || !state.diagnostics.is_none()
            || !state.sibling_roots.is_empty()
        {
            if env::var_os(TRACE_RUN).is_some() {
                eprintln!(
                    "    Cinder trace: locked check sources=false inputs=skipped direct=false"
                );
            }
            return Ok(false);
        }
        if !cargo_outputs_and_inputs_are_unchanged(&state, &directory)? {
            return Ok(false);
        }
        let recipe = state
            .compiler_recipe
            .as_ref()
            .ok_or_else(|| "validated check state has no compiler recipe".to_owned())?;
        let replay_directory = state_project_directory(&directory).join("compiler-replay");
        if !replay_compiler(recipe, &replay_directory)? {
            return Ok(false);
        }
        if !recipe.replayed_dependency_environment_matches(&state.cargo_outputs.dependency_file)? {
            if env::var_os(TRACE_RUN).is_some() {
                eprintln!(
                    "    Cinder trace: compiler replay introduced an unmatched environment dependency"
                );
            }
            return Ok(false);
        }
        state.record_replayed_check(&directory)?;
        crate::usage::record(
            crate::usage::CommandKind::Check,
            crate::usage::Outcome::DirectCompile,
            decision_started.elapsed(),
        );
        eprintln!(
            "    Cinder replayed Cargo's validated compiler recipe for {}",
            state.artifact.display()
        );
        return Ok(true);
    }
    if !cargo_outputs_and_inputs_are_unchanged(&state, &directory)? {
        return Ok(false);
    }
    if env::var_os(TRACE_RUN).is_some() {
        eprintln!("    Cinder trace: locked check sources=true inputs=true direct=false");
    }
    crate::usage::record(
        crate::usage::CommandKind::Check,
        crate::usage::Outcome::CurrentReuse,
        decision_started.elapsed(),
    );
    state.diagnostics.replay_to_stderr();
    if state.sibling_roots.is_empty() {
        eprintln!(
            "    Cinder reused the validated check of {} without invoking Cargo",
            state.artifact.display()
        );
    } else {
        eprintln!(
            "    Cinder reused the validated check of {} targets without invoking Cargo",
            state.sibling_roots.len() + 1
        );
    }
    Ok(true)
}

/// Reuses an unchanged test build for one explicitly selected test target.
/// Package-root `test --lib` also runs the exact validated Cargo-built harness;
/// broader test command shapes remain entirely Cargo-owned.
pub fn try_fast_test(arguments: &[OsString], test_context: &[u8]) -> Result<Option<u8>, String> {
    let decision_started = Instant::now();
    if !test_eligible(arguments)? {
        return Ok(None);
    }
    let executes_test = test_executes(arguments);
    let directory = canonical_current_directory()?;
    let Some(state) = State::load_lock_probe(&directory, StateKind::Test)? else {
        if env::var_os(TRACE_RUN).is_some() {
            eprintln!("    Cinder trace: no recorded test state");
        }
        return Ok(None);
    };
    if !state.context_matches(test_context) {
        if env::var_os(TRACE_RUN).is_some() {
            eprintln!("    Cinder trace: test context changed before target lock");
        }
        return Ok(None);
    }
    let target_lock_path = cargo_target_lock_path(&state.public_artifact)?;
    let _target_lock = CargoTargetLock::acquire(&target_lock_path)?;
    let Some(state) = State::load(&directory, StateKind::Test)? else {
        return Ok(None);
    };
    let context_matches = state.context_matches(test_context);
    let artifact_unchanged = state.artifact_is_unchanged()?;
    if !context_matches || !artifact_unchanged {
        if env::var_os(TRACE_RUN).is_some() {
            eprintln!(
                "    Cinder trace: locked test context={context_matches} artifact={artifact_unchanged}"
            );
        }
        return Ok(None);
    }
    let sources_unchanged = state.sources_match_revision(&directory)?;
    if !sources_unchanged {
        if env::var_os(TRACE_RUN).is_some() {
            eprintln!("    Cinder trace: locked test sources=false inputs=skipped");
        }
        return Ok(None);
    }
    if !cargo_outputs_and_inputs_are_unchanged(&state, &directory)? {
        return Ok(None);
    }
    if executes_test {
        let Some(runtime_directory) = state.validated_test_runtime_directory()? else {
            return Ok(None);
        };
        crate::usage::record(
            crate::usage::CommandKind::Test,
            crate::usage::Outcome::CurrentReuse,
            decision_started.elapsed(),
        );
        state.diagnostics.replay_to_stderr();
        eprintln!(
            "    Cinder running the validated test executable {} without invoking Cargo",
            state.artifact.display()
        );
        run_validated_test(
            &state.artifact,
            runtime_arguments(arguments),
            &state.runtime_environment,
            &runtime_directory,
        )
        .map(Some)
    } else {
        crate::usage::record(
            crate::usage::CommandKind::Test,
            crate::usage::Outcome::CurrentReuse,
            decision_started.elapsed(),
        );
        state.diagnostics.replay_to_stderr();
        eprintln!(
            "    Cinder reused the validated test build of {} without invoking Cargo",
            state.artifact.display()
        );
        Ok(Some(0))
    }
}

fn cargo_outputs_and_inputs_are_unchanged(state: &State, directory: &Path) -> Result<bool, String> {
    let (cargo_outputs_unchanged, inputs_unchanged) = thread::scope(|scope| {
        let cargo_outputs = scope.spawn(|| {
            let started = Instant::now();
            let unchanged = state.cargo_outputs_are_available();
            patch::trace_run("validate Cargo output graph", started);
            unchanged
        });
        let started = Instant::now();
        let inputs_unchanged = state.inputs_are_unchanged(directory);
        patch::trace_run("validate build inputs", started);
        (cargo_outputs.join().unwrap_or(false), inputs_unchanged)
    });
    let inputs_unchanged = inputs_unchanged?;
    if env::var_os(TRACE_RUN).is_some() {
        eprintln!(
            "    Cinder trace: validation Cargo-outputs={cargo_outputs_unchanged} inputs={inputs_unchanged}"
        );
    }
    Ok(cargo_outputs_unchanged && inputs_unchanged)
}

fn short_digest(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn try_fast_build_locked(
    directory: &Path,
    build_context: &[u8],
    target_lock_path: &Path,
    decision_started: Instant,
) -> Result<bool, String> {
    let mut history_probes = HistoryProbeCache::default();
    let stage_started = Instant::now();
    let current = State::load(directory, StateKind::Build)?;
    patch::trace_run("load current build state", stage_started);
    if env::var_os(TRACE_RUN).is_some() {
        if let Some(state) = current.as_ref() {
            eprintln!(
                "    Cinder trace: locked build context={} target={} artifact={} cargo-outputs=deferred",
                state.context_matches(build_context),
                cargo_target_lock_path(&state.public_artifact)
                    .is_ok_and(|path| path == target_lock_path),
                state.artifact_is_unchanged().unwrap_or(false)
            );
        }
    }
    if let Some(state) = current.filter(|state| {
        state.context_matches(build_context)
            && cargo_target_lock_path(&state.public_artifact)
                .is_ok_and(|path| path == target_lock_path)
            && state.artifact_is_unchanged().unwrap_or(false)
    }) {
        let stage_started = Instant::now();
        // A patch produces source Cargo never rendered diagnostics for, so
        // recorded warnings disqualify the transformation rather than being
        // replayed inexactly. Patching also stays single-root: a multi-target
        // command would need every sibling artifact patched coherently.
        let change = if state.diagnostics.is_none()
            && state.sibling_roots.is_empty()
            && artifact_is_executable(&state.public_artifact)
        {
            find_literal_change(directory, &state.snapshot, &state.sources)?
        } else {
            None
        };
        patch::trace_run("inspect build literal change", stage_started);
        if let Some(change) = change {
            if cargo_outputs_and_inputs_are_unchanged(&state, directory)? {
                if let Some(patched) =
                    patch_artifact(directory, &state, &change, PatchMode::BuildInPlace)?
                {
                    state.record_patched(directory, StateKind::Build, &patched, &change, false)?;
                    crate::usage::record(
                        crate::usage::CommandKind::Build,
                        crate::usage::Outcome::BinaryPatch,
                        decision_started.elapsed(),
                    );
                    eprintln!(
                        "    Cinder patched {} into {} without recompiling",
                        change.relative.display(),
                        patched.display()
                    );
                    return Ok(true);
                }
            }
        } else {
            let stage_started = Instant::now();
            let sources_unchanged =
                history_probes.source_digest(directory, &state.sources)? == state.source_digest;
            patch::trace_run("validate current build sources", stage_started);
            let inputs_unchanged = if sources_unchanged {
                cargo_outputs_and_inputs_are_unchanged(&state, directory)?
                    && state.sibling_roots_are_unchanged()?
            } else {
                false
            };
            if env::var_os(TRACE_RUN).is_some() {
                eprintln!(
                    "    Cinder trace: locked build sources={sources_unchanged} inputs={inputs_unchanged}"
                );
            }
            if sources_unchanged && inputs_unchanged {
                crate::usage::record(
                    crate::usage::CommandKind::Build,
                    crate::usage::Outcome::CurrentReuse,
                    decision_started.elapsed(),
                );
                state.diagnostics.replay_to_stderr();
                if state.sibling_roots.is_empty() {
                    eprintln!(
                        "    Cinder reused {} without invoking Cargo",
                        state.artifact.display()
                    );
                } else {
                    eprintln!(
                        "    Cinder reused the validated build of {} targets without invoking Cargo",
                        state.sibling_roots.len() + 1
                    );
                }
                return Ok(true);
            }
        }
    }

    let stage_started = Instant::now();
    let historical = State::matching_history(
        directory,
        StateKind::Build,
        build_context,
        Some(target_lock_path),
        &mut history_probes,
    )?;
    patch::trace_run("search build revision history", stage_started);
    let Some(historical) = historical else {
        return Ok(false);
    };
    let restored = restore_cached_build_artifact(directory, &historical)?;
    historical.promote(directory, StateKind::Build, &restored, false)?;
    crate::usage::record(
        crate::usage::CommandKind::Build,
        crate::usage::Outcome::RevisionRestore,
        decision_started.elapsed(),
    );
    historical.diagnostics.replay_to_stderr();
    eprintln!(
        "    Cinder restored a validated previous build of {} without invoking Cargo",
        restored.display()
    );
    Ok(true)
}

fn artifact_is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

struct CargoTargetLock(fs::File);

impl CargoTargetLock {
    fn acquire(path: &Path) -> Result<Self, String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| {
                format!(
                    "could not open Cargo target lock {}: {error}",
                    path.display()
                )
            })?;
        file.lock_exclusive()
            .map_err(|error| format!("could not acquire Cargo target lock: {error}"))?;
        Ok(Self(file))
    }
}

fn cargo_target_lock_path(public_artifact: &Path) -> Result<PathBuf, String> {
    cargo_profile_directory(public_artifact)
        .map(|profile| profile.join(".cargo-lock"))
        .ok_or_else(|| {
            format!(
                "public artifact has no profile directory: {}",
                public_artifact.display()
            )
        })
}

impl Drop for CargoTargetLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn launch_new_fast_run(
    directory: &Path,
    artifact: &Path,
    program_name: &OsStr,
    arguments: &[OsString],
    runtime_environment: &[(OsString, OsString)],
    launch_policy: LaunchPolicy,
) -> Result<(), String> {
    if launch_policy == LaunchPolicy::Immediate {
        return exec_artifact(
            artifact,
            program_name,
            runtime_arguments(arguments),
            runtime_environment,
        )
        .map(|_| ());
    }

    // Tauri can deliver a second event for one atomic editor save. Waiting here
    // lets that event cancel this runner; the replacement invocation consumes
    // the token and launches the already prepared artifact.
    thread::sleep(DUPLICATE_EVENT_SETTLE_TIME);
    if !State::fresh_duplicate_is_pending(directory)? {
        std::process::exit(0);
    }
    exec_artifact(
        artifact,
        program_name,
        runtime_arguments(arguments),
        runtime_environment,
    )
    .map(|_| ())
}

fn restore_cached_run_artifact(directory: &Path, historical: &State) -> Result<PathBuf, String> {
    let parent = historical.public_artifact.parent().ok_or_else(|| {
        format!(
            "public artifact has no parent: {}",
            historical.public_artifact.display()
        )
    })?;
    record_artifact_root(directory, parent)?;
    let program_name = historical.program_name.to_string_lossy();
    let artifact_digest = historical
        .artifact_digest
        .ok_or_else(|| "cached run artifact has no digest".to_owned())?;
    let restored = restored_run_artifact_path(directory, historical)?;
    if restored.is_file()
        && restored_run_artifact_is_trusted(directory, &restored, &artifact_digest)?
    {
        prune_run_artifacts(directory)?;
        return Ok(restored);
    }
    if restored.exists() {
        fs::remove_file(&restored)
            .map_err(|error| format!("could not replace restored run artifact: {error}"))?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let temporary = parent.join(format!(
        ".cinder-restore-{}-{nonce}-{program_name}",
        std::process::id(),
    ));
    let result = (|| {
        clone_file(&historical.artifact, &temporary)?;
        if !historical.cached_artifact_is_trusted()? {
            return Err("cached run artifact changed while restoring it".to_owned());
        }
        make_owner_writable(&temporary)?;
        remove_launch_xattrs(&temporary);
        make_cached_artifact_read_only(&temporary)?;
        fs::rename(&temporary, &restored)
            .map_err(|error| format!("could not publish cached run artifact: {error}"))?;
        let (restored_identity, restored_digest) = artifact_identity(&restored)?;
        if restored_digest != artifact_digest {
            return Err("cached run artifact changed while restoring it".to_owned());
        }
        record_restored_run_artifact(directory, &restored, &artifact_digest, &restored_identity)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    prune_run_artifacts(directory)?;
    Ok(restored)
}

fn restored_run_artifact_path(directory: &Path, state: &State) -> Result<PathBuf, String> {
    let parent = state.public_artifact.parent().ok_or_else(|| {
        format!(
            "public artifact has no parent: {}",
            state.public_artifact.display()
        )
    })?;
    let digest = digest_hex(
        &state
            .artifact_digest
            .ok_or_else(|| "cached run artifact has no digest".to_owned())?,
    );
    Ok(parent.join(format!(
        ".cinder-fast-{}-{digest}-{}",
        project_namespace(directory),
        state.program_name.to_string_lossy()
    )))
}

/// Cinder's per-project state directory, exposed for the tuned-toolchain
/// markers so they live and die with the rest of the project's state.
pub fn project_state_directory(directory: &Path) -> PathBuf {
    state_project_directory(directory)
}

fn restored_run_artifact_receipt_path(directory: &Path, artifact: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(artifact.as_os_str().as_bytes());
    state_project_directory(directory)
        .join("run-artifact-identities")
        .join(format!("{:x}", hasher.finalize()))
}

fn record_restored_run_artifact(
    directory: &Path,
    artifact: &Path,
    digest: &[u8; 32],
    expected_identity: &ArtifactFileIdentity,
) -> Result<(), String> {
    let receipt = restored_run_artifact_receipt_path(directory, artifact);
    let parent = receipt
        .parent()
        .ok_or_else(|| "restored artifact receipt has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create restored artifact receipts: {error}"))?;
    make_private_directory(parent)?;
    let identity = artifact_file_identity(artifact)?;
    if &identity != expected_identity {
        return Err("restored run artifact changed before its receipt was published".to_owned());
    }
    let temporary = receipt.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(
        &temporary,
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            digest_hex(digest),
            identity.size,
            identity.modified_ns,
            identity.device,
            identity.inode,
            identity.changed_seconds,
            identity.changed_nanoseconds,
        ),
    )
    .map_err(|error| format!("could not stage restored artifact receipt: {error}"))?;
    fs::rename(&temporary, &receipt)
        .map_err(|error| format!("could not publish restored artifact receipt: {error}"))
}

fn restored_run_artifact_is_trusted(
    directory: &Path,
    artifact: &Path,
    digest: &[u8; 32],
) -> Result<bool, String> {
    let receipt = restored_run_artifact_receipt_path(directory, artifact);
    let contents = match fs::read_to_string(receipt) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not read restored artifact receipt: {error}")),
    };
    let mut lines = contents.lines();
    let expected_digest = digest_hex(digest);
    if lines.next() != Some(expected_digest.as_str()) {
        return Ok(false);
    }
    let recorded = ArtifactFileIdentity {
        size: parse_state_number(lines.next(), "restored artifact size")?,
        modified_ns: parse_state_number(lines.next(), "restored artifact timestamp")?,
        device: parse_state_number(lines.next(), "restored artifact device")?,
        inode: parse_state_number(lines.next(), "restored artifact inode")?,
        changed_seconds: parse_state_number(lines.next(), "restored artifact change timestamp")?,
        changed_nanoseconds: parse_state_number(
            lines.next(),
            "restored artifact change timestamp nanoseconds",
        )?,
    };
    Ok(artifact_file_identity(artifact)? == recorded
        && fs::metadata(artifact).is_ok_and(|metadata| metadata.permissions().mode() & 0o222 == 0))
}

fn digest_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn restore_cached_build_artifact(directory: &Path, historical: &State) -> Result<PathBuf, String> {
    let parent = historical.public_artifact.parent().ok_or_else(|| {
        format!(
            "public artifact has no parent: {}",
            historical.public_artifact.display()
        )
    })?;
    record_artifact_root(directory, parent)?;
    let program_name = historical.program_name.to_string_lossy();
    let temporary = parent.join(format!(
        ".cinder-restore-{}-{program_name}",
        std::process::id()
    ));
    let result = (|| {
        clone_file(&historical.artifact, &temporary)?;
        if !historical.cached_artifact_is_trusted()? {
            return Err("cached build artifact changed while restoring it".to_owned());
        }
        make_owner_writable(&temporary)?;
        remove_launch_xattrs(&temporary);
        let oldest_source = historical
            .sources
            .iter()
            .map(|source| {
                fs::metadata(directory.join(source))
                    .and_then(|metadata| metadata.modified())
                    .map_err(|error| {
                        format!(
                            "could not inspect source timestamp {}: {error}",
                            source.display()
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .min()
            .ok_or_else(|| "cached build has no source timestamps".to_owned())?;
        let stale_time = oldest_source
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(UNIX_EPOCH);
        fs::File::open(&temporary)
            .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(stale_time)))
            .map_err(|error| format!("could not make cached artifact Cargo-stale: {error}"))?;
        fs::rename(&temporary, &historical.public_artifact)
            .map_err(|error| format!("could not publish cached build artifact: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(historical.public_artifact.clone())
}

pub fn run_artifact(mut arguments: Vec<OsString>) -> Result<u8, String> {
    if arguments.len() < 4 {
        return Err(
            "artifact runner requires context, receipts, recording mode, and executable paths"
                .to_owned(),
        );
    }
    let context_path = PathBuf::from(arguments.remove(0));
    let receipt_directory = PathBuf::from(arguments.remove(0));
    let synchronous_recording = match arguments.remove(0).to_str() {
        Some("sync") => true,
        Some("async") => false,
        _ => return Err("artifact runner received an invalid recording mode".to_owned()),
    };
    messages::wait_for_runner_receipts(&receipt_directory)?;
    let artifact = absolute_path(Path::new(&arguments.remove(0)))?;
    let program_name = artifact
        .file_name()
        .ok_or_else(|| format!("Cargo artifact has no file name: {}", artifact.display()))?
        .to_owned();
    let directory = fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))?;
    let run_context = fs::read(&context_path)
        .map_err(|error| format!("could not read Cinder run context: {error}"));
    let _ = fs::remove_file(&context_path);
    if let Err(error) = run_context.and_then(|run_context| {
        schedule_run_state(
            &directory,
            &artifact,
            &program_name,
            &run_context,
            Some(receipt_directory),
            synchronous_recording,
        )
    }) {
        eprintln!("cinder: could not prepare the next fast run: {error}");
    }
    eprintln!("     Running `{}`", artifact.display());
    exec_artifact(&artifact, &program_name, arguments.iter(), &[])
}

/// Cargo target runner used only for one explicitly selected standard library
/// test. The test always runs, even when state capture is unavailable. A
/// successful execution can establish the next exact fast-test baseline.
pub fn run_test_artifact(mut arguments: Vec<OsString>) -> Result<u8, String> {
    if arguments.len() < 5 {
        return Err(
            "test artifact runner requires context, receipts, project, recording mode, and executable paths".to_owned(),
        );
    }
    let context_path = PathBuf::from(arguments.remove(0));
    let receipt_directory = PathBuf::from(arguments.remove(0));
    let project_directory = PathBuf::from(arguments.remove(0));
    let synchronous_recording = match arguments.remove(0).to_str() {
        Some("sync") => true,
        Some("async") => false,
        _ => return Err("test artifact runner received an invalid recording mode".to_owned()),
    };
    let artifact = absolute_path(Path::new(&arguments.remove(0)))?;
    let program_name = artifact
        .file_name()
        .ok_or_else(|| {
            format!(
                "Cargo test artifact has no file name: {}",
                artifact.display()
            )
        })?
        .to_owned();
    let receipts_ready = messages::wait_for_runner_receipts(&receipt_directory);
    let project_directory = fs::canonicalize(&project_directory);
    let runtime_directory = canonical_current_directory();
    let test_context = fs::read(&context_path);

    let status = test_command(&artifact, arguments.iter(), None, None)
        .status()
        .map_err(|error| {
            format!(
                "could not execute Cargo test {}: {error}",
                artifact.display()
            )
        })?;
    if status.success() {
        let recording = receipts_ready
            .map_err(|error| format!("Cargo test receipts were unavailable: {error}"))
            .and_then(|()| matching_test_artifact_receipt(&receipt_directory, &artifact))
            .and_then(|receipt| {
                receipt.ok_or_else(|| "Cargo produced no exact test artifact receipt".to_owned())
            })
            .and_then(|receipt| {
                let project_directory = project_directory
                    .map_err(|error| format!("could not resolve test project: {error}"))?;
                let runtime_directory = runtime_directory.map_err(|error| {
                    format!("could not resolve test working directory: {error}")
                })?;
                let manifest_directory = receipt
                    .manifest_directory
                    .as_deref()
                    .ok_or_else(|| "Cargo test receipt has no package manifest".to_owned())?;
                let manifest_directory = fs::canonicalize(manifest_directory).map_err(|error| {
                    format!("could not resolve Cargo test package directory: {error}")
                })?;
                if runtime_directory != manifest_directory {
                    return Err(
                        "Cargo test working directory did not match the selected package"
                            .to_owned(),
                    );
                }
                if !manifest_has_standard_library_test_harness_at(&runtime_directory)? {
                    return Err("Cargo selected a nonstandard library test harness".to_owned());
                }
                let test_context = test_context
                    .map_err(|error| format!("could not read Cinder test context: {error}"))?;
                schedule_test_execution_state(
                    &project_directory,
                    &runtime_directory,
                    &artifact,
                    &program_name,
                    &test_context,
                    Some(receipt_directory.clone()),
                    synchronous_recording,
                )
            });
        if let Err(error) = recording {
            let _ = fs::remove_dir_all(&receipt_directory);
            eprintln!("cinder: could not prepare the next fast test execution: {error}");
        }
    }
    let _ = fs::remove_file(context_path);
    child_status(status)
}

fn schedule_test_execution_state(
    directory: &Path,
    runtime_directory: &Path,
    artifact: &Path,
    program_name: &OsStr,
    test_context: &[u8],
    receipt_directory: Option<PathBuf>,
    synchronous: bool,
) -> Result<(), String> {
    if synchronous {
        let receipt = receipt_directory
            .as_deref()
            .map(|directory| matching_test_artifact_receipt(directory, artifact))
            .transpose()?
            .flatten()
            .ok_or_else(|| "Cargo produced no exact test artifact receipt".to_owned())?;
        let diagnostics = receipt_directory
            .as_deref()
            .map(|receipts| {
                captured_receipt_diagnostics(receipts, StateKind::Test, artifact, directory)
            })
            .transpose()?;
        let result = State::record_fresh_test_execution(
            directory,
            artifact,
            program_name,
            test_context,
            &receipt,
            runtime_directory,
            diagnostics,
        );
        if let Some(directory) = receipt_directory {
            let _ = fs::remove_dir_all(directory);
        }
        return result;
    }
    let root = env::temp_dir().join("cinder").join("recordings");
    fs::create_dir_all(&root)
        .map_err(|error| format!("could not create state recording directory: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let context_path = root.join(format!("test-{}-{nonce}", std::process::id()));
    fs::write(&context_path, test_context)
        .map_err(|error| format!("could not stage test state context: {error}"))?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    let mut command = Command::new(cinder);
    command
        .arg("__record-test-execution")
        .arg(artifact)
        .arg(program_name)
        .arg(&context_path)
        .arg(runtime_directory)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(receipt_directory) = receipt_directory {
        command.arg(receipt_directory);
    }
    match command.spawn() {
        Ok(_) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&context_path);
            Err(format!("could not start the test state recorder: {error}"))
        }
    }
}

pub fn record_test_execution_state_command(arguments: &[OsString]) -> Result<u8, String> {
    if !(4..=5).contains(&arguments.len()) {
        return Err(
            "test execution recorder requires artifact, program, context, runtime directory, and optional receipts".to_owned(),
        );
    }
    let artifact = &arguments[0];
    let program_name = &arguments[1];
    let context_path = &arguments[2];
    let runtime_directory = &arguments[3];
    let receipt_directory = arguments.get(4).map(PathBuf::from);
    let context = fs::read(context_path)
        .map_err(|error| format!("could not read test state context: {error}"))?;
    let directory = canonical_current_directory()?;
    let receipt = receipt_directory
        .as_deref()
        .map(|directory| matching_test_artifact_receipt(directory, Path::new(artifact)))
        .transpose()?
        .flatten()
        .ok_or_else(|| "Cargo produced no exact test artifact receipt".to_owned())?;
    let diagnostics = receipt_directory
        .as_deref()
        .map(|receipts| {
            captured_receipt_diagnostics(receipts, StateKind::Test, Path::new(artifact), &directory)
        })
        .transpose()?;
    let result = State::record_fresh_test_execution(
        &directory,
        Path::new(artifact),
        program_name,
        &context,
        &receipt,
        Path::new(runtime_directory),
        diagnostics,
    );
    let _ = fs::remove_file(context_path);
    if let Some(directory) = receipt_directory {
        let _ = fs::remove_dir_all(directory);
    }
    result.map(|()| 0)
}

fn schedule_run_state(
    directory: &Path,
    artifact: &Path,
    program_name: &OsStr,
    run_context: &[u8],
    receipt_directory: Option<PathBuf>,
    synchronous: bool,
) -> Result<(), String> {
    if synchronous {
        let receipt = receipt_directory
            .as_deref()
            .map(|directory| matching_artifact_receipt(directory, artifact))
            .transpose()?
            .flatten();
        let diagnostics = receipt_directory
            .as_deref()
            .map(|receipts| {
                captured_receipt_diagnostics(receipts, StateKind::Run, artifact, directory)
            })
            .transpose()?;
        let result = State::record_fresh(
            directory,
            StateKind::Run,
            artifact,
            program_name,
            run_context,
            receipt.as_ref(),
            diagnostics,
        );
        if let Some(directory) = receipt_directory {
            let _ = fs::remove_dir_all(directory);
        }
        return result;
    }
    let root = env::temp_dir().join("cinder").join("recordings");
    fs::create_dir_all(&root)
        .map_err(|error| format!("could not create state recording directory: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let context_path = root.join(format!("run-{}-{nonce}", std::process::id()));
    fs::write(&context_path, run_context)
        .map_err(|error| format!("could not stage run state context: {error}"))?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    let mut command = Command::new(cinder);
    command
        .arg("__record-run")
        .arg(artifact)
        .arg(program_name)
        .arg(&context_path)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(receipt_directory) = receipt_directory {
        command.arg(receipt_directory);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start the run state recorder: {error}"))
}

pub fn record_run_state_command(arguments: &[OsString]) -> Result<u8, String> {
    if !(3..=4).contains(&arguments.len()) {
        return Err(
            "run state recorder requires artifact, program, context, and optional receipt paths"
                .to_owned(),
        );
    }
    let artifact = &arguments[0];
    let program_name = &arguments[1];
    let context_path = &arguments[2];
    let receipt_directory = arguments.get(3).map(PathBuf::from);
    let context = fs::read(context_path)
        .map_err(|error| format!("could not read run state context: {error}"))?;
    let directory = canonical_current_directory()?;
    let receipt = receipt_directory
        .as_deref()
        .map(|directory| matching_artifact_receipt(directory, Path::new(artifact)))
        .transpose()?
        .flatten();
    let diagnostics = receipt_directory
        .as_deref()
        .map(|receipts| {
            captured_receipt_diagnostics(receipts, StateKind::Run, Path::new(artifact), &directory)
        })
        .transpose()?;
    let result = State::record_fresh(
        &directory,
        StateKind::Run,
        Path::new(artifact),
        program_name,
        &context,
        receipt.as_ref(),
        diagnostics,
    );
    let _ = fs::remove_file(context_path);
    if let Some(directory) = receipt_directory {
        let _ = fs::remove_dir_all(directory);
    }
    result.map(|()| 0)
}

fn matching_artifact_receipt(
    directory: &Path,
    artifact: &Path,
) -> Result<Option<ArtifactReceipt>, String> {
    let artifact = absolute_path(artifact)?;
    let mut matching = Vec::new();
    for receipt in read_artifact_receipts(directory)? {
        if public_artifact(&receipt).ok().as_deref() == Some(artifact.as_path()) {
            matching.push(receipt);
        }
    }
    match matching.len() {
        0 => Ok(None),
        1 => Ok(matching.pop()),
        _ => Err(format!(
            "compiler artifact receipt is ambiguous for {}",
            artifact.display()
        )),
    }
}

fn matching_test_artifact_receipt(
    directory: &Path,
    artifact: &Path,
) -> Result<Option<ArtifactReceipt>, String> {
    let artifact = absolute_path(artifact)?;
    let mut matching = Vec::new();
    for receipt in read_artifact_receipts(directory)? {
        if absolute_path(&receipt.artifact).ok().as_deref() == Some(artifact.as_path()) {
            matching.push(receipt);
        }
    }
    match matching.len() {
        0 => Ok(None),
        1 => Ok(matching.pop()),
        _ => Err(format!(
            "compiler artifact receipt is ambiguous for test {}",
            artifact.display()
        )),
    }
}

/// Reads the staged Cargo invocation and runs the hidden diagnostic replay
/// passes for a freshly recorded command. Before any Cargo child is spawned,
/// every source consumed by the staged receipts must predate the staged
/// invocation file, which was written before the recorded command's own Cargo
/// child: a newer source means the user kept editing after the command, and a
/// "no-change" pass would start a real, unrequested background compile.
fn captured_receipt_diagnostics(
    receipts: &Path,
    kind: StateKind,
    artifact: &Path,
    directory: &Path,
) -> Result<DiagnosticsReplay, String> {
    let (cargo, original_arguments) = read_cargo_invocation(receipts)?;
    let (_, staged_ns) = artifact_metadata(&receipts.join("cargo-invocation"))?;
    for receipt in read_artifact_receipts(receipts)? {
        // The receipt's artifact path may be relative; the target-directory
        // filter that excludes build-script-generated files needs it absolute.
        let receipt_artifact = absolute_path(&receipt.artifact)?;
        let dependency_file = absolute_path(&receipt.dependency_file)?;
        for source in build_source_paths(directory, &receipt_artifact, &dependency_file)? {
            let (_, modified_ns) = artifact_metadata(&directory.join(&source))?;
            if modified_ns > staged_ns {
                return Err(format!(
                    "{} changed after the Cargo command started; diagnostic replay abandoned",
                    source.display()
                ));
            }
        }
    }
    capture_replay_diagnostics(&cargo, &original_arguments, kind, artifact)
}

/// A multi-root recording must cover every selected unit Cargo built. A unit
/// dropped from the receipt set for any reason other than being a build
/// script (a procedural macro, a multi-crate-type target, an unknown layout)
/// would let a later reuse hit report success for a unit it never validated.
/// An explicit single-target selector keeps today's behavior: its reuse
/// validates the selected root's complete reachable graph.
fn receipt_set_is_complete(receipt_directory: &Path, selects_executable: bool) -> bool {
    match messages::read_receipt_gaps(receipt_directory) {
        Some(0) => true,
        Some(gaps) => {
            if !selects_executable && env::var_os(TRACE_RUN).is_some() {
                eprintln!("    Cinder trace: {gaps} selected units have no receipt");
            }
            selects_executable
        }
        None => {
            if env::var_os(TRACE_RUN).is_some() {
                eprintln!("    Cinder trace: the receipt gap marker is missing or malformed");
            }
            false
        }
    }
}

pub fn record_completed_build(
    receipt_directory: &Path,
    build_context: &[u8],
    selects_executable: bool,
) -> Result<bool, String> {
    let result = (|| {
        if !receipt_set_is_complete(receipt_directory, selects_executable) {
            return Ok(false);
        }
        let receipts = read_artifact_receipts(receipt_directory)?;
        let mut artifacts = BTreeMap::<PathBuf, ArtifactReceipt>::new();
        for receipt in receipts {
            if selects_executable && receipt.crate_type != "bin" {
                continue;
            }
            let artifact = public_artifact(&receipt)?;
            artifacts.entry(artifact).or_insert(receipt);
        }
        // A selector shape still requires exactly one executable; the default
        // target set may record every selected public artifact. Proc-macro
        // units are accepted as check roots only: build-mode publication and
        // promotion reason about public executables and libraries, so a
        // proc-macro root keeps build recording refused exactly as the
        // receipt-gap rule did before these units converted.
        if artifacts.is_empty()
            || artifacts.len() > MAX_STATE_ROOTS
            || (selects_executable && artifacts.len() != 1)
            || artifacts
                .values()
                .any(|receipt| receipt.crate_type == "proc-macro")
        {
            if env::var_os(TRACE_RUN).is_some() {
                eprintln!(
                    "    Cinder trace: unsupported executable receipt count {}",
                    artifacts.len()
                );
            }
            return Ok(false);
        }
        let roots: Vec<(PathBuf, ArtifactReceipt)> = artifacts.into_iter().collect();
        let directory = canonical_current_directory()?;
        let diagnostics = captured_receipt_diagnostics(
            receipt_directory,
            StateKind::Build,
            &roots[0].0,
            &directory,
        )?;
        if let [(artifact, receipt)] = roots.as_slice() {
            let program_name = artifact.file_name().ok_or_else(|| {
                format!("Cargo artifact has no file name: {}", artifact.display())
            })?;
            State::record_fresh(
                &directory,
                StateKind::Build,
                artifact,
                program_name,
                build_context,
                Some(receipt),
                Some(diagnostics),
            )?;
        } else {
            let (_, command_started_ns) =
                artifact_metadata(&receipt_directory.join("cargo-invocation"))?;
            State::record_fresh_multi(
                &directory,
                StateKind::Build,
                &roots,
                build_context,
                diagnostics,
                command_started_ns,
            )?;
        }
        Ok(true)
    })();
    let _ = fs::remove_dir_all(receipt_directory);
    result
}

pub fn record_completed_check(
    receipt_directory: &Path,
    check_context: &[u8],
    selects_executable: bool,
) -> Result<bool, String> {
    let result = (|| {
        if !receipt_set_is_complete(receipt_directory, selects_executable) {
            return Ok(false);
        }
        let receipts = read_artifact_receipts(receipt_directory)?;
        let mut artifacts = BTreeMap::<PathBuf, ArtifactReceipt>::new();
        for receipt in receipts {
            if selects_executable && receipt.crate_type != "bin" {
                continue;
            }
            artifacts.entry(receipt.artifact.clone()).or_insert(receipt);
        }
        if artifacts.is_empty() || artifacts.len() > MAX_STATE_ROOTS {
            if env::var_os(TRACE_RUN).is_some() {
                eprintln!(
                    "    Cinder trace: expected 1..={MAX_STATE_ROOTS} check receipts, found {}",
                    artifacts.len()
                );
            }
            return Ok(false);
        }
        // BTreeMap iteration keeps the roots in lexicographic artifact order,
        // so the first root is the deterministic primary.
        let roots: Vec<(PathBuf, ArtifactReceipt)> = artifacts.into_iter().collect();
        let directory = canonical_current_directory()?;
        let diagnostics = captured_receipt_diagnostics(
            receipt_directory,
            StateKind::Check,
            &roots[0].0,
            &directory,
        )?;
        if let [(artifact, receipt)] = roots.as_slice() {
            let program_name = artifact.file_name().ok_or_else(|| {
                format!(
                    "Cargo check artifact has no file name: {}",
                    artifact.display()
                )
            })?;
            State::record_fresh(
                &directory,
                StateKind::Check,
                artifact,
                program_name,
                check_context,
                Some(receipt),
                Some(diagnostics),
            )?;
        } else {
            // The staged invocation was written before the Cargo child
            // spawned; its timestamp bounds mid-command source edits.
            let (_, command_started_ns) =
                artifact_metadata(&receipt_directory.join("cargo-invocation"))?;
            State::record_fresh_multi(
                &directory,
                StateKind::Check,
                &roots,
                check_context,
                diagnostics,
                command_started_ns,
            )?;
        }
        Ok(true)
    })();
    let _ = fs::remove_dir_all(receipt_directory);
    result
}

pub fn record_completed_test(
    receipt_directory: &Path,
    test_context: &[u8],
) -> Result<bool, String> {
    let result = (|| {
        let receipts = read_artifact_receipts(receipt_directory)?;
        let mut artifacts = BTreeMap::<PathBuf, ArtifactReceipt>::new();
        for receipt in receipts {
            if receipt.crate_type != "bin" {
                continue;
            }
            artifacts.entry(receipt.artifact.clone()).or_insert(receipt);
        }
        if artifacts.len() != 1 {
            if env::var_os(TRACE_RUN).is_some() {
                eprintln!(
                    "    Cinder trace: expected one test receipt, found {}",
                    artifacts.len()
                );
            }
            return Ok(false);
        }
        let (artifact, receipt) = artifacts
            .into_iter()
            .next()
            .ok_or_else(|| "primary test artifact disappeared from state".to_owned())?;
        let directory = canonical_current_directory()?;
        let program_name = artifact.file_name().ok_or_else(|| {
            format!(
                "Cargo test artifact has no file name: {}",
                artifact.display()
            )
        })?;
        let diagnostics = captured_receipt_diagnostics(
            receipt_directory,
            StateKind::Test,
            &artifact,
            &directory,
        )?;
        State::record_fresh(
            &directory,
            StateKind::Test,
            &artifact,
            program_name,
            test_context,
            Some(&receipt),
            Some(diagnostics),
        )?;
        Ok(true)
    })();
    let _ = fs::remove_dir_all(receipt_directory);
    result
}

pub fn schedule_completed_build(
    receipt_directory: &Path,
    build_context: &[u8],
    selects_executable: bool,
) -> Result<(), String> {
    if env::var_os(SYNCHRONOUS_STATE_RECORDING).is_some() {
        let _ = record_completed_build(receipt_directory, build_context, selects_executable)?;
        return Ok(());
    }
    let context_path = receipt_directory.join("build-context");
    fs::write(&context_path, build_context)
        .map_err(|error| format!("could not stage build state context: {error}"))?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    Command::new(cinder)
        .arg("__record-build")
        .arg(receipt_directory)
        .arg(&context_path)
        .arg(if selects_executable { "bin" } else { "single" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start the build state recorder: {error}"))
}

pub fn schedule_completed_check(
    receipt_directory: &Path,
    check_context: &[u8],
    selects_executable: bool,
) -> Result<(), String> {
    if env::var_os(SYNCHRONOUS_STATE_RECORDING).is_some() {
        let _ = record_completed_check(receipt_directory, check_context, selects_executable)?;
        return Ok(());
    }
    let context_path = receipt_directory.join("check-context");
    fs::write(&context_path, check_context)
        .map_err(|error| format!("could not stage check state context: {error}"))?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    Command::new(cinder)
        .arg("__record-check")
        .arg(receipt_directory)
        .arg(&context_path)
        .arg(if selects_executable { "bin" } else { "single" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start the check state recorder: {error}"))
}

pub fn schedule_completed_test(
    receipt_directory: &Path,
    test_context: &[u8],
) -> Result<(), String> {
    if env::var_os(SYNCHRONOUS_STATE_RECORDING).is_some() {
        let _ = record_completed_test(receipt_directory, test_context)?;
        return Ok(());
    }
    let context_path = receipt_directory.join("test-context");
    fs::write(&context_path, test_context)
        .map_err(|error| format!("could not stage test state context: {error}"))?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    Command::new(cinder)
        .arg("__record-test")
        .arg(receipt_directory)
        .arg(&context_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start the test state recorder: {error}"))
}

pub fn record_build_state_command(arguments: Vec<OsString>) -> Result<u8, String> {
    let [receipt_directory, context_path, selection] = <[OsString; 3]>::try_from(arguments)
        .map_err(|_| "build state recorder requires receipt, context, and selection".to_owned())?;
    let selects_executable = match selection.to_str() {
        Some("bin") => true,
        Some("single") => false,
        _ => return Err("build state recorder has an invalid selection".to_owned()),
    };
    let context = fs::read(&context_path)
        .map_err(|error| format!("could not read build state context: {error}"))?;
    let _ = record_completed_build(Path::new(&receipt_directory), &context, selects_executable)?;
    Ok(0)
}

pub fn record_check_state_command(arguments: Vec<OsString>) -> Result<u8, String> {
    let [receipt_directory, context_path, selection] = <[OsString; 3]>::try_from(arguments)
        .map_err(|_| "check state recorder requires receipt, context, and selection".to_owned())?;
    let selects_executable = match selection.to_str() {
        Some("bin") => true,
        Some("single") => false,
        _ => return Err("check state recorder has an invalid selection".to_owned()),
    };
    let context = fs::read(&context_path)
        .map_err(|error| format!("could not read check state context: {error}"))?;
    let _ = record_completed_check(Path::new(&receipt_directory), &context, selects_executable)?;
    Ok(0)
}

pub fn record_test_state_command(arguments: Vec<OsString>) -> Result<u8, String> {
    let [receipt_directory, context_path] = <[OsString; 2]>::try_from(arguments)
        .map_err(|_| "test state recorder requires receipt and context".to_owned())?;
    let context = fs::read(&context_path)
        .map_err(|error| format!("could not read test state context: {error}"))?;
    let _ = record_completed_test(Path::new(&receipt_directory), &context)?;
    Ok(0)
}

/// The hidden `RUSTC_WRAPPER` mode used only during environment-witness
/// generation: dump the invocation, then exec the real compiler.
pub fn env_probe_wrapper_main(arguments: &[OsString]) -> Option<u8> {
    envprobe::wrapper_main(arguments)
}

/// Loads or lazily generates the per-toolchain compiler-environment witness.
/// Only the experimental recipe-capture path calls this; everything about it
/// fails closed to a missing witness.
pub fn compiler_environment_witness(cargo: &Path) -> Option<EnvironmentWitness> {
    envprobe::witness_for(cargo)
}

pub fn clear_environment_witnesses() -> Result<(), String> {
    envprobe::clear_witnesses()
}

fn read_artifact_receipts(directory: &Path) -> Result<Vec<ArtifactReceipt>, String> {
    let mut receipts = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("could not inspect artifact receipts: {error}"))?
    {
        let path = entry
            .map_err(|error| format!("could not inspect artifact receipt: {error}"))?
            .path();
        if path.extension() == Some("receipt".as_ref()) {
            let mut receipt = read_artifact_receipt(&path)?;
            let recipe = path.with_extension("recipe");
            if recipe.is_file() {
                receipt.compiler_recipe = read_compiler_recipe(&recipe).ok();
            }
            receipts.push(receipt);
        }
    }
    Ok(receipts)
}

fn public_artifact(receipt: &ArtifactReceipt) -> Result<PathBuf, String> {
    let artifact = absolute_path(&receipt.artifact)?;
    let parent = artifact
        .parent()
        .ok_or_else(|| format!("artifact has no parent: {}", artifact.display()))?;
    let file_name = &receipt.public_file_name;
    let mut candidates = vec![parent.join(file_name)];
    if let Some(profile_directory) = parent.parent() {
        candidates.push(profile_directory.join(file_name));
    }
    candidates.sort();
    candidates.dedup();
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| candidate.is_file())
        .collect();
    match candidates.as_slice() {
        [candidate] => absolute_path(candidate),
        [] => Err(format!(
            "could not find Cargo's public artifact for {}",
            artifact.display()
        )),
        _ => {
            let metadata = artifact_metadata(&artifact)?;
            let matching: Vec<_> = candidates
                .into_iter()
                .filter(|candidate| artifact_metadata(candidate).ok() == Some(metadata))
                .collect();
            match matching.as_slice() {
                [candidate] => absolute_path(candidate),
                _ => Err(format!(
                    "Cargo's public artifact is ambiguous for {}",
                    artifact.display()
                )),
            }
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()
            .map_err(|error| format!("could not inspect the current directory: {error}"))?
            .join(path)
    };
    fs::canonicalize(&path).map_err(|error| {
        format!(
            "could not resolve Cargo artifact {}: {error}",
            path.display()
        )
    })
}

fn runtime_arguments(arguments: &[OsString]) -> impl Iterator<Item = &OsString> {
    arguments
        .iter()
        .skip_while(|argument| argument.as_os_str() != "--")
        .skip(1)
}

fn run_validated_test<'a>(
    artifact: &Path,
    arguments: impl IntoIterator<Item = &'a OsString>,
    runtime_environment: &[(OsString, OsString)],
    runtime_directory: &Path,
) -> Result<u8, String> {
    let status = test_command(
        artifact,
        arguments,
        Some(runtime_environment),
        Some(runtime_directory),
    )
    .status()
    .map_err(|error| {
        format!(
            "could not execute validated test {}: {error}",
            artifact.display()
        )
    })?;
    if status.success() {
        return Ok(0);
    }
    eprintln!("error: test failed, to rerun pass `--lib`");
    Ok(101)
}

fn test_command<'a>(
    artifact: &Path,
    arguments: impl IntoIterator<Item = &'a OsString>,
    runtime_environment: Option<&[(OsString, OsString)]>,
    runtime_directory: Option<&Path>,
) -> Command {
    let mut command = Command::new(artifact);
    command.args(arguments);
    if let Some(runtime_directory) = runtime_directory {
        command.current_dir(runtime_directory);
    }
    if let Some(runtime_environment) = runtime_environment {
        for key in RUNTIME_LINKER_ENVIRONMENT_KEYS {
            command.env_remove(key);
        }
        command.envs(runtime_environment.iter().map(|(key, value)| (key, value)));
    }
    crate::command::restore_runtime_environment(&mut command);
    command
}

fn child_status(status: std::process::ExitStatus) -> Result<u8, String> {
    if let Some(code) = status.code() {
        return Ok(code.clamp(0, u8::MAX as i32) as u8);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            // SAFETY: the child terminated from this signal. Restoring the
            // default disposition and raising it gives Cargo the same runner
            // termination class it would have observed from the test itself.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
        }
    }
    Err("Cargo test process ended without an exit code".to_owned())
}

fn exec_artifact<'a>(
    artifact: &Path,
    program_name: &OsStr,
    arguments: impl IntoIterator<Item = &'a OsString>,
    runtime_environment: &[(OsString, OsString)],
) -> Result<u8, String> {
    let mut command = Command::new(artifact);
    command.args(arguments);
    command.arg0(program_name);
    command.envs(runtime_environment.iter().map(|(key, value)| (key, value)));
    crate::command::restore_runtime_environment(&mut command);
    let error = command.exec();
    Err(format!(
        "could not execute artifact {}: {error}",
        artifact.display()
    ))
}

#[cfg(test)]
mod tests;
