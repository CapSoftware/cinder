//! Cargo-native artifact capture through the stable JSON message stream.
//!
//! Default Cinder commands use this path instead of installing a compiler
//! wrapper. Cargo continues to render diagnostics and status messages to
//! stderr; Cinder consumes only Cargo's structured stdout records and forwards
//! every non-Cargo byte unchanged.

use super::{
    ArtifactReceipt, BTreeMap, BuildScriptOutput, CompilerObserver, CompilerRecipe, OsStr,
    OsStrExt, OsString, Path, PathBuf, StateKind, env, fs, inputs::primary_dependency_file,
    make_private_directory, state_project_directory, write_artifact_receipt, write_compiler_recipe,
};
use std::{
    io::{self, BufRead, BufReader, Write},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const CAPTURE_ENABLED: &str = "cargo-message-capture";
const CAPTURE_READY: &str = "cargo-messages-ready";
const RECEIPT_GAPS: &str = "receipt-gaps";
const PACKAGE_CACHE_MAGIC: &[u8] = b"CINDER-PACKAGE-ID-1\n";
const MAX_PACKAGE_CACHE_BYTES: u64 = 1_048_576;
const MAX_PACKAGE_MANIFEST_BYTES: u64 = 4 * 1_048_576;
const MAX_SELECTED_ARTIFACTS: usize = 256;
const MAX_BUILD_SCRIPT_MESSAGES: usize = 4_096;
const MAX_PACKAGE_MANIFESTS: usize = 16_384;
const RUNNER_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolves Cargo's exact package ID for a single selected package.
///
/// Resolution is a capability probe. Any unsupported Cargo implementation,
/// virtual-workspace ambiguity, or complex multi-package selection simply
/// disables capture; the user's original command is still delegated intact.
pub struct PackageSelection {
    package_id: Option<String>,
    manifest_path: Option<PathBuf>,
    cache_path: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
}

/// How the invocation-directory manifest selects packages for capture.
enum ManifestSelection {
    /// One exact package directory; artifacts match its canonical manifest.
    Package(PathBuf),
    /// A workspace root `check`; every member unit under the root is a
    /// selected root.
    Workspace(PathBuf),
    /// A manifest shape whose selected package set Cinder cannot prove, such
    /// as explicit `default-members` outside a workspace-mode `check`.
    /// Capture stays disabled so reuse can never under-validate the command.
    Blocked,
    /// Not a package or workspace manifest Cinder recognizes; the exact
    /// package-ID probe may still resolve an explicit selection.
    NoSelection,
}

pub fn selected_package(
    cargo: &Path,
    arguments: &[OsString],
    context: &[u8],
) -> Option<PackageSelection> {
    if package_selector(arguments)?.is_none() {
        match selected_manifest_mode(arguments) {
            ManifestSelection::Package(manifest_path) => {
                if env::var_os(super::TRACE_RUN).is_some() {
                    eprintln!("    Cinder trace: Cargo package ID=message-manifest");
                }
                return Some(PackageSelection {
                    package_id: None,
                    manifest_path: Some(manifest_path),
                    cache_path: None,
                    workspace_root: None,
                });
            }
            ManifestSelection::Workspace(workspace_root) => {
                if env::var_os(super::TRACE_RUN).is_some() {
                    eprintln!("    Cinder trace: Cargo package ID=workspace-members");
                }
                return Some(PackageSelection {
                    package_id: None,
                    manifest_path: None,
                    cache_path: None,
                    workspace_root: Some(workspace_root),
                });
            }
            ManifestSelection::Blocked => return None,
            ManifestSelection::NoSelection => {}
        }
    }
    let (cache_path, workspace) = package_cache_path(arguments)?;
    if let Some(package_id) = read_cached_package(&cache_path, context) {
        if env::var_os(super::TRACE_RUN).is_some() {
            eprintln!("    Cinder trace: Cargo package ID=cache");
        }
        return Some(PackageSelection {
            package_id: Some(package_id),
            manifest_path: None,
            cache_path: Some(cache_path),
            workspace_root: None,
        });
    }
    let package_id = resolve_package_id(cargo, arguments)?;
    if env::var_os(super::TRACE_RUN).is_some() {
        eprintln!("    Cinder trace: Cargo package ID=resolved");
    }
    let _ = write_cached_package(&cache_path, &workspace, context, &package_id);
    Some(PackageSelection {
        package_id: Some(package_id),
        manifest_path: None,
        cache_path: Some(cache_path),
        workspace_root: None,
    })
}

fn selected_manifest_mode(arguments: &[OsString]) -> ManifestSelection {
    selected_manifest_path(arguments).map_or(ManifestSelection::NoSelection, |manifest| {
        classify_selected_manifest(arguments, &manifest)
    })
}

fn selected_manifest_path(arguments: &[OsString]) -> Option<PathBuf> {
    let command_index = super::capture::cargo_subcommand_index(arguments)?;
    let command_directory = super::cargo::cargo_change_directory(arguments)
        .ok()?
        .map_or_else(|| fs::canonicalize(env::current_dir().ok()?).ok(), Some)?;
    let mut selected = None;
    let mut index = command_index + 1;
    while index < arguments.len() {
        let argument = arguments[index].as_os_str();
        if argument == "--" {
            break;
        }
        if argument == "--manifest-path" {
            let value = arguments.get(index + 1)?;
            if selected.replace(PathBuf::from(value)).is_some() {
                return None;
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--manifest-path="))
        {
            if value.is_empty() || selected.replace(PathBuf::from(value)).is_some() {
                return None;
            }
        }
        index += 1;
    }
    let manifest = selected.unwrap_or_else(|| PathBuf::from("Cargo.toml"));
    let manifest = if manifest.is_absolute() {
        manifest
    } else {
        command_directory.join(manifest)
    };
    let metadata = fs::metadata(&manifest).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_PACKAGE_MANIFEST_BYTES {
        return None;
    }
    Some(manifest)
}

/// Decides how the invocation manifest selects packages.
///
/// A plain package manifest keeps the existing exact-package mode. A manifest
/// with a `[workspace]` table can build members beyond one package's
/// dependency closure, so a workspace-root `check` captures every member unit
/// instead, and every other command shape with explicit `default-members`
/// stays entirely Cargo-owned: single-package capture there would record only
/// part of what Cargo built and a later reuse could return stale results for
/// the other members.
fn classify_selected_manifest(arguments: &[OsString], manifest: &Path) -> ManifestSelection {
    let Ok(contents) = fs::read_to_string(manifest) else {
        return ManifestSelection::NoSelection;
    };
    let Ok(manifest_value) = toml::from_str::<toml::Table>(&contents) else {
        return ManifestSelection::NoSelection;
    };
    let has_package = manifest_value
        .get("package")
        .is_some_and(toml::Value::is_table);
    let workspace = match manifest_value.get("workspace") {
        None => {
            return if has_package {
                fs::canonicalize(manifest)
                    .ok()
                    .map_or(ManifestSelection::NoSelection, ManifestSelection::Package)
            } else {
                ManifestSelection::NoSelection
            };
        }
        Some(toml::Value::Table(workspace)) => workspace,
        Some(_) => return ManifestSelection::Blocked,
    };
    let has_default_members = workspace.contains_key("default-members");
    if has_package && !has_default_members {
        // Without explicit default-members, a root-package workspace command
        // selects only the root package; exact-package capture stays sound.
        return fs::canonicalize(manifest)
            .ok()
            .map_or(ManifestSelection::NoSelection, ManifestSelection::Package);
    }
    if matches!(super::cargo_subcommand(arguments), Some("check" | "c"))
        && super::cargo::primary_target_selector_count(arguments) == 0
    {
        // A member declared outside the root directory would produce units the
        // canonical-root artifact matcher cannot select, so its edits could
        // never invalidate the recorded state. Only provably in-root member
        // declarations may enter workspace capture.
        if !workspace_member_declarations_stay_in_root(workspace) {
            return ManifestSelection::Blocked;
        }
        return fs::canonicalize(manifest)
            .ok()
            .and_then(|manifest| manifest.parent().map(Path::to_owned))
            .map_or(ManifestSelection::NoSelection, ManifestSelection::Workspace);
    }
    if has_package && has_default_members {
        return ManifestSelection::Blocked;
    }
    ManifestSelection::NoSelection
}

/// Accepts only `members`/`default-members` entries that are relative paths or
/// globs without any parent-directory component. Anything else — an absolute
/// path, a `..` component, or a malformed array — could name a member outside
/// the workspace root.
fn workspace_member_declarations_stay_in_root(workspace: &toml::Table) -> bool {
    for key in ["members", "default-members"] {
        let Some(value) = workspace.get(key) else {
            continue;
        };
        let Some(entries) = value.as_array() else {
            return false;
        };
        for entry in entries {
            let Some(entry) = entry.as_str() else {
                return false;
            };
            if entry.is_empty()
                || Path::new(entry).is_absolute()
                || Path::new(entry)
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return false;
            }
        }
    }
    true
}

fn resolve_package_id(cargo: &Path, arguments: &[OsString]) -> Option<String> {
    let selector = package_selector(arguments)?;
    let mut command = package_probe(cargo, arguments, selector, "pkgid");
    let output = command.output().ok()?;
    if !output.status.success()
        && String::from_utf8_lossy(&output.stderr).contains("Cargo.lock must exist")
        && !arguments.iter().any(|argument| {
            matches!(
                argument.to_str(),
                Some("--locked" | "--frozen" | "--offline")
            )
        })
    {
        let mut metadata = package_probe(cargo, arguments, None, "metadata");
        metadata.args(["--no-deps", "--format-version=1"]);
        let metadata = metadata.output().ok()?;
        if metadata.status.success() {
            if let Some(package_id) = metadata_package_id(&metadata.stdout, selector) {
                return Some(package_id);
            }
        }
    }
    if !output.status.success() {
        if env::var_os(super::TRACE_RUN).is_some() {
            eprintln!(
                "    Cinder trace: Cargo pkgid probe failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
            );
        }
        return None;
    }
    let package_id = String::from_utf8(output.stdout).ok()?;
    let package_id = package_id.trim_end_matches(['\r', '\n']);
    (!package_id.is_empty()).then(|| package_id.to_owned())
}

fn package_cache_path(arguments: &[OsString]) -> Option<(PathBuf, PathBuf)> {
    let kind = match super::cargo_subcommand(arguments) {
        Some("run" | "r") => StateKind::Run,
        Some("build" | "b") => StateKind::Build,
        Some("check" | "c") => StateKind::Check,
        Some("test" | "t") => StateKind::Test,
        _ => return None,
    };
    let directory = fs::canonicalize(env::current_dir().ok()?).ok()?;
    let project = state_project_directory(&directory);
    Some((
        project.join(format!("package-id-{}", kind.directory_name())),
        directory,
    ))
}

fn read_cached_package(path: &Path, context: &[u8]) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_PACKAGE_CACHE_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let bytes = bytes.strip_prefix(PACKAGE_CACHE_MAGIC)?;
    let (length, bytes) = bytes.split_first_chunk::<4>()?;
    let context_length = usize::try_from(u32::from_le_bytes(*length)).ok()?;
    let (recorded_context, package_id) = bytes.split_at_checked(context_length)?;
    if recorded_context != context || package_id.is_empty() {
        return None;
    }
    String::from_utf8(package_id.to_vec()).ok()
}

fn write_cached_package(
    path: &Path,
    workspace: &Path,
    context: &[u8],
    package_id: &str,
) -> Result<(), String> {
    let context_length = u32::try_from(context.len())
        .map_err(|_| "Cargo package context is too large".to_owned())?;
    let total_length = PACKAGE_CACHE_MAGIC
        .len()
        .checked_add(4)
        .and_then(|length| length.checked_add(context.len()))
        .and_then(|length| length.checked_add(package_id.len()))
        .ok_or_else(|| "Cargo package cache is too large".to_owned())?;
    if total_length > usize::try_from(MAX_PACKAGE_CACHE_BYTES).unwrap_or(usize::MAX) {
        return Err("Cargo package cache is too large".to_owned());
    }
    let mut bytes = Vec::with_capacity(total_length);
    bytes.extend_from_slice(PACKAGE_CACHE_MAGIC);
    bytes.extend_from_slice(&context_length.to_le_bytes());
    bytes.extend_from_slice(context);
    bytes.extend_from_slice(package_id.as_bytes());
    let parent = path
        .parent()
        .ok_or_else(|| "Cargo package cache has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create Cargo package cache: {error}"))?;
    make_private_directory(parent)?;
    fs::write(parent.join("workspace"), workspace.as_os_str().as_bytes())
        .map_err(|error| format!("could not record Cargo package workspace: {error}"))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, bytes)
        .and_then(|()| fs::rename(&temporary, path))
        .map_err(|error| format!("could not cache Cargo package ID: {error}"))
}

fn metadata_package_id(bytes: &[u8], selector: Option<&OsStr>) -> Option<String> {
    let metadata: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if let Some(selector) = selector {
        let selector = selector.to_str()?;
        if selector.is_empty()
            || !selector
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return None;
        }
        let mut matching = metadata
            .get("packages")?
            .as_array()?
            .iter()
            .filter(|package| {
                package.get("name").and_then(serde_json::Value::as_str) == Some(selector)
            })
            .filter_map(|package| package.get("id").and_then(serde_json::Value::as_str));
        let package_id = matching.next()?.to_owned();
        return matching.next().is_none().then_some(package_id);
    }
    let default_members = metadata.get("workspace_default_members")?.as_array()?;
    let [package_id] = default_members.as_slice() else {
        return None;
    };
    package_id.as_str().map(str::to_owned)
}

fn package_probe(
    cargo: &Path,
    arguments: &[OsString],
    selector: Option<&OsStr>,
    subcommand: &str,
) -> Command {
    let command_index = super::capture::cargo_subcommand_index(arguments).unwrap_or(0);
    let mut command = Command::new(cargo);
    command.args(&arguments[..command_index]).arg(subcommand);
    let mut index = command_index.saturating_add(1);
    while index < arguments.len() {
        let argument = arguments[index].as_os_str();
        if argument == "--" {
            break;
        }
        if argument == "--manifest-path" {
            if let Some(value) = arguments.get(index + 1) {
                command.arg(argument).arg(value);
            }
            index += 2;
            continue;
        }
        if argument
            .to_str()
            .is_some_and(|argument| argument.starts_with("--manifest-path="))
        {
            command.arg(argument);
        }
        index += 1;
    }
    if let Some(selector) = selector {
        command.arg("--package").arg(selector);
    }
    crate::usage::remove_control_environment(&mut command);
    command
}

fn package_selector(arguments: &[OsString]) -> Option<Option<&OsStr>> {
    let mut selected = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_os_str();
        if argument == "--" {
            break;
        }
        if matches!(argument.to_str(), Some("-p" | "--package")) {
            let value = arguments.get(index + 1)?.as_os_str();
            if selected.replace(value).is_some() {
                return None;
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--package="))
        {
            if value.is_empty() || selected.replace(OsStr::new(value)).is_some() {
                return None;
            }
        }
        index += 1;
    }
    Some(selected)
}

/// Runs Cargo while consuming only its documented JSON build messages.
pub fn run_cargo_messages(
    command: &mut Command,
    receipt_directory: &Path,
    selection: &PackageSelection,
    capture_compiler_recipes: bool,
) -> Result<ExitStatus, String> {
    fs::write(receipt_directory.join(CAPTURE_ENABLED), b"1")
        .map_err(|error| format!("could not stage Cargo message capture: {error}"))?;
    command.stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not execute Cargo: {error}"))?;
    let mut observer = Some(CompilerObserver::start(
        child.id(),
        capture_compiler_recipes,
    ));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Cargo stdout was not captured".to_owned())?;
    let mut reader = BufReader::new(stdout);
    let standard_output = io::stdout();
    let mut forwarded = standard_output.lock();
    let mut capture = MessageCapture::new(receipt_directory, selection, capture_compiler_recipes);
    let mut line = Vec::new();
    let mut build_finished = false;
    let stream_result = (|| {
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .map_err(|error| format!("could not read Cargo output: {error}"))?;
            if read == 0 {
                break;
            }
            match capture.observe(&line) {
                MessageDisposition::Consumed => {}
                MessageDisposition::BuildFinished => {
                    finish_capture(&mut capture, &mut observer);
                    build_finished = true;
                    io::copy(&mut reader, &mut forwarded)
                        .map_err(|error| format!("could not forward program output: {error}"))?;
                    break;
                }
                MessageDisposition::Forward => forwarded
                    .write_all(&line)
                    .map_err(|error| format!("could not forward Cargo output: {error}"))?,
            }
        }
        if !build_finished {
            finish_capture(&mut capture, &mut observer);
        }
        forwarded
            .flush()
            .map_err(|error| format!("could not flush Cargo output: {error}"))
    })();
    drop(reader);
    drop(forwarded);
    if stream_result.is_err() {
        // A closed downstream pipe or read error must not leave Cargo running
        // after Cinder returns. Killing only applies to Cinder's own child.
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for Cargo: {error}"))?;
    stream_result?;
    Ok(status)
}

fn finish_capture(capture: &mut MessageCapture<'_>, observer: &mut Option<CompilerObserver>) {
    if let Some(observer) = observer.take() {
        capture.compiler_recipes = observer.finish();
    }
    capture.finish();
}

/// Prevents Cargo's runner from racing the message reader at build completion.
pub fn wait_for_runner_receipts(receipt_directory: &Path) -> Result<(), String> {
    if !receipt_directory.join(CAPTURE_ENABLED).is_file() {
        return Ok(());
    }
    let ready = receipt_directory.join(CAPTURE_READY);
    let started = Instant::now();
    while !ready.is_file() {
        if started.elapsed() >= RUNNER_READY_TIMEOUT {
            return Err("Cargo message receipts were not ready for the target runner".to_owned());
        }
        thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

enum MessageDisposition {
    Consumed,
    BuildFinished,
    Forward,
}

struct MessageCapture<'a> {
    receipt_directory: &'a Path,
    selected_package_id: Option<String>,
    selected_manifest_path: Option<&'a Path>,
    workspace_root: Option<&'a Path>,
    package_cache_path: Option<&'a Path>,
    artifacts: Vec<serde_json::Value>,
    build_scripts: Vec<(String, PathBuf)>,
    package_manifests: BTreeMap<String, PathBuf>,
    compiler_recipes: Vec<CompilerRecipe>,
    compiler_replay_blocked: bool,
    capture_compiler_recipes: bool,
    disabled: bool,
    finished: bool,
}

impl<'a> MessageCapture<'a> {
    fn new(
        receipt_directory: &'a Path,
        selection: &'a PackageSelection,
        capture_compiler_recipes: bool,
    ) -> Self {
        Self {
            receipt_directory,
            selected_package_id: selection.package_id.clone(),
            selected_manifest_path: selection.manifest_path.as_deref(),
            workspace_root: selection.workspace_root.as_deref(),
            package_cache_path: selection.cache_path.as_deref(),
            artifacts: Vec::new(),
            build_scripts: Vec::new(),
            package_manifests: BTreeMap::new(),
            compiler_recipes: Vec::new(),
            compiler_replay_blocked: false,
            capture_compiler_recipes,
            disabled: false,
            finished: false,
        }
    }

    fn observe(&mut self, line: &[u8]) -> MessageDisposition {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return MessageDisposition::Forward;
        };
        let Some(reason) = value.get("reason").and_then(serde_json::Value::as_str) else {
            return MessageDisposition::Forward;
        };
        match reason {
            "compiler-message" => MessageDisposition::Consumed,
            "compiler-artifact" => {
                self.observe_package_manifest(&value);
                if self.capture_compiler_recipes {
                    match string_array(value.get("target").and_then(|target| target.get("kind"))) {
                        Ok(kinds) if kinds.contains(&"proc-macro") => {
                            self.compiler_replay_blocked = true;
                        }
                        Ok(_) => {}
                        Err(_) => self.compiler_replay_blocked = true,
                    }
                }
                if self.compiler_artifact_matches(&value) {
                    if self.artifacts.len() == MAX_SELECTED_ARTIFACTS {
                        self.artifacts.clear();
                        self.disabled = true;
                    } else if !self.disabled {
                        self.artifacts.push(value);
                    }
                }
                MessageDisposition::Consumed
            }
            "build-script-executed" => {
                let package_id = value.get("package_id").and_then(serde_json::Value::as_str);
                let out_directory = value.get("out_dir").and_then(serde_json::Value::as_str);
                match (package_id, out_directory.map(PathBuf::from)) {
                    (Some(package_id), Some(out_directory))
                        if !package_id.is_empty()
                            && out_directory.is_absolute()
                            && self.build_scripts.len() < MAX_BUILD_SCRIPT_MESSAGES =>
                    {
                        self.build_scripts
                            .push((package_id.to_owned(), out_directory));
                    }
                    _ => self.disabled = true,
                }
                MessageDisposition::Consumed
            }
            "build-finished" => MessageDisposition::BuildFinished,
            _ => MessageDisposition::Forward,
        }
    }

    fn observe_package_manifest(&mut self, value: &serde_json::Value) {
        let Some(package_id) = value.get("package_id").and_then(serde_json::Value::as_str) else {
            self.disabled = true;
            return;
        };
        let Some(manifest) = value
            .get("manifest_path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
        else {
            self.disabled = true;
            return;
        };
        if !manifest.is_absolute() {
            self.disabled = true;
            return;
        }
        if let Some(recorded) = self.package_manifests.get(package_id) {
            if recorded != &manifest {
                self.disabled = true;
            }
            return;
        }
        if self.package_manifests.len() == MAX_PACKAGE_MANIFESTS {
            self.disabled = true;
            return;
        }
        self.package_manifests
            .insert(package_id.to_owned(), manifest);
    }

    fn compiler_artifact_matches(&mut self, value: &serde_json::Value) -> bool {
        let Some(package_id) = value.get("package_id").and_then(serde_json::Value::as_str) else {
            return false;
        };
        // Workspace mode selects every member unit under the canonical root;
        // dependency units outside the root remain graph-validated inputs.
        if let Some(root) = self.workspace_root {
            let Some(manifest) = value
                .get("manifest_path")
                .and_then(serde_json::Value::as_str)
                .map(Path::new)
            else {
                return false;
            };
            return manifest.starts_with(root)
                || fs::canonicalize(manifest)
                    .ok()
                    .is_some_and(|resolved| resolved.starts_with(root));
        }
        if let Some(selected) = self.selected_package_id.as_deref() {
            return selected == package_id;
        }
        let Some(selected_manifest) = self.selected_manifest_path else {
            return false;
        };
        let Some(message_manifest) = value
            .get("manifest_path")
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
        else {
            return false;
        };
        if message_manifest != selected_manifest
            && fs::canonicalize(message_manifest).ok().as_deref() != Some(selected_manifest)
        {
            return false;
        }
        self.selected_package_id = Some(package_id.to_owned());
        true
    }

    /// Resolves the build-script output directory for one selected artifact
    /// message by its package ID. Multi-package workspace captures need this
    /// per receipt; a single-package capture resolves the same value for every
    /// receipt, matching the previous selected-package behavior exactly.
    fn receipt_out_directory(&self, message: &serde_json::Value) -> Result<Option<&Path>, String> {
        let package_id = message
            .get("package_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Cargo artifact has no package ID".to_owned())?;
        let mut matching = self
            .build_scripts
            .iter()
            .filter(|(script_package_id, _)| script_package_id == package_id)
            .map(|(_, out_directory)| out_directory.as_path());
        let Some(first) = matching.next() else {
            return Ok(None);
        };
        if matching.any(|out_directory| out_directory != first) {
            return Err("Cargo selected package has ambiguous build-script output".to_owned());
        }
        Ok(Some(first))
    }

    fn build_script_outputs(&self) -> Result<Vec<BuildScriptOutput>, String> {
        let mut outputs = self
            .build_scripts
            .iter()
            .map(|(package_id, out_directory)| {
                let manifest = self.package_manifests.get(package_id).ok_or_else(|| {
                    format!("Cargo build script has no package manifest: {package_id}")
                })?;
                let manifest_directory = manifest.parent().ok_or_else(|| {
                    format!(
                        "Cargo package manifest has no parent: {}",
                        manifest.display()
                    )
                })?;
                Ok(BuildScriptOutput {
                    manifest_directory: manifest_directory.to_owned(),
                    out_directory: out_directory.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        outputs.sort();
        outputs.dedup();
        Ok(outputs)
    }

    fn package_manifest_paths(&self) -> Vec<PathBuf> {
        let mut manifests = self.package_manifests.values().cloned().collect::<Vec<_>>();
        manifests.sort();
        manifests.dedup();
        manifests
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if env::var_os(super::TRACE_RUN).is_some() {
            eprintln!(
                "    Cinder trace: Cargo messages selected-artifacts={} build-scripts={} disabled={}",
                self.artifacts.len(),
                self.build_scripts.len(),
                self.disabled,
            );
        }
        if self.artifacts.is_empty() {
            if let Some(package_cache_path) = self.package_cache_path {
                let _ = fs::remove_file(package_cache_path);
            }
        }
        if !self.disabled {
            match self.publish_receipts() {
                Ok(()) => {}
                Err(error) if env::var_os(super::TRACE_RUN).is_some() => {
                    eprintln!("    Cinder trace: ignored Cargo artifact messages ({error})");
                }
                Err(_) => {}
            }
        }
        let _ = fs::write(self.receipt_directory.join(CAPTURE_READY), b"1");
    }

    /// Converts every selected Cargo artifact before publishing any receipt.
    /// An unknown output layout must disable the whole optimization candidate;
    /// accepting a parseable subset could make a multi-unit command appear to
    /// have built only one unit.
    fn publish_receipts(&self) -> Result<(), String> {
        // A workspace member reached through a directory symlink has a
        // literal manifest path under the root but a canonical path outside
        // it, so the canonical-root matcher silently drops its units. Any
        // such manifest disables workspace capture entirely.
        if let Some(root) = self.workspace_root {
            for manifest in self.package_manifests.values() {
                if manifest.starts_with(root)
                    && !fs::canonicalize(manifest)
                        .map_err(|error| {
                            format!(
                                "could not resolve Cargo package manifest {}: {error}",
                                manifest.display()
                            )
                        })?
                        .starts_with(root)
                {
                    return Err(format!(
                        "workspace package {} escapes the workspace root",
                        manifest.display()
                    ));
                }
            }
        }
        let build_script_outputs = self.build_script_outputs()?;
        let package_manifests = self.package_manifest_paths();
        // A selected unit that converts to no receipt for any reason other
        // than being a build script is a gap: a later multi-root recording
        // that silently omitted it could reuse state while that unit fails.
        // The gap count is staged beside the receipts so the recorder can
        // refuse under-validated root sets.
        let mut receipt_gaps = 0usize;
        let mut receipts = Vec::new();
        for artifact in &self.artifacts {
            match cargo_artifact_receipt(
                artifact,
                self.receipt_out_directory(artifact)?,
                &package_manifests,
                &build_script_outputs,
            )? {
                Some(receipt) => receipts.push(receipt),
                None if artifact_is_build_script(artifact) => {}
                None => receipt_gaps += 1,
            }
        }
        if !self.compiler_replay_blocked {
            for receipt in &mut receipts {
                let mut matching = self.compiler_recipes.iter().filter(|recipe| {
                    recipe.artifact_path().as_deref() == Some(receipt.artifact.as_path())
                });
                let Some(recipe) = matching.next() else {
                    continue;
                };
                if matching.next().is_none() {
                    let mut recipe = recipe.clone();
                    if recipe
                        .bind_dependency_environment(
                            &receipt.dependency_file,
                            receipt.out_directory.as_deref(),
                        )
                        .is_ok()
                    {
                        receipt.compiler_recipe = Some(recipe);
                    }
                }
            }
        } else if env::var_os(super::TRACE_RUN).is_some() {
            eprintln!(
                "    Cinder trace: compiler replay disabled by unsupported Cargo target graph"
            );
        }
        let paths = receipts
            .iter()
            .enumerate()
            .map(|(index, _)| {
                (
                    self.receipt_directory
                        .join(format!("cargo-message-{index}.pending")),
                    self.receipt_directory
                        .join(format!("cargo-message-{index}.receipt")),
                    self.receipt_directory
                        .join(format!("cargo-message-{index}.recipe-pending")),
                    self.receipt_directory
                        .join(format!("cargo-message-{index}.recipe")),
                )
            })
            .collect::<Vec<_>>();
        let publish = (|| {
            fs::write(
                self.receipt_directory.join(RECEIPT_GAPS),
                receipt_gaps.to_string(),
            )
            .map_err(|error| format!("could not stage the Cargo receipt gap count: {error}"))?;
            for (receipt, (pending, _, recipe_pending, _)) in receipts.iter().zip(&paths) {
                write_artifact_receipt(pending, receipt)?;
                if let Some(recipe) = receipt.compiler_recipe.as_ref() {
                    write_compiler_recipe(recipe_pending, recipe)?;
                }
            }
            for (receipt, (pending, published, recipe_pending, recipe_published)) in
                receipts.iter().zip(&paths)
            {
                if receipt.compiler_recipe.is_some() {
                    fs::rename(recipe_pending, recipe_published).map_err(|error| {
                        format!("could not publish observed compiler recipe: {error}")
                    })?;
                }
                fs::rename(pending, published).map_err(|error| {
                    format!("could not publish Cargo artifact receipt: {error}")
                })?;
            }
            Ok(())
        })();
        if publish.is_err() {
            let _ = fs::remove_file(self.receipt_directory.join(RECEIPT_GAPS));
            for (pending, published, recipe_pending, recipe_published) in paths {
                let _ = fs::remove_file(pending);
                let _ = fs::remove_file(published);
                let _ = fs::remove_file(recipe_pending);
                let _ = fs::remove_file(recipe_published);
            }
        }
        publish
    }
}

/// Reads the staged receipt gap count. A missing or malformed marker means
/// the receipt set cannot be proven complete and the caller must refuse.
pub(super) fn read_receipt_gaps(receipt_directory: &Path) -> Option<usize> {
    let contents = fs::read_to_string(receipt_directory.join(RECEIPT_GAPS)).ok()?;
    let contents = contents.trim();
    if contents.is_empty() || contents.len() > 6 || !contents.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    contents.parse().ok()
}

fn artifact_is_build_script(message: &serde_json::Value) -> bool {
    message
        .get("target")
        .and_then(|target| target.get("kind"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|kinds| {
            kinds
                .iter()
                .any(|kind| kind.as_str() == Some("custom-build"))
        })
}

fn cargo_artifact_receipt(
    message: &serde_json::Value,
    out_directory: Option<&Path>,
    package_manifests: &[PathBuf],
    build_script_outputs: &[BuildScriptOutput],
) -> Result<Option<ArtifactReceipt>, String> {
    let target = message
        .get("target")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "Cargo artifact has no target".to_owned())?;
    let kinds = string_array(target.get("kind"))?;
    if kinds.contains(&"custom-build") {
        return Ok(None);
    }
    let crate_types = string_array(target.get("crate_types"))?;
    let [crate_type] = crate_types.as_slice() else {
        return Ok(None);
    };
    // "proc-macro" is ordinary no-change evidence: the unit's hashed
    // artifact, dep-info, and fingerprint are validated like any other
    // check output, and macro-time environment reads are bound by the
    // whole-environment context identity. This is narrower than replaying a
    // compiler process: the experimental recipe path keeps rejecting any
    // graph containing a proc-macro target, and that guard reads target
    // kinds from the message stream independently of receipt conversion.
    if !matches!(
        *crate_type,
        "bin" | "lib" | "rlib" | "staticlib" | "dylib" | "cdylib" | "proc-macro"
    ) {
        return Ok(None);
    }
    let target_name = target
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Cargo artifact target has no name".to_owned())?;
    let manifest = PathBuf::from(
        message
            .get("manifest_path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Cargo artifact has no manifest path".to_owned())?,
    );
    if !manifest.is_absolute() {
        return Err("Cargo artifact manifest path is not absolute".to_owned());
    }
    let manifest_directory = manifest
        .parent()
        .ok_or_else(|| "Cargo artifact manifest has no parent".to_owned())?
        .to_owned();
    let filenames = string_array(message.get("filenames"))?
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let executable = message
        .get("executable")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    if filenames.iter().any(|path| !path.is_absolute())
        || executable.as_ref().is_some_and(|path| !path.is_absolute())
    {
        return Err("Cargo artifact output path is not absolute".to_owned());
    }

    let (artifact, dependency_file, public_artifact) = if let Some(executable) = executable {
        let adjacent_dependency = executable.with_extension("d");
        if adjacent_dependency.is_file() && artifact_name_has_hash(&executable) {
            let dependency_file = adjacent_dependency;
            (executable.clone(), dependency_file, executable)
        } else {
            let dependency_file = primary_dependency_file(&executable)?;
            let artifact = dependency_file.with_extension("");
            (artifact, dependency_file, executable)
        }
    } else if let Some(metadata) = filenames
        .iter()
        .find(|path| path.extension() == Some(OsStr::new("rmeta")))
    {
        let dependency_file = dependency_file_for_metadata(metadata)?;
        let public_artifact = filenames
            .iter()
            .find(|path| *path != metadata && path.is_file())
            .cloned()
            .unwrap_or_else(|| metadata.clone());
        let artifact = if public_artifact == *metadata
            || (public_artifact.parent() == metadata.parent()
                && artifact_name_has_hash(&public_artifact))
        {
            public_artifact.clone()
        } else {
            linked_artifact_for_metadata(metadata, &public_artifact, &dependency_file)?
        };
        (artifact, dependency_file, public_artifact)
    } else {
        let [public_artifact] = filenames.as_slice() else {
            return Err("Cargo linked artifact has ambiguous outputs".to_owned());
        };
        // A used proc-macro's compiled dylib is a hashed dependency-directory
        // artifact whose dep-info follows the metadata naming rule (`lib`
        // stripped, same hash). Unhashed single outputs remain public
        // artifacts mapped through the dependency directory.
        let direct_dependency = artifact_name_has_hash(public_artifact)
            .then(|| dependency_file_for_metadata(public_artifact).ok())
            .flatten()
            .filter(|dependency_file| dependency_file.is_file());
        if let Some(dependency_file) = direct_dependency {
            (
                public_artifact.clone(),
                dependency_file,
                public_artifact.clone(),
            )
        } else {
            let (artifact, dependency_file) =
                linked_artifact_for_public(target_name, public_artifact)?;
            (artifact, dependency_file, public_artifact.clone())
        }
    };
    if !artifact.is_file() || !dependency_file.is_file() || !public_artifact.is_file() {
        return Err("Cargo artifact outputs are incomplete".to_owned());
    }
    let public_file_name = public_artifact
        .file_name()
        .ok_or_else(|| "Cargo public artifact has no file name".to_owned())?
        .to_owned();
    Ok(Some(ArtifactReceipt {
        artifact,
        dependency_file,
        public_file_name,
        crate_type: if message
            .get("executable")
            .and_then(serde_json::Value::as_str)
            .is_some()
        {
            "bin".to_owned()
        } else {
            (*crate_type).to_owned()
        },
        manifest_directory: Some(manifest_directory),
        out_directory: out_directory.map(Path::to_owned),
        package_manifests: package_manifests.to_vec(),
        build_script_outputs: build_script_outputs.to_vec(),
        compiler_recipe: None,
    }))
}

fn artifact_name_has_hash(path: &Path) -> bool {
    path.file_stem()
        .and_then(OsStr::to_str)
        .and_then(|stem| stem.rsplit_once('-').map(|(_, hash)| hash))
        .is_some_and(|hash| hash.len() >= 8 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn string_array(value: Option<&serde_json::Value>) -> Result<Vec<&str>, String> {
    value
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Cargo message field is not an array".to_owned())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "Cargo message array contains a non-string".to_owned())
        })
        .collect()
}

fn dependency_file_for_metadata(metadata: &Path) -> Result<PathBuf, String> {
    let stem = metadata
        .file_stem()
        .and_then(OsStr::to_str)
        .and_then(|stem| stem.strip_prefix("lib"))
        .ok_or_else(|| "Cargo metadata artifact has an unsupported name".to_owned())?;
    Ok(metadata.with_file_name(format!("{stem}.d")))
}

fn linked_artifact_for_metadata(
    metadata: &Path,
    public_artifact: &Path,
    dependency_file: &Path,
) -> Result<PathBuf, String> {
    let hash = dependency_file
        .file_stem()
        .and_then(OsStr::to_str)
        .and_then(|stem| stem.rsplit_once('-').map(|(_, hash)| hash))
        .filter(|hash| !hash.is_empty() && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "Cargo dependency artifact has no unit hash".to_owned())?;
    let stem = public_artifact
        .file_stem()
        .ok_or_else(|| "Cargo public artifact has no stem".to_owned())?;
    let mut name = stem.to_os_string();
    name.push("-");
    name.push(hash);
    if let Some(extension) = public_artifact.extension() {
        name.push(".");
        name.push(extension);
    }
    let artifact = metadata.with_file_name(name);
    if !artifact.is_file() {
        return Err("Cargo linked artifact is missing from the dependency directory".to_owned());
    }
    super::inputs::cargo_fingerprint_directory(metadata, hash)?;
    Ok(artifact)
}

fn linked_artifact_for_public(
    target_name: &str,
    public_artifact: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let profile_directory = public_artifact
        .parent()
        .ok_or_else(|| "Cargo public artifact has no parent".to_owned())?;
    let dependency_directory = profile_directory.join("deps");
    let public_stem = public_artifact
        .file_stem()
        .ok_or_else(|| "Cargo public artifact has no stem".to_owned())?;
    let dependency_prefix = format!("{}-", target_name.replace('-', "_"));
    let mut matching = Vec::new();
    for entry in fs::read_dir(&dependency_directory)
        .map_err(|error| format!("could not inspect Cargo dependency outputs: {error}"))?
    {
        let dependency_file = entry
            .map_err(|error| format!("could not inspect Cargo dependency output: {error}"))?
            .path();
        let Some(dependency_stem) = dependency_file.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        let Some(hash) = dependency_stem.strip_prefix(&dependency_prefix) else {
            continue;
        };
        if dependency_file.extension() != Some(OsStr::new("d"))
            || hash.is_empty()
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        let mut artifact_name = public_stem.to_os_string();
        artifact_name.push("-");
        artifact_name.push(hash);
        if let Some(extension) = public_artifact.extension() {
            artifact_name.push(".");
            artifact_name.push(extension);
        }
        let artifact = dependency_directory.join(artifact_name);
        let identities = (
            super::inputs::artifact_metadata(&artifact),
            super::inputs::artifact_metadata(public_artifact),
        );
        if artifact.is_file()
            && matches!(identities, (Ok(artifact), Ok(public)) if artifact == public)
            && super::inputs::cargo_fingerprint_directory(profile_directory, hash).is_ok()
        {
            matching.push((artifact, dependency_file));
        }
    }
    match matching.as_slice() {
        [matching] => Ok(matching.clone()),
        [] => Err("Cargo linked artifact has no exact dependency output".to_owned()),
        _ => Err("Cargo linked artifact dependency output is ambiguous".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_package_selectors_conservatively() {
        let arguments = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            package_selector(&arguments(&["check", "-p", "app", "--lib"])),
            Some(Some(OsStr::new("app")))
        );
        assert_eq!(
            package_selector(&arguments(&["check", "--package=app"])),
            Some(Some(OsStr::new("app")))
        );
        assert_eq!(
            package_selector(&arguments(&["check", "--lib"])),
            Some(None)
        );
        assert_eq!(
            package_selector(&arguments(&["check", "-p", "one", "-p", "two"])),
            None
        );
    }

    #[test]
    fn selects_the_current_real_package_manifest_without_spawning_cargo() {
        let arguments = [
            OsString::from("check"),
            OsString::from("--bin"),
            OsString::from("cinder"),
        ];
        let manifest = fs::canonicalize("Cargo.toml").unwrap();
        let parsed =
            toml::from_str::<toml::Table>(&fs::read_to_string(&manifest).unwrap()).unwrap();
        assert!(
            parsed
                .get("package")
                .and_then(toml::Value::as_table)
                .is_some()
        );
        assert!(matches!(
            selected_manifest_mode(&arguments),
            ManifestSelection::Package(selected) if selected == manifest
        ));
    }

    #[test]
    fn resolves_fresh_package_ids_only_when_metadata_is_unambiguous() {
        let metadata = br#"{
            "packages": [
                {"name":"app","id":"path+file:///workspace/app#0.1.0"},
                {"name":"helper","id":"path+file:///workspace/helper#0.1.0"}
            ],
            "workspace_default_members": ["path+file:///workspace/app#0.1.0"]
        }"#;
        assert_eq!(
            metadata_package_id(metadata, None).as_deref(),
            Some("path+file:///workspace/app#0.1.0")
        );
        assert_eq!(
            metadata_package_id(metadata, Some(OsStr::new("helper"))).as_deref(),
            Some("path+file:///workspace/helper#0.1.0")
        );
        assert_eq!(
            metadata_package_id(metadata, Some(OsStr::new("helper@0.1.0"))),
            None
        );

        let ambiguous = br#"{
            "packages": [],
            "workspace_default_members": ["one", "two"]
        }"#;
        assert_eq!(metadata_package_id(ambiguous, None), None);
    }

    #[test]
    fn package_cache_is_context_bound_and_empty_captures_invalidate_it() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "cinder-package-cache-test-{}-{nonce}",
            std::process::id()
        ));
        let cache = root.join("package-id-check");
        write_cached_package(&cache, &root, b"context-a", "package-id").unwrap();
        assert_eq!(
            read_cached_package(&cache, b"context-a").as_deref(),
            Some("package-id")
        );
        assert_eq!(read_cached_package(&cache, b"context-b"), None);

        let receipts = root.join("receipts");
        fs::create_dir(&receipts).unwrap();
        let selection = PackageSelection {
            package_id: Some("package-id".to_owned()),
            manifest_path: None,
            cache_path: Some(cache.clone()),
            workspace_root: None,
        };
        MessageCapture::new(&receipts, &selection, false).finish();
        assert!(!cache.exists());
        assert!(receipts.join(CAPTURE_READY).is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compiler_recipe_capture_rejects_procedural_macros_and_malformed_targets() {
        let selection = PackageSelection {
            package_id: Some("selected".to_owned()),
            manifest_path: None,
            cache_path: None,
            workspace_root: None,
        };
        let artifact = |kind: serde_json::Value| {
            serde_json::to_vec(&serde_json::json!({
                "reason": "compiler-artifact",
                "package_id": "dependency",
                "target": {"kind": kind},
            }))
            .unwrap()
        };

        let mut procedural_macro = MessageCapture::new(Path::new("."), &selection, true);
        assert!(matches!(
            procedural_macro.observe(&artifact(serde_json::json!(["proc-macro"]))),
            MessageDisposition::Consumed
        ));
        assert!(procedural_macro.compiler_replay_blocked);

        let mut malformed = MessageCapture::new(Path::new("."), &selection, true);
        assert!(matches!(
            malformed.observe(&artifact(serde_json::json!("lib"))),
            MessageDisposition::Consumed
        ));
        assert!(malformed.compiler_replay_blocked);

        let mut ordinary = MessageCapture::new(Path::new("."), &selection, true);
        assert!(matches!(
            ordinary.observe(&artifact(serde_json::json!(["lib"]))),
            MessageDisposition::Consumed
        ));
        assert!(!ordinary.compiler_replay_blocked);
    }

    #[test]
    fn malformed_or_overflowing_build_script_messages_disable_receipts() {
        let selection = PackageSelection {
            package_id: Some("selected".to_owned()),
            manifest_path: None,
            cache_path: None,
            workspace_root: None,
        };
        let message = |package_id: serde_json::Value, out_directory: serde_json::Value| {
            serde_json::to_vec(&serde_json::json!({
                "reason": "build-script-executed",
                "package_id": package_id,
                "out_dir": out_directory,
            }))
            .unwrap()
        };

        let mut missing = MessageCapture::new(Path::new("."), &selection, false);
        assert!(matches!(
            missing.observe(&message(
                serde_json::Value::Null,
                serde_json::json!("/tmp/out")
            )),
            MessageDisposition::Consumed
        ));
        assert!(missing.disabled);

        let mut relative = MessageCapture::new(Path::new("."), &selection, false);
        relative.observe(&message(
            serde_json::json!("package"),
            serde_json::json!("out"),
        ));
        assert!(relative.disabled);

        let mut overflowing = MessageCapture::new(Path::new("."), &selection, false);
        overflowing.build_scripts =
            vec![("package".to_owned(), PathBuf::from("/tmp/out")); MAX_BUILD_SCRIPT_MESSAGES];
        overflowing.observe(&message(
            serde_json::json!("package"),
            serde_json::json!("/tmp/out"),
        ));
        assert!(overflowing.disabled);
    }

    #[test]
    fn an_unknown_selected_artifact_suppresses_every_message_receipt() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "cinder-transactional-message-test-{}-{nonce}",
            std::process::id()
        ));
        let receipts = root.join("receipts");
        let dependencies = root.join("target/debug/deps");
        fs::create_dir_all(&receipts).unwrap();
        fs::create_dir_all(&dependencies).unwrap();
        let executable = dependencies.join("app-12345678");
        fs::write(&executable, b"binary").unwrap();
        fs::write(executable.with_extension("d"), b"app: src/main.rs\n").unwrap();
        let manifest = root.join("Cargo.toml");
        fs::write(&manifest, b"[package]\nname='app'\nversion='0.1.0'\n").unwrap();
        let valid = serde_json::json!({
            "package_id": "app 0.1.0 (path+file:///app)",
            "manifest_path": manifest,
            "target": {"kind": ["bin"], "crate_types": ["bin"], "name": "app"},
            "filenames": [executable],
            "executable": executable,
        });
        let invalid = serde_json::json!({
            "package_id": "app 0.1.0 (path+file:///app)",
            "manifest_path": manifest,
        });
        let selection = PackageSelection {
            package_id: Some("app 0.1.0 (path+file:///app)".to_owned()),
            manifest_path: None,
            cache_path: None,
            workspace_root: None,
        };
        let mut capture = MessageCapture::new(&receipts, &selection, false);
        capture.artifacts = vec![valid, invalid];
        capture.finish();
        assert!(receipts.join(CAPTURE_READY).is_file());
        assert!(
            fs::read_dir(&receipts)
                .unwrap()
                .all(|entry| entry.unwrap().path().extension() != Some(OsStr::new("receipt")))
        );
        fs::remove_dir_all(root).unwrap();
    }
}
