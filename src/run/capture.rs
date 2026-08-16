//! Compiler receipt capture and Cargo argument normalization.

use super::{
    OsStr, OsString, Path, PathBuf, StateKind, SystemTime, UNIX_EPOCH, eligible, env, fs,
    host_target, make_private_directory, state_directory, test_execution_eligible, toml_string,
};

pub fn stage_run_context(context: &[u8]) -> Result<PathBuf, String> {
    stage_execution_context(context, StateKind::Run, "run")
}

pub fn stage_test_context(context: &[u8]) -> Result<PathBuf, String> {
    stage_execution_context(context, StateKind::Test, "test")
}

fn stage_execution_context(
    context: &[u8],
    kind: StateKind,
    label: &str,
) -> Result<PathBuf, String> {
    let directory = fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))?;
    let root = state_directory(&directory, kind);
    let parent = root
        .parent()
        .ok_or_else(|| "Cinder state directory has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create Cinder context directory: {error}"))?;
    let path = parent.join(format!("{label}-context-{}", std::process::id()));
    fs::write(&path, context)
        .map_err(|error| format!("could not stage Cinder {label} context: {error}"))?;
    Ok(path)
}

pub fn stage_artifact_receipts() -> Result<PathBuf, String> {
    let root = env::temp_dir().join("cinder").join("receipts");
    fs::create_dir_all(&root)
        .map_err(|error| format!("could not create Cinder receipt directory: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates the Unix epoch".to_owned())?
        .as_nanos();
    let directory = root.join(format!("{}-{nonce}", std::process::id()));
    fs::create_dir(&directory)
        .map_err(|error| format!("could not stage Cinder artifact receipts: {error}"))?;
    make_private_directory(&directory)?;
    Ok(directory)
}

pub(super) fn metadata_artifact_name(crate_name: &OsStr, extra_filename: &OsStr) -> OsString {
    let mut name = OsString::from("lib");
    name.push(crate_name);
    name.push(extra_filename);
    name.push(".rmeta");
    name
}

pub(super) fn linked_artifact_name(
    crate_name: &OsStr,
    extra_filename: &OsStr,
    crate_type: &str,
) -> Result<OsString, String> {
    let mut name = OsString::new();
    match crate_type {
        "bin" => name.push(crate_name),
        "lib" | "rlib" => {
            name.push("lib");
            name.push(crate_name);
        }
        "staticlib" => {
            name.push("lib");
            name.push(crate_name);
        }
        "dylib" | "cdylib" => {
            name.push(env::consts::DLL_PREFIX);
            name.push(crate_name);
        }
        _ => return Err(format!("unsupported Cargo artifact type: {crate_type}")),
    }
    name.push(extra_filename);
    match crate_type {
        "bin" => name.push(env::consts::EXE_SUFFIX),
        "lib" | "rlib" => name.push(".rlib"),
        "staticlib" => name.push(".a"),
        "dylib" | "cdylib" => name.push(env::consts::DLL_SUFFIX),
        _ => return Err(format!("unsupported Cargo artifact type: {crate_type}")),
    }
    Ok(name)
}

pub(super) fn rustc_option<'a>(arguments: &'a [OsString], option: &str) -> Option<&'a OsStr> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == option)
        .map(|pair| pair[1].as_os_str())
        .or_else(|| {
            let prefix = format!("{option}=");
            arguments
                .iter()
                .filter_map(|argument| argument.to_str())
                .find_map(|argument| argument.strip_prefix(&prefix).map(OsStr::new))
        })
}

pub(super) fn rustc_list_options<'a>(arguments: &'a [OsString], option: &str) -> Vec<&'a str> {
    let joined_prefix = format!("{option}=");
    let mut values = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if argument == option {
            if let Some(value) = arguments.get(index + 1).and_then(|value| value.to_str()) {
                values.push(value);
            }
        } else if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix(&joined_prefix))
        {
            values.push(value);
        }
    }
    values
}

pub(super) fn rustc_codegen_option<'a>(
    arguments: &'a [OsString],
    option: &str,
) -> Option<&'a OsStr> {
    let prefix = format!("{option}=");
    arguments
        .windows(2)
        .filter(|pair| pair[0] == "-C")
        .filter_map(|pair| pair[1].to_str())
        .find_map(|argument| argument.strip_prefix(&prefix).map(OsStr::new))
        .or_else(|| {
            let joined_prefix = format!("-C{prefix}");
            arguments
                .iter()
                .filter_map(|argument| argument.to_str())
                .find_map(|argument| argument.strip_prefix(&joined_prefix).map(OsStr::new))
        })
}

/// Preserves Cargo's `run` implementation and replaces only its final target
/// runner. Cargo therefore remains responsible for package/target selection,
/// builds, diagnostics, dynamic-library paths, and the application environment.
pub fn cargo_arguments(
    mut arguments: Vec<OsString>,
    context_path: Option<&Path>,
    receipt_directory: Option<&Path>,
    cargo_messages: bool,
) -> Result<Vec<OsString>, String> {
    let runner = if eligible(&arguments)? {
        Some("__run-artifact")
    } else if test_execution_eligible(&arguments)? {
        Some("__run-test-artifact")
    } else {
        None
    };
    if env::var_os(super::TRACE_RUN).is_some() {
        eprintln!(
            "    Cinder trace: Cargo target runner={}",
            runner.unwrap_or("disabled")
        );
    }
    if let Some(runner_command) = runner {
        let (Some(context_path), Some(receipt_directory)) = (context_path, receipt_directory)
        else {
            return Ok(arguments);
        };
        let target = host_target()?;
        let cinder = env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
        let runner = if runner_command == "__run-test-artifact" {
            let project = fs::canonicalize(
                env::current_dir()
                    .map_err(|error| format!("could not inspect current directory: {error}"))?,
            )
            .map_err(|error| format!("could not resolve current directory: {error}"))?;
            let recording_mode = if env::var_os(super::SYNCHRONOUS_STATE_RECORDING).is_some() {
                "sync"
            } else {
                "async"
            };
            format!(
                "target.{target}.runner=[{},{},{},{},{},{}]",
                toml_string(cinder.as_os_str())?,
                toml_string(OsStr::new(runner_command))?,
                toml_string(context_path.as_os_str())?,
                toml_string(receipt_directory.as_os_str())?,
                toml_string(project.as_os_str())?,
                toml_string(OsStr::new(recording_mode))?,
            )
        } else {
            format!(
                "target.{target}.runner=[{},{},{},{}]",
                toml_string(cinder.as_os_str())?,
                toml_string(OsStr::new(runner_command))?,
                toml_string(context_path.as_os_str())?,
                toml_string(receipt_directory.as_os_str())?,
            )
        };
        insert_cargo_config(&mut arguments, runner)?;
    }
    if cargo_messages {
        insert_cargo_option(
            &mut arguments,
            OsString::from("--message-format=json-render-diagnostics"),
        )?;
    }

    Ok(arguments)
}

fn insert_cargo_option(arguments: &mut Vec<OsString>, value: OsString) -> Result<(), String> {
    let command_index = cargo_subcommand_index(arguments)
        .ok_or_else(|| "eligible Cargo command has no subcommand".to_owned())?;
    arguments.insert(command_index + 1, value);
    Ok(())
}

fn insert_cargo_config(arguments: &mut Vec<OsString>, value: String) -> Result<(), String> {
    let command_index = cargo_subcommand_index(arguments)
        .ok_or_else(|| "eligible Cargo command has no subcommand".to_owned())?;
    arguments.insert(command_index + 1, OsString::from("--config"));
    arguments.insert(command_index + 2, OsString::from(value));
    Ok(())
}

pub fn cargo_subcommand(arguments: &[OsString]) -> Option<&str> {
    cargo_subcommand_index(arguments).and_then(|index| arguments[index].to_str())
}

pub(super) fn cargo_subcommand_index(arguments: &[OsString]) -> Option<usize> {
    let mut index = 0;
    if arguments
        .first()
        .and_then(|argument| argument.to_str())
        .is_some_and(|argument| argument.starts_with('+') && argument.len() > 1)
    {
        index += 1;
    }

    while index < arguments.len() {
        let argument = arguments[index].to_str()?;
        match argument {
            "-V" | "--version" | "--list" | "--explain" | "-h" | "--help" => return None,
            "-v" | "--verbose" | "-q" | "--quiet" | "--locked" | "--offline" | "--frozen" => {
                index += 1
            }
            "--color" | "-C" | "--config" | "-Z" => {
                index = index.checked_add(2)?;
            }
            _ if argument.len() > 2
                && argument.starts_with('-')
                && argument[1..].bytes().all(|byte| byte == b'v') =>
            {
                index += 1;
            }
            _ if argument.starts_with("--explain=") => return None,
            _ if argument.starts_with("--color=")
                || argument.starts_with("--config=")
                || (argument.starts_with("-C") && argument.len() > 2)
                || (argument.starts_with("-Z") && argument.len() > 2) =>
            {
                index += 1;
            }
            _ if argument.starts_with('-') => return None,
            _ => return Some(index),
        }
    }
    None
}
