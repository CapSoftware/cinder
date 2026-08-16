mod benchmark;
mod command;
#[cfg(not(windows))]
mod run;
#[cfg(windows)]
#[path = "run_windows.rs"]
mod run;
mod toolchain;
#[cfg(not(windows))]
mod usage;
#[cfg(windows)]
#[path = "usage_windows.rs"]
mod usage;

use std::{env, ffi::OsString, path::Path, process::ExitCode};

fn main() -> ExitCode {
    let mut process_arguments = env::args_os();
    let executable = process_arguments.next().unwrap_or_default();
    let arguments: Vec<OsString> = process_arguments.collect();
    // The environment-probe wrapper mode engages only when the probe output
    // variable is set AND the first argument is an absolute rustc path — the
    // exact shape Cargo uses for RUSTC_WRAPPER invocations during witness
    // generation. It dumps the invocation and execs the real compiler.
    if let Some(code) = run::env_probe_wrapper_main(&arguments) {
        return ExitCode::from(code);
    }
    let cargo_invocation = invoked_as_cargo(&executable);
    let launch_policy = run::LaunchPolicy::for_invocation(cargo_invocation);
    let result = if cargo_invocation {
        command::run_cargo(arguments, launch_policy)
    } else {
        run(arguments, launch_policy)
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("cinder: {error}");
            ExitCode::FAILURE
        }
    }
}

fn invoked_as_cargo(executable: &std::ffi::OsStr) -> bool {
    Path::new(executable).file_name() == Some("cargo".as_ref())
}

fn run(arguments: Vec<OsString>, launch_policy: run::LaunchPolicy) -> Result<u8, String> {
    match arguments.first().and_then(|value| value.to_str()) {
        None | Some("--help" | "-h") => {
            print_help();
            Ok(0)
        }
        Some("--version" | "-V") => {
            println!("cinder {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some("stats") => usage::print_report(&arguments[1..]),
        Some("__bench-cap") => benchmark::run(arguments.into_iter().skip(1).collect()),
        Some("__run-artifact") => run::run_artifact(arguments.into_iter().skip(1).collect()),
        Some("__run-test-artifact") => {
            run::run_test_artifact(arguments.into_iter().skip(1).collect())
        }
        Some("__record-run") => run::record_run_state_command(&arguments[1..]),
        Some("__record-test-execution") => {
            run::record_test_execution_state_command(&arguments[1..])
        }
        Some("__record-build") => {
            run::record_build_state_command(arguments.into_iter().skip(1).collect())
        }
        Some("__record-check") => {
            run::record_check_state_command(arguments.into_iter().skip(1).collect())
        }
        Some("__record-test") => {
            run::record_test_state_command(arguments.into_iter().skip(1).collect())
        }
        Some("__record-units") => {
            run::record_units_command(arguments.into_iter().skip(1).collect())
        }
        Some(_) => command::run_cargo(arguments, launch_policy),
    }
}

fn print_help() {
    println!(
        "Cinder: a Cargo-compatible development accelerator\n\n\
         Usage: cinder <COMMAND> [OPTIONS]\n\n\
         Common commands:\n  \
           run      Build and run a binary or example\n  \
           build    Compile a package\n  \
           check    Analyze a package without producing a binary\n  \
           test     Run tests\n  \
           stats    Report privacy-safe local acceleration evidence\n\n\
         All other commands and options are passed through to Cargo."
    );
}
