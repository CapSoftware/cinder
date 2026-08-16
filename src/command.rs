use std::{
    env,
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

/// A tuned Cargo child that died to a signal is reported through this
/// sentinel so the stock fallback can rerun the command; it never reaches
/// the user.
const TUNED_SIGNAL_SENTINEL: &str = "cinder-tuned compiler terminated by signal";

pub fn run_cargo(
    arguments: Vec<OsString>,
    launch_policy: crate::run::LaunchPolicy,
) -> Result<u8, String> {
    let routing = crate::toolchain::apply_routing(&arguments);
    if matches!(routing, crate::toolchain::Routing::Stock) {
        return run_cargo_inner(arguments, launch_policy, routing);
    }
    let original_arguments = arguments.clone();
    match run_cargo_inner(arguments, launch_policy, routing) {
        Ok(code) => Ok(code),
        Err(error) if error == TUNED_SIGNAL_SENTINEL => crate::toolchain::fallback_to_stock(
            &original_arguments,
            crate::toolchain::TunedFailure::Signal,
        ),
        Err(error) if error.starts_with("could not execute") => {
            crate::toolchain::fallback_to_stock(
                &original_arguments,
                crate::toolchain::TunedFailure::Launch,
            )
        }
        Err(error) => Err(error),
    }
}

fn run_cargo_inner(
    arguments: Vec<OsString>,
    launch_policy: crate::run::LaunchPolicy,
    routing: crate::toolchain::Routing,
) -> Result<u8, String> {
    let cargo = cargo_executable()?;
    reject_recursive_delegate(&cargo)?;
    let run_context =
        crate::run::run_context_with_cargo(&arguments, launch_policy, cargo.as_os_str());
    let subcommand = crate::run::cargo_subcommand(&arguments);
    if matches!(subcommand, Some("test" | "t")) {
        if let Some(status) =
            recorded_fast_test_decision(|| crate::run::try_fast_test(&arguments, &run_context))
        {
            return Ok(status);
        }
    }
    let accelerated = match subcommand {
        Some("run" | "r") => recorded_fast_decision(crate::usage::CommandKind::Run, || {
            crate::run::try_fast_run(&arguments, &run_context, launch_policy).map(|()| false)
        }),
        Some("build" | "b") => recorded_fast_decision(crate::usage::CommandKind::Build, || {
            crate::run::try_fast_build(&arguments, &run_context)
        }),
        Some("check" | "c") => recorded_fast_decision(crate::usage::CommandKind::Check, || {
            crate::run::try_fast_check(&arguments, &run_context)
        }),
        Some("test" | "t") => false,
        _ => false,
    };
    if accelerated {
        return Ok(0);
    }
    let capture_mode = if crate::run::artifact_capture_eligible(&arguments)? {
        let capture_compiler_recipes = matches!(subcommand, Some("check" | "c"))
            && env::var_os(crate::run::EXPERIMENTAL_DIRECT_CHECK).as_deref()
                == Some(OsStr::new("1"));
        if let Some(selection) = crate::run::selected_package(&cargo, &arguments, &run_context) {
            CaptureMode::CargoMessages {
                selection,
                capture_compiler_recipes,
            }
        } else {
            CaptureMode::None
        }
    } else {
        CaptureMode::None
    };
    if env::var_os(crate::run::TRACE_RUN).is_some() {
        eprintln!("    Cinder trace: Cargo capture={}", capture_mode.name());
    }
    let mut capture_mode = capture_mode;
    let mut receipt_directory = (!matches!(capture_mode, CaptureMode::None))
        .then(crate::run::stage_artifact_receipts)
        .transpose()?;
    // The staged invocation lets the recorder prove the diagnostic replay.
    // An unstageable invocation only disables capture; the user's Cargo
    // command must still run exactly as requested.
    if let Some(directory) = receipt_directory.as_deref() {
        if let Err(error) = crate::run::stage_cargo_invocation(directory, &cargo, &arguments) {
            if env::var_os(crate::run::TRACE_RUN).is_some() {
                eprintln!("    Cinder trace: Cargo capture disabled ({error})");
            }
            let _ = std::fs::remove_dir_all(directory);
            receipt_directory = None;
            capture_mode = CaptureMode::None;
        }
    }
    let capture_mode = capture_mode;
    let test_execution = matches!(subcommand, Some("test" | "t"))
        && crate::run::test_execution_eligible(&arguments)?;
    let context_path = if receipt_directory.is_some() {
        match subcommand {
            Some("run" | "r") => Some(crate::run::stage_run_context(&run_context)?),
            Some("test" | "t") if test_execution => {
                Some(crate::run::stage_test_context(&run_context)?)
            }
            _ => None,
        }
    } else {
        None
    };
    let captures_build = matches!(subcommand, Some("build" | "b")) && receipt_directory.is_some();
    let captures_check = matches!(subcommand, Some("check" | "c")) && receipt_directory.is_some();
    let captures_test =
        matches!(subcommand, Some("test" | "t")) && receipt_directory.is_some() && !test_execution;
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
    let arguments = crate::run::cargo_arguments(
        arguments,
        context_path.as_deref(),
        receipt_directory.as_deref(),
        matches!(capture_mode, CaptureMode::CargoMessages { .. }),
    )?;

    if captures_build || captures_check || captures_test {
        let receipt_directory = receipt_directory
            .as_deref()
            .ok_or_else(|| "artifact receipt directory was not staged".to_owned())?;
        let mut command = Command::new(&cargo);
        command.args(arguments);
        crate::usage::remove_control_environment(&mut command);
        let status = execute_captured_cargo(&mut command, receipt_directory, &capture_mode)?;
        if matches!(routing, crate::toolchain::Routing::Tuned)
            && crate::toolchain::died_to_signal(status)
        {
            let _ = std::fs::remove_dir_all(receipt_directory);
            return Err(TUNED_SIGNAL_SENTINEL.to_owned());
        }
        if status.success() {
            if captures_build {
                crate::run::schedule_completed_build(
                    receipt_directory,
                    &run_context,
                    selects_executable,
                )
                .unwrap_or_else(|error| {
                    eprintln!("cinder: could not prepare the next fast build: {error}")
                });
            } else if captures_check {
                crate::run::schedule_completed_check(
                    receipt_directory,
                    &run_context,
                    selects_executable,
                )
                .unwrap_or_else(|error| {
                    eprintln!("cinder: could not prepare the next fast check: {error}")
                });
            } else {
                crate::run::schedule_completed_test(receipt_directory, &run_context)
                    .unwrap_or_else(|error| {
                        eprintln!("cinder: could not prepare the next fast test: {error}")
                    });
            }
        }
        if !status.success() {
            let _ = std::fs::remove_dir_all(receipt_directory);
        }
        return Ok(exit_code(status));
    }

    if cleans_project {
        let mut command = Command::new(&cargo);
        command.args(&arguments);
        crate::usage::remove_control_environment(&mut command);
        let status = command
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
        crate::usage::remove_control_environment(&mut command);
        if let Some(receipt_directory) = receipt_directory.as_deref() {
            let status = execute_captured_cargo(&mut command, receipt_directory, &capture_mode)?;
            if matches!(routing, crate::toolchain::Routing::Tuned)
                && crate::toolchain::died_to_signal(status)
            {
                let _ = std::fs::remove_dir_all(receipt_directory);
                if let Some(context_path) = context_path {
                    let _ = std::fs::remove_file(context_path);
                }
                return Err(TUNED_SIGNAL_SENTINEL.to_owned());
            }
            if !status.success() {
                let _ = std::fs::remove_dir_all(receipt_directory);
                if let Some(context_path) = context_path {
                    let _ = std::fs::remove_file(context_path);
                }
            }
            return Ok(exit_code(status));
        }
        if matches!(routing, crate::toolchain::Routing::Tuned) {
            // Keep the parent alive under tuned routing so a crashed tuned
            // compiler can still fall back to the stock toolchain; the stock
            // path keeps Cargo's exact exec semantics.
            let status = command
                .status()
                .map_err(|error| format!("could not execute {}: {error}", cargo.display()))?;
            if crate::toolchain::died_to_signal(status) {
                return Err(TUNED_SIGNAL_SENTINEL.to_owned());
            }
            return Ok(exit_code(status));
        }
        let error = command.exec();
        Err(format!("could not execute {}: {error}", cargo.display()))
    }

    #[cfg(not(unix))]
    {
        let mut command = Command::new(&cargo);
        command.args(arguments);
        crate::usage::remove_control_environment(&mut command);
        let status = if let Some(receipt_directory) = receipt_directory.as_deref() {
            execute_captured_cargo(&mut command, receipt_directory, &capture_mode)?
        } else {
            command
                .status()
                .map_err(|error| format!("could not execute {}: {error}", cargo.display()))?
        };
        Ok(status.code().unwrap_or(1).clamp(0, u8::MAX as i32) as u8)
    }
}

enum CaptureMode {
    None,
    CargoMessages {
        selection: crate::run::PackageSelection,
        capture_compiler_recipes: bool,
    },
}

impl CaptureMode {
    const fn name(&self) -> &'static str {
        match self {
            Self::None => "disabled",
            Self::CargoMessages {
                capture_compiler_recipes: false,
                ..
            } => "messages",
            Self::CargoMessages {
                capture_compiler_recipes: true,
                ..
            } => "messages+compiler-observer",
        }
    }
}

fn execute_captured_cargo(
    command: &mut Command,
    receipt_directory: &Path,
    capture_mode: &CaptureMode,
) -> Result<std::process::ExitStatus, String> {
    match capture_mode {
        CaptureMode::CargoMessages {
            selection,
            capture_compiler_recipes,
        } => crate::run::run_cargo_messages(
            command,
            receipt_directory,
            selection,
            *capture_compiler_recipes,
        ),
        CaptureMode::None => Err("Cargo capture mode was not selected".to_owned()),
    }
}

fn recorded_fast_decision<F>(command: crate::usage::CommandKind, operation: F) -> bool
where
    F: FnOnce() -> Result<bool, String>,
{
    let started = Instant::now();
    let result = operation();
    match result {
        Ok(true) => true,
        Ok(false) => {
            crate::usage::record(
                command,
                crate::usage::Outcome::CargoFallback,
                started.elapsed(),
            );
            false
        }
        Err(error) => {
            crate::usage::record(
                command,
                crate::usage::Outcome::FastPathError,
                started.elapsed(),
            );
            eprintln!(
                "cinder: fast {} unavailable ({error}); using Cargo",
                command.name()
            );
            false
        }
    }
}

fn recorded_fast_test_decision<F>(operation: F) -> Option<u8>
where
    F: FnOnce() -> Result<Option<u8>, String>,
{
    let started = Instant::now();
    match operation() {
        Ok(Some(status)) => Some(status),
        Ok(None) => {
            crate::usage::record(
                crate::usage::CommandKind::Test,
                crate::usage::Outcome::CargoFallback,
                started.elapsed(),
            );
            None
        }
        Err(error) => {
            crate::usage::record(
                crate::usage::CommandKind::Test,
                crate::usage::Outcome::FastPathError,
                started.elapsed(),
            );
            eprintln!("cinder: fast test unavailable ({error}); using Cargo");
            None
        }
    }
}

#[cfg_attr(windows, allow(dead_code))]
pub fn restore_runtime_environment(command: &mut Command) {
    crate::usage::remove_control_environment(command);
}

fn exit_code(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
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
