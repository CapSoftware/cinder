#![cfg(target_os = "macos")]

use fs2::FileExt;
use std::{
    collections::hash_map::DefaultHasher,
    ffi::{OsStr, OsString},
    fs,
    hash::{Hash, Hasher},
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn direct_no_change_runs_keep_reusing_the_validated_immutable_artifact() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);

    let initial = fixture.run("direct");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "15");

    let restored = fixture.run("direct");
    assert_success(&restored);
    assert_eq!(stdout(&restored), "15");
    assert!(stderr(&restored).contains("Cinder restored"));
    assert!(
        !fixture
            .state_directory()
            .join("run/duplicate-ready")
            .exists()
    );

    for _ in 0..2 {
        let reused = fixture.run("direct");
        assert_success(&reused);
        assert_eq!(stdout(&reused), "15");
        assert!(
            stderr(&reused).contains("Cinder reusing"),
            "direct no-change run did not reuse its immutable artifact:\n{}",
            stderr(&reused)
        );
        assert!(!stderr(&reused).contains("Cinder restored"));
    }
}

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
fn restores_a_previous_structural_build_without_invoking_cargo() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    let first = fixture.build();
    assert_success(&first);
    assert_eq!(fixture.built_stdout(), "15");

    fixture.write_structural_source(3);
    let second = fixture.build();
    assert_success(&second);
    assert_eq!(fixture.built_stdout(), "22");
    assert!(!stderr(&second).contains("Cinder restored"));

    fixture.write_structural_source(2);
    let restored = fixture.build();
    assert_success(&restored);
    assert_eq!(fixture.built_stdout(), "15");
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "expected revision-history hit, got:\n{}",
        stderr(&restored)
    );
    assert!(!stderr(&restored).contains("Compiling cinder-fast-run-fixture"));

    fixture.write_structural_source(3);
    let restored_again = fixture.build();
    assert_success(&restored_again);
    assert_eq!(fixture.built_stdout(), "22");
    assert!(stderr(&restored_again).contains("Cinder restored a validated previous build"));

    fixture.write_structural_source(2);
    let restored_third = fixture.build();
    assert_success(&restored_third);
    assert_eq!(fixture.built_stdout(), "15");
    assert!(stderr(&restored_third).contains("Cinder restored a validated previous build"));

    let cargo_reference = fixture.cargo_build();
    assert_success(&cargo_reference);
    assert!(
        stderr(&cargo_reference).contains("Compiling cinder-fast-run-fixture"),
        "Cargo incorrectly considered a restored artifact fresh:\n{}",
        stderr(&cargo_reference)
    );
    assert_eq!(fixture.built_stdout(), "15");
}

#[test]
fn cargo_build_alias_restores_a_previous_revision() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.build_alias());
    fixture.write_structural_source(3);
    assert_success(&fixture.build_alias());
    fixture.write_structural_source(2);

    let restored = fixture.build_alias();
    assert_success(&restored);
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "cargo b did not use build history:\n{}",
        stderr(&restored)
    );
    assert_eq!(fixture.built_stdout(), "15");
}

#[test]
fn restores_a_previous_static_library_build_without_invoking_cargo() {
    let fixture = Fixture::new_static_library();
    fixture.write_static_library_source(2);
    assert_success(&fixture.build());
    let first = fs::read(fixture.static_library_artifact()).unwrap();

    fixture.write_static_library_source(3);
    assert_success(&fixture.build());
    let second = fs::read(fixture.static_library_artifact()).unwrap();
    assert_ne!(first, second);

    fixture.write_static_library_source(2);
    let restored = fixture.build();
    assert_success(&restored);
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "static library revision did not use Cinder history:\n{}",
        stderr(&restored)
    );
    assert_eq!(fs::read(fixture.static_library_artifact()).unwrap(), first);

    let cargo_reference = fixture.cargo_build();
    assert_success(&cargo_reference);
    assert!(stderr(&cargo_reference).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn multiple_library_crate_types_stay_on_cargo() {
    let fixture = Fixture::new_multi_library();
    fixture.write_static_library_source(2);
    assert_success(&fixture.build());

    fixture.write_static_library_source(3);
    assert_success(&fixture.build());

    fixture.write_static_library_source(2);
    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert!(
        stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"),
        "a multi-crate-type build restored only one artifact:\n{}",
        stderr(&rebuilt)
    );
    assert!(!stderr(&rebuilt).contains("Cinder restored"));
}

#[test]
fn restores_a_selected_library_build_without_invoking_cargo() {
    let fixture = Fixture::new_static_library();
    fixture.write_static_library_source(2);
    assert_success(&fixture.build_selected_lib());
    let first = fs::read(fixture.static_library_artifact()).unwrap();

    fixture.write_static_library_source(3);
    assert_success(&fixture.build_selected_lib());
    assert_ne!(fs::read(fixture.static_library_artifact()).unwrap(), first);

    fixture.write_static_library_source(2);
    let restored = fixture.build_selected_lib();
    assert_success(&restored);
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "selected library revision did not use Cinder history:\n{}",
        stderr(&restored)
    );
    assert_eq!(fs::read(fixture.static_library_artifact()).unwrap(), first);
}

#[test]
fn restores_a_selected_example_build_without_invoking_cargo() {
    let fixture = Fixture::new_example();
    fixture.write_structural_source(2);
    assert_success(&fixture.build_selected_example());
    assert_eq!(fixture.built_example_stdout(), "15");

    fixture.write_structural_source(3);
    assert_success(&fixture.build_selected_example());
    assert_eq!(fixture.built_example_stdout(), "22");

    fixture.write_structural_source(2);
    let restored = fixture.build_selected_example();
    assert_success(&restored);
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "selected example revision did not use Cinder history:\n{}",
        stderr(&restored)
    );
    assert_eq!(fixture.built_example_stdout(), "15");
}

#[test]
fn compiler_observable_shell_environment_invalidates_history() {
    let fixture = Fixture::new();
    fixture.write_shell_environment_source();

    assert_success(&fixture.build_with_environment("SHLVL", "11"));
    assert_eq!(fixture.built_stdout(), "11");

    let changed = fixture.build_with_environment("SHLVL", "33");
    assert_success(&changed);
    assert!(
        stderr(&changed).contains("Compiling cinder-fast-run-fixture"),
        "a linked library run environment dependency was not invalidated:\n{}",
        stderr(&changed)
    );
    assert_eq!(fixture.built_stdout(), "33");

    let restored = fixture.build_with_environment("SHLVL", "11");
    assert_success(&restored);
    assert!(stderr(&restored).contains("Cinder restored"));
    assert_eq!(fixture.built_stdout(), "11");
}

#[test]
fn unobserved_shell_underscore_does_not_eject_a_hot_build_to_cargo() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);

    assert_success(&fixture.build_with_environment("_", "first"));
    assert_eq!(fixture.built_stdout(), "15");

    let reused = fixture.build_with_environment("_", "second");
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused"),
        "an unobserved shell underscore forced a Cargo fallback:\n{}",
        stderr(&reused)
    );
    assert!(!stderr(&reused).contains("Compiling cinder-fast-run-fixture"));
    assert_eq!(fixture.built_stdout(), "15");
}

#[test]
fn unrelated_observing_target_does_not_poison_a_hot_build() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    fixture.write_unrelated_underscore_observer();

    assert_success(&fixture.cargo_build_named_bin_with_environment("observer", "_", "observer"));
    assert_success(&fixture.build_selected_bin_with_environment("_", "first"));

    let reused = fixture.build_selected_bin_with_environment("_", "second");
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused"),
        "an unrelated target's environment dependency poisoned reuse:\n{}",
        stderr(&reused)
    );
    assert_eq!(fixture.built_stdout(), "15");
}

#[test]
fn compiler_observed_shell_underscore_invalidates_and_restores_history() {
    let fixture = Fixture::new();
    fixture.write_underscore_environment_source();

    assert_success(&fixture.build_with_environment("_", "11"));
    assert_eq!(fixture.built_stdout(), "11");

    let changed = fixture.build_with_environment("_", "33");
    assert_success(&changed);
    assert!(stderr(&changed).contains("Compiling cinder-fast-run-fixture"));
    assert_eq!(fixture.built_stdout(), "33");

    let restored = fixture.build_with_environment("_", "11");
    assert_success(&restored);
    assert!(stderr(&restored).contains("Cinder restored"));
    assert_eq!(fixture.built_stdout(), "11");
}

#[test]
fn linked_library_shell_underscore_invalidates_selected_binary_history() {
    let fixture = Fixture::new();
    fixture.write_linked_underscore_environment_source();

    assert_success(&fixture.build_selected_bin_with_environment("_", "11"));
    assert_eq!(fixture.built_stdout(), "11");

    let changed = fixture.build_selected_bin_with_environment("_", "33");
    assert_success(&changed);
    assert!(
        stderr(&changed).contains("Compiling cinder-fast-run-fixture"),
        "a linked library environment dependency was not invalidated:\n{}",
        stderr(&changed)
    );
    assert_eq!(fixture.built_stdout(), "33");

    let restored = fixture.build_selected_bin_with_environment("_", "11");
    assert_success(&restored);
    assert!(stderr(&restored).contains("Cinder restored"));
    assert_eq!(fixture.built_stdout(), "11");
}

#[test]
fn linked_library_shell_underscore_invalidates_run_history() {
    let fixture = Fixture::new();
    fixture.write_linked_underscore_environment_source();

    let initial = fixture.run_with_environment("unused", "_", "44");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "44");

    let changed = fixture.run_with_environment("unused", "_", "55");
    assert_success(&changed);
    assert!(
        !stderr(&changed).contains("Cinder reused")
            && !stderr(&changed).contains("Cinder restored")
            && !stderr(&changed).contains("Cinder patched"),
        "a linked library run environment dependency incorrectly used Cinder:\n{}",
        stderr(&changed)
    );
    assert_eq!(stdout(&changed), "55");

    let restored = fixture.run_with_environment("unused", "_", "44");
    assert_success(&restored);
    assert!(stderr(&restored).contains("Cinder restored"));
    assert_eq!(stdout(&restored), "44");
}

#[test]
fn workspace_dependency_shell_underscore_invalidates_selected_binary_history() {
    let fixture = Fixture::new_workspace_dependency();

    assert_success(&fixture.build_selected_bin_with_environment("_", "71"));
    assert_eq!(fixture.built_stdout(), "71");

    let changed = fixture.build_selected_bin_with_environment("_", "72");
    assert_success(&changed);
    assert!(stderr(&changed).contains("Compiling cinder-workspace-dependency"));
    assert_eq!(fixture.built_stdout(), "72");

    let restored = fixture.build_selected_bin_with_environment("_", "71");
    assert_success(&restored);
    assert!(stderr(&restored).contains("Cinder restored"));
    assert_eq!(fixture.built_stdout(), "71");
}

#[test]
fn build_script_shell_underscore_invalidates_and_restores_history() {
    let fixture = Fixture::new();
    fixture.enable_environment_build_script();

    let initial = fixture.build_with_environment("_", "81");
    assert_success(&initial);
    assert!(
        !stderr(&initial).contains("could not record compiler artifact"),
        "a build-script compiler unit was treated as a public binary:\n{}",
        stderr(&initial)
    );
    assert_eq!(fixture.built_stdout(), "81");

    let changed = fixture.build_with_environment("_", "82");
    assert_success(&changed);
    assert!(stderr(&changed).contains("Compiling cinder-fast-run-fixture"));
    assert_eq!(fixture.built_stdout(), "82");

    let restored = fixture.build_with_environment("_", "81");
    assert_success(&restored);
    assert!(stderr(&restored).contains("Cinder restored"));
    assert_eq!(fixture.built_stdout(), "81");
}

#[test]
fn build_script_shell_underscore_invalidates_and_restores_run_history() {
    let fixture = Fixture::new();
    fixture.enable_environment_build_script();

    let initial = fixture.run_with_environment("unused", "_", "84");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "84");

    let changed = fixture.run_with_environment("unused", "_", "85");
    assert_success(&changed);
    assert!(
        !stderr(&changed).contains("Cinder reused")
            && !stderr(&changed).contains("Cinder restored")
            && !stderr(&changed).contains("Cinder patched"),
        "a build-script runtime environment dependency incorrectly used Cinder:\n{}",
        stderr(&changed)
    );
    assert_eq!(stdout(&changed), "85");

    let restored = fixture.run_with_environment("unused", "_", "84");
    assert_success(&restored);
    assert!(stderr(&restored).contains("Cinder restored"));
    assert_eq!(stdout(&restored), "84");
}

#[test]
fn restores_a_previous_structural_run_without_invoking_cargo() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    let first = fixture.run("unused");
    assert_success(&first);
    assert_eq!(stdout(&first), "15");

    fixture.write_structural_source(3);
    let second = fixture.run("unused");
    assert_success(&second);
    assert_eq!(stdout(&second), "22");
    assert!(!stderr(&second).contains("Cinder restored"));

    fixture.write_structural_source(2);
    let restored = fixture.run("unused");
    assert_success(&restored);
    assert_eq!(stdout(&restored), "15");
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "expected revision-history hit, got:\n{}",
        stderr(&restored)
    );
    assert!(!stderr(&restored).contains("Compiling cinder-fast-run-fixture"));

    fixture.write_structural_source(3);
    let restored_again = fixture.run("unused");
    assert_success(&restored_again);
    assert_eq!(stdout(&restored_again), "22");
    assert!(stderr(&restored_again).contains("Cinder restored a validated previous build"));
    let restored_artifacts = fs::read_dir(fixture.root.join("target/debug"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".cinder-fast-")
        })
        .count();
    assert!(
        restored_artifacts >= 2,
        "different revisions shared one mutable run artifact"
    );
}

#[test]
fn cargo_run_alias_restores_a_previous_revision() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    let initial = fixture.run_alias();
    assert_success(&initial);
    assert_eq!(stdout(&initial), "15");
    fixture.write_structural_source(3);
    assert_success(&fixture.run_alias());
    fixture.write_structural_source(2);

    let restored = fixture.run_alias();
    assert_success(&restored);
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "cargo r did not use run history:\n{}",
        stderr(&restored)
    );
    assert_eq!(stdout(&restored), "15");
}

#[test]
fn fast_build_waits_for_cargos_target_lock() {
    let fixture = Fixture::new();
    fixture.write_ordinary_source("one");
    assert_success(&fixture.build());
    fixture.write_ordinary_source("two");

    let lock_path = fixture.root.join("target/debug/.cargo-lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
    fixture.configure_build(&mut command);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(250));
    assert!(
        child.try_wait().unwrap().is_none(),
        "fast build ignored Cargo's target-directory lock"
    );
    FileExt::unlock(&lock).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_success(&output);
    assert_eq!(fixture.built_stdout(), "build ordinary-two");
}

#[test]
fn historical_build_waits_for_its_own_target_lock() {
    let fixture = Fixture::new();
    let target_a = fixture.root.join("target-a");
    let target_b = fixture.root.join("target-b");

    fixture.write_structural_source(2);
    assert_success(&fixture.build_in_target(&target_b));
    fixture.write_structural_source(3);
    assert_success(&fixture.build_in_target(&target_a));
    fixture.write_structural_source(2);

    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(target_b.join("debug/.cargo-lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
    fixture.configure_build_in_target(&mut command, &target_b);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(250));
    assert!(
        child.try_wait().unwrap().is_none(),
        "history restore locked a different Cargo target directory"
    );
    FileExt::unlock(&lock).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_success(&output);
    assert!(stderr(&output).contains("Cinder restored a validated previous build"));
    assert_eq!(fixture.built_stdout_in_target(&target_b), "15");
}

#[test]
fn cargo_clean_invalidates_previous_structural_builds() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.build());
    fixture.write_structural_source(3);
    assert_success(&fixture.build());
    fixture.write_structural_source(2);
    assert_success(&fixture.cargo_clean());

    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert!(
        !stderr(&rebuilt).contains("Cinder restored"),
        "cargo clean was bypassed by revision history:\n{}",
        stderr(&rebuilt)
    );
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
    assert_eq!(fixture.built_stdout(), "15");
    assert!(
        fs::read_dir(fixture.root.join("target/debug/deps"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("cinder_fast_run_fixture-")),
        "Cargo's hashed package outputs were not rebuilt"
    );
}

#[test]
fn cinder_clean_removes_project_state_and_fast_run_artifacts() {
    let fixture = Fixture::new();
    fixture.write_ordinary_source("one");
    assert_success(&fixture.run("initial"));
    fixture.write_ordinary_source("two");
    let patched = fixture.run("initial");
    assert_success(&patched);
    assert!(stderr(&patched).contains("Cinder patched"));
    assert!(fixture.state_directory().is_dir());
    assert!(
        fs::read_dir(fixture.root.join("target/debug"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".cinder-fast-"))
    );

    let cleaned = fixture.cinder_clean();
    assert_success(&cleaned);
    assert!(!fixture.state_directory().exists());
    assert!(
        fs::read_dir(fixture.root.join("target/debug"))
            .map(|entries| entries.filter_map(Result::ok).all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".cinder-fast-")))
            .unwrap_or(true),
        "cinder clean left project-owned fast-run artifacts"
    );
}

#[test]
fn global_options_before_clean_still_remove_cinder_state() {
    let fixture = Fixture::new();
    fixture.write_ordinary_source("one");
    assert_success(&fixture.run("initial"));
    assert!(fixture.state_directory().is_dir());

    let cleaned = fixture.cinder_clean_with_global_options();
    assert_success(&cleaned);
    assert!(!fixture.state_directory().exists());
}

#[test]
fn global_change_directory_clean_removes_effective_project_state() {
    let current = Fixture::new();
    current.write_ordinary_source("current");
    assert_success(&current.run("initial"));
    let selected = Fixture::new();
    selected.write_ordinary_source("selected");
    assert_success(&selected.run("initial"));
    assert!(current.state_directory().is_dir());
    assert!(selected.state_directory().is_dir());

    let fake_cargo = current.root.join("fake-cargo");
    fs::write(&fake_cargo, "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_cargo, permissions).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cinder"))
        .current_dir(&current.root)
        .args([
            OsString::from("-C"),
            selected.root.as_os_str().to_owned(),
            OsString::from("clean"),
        ])
        .env("CINDER_REAL_CARGO", &fake_cargo)
        .output()
        .unwrap();

    assert_success(&output);
    assert!(current.state_directory().is_dir());
    assert!(!selected.state_directory().exists());
}

#[test]
fn reuses_an_unchanged_selected_binary_check() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    let initial = fixture.check_selected_bin();
    assert_success(&initial);
    assert!(stderr(&initial).contains("Checking cinder-fast-run-fixture"));

    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check"),
        "unchanged check did not use Cinder state:\n{}",
        stderr(&reused)
    );
}

#[test]
fn cargo_check_alias_reuses_an_unchanged_selected_binary() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.check_selected_bin_alias());

    let reused = fixture.check_selected_bin_alias();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check"),
        "cargo c did not reuse validated check state:\n{}",
        stderr(&reused)
    );
}

#[test]
fn changed_sources_and_missing_metadata_invalidate_fast_check() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.check_selected_bin());
    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    fixture.write_structural_source(3);
    let changed = fixture.check_selected_bin();
    assert_success(&changed);
    assert!(!stderr(&changed).contains("Cinder reused"));
    assert!(stderr(&changed).contains("Checking cinder-fast-run-fixture"));

    let artifact = PathBuf::from(OsString::from_vec(
        fs::read(fixture.state_directory().join("check/artifact")).unwrap(),
    ));
    fs::remove_file(&artifact).unwrap();
    let missing = fixture.check_selected_bin();
    assert_success(&missing);
    assert!(!stderr(&missing).contains("Cinder reused"));
    assert!(stderr(&missing).contains("Checking cinder-fast-run-fixture"));
}

#[test]
fn missing_linked_unit_fingerprint_invalidates_fast_check() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    assert_success(&fixture.check_selected_bin());
    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    let artifact = PathBuf::from(OsString::from_vec(
        fs::read(fixture.state_directory().join("check/artifact")).unwrap(),
    ));
    let selected_hash = artifact
        .file_stem()
        .and_then(OsStr::to_str)
        .and_then(|name| name.rsplit_once('-').map(|(_, hash)| hash))
        .unwrap();
    let linked_fingerprint = fs::read_dir(fixture.root.join("target/debug/.fingerprint"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    name.starts_with("cinder-fast-run-fixture-") && !name.ends_with(selected_hash)
                })
        })
        .expect("missing linked library fingerprint");
    fs::rename(
        &linked_fingerprint,
        fixture.root.join("held-linked-fingerprint"),
    )
    .unwrap();

    let invalidated = fixture.check_selected_bin();
    assert_success(&invalidated);
    assert!(
        !stderr(&invalidated).contains("Cinder reused"),
        "a missing linked-unit fingerprint reused stale check state:\n{}",
        stderr(&invalidated)
    );
    assert!(stderr(&invalidated).contains("Checking cinder-fast-run-fixture"));
}

#[test]
fn newly_added_build_script_invalidates_fast_check() {
    let fixture = Fixture::new();
    fixture.write_build_script_probe_source();
    assert_success(&fixture.check_selected_bin());
    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    fs::write(
        fixture.root.join("build.rs"),
        "fn main() { println!(\"cargo:rustc-env=CINDER_BUILD_SCRIPT_PROBE=present\"); }\n",
    )
    .unwrap();
    let invalidated = fixture.check_selected_bin();
    assert_success(&invalidated);
    assert!(
        !stderr(&invalidated).contains("Cinder reused"),
        "a newly added build.rs reused stale check state:\n{}",
        stderr(&invalidated)
    );
    assert!(
        stderr(&invalidated).contains("Checking cinder-fast-run-fixture")
            || stderr(&invalidated).contains("Compiling cinder-fast-run-fixture"),
        "Cargo did not refresh the package after build.rs was added:\n{}",
        stderr(&invalidated)
    );
}

#[test]
fn newly_added_cargo_home_config_invalidates_fast_check() {
    let fixture = Fixture::new();
    let cargo_home = fixture.root.join("cargo-home");
    fs::create_dir_all(&cargo_home).unwrap();
    fixture.write_cfg_probe_source();
    assert_success(&fixture.check_selected_bin_with_cargo_home(&cargo_home));
    let reused = fixture.check_selected_bin_with_cargo_home(&cargo_home);
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    fs::write(
        cargo_home.join("config.toml"),
        "[build]\nrustflags = [\"--cfg\", \"cinder_probe\"]\n",
    )
    .unwrap();
    let invalidated = fixture.check_selected_bin_with_cargo_home(&cargo_home);
    assert_success(&invalidated);
    assert!(
        !stderr(&invalidated).contains("Cinder reused"),
        "new Cargo-home configuration reused stale check state:\n{}",
        stderr(&invalidated)
    );
    assert!(stderr(&invalidated).contains("Checking cinder-fast-run-fixture"));
}

#[test]
fn reuses_an_unchanged_named_integration_check() {
    let fixture = Fixture::new();
    fixture.write_integration_test_source("one");
    let initial = fixture.check_selected_integration();
    assert_success(&initial);
    assert!(stderr(&initial).contains("Checking cinder-fast-run-fixture"));

    let reused = fixture.check_selected_integration();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check"),
        "unchanged integration check did not use Cinder state:\n{}",
        stderr(&reused)
    );
}

#[test]
fn multi_unit_check_stays_on_cargo() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    assert_success(&fixture.check_default());
    assert!(!fixture.state_directory().join("check").exists());

    let fresh = fixture.check_default();
    assert_success(&fresh);
    assert!(!stderr(&fresh).contains("Cinder reused"));
    assert!(!fixture.state_directory().join("check").exists());
}

#[test]
fn explicitly_selected_multi_unit_check_stays_on_cargo() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    let checked = fixture.check_selected_bin_and_lib();
    assert_success(&checked);
    assert!(!fixture.state_directory().join("check").exists());

    let repeated = fixture.check_selected_bin_and_lib();
    assert_success(&repeated);
    assert!(!stderr(&repeated).contains("Cinder reused"));
    assert!(!fixture.state_directory().join("check").exists());
}

#[test]
fn combined_library_and_integration_checks_stay_on_cargo() {
    let fixture = Fixture::new();
    fixture.write_integration_test_source("one");
    assert_success(&fixture.check_selected_lib_and_integration());
    assert!(!fixture.state_directory().join("check").exists());

    let repeated = fixture.check_selected_lib_and_integration();
    assert_success(&repeated);
    assert!(!stderr(&repeated).contains("Cinder reused"));
    assert!(!fixture.state_directory().join("check").exists());
}

#[test]
fn background_recorders_prepare_new_selected_check_and_test_targets() {
    let test_fixture = Fixture::new();
    test_fixture.write_library_source("one");
    assert_success(&test_fixture.test_selected_lib_no_run_asynchronously());
    test_fixture.wait_for_state("test");
    let reused_test = test_fixture.test_selected_lib_no_run_asynchronously();
    assert_success(&reused_test);
    assert!(
        stderr(&reused_test).contains("Cinder reused the validated test build"),
        "background test recorder did not prepare reusable state:\n{}",
        stderr(&reused_test)
    );

    let check_fixture = Fixture::new();
    check_fixture.write_integration_test_source("one");
    assert_success(&check_fixture.check_selected_integration_asynchronously());
    check_fixture.wait_for_state("check");
    let reused_check = check_fixture.check_selected_integration_asynchronously();
    assert_success(&reused_check);
    assert!(
        stderr(&reused_check).contains("Cinder reused the validated check"),
        "background check recorder did not prepare reusable state:\n{}",
        stderr(&reused_check)
    );
}

#[test]
fn reuses_an_unchanged_selected_test_build() {
    let fixture = Fixture::new_example();
    fixture.write_structural_source(2);
    let initial = fixture.test_selected_example_no_run("test");
    assert_success(&initial);
    assert!(stderr(&initial).contains("Compiling cinder-fast-run-fixture"));

    let reused = fixture.test_selected_example_no_run("test");
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated test build"),
        "unchanged test --no-run did not use Cinder state:\n{}",
        stderr(&reused)
    );
}

#[test]
fn reuses_an_unchanged_selected_library_test_build() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    let initial = fixture.test_selected_lib_no_run();
    assert_success(&initial);
    assert!(stderr(&initial).contains("Compiling cinder-fast-run-fixture"));

    let reused = fixture.test_selected_lib_no_run();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated test build"),
        "unchanged library test build did not use Cinder state:\n{}",
        stderr(&reused)
    );
}

#[test]
fn reuses_an_unchanged_named_integration_test_build() {
    let fixture = Fixture::new();
    fixture.write_integration_test_source("one");
    let initial = fixture.test_selected_integration_no_run();
    assert_success(&initial);
    assert!(stderr(&initial).contains("Compiling cinder-fast-run-fixture"));

    let reused = fixture.test_selected_integration_no_run();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated test build"),
        "unchanged integration test build did not use Cinder state:\n{}",
        stderr(&reused)
    );

    fixture.write_integration_test_source("two");
    let rebuilt = fixture.test_selected_integration_no_run();
    assert_success(&rebuilt);
    assert!(
        !stderr(&rebuilt).contains("Cinder reused"),
        "changed integration test inputs reused stale state:\n{}",
        stderr(&rebuilt)
    );
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn combined_library_and_integration_test_builds_stay_on_cargo() {
    let fixture = Fixture::new();
    fixture.write_integration_test_source("one");
    assert_success(&fixture.test_selected_lib_and_integration_no_run());
    assert!(!fixture.state_directory().join("test").exists());

    let repeated = fixture.test_selected_lib_and_integration_no_run();
    assert_success(&repeated);
    assert!(!stderr(&repeated).contains("Cinder reused"));
    assert!(!fixture.state_directory().join("test").exists());
}

#[test]
fn test_alias_reuses_an_unchanged_selected_test_build() {
    let fixture = Fixture::new_example();
    fixture.write_structural_source(2);
    assert_success(&fixture.test_selected_example_no_run("t"));

    let reused = fixture.test_selected_example_no_run("t");
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated test build"),
        "default build-script test state was not reused:\n{}",
        stderr(&reused)
    );
}

#[test]
fn changed_sources_and_normal_test_execution_bypass_fast_test() {
    let fixture = Fixture::new_example();
    fixture.write_structural_source(2);
    assert_success(&fixture.test_selected_example_no_run("test"));
    assert_success(&fixture.test_selected_example_no_run("test"));

    fixture.write_structural_source(3);
    let changed = fixture.test_selected_example_no_run("test");
    assert_success(&changed);
    assert!(!stderr(&changed).contains("Cinder reused"));
    assert!(stderr(&changed).contains("Compiling cinder-fast-run-fixture"));

    let executed = fixture.test_selected_example();
    assert_success(&executed);
    assert!(!stderr(&executed).contains("Cinder reused"));
    assert!(stdout(&executed).contains("test result: ok"));
}

#[test]
fn default_build_script_package_inputs_invalidate_fast_test() {
    let fixture = Fixture::new_example();
    fixture.write_structural_source(2);
    fixture.enable_default_build_script();
    let initial = fixture.test_selected_example_no_run("test");
    assert_success(&initial);
    assert!(
        fixture.state_directory().join("test").is_dir(),
        "default build-script test state was not recorded:\n{}",
        stderr(&initial)
    );
    let reused = fixture.test_selected_example_no_run("test");
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated test build"),
        "default build-script test state was not reused:\n{}",
        stderr(&reused)
    );

    fs::write(fixture.root.join("build-asset.txt"), "changed\n").unwrap();
    let invalidated = fixture.test_selected_example_no_run("test");
    assert_success(&invalidated);
    assert!(
        !stderr(&invalidated).contains("Cinder reused"),
        "default build-script package input reused stale test state:\n{}",
        stderr(&invalidated)
    );
}

#[test]
fn multi_crate_type_test_dependencies_use_cargos_encoded_environment_data() {
    let fixture = Fixture::new_multi_crate_type_binary();
    assert_success(&fixture.test_selected_bin_no_run());
    assert_eq!(
        fs::read(fixture.state_directory().join("test/observes-underscore")).unwrap(),
        b"1"
    );

    let reused = fixture.test_selected_bin_no_run();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated test build"),
        "multi-crate-type test state was not reused:\n{}",
        stderr(&reused)
    );
}

#[test]
fn encoded_dependency_environment_changes_invalidate_fast_test() {
    let fixture = Fixture::new_multi_crate_type_binary();
    assert_success(&fixture.test_selected_bin_no_run_with_shell_value("one"));

    let changed = fixture.test_selected_bin_no_run_with_shell_value("two");
    assert_success(&changed);
    assert!(
        !stderr(&changed).contains("Cinder reused"),
        "encoded dependency environment change reused stale test state:\n{}",
        stderr(&changed)
    );
    assert!(stderr(&changed).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn harness_arguments_cannot_turn_test_execution_into_no_run() {
    let fixture = Fixture::new_harnessless_example();
    fixture.write_structural_source(2);

    for _ in 0..2 {
        let executed = fixture.test_with_harness_no_run_argument();
        assert_success(&executed);
        assert_eq!(stdout(&executed), "15");
        assert!(!stderr(&executed).contains("Cinder reused"));
    }
    assert!(!fixture.state_directory().join("test").exists());
}

#[test]
fn partial_cargo_clean_with_only_public_outputs_falls_back() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.build());
    fixture.preserve_only_public_outputs_across_clean();

    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert!(
        !stderr(&rebuilt).contains("Cinder reused")
            && !stderr(&rebuilt).contains("Cinder restored"),
        "partial Cargo outputs were treated as a complete build:\n{}",
        stderr(&rebuilt)
    );
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn missing_exact_cargo_hash_falls_back_when_an_older_hash_survives() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);

    assert_success(&fixture.build_with_rustflags("--cfg cinder_hash_one"));
    let first = fixture.active_hashed_outputs();
    assert_success(&fixture.build_with_rustflags("--cfg cinder_hash_two"));
    let second = fixture.active_hashed_outputs();
    assert_ne!(
        first.0, second.0,
        "RUSTFLAGS did not produce distinct hashes"
    );
    assert!(first.0.is_file() && first.1.is_file() && first.2.is_dir());

    let saved = fixture.root.join("saved-active-hash");
    fs::create_dir(&saved).unwrap();
    fs::rename(&second.0, saved.join("dependency.d")).unwrap();
    fs::rename(&second.1, saved.join("executable")).unwrap();
    fs::rename(&second.2, saved.join("fingerprint")).unwrap();

    let rebuilt = fixture.build_with_rustflags("--cfg cinder_hash_two");
    assert_success(&rebuilt);
    assert!(
        !stderr(&rebuilt).contains("Cinder reused")
            && !stderr(&rebuilt).contains("Cinder restored"),
        "an unrelated Cargo hash satisfied exact-output validation:\n{}",
        stderr(&rebuilt)
    );
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn restored_run_artifacts_follow_the_history_bound() {
    let fixture = Fixture::new();
    for multiplier in 1..=9 {
        fixture.write_structural_source(multiplier);
        assert_success(&fixture.run("unused"));
    }
    for multiplier in 2..=9 {
        fixture.write_structural_source(multiplier);
        let restored = fixture.run("unused");
        assert_success(&restored);
        assert!(stderr(&restored).contains("Cinder restored a validated previous build"));
    }
    assert_eq!(fixture.revision_run_artifact_count(), 8);
    assert_eq!(fixture.revision_run_artifact_receipt_count(), 8);

    for multiplier in 10..=18 {
        fixture.write_structural_source(multiplier);
        assert_success(&fixture.run("unused"));
    }
    assert_eq!(
        fixture.revision_run_artifact_count(),
        0,
        "evicted history left restored executables in the target directory"
    );
    assert_eq!(fixture.revision_run_artifact_receipt_count(), 0);
    for multiplier in 11..=18 {
        fixture.write_structural_source(multiplier);
        assert_success(&fixture.run("unused"));
    }
    assert_eq!(fixture.revision_run_artifact_count(), 8);
    assert_eq!(fixture.revision_run_artifact_receipt_count(), 8);
}

#[test]
fn newly_added_build_script_invalidates_cached_runs() {
    let fixture = Fixture::new();
    fixture.write_build_script_probe_source();
    let initial = fixture.run("unused");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "absent");

    fs::write(
        fixture.root.join("build.rs"),
        "fn main() { println!(\"cargo:rustc-env=CINDER_BUILD_SCRIPT_PROBE=present\"); }\n",
    )
    .unwrap();
    let rebuilt = fixture.run("unused");
    assert_success(&rebuilt);
    assert_eq!(stdout(&rebuilt), "present");
    assert!(
        !stderr(&rebuilt).contains("Cinder restored"),
        "a newly added build.rs reused a stale executable:\n{}",
        stderr(&rebuilt)
    );
}

#[test]
fn newly_added_cargo_home_config_invalidates_cached_runs() {
    let fixture = Fixture::new();
    let cargo_home = fixture.root.join("cargo-home");
    fs::create_dir_all(&cargo_home).unwrap();
    fixture.write_cfg_probe_source();
    let initial = fixture.run_with_cargo_home("unused", &cargo_home);
    assert_success(&initial);
    assert_eq!(stdout(&initial), "plain");

    fs::write(
        cargo_home.join("config.toml"),
        "[build]\nrustflags = [\"--cfg\", \"cinder_probe\"]\n",
    )
    .unwrap();
    let rebuilt = fixture.run_with_cargo_home("unused", &cargo_home);
    assert_success(&rebuilt);
    assert_eq!(stdout(&rebuilt), "configured");
    assert!(
        !stderr(&rebuilt).contains("Cinder restored"),
        "a new Cargo-home config reused a stale executable:\n{}",
        stderr(&rebuilt)
    );
}

#[test]
fn newly_added_auto_target_invalidates_cached_builds() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.build());
    fs::create_dir_all(fixture.root.join("src/bin")).unwrap();
    fs::write(
        fixture.root.join("src/bin/extra.rs"),
        "fn main() { println!(\"extra\"); }\n",
    )
    .unwrap();

    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert!(
        !stderr(&rebuilt).contains("Cinder reused")
            && !stderr(&rebuilt).contains("Cinder restored"),
        "a newly added auto target bypassed Cargo:\n{}",
        stderr(&rebuilt)
    );
    assert!(fixture.root.join("target/debug/extra").is_file());
}

#[test]
fn changed_compiler_identity_invalidates_revision_history() {
    let fixture = Fixture::new();
    let compiler = fixture.root.join("rustc-proxy.sh");
    fixture.write_rustc_proxy(&compiler, "one");
    fixture.write_structural_source(2);
    assert_success(&fixture.build_with_rustc(&compiler));
    fixture.write_structural_source(3);
    assert_success(&fixture.build_with_rustc(&compiler));

    fixture.write_rustc_proxy(&compiler, "two");
    fixture.write_structural_source(2);
    let rebuilt = fixture.build_with_rustc(&compiler);
    assert_success(&rebuilt);
    assert_eq!(fixture.built_stdout(), "15");
    assert!(
        !stderr(&rebuilt).contains("Cinder restored"),
        "revision history ignored a changed compiler identity:\n{}",
        stderr(&rebuilt)
    );
}

#[test]
fn changed_manifest_invalidates_previous_structural_builds() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.build());
    fixture.write_structural_source(3);
    assert_success(&fixture.build());

    fs::write(
        fixture.root.join("Cargo.toml"),
        "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[profile.dev]\nopt-level = 1\n",
    )
    .unwrap();
    fixture.write_structural_source(2);
    let invalidated = fixture.build();
    assert_success(&invalidated);
    assert!(
        !stderr(&invalidated).contains("Cinder restored"),
        "changed manifest incorrectly reused revision history:\n{}",
        stderr(&invalidated)
    );
    assert!(stderr(&invalidated).contains("Compiling cinder-fast-run-fixture"));
    assert_eq!(fixture.built_stdout(), "15");
}

#[test]
fn revision_history_prunes_least_recently_used_builds() {
    let fixture = Fixture::new();
    for multiplier in 1..=10 {
        fixture.write_structural_source(multiplier);
        assert_success(&fixture.build());
    }

    fixture.write_structural_source(3);
    let retained = fixture.build();
    assert_success(&retained);
    assert!(
        stderr(&retained).contains("Cinder restored a validated previous build"),
        "a recent revision was not retained:\n{}",
        stderr(&retained)
    );

    fixture.write_structural_source(1);
    let evicted = fixture.build();
    assert_success(&evicted);
    assert!(
        !stderr(&evicted).contains("Cinder restored"),
        "the oldest revision should have been evicted:\n{}",
        stderr(&evicted)
    );
    assert!(stderr(&evicted).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn revision_history_hashes_each_live_source_set_once() {
    let fixture = Fixture::new();
    for multiplier in 1..=8 {
        fixture.write_structural_source(multiplier);
        assert_success(&fixture.build_with_environment("CINDER_TRACE_RUN", "1"));
    }

    fixture.write_structural_source(1);
    let restored = fixture.build_with_environment("CINDER_TRACE_RUN", "1");
    assert_success(&restored);
    assert_eq!(fixture.built_stdout(), "8");
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "expected revision-history hit, got:\n{}",
        stderr(&restored)
    );
    assert!(
        stderr(&restored).contains("Cinder trace: history source-probes=1"),
        "revision lookup rehashed an identical live source set:\n{}",
        stderr(&restored)
    );
}

#[test]
fn corrupted_revision_snapshot_falls_back_to_cargo() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.build());
    fixture.write_structural_source(3);
    assert_success(&fixture.build());

    let history = fixture.state_directory().join("build-history");
    let revision = fs::read_dir(history)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|entry| {
            fs::read_to_string(entry.join("snapshot/src/main.rs"))
                .is_ok_and(|source| source.contains("input * 2 + 1"))
        })
        .expect("missing retained revision for multiplier 2");
    fs::write(
        revision.join("snapshot/src/main.rs"),
        "fn main() { println!(\"corrupted\"); }\n",
    )
    .unwrap();

    fixture.write_structural_source(2);
    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert_eq!(fixture.built_stdout(), "15");
    assert!(
        !stderr(&rebuilt).contains("Cinder restored"),
        "corrupted revision snapshot was restored:\n{}",
        stderr(&rebuilt)
    );
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn cached_artifact_identity_detects_content_changes_with_restored_mtime() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.build());
    fixture.write_structural_source(3);
    assert_success(&fixture.build());

    let history = fixture.state_directory().join("build-history");
    let revision = fs::read_dir(history)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|entry| {
            fs::read_to_string(entry.join("snapshot/src/main.rs"))
                .is_ok_and(|source| source.contains("input * 2 + 1"))
        })
        .expect("missing retained revision for multiplier 2");
    let artifact = revision.join("cached-artifact");
    let metadata = fs::metadata(&artifact).unwrap();
    let modified = metadata.modified().unwrap();
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    fs::set_permissions(&artifact, permissions).unwrap();
    let mut contents = fs::read(&artifact).unwrap();
    contents[0] ^= 0xff;
    fs::write(&artifact, contents).unwrap();
    fs::File::open(&artifact)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let mut permissions = fs::metadata(&artifact).unwrap().permissions();
    permissions.set_mode(permissions.mode() & !0o222);
    fs::set_permissions(&artifact, permissions).unwrap();

    fixture.write_structural_source(2);
    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert_eq!(fixture.built_stdout(), "15");
    assert!(
        !stderr(&rebuilt).contains("Cinder restored"),
        "modified cached artifact was restored:\n{}",
        stderr(&rebuilt)
    );
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn control_input_identity_detects_same_size_changes_with_restored_mtime() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.source,
        "#[cfg(feature = \"alpha\")]\nconst VALUE: &str = \"alpha\";\n#[cfg(feature = \"bravo\")]\nconst VALUE: &str = \"bravo\";\nfn main() { println!(\"{VALUE}\"); }\n",
    )
    .unwrap();
    let manifest = fixture.root.join("Cargo.toml");
    fs::write(
        &manifest,
        "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\ndefault = [\"alpha\"]\nalpha = []\nbravo = []\n",
    )
    .unwrap();
    let initial = fixture.build();
    assert_success(&initial);
    assert_eq!(fixture.built_stdout(), "alpha");

    let modified = fs::metadata(&manifest).unwrap().modified().unwrap();
    let contents = fs::read_to_string(&manifest)
        .unwrap()
        .replace("default = [\"alpha\"]", "default = [\"bravo\"]");
    fs::write(&manifest, contents).unwrap();
    fs::File::open(&manifest)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();

    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert_eq!(fixture.built_stdout(), "bravo");
    assert!(
        !stderr(&rebuilt).contains("Cinder reused"),
        "changed control input was incorrectly reused:\n{}",
        stderr(&rebuilt)
    );
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn restored_run_artifact_receipt_detects_content_changes_with_restored_mtime() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_eq!(stdout(&fixture.run("alpha")), "15");
    fixture.write_structural_source(3);
    assert_eq!(stdout(&fixture.run("alpha")), "22");
    fixture.write_structural_source(2);
    let first_restore = fixture.run("alpha");
    assert_success(&first_restore);
    assert_eq!(stdout(&first_restore), "15");
    assert!(
        stderr(&first_restore).contains("Cinder restored a validated previous build"),
        "expected first run revision restore:\n{}",
        stderr(&first_restore)
    );

    let artifact = PathBuf::from(
        String::from_utf8(fs::read(fixture.state_directory().join("run/artifact")).unwrap())
            .unwrap(),
    );
    let metadata = fs::metadata(&artifact).unwrap();
    let modified = metadata.modified().unwrap();
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    fs::set_permissions(&artifact, permissions).unwrap();
    let mut contents = fs::read(&artifact).unwrap();
    contents[0] ^= 0xff;
    fs::write(&artifact, contents).unwrap();
    fs::File::open(&artifact)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let mut permissions = fs::metadata(&artifact).unwrap().permissions();
    permissions.set_mode(permissions.mode() & !0o222);
    fs::set_permissions(&artifact, permissions).unwrap();

    fixture.write_structural_source(3);
    assert_eq!(stdout(&fixture.run("alpha")), "22");
    fixture.write_structural_source(2);
    let repaired = fixture.run("alpha");
    assert_success(&repaired);
    assert_eq!(stdout(&repaired), "15");
    assert!(stderr(&repaired).contains("Cinder restored a validated previous build"));
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
fn build_script_input_revisions_restore_by_content() {
    let fixture = Fixture::new();
    fixture.enable_value_build_script("initial");

    let first = fixture.build();
    assert_success(&first);
    assert_eq!(fixture.built_stdout(), "initial");

    fs::write(fixture.root.join("build-input.txt"), "changed\n").unwrap();
    let changed = fixture.build();
    assert_success(&changed);
    assert_eq!(fixture.built_stdout(), "changed");

    fs::write(fixture.root.join("build-input.txt"), "initial\n").unwrap();
    let restored = fixture.build();
    assert_success(&restored);
    assert_eq!(fixture.built_stdout(), "initial");
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "reverted build input did not restore its compiled revision:\n{}",
        stderr(&restored)
    );
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
    assert_success(&fixture.build_selected_bin());
    assert_eq!(fixture.built_stdout(), "library-one");

    fixture.write_library_source("two");
    let patched = fixture.build_selected_bin();
    assert_success(&patched);
    assert!(
        stderr(&patched).contains("Cinder patched src/lib.rs"),
        "expected linked-library patch marker, got:\n{}",
        stderr(&patched)
    );
    assert_eq!(fixture.built_stdout(), "library-two");
}

#[test]
fn unselected_multi_artifact_builds_stay_on_cargo() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    assert_success(&fixture.build());

    fixture.write_library_source("two");
    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert!(
        stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"),
        "an unselected bin-plus-lib build skipped Cargo:\n{}",
        stderr(&rebuilt)
    );
    assert!(!stderr(&rebuilt).contains("Cinder patched"));
    assert_eq!(fixture.built_stdout(), "library-two");
}

#[test]
fn explicitly_selected_multi_artifact_builds_stay_on_cargo() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    assert_success(&fixture.build_selected_bin_and_lib());
    assert!(!fixture.state_directory().join("build").exists());

    fixture.write_library_source("two");
    let rebuilt = fixture.build_selected_bin_and_lib();
    assert_success(&rebuilt);
    assert!(
        stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"),
        "an explicitly selected bin-plus-lib build skipped Cargo:\n{}",
        stderr(&rebuilt)
    );
    assert!(!stderr(&rebuilt).contains("Cinder patched"));
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

    fn new_workspace_dependency() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cinder-workspace-dependency-{}-{nonce}-{sequence}",
            std::process::id(),
        ));
        fs::create_dir_all(root.join("app/src")).unwrap();
        fs::create_dir_all(root.join("dependency/src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"dependency\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        fs::write(
            root.join("app/Cargo.toml"),
            "[package]\nname = \"cinder-workspace-app\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\ncinder-workspace-dependency = { path = \"../dependency\" }\n",
        )
        .unwrap();
        fs::write(
            root.join("app/src/main.rs"),
            "fn main() { println!(\"{}\", cinder_workspace_dependency::value()); }\n",
        )
        .unwrap();
        fs::write(
            root.join("dependency/Cargo.toml"),
            "[package]\nname = \"cinder-workspace-dependency\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            root.join("dependency/src/lib.rs"),
            "pub fn value() -> &'static str { env!(\"_\") }\n",
        )
        .unwrap();
        Self {
            source: root.join("app/src/main.rs"),
            root,
            package: Some("cinder-workspace-app"),
        }
    }

    fn new_static_library() -> Self {
        Self::new_library_with_crate_types("\"staticlib\"")
    }

    fn new_example() -> Self {
        let mut fixture = Self::new();
        let examples = fixture.root.join("examples");
        fs::create_dir_all(&examples).unwrap();
        fs::write(
            fixture.root.join("src/lib.rs"),
            "pub fn linked_value() -> i32 { 7 }\n",
        )
        .unwrap();
        fixture.source = examples.join("demo.rs");
        fixture
    }

    fn new_harnessless_example() -> Self {
        let fixture = Self::new_example();
        fs::write(
            fixture.root.join("Cargo.toml"),
            "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[[example]]\nname = \"demo\"\npath = \"examples/demo.rs\"\nharness = false\n",
        )
        .unwrap();
        fixture
    }

    fn new_multi_library() -> Self {
        Self::new_library_with_crate_types("\"rlib\", \"staticlib\"")
    }

    fn new_multi_crate_type_binary() -> Self {
        let fixture = Self::new();
        fs::write(
            fixture.root.join("Cargo.toml"),
            "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\ncrate-type = [\"rlib\", \"staticlib\"]\n",
        )
        .unwrap();
        fs::write(
            &fixture.source,
            "fn main() { println!(\"{}\", cinder_fast_run_fixture::value()); }\n",
        )
        .unwrap();
        fs::write(
            fixture.root.join("src/lib.rs"),
            "pub fn value() -> &'static str { env!(\"_\") }\n",
        )
        .unwrap();
        fixture
    }

    fn new_library_with_crate_types(crate_types: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cinder-staticlib-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\ncrate-type = [{crate_types}]\n"
            ),
        )
        .unwrap();
        Self {
            source: root.join("src/lib.rs"),
            root,
            package: None,
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

    fn write_structural_source(&self, multiplier: i32) {
        fs::write(
            &self.source,
            format!(
                "fn compute(input: i32) -> i32 {{ input * {multiplier} + 1 }}\nfn main() {{ println!(\"{{}}\", compute(7)); }}\n"
            ),
        )
        .unwrap();
    }

    fn write_static_library_source(&self, multiplier: i32) {
        fs::write(
            &self.source,
            format!(
                "#[unsafe(no_mangle)]\npub extern \"C\" fn cinder_compute(input: i32) -> i32 {{ input * {multiplier} + 1 }}\n"
            ),
        )
        .unwrap();
    }

    fn write_shell_environment_source(&self) {
        fs::write(
            &self.source,
            "fn main() { println!(\"{}\", env!(\"SHLVL\")); }\n",
        )
        .unwrap();
    }

    fn write_underscore_environment_source(&self) {
        fs::write(
            &self.source,
            "fn main() { println!(\"{}\", env!(\"_\")); }\n",
        )
        .unwrap();
    }

    fn write_linked_underscore_environment_source(&self) {
        fs::write(
            &self.source,
            "fn main() { println!(\"{}\", cinder_fast_run_fixture::value()); }\n",
        )
        .unwrap();
        fs::write(
            self.root.join("src/lib.rs"),
            "pub fn value() -> &'static str { env!(\"_\") }\n",
        )
        .unwrap();
    }

    fn write_unrelated_underscore_observer(&self) {
        fs::create_dir_all(self.root.join("src/bin")).unwrap();
        fs::write(
            self.root.join("src/bin/observer.rs"),
            "fn main() { println!(\"{}\", env!(\"_\")); }\n",
        )
        .unwrap();
    }

    fn static_library_artifact(&self) -> PathBuf {
        self.root.join("target/debug/libcinder_fast_run_fixture.a")
    }

    fn write_build_script_probe_source(&self) {
        fs::write(
            &self.source,
            "fn main() { println!(\"{}\", option_env!(\"CINDER_BUILD_SCRIPT_PROBE\").unwrap_or(\"absent\")); }\n",
        )
        .unwrap();
    }

    fn write_cfg_probe_source(&self) {
        fs::write(
            &self.source,
            "#[cfg(cinder_probe)]\nconst VALUE: &str = \"configured\";\n#[cfg(not(cinder_probe))]\nconst VALUE: &str = \"plain\";\nfn main() { println!(\"{VALUE}\"); }\n",
        )
        .unwrap();
    }

    fn write_rustc_proxy(&self, path: &Path, marker: &str) {
        fs::write(
            path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"-vV\" ]; then\n  rustc -vV\n  echo 'cinder-test-tool {marker}'\n  exit 0\nfi\nexec rustc \"$@\"\n"
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
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

    fn enable_default_build_script(&self) {
        fs::write(self.root.join("build.rs"), "fn main() {}\n").unwrap();
        fs::write(self.root.join("build-asset.txt"), "initial\n").unwrap();
    }

    fn enable_value_build_script(&self, value: &str) {
        fs::write(
            &self.source,
            "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\nfn main() { println!(\"{CINDER_BUILD_INPUT}\"); }\n",
        )
        .unwrap();
        fs::write(
            self.root.join("build.rs"),
            "fn main() { let value = std::fs::read_to_string(\"build-input.txt\").unwrap(); let output = std::path::PathBuf::from(std::env::var_os(\"OUT_DIR\").unwrap()).join(\"generated.rs\"); std::fs::write(output, format!(\"const CINDER_BUILD_INPUT: &str = {:?};\\n\", value.trim())).unwrap(); println!(\"cargo:rerun-if-changed=build-input.txt\"); }\n",
        )
        .unwrap();
        fs::write(self.root.join("build-input.txt"), format!("{value}\n")).unwrap();
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

    fn write_integration_test_source(&self, value: &str) {
        fs::write(
            self.root.join("src/lib.rs"),
            format!("pub fn value() -> &'static str {{ \"integration-{value}\" }}\n"),
        )
        .unwrap();
        fs::create_dir_all(self.root.join("tests")).unwrap();
        fs::write(
            self.root.join("tests/smoke.rs"),
            "#[test]\nfn smoke() { assert!(cinder_fast_run_fixture::value().starts_with(\"integration-\")); }\n",
        )
        .unwrap();
    }

    fn enable_environment_build_script(&self) {
        fs::write(
            &self.source,
            "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\nfn main() { println!(\"{CINDER_BUILD_ENVIRONMENT}\"); }\n",
        )
        .unwrap();
        fs::write(
            self.root.join("build.rs"),
            "fn main() { let value = std::env::var(\"_\").unwrap(); let output = std::path::PathBuf::from(std::env::var_os(\"OUT_DIR\").unwrap()).join(\"generated.rs\"); std::fs::write(output, format!(\"const CINDER_BUILD_ENVIRONMENT: &str = {:?};\\n\", value)).unwrap(); println!(\"cargo:rerun-if-env-changed=_\"); println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
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

    fn run_alias(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&self.root).args(["r", "--quiet"]);
        if let Some(package) = self.package {
            command.args(["-p", package]);
        }
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_RUN")
            .env_remove("CINDER_RUN_CONTEXT_FILE")
            .output()
            .unwrap()
    }

    fn run_with_environment(&self, context: &str, key: &str, value: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure(&mut command, context);
        command.env(key, value).output().unwrap()
    }

    fn cargo_run(&self, context: &str) -> Output {
        let mut command = Command::new("cargo");
        self.configure(&mut command, context);
        command.output().unwrap()
    }

    fn run_with_cargo_home(&self, context: &str, cargo_home: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure(&mut command, context);
        command.env("CARGO_HOME", cargo_home).output().unwrap()
    }

    fn build(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.output().unwrap()
    }

    fn build_alias(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&self.root).arg("b");
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
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER")
            .output()
            .unwrap()
    }

    fn build_selected_bin(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.args(["--bin", self.package.unwrap_or("cinder-fast-run-fixture")]);
        command.output().unwrap()
    }

    fn build_selected_bin_and_lib(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.args([
            "--bin",
            self.package.unwrap_or("cinder-fast-run-fixture"),
            "--lib",
        ]);
        command.output().unwrap()
    }

    fn build_selected_lib(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.arg("--lib").output().unwrap()
    }

    fn build_selected_example(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.args(["--example", "demo"]).output().unwrap()
    }

    fn check_selected_bin(&self) -> Output {
        self.check_selected_bin_with_command("check")
    }

    fn check_selected_bin_alias(&self) -> Output {
        self.check_selected_bin_with_command("c")
    }

    fn check_selected_bin_and_lib(&self) -> Output {
        self.check_selected_targets(
            "check",
            &[
                "--bin",
                self.package.unwrap_or("cinder-fast-run-fixture"),
                "--lib",
            ],
            None,
        )
    }

    fn check_selected_bin_with_command(&self, command_name: &str) -> Output {
        self.check_selected_targets(
            command_name,
            &["--bin", self.package.unwrap_or("cinder-fast-run-fixture")],
            None,
        )
    }

    fn check_selected_bin_with_cargo_home(&self, cargo_home: &Path) -> Output {
        self.check_selected_targets(
            "check",
            &["--bin", self.package.unwrap_or("cinder-fast-run-fixture")],
            Some(cargo_home),
        )
    }

    fn check_selected_integration(&self) -> Output {
        self.check_selected_targets("check", &["--test", "smoke"], None)
    }

    fn check_selected_integration_asynchronously(&self) -> Output {
        self.check_selected_targets_with_recording("check", &["--test", "smoke"], None, false)
    }

    fn check_selected_lib_and_integration(&self) -> Output {
        self.check_selected_targets("check", &["--lib", "--test", "smoke"], None)
    }

    fn check_selected_targets(
        &self,
        command_name: &str,
        selectors: &[&str],
        cargo_home: Option<&Path>,
    ) -> Output {
        self.check_selected_targets_with_recording(command_name, selectors, cargo_home, true)
    }

    fn check_selected_targets_with_recording(
        &self,
        command_name: &str,
        selectors: &[&str],
        cargo_home: Option<&Path>,
        synchronous: bool,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .arg(command_name)
            .args(selectors);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .env_remove("CINDER_ARTIFACT_RECEIPT_DIRECTORY")
            .env_remove("CINDER_RUSTC_WRAPPER_MODE")
            .env_remove("CINDER_WRAPPER_ACTIVE")
            .env_remove("CINDER_NEXT_RUSTC_WRAPPER")
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER");
        if synchronous {
            command.env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1");
        } else {
            command.env_remove("CINDER_SYNCHRONOUS_STATE_RECORDING");
        }
        if let Some(cargo_home) = cargo_home {
            command.env("CARGO_HOME", cargo_home);
        }
        command.output().unwrap()
    }

    fn check_default(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&self.root).arg("check");
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .env_remove("CINDER_ARTIFACT_RECEIPT_DIRECTORY")
            .env_remove("CINDER_RUSTC_WRAPPER_MODE")
            .env_remove("CINDER_WRAPPER_ACTIVE")
            .env_remove("CINDER_NEXT_RUSTC_WRAPPER")
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER")
            .output()
            .unwrap()
    }

    fn test_selected_example_no_run(&self, command_name: &str) -> Output {
        self.test_no_run_command(command_name, &["--example", "demo"], None)
    }

    fn test_selected_example(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .args(["test", "--example", "demo"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "test")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_TEST")
            .output()
            .unwrap()
    }

    fn test_selected_bin_no_run(&self) -> Output {
        self.test_selected_bin_no_run_command(None)
    }

    fn test_selected_bin_no_run_with_shell_value(&self, value: &str) -> Output {
        self.test_selected_bin_no_run_command(Some(value))
    }

    fn test_selected_bin_no_run_command(&self, shell_value: Option<&str>) -> Output {
        self.test_no_run_command(
            "test",
            &["--bin", self.package.unwrap_or("cinder-fast-run-fixture")],
            shell_value,
        )
    }

    fn test_selected_lib_no_run(&self) -> Output {
        self.test_no_run_command("test", &["--lib"], None)
    }

    fn test_selected_lib_no_run_asynchronously(&self) -> Output {
        self.test_no_run_command_with_recording("test", &["--lib"], None, false)
    }

    fn test_selected_integration_no_run(&self) -> Output {
        self.test_no_run_command("test", &["--test", "smoke"], None)
    }

    fn test_selected_lib_and_integration_no_run(&self) -> Output {
        self.test_no_run_command("test", &["--lib", "--test", "smoke"], None)
    }

    fn test_no_run_command(
        &self,
        command_name: &str,
        selectors: &[&str],
        shell_value: Option<&str>,
    ) -> Output {
        self.test_no_run_command_with_recording(command_name, selectors, shell_value, true)
    }

    fn test_no_run_command_with_recording(
        &self,
        command_name: &str,
        selectors: &[&str],
        shell_value: Option<&str>,
        synchronous: bool,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .args([command_name, "--no-run"])
            .args(selectors);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "test")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_TEST")
            .env_remove("CINDER_ARTIFACT_RECEIPT_DIRECTORY")
            .env_remove("CINDER_RUSTC_WRAPPER_MODE")
            .env_remove("CINDER_WRAPPER_ACTIVE")
            .env_remove("CINDER_NEXT_RUSTC_WRAPPER")
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER");
        if synchronous {
            command.env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1");
        } else {
            command.env_remove("CINDER_SYNCHRONOUS_STATE_RECORDING");
        }
        if let Some(value) = shell_value {
            command.env("_", value);
        }
        command.output().unwrap()
    }

    fn test_with_harness_no_run_argument(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .args(["test", "--example", "demo", "--", "--no-run"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "test")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_TEST")
            .output()
            .unwrap()
    }

    fn build_selected_bin_with_environment(&self, key: &str, value: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command
            .args(["--bin", self.package.unwrap_or("cinder-fast-run-fixture")])
            .env(key, value)
            .output()
            .unwrap()
    }

    fn build_in_target(&self, target: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build_in_target(&mut command, target);
        command.output().unwrap()
    }

    fn build_with_rustflags(&self, rustflags: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.env("RUSTFLAGS", rustflags).output().unwrap()
    }

    fn build_with_environment(&self, key: &str, value: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.env(key, value).output().unwrap()
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

    fn build_with_rustc(&self, compiler: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.env("RUSTC", compiler).output().unwrap()
    }

    fn cargo_build(&self) -> Output {
        let mut command = Command::new("cargo");
        self.configure_build(&mut command);
        command.output().unwrap()
    }

    fn cargo_build_named_bin_with_environment(
        &self,
        binary: &str,
        key: &str,
        value: &str,
    ) -> Output {
        let mut command = Command::new("cargo");
        self.configure_build(&mut command);
        command
            .args(["--bin", binary])
            .env(key, value)
            .output()
            .unwrap()
    }

    fn cargo_clean(&self) -> Output {
        let mut command = Command::new("cargo");
        command.current_dir(&self.root).args(["clean", "-p"]);
        command.arg(self.package.unwrap_or("cinder-fast-run-fixture"));
        command
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .output()
            .unwrap()
    }

    fn cinder_clean(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&self.root).args(["clean", "-p"]);
        command.arg(self.package.unwrap_or("cinder-fast-run-fixture"));
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .output()
            .unwrap()
    }

    fn cinder_clean_with_global_options(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .args(["--locked", "clean", "-p"]);
        command.arg(self.package.unwrap_or("cinder-fast-run-fixture"));
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .output()
            .unwrap()
    }

    fn preserve_only_public_outputs_across_clean(&self) {
        let name = self.package.unwrap_or("cinder-fast-run-fixture");
        let profile = self.root.join("target/debug");
        let artifact = profile.join(name);
        let dependency = artifact.with_extension("d");
        let saved_artifact = self.root.join("saved-public-artifact");
        let saved_dependency = self.root.join("saved-public-dependency");
        fs::rename(&artifact, &saved_artifact).unwrap();
        fs::rename(&dependency, &saved_dependency).unwrap();
        assert_success(&self.cargo_clean());
        fs::create_dir_all(&profile).unwrap();
        fs::rename(saved_artifact, artifact).unwrap();
        fs::rename(saved_dependency, dependency).unwrap();
    }

    fn built_stdout(&self) -> String {
        self.built_stdout_in_target(&self.root.join("target"))
    }

    fn built_example_stdout(&self) -> String {
        let output = Command::new(self.root.join("target/debug/examples/demo"))
            .output()
            .unwrap();
        assert_success(&output);
        stdout(&output)
    }

    fn built_stdout_in_target(&self, target: &Path) -> String {
        let name = self.package.unwrap_or("cinder-fast-run-fixture");
        let output = Command::new(target.join("debug").join(name))
            .env("CINDER_FIXTURE_VALUE", "build")
            .output()
            .unwrap();
        assert_success(&output);
        stdout(&output)
    }

    fn active_hashed_outputs(&self) -> (PathBuf, PathBuf, PathBuf) {
        let name = self.package.unwrap_or("cinder-fast-run-fixture");
        let normalized = name.replace('-', "_");
        let profile = self.root.join("target/debug");
        let public = fs::metadata(profile.join(name)).unwrap();
        let candidates: Vec<_> = fs::read_dir(profile.join("deps"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                    return false;
                };
                if path.extension().and_then(|extension| extension.to_str()) != Some("d")
                    || !file_name.starts_with(&format!("{normalized}-"))
                {
                    return false;
                }
                fs::metadata(path.with_extension("")).is_ok_and(|metadata| {
                    metadata.len() == public.len()
                        && metadata.modified().ok() == public.modified().ok()
                })
            })
            .collect();
        let [dependency] = candidates.as_slice() else {
            panic!("could not identify active Cargo hash: {candidates:?}");
        };
        let hash = dependency
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.strip_prefix(&format!("{normalized}-")))
            .unwrap();
        let fingerprints: Vec<_> = fs::read_dir(profile.join(".fingerprint"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(&format!("-{hash}")))
            })
            .collect();
        let [fingerprint] = fingerprints.as_slice() else {
            panic!("could not identify active Cargo fingerprint: {fingerprints:?}");
        };
        (
            dependency.clone(),
            dependency.with_extension(""),
            fingerprint.clone(),
        )
    }

    fn revision_run_artifact_count(&self) -> usize {
        fs::read_dir(self.root.join("target/debug"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(digest) = name
                    .strip_prefix(".cinder-fast-")
                    .and_then(|suffix| suffix.split_once('-').map(|(_, revision)| revision))
                    .and_then(|revision| revision.split('-').next())
                else {
                    return false;
                };
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .count()
    }

    fn revision_run_artifact_receipt_count(&self) -> usize {
        fs::read_dir(self.state_directory().join("run-artifact-identities"))
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0)
    }

    fn state_directory(&self) -> PathBuf {
        let canonical = fs::canonicalize(&self.root).unwrap();
        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        std::env::temp_dir()
            .join("cinder/state")
            .join(format!("{:016x}", hasher.finish()))
    }

    fn wait_for_state(&self, kind: &str) {
        let state = self.state_directory().join(kind).join("run-context");
        for _ in 0..100 {
            if state.is_file() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("background recorder did not publish {}", state.display());
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
        self.configure_build_in_target(command, &self.root.join("target"));
    }

    fn configure_build_in_target(&self, command: &mut Command, target: &Path) {
        command.current_dir(&self.root).args(["build"]);
        if let Some(package) = self.package {
            command.args(["-p", package]);
        }
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "build")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", target)
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
        let canonical = fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        let state = std::env::temp_dir()
            .join("cinder/state")
            .join(format!("{:016x}", hasher.finish()));
        let _ = fs::remove_dir_all(state);
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
