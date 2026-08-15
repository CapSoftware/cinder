use std::{
    env,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Command,
};

const RUSTC_WRAPPER_MODE: &str = "CINDER_RUSTC_WRAPPER_MODE";
const WRAPPER_ACTIVE: &str = "CINDER_WRAPPER_ACTIVE";
const NEXT_RUSTC_WRAPPER: &str = "CINDER_NEXT_RUSTC_WRAPPER";
const ORIGINAL_RUSTC_WRAPPER: &str = "CINDER_ORIGINAL_RUSTC_WRAPPER";

pub fn run_cargo(
    arguments: Vec<OsString>,
    launch_policy: crate::run::LaunchPolicy,
) -> Result<u8, String> {
    let cargo = cargo_executable()?;
    reject_recursive_delegate(&cargo)?;
    let run_context =
        crate::run::run_context_with_cargo(&arguments, launch_policy, cargo.as_os_str());
    let subcommand = crate::run::cargo_subcommand(&arguments);
    match subcommand {
        Some("run" | "r") => {
            if let Err(error) = crate::run::try_fast_run(&arguments, &run_context, launch_policy) {
                eprintln!("cinder: fast run unavailable ({error}); using Cargo");
            }
        }
        Some("build" | "b") => match crate::run::try_fast_build(&arguments, &run_context) {
            Ok(true) => return Ok(0),
            Ok(false) => {}
            Err(error) => eprintln!("cinder: fast build unavailable ({error}); using Cargo"),
        },
        Some("check" | "c") => match crate::run::try_fast_check(&arguments, &run_context) {
            Ok(true) => return Ok(0),
            Ok(false) => {}
            Err(error) => eprintln!("cinder: fast check unavailable ({error}); using Cargo"),
        },
        Some("test" | "t") => match crate::run::try_fast_test(&arguments, &run_context) {
            Ok(true) => return Ok(0),
            Ok(false) => {}
            Err(error) => eprintln!("cinder: fast test unavailable ({error}); using Cargo"),
        },
        _ => {}
    }
    let context_path = matches!(subcommand, Some("run" | "r"))
        .then(|| crate::run::stage_run_context(&run_context))
        .transpose()?;
    let receipt_directory = crate::run::artifact_capture_eligible(&arguments)?
        .then(crate::run::stage_artifact_receipts)
        .transpose()?;
    let captures_build = matches!(subcommand, Some("build" | "b")) && receipt_directory.is_some();
    let captures_check = matches!(subcommand, Some("check" | "c")) && receipt_directory.is_some();
    let captures_test = matches!(subcommand, Some("test" | "t")) && receipt_directory.is_some();
    let cleans_project = subcommand == Some("clean");
    let selects_executable = arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
        .any(|argument| {
            argument.to_str().is_some_and(|argument| {
                ["--bin", "--example", "--test"]
                    .iter()
                    .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
            })
        });
    let arguments = crate::run::cargo_arguments(arguments)?;

    if captures_build {
        let receipt_directory = receipt_directory
            .as_deref()
            .ok_or_else(|| "build receipt directory was not staged".to_owned())?;
        let mut command = Command::new(&cargo);
        command.args(arguments);
        configure_rustc_wrapper(&mut command, receipt_directory)?;
        let status = command
            .status()
            .map_err(|error| format!("could not execute {}: {error}", cargo.display()))?;
        if status.success() {
            crate::run::schedule_completed_build(
                receipt_directory,
                &run_context,
                selects_executable,
            )
            .unwrap_or_else(|error| {
                eprintln!("cinder: could not prepare the next fast build: {error}")
            });
        }
        if !status.success() {
            let _ = std::fs::remove_dir_all(receipt_directory);
        }
        return Ok(exit_code(status));
    }

    if captures_check {
        let receipt_directory = receipt_directory
            .as_deref()
            .ok_or_else(|| "check receipt directory was not staged".to_owned())?;
        let mut command = Command::new(&cargo);
        command.args(arguments);
        configure_rustc_wrapper(&mut command, receipt_directory)?;
        let status = command
            .status()
            .map_err(|error| format!("could not execute {}: {error}", cargo.display()))?;
        if status.success() {
            crate::run::schedule_completed_check(
                receipt_directory,
                &run_context,
                selects_executable,
            )
            .unwrap_or_else(|error| {
                eprintln!("cinder: could not prepare the next fast check: {error}")
            });
        }
        if !status.success() {
            let _ = std::fs::remove_dir_all(receipt_directory);
        }
        return Ok(exit_code(status));
    }

    if captures_test {
        let receipt_directory = receipt_directory
            .as_deref()
            .ok_or_else(|| "test receipt directory was not staged".to_owned())?;
        let mut command = Command::new(&cargo);
        command.args(arguments);
        configure_rustc_wrapper(&mut command, receipt_directory)?;
        let status = command
            .status()
            .map_err(|error| format!("could not execute {}: {error}", cargo.display()))?;
        if status.success() {
            crate::run::schedule_completed_test(receipt_directory, &run_context).unwrap_or_else(
                |error| eprintln!("cinder: could not prepare the next fast test: {error}"),
            );
        }
        if !status.success() {
            let _ = std::fs::remove_dir_all(receipt_directory);
        }
        return Ok(exit_code(status));
    }

    if cleans_project {
        let status = Command::new(&cargo)
            .args(&arguments)
            .status()
            .map_err(|error| format!("could not execute {}: {error}", cargo.display()))?;
        if status.success() {
            crate::run::clear_project_state(&arguments).unwrap_or_else(|error| {
                eprintln!("cinder: Cargo cleaned successfully, but Cinder state remains: {error}");
            });
        }
        return Ok(exit_code(status));
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(&cargo);
        command.args(arguments);
        if let Some(context_path) = context_path {
            command.env(crate::run::RUN_CONTEXT_FILE, context_path);
        }
        if let Some(receipt_directory) = receipt_directory.as_deref() {
            configure_rustc_wrapper(&mut command, receipt_directory)?;
        }
        let error = command.exec();
        Err(format!("could not execute {}: {error}", cargo.display()))
    }

    #[cfg(not(unix))]
    {
        let mut command = Command::new(&cargo);
        command.args(arguments);
        if let Some(context_path) = context_path {
            command.env(crate::run::RUN_CONTEXT_FILE, context_path);
        }
        let status = command
            .status()
            .map_err(|error| format!("could not execute {}: {error}", cargo.display()))?;
        Ok(status.code().unwrap_or(1).clamp(0, u8::MAX as i32) as u8)
    }
}

pub fn restore_runtime_environment(command: &mut Command) {
    if env::var_os(WRAPPER_ACTIVE).as_deref() != Some("1".as_ref()) {
        return;
    }
    if let Some(wrapper) = env::var_os(ORIGINAL_RUSTC_WRAPPER) {
        command.env("RUSTC_WRAPPER", wrapper);
    } else {
        command.env_remove("RUSTC_WRAPPER");
    }
    for key in [
        RUSTC_WRAPPER_MODE,
        WRAPPER_ACTIVE,
        NEXT_RUSTC_WRAPPER,
        ORIGINAL_RUSTC_WRAPPER,
        crate::run::ARTIFACT_RECEIPT_DIRECTORY,
    ] {
        command.env_remove(key);
    }
}

fn configure_rustc_wrapper(command: &mut Command, receipt_directory: &Path) -> Result<(), String> {
    let cinder = env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;
    command
        .env("RUSTC_WRAPPER", &cinder)
        .env(RUSTC_WRAPPER_MODE, "1")
        .env(WRAPPER_ACTIVE, "1")
        .env(crate::run::ARTIFACT_RECEIPT_DIRECTORY, receipt_directory);
    if let Some(wrapper) = env::var_os("RUSTC_WRAPPER").filter(|wrapper| !wrapper.is_empty()) {
        if canonical_if_explicit(Path::new(&wrapper))?.as_deref() == Some(cinder.as_path()) {
            return Err("RUSTC_WRAPPER resolves to Cinder itself".to_owned());
        }
        command
            .env(NEXT_RUSTC_WRAPPER, &wrapper)
            .env(ORIGINAL_RUSTC_WRAPPER, wrapper);
    } else {
        command
            .env_remove(NEXT_RUSTC_WRAPPER)
            .env_remove(ORIGINAL_RUSTC_WRAPPER);
    }
    Ok(())
}

/// Runs Cinder as a transparent `RUSTC_WRAPPER`.
///
/// Cargo invokes wrappers as `<wrapper> <rustc> <rustc arguments...>`. Extra
/// arguments are only added to the explicitly selected crate, so dependency
/// compilation remains identical to Cargo. This is an internal compatibility
/// boundary: normal Cinder commands do not enable experimental compiler flags.
pub fn run_rustc_wrapper(mut arguments: Vec<OsString>) -> Result<u8, String> {
    if arguments.is_empty() {
        return Err("rustc wrapper mode requires the compiler path".to_owned());
    }

    let compiler = PathBuf::from(arguments.remove(0));
    let primary_crate = env::var_os("CINDER_RUSTC_PRIMARY_CRATE");
    let crate_name = rustc_crate_name(&arguments);

    let selected = primary_crate.as_deref() == crate_name;
    if selected {
        if env::var_os("CINDER_RUSTC_RLIB_EXTERNS").as_deref() == Some("1".as_ref()) {
            prefer_rlib_externs(&mut arguments);
        }
        if let Some(extra_arguments) = env::var_os("CINDER_RUSTC_EXTRA_ARGS") {
            let extra_arguments = extra_arguments
                .to_str()
                .ok_or_else(|| "CINDER_RUSTC_EXTRA_ARGS must be valid UTF-8".to_owned())?;
            arguments.extend(
                extra_arguments
                    .split('\n')
                    .filter(|argument| !argument.is_empty())
                    .map(OsString::from),
            );
        }
    }

    let captures_artifact = env::var_os(crate::run::ARTIFACT_RECEIPT_DIRECTORY).is_some();
    if captures_artifact {
        let mut command = wrapped_compiler_command(&compiler, &arguments);
        configure_selected_compiler(&mut command, selected);
        let status = command.status().map_err(|error| {
            format!(
                "could not execute wrapped compiler {}: {error}",
                compiler.display()
            )
        })?;
        if status.success() {
            crate::run::record_artifact_receipt(&arguments).unwrap_or_else(|error| {
                eprintln!("cinder: could not record compiler artifact: {error}");
            });
        }
        return Ok(exit_code(status));
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let mut command = wrapped_compiler_command(&compiler, &arguments);
        configure_selected_compiler(&mut command, selected);
        let error = command.exec();
        Err(format!(
            "could not execute wrapped compiler {}: {error}",
            compiler.display()
        ))
    }

    #[cfg(not(unix))]
    {
        let mut command = wrapped_compiler_command(&compiler, &arguments);
        configure_selected_compiler(&mut command, selected);
        let status = command.status().map_err(|error| {
            format!(
                "could not execute wrapped compiler {}: {error}",
                compiler.display()
            )
        })?;
        Ok(status.code().unwrap_or(1).clamp(0, u8::MAX as i32) as u8)
    }
}

fn wrapped_compiler_command(compiler: &Path, arguments: &[OsString]) -> Command {
    let mut command = env::var_os(NEXT_RUSTC_WRAPPER).map_or_else(
        || Command::new(compiler),
        |wrapper| {
            let mut command = Command::new(wrapper);
            command.arg(compiler);
            command
        },
    );
    command.args(arguments);
    command
}

fn exit_code(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

fn configure_selected_compiler(command: &mut Command, selected: bool) {
    if selected && env::var_os("CINDER_RUSTC_BOOTSTRAP").as_deref() == Some("1".as_ref()) {
        command.env("RUSTC_BOOTSTRAP", "1");
    }
}

fn prefer_rlib_externs(arguments: &mut [OsString]) {
    for index in 0..arguments.len() {
        let value_index = if arguments[index] == "--extern" {
            index + 1
        } else {
            continue;
        };
        let Some(value) = arguments.get(value_index).and_then(|value| value.to_str()) else {
            continue;
        };
        let Some((name, path)) = value.split_once('=') else {
            continue;
        };
        let rlib = Path::new(path).with_extension("rlib");
        if Path::new(path).extension() == Some("rmeta".as_ref()) && rlib.is_file() {
            arguments[value_index] = OsString::from(format!("{name}={}", rlib.display()));
        }
    }
}

fn rustc_crate_name(arguments: &[OsString]) -> Option<&std::ffi::OsStr> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == "--crate-name")
        .map(|pair| pair[1].as_os_str())
}

fn cargo_executable() -> Result<PathBuf, String> {
    if let Some(configured) = env::var_os("CINDER_REAL_CARGO").filter(|value| !value.is_empty()) {
        let configured_path = PathBuf::from(&configured);
        if configured_path.components().count() > 1 {
            return Ok(configured_path);
        }
        return path_executable(&configured, false).ok_or_else(|| {
            format!(
                "CINDER_REAL_CARGO could not be found on PATH: {}",
                configured_path.display()
            )
        });
    }

    path_executable("cargo".as_ref(), true).ok_or_else(|| {
        "could not find the real Cargo executable after Cinder on PATH; set CINDER_REAL_CARGO explicitly"
            .to_owned()
    })
}

fn path_executable(program: &std::ffi::OsStr, skip_current: bool) -> Option<PathBuf> {
    let search_path = env::var_os("PATH")?;
    let current = env::current_exe().ok();
    env::split_paths(&search_path)
        .flat_map(|directory| executable_candidates(&directory, program))
        .find(|candidate| {
            executable_file(candidate)
                && (!skip_current
                    || current
                        .as_deref()
                        .is_none_or(|current| !same_file(candidate, current)))
        })
}

#[cfg(not(windows))]
fn executable_candidates(directory: &Path, program: &std::ffi::OsStr) -> Vec<PathBuf> {
    vec![absolute_search_directory(directory).join(program)]
}

#[cfg(windows)]
fn executable_candidates(directory: &Path, program: &std::ffi::OsStr) -> Vec<PathBuf> {
    let directory = absolute_search_directory(directory);
    let program_path = Path::new(program);
    if program_path.extension().is_some() {
        return vec![directory.join(program)];
    }
    let extensions = env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    extensions
        .to_string_lossy()
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| {
            let mut name = program.to_os_string();
            name.push(extension.to_ascii_lowercase());
            directory.join(name)
        })
        .collect()
}

fn absolute_search_directory(directory: &Path) -> PathBuf {
    if directory.as_os_str().is_empty() {
        env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else if directory.is_absolute() {
        directory.to_owned()
    } else {
        env::current_dir()
            .map(|current| current.join(directory))
            .unwrap_or_else(|_| directory.to_owned())
    }
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(unix)]
fn same_file(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(left) = std::fs::metadata(left) else {
        return false;
    };
    let Ok(right) = std::fs::metadata(right) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file(left: &Path, right: &Path) -> bool {
    std::fs::canonicalize(left).ok() == std::fs::canonicalize(right).ok()
}

fn reject_recursive_delegate(cargo: &Path) -> Result<(), String> {
    let Some(cargo_path) = canonical_if_explicit(cargo)? else {
        return Ok(());
    };
    let current = env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("could not identify the Cinder executable: {error}"))?;

    if cargo_path == current {
        return Err(
            "the selected Cargo executable resolves to Cinder itself; point CINDER_REAL_CARGO at the real Cargo executable"
                .to_owned(),
        );
    }
    Ok(())
}

fn canonical_if_explicit(path: &Path) -> Result<Option<PathBuf>, String> {
    if path.components().count() == 1 {
        return Ok(None);
    }
    std::fs::canonicalize(path)
        .map(Some)
        .map_err(|error: io::Error| format!("could not resolve {}: {error}", path.display()))
}
