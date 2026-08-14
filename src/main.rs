mod benchmark;
mod command;
mod run;

use std::{env, ffi::OsString, path::Path, process::ExitCode};

fn main() -> ExitCode {
    let mut process_arguments = env::args_os();
    let executable = process_arguments.next().unwrap_or_default();
    let arguments = process_arguments.collect();
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
    let internal_command = arguments
        .first()
        .and_then(|argument| argument.to_str())
        .is_some_and(|argument| argument.starts_with("__"));
    if !internal_command && env::var_os("CINDER_RUSTC_WRAPPER_MODE").is_some() {
        return command::run_rustc_wrapper(arguments);
    }

    match arguments.first().and_then(|value| value.to_str()) {
        None | Some("--help" | "-h") => {
            print_help();
            Ok(0)
        }
        Some("--version" | "-V") => {
            println!("cinder {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some("__bench-cap") => benchmark::run(arguments.into_iter().skip(1).collect()),
        Some("__run-artifact") => run::run_artifact(arguments.into_iter().skip(1).collect()),
        Some("__record-run") => run::record_run_state_command(&arguments[1..]),
        Some("__record-build") => {
            run::record_build_state_command(arguments.into_iter().skip(1).collect())
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
           test     Run tests\n\n\
         All other commands and options are passed through to Cargo."
    );
}
