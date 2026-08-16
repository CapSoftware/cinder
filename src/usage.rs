//! Privacy-safe, opt-in evidence for Cinder acceleration decisions.
//!
//! Records intentionally contain no paths, command arguments, source, environment
//! values, or project identifiers. The file is a bounded sequence of fixed-size
//! records so recording requires one append and never blocks a Cargo fallback.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;

pub const USAGE_ENVIRONMENT: &str = "CINDER_USAGE";

const CONTROL_ENVIRONMENTS: [&str; 19] = [
    crate::toolchain::STOCK_ENVIRONMENT,
    crate::toolchain::BACKEND_ENVIRONMENT,
    USAGE_ENVIRONMENT,
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

const RECORD_MAGIC: &[u8; 4] = b"CNDM";
const RECORD_VERSION: u8 = 1;
const RECORD_LENGTH: usize = 16;
const MAX_EVENT_BYTES: u64 = 16 * 1024 * 1024;
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CommandKind {
    Run = 1,
    Build = 2,
    Check = 3,
    Test = 4,
}

impl CommandKind {
    const ALL: [Self; 4] = [Self::Run, Self::Build, Self::Check, Self::Test];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Build => "build",
            Self::Check => "check",
            Self::Test => "test",
        }
    }

    const fn index(self) -> usize {
        self as usize - 1
    }

    const fn from_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Run),
            2 => Some(Self::Build),
            3 => Some(Self::Check),
            4 => Some(Self::Test),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Outcome {
    CargoFallback = 1,
    CurrentReuse = 2,
    RevisionRestore = 3,
    BinaryPatch = 4,
    FastPathError = 5,
    DirectCompile = 6,
}

impl Outcome {
    const ALL: [Self; 6] = [
        Self::CargoFallback,
        Self::CurrentReuse,
        Self::RevisionRestore,
        Self::BinaryPatch,
        Self::FastPathError,
        Self::DirectCompile,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::CargoFallback => "cargo_fallback",
            Self::CurrentReuse => "current_reuse",
            Self::RevisionRestore => "revision_restore",
            Self::BinaryPatch => "binary_patch",
            Self::FastPathError => "fast_path_error",
            Self::DirectCompile => "direct_compile",
        }
    }

    const fn index(self) -> usize {
        self as usize - 1
    }

    const fn from_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::CargoFallback),
            2 => Some(Self::CurrentReuse),
            3 => Some(Self::RevisionRestore),
            4 => Some(Self::BinaryPatch),
            5 => Some(Self::FastPathError),
            6 => Some(Self::DirectCompile),
            _ => None,
        }
    }

    const fn selects_fast_path(self) -> bool {
        matches!(
            self,
            Self::CurrentReuse | Self::RevisionRestore | Self::BinaryPatch | Self::DirectCompile
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Event {
    command: CommandKind,
    outcome: Outcome,
    decision_microseconds: u32,
    day: u32,
}

#[derive(Default)]
struct Summary {
    counts: [[u64; Outcome::ALL.len()]; CommandKind::ALL.len()],
    decision_microseconds: Vec<u32>,
    invalid_records: u64,
    capacity_reached: bool,
}

impl Summary {
    fn record(&mut self, event: Event) {
        self.counts[event.command.index()][event.outcome.index()] += 1;
        self.decision_microseconds.push(event.decision_microseconds);
    }

    const fn count(&self, command: CommandKind, outcome: Outcome) -> u64 {
        self.counts[command.index()][outcome.index()]
    }

    fn outcome_count(&self, outcome: Outcome) -> u64 {
        CommandKind::ALL
            .iter()
            .map(|command| self.count(*command, outcome))
            .sum()
    }

    fn total(&self) -> u64 {
        self.counts.iter().flatten().sum()
    }

    fn fast_path_selections(&self) -> u64 {
        Outcome::ALL
            .iter()
            .filter(|outcome| outcome.selects_fast_path())
            .map(|outcome| self.outcome_count(*outcome))
            .sum()
    }

    fn latency_quantile(&mut self, numerator: usize, denominator: usize) -> u32 {
        if self.decision_microseconds.is_empty() {
            return 0;
        }
        self.decision_microseconds.sort_unstable();
        let rank = self
            .decision_microseconds
            .len()
            .saturating_mul(numerator)
            .div_ceil(denominator)
            .saturating_sub(1)
            .min(self.decision_microseconds.len() - 1);
        self.decision_microseconds[rank]
    }
}

/// Records one acceleration decision when `CINDER_USAGE=1` is set.
///
/// Evidence collection is never allowed to affect command success or fallback.
pub fn record(command: CommandKind, outcome: Outcome, elapsed: Duration) {
    if env::var_os(USAGE_ENVIRONMENT).as_deref() != Some(OsStr::new("1")) {
        return;
    }
    let event = Event {
        command,
        outcome,
        decision_microseconds: u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX),
        day: u32::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .checked_div(SECONDS_PER_DAY)
                .unwrap_or_default(),
        )
        .unwrap_or(u32::MAX),
    };
    if let Ok(path) = usage_path() {
        let _ = append_event(&path, event);
    }
}

/// Removes Cinder-only controls from Cargo, compiler, build-script, and program
/// environments. These switches control Cinder's own behavior and must never
/// become observable build inputs.
pub fn remove_control_environment(command: &mut Command) {
    for key in CONTROL_ENVIRONMENTS {
        command.env_remove(key);
    }
}

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
    let mut summary = read_summary(&usage_path()?)?;
    if json {
        print_json_report(&mut summary)?;
    } else {
        print_human_report(&mut summary);
    }
    Ok(0)
}

fn append_event(path: &Path, event: Event) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Cinder usage path has no parent".to_owned())?;
    ensure_private_directory(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err("Cinder usage evidence is not a regular file".to_owned());
        }
    }
    let open = || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    };
    let mut file =
        open().map_err(|error| format!("could not open Cinder usage evidence: {error}"))?;
    file.lock_exclusive()
        .map_err(|error| format!("could not lock Cinder usage evidence: {error}"))?;
    let result = (|| {
        let metadata = file
            .metadata()
            .map_err(|error| format!("could not inspect Cinder usage evidence: {error}"))?;
        if !metadata.is_file() {
            return Err("Cinder usage evidence is not a regular file".to_owned());
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("could not protect Cinder usage evidence: {error}"))?;
        }
        if metadata.len() > MAX_EVENT_BYTES.saturating_sub(RECORD_LENGTH as u64) {
            return Ok(());
        }
        file.write_all(&encode_event(event))
            .map_err(|error| format!("could not append Cinder usage evidence: {error}"))
    })();
    let unlock = FileExt::unlock(&file)
        .map_err(|error| format!("could not unlock Cinder usage evidence: {error}"));
    result.and(unlock)
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("could not create Cinder usage directory: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect Cinder usage directory: {error}"))?;
    if !metadata.file_type().is_dir() {
        return Err("Cinder usage directory is not a real directory".to_owned());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not protect Cinder usage directory: {error}"))?;
    }
    Ok(())
}

fn encode_event(event: Event) -> [u8; RECORD_LENGTH] {
    let mut record = [0_u8; RECORD_LENGTH];
    record[..4].copy_from_slice(RECORD_MAGIC);
    record[4] = RECORD_VERSION;
    record[5] = event.command as u8;
    record[6] = event.outcome as u8;
    record[8..12].copy_from_slice(&event.decision_microseconds.to_le_bytes());
    record[12..16].copy_from_slice(&event.day.to_le_bytes());
    record
}

fn decode_event(record: &[u8]) -> Option<Event> {
    if record.len() != RECORD_LENGTH
        || &record[..4] != RECORD_MAGIC
        || record[4] != RECORD_VERSION
        || record[7] != 0
    {
        return None;
    }
    Some(Event {
        command: CommandKind::from_byte(record[5])?,
        outcome: Outcome::from_byte(record[6])?,
        decision_microseconds: u32::from_le_bytes(record[8..12].try_into().ok()?),
        day: u32::from_le_bytes(record[12..16].try_into().ok()?),
    })
}

fn read_summary(path: &Path) -> Result<Summary, String> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Summary::default()),
        Err(error) => return Err(format!("could not open Cinder usage evidence: {error}")),
    };
    FileExt::lock_shared(&file)
        .map_err(|error| format!("could not lock Cinder usage evidence: {error}"))?;
    let result = (|| {
        let metadata = file
            .metadata()
            .map_err(|error| format!("could not inspect Cinder usage evidence: {error}"))?;
        if !metadata.is_file() {
            return Err("Cinder usage evidence is not a regular file".to_owned());
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len().min(MAX_EVENT_BYTES)).unwrap_or_default(),
        );
        Read::by_ref(&mut file)
            .take(MAX_EVENT_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("could not read Cinder usage evidence: {error}"))?;
        let mut summary = Summary {
            capacity_reached: metadata.len() >= MAX_EVENT_BYTES,
            ..Summary::default()
        };
        let mut records = bytes.chunks_exact(RECORD_LENGTH);
        for record in &mut records {
            match decode_event(record) {
                Some(event) => summary.record(event),
                None => summary.invalid_records += 1,
            }
        }
        if !records.remainder().is_empty() {
            summary.invalid_records += 1;
        }
        Ok(summary)
    })();
    let unlock = FileExt::unlock(&file)
        .map_err(|error| format!("could not unlock Cinder usage evidence: {error}"));
    result.and_then(|summary| unlock.map(|()| summary))
}

fn print_human_report(summary: &mut Summary) {
    let total = summary.total();
    let fast_path_selections = summary.fast_path_selections();
    let selection_rate_tenths = percentage_tenths(fast_path_selections, total);
    let median = summary.latency_quantile(1, 2);
    let p95 = summary.latency_quantile(95, 100);

    println!("Cinder local acceleration evidence");
    println!("Collection: opt-in with CINDER_USAGE=1");
    println!("Recorded decisions: {total}");
    println!(
        "Fast-path selections: {fast_path_selections} ({}.{:01}%)",
        selection_rate_tenths / 10,
        selection_rate_tenths % 10
    );
    println!(
        "Cargo fallbacks: {}",
        summary.outcome_count(Outcome::CargoFallback)
    );
    println!(
        "Fast-path errors: {}",
        summary.outcome_count(Outcome::FastPathError)
    );
    println!("Decision overhead: median {median}us, p95 {p95}us");
    println!();
    println!("command  decisions  selected  fallbacks  errors");
    for command in CommandKind::ALL {
        let decisions: u64 = Outcome::ALL
            .iter()
            .map(|outcome| summary.count(command, *outcome))
            .sum();
        let selected: u64 = Outcome::ALL
            .iter()
            .filter(|outcome| outcome.selects_fast_path())
            .map(|outcome| summary.count(command, *outcome))
            .sum();
        println!(
            "{:<7} {:>9} {:>8} {:>10} {:>7}",
            command.name(),
            decisions,
            selected,
            summary.count(command, Outcome::CargoFallback),
            summary.count(command, Outcome::FastPathError),
        );
    }
    if summary.invalid_records != 0 {
        println!();
        println!(
            "Ignored malformed or partial records: {}",
            summary.invalid_records
        );
    }
    if summary.capacity_reached {
        println!();
        println!("The bounded evidence file is full; new decisions are not being recorded.");
    }
    println!();
    println!(
        "This reports observed decisions and lookup overhead; it does not estimate time saved."
    );
}

fn print_json_report(summary: &mut Summary) -> Result<(), String> {
    let total = summary.total();
    let fast_path_selections = summary.fast_path_selections();
    let median = summary.latency_quantile(1, 2);
    let p95 = summary.latency_quantile(95, 100);
    let commands = CommandKind::ALL.map(|command| {
        let outcomes =
            Outcome::ALL.map(|outcome| (outcome.name(), summary.count(command, outcome)));
        serde_json::json!({
            "command": command.name(),
            "decisions": outcomes.iter().map(|(_, count)| count).sum::<u64>(),
            "outcomes": outcomes.into_iter().collect::<std::collections::BTreeMap<_, _>>(),
        })
    });
    let report = serde_json::json!({
        "schema_version": 2,
        "collection": "opt-in",
        "recorded_decisions": total,
        "fast_path_selections": fast_path_selections,
        "fast_path_selection_rate_basis_points": percentage_tenths(fast_path_selections, total) * 10,
        "cargo_fallbacks": summary.outcome_count(Outcome::CargoFallback),
        "fast_path_errors": summary.outcome_count(Outcome::FastPathError),
        "decision_microseconds": { "median": median, "p95": p95 },
        "commands": commands,
        "invalid_records": summary.invalid_records,
        "capacity_reached": summary.capacity_reached,
        "time_saved_estimate": serde_json::Value::Null,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report)
            .map_err(|error| format!("could not serialize Cinder usage evidence: {error}"))?
    );
    Ok(())
}

fn percentage_tenths(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        0
    } else {
        u64::try_from(u128::from(numerator) * 1_000 / u128::from(denominator)).unwrap_or(u64::MAX)
    }
}

fn usage_path() -> Result<PathBuf, String> {
    let state_home = env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })
        .ok_or_else(|| {
            "could not locate a private user state directory; set XDG_STATE_HOME or HOME".to_owned()
        })?;
    if !state_home.is_absolute() {
        return Err("Cinder user state directory must be absolute".to_owned());
    }
    Ok(state_home.join("cinder").join("events-v1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_records_round_trip_without_private_context() {
        let event = Event {
            command: CommandKind::Build,
            outcome: Outcome::RevisionRestore,
            decision_microseconds: 42,
            day: 20_000,
        };
        let encoded = encode_event(event);
        assert_eq!(encoded.len(), RECORD_LENGTH);
        assert_eq!(decode_event(&encoded), Some(event));
        assert!(!encoded.windows(5).any(|window| window == b"/home"));
        assert!(!encoded.windows(5).any(|window| window == b"cargo"));
    }

    #[test]
    fn malformed_records_are_rejected() {
        let mut encoded = encode_event(Event {
            command: CommandKind::Check,
            outcome: Outcome::CurrentReuse,
            decision_microseconds: 7,
            day: 20_000,
        });
        encoded[5] = u8::MAX;
        assert_eq!(decode_event(&encoded), None);
        encoded[5] = CommandKind::Check as u8;
        encoded[7] = 1;
        assert_eq!(decode_event(&encoded), None);
        assert_eq!(decode_event(&encoded[..RECORD_LENGTH - 1]), None);
    }

    #[test]
    fn summary_counts_only_fast_path_outcomes_as_selections() {
        let mut summary = Summary::default();
        for outcome in Outcome::ALL {
            summary.record(Event {
                command: CommandKind::Run,
                outcome,
                decision_microseconds: outcome as u32,
                day: 20_000,
            });
        }
        assert_eq!(summary.total(), 6);
        assert_eq!(summary.fast_path_selections(), 4);
        assert_eq!(summary.outcome_count(Outcome::CargoFallback), 1);
        assert_eq!(summary.outcome_count(Outcome::FastPathError), 1);
    }

    #[test]
    fn removes_every_cinder_only_control_from_child_environments() {
        let mut command = Command::new("unused");
        for control in CONTROL_ENVIRONMENTS {
            command.env(control, "private-control");
        }
        remove_control_environment(&mut command);

        let removed: Vec<_> = command
            .get_envs()
            .filter_map(|(key, value)| value.is_none().then_some(key))
            .collect();
        for control in CONTROL_ENVIRONMENTS {
            assert!(removed.contains(&OsStr::new(control)));
        }
    }

    #[test]
    fn concurrent_appenders_produce_only_complete_records() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("cinder-usage-test-{}-{unique}", std::process::id()));
        let evidence = root.join("events");
        let mut writers = Vec::new();
        for writer in 0..8 {
            let evidence = evidence.clone();
            writers.push(std::thread::spawn(move || {
                for event in 0..200 {
                    append_event(
                        &evidence,
                        Event {
                            command: CommandKind::Build,
                            outcome: Outcome::CurrentReuse,
                            decision_microseconds: writer * 200 + event,
                            day: 20_000,
                        },
                    )
                    .unwrap();
                }
            }));
        }
        for writer in writers {
            writer.join().unwrap();
        }
        let summary = read_summary(&evidence).unwrap();
        assert_eq!(summary.total(), 1_600);
        assert_eq!(summary.invalid_records, 0);
        assert_eq!(fs::metadata(&evidence).unwrap().len(), 1_600 * 16);
        assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(
            fs::metadata(&evidence).unwrap().permissions().mode() & 0o077,
            0
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn evidence_capacity_is_exact_even_at_the_record_boundary() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("cinder-usage-cap-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let evidence = root.join("events");
        let file = fs::File::create(&evidence).unwrap();
        file.set_len(MAX_EVENT_BYTES - RECORD_LENGTH as u64)
            .unwrap();
        drop(file);
        let event = Event {
            command: CommandKind::Check,
            outcome: Outcome::CargoFallback,
            decision_microseconds: 1,
            day: 20_000,
        };

        append_event(&evidence, event).unwrap();
        assert_eq!(fs::metadata(&evidence).unwrap().len(), MAX_EVENT_BYTES);
        append_event(&evidence, event).unwrap();
        assert_eq!(fs::metadata(&evidence).unwrap().len(), MAX_EVENT_BYTES);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn evidence_file_must_not_be_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("cinder-usage-link-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let target = root.join("target");
        fs::write(&target, b"untouched").unwrap();
        let evidence = root.join("events");
        symlink(&target, &evidence).unwrap();

        let error = append_event(
            &evidence,
            Event {
                command: CommandKind::Check,
                outcome: Outcome::CargoFallback,
                decision_microseconds: 1,
                day: 20_000,
            },
        )
        .unwrap_err();
        assert!(error.contains("not a regular file"));
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        fs::remove_dir_all(root).unwrap();
    }
}
