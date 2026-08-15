//! Cargo command eligibility, configuration guards, and clean integration.

use super::{
    Command, DISABLE_FAST_BUILD, DISABLE_FAST_CHECK, DISABLE_FAST_RUN, DISABLE_FAST_TEST, OsStr,
    OsString, OsStringExt, Path, PathBuf, cargo_subcommand, cargo_subcommand_index, env, fs, io,
    is_cinder_run_artifact, remove_directory_if_present, state_project_directory,
};

pub fn artifact_capture_eligible(arguments: &[OsString]) -> Result<bool, String> {
    match cargo_subcommand(arguments) {
        Some("run" | "r") => eligible(arguments),
        Some("build" | "b") => build_eligible(arguments),
        Some("check" | "c") => check_eligible(arguments),
        Some("test" | "t") => test_eligible(arguments),
        _ => Ok(false),
    }
}

pub(super) fn eligible(arguments: &[OsString]) -> Result<bool, String> {
    if !cfg!(target_os = "macos")
        || env::var_os(DISABLE_FAST_RUN).is_some()
        || !matches!(cargo_subcommand(arguments), Some("run" | "r"))
    {
        return Ok(false);
    }

    let cargo_arguments = arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
        .filter_map(|argument| argument.to_str());
    if cargo_arguments.clone().any(unsupported_argument) {
        return Ok(false);
    }
    if env::var_os("CARGO_BUILD_TARGET").is_some()
        || env::vars_os().any(|(key, _)| runner_environment_key(&key))
    {
        return Ok(false);
    }

    Ok(!cargo_config_may_change_runner_or_target()?)
}

pub(super) fn build_eligible(arguments: &[OsString]) -> Result<bool, String> {
    if !cfg!(target_os = "macos")
        || env::var_os(DISABLE_FAST_BUILD).is_some()
        || !matches!(cargo_subcommand(arguments), Some("build" | "b"))
    {
        return Ok(false);
    }

    let cargo_arguments = arguments.iter().filter_map(|argument| argument.to_str());
    if primary_target_selector_count(arguments) > 1
        || cargo_arguments
            .clone()
            .any(|argument| unsupported_argument(argument) || unsupported_build_argument(argument))
    {
        return Ok(false);
    }
    if env::var_os("CARGO_BUILD_TARGET").is_some() {
        return Ok(false);
    }
    Ok(!cargo_config_may_change_runner_or_target()?)
}

pub(super) fn check_eligible(arguments: &[OsString]) -> Result<bool, String> {
    if !cfg!(target_os = "macos")
        || env::var_os(DISABLE_FAST_CHECK).is_some()
        || !matches!(cargo_subcommand(arguments), Some("check" | "c"))
    {
        return Ok(false);
    }

    if primary_target_selector_count(arguments) > 1
        || arguments
            .iter()
            .filter_map(|argument| argument.to_str())
            .any(|argument| unsupported_argument(argument) || unsupported_check_argument(argument))
    {
        return Ok(false);
    }
    if env::var_os("CARGO_BUILD_TARGET").is_some() {
        return Ok(false);
    }
    Ok(true)
}

pub(super) fn primary_target_selector_count(arguments: &[OsString]) -> usize {
    target_selector_count(arguments, &["--bin", "--example", "--test"])
}

pub(super) fn target_selector_count(arguments: &[OsString], named_selectors: &[&str]) -> usize {
    arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
        .filter_map(|argument| argument.to_str())
        .filter(|argument| {
            *argument == "--lib"
                || named_selectors
                    .iter()
                    .any(|flag| *argument == *flag || argument.starts_with(&format!("{flag}=")))
        })
        .count()
}

pub(super) fn unsupported_check_argument(argument: &str) -> bool {
    matches!(
        argument,
        "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--workspace"
            | "--all"
            | "--timings"
            | "--future-incompat-report"
            | "--build-plan"
            | "--unit-graph"
    ) || ["--bench", "--message-format"]
        .iter()
        .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
}

pub(super) fn test_eligible(arguments: &[OsString]) -> Result<bool, String> {
    let cargo_arguments: Vec<_> = arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
        .collect();
    if !cfg!(target_os = "macos")
        || env::var_os(DISABLE_FAST_TEST).is_some()
        || !matches!(cargo_subcommand(arguments), Some("test" | "t"))
        || !cargo_arguments
            .iter()
            .any(|argument| argument.as_os_str() == "--no-run")
    {
        return Ok(false);
    }

    let selectors = target_selector_count(arguments, &["--bin", "--example", "--test"]);
    if selectors != 1
        || cargo_arguments
            .iter()
            .filter_map(|argument| argument.as_os_str().to_str())
            .any(|argument| unsupported_argument(argument) || unsupported_test_argument(argument))
        || env::var_os("CARGO_BUILD_TARGET").is_some()
    {
        return Ok(false);
    }
    Ok(true)
}

pub(super) fn unsupported_test_argument(argument: &str) -> bool {
    matches!(
        argument,
        "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--workspace"
            | "--all"
            | "--doc"
            | "--timings"
            | "--future-incompat-report"
            | "--build-plan"
            | "--unit-graph"
    ) || ["--bench", "--message-format"]
        .iter()
        .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
}

pub(super) fn unsupported_build_argument(argument: &str) -> bool {
    matches!(
        argument,
        "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--workspace"
            | "--all"
            | "--timings"
            | "--build-plan"
            | "--unit-graph"
    ) || ["--test", "--bench", "--message-format", "--artifact-dir"]
        .iter()
        .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
}

pub fn clear_project_state(arguments: &[OsString]) -> Result<(), String> {
    let current = canonical_current_directory()?;
    let command_directory = cargo_change_directory(arguments)?.unwrap_or(current);
    let mut directories = vec![command_directory.clone()];
    if let Some(directory) = cargo_manifest_directory(arguments, &command_directory)? {
        directories.push(directory);
    }
    directories.sort();
    directories.dedup();
    for directory in directories {
        clear_project_state_at(&directory)?;
    }
    Ok(())
}

pub(super) fn cargo_change_directory(arguments: &[OsString]) -> Result<Option<PathBuf>, String> {
    let Some(command_index) = cargo_subcommand_index(arguments) else {
        return Ok(None);
    };
    let mut selected = None;
    let mut index = 0;
    while index < command_index {
        let argument = arguments[index].to_str().unwrap_or_default();
        if argument == "-C" {
            selected = arguments.get(index + 1).map(PathBuf::from);
            index += 2;
        } else if let Some(directory) = argument.strip_prefix("-C").filter(|path| !path.is_empty())
        {
            selected = Some(PathBuf::from(directory));
            index += 1;
        } else {
            index += 1;
        }
    }
    let Some(selected) = selected else {
        return Ok(None);
    };
    let directory = if selected.is_absolute() {
        selected
    } else {
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?
            .join(selected)
    };
    fs::canonicalize(&directory).map(Some).map_err(|error| {
        format!(
            "could not resolve Cargo -C directory {}: {error}",
            directory.display()
        )
    })
}

pub(super) fn cargo_manifest_directory(
    arguments: &[OsString],
    command_directory: &Path,
) -> Result<Option<PathBuf>, String> {
    let mut manifest = None;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        if argument == "--manifest-path" {
            manifest = arguments.next().map(PathBuf::from);
        } else if let Some(path) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--manifest-path="))
        {
            manifest = Some(PathBuf::from(path));
        }
    }
    let Some(manifest) = manifest else {
        return Ok(None);
    };
    let manifest = if manifest.is_absolute() {
        manifest
    } else {
        command_directory.join(manifest)
    };
    let parent = manifest
        .parent()
        .ok_or_else(|| format!("Cargo manifest has no parent: {}", manifest.display()))?;
    fs::canonicalize(parent).map(Some).map_err(|error| {
        format!(
            "could not resolve Cargo manifest directory {}: {error}",
            parent.display()
        )
    })
}

pub(super) fn clear_project_state_at(directory: &Path) -> Result<(), String> {
    let project = state_project_directory(directory);
    let registry = project.join("run-artifact-roots");
    let roots = match fs::read_dir(&registry) {
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
            if !root.is_absolute() {
                continue;
            }
            let entries = match fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "could not inspect Cinder run artifacts {}: {error}",
                        root.display()
                    ));
                }
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if !is_cinder_run_artifact(directory, &path) {
                    continue;
                }
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "could not remove Cinder run artifact {}: {error}",
                            path.display()
                        ));
                    }
                }
            }
        }
    }
    remove_directory_if_present(&project)
}

pub(super) fn canonical_current_directory() -> Result<PathBuf, String> {
    fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))
}

pub(super) fn unsupported_argument(argument: &str) -> bool {
    argument == "--release"
        || argument == "-r"
        || argument == "--target"
        || argument.starts_with("--target=")
        || argument == "--profile"
        || argument.starts_with("--profile=")
        || argument == "--config"
        || argument.starts_with("--config=")
        || argument == "--manifest-path"
        || argument.starts_with("--manifest-path=")
        || argument == "-C"
        || (argument.starts_with("-C") && argument.len() > 2)
        || argument == "-Z"
        || argument.starts_with("-Z")
}

pub(super) fn runner_environment_key(key: &OsStr) -> bool {
    key.to_str()
        .is_some_and(|key| key.starts_with("CARGO_TARGET_") && key.ends_with("_RUNNER"))
}

pub(super) fn cargo_config_may_change_runner_or_target() -> Result<bool, String> {
    let mut directories = Vec::new();
    let mut directory = env::current_dir()
        .map_err(|error| format!("could not inspect the current directory: {error}"))?;
    loop {
        directories.push(directory.join(".cargo"));
        if !directory.pop() {
            break;
        }
    }
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        directories.push(PathBuf::from(cargo_home));
    } else if let Some(home) = env::var_os("HOME") {
        directories.push(PathBuf::from(home).join(".cargo"));
    }

    for directory in directories {
        for name in ["config.toml", "config"] {
            let path = directory.join(name);
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "could not inspect Cargo configuration {}: {error}",
                        path.display()
                    ));
                }
            };
            if cargo_config_contents_may_change_runner_or_target(&contents).map_err(|error| {
                format!(
                    "could not parse Cargo configuration {}: {error}",
                    path.display()
                )
            })? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn cargo_config_contents_may_change_runner_or_target(
    contents: &str,
) -> Result<bool, String> {
    let config = toml::from_str::<toml::Table>(contents).map_err(|error| error.to_string())?;
    let build_target = config
        .get("build")
        .and_then(toml::Value::as_table)
        .is_some_and(|build| build.contains_key("target"));
    let target_runner = config
        .get("target")
        .and_then(toml::Value::as_table)
        .is_some_and(|targets| {
            targets.values().any(|target| {
                target
                    .as_table()
                    .is_some_and(|target| target.contains_key("runner"))
            })
        });
    Ok(build_target || target_runner)
}

pub(super) fn host_target() -> Result<String, String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|error| format!("could not query the active Rust target: {error}"))?;
    if !output.status.success() {
        return Err("rustc -vV failed while querying the active Rust target".to_owned());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV did not report a host target".to_owned())
}

pub(super) fn toml_string(value: &OsStr) -> Result<String, String> {
    let value = value
        .to_str()
        .ok_or_else(|| "the Cinder executable path must be valid UTF-8".to_owned())?;
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                return Err("the Cinder executable path contains a control character".to_owned());
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    Ok(escaped)
}
