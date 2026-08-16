//! Privacy-safe no-op evidence surface for the Windows Cargo proxy.

use std::{
    ffi::{OsStr, OsString},
    process::Command,
    time::Duration,
};

const CONTROL_ENVIRONMENTS: [&str; 17] = [
    "CINDER_USAGE",
    "CINDER_EXPERIMENTAL_DIRECT_CHECK",
    "CINDER_TRACE_RUN",
    "CINDER_SYNCHRONOUS_STATE_RECORDING",
    "CINDER_REAL_CARGO",
    "CINDER_DISABLE_FAST_RUN",
    "CINDER_DISABLE_FAST_BUILD",
    "CINDER_DISABLE_FAST_CHECK",
    "CINDER_DISABLE_FAST_TEST",
    "CINDER_RUN_CONTEXT_FILE",
    "CINDER_ARTIFACT_RECEIPT_DIRECTORY",
    "CINDER_COALESCE_RUN_EVENTS",
    "CINDER_RUSTC_WRAPPER_MODE",
    "CINDER_WRAPPER_ACTIVE",
    "CINDER_NEXT_RUSTC_WRAPPER",
    "CINDER_ORIGINAL_RUSTC_WRAPPER",
    "CINDER_CAPTURE_COMPILER_RECIPE",
];

#[derive(Clone, Copy)]
pub enum CommandKind {
    Run,
    Build,
    Check,
    Test,
}

impl CommandKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Build => "build",
            Self::Check => "check",
            Self::Test => "test",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Outcome {
    CargoFallback,
    CurrentReuse,
    RevisionRestore,
    BinaryPatch,
    FastPathError,
    DirectCompile,
}

pub fn record(_command: CommandKind, _outcome: Outcome, _decision_time: Duration) {}

pub fn remove_control_environment(command: &mut Command) {
    for key in CONTROL_ENVIRONMENTS {
        command.env_remove(key);
    }
}

#[allow(dead_code)]
pub fn is_control_environment(key: &OsStr) -> bool {
    CONTROL_ENVIRONMENTS
        .iter()
        .any(|control| key == OsStr::new(control))
}

pub fn print_report(arguments: &[OsString]) -> Result<u8, String> {
    let json = match arguments {
        [] => false,
        [argument] if argument == "--json" => true,
        _ => return Err("usage: cinder stats [--json]".to_owned()),
    };
    if json {
        let commands = ["run", "build", "check", "test"].map(|command| {
            serde_json::json!({
                "command": command,
                "decisions": 0,
                "outcomes": {
                    "cargo_fallback": 0,
                    "current_reuse": 0,
                    "revision_restore": 0,
                    "binary_patch": 0,
                    "fast_path_error": 0,
                    "direct_compile": 0,
                },
            })
        });
        let report = serde_json::json!({
            "schema_version": 2,
            "collection": "opt-in",
            "recorded_decisions": 0,
            "fast_path_selections": 0,
            "fast_path_selection_rate_basis_points": 0,
            "cargo_fallbacks": 0,
            "fast_path_errors": 0,
            "decision_microseconds": { "median": 0, "p95": 0 },
            "commands": commands,
            "invalid_records": 0,
            "capacity_reached": false,
            "time_saved_estimate": serde_json::Value::Null,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| format!("could not serialize Cinder usage evidence: {error}"))?
        );
    } else {
        println!("Cinder local acceleration evidence");
        println!("Collection: opt-in with CINDER_USAGE=1");
        println!("Recorded decisions: 0");
        println!("Fast-path selections: 0 (0.0%)");
        println!("Cargo fallbacks: 0");
        println!("Fast-path errors: 0");
        println!("Decision overhead: median 0us, p95 0us");
        println!();
        println!(
            "Windows currently uses the strict Cargo proxy; no accelerated decisions are recorded."
        );
        println!();
        println!(
            "This reports observed decisions and lookup overhead; it does not estimate time saved."
        );
    }
    Ok(0)
}
