#![cfg(target_os = "macos")]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn patches_unique_string_data_and_falls_back_for_context_changes() {
    let fixture = Fixture::new();
    fixture.write_format_source("one");

    let initial = fixture.run("alpha");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "alpha value cinder-one");

    fixture.write_format_source("two");
    let patched = fixture.run("alpha");
    assert_success(&patched);
    assert_eq!(stdout(&patched), "alpha value cinder-two");
    assert!(
        stderr(&patched).contains("Cinder patched"),
        "expected format patch marker, got:\n{}",
        stderr(&patched)
    );

    let cargo_reference = fixture.cargo_run("alpha");
    assert_success(&cargo_reference);
    assert_eq!(stdout(&cargo_reference), stdout(&patched));

    fixture.write_ordinary_source("one");
    let ordinary_baseline = fixture.run("alpha");
    assert_success(&ordinary_baseline);
    assert_eq!(stdout(&ordinary_baseline), "alpha ordinary-one");
    assert!(!stderr(&ordinary_baseline).contains("Cinder patched"));

    fixture.write_ordinary_source("two");
    let ordinary_patched = fixture.run("alpha");
    assert_success(&ordinary_patched);
    assert_eq!(stdout(&ordinary_patched), "alpha ordinary-two");
    assert!(
        stderr(&ordinary_patched).contains("Cinder patched"),
        "expected ordinary patch marker, got:\n{}",
        stderr(&ordinary_patched)
    );

    let cargo_reference = fixture.cargo_run("alpha");
    assert_success(&cargo_reference);
    assert_eq!(stdout(&cargo_reference), stdout(&ordinary_patched));

    let context_changed = fixture.run("bravo");
    assert_success(&context_changed);
    assert_eq!(stdout(&context_changed), "bravo ordinary-two");
    assert!(!stderr(&context_changed).contains("Cinder reusing"));
    assert!(!stderr(&context_changed).contains("Cinder patched"));

    fixture.write_non_format_source();
    let unsupported = fixture.run("bravo");
    assert_success(&unsupported);
    assert_eq!(stdout(&unsupported), "bravo ordinary-two 2");
    assert!(!stderr(&unsupported).contains("Cinder patched"));
}

#[test]
fn ambiguous_string_data_falls_back_to_cargo() {
    let fixture = Fixture::new();
    fixture.write_ambiguous_source("one");

    let initial = fixture.run("alpha");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "ordinary-one ordinary-one");

    fixture.write_ambiguous_source("two");
    let fallback = fixture.run("alpha");
    assert_success(&fallback);
    assert_eq!(stdout(&fallback), "ordinary-two ordinary-one");
    assert!(!stderr(&fallback).contains("Cinder patched"));
}

#[test]
fn patches_a_selected_package_from_the_workspace_root() {
    let fixture = Fixture::new_workspace();
    fixture.write_ordinary_source("one");

    let initial = fixture.run("alpha");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "alpha ordinary-one");

    fixture.write_ordinary_source("two");
    let patched = fixture.run("alpha");
    assert_success(&patched);
    assert_eq!(stdout(&patched), "alpha ordinary-two");
    assert!(
        stderr(&patched).contains("Cinder patched app/src/main.rs"),
        "expected workspace patch marker, got:\n{}",
        stderr(&patched)
    );
}

#[test]
fn patches_a_binary_build_and_keeps_cargo_freshness_correct() {
    let fixture = Fixture::new();
    fixture.write_ordinary_source("one");

    let initial = fixture.build();
    assert_success(&initial);
    assert_eq!(fixture.built_stdout(), "build ordinary-one");

    let unchanged = fixture.build();
    assert_success(&unchanged);
    assert!(
        stderr(&unchanged).contains("Cinder reused"),
        "expected no-change build reuse, got:\n{}",
        stderr(&unchanged)
    );

    fixture.write_ordinary_source("two");
    let patched = fixture.build();
    assert_success(&patched);
    assert!(
        stderr(&patched).contains("Cinder patched src/main.rs"),
        "expected build patch marker, got:\n{}",
        stderr(&patched)
    );
    assert_eq!(fixture.built_stdout(), "build ordinary-two");

    let cargo_reference = fixture.cargo_build();
    assert_success(&cargo_reference);
    assert!(
        stderr(&cargo_reference).contains("Compiling cinder-fast-run-fixture"),
        "Cargo incorrectly considered Cinder's patched artifact fresh:\n{}",
        stderr(&cargo_reference)
    );
    assert_eq!(fixture.built_stdout(), "build ordinary-two");
}

#[test]
fn build_script_inputs_invalidate_the_binary_patch_path() {
    let fixture = Fixture::new();
    fixture.enable_build_script();
    fixture.write_ordinary_source("one");
    assert_success(&fixture.build());

    fixture.write_ordinary_source("two");
    let patched = fixture.build();
    assert_success(&patched);
    assert!(stderr(&patched).contains("Cinder patched src/main.rs"));

    fs::write(fixture.root.join("build-input.txt"), "changed\n").unwrap();
    fixture.write_ordinary_source("six");
    let invalidated = fixture.build();
    assert_success(&invalidated);
    assert!(
        !stderr(&invalidated).contains("Cinder patched"),
        "a changed build-script input incorrectly used the patch path:\n{}",
        stderr(&invalidated)
    );
    assert!(stderr(&invalidated).contains("Compiling cinder-fast-run-fixture"));
    assert_eq!(fixture.built_stdout(), "build ordinary-six");
}

#[test]
fn build_script_inputs_invalidate_the_run_patch_path() {
    let fixture = Fixture::new();
    fixture.enable_build_script();
    fixture.write_ordinary_source("one");
    assert_success(&fixture.run("run"));

    fixture.write_ordinary_source("two");
    let patched = fixture.run("run");
    assert_success(&patched);
    assert!(stderr(&patched).contains("Cinder patched src/main.rs"));

    fs::write(fixture.root.join("build-input.txt"), "changed\n").unwrap();
    fixture.write_ordinary_source("six");
    let invalidated = fixture.run("run");
    assert_success(&invalidated);
    assert!(
        !stderr(&invalidated).contains("Cinder patched"),
        "a changed build-script input incorrectly used the run patch path:\n{}",
        stderr(&invalidated)
    );
    assert_eq!(stdout(&invalidated), "run ordinary-six");
}

#[test]
fn run_waits_for_a_compiler_receipt_before_optimizing_build_script_packages() {
    let fixture = Fixture::new();
    fixture.enable_build_script();
    fixture.write_ordinary_source("one");
    assert_success(&fixture.cargo_run("run"));

    let cargo_fresh = fixture.run("run");
    assert_success(&cargo_fresh);
    assert!(!stderr(&cargo_fresh).contains("Cinder patched"));

    fixture.write_ordinary_source("two");
    let receipt_build = fixture.run("run");
    assert_success(&receipt_build);
    assert!(!stderr(&receipt_build).contains("Cinder patched"));

    fixture.write_ordinary_source("six");
    let patched = fixture.run("run");
    assert_success(&patched);
    assert!(stderr(&patched).contains("Cinder patched src/main.rs"));
    assert_eq!(stdout(&patched), "run ordinary-six");
}

#[test]
fn patches_a_selected_binary_build_from_a_virtual_workspace() {
    let fixture = Fixture::new_workspace();
    fixture.write_ordinary_source("one");
    assert_success(&fixture.build());

    fixture.write_ordinary_source("two");
    let patched = fixture.build();
    assert_success(&patched);
    assert!(
        stderr(&patched).contains("Cinder patched app/src/main.rs"),
        "expected workspace build patch marker, got:\n{}",
        stderr(&patched)
    );
    assert_eq!(fixture.built_stdout(), "build ordinary-two");
}

#[test]
fn build_capture_preserves_an_existing_rustc_wrapper() {
    let fixture = Fixture::new();
    fixture.write_ordinary_source("one");
    let wrapper = fixture.root.join("rustc-wrapper.sh");
    let probe = fixture.root.join("wrapper-probe");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf x >> \"$CINDER_WRAPPER_PROBE\"\nexec \"$@\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();

    let initial = fixture.build_with_wrapper(&wrapper, &probe);
    assert_success(&initial);
    assert!(
        probe.is_file(),
        "the configured rustc wrapper was not invoked"
    );

    fixture.write_ordinary_source("two");
    let patched = fixture.build_with_wrapper(&wrapper, &probe);
    assert_success(&patched);
    assert!(stderr(&patched).contains("Cinder patched src/main.rs"));
}

#[test]
fn patches_strings_linked_from_the_packages_library_target() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    assert_success(&fixture.build());
    assert_eq!(fixture.built_stdout(), "library-one");

    fixture.write_library_source("two");
    let patched = fixture.build();
    assert_success(&patched);
    assert!(
        stderr(&patched).contains("Cinder patched src/lib.rs"),
        "expected linked-library patch marker, got:\n{}",
        stderr(&patched)
    );
    assert_eq!(fixture.built_stdout(), "library-two");
}

#[test]
fn patches_run_strings_linked_from_the_packages_library_target() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    let initial = fixture.run("unused");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "library-one");

    fixture.write_library_source("two");
    let patched = fixture.run("unused");
    assert_success(&patched);
    assert!(
        stderr(&patched).contains("Cinder patched src/lib.rs"),
        "expected linked-library run patch marker, got:\n{}",
        stderr(&patched)
    );
    assert_eq!(stdout(&patched), "library-two");
}

#[test]
fn fast_run_restores_cargos_runtime_library_path() {
    let fixture = Fixture::new();
    fixture.write_runtime_environment_source("one");
    let initial = fixture.run("unused");
    assert_success(&initial);
    let initial_stdout = stdout(&initial);
    let (initial_path, initial_value) = initial_stdout.split_once('|').unwrap();
    assert!(!initial_path.is_empty());
    assert_eq!(initial_value, "ordinary-one");

    fixture.write_runtime_environment_source("two");
    let patched = fixture.run("unused");
    assert_success(&patched);
    let patched_stdout = stdout(&patched);
    let (patched_path, patched_value) = patched_stdout.split_once('|').unwrap();
    assert_eq!(patched_path, initial_path);
    assert_eq!(patched_value, "ordinary-two");
    assert!(stderr(&patched).contains("Cinder patched src/main.rs"));
}

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    package: Option<&'static str>,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cinder-fast-run-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let source = root.join("src/main.rs");
        Self {
            root,
            source,
            package: None,
        }
    }

    fn new_workspace() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cinder-workspace-fast-run-{}-{nonce}-{sequence}",
            std::process::id(),
        ));
        let package = root.join("app");
        fs::create_dir_all(package.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        fs::write(
            package.join("Cargo.toml"),
            "[package]\nname = \"cinder-workspace-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        Self {
            source: package.join("src/main.rs"),
            root,
            package: Some("cinder-workspace-fixture"),
        }
    }

    fn write_format_source(&self, value: &str) {
        fs::write(
            &self.source,
            format!(
                "fn main() {{ println!(\"{{}} {{}}\", env!(\"CINDER_FIXTURE_VALUE\"), format!(\"{{}} cinder-{value}\", \"value\")); }}\n"
            ),
        )
        .unwrap();
    }

    fn write_ordinary_source(&self, value: &str) {
        fs::write(
            &self.source,
            format!(
                "fn main() {{ println!(\"{{}} {{}}\", env!(\"CINDER_FIXTURE_VALUE\"), \"ordinary-{value}\"); }}\n"
            ),
        )
        .unwrap();
    }

    fn write_non_format_source(&self) {
        fs::write(
            &self.source,
            "fn main() { println!(\"{} {} {}\", env!(\"CINDER_FIXTURE_VALUE\"), \"ordinary-two\", 1 + 1); }\n",
        )
        .unwrap();
    }

    fn write_ambiguous_source(&self, value: &str) {
        fs::write(
            &self.source,
            format!(
                "fn main() {{ println!(\"{{}} {{}}\", \"ordinary-{value}\", \"ordinary-one\"); }}\n"
            ),
        )
        .unwrap();
    }

    fn enable_build_script(&self) {
        fs::write(
            self.root.join("build.rs"),
            "fn main() { println!(\"cargo:rerun-if-changed=build-input.txt\"); }\n",
        )
        .unwrap();
        fs::write(self.root.join("build-input.txt"), "initial\n").unwrap();
    }

    fn write_library_source(&self, value: &str) {
        fs::write(
            &self.source,
            "fn main() { println!(\"{}\", cinder_fast_run_fixture::value()); }\n",
        )
        .unwrap();
        fs::write(
            self.root.join("src/lib.rs"),
            format!("pub fn value() -> &'static str {{ \"library-{value}\" }}\n"),
        )
        .unwrap();
    }

    fn write_runtime_environment_source(&self, value: &str) {
        fs::write(
            &self.source,
            format!(
                "fn main() {{ println!(\"{{}}|ordinary-{value}\", std::env::var(\"DYLD_FALLBACK_LIBRARY_PATH\").unwrap_or_default()); }}\n"
            ),
        )
        .unwrap();
    }

    fn run(&self, context: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure(&mut command, context);
        command.output().unwrap()
    }

    fn cargo_run(&self, context: &str) -> Output {
        let mut command = Command::new("cargo");
        self.configure(&mut command, context);
        command.output().unwrap()
    }

    fn build(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.output().unwrap()
    }

    fn build_with_wrapper(&self, wrapper: &Path, probe: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command
            .env("RUSTC_WRAPPER", wrapper)
            .env("CINDER_WRAPPER_PROBE", probe)
            .output()
            .unwrap()
    }

    fn cargo_build(&self) -> Output {
        let mut command = Command::new("cargo");
        self.configure_build(&mut command);
        command.output().unwrap()
    }

    fn built_stdout(&self) -> String {
        let name = self.package.unwrap_or("cinder-fast-run-fixture");
        let output = Command::new(self.root.join("target/debug").join(name))
            .env("CINDER_FIXTURE_VALUE", "build")
            .output()
            .unwrap();
        assert_success(&output);
        stdout(&output)
    }

    fn configure(&self, command: &mut Command, context: &str) {
        command.current_dir(&self.root).args(["run", "--quiet"]);
        if let Some(package) = self.package {
            command.args(["-p", package]);
        }
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", context)
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_RUN")
            .env_remove("CINDER_RUN_CONTEXT_FILE");
    }

    fn configure_build(&self, command: &mut Command) {
        command.current_dir(&self.root).args(["build"]);
        if let Some(package) = self.package {
            command.args(["-p", package]);
        }
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "build")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_BUILD")
            .env_remove("CINDER_ARTIFACT_RECEIPT_DIRECTORY")
            .env_remove("CINDER_RUSTC_WRAPPER_MODE")
            .env_remove("CINDER_WRAPPER_ACTIVE")
            .env_remove("CINDER_NEXT_RUSTC_WRAPPER")
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
