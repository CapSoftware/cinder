//! Windows Cargo-parity boundary.
//!
//! Cinder's validated artifact fast paths currently rely on Unix filesystem
//! identities and process replacement. On Windows the binary remains a strict
//! Cargo proxy: every Cargo argument, diagnostic, exit status, and side effect
//! is owned by Cargo rather than approximated by an unsupported cache path.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

pub const EXPERIMENTAL_DIRECT_CHECK: &str = "CINDER_EXPERIMENTAL_DIRECT_CHECK";
pub const TRACE_RUN: &str = "CINDER_TRACE_RUN";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchPolicy {
    Immediate,
    CoalesceDuplicateEvents,
}

impl LaunchPolicy {
    pub const fn for_invocation(invoked_as_cargo: bool) -> Self {
        if invoked_as_cargo {
            Self::CoalesceDuplicateEvents
        } else {
            Self::Immediate
        }
    }
}

pub struct PackageSelection;

pub fn run_context_with_cargo(
    _arguments: &[OsString],
    _launch_policy: LaunchPolicy,
    _cargo: &OsStr,
) -> Vec<u8> {
    Vec::new()
}

pub fn try_fast_run(
    _arguments: &[OsString],
    _context: &[u8],
    _launch_policy: LaunchPolicy,
) -> Result<(), String> {
    Ok(())
}

pub fn try_fast_build(_arguments: &[OsString], _context: &[u8]) -> Result<bool, String> {
    Ok(false)
}

pub fn try_fast_check(_arguments: &[OsString], _context: &[u8]) -> Result<bool, String> {
    Ok(false)
}

pub fn try_fast_test(_arguments: &[OsString], _context: &[u8]) -> Result<Option<u8>, String> {
    Ok(None)
}

pub fn test_execution_eligible(_arguments: &[OsString]) -> Result<bool, String> {
    Ok(false)
}

pub fn artifact_capture_eligible(_arguments: &[OsString]) -> Result<bool, String> {
    Ok(false)
}

pub fn selected_package(
    _cargo: &Path,
    _arguments: &[OsString],
    _context: &[u8],
) -> Option<PackageSelection> {
    None
}

pub fn stage_artifact_receipts() -> Result<PathBuf, String> {
    Err("artifact capture is disabled on Windows".to_owned())
}

pub fn stage_cargo_invocation(
    _receipt_directory: &Path,
    _cargo: &Path,
    _arguments: &[OsString],
) -> Result<(), String> {
    Err("artifact capture is disabled on Windows".to_owned())
}

pub fn stage_run_context(_context: &[u8]) -> Result<PathBuf, String> {
    Err("fast run is disabled on Windows".to_owned())
}

pub fn stage_test_context(_context: &[u8]) -> Result<PathBuf, String> {
    Err("fast test execution is disabled on Windows".to_owned())
}

pub fn run_test_artifact(_arguments: Vec<OsString>) -> Result<u8, String> {
    Err("fast test execution is disabled on Windows".to_owned())
}

pub fn record_test_execution_state_command(_arguments: &[OsString]) -> Result<u8, String> {
    Err("fast test execution is disabled on Windows".to_owned())
}

pub fn cargo_arguments(
    arguments: Vec<OsString>,
    _context_path: Option<&Path>,
    _receipt_directory: Option<&Path>,
    _cargo_messages: bool,
) -> Result<Vec<OsString>, String> {
    Ok(arguments)
}

pub fn cargo_subcommand(arguments: &[OsString]) -> Option<&str> {
    cargo_subcommand_index(arguments).and_then(|index| arguments[index].to_str())
}

fn cargo_subcommand_index(arguments: &[OsString]) -> Option<usize> {
    let mut index = usize::from(
        arguments
            .first()
            .and_then(|argument| argument.to_str())
            .is_some_and(|argument| argument.starts_with('+') && argument.len() > 1),
    );
    while index < arguments.len() {
        let argument = arguments[index].to_str()?;
        match argument {
            "-V" | "--version" | "--list" | "--explain" | "-h" | "--help" => return None,
            "-v" | "--verbose" | "-q" | "--quiet" | "--locked" | "--offline" | "--frozen" => {
                index += 1;
            }
            "--color" | "-C" | "--config" | "-Z" => index = index.checked_add(2)?,
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

pub fn run_cargo_messages(
    command: &mut Command,
    _receipt_directory: &Path,
    _selection: &PackageSelection,
    _capture_compiler_recipes: bool,
) -> Result<ExitStatus, String> {
    command
        .status()
        .map_err(|error| format!("could not execute Cargo: {error}"))
}

pub fn schedule_completed_build(
    _receipt_directory: &Path,
    _context: &[u8],
    _selects_executable: bool,
) -> Result<(), String> {
    Ok(())
}

pub fn schedule_completed_check(
    _receipt_directory: &Path,
    _context: &[u8],
    _selects_executable: bool,
) -> Result<(), String> {
    Ok(())
}

pub fn schedule_completed_test(_receipt_directory: &Path, _context: &[u8]) -> Result<(), String> {
    Ok(())
}

pub fn clear_project_state(_arguments: &[OsString]) -> Result<(), String> {
    Ok(())
}

pub fn run_artifact(_arguments: Vec<OsString>) -> Result<u8, String> {
    Err("the internal artifact runner is unavailable on Windows".to_owned())
}

pub fn record_run_state_command(_arguments: &[OsString]) -> Result<u8, String> {
    Err("the internal run recorder is unavailable on Windows".to_owned())
}

pub fn record_build_state_command(_arguments: Vec<OsString>) -> Result<u8, String> {
    Err("the internal build recorder is unavailable on Windows".to_owned())
}

pub fn record_check_state_command(_arguments: Vec<OsString>) -> Result<u8, String> {
    Err("the internal check recorder is unavailable on Windows".to_owned())
}

pub fn record_test_state_command(_arguments: Vec<OsString>) -> Result<u8, String> {
    Err("the internal test recorder is unavailable on Windows".to_owned())
}
