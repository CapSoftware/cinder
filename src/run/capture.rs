//! Compiler receipt capture and Cargo argument normalization.

use super::{
    ARTIFACT_RECEIPT_DIRECTORY, ArtifactReceipt, OsStr, OsString, PathBuf, StateKind, SystemTime,
    UNIX_EPOCH, eligible, env, fs, host_target, make_private_directory, state_directory,
    toml_string, write_artifact_receipt,
};

pub fn stage_run_context(context: &[u8]) -> Result<PathBuf, String> {
    let directory = fs::canonicalize(
        env::current_dir()
            .map_err(|error| format!("could not inspect current directory: {error}"))?,
    )
    .map_err(|error| format!("could not resolve current directory: {error}"))?;
    let root = state_directory(&directory, StateKind::Run);
    let parent = root
        .parent()
        .ok_or_else(|| "Cinder state directory has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create Cinder context directory: {error}"))?;
    let path = parent.join(format!("run-context-{}", std::process::id()));
    fs::write(&path, context)
        .map_err(|error| format!("could not stage Cinder run context: {error}"))?;
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

/// Records a primary binary produced by one wrapped rustc invocation.
///
/// Receipt failures never alter compiler success; the caller reports them as a
/// disabled optimization and the next command continues through Cargo.
pub fn record_artifact_receipt(arguments: &[OsString]) -> Result<(), String> {
    if env::var_os("CARGO_PRIMARY_PACKAGE").as_deref() != Some("1".as_ref()) {
        return Ok(());
    }
    let emits_link = rustc_list_options(arguments, "--emit")
        .iter()
        .any(|values| values.split(',').any(|value| value == "link"));
    let emits_metadata = rustc_list_options(arguments, "--emit")
        .iter()
        .any(|values| values.split(',').any(|value| value == "metadata"));
    if !emits_link && !emits_metadata {
        return Ok(());
    }
    let is_test_harness = arguments.iter().any(|argument| argument == "--test");
    let mut crate_types: Vec<_> = rustc_list_options(arguments, "--crate-type")
        .into_iter()
        .flat_map(|values| values.split(','))
        .filter(|kind| {
            matches!(
                *kind,
                "bin" | "lib" | "rlib" | "staticlib" | "dylib" | "cdylib"
            )
        })
        .collect();
    if crate_types.is_empty() && is_test_harness {
        crate_types.push("bin");
    }
    let [crate_type] = crate_types.as_slice() else {
        return Ok(());
    };
    if *crate_type == "bin" && env::var_os("CARGO_BIN_NAME").is_none() && !is_test_harness {
        return Ok(());
    }
    let crate_name = rustc_option(arguments, "--crate-name")
        .ok_or_else(|| "binary rustc invocation has no crate name".to_owned())?;
    let out_directory = rustc_option(arguments, "--out-dir")
        .map(PathBuf::from)
        .ok_or_else(|| "binary rustc invocation has no output directory".to_owned())?;
    let extra_filename = rustc_codegen_option(arguments, "extra-filename").unwrap_or_default();
    let artifact = if emits_link {
        out_directory.join(linked_artifact_name(
            crate_name,
            extra_filename,
            crate_type,
        )?)
    } else {
        out_directory.join(metadata_artifact_name(crate_name, extra_filename))
    };
    let mut dependency_name = OsString::from(crate_name);
    dependency_name.push(extra_filename);
    dependency_name.push(".d");
    let dependency_file = out_directory.join(dependency_name);
    if !artifact.is_file() {
        return Err(format!(
            "wrapped compiler did not produce {}",
            artifact.display()
        ));
    }
    let directory = env::var_os(ARTIFACT_RECEIPT_DIRECTORY)
        .map(PathBuf::from)
        .ok_or_else(|| "artifact receipt directory is not configured".to_owned())?;
    let public_file_name = if emits_link && (*crate_type != "bin" || !is_test_harness) {
        public_artifact_name(crate_name, crate_type)?
    } else {
        artifact
            .file_name()
            .ok_or_else(|| "metadata artifact has no file name".to_owned())?
            .to_owned()
    };
    let receipt = ArtifactReceipt {
        artifact,
        dependency_file,
        public_file_name,
        crate_type: (*crate_type).to_owned(),
        manifest_directory: env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
        out_directory: env::var_os("OUT_DIR").map(PathBuf::from),
    };
    write_artifact_receipt(
        &directory.join(format!("{}.receipt", std::process::id())),
        &receipt,
    )
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

pub(super) fn public_artifact_name(
    crate_name: &OsStr,
    crate_type: &str,
) -> Result<OsString, String> {
    if crate_type == "bin" {
        let mut name = env::var_os("CARGO_BIN_NAME")
            .ok_or_else(|| "binary Cargo target has no public name".to_owned())?;
        name.push(env::consts::EXE_SUFFIX);
        return Ok(name);
    }
    linked_artifact_name(crate_name, OsStr::new(""), crate_type)
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
pub fn cargo_arguments(mut arguments: Vec<OsString>) -> Result<Vec<OsString>, String> {
    if !eligible(&arguments)? {
        return Ok(arguments);
    }

    let target = host_target()?;
    let cinder = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    let runner = format!(
        "target.{target}.runner=[{},\"__run-artifact\"]",
        toml_string(cinder.as_os_str())?
    );

    let command_index = cargo_subcommand_index(&arguments)
        .ok_or_else(|| "eligible Cargo run has no subcommand".to_owned())?;
    arguments.insert(command_index + 1, OsString::from("--config"));
    arguments.insert(command_index + 2, OsString::from(runner));
    Ok(arguments)
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
