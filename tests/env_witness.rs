#![cfg(target_os = "macos")]

//! End-to-end proof of the probe-verified environment witness: a graph
//! containing a procedural macro — whose macro body performs an untracked
//! `std::env::var` read at expansion time — records a complete witnessed
//! compiler recipe and replays it directly on a later source change, and the
//! replayed artifact is byte-identical to what Cargo itself then reports
//! fresh. Before the witness existed such graphs were blanket-blocked.

use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct WitnessFixture {
    root: PathBuf,
}

impl WitnessFixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cinder-env-witness-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("pm/src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"env-witness-fixture\"\nversion = \"0.1.0\"\n\
             edition = \"2021\"\n\n[lib]\ncrate-type = [\"rlib\"]\n\n\
             [dependencies]\nwitness_pm = { path = \"pm\" }\n",
        )
        .unwrap();
        fs::write(
            root.join("pm/Cargo.toml"),
            "[package]\nname = \"witness_pm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [lib]\nproc-macro = true\n",
        )
        .unwrap();
        fs::write(
            root.join("pm/src/lib.rs"),
            "use proc_macro::TokenStream;\n\n\
             #[proc_macro]\npub fn pm_value(_input: TokenStream) -> TokenStream {\n    \
             // An untracked macro-time environment read: dep-info never\n    \
             // records it, so replay soundness rests on the witness.\n    \
             let suffix = std::env::var(\"CARGO_PKG_NAME\").unwrap_or_default().len() as u32;\n    \
             format!(\"{}u32\", 100 + suffix).parse().unwrap()\n}\n",
        )
        .unwrap();
        Self { root }
    }

    fn write_source(&self, value: i32) {
        fs::write(
            self.root.join("src/lib.rs"),
            format!("pub fn value() -> u32 {{ witness_pm::pm_value!() + {value} }}\n"),
        )
        .unwrap();
    }

    fn check_direct(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cinder"))
            .current_dir(&self.root)
            .args(["check", "--lib"])
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CINDER_EXPERIMENTAL_DIRECT_CHECK", "1")
            .env("CINDER_TRACE_RUN", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .output()
            .unwrap()
    }

    fn state_directory(&self) -> PathBuf {
        let canonical = fs::canonicalize(&self.root).unwrap();
        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        std::env::temp_dir()
            .join("cinder/state")
            .join(format!("{:016x}", hasher.finish()))
    }

    fn library_rmeta(&self) -> PathBuf {
        fs::read_dir(self.root.join("target/debug/deps"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension() == Some(std::ffi::OsStr::new("rmeta"))
                    && path
                        .file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|name| name.starts_with("libenv_witness_fixture-"))
            })
            .expect("the fixture produced no library rmeta")
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Scans every stored witness for one carrying the given byte needle.
fn witnesses_containing(needle: &[u8]) -> Vec<PathBuf> {
    let root = std::env::temp_dir().join("cinder/state/env-witness");
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("witness"))
        .filter(|path| {
            fs::read(path).is_ok_and(|bytes| {
                bytes
                    .windows(needle.len().max(1))
                    .any(|window| window == needle)
            })
        })
        .collect()
}

#[test]
fn witness_generation_follows_the_pinned_toolchain() {
    // The Cap-shaped regression: the invocation directory pins a toolchain
    // through rust-toolchain.toml, so the witness key derives from the
    // pinned cargo and rustc — generation must resolve the same pin, not
    // the rustup default, or every pinned project permanently declines.
    let pinned_cargo = b"/toolchains/1.85.0-aarch64-apple-darwin/bin/cargo".as_slice();
    let default_cargo = b"/toolchains/stable-aarch64-apple-darwin/bin/cargo".as_slice();

    // Earlier sessions may have left witnesses for this toolchain in the
    // shared store; remove them so both phases prove fresh generation.
    for stale in witnesses_containing(pinned_cargo) {
        if let Some(directory) = stale.parent() {
            let _ = fs::remove_dir_all(directory);
        }
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("cinder-witness-pin-{}-{nonce}", std::process::id()));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"witness-pin-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [lib]\ncrate-type = [\"rlib\"]\n",
    )
    .unwrap();
    fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.85.0\"\n",
    )
    .unwrap();
    let check = |value: i32, override_toolchain: bool| {
        fs::write(
            root.join("src/lib.rs"),
            format!("pub fn value() -> i32 {{ {value} }}\n"),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&root)
            .args(["check", "--lib"])
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CINDER_EXPERIMENTAL_DIRECT_CHECK", "1")
            .env("CINDER_TRACE_RUN", "1")
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env_remove("RUSTC");
        if override_toolchain {
            command.env("RUSTUP_TOOLCHAIN", "1.85.0-aarch64-apple-darwin");
        } else {
            command.env_remove("RUSTUP_TOOLCHAIN");
        }
        let output = command.output().unwrap();
        assert_success(&output);
    };

    // Pin-file resolution: the shim discovers rust-toolchain.toml by
    // directory and injects RUSTUP_TOOLCHAIN itself.
    let mut pinned = Vec::new();
    for value in 2..=5 {
        check(value, false);
        pinned = witnesses_containing(pinned_cargo);
        if !pinned.is_empty() {
            break;
        }
    }
    assert!(
        !pinned.is_empty(),
        "no witness carries the pinned toolchain's Cargo path"
    );
    for path in &pinned {
        let bytes = fs::read(path).unwrap();
        assert!(
            !bytes
                .windows(default_cargo.len())
                .any(|window| window == default_cargo),
            "a pinned-context witness carries the default toolchain's Cargo: {}",
            path.display()
        );
        assert!(
            bytes
                .windows(b"RUSTUP_TOOLCHAIN".len())
                .any(|window| window == b"RUSTUP_TOOLCHAIN"),
            "a pinned-context witness records no toolchain constant: {}",
            path.display()
        );
    }

    // Explicit environment override: a distinct launch context (the
    // override is inherited rather than injected) whose witness must also
    // carry the overridden toolchain's Cargo under its own key.
    let before = witnesses_containing(pinned_cargo);
    let mut grown = false;
    for value in 6..=9 {
        check(value, true);
        if witnesses_containing(pinned_cargo)
            .iter()
            .any(|path| !before.contains(path))
        {
            grown = true;
            break;
        }
    }
    assert!(
        grown,
        "the RUSTUP_TOOLCHAIN override context generated no witness with the overridden Cargo"
    );
}

#[test]
fn witnessed_environment_replays_a_proc_macro_graph_exactly() {
    let fixture = WitnessFixture::new();

    // Establish a recorded state whose recipe carries the complete
    // witnessed environment. The process observer is best-effort, so allow
    // several attempts; every attempt is an ordinary captured Cargo check.
    let mut recipe_established = false;
    let mut last_errors = String::new();
    for value in 2..=9 {
        fixture.write_source(value);
        let output = fixture.check_direct();
        assert_success(&output);
        last_errors = stderr(&output);
        if fixture
            .state_directory()
            .join("check/compiler-recipe")
            .is_file()
        {
            recipe_established = true;
            break;
        }
    }
    assert!(
        recipe_established,
        "no witnessed compiler recipe was recorded for the proc-macro graph:\n{last_errors}"
    );

    // The per-toolchain witness must exist for this launch context.
    let witness_root = std::env::temp_dir().join("cinder/state/env-witness");
    let witnessed = fs::read_dir(&witness_root)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.path().join("witness").is_file())
        })
        .unwrap_or(false);
    assert!(
        witnessed,
        "no environment witness was generated under {}:\n{last_errors}",
        witness_root.display()
    );

    // A later source change replays the recipe directly — the lifted
    // proc-macro block — and Cargo then agrees byte-for-byte.
    fixture.write_source(77);
    let replayed = fixture.check_direct();
    assert_success(&replayed);
    let errors = stderr(&replayed);
    assert!(
        errors.contains("Cinder replayed Cargo's validated compiler recipe"),
        "the proc-macro graph did not replay directly:\n{errors}"
    );
    assert!(
        !errors.contains("Checking env-witness-fixture"),
        "the replay still invoked Cargo:\n{errors}"
    );

    let rmeta = fixture.library_rmeta();
    let replayed_bytes = fs::read(&rmeta).unwrap();
    let cargo = Command::new("cargo")
        .current_dir(&fixture.root)
        .args(["check", "--lib"])
        .env("CARGO_TARGET_DIR", fixture.root.join("target"))
        .env_remove("CINDER_EXPERIMENTAL_DIRECT_CHECK")
        .output()
        .unwrap();
    assert_success(&cargo);
    assert_eq!(
        fs::read(&rmeta).unwrap(),
        replayed_bytes,
        "Cargo disagreed with the replayed proc-macro artifact"
    );
}
