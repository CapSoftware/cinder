//! Recorded Cargo diagnostic replay.
//!
//! Cargo replays cached compiler warnings and manifest diagnostics on every
//! no-change command. A Cinder reuse hit must therefore reproduce those bytes
//! or it silently changes user-visible output. After a successful capture, the
//! recorder runs a hidden no-change Cargo pass with piped output, keeps the
//! stderr bytes that precede Cargo's `Finished` status line, and stores them
//! with the published state. Every exact-state reuse hit replays the recorded
//! bytes before Cinder's own marker line. Any unexpected hidden-pass output is
//! an abandoned recording, never a guess.

use super::{
    Command, OsStr, OsString, Path, PathBuf, StateKind, StateReader, Stdio,
    cargo_config_pins_term_color, cargo_subcommand_index, env, fs, io, read_bounded_state,
    write_state_bytes,
};

pub(super) const DIAGNOSTICS_MAGIC: &[u8; 8] = b"CNDD0001";
const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1_048_576;

/// The diagnostics a real no-change Cargo pass replays for this exact state.
///
/// `Pinned` holds the single rendering selected by an explicit user color
/// setting. `Both` holds Cargo's plain and ANSI renderings so a later hit can
/// match what Cargo's automatic color choice would print on the current
/// stderr.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DiagnosticsReplay {
    None,
    Pinned(Vec<u8>),
    Both { plain: Vec<u8>, ansi: Vec<u8> },
}

impl DiagnosticsReplay {
    pub(super) const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub(super) fn replay_to_stderr(&self) {
        use io::IsTerminal as _;
        let stderr_is_tty = io::stderr().is_terminal();
        if let Some(bytes) = self.replay_bytes(stderr_is_tty) {
            use io::Write as _;
            let mut stderr = io::stderr().lock();
            let _ = stderr.write_all(bytes);
            let _ = stderr.flush();
        }
    }

    pub(super) fn replay_bytes(&self, stderr_is_tty: bool) -> Option<&[u8]> {
        match self {
            Self::None => None,
            Self::Pinned(bytes) => Some(bytes),
            Self::Both { plain, ansi } => Some(if stderr_is_tty { ansi } else { plain }),
        }
    }
}

pub(super) fn write_diagnostics(
    path: &Path,
    diagnostics: &DiagnosticsReplay,
) -> Result<(), String> {
    let write_blob = |buffer: &mut Vec<u8>, bytes: &[u8]| -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > MAX_DIAGNOSTIC_BYTES {
            return Err("recorded Cargo diagnostics have an unsupported size".to_owned());
        }
        write_state_bytes(buffer, bytes, "diagnostic replay state")
    };
    let mut buffer = Vec::new();
    buffer.extend_from_slice(DIAGNOSTICS_MAGIC);
    match diagnostics {
        DiagnosticsReplay::None => buffer.push(0),
        DiagnosticsReplay::Pinned(bytes) => {
            buffer.push(1);
            write_blob(&mut buffer, bytes)?;
        }
        DiagnosticsReplay::Both { plain, ansi } => {
            buffer.push(2);
            write_blob(&mut buffer, plain)?;
            write_blob(&mut buffer, ansi)?;
        }
    }
    fs::write(path, buffer)
        .map_err(|error| format!("could not write diagnostic replay state: {error}"))
}

pub(super) fn read_diagnostics(path: &Path) -> Result<DiagnosticsReplay, String> {
    let max_len = (DIAGNOSTICS_MAGIC.len() + 1 + 2 * (4 + MAX_DIAGNOSTIC_BYTES)) as u64;
    let contents = read_bounded_state(path, max_len, "diagnostic replay state")?;
    let mut reader = StateReader::new(&contents);
    let magic: [u8; 8] = reader.array("diagnostic replay state")?;
    if &magic != DIAGNOSTICS_MAGIC {
        return Err("diagnostic replay state has an unsupported format".to_owned());
    }
    let [mode] = reader.array::<1>("diagnostic replay state")?;
    let read_blob = |reader: &mut StateReader<'_>| -> Result<Vec<u8>, String> {
        let bytes = reader.length_prefixed(MAX_DIAGNOSTIC_BYTES, "diagnostic replay state")?;
        if bytes.is_empty() {
            return Err("recorded Cargo diagnostics have an unsupported size".to_owned());
        }
        Ok(bytes.to_vec())
    };
    let diagnostics = match mode {
        0 => DiagnosticsReplay::None,
        1 => DiagnosticsReplay::Pinned(read_blob(&mut reader)?),
        2 => {
            let plain = read_blob(&mut reader)?;
            let ansi = read_blob(&mut reader)?;
            DiagnosticsReplay::Both { plain, ansi }
        }
        _ => return Err("diagnostic replay state has an unsupported mode".to_owned()),
    };
    reader.finish("diagnostic replay state")?;
    Ok(diagnostics)
}

/// Runs the hidden no-change passes for a freshly recorded command and returns
/// the diagnostics a later reuse hit must replay. Any error abandons the
/// entire state recording.
pub(super) fn capture_replay_diagnostics(
    cargo: &Path,
    original_arguments: &[OsString],
    kind: StateKind,
    artifact: &Path,
) -> Result<DiagnosticsReplay, String> {
    let arguments = hidden_pass_arguments(original_arguments, kind, artifact)?;
    // An explicit `auto` still depends on the live terminal, and the
    // environment value takes precedence over any configured `term.color`.
    let pinned = match env::var_os("CARGO_TERM_COLOR") {
        Some(value) => value != "auto",
        None => cargo_config_pins_term_color()?,
    };
    if pinned {
        let region = quiet_hidden_pass(cargo, &arguments, None)?;
        if region_is_empty(&region) {
            return Ok(DiagnosticsReplay::None);
        }
        if auto_color_environment_is_overridden() {
            return Err(
                "a color-override environment variable prevents proving the diagnostic replay"
                    .to_owned(),
            );
        }
        return Ok(DiagnosticsReplay::Pinned(region));
    }
    let plain = quiet_hidden_pass(cargo, &arguments, Some("never"))?;
    if region_is_empty(&plain) {
        return Ok(DiagnosticsReplay::None);
    }
    if auto_color_environment_is_overridden() {
        return Err(
            "a color-override environment variable prevents proving the diagnostic replay"
                .to_owned(),
        );
    }
    let ansi = quiet_hidden_pass(cargo, &arguments, Some("always"))?;
    if region_is_empty(&ansi) {
        return Err("Cargo's colored diagnostic replay was unexpectedly empty".to_owned());
    }
    Ok(DiagnosticsReplay::Both { plain, ansi })
}

/// Cargo's automatic color choice also honors `NO_COLOR`, `CLICOLOR_FORCE`,
/// and a dumb terminal. Cinder does not model that rendering matrix; when one
/// of these is present and diagnostics exist, the recording is abandoned so a
/// replay can never diverge from what Cargo itself would print.
fn auto_color_environment_is_overridden() -> bool {
    env::var_os("NO_COLOR").is_some()
        || env::var_os("CLICOLOR_FORCE").is_some()
        || env::var_os("TERM").is_some_and(|term| term == "dumb")
}

fn quiet_hidden_pass(
    cargo: &Path,
    arguments: &[OsString],
    color: Option<&str>,
) -> Result<Vec<u8>, String> {
    let stderr = run_hidden_pass(cargo, arguments, color)?;
    replay_region(&stderr)
}

fn region_is_empty(region: &[u8]) -> bool {
    region.iter().all(u8::is_ascii_whitespace)
}

fn run_hidden_pass(
    cargo: &Path,
    arguments: &[OsString],
    color: Option<&str>,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(cargo);
    command.args(arguments).stdin(Stdio::null());
    crate::usage::remove_control_environment(&mut command);
    if let Some(color) = color {
        command.env("CARGO_TERM_COLOR", color);
    }
    let output = command
        .output()
        .map_err(|error| format!("could not rerun Cargo for diagnostic replay: {error}"))?;
    if !output.status.success() {
        return Err("Cargo's hidden diagnostic replay pass failed".to_owned());
    }
    if output.stderr.len() > MAX_DIAGNOSTIC_BYTES {
        return Err("Cargo's hidden diagnostic replay pass was too large".to_owned());
    }
    Ok(output.stderr)
}

/// Derives the no-change Cargo command for the hidden pass.
///
/// A recorded `test` gains `--no-run` and drops harness arguments so the pass
/// cannot execute tests. A recorded `run` becomes the equivalent `build` of
/// its selected executable so the pass cannot launch the program; without an
/// explicit selector the recorded artifact names the binary target.
pub(super) fn hidden_pass_arguments(
    arguments: &[OsString],
    kind: StateKind,
    artifact: &Path,
) -> Result<Vec<OsString>, String> {
    let command_index = cargo_subcommand_index(arguments)
        .ok_or_else(|| "recorded Cargo command has no subcommand".to_owned())?;
    let mut derived: Vec<OsString> = arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
        .cloned()
        .collect();
    match kind {
        StateKind::Build | StateKind::Check => {}
        StateKind::Test => {
            if !derived[command_index + 1..]
                .iter()
                .any(|argument| argument == "--no-run")
            {
                derived.push(OsString::from("--no-run"));
            }
        }
        StateKind::Run => {
            derived[command_index] = OsString::from("build");
            let has_selector = derived[command_index + 1..].iter().any(|argument| {
                argument.to_str().is_some_and(|argument| {
                    ["--bin", "--example"]
                        .iter()
                        .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
                })
            });
            if !has_selector {
                let name = artifact
                    .file_stem()
                    .ok_or_else(|| "recorded run artifact has no file name".to_owned())?;
                derived.push(OsString::from("--bin"));
                derived.push(name.to_owned());
            }
        }
    }
    Ok(derived)
}

/// Extracts the replayable diagnostic bytes from a hidden pass's stderr.
///
/// The pass must contain exactly one `Finished` status line. Everything before
/// it is the replay region; classification uses ANSI-stripped text while the
/// stored bytes stay exact. A compile, download, or any other status marker
/// proves the pass was not the expected quiet no-change replay, so the
/// recording is abandoned. `Blocking` lock-wait lines only mean a concurrent
/// Cargo briefly held a shared lock while the pass waited; Cargo then
/// proceeded normally, and an uncontended no-change command prints the same
/// bytes without them, so those complete status lines are removed from the
/// stored region rather than failing recording on a busy machine. Rendered
/// rustc diagnostics gutter every source line, so a diagnostic body cannot
/// produce a bare leading `Blocking` status token.
pub(super) fn replay_region(stderr: &[u8]) -> Result<Vec<u8>, String> {
    const FORBIDDEN: [&str; 17] = [
        "Compiling",
        "Checking",
        "Building",
        "Fresh",
        "Running",
        "Executable",
        "Downloading",
        "Downloaded",
        "Updating",
        "Locking",
        "Adding",
        "Removing",
        "Installing",
        "Documenting",
        "Packaging",
        "Verifying",
        "Waiting",
    ];

    let mut finished = false;
    let mut region = Vec::new();
    let mut cursor = 0;
    while cursor <= stderr.len() {
        let line_start = cursor;
        let line_end = stderr[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(stderr.len(), |offset| cursor + offset);
        if line_start == stderr.len() && line_start == line_end {
            break;
        }
        cursor = line_end + 1;
        let stripped = strip_ansi(&stderr[line_start..line_end]);
        let token = status_token(&stripped);
        if token == Some("Blocking") {
            continue;
        }
        if finished {
            match token {
                Some("Finished") => {
                    return Err("Cargo's diagnostic replay finished more than once".to_owned());
                }
                Some("Executable" | "Running") => {}
                None if stripped.iter().all(u8::is_ascii_whitespace) => {}
                Some(_) | None => {
                    return Err(
                        "Cargo's diagnostic replay produced unexpected output after finishing"
                            .to_owned(),
                    );
                }
            }
            continue;
        }
        match token {
            Some("Finished") => finished = true,
            Some(token) if FORBIDDEN.contains(&token) => {
                return Err(format!(
                    "Cargo's diagnostic replay was not a quiet no-change pass ({token})"
                ));
            }
            _ => region.extend_from_slice(&stderr[line_start..cursor.min(stderr.len())]),
        }
    }
    if !finished {
        return Err("Cargo's diagnostic replay never reported completion".to_owned());
    }
    Ok(region)
}

fn status_token(stripped: &[u8]) -> Option<&str> {
    let start = stripped.iter().position(|byte| *byte != b' ')?;
    let word = &stripped[start..];
    let end = word
        .iter()
        .position(|byte| *byte == b' ')
        .filter(|end| *end > 0)?;
    std::str::from_utf8(&word[..end]).ok()
}

/// Removes ANSI escape sequences for classification only. Replayed bytes are
/// stored and written back exactly as Cargo produced them.
pub(super) fn strip_ansi(line: &[u8]) -> Vec<u8> {
    let mut stripped = Vec::with_capacity(line.len());
    let mut cursor = 0;
    while cursor < line.len() {
        if line[cursor] != 0x1b {
            stripped.push(line[cursor]);
            cursor += 1;
            continue;
        }
        match line.get(cursor + 1) {
            Some(b'[') => {
                cursor += 2;
                while cursor < line.len() && !(0x40..=0x7e).contains(&line[cursor]) {
                    cursor += 1;
                }
                cursor = (cursor + 1).min(line.len());
            }
            Some(b']') => {
                cursor += 2;
                while cursor < line.len()
                    && line[cursor] != 0x07
                    && !(line[cursor] == 0x1b && line.get(cursor + 1) == Some(&b'\\'))
                {
                    cursor += 1;
                }
                if cursor < line.len() {
                    cursor += if line[cursor] == 0x07 { 1 } else { 2 };
                }
            }
            Some(_) => cursor += 2,
            None => cursor += 1,
        }
    }
    stripped
}

const MAX_INVOCATION_ARGUMENTS: usize = 4_096;
const MAX_INVOCATION_VALUE_BYTES: usize = 1_048_576;
// macOS bounds a complete argument vector well below this; the cap only
// rejects a corrupt staging file, never a real invocation.
const MAX_INVOCATION_TOTAL_BYTES: u64 = 4 * 1_048_576;
const INVOCATION_MAGIC: &[u8; 8] = b"CNDI0001";

/// Stages the exact original Cargo invocation beside the artifact receipts so
/// the detached recorder can run the hidden diagnostic replay passes.
pub fn stage_cargo_invocation(
    receipt_directory: &Path,
    cargo: &Path,
    arguments: &[OsString],
) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;

    if arguments.len() > MAX_INVOCATION_ARGUMENTS {
        return Err("Cargo invocation has too many arguments to stage".to_owned());
    }
    let total: u64 = std::iter::once(cargo.as_os_str())
        .chain(arguments.iter().map(OsString::as_os_str))
        .map(|value| value.as_bytes().len() as u64 + 4)
        .sum();
    if total > MAX_INVOCATION_TOTAL_BYTES {
        return Err("Cargo invocation is too large to stage".to_owned());
    }
    let write_value = |buffer: &mut Vec<u8>, value: &OsStr| -> Result<(), String> {
        if value.as_bytes().len() > MAX_INVOCATION_VALUE_BYTES {
            return Err("Cargo invocation argument is too long to stage".to_owned());
        }
        write_state_bytes(buffer, value.as_bytes(), "staged Cargo invocation")
    };
    let mut buffer = Vec::new();
    buffer.extend_from_slice(INVOCATION_MAGIC);
    write_value(&mut buffer, cargo.as_os_str())?;
    let count = u32::try_from(arguments.len())
        .map_err(|_| "Cargo invocation has too many arguments to stage".to_owned())?;
    buffer.extend_from_slice(&count.to_le_bytes());
    for argument in arguments {
        write_value(&mut buffer, argument)?;
    }
    fs::write(receipt_directory.join("cargo-invocation"), buffer)
        .map_err(|error| format!("could not stage the Cargo invocation: {error}"))
}

pub(super) fn read_cargo_invocation(
    receipt_directory: &Path,
) -> Result<(PathBuf, Vec<OsString>), String> {
    use std::os::unix::ffi::OsStringExt as _;

    let max_len = INVOCATION_MAGIC.len() as u64 + 4 + MAX_INVOCATION_TOTAL_BYTES;
    let contents = read_bounded_state(
        &receipt_directory.join("cargo-invocation"),
        max_len,
        "staged Cargo invocation",
    )?;
    let mut reader = StateReader::new(&contents);
    let magic: [u8; 8] = reader.array("staged Cargo invocation")?;
    if &magic != INVOCATION_MAGIC {
        return Err("staged Cargo invocation has an unsupported format".to_owned());
    }
    let read_value = |reader: &mut StateReader<'_>| -> Result<OsString, String> {
        reader
            .length_prefixed(MAX_INVOCATION_VALUE_BYTES, "staged Cargo invocation")
            .map(|bytes| OsString::from_vec(bytes.to_vec()))
    };
    let cargo = PathBuf::from(read_value(&mut reader)?);
    let count = u32::from_le_bytes(reader.array("staged Cargo invocation")?) as usize;
    if count > MAX_INVOCATION_ARGUMENTS {
        return Err("staged Cargo invocation has too many arguments".to_owned());
    }
    let mut arguments = Vec::with_capacity(count);
    for _ in 0..count {
        arguments.push(read_value(&mut reader)?);
    }
    reader.finish("staged Cargo invocation")?;
    Ok((cargo, arguments))
}
