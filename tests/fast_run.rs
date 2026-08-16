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

fn prepare_direct_check_recipe(fixture: &Fixture, mut write_source: impl FnMut(i32)) -> i32 {
    let mut last_errors = String::new();
    for value in 2..=9 {
        write_source(value);
        let output = fixture.check_selected_lib_direct();
        assert_success(&output);
        last_errors = stderr(&output);
        if fixture
            .state_directory()
            .join("check/compiler-recipe")
            .is_file()
        {
            return value;
        }
    }
    panic!("compiler observer missed eight selected compilations:\n{last_errors}");
}

fn prepare_direct_integration_check_recipe(
    fixture: &Fixture,
    mut write_source: impl FnMut(i32),
) -> i32 {
    let mut last_errors = String::new();
    for value in 2..=9 {
        write_source(value);
        let output = fixture.check_selected_integration_direct();
        assert_success(&output);
        last_errors = stderr(&output);
        if fixture
            .state_directory()
            .join("check/compiler-recipe")
            .is_file()
        {
            return value;
        }
    }
    panic!("compiler observer missed eight integration-test compilations:\n{last_errors}");
}

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
        "expected no-change build reuse:\ninitial:\n{}\nunchanged:\n{}",
        stderr(&initial),
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
    let first = fixture.build_alias();
    assert_success(&first);
    fixture.write_structural_source(3);
    let second = fixture.build_alias();
    assert_success(&second);
    fixture.write_structural_source(2);

    let restored = fixture.build_alias();
    assert_success(&restored);
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "cargo b did not use build history:\nfirst:\n{}\nsecond:\n{}\nrestored:\n{}",
        stderr(&first),
        stderr(&second),
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
    command.env("CINDER_TRACE_RUN", "1");
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
    assert!(
        stderr(&output).contains("Cinder restored a validated previous build"),
        "historical build did not restore after the correct target lock was released:\n{}",
        stderr(&output)
    );
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
    assert!(
        fixture.state_directory().join("check").is_dir(),
        "initial check did not record Cinder state:\n{}",
        stderr(&initial)
    );

    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check"),
        "unchanged check did not use Cinder state:\n{}",
        stderr(&reused)
    );
}

#[test]
fn unrelated_existing_rust_file_changes_do_not_poison_a_selected_check() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    let unrelated = fixture.root.join("src/unrelated.rs");
    fs::write(&unrelated, "pub const VALUE: u32 = 1;\n").unwrap();

    assert_success(&fixture.check_selected_bin());
    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    fs::write(&unrelated, "pub const VALUE: u32 = 2;\n").unwrap();
    let changed = fixture.check_selected_bin();
    assert_success(&changed);
    assert!(
        stderr(&changed).contains("Cinder reused the validated check"),
        "an unrelated Rust file poisoned selected-check reuse:\n{}",
        stderr(&changed)
    );
}

#[test]
fn internal_symlinked_directories_preserve_safe_check_reuse() {
    let fixture = Fixture::new();
    let real = fixture.root.join("src/runtime/cli");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("used.rs"), "pub const VALUE: i32 = 1;\n").unwrap();
    std::os::unix::fs::symlink("runtime/cli", fixture.root.join("src/cli")).unwrap();
    fs::write(
        &fixture.source,
        "#[path = \"cli/used.rs\"]\nmod used;\nfn main() { println!(\"{}\", used::VALUE); }\n",
    )
    .unwrap();

    assert_success(&fixture.check_selected_bin());
    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check"),
        "an internal symlink disabled safe selected-check reuse:\n{}",
        stderr(&reused)
    );

    fs::remove_file(fixture.root.join("src/cli")).unwrap();
    let removed_alias = fixture.check_selected_bin();
    assert_success(&removed_alias);
    assert!(
        !stderr(&removed_alias).contains("Cinder reused"),
        "removing an internal symlink reused stale topology:\n{}",
        stderr(&removed_alias)
    );

    std::os::unix::fs::symlink("runtime/cli", fixture.root.join("src/cli")).unwrap();
    assert_success(&fixture.check_selected_bin());
    let reused_after_restore = fixture.check_selected_bin();
    assert_success(&reused_after_restore);
    assert!(
        stderr(&reused_after_restore).contains("Cinder reused"),
        "a restored internal symlink did not produce reusable state:\n{}",
        stderr(&reused_after_restore)
    );

    fs::write(real.join("used.rs"), "pub const VALUE: i32 = 2;\n").unwrap();
    let changed = fixture.check_selected_bin();
    assert_success(&changed);
    assert!(
        !stderr(&changed).contains("Cinder reused"),
        "a file behind an internal symlink reused stale state:\n{}",
        stderr(&changed)
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
fn default_check_reuse_does_not_persist_experimental_compiler_recipes() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    fixture.write_static_library_source(2);
    assert_success(&fixture.check_selected_lib());
    assert!(
        !fixture
            .state_directory()
            .join("check/compiler-recipe")
            .exists()
    );

    let reused = fixture.check_selected_lib();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));
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
fn experimental_direct_check_matches_cargos_selected_library_artifact() {
    let fixture = Fixture::new_static_library();
    let initial =
        prepare_direct_check_recipe(&fixture, |value| fixture.write_static_library_source(value));

    fixture.write_static_library_source(initial + 1);
    let replayed = fixture.check_selected_lib_direct();
    assert_success(&replayed);
    assert!(
        stderr(&replayed).contains("Cinder replayed Cargo's validated compiler recipe"),
        "changed library did not use the direct compiler recipe:\n{}",
        stderr(&replayed)
    );
    assert!(!stderr(&replayed).contains("Checking cinder-fast-run-fixture"));

    let artifact = PathBuf::from(OsString::from_vec(
        fs::read(fixture.state_directory().join("check/artifact")).unwrap(),
    ));
    let replayed_artifact = fs::read(&artifact).unwrap();
    let dependency_file = fs::read_dir(fixture.root.join("target/debug/deps"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension() == Some(OsStr::new("d"))
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with("cinder_fast_run_fixture-"))
        })
        .unwrap();
    let replayed_dependencies = fs::read(&dependency_file).unwrap();
    assert!(!replayed_dependencies.is_empty());

    let reused = fixture.check_selected_lib_direct();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    let cargo = fixture.cargo_check_selected_lib();
    assert_success(&cargo);
    assert!(stderr(&cargo).contains("Checking cinder-fast-run-fixture"));
    assert_eq!(fs::read(artifact).unwrap(), replayed_artifact);
    assert_eq!(fs::read(dependency_file).unwrap(), replayed_dependencies);
}

#[test]
fn experimental_direct_check_uses_a_local_topology_rescan_after_an_atomic_save() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let initial =
        prepare_direct_check_recipe(&fixture, |value| fixture.write_static_library_source(value));

    fixture.write_static_library_source(initial + 1);
    let editor_staging = fixture.root.join("src/.editor-staging");
    fs::write(&editor_staging, b"temporary").unwrap();
    fs::remove_file(editor_staging).unwrap();

    let replayed = fixture.check_selected_lib_direct();
    assert_success(&replayed);
    let errors = stderr(&replayed);
    assert!(
        errors.contains("Cinder replayed Cargo's validated compiler recipe"),
        "an atomic editor save did not retain the guarded direct path:\n{errors}"
    );
    assert!(
        errors.contains("project-rescans=1"),
        "an atomic editor save did not validate only its changed project subtree:\n{errors}"
    );
    assert!(
        !errors.contains("full-rescan=true"),
        "an atomic editor save unexpectedly required a full topology scan:\n{errors}"
    );
}

#[test]
fn experimental_direct_check_defers_failures_to_cargo() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    prepare_direct_check_recipe(&fixture, |value| fixture.write_static_library_source(value));

    fs::write(&fixture.source, "pub fn broken( {\n").unwrap();
    let failed = fixture.check_selected_lib_direct();
    assert!(!failed.status.success());
    assert!(stderr(&failed).contains("could not compile `cinder-fast-run-fixture`"));
    assert!(!stderr(&failed).contains("\"$message_type\""));
    assert!(!stderr(&failed).contains("Cinder replayed Cargo's validated compiler recipe"));
}

#[test]
fn experimental_direct_check_keeps_build_script_packages_on_cargo() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let environment_probe = fixture.root.join("build-script-compiler-environment");
    fs::write(
        fixture.root.join("build.rs"),
        r#"fn main() {
    let values = [
        "RUSTC_WRAPPER",
        "CINDER_ARTIFACT_RECEIPT_DIRECTORY",
        "CINDER_EXPERIMENTAL_DIRECT_CHECK",
        "CINDER_TRACE_RUN",
        "CINDER_SYNCHRONOUS_STATE_RECORDING",
        "CINDER_USAGE",
        "CINDER_REAL_CARGO",
    ]
        .map(|key| std::env::var(key).unwrap_or_else(|_| "absent".to_owned()));
    let output = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("build-script-compiler-environment");
    std::fs::write(output, values.join("|")).unwrap();
    println!("cargo:rerun-if-changed=build-input.txt");
}
"#,
    )
    .unwrap();
    fs::write(fixture.root.join("build-input.txt"), "initial\n").unwrap();
    fixture.write_static_library_source(2);
    let mut initial = fixture.check_selected_lib_direct_command();
    initial.env("CINDER_USAGE", "1");
    assert_success(&initial.output().unwrap());
    assert_eq!(
        fs::read_to_string(environment_probe).unwrap(),
        "absent|absent|absent|absent|absent|absent|absent"
    );

    fixture.write_static_library_source(3);
    let changed = fixture.check_selected_lib_direct();
    assert_success(&changed);
    assert!(stderr(&changed).contains("Checking cinder-fast-run-fixture"));
    assert!(!stderr(&changed).contains("Cinder replayed Cargo's validated compiler recipe"));
}

#[test]
fn experimental_direct_check_restores_cargo_environment_observed_in_dep_info() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let initial = prepare_direct_check_recipe(&fixture, |value| {
        fs::write(
            &fixture.source,
            format!("pub fn value() -> i32 {{ let _ = env!(\"CARGO_PKG_VERSION\"); {value} }}\n"),
        )
        .unwrap();
    });

    fs::write(
        &fixture.source,
        format!(
            "pub fn value() -> i32 {{ let _ = env!(\"CARGO_PKG_VERSION\"); {} }}\n",
            initial + 1
        ),
    )
    .unwrap();
    let replayed = fixture.check_selected_lib_direct();
    assert_success(&replayed);
    assert!(
        stderr(&replayed).contains("Cinder replayed Cargo's validated compiler recipe"),
        "Cargo-defined environment was not restored for compiler replay:\n{}",
        stderr(&replayed)
    );
    assert!(!stderr(&replayed).contains("Checking cinder-fast-run-fixture"));
}

#[test]
fn experimental_direct_check_rejects_new_unreplayed_environment_dependencies() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let cargo_config = fixture.root.join(".cargo");
    fs::create_dir(&cargo_config).unwrap();
    fs::write(
        cargo_config.join("config.toml"),
        "[env]\nCINDER_REPLAY_CARGO_ONLY = \"cargo-only\"\n",
    )
    .unwrap();
    let initial =
        prepare_direct_check_recipe(&fixture, |value| fixture.write_static_library_source(value));

    fs::write(
        &fixture.source,
        format!(
            "pub const OBSERVED: &str = match option_env!(\"CINDER_REPLAY_CARGO_ONLY\") {{ Some(value) => value, None => \"missing\" }};\npub fn value() -> i32 {{ {} }}\n",
            initial + 1
        ),
    )
    .unwrap();
    let changed = fixture.check_selected_lib_direct();
    assert_success(&changed);
    let errors = stderr(&changed);
    assert!(
        errors.contains("compiler replay introduced an unmatched environment dependency"),
        "a newly introduced Cargo-only option_env dependency was not detected:\n{errors}"
    );
    assert!(
        errors.contains("Checking cinder-fast-run-fixture"),
        "Cargo did not replace the rejected compiler replay:\n{errors}"
    );
    assert!(
        !errors.contains("Cinder replayed Cargo's validated compiler recipe"),
        "Cinder published an artifact built with the wrong environment:\n{errors}"
    );
}

#[test]
fn experimental_direct_check_rejects_new_absent_environment_dependencies() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    assert!(std::env::var_os("CINDER_REPLAY_NEW_UNSET").is_none());
    let initial =
        prepare_direct_check_recipe(&fixture, |value| fixture.write_static_library_source(value));

    fs::write(
        &fixture.source,
        format!(
            "pub const OBSERVED: Option<&str> = option_env!(\"CINDER_REPLAY_NEW_UNSET\");\npub fn value() -> i32 {{ {} }}\n",
            initial + 1
        ),
    )
    .unwrap();
    let changed = fixture.check_selected_lib_direct();
    assert_success(&changed);
    let errors = stderr(&changed);
    assert!(
        errors.contains("compiler replay introduced an unmatched environment dependency"),
        "a newly introduced absent option_env dependency was not detected:\n{errors}"
    );
    assert!(
        errors.contains("Checking cinder-fast-run-fixture"),
        "Cargo did not establish a baseline for the new absent dependency:\n{errors}"
    );
    assert!(
        !errors.contains("Cinder replayed Cargo's validated compiler recipe"),
        "Cinder published a replay with a new absent environment dependency:\n{errors}"
    );
}

#[test]
fn experimental_direct_check_unescapes_cargo_environment_exactly() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let manifest_path = fixture.root.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).unwrap();
    fs::write(
        &manifest_path,
        manifest.replace(
            "edition = \"2024\"\n",
            "edition = \"2024\"\ndescription = \"line\\\\\\\\path\\nnext\\rend\"\n",
        ),
    )
    .unwrap();
    let initial = prepare_direct_check_recipe(&fixture, |value| {
        fs::write(
            &fixture.source,
            format!(
                "pub const DESCRIPTION: &str = env!(\"CARGO_PKG_DESCRIPTION\");\nconst _: () = {{ let bytes = DESCRIPTION.as_bytes(); assert!(bytes.len() == 19 && bytes[4] == b'\\\\' && bytes[5] == b'\\\\' && bytes[10] == b'\\n' && bytes[15] == b'\\r'); }};\npub fn value() -> i32 {{ {value} }}\n"
            ),
        )
        .unwrap();
    });

    fs::write(
        &fixture.source,
        format!(
            "pub const DESCRIPTION: &str = env!(\"CARGO_PKG_DESCRIPTION\");\nconst _: () = {{ let bytes = DESCRIPTION.as_bytes(); assert!(bytes.len() == 19 && bytes[4] == b'\\\\' && bytes[5] == b'\\\\' && bytes[10] == b'\\n' && bytes[15] == b'\\r'); }};\npub fn value() -> i32 {{ {} }}\n",
            initial + 1
        ),
    )
    .unwrap();
    let replayed = fixture.check_selected_lib_direct();
    assert_success(&replayed);
    assert!(
        stderr(&replayed).contains("Cinder replayed Cargo's validated compiler recipe"),
        "escaped Cargo environment did not use the guarded replay:\n{}",
        stderr(&replayed)
    );
}

#[test]
fn experimental_direct_check_restores_cargo_target_tmpdir_for_integration_tests() {
    let fixture = Fixture::new();
    let initial = prepare_direct_integration_check_recipe(&fixture, |value| {
        fixture.write_integration_target_tmpdir_source(value);
    });

    fixture.write_integration_target_tmpdir_source(initial + 1);
    let replayed = fixture.check_selected_integration_direct();
    assert_success(&replayed);
    assert!(
        stderr(&replayed).contains("Cinder replayed Cargo's validated compiler recipe"),
        "Cargo's integration-test environment was not restored for compiler replay:\n{}",
        stderr(&replayed)
    );
    let artifact = PathBuf::from(OsString::from_vec(
        fs::read(fixture.state_directory().join("check/artifact")).unwrap(),
    ));
    let replayed_artifact = fs::read(&artifact).unwrap();

    let cargo = fixture.cargo_check_selected_integration();
    assert_success(&cargo);
    assert!(stderr(&cargo).contains("Checking cinder-fast-run-fixture"));
    assert_eq!(
        fs::read(artifact).unwrap(),
        replayed_artifact,
        "compiler replay diverged from Cargo's integration-test environment"
    );
}

#[test]
fn untracked_proc_macro_environment_cannot_produce_a_wrong_replay() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let manifest = fs::read_to_string(fixture.root.join("Cargo.toml")).unwrap();
    fs::write(
        fixture.root.join("Cargo.toml"),
        format!(
            "{}\n[dependencies]\nmacro-env = {{ path = \"macro-env\" }}\n",
            manifest.replace("version = \"0.0.0\"", "version = \"1.2.3\"")
        ),
    )
    .unwrap();
    fs::create_dir_all(fixture.root.join("macro-env/src")).unwrap();
    fs::write(
        fixture.root.join("macro-env/Cargo.toml"),
        "[package]\nname = \"macro-env\"\nversion = \"9.9.9\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("macro-env/src/lib.rs"),
        r#"extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro]
pub fn package_version(_: TokenStream) -> TokenStream {
    let value = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "missing".to_owned());
    format!("{value:?}").parse().unwrap()
}
"#,
    )
    .unwrap();
    fs::write(
        &fixture.source,
        "pub const VERSION: &str = macro_env::package_version!();\npub fn value() -> i32 { 2 }\n",
    )
    .unwrap();
    let initial = fixture.check_selected_lib_direct();
    assert_success(&initial);
    assert!(
        !fixture
            .state_directory()
            .join("check/compiler-recipe")
            .exists(),
        "a procedural-macro graph published an unsafe compiler recipe:\n{}",
        stderr(&initial)
    );

    fs::write(
        &fixture.source,
        "pub const VERSION: &str = macro_env::package_version!();\npub fn value() -> i32 { 3 }\n",
    )
    .unwrap();
    let changed = fixture.check_selected_lib_direct();
    assert_success(&changed);
    assert!(
        stderr(&changed).contains("Checking cinder-fast-run-fixture"),
        "a changed procedural-macro consumer did not stay on Cargo:\n{}",
        stderr(&changed)
    );
    assert!(
        !stderr(&changed).contains("Cinder replayed Cargo's validated compiler recipe"),
        "a procedural macro ran outside Cargo's process contract:\n{}",
        stderr(&changed)
    );
    assert!(
        !fixture
            .state_directory()
            .join("check/compiler-recipe")
            .exists(),
        "a fresh procedural-macro dependency failed to suppress recipe publication:\n{}",
        stderr(&changed)
    );

    fs::write(
        &fixture.source,
        "pub const VERSION: &str = macro_env::package_version!();\npub fn value() -> i32 { 4 }\n",
    )
    .unwrap();
    let repeated = fixture.check_selected_lib_direct();
    assert_success(&repeated);
    assert!(
        stderr(&repeated).contains("Checking cinder-fast-run-fixture"),
        "a repeated procedural-macro edit escaped Cargo ownership:\n{}",
        stderr(&repeated)
    );
    assert!(
        !stderr(&repeated).contains("Cinder replayed Cargo's validated compiler recipe"),
        "a repeated procedural-macro edit used an unsafe replay:\n{}",
        stderr(&repeated)
    );
}

#[test]
fn experimental_direct_check_defers_warnings_to_cargo() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    prepare_direct_check_recipe(&fixture, |value| fixture.write_static_library_source(value));

    fs::write(
        &fixture.source,
        "pub fn changed() { let deliberately_unused = 3; }\n",
    )
    .unwrap();
    let warned = fixture.check_selected_lib_direct();
    assert_success(&warned);
    let errors = stderr(&warned);
    assert!(errors.contains("Checking cinder-fast-run-fixture"));
    assert!(errors.contains("unused variable: `deliberately_unused`"));
    assert!(!errors.contains("\"$message_type\""));
    assert!(!errors.contains("Cinder replayed Cargo's validated compiler recipe"));
}

#[test]
fn experimental_direct_check_defers_source_topology_changes_to_cargo() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    fs::write(
        fixture.root.join("src/extra.rs"),
        "pub fn value() -> i32 { 3 }\n",
    )
    .unwrap();
    prepare_direct_check_recipe(&fixture, |value| {
        fs::write(
            &fixture.source,
            format!("pub fn value() -> i32 {{ {value} }}\n"),
        )
        .unwrap();
    });

    fs::write(
        &fixture.source,
        "mod extra;\npub fn value() -> i32 { extra::value() }\n",
    )
    .unwrap();
    let changed = fixture.check_selected_lib_direct();
    assert_success(&changed);
    assert!(stderr(&changed).contains("Checking cinder-fast-run-fixture"));
    assert!(!stderr(&changed).contains("Cinder replayed Cargo's validated compiler recipe"));
}

#[test]
fn corrupt_compiler_recipe_does_not_disable_normal_check_reuse() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let initial =
        prepare_direct_check_recipe(&fixture, |value| fixture.write_static_library_source(value));
    fs::write(
        fixture.state_directory().join("check/compiler-recipe"),
        b"invalid",
    )
    .unwrap();

    let reused = fixture.check_selected_lib_direct();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    fixture.write_static_library_source(initial + 1);
    let changed = fixture.check_selected_lib_direct();
    assert_success(&changed);
    assert!(stderr(&changed).contains("Checking cinder-fast-run-fixture"));
    assert!(!stderr(&changed).contains("Cinder replayed Cargo's validated compiler recipe"));
}

#[test]
fn concurrent_experimental_direct_checks_share_cargos_target_lock() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let initial =
        prepare_direct_check_recipe(&fixture, |value| fixture.write_static_library_source(value));
    fixture.write_static_library_source(initial + 1);

    let mut first = fixture.check_selected_lib_direct_command();
    first.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut second = fixture.check_selected_lib_direct_command();
    second.stdout(Stdio::piped()).stderr(Stdio::piped());
    let first = first.spawn().unwrap();
    let second = second.spawn().unwrap();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert_success(&first);
    assert_success(&second);

    let errors = format!("{}{}", stderr(&first), stderr(&second));
    assert_eq!(
        errors
            .matches("Cinder replayed Cargo's validated compiler recipe")
            .count(),
        1,
        "concurrent checks did not compile exactly once:\n{errors}"
    );
    assert_eq!(
        errors.matches("Cinder reused the validated check").count(),
        1,
        "the second concurrent check did not reuse published state:\n{errors}"
    );
    assert!(!errors.contains("Checking cinder-fast-run-fixture"));
}

#[test]
fn missing_project_topology_falls_back_until_cargo_rebuilds_it() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.check_selected_bin());
    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    let topology = fixture.state_directory().join("check/project-topology");
    assert!(topology.is_file());
    fs::remove_file(&topology).unwrap();

    let fallback = fixture.check_selected_bin();
    assert_success(&fallback);
    assert!(
        !stderr(&fallback).contains("Cinder reused"),
        "state without its topology guard was reused:\n{}",
        stderr(&fallback)
    );
    assert!(
        topology.is_file(),
        "Cargo's fresh artifact message did not restore the topology guard"
    );

    let refreshed = fixture.check_selected_bin();
    assert_success(&refreshed);
    assert!(stderr(&refreshed).contains("Cinder reused the validated check"));

    assert_success(&fixture.cargo_clean());
    let rebuilt = fixture.check_selected_bin();
    assert_success(&rebuilt);
    assert!(stderr(&rebuilt).contains("Checking cinder-fast-run-fixture"));
    assert!(topology.is_file());

    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));
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
fn multi_unit_check_reuses_and_invalidates_per_target() {
    let fixture = Fixture::new();
    fixture.write_library_source("one");
    assert_success(&fixture.check_default());
    assert!(fixture.state_directory().join("check").exists());

    let reused = fixture.check_default();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check of 2 targets"),
        "multi-target no-change check did not reuse:\n{}",
        stderr(&reused)
    );

    // Editing only the binary source must invalidate the whole recorded set.
    fs::write(
        &fixture.source,
        "fn main() { println!(\"{} bin-two\", env!(\"CINDER_FIXTURE_VALUE\")); }\n",
    )
    .unwrap();
    let changed = fixture.check_default();
    assert_success(&changed);
    assert!(!stderr(&changed).contains("Cinder reused"));
    let reconverged = fixture.check_default();
    assert_success(&reconverged);
    assert!(
        stderr(&reconverged).contains("Cinder reused the validated check of 2 targets"),
        "edited multi-target check did not reconverge:\n{}",
        stderr(&reconverged)
    );

    // Editing only the library source must invalidate as well.
    fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn value() -> &'static str { \"library-two\" }\n",
    )
    .unwrap();
    let library_changed = fixture.check_default();
    assert_success(&library_changed);
    assert!(!stderr(&library_changed).contains("Cinder reused"));
    let library_reconverged = fixture.check_default();
    assert_success(&library_reconverged);
    assert!(
        stderr(&library_reconverged).contains("Cinder reused the validated check of 2 targets")
    );
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
fn repeated_package_library_tests_run_without_cargo_and_preserve_runtime_contract() {
    let fixture = Fixture::new();
    fixture.write_library_runtime_test_source("initial");
    fs::create_dir_all(fixture.root.join("target")).unwrap();
    fs::write(fixture.root.join("target/cinder-runtime-result"), "pass\n").unwrap();

    let initial = fixture.test_selected_lib();
    assert_success(&initial);
    assert!(stdout(&initial).contains("cinder-runtime-initial-pass"));
    assert!(stderr(&initial).contains("Compiling cinder-fast-run-fixture"));
    fixture.wait_for_state("test");
    assert!(
        fixture.state_directory().join("test").is_dir(),
        "successful Cargo test did not record execution state:\n{}",
        stderr(&initial)
    );

    let reused = fixture.test_selected_lib();
    assert_success(&reused);
    assert!(stdout(&reused).contains("cinder-runtime-initial-pass"));
    assert!(
        stderr(&reused).contains("Cinder running the validated test executable"),
        "an unchanged library test did not bypass Cargo:\n{}",
        stderr(&reused)
    );
    assert!(!stderr(&reused).contains("Finished `test` profile"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("target/cinder-runtime-count")).unwrap(),
        "2"
    );

    fs::write(fixture.root.join("target/cinder-runtime-result"), "fail\n").unwrap();
    let failed = fixture.test_selected_lib();
    assert_eq!(failed.status.code(), Some(101));
    assert!(stdout(&failed).contains("cinder-runtime-initial-fail"));
    assert!(stdout(&failed).contains("test result: FAILED"));
    assert!(stderr(&failed).contains("Cinder running the validated test executable"));
    assert!(stderr(&failed).contains("error: test failed, to rerun pass `--lib`"));
    assert!(!stderr(&failed).contains("Compiling cinder-fast-run-fixture"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("target/cinder-runtime-count")).unwrap(),
        "3",
        "a failing fast test was executed more than once"
    );
}

#[test]
fn workspace_selected_library_tests_reuse_the_exact_package_working_directory() {
    let fixture = Fixture::new_workspace();
    fs::write(
        fixture.root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"dependency\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("app/Cargo.toml"),
        "[package]\nname = \"cinder-workspace-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\ncinder-workspace-dependency = { path = \"../dependency\" }\n",
    )
    .unwrap();
    let dependency = fixture.root.join("dependency");
    fs::create_dir_all(dependency.join("src")).unwrap();
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"cinder-workspace-dependency\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(dependency.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fixture.write_library_runtime_test_source("workspace");
    let package = fixture.root.join("app");
    fs::create_dir_all(package.join("target")).unwrap();
    fs::write(package.join("target/cinder-runtime-result"), "pass\n").unwrap();

    let initial = fixture.test_selected_workspace_lib();
    assert_success(&initial);
    assert!(stdout(&initial).contains("cinder-runtime-workspace-pass"));
    assert!(stderr(&initial).contains("Compiling cinder-workspace-fixture"));

    let reused = fixture.test_selected_workspace_lib();
    assert_success(&reused);
    assert!(stdout(&reused).contains("cinder-runtime-workspace-pass"));
    assert!(stderr(&reused).contains("Cinder running the validated test executable"));
    assert!(!stderr(&reused).contains("Finished `test` profile"));
    assert_eq!(
        fs::read_to_string(package.join("target/cinder-runtime-count")).unwrap(),
        "2"
    );

    let runtime_directory_path = fixture.state_directory().join("test/runtime-directory");
    let mut runtime_directory_record = fs::read(&runtime_directory_path).unwrap();
    runtime_directory_record.truncate(32);
    runtime_directory_record.extend_from_slice(
        fs::canonicalize(&dependency)
            .unwrap()
            .as_os_str()
            .as_encoded_bytes(),
    );
    fs::write(runtime_directory_path, runtime_directory_record).unwrap();
    let corrupted = fixture.test_selected_workspace_lib();
    assert_success(&corrupted);
    assert!(stdout(&corrupted).contains("cinder-runtime-workspace-pass"));
    assert!(!stderr(&corrupted).contains("Cinder running the validated test executable"));
    assert!(stderr(&corrupted).contains("Finished `test` profile"));

    let equals_initial = fixture.test_selected_workspace_lib_with_equals();
    assert_success(&equals_initial);
    assert!(stderr(&equals_initial).contains("Finished `test` profile"));
    let equals_reused = fixture.test_selected_workspace_lib_with_equals();
    assert_success(&equals_reused);
    assert!(stderr(&equals_reused).contains("Cinder running the validated test executable"));

    fixture.write_library_runtime_test_source("workspace-changed");
    let changed = fixture.test_selected_workspace_lib_with_equals();
    assert_success(&changed);
    assert!(stdout(&changed).contains("cinder-runtime-workspace-changed-pass"));
    assert!(!stderr(&changed).contains("Cinder running the validated test executable"));
    assert!(stderr(&changed).contains("Compiling cinder-workspace-fixture"));
}

#[test]
fn failed_cargo_tests_never_publish_fast_execution_state() {
    let fixture = Fixture::new();
    fixture.write_library_runtime_test_source("initial-failure");
    fs::create_dir_all(fixture.root.join("target")).unwrap();
    fs::write(fixture.root.join("target/cinder-runtime-result"), "fail\n").unwrap();

    let failed = fixture.test_selected_lib();
    assert_eq!(failed.status.code(), Some(101));
    assert!(stdout(&failed).contains("cinder-runtime-initial-failure-fail"));
    assert!(!stderr(&failed).contains("Cinder running the validated test executable"));
    assert!(!fixture.state_directory().join("test").exists());

    fs::write(fixture.root.join("target/cinder-runtime-result"), "pass\n").unwrap();
    let recovered = fixture.test_selected_lib();
    assert_success(&recovered);
    assert!(stdout(&recovered).contains("cinder-runtime-initial-failure-pass"));
    assert!(stderr(&recovered).contains("Finished `test` profile"));
    assert!(!stderr(&recovered).contains("Cinder running the validated test executable"));
    fixture.wait_for_state("test");

    let reused = fixture.test_selected_lib();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder running the validated test executable"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("target/cinder-runtime-count")).unwrap(),
        "3"
    );
}

#[test]
fn test_alias_runs_the_same_validated_library_harness() {
    let fixture = Fixture::new();
    fixture.write_library_runtime_test_source("alias");
    fs::create_dir_all(fixture.root.join("target")).unwrap();
    fs::write(fixture.root.join("target/cinder-runtime-result"), "pass\n").unwrap();

    assert_success(&fixture.test_selected_lib_alias());
    let reused = fixture.test_selected_lib_alias();
    assert_success(&reused);
    assert!(stdout(&reused).contains("cinder-runtime-alias-pass"));
    assert!(stderr(&reused).contains("Cinder running the validated test executable"));
}

#[test]
fn changed_package_library_tests_return_to_cargo_before_execution() {
    let fixture = Fixture::new();
    fixture.write_library_runtime_test_source("first");
    fs::create_dir_all(fixture.root.join("target")).unwrap();
    fs::write(fixture.root.join("target/cinder-runtime-result"), "pass\n").unwrap();
    assert_success(&fixture.test_selected_lib());
    fixture.wait_for_state("test");
    assert_success(&fixture.test_selected_lib());

    fixture.write_library_runtime_test_source("second");
    let changed = fixture.test_selected_lib();
    assert_success(&changed);
    assert!(stdout(&changed).contains("cinder-runtime-second-pass"));
    assert!(!stderr(&changed).contains("Cinder running the validated test executable"));
    assert!(stderr(&changed).contains("Compiling cinder-fast-run-fixture"));
}

#[test]
fn positional_test_filters_and_project_runners_stay_cargo_owned() {
    let filtered = Fixture::new();
    filtered.write_library_runtime_test_source("filtered");
    fs::create_dir_all(filtered.root.join("target")).unwrap();
    fs::write(filtered.root.join("target/cinder-runtime-result"), "pass\n").unwrap();
    for _ in 0..2 {
        let output = filtered.test_selected_lib_with_positional_filter();
        assert_success(&output);
        assert!(stdout(&output).contains("cinder-runtime-filtered-pass"));
        assert!(!stderr(&output).contains("Cinder running the validated test executable"));
    }
    assert!(!filtered.state_directory().join("test").exists());

    let configured = Fixture::new();
    configured.write_library_runtime_test_source("runner");
    fs::create_dir_all(configured.root.join("target")).unwrap();
    fs::write(
        configured.root.join("target/cinder-runtime-result"),
        "pass\n",
    )
    .unwrap();
    let runner_probe = configured.root.join("runner-probe");
    configured.enable_test_runner(&runner_probe);
    for _ in 0..2 {
        let output = configured.test_selected_lib();
        assert_success(&output);
        assert!(stdout(&output).contains("cinder-runtime-runner-pass"));
        assert!(!stderr(&output).contains("Cinder running the validated test executable"));
    }
    assert_eq!(fs::read_to_string(runner_probe).unwrap(), "run\nrun\n");
    assert!(!configured.state_directory().join("test").exists());
}

#[test]
fn nonstandard_library_harnesses_run_through_cargo_every_time() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("Cargo.toml"),
        "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\nharness = false\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("src/lib.rs"),
        "fn main() { println!(\"cinder-custom-library-harness\"); }\n",
    )
    .unwrap();

    for _ in 0..2 {
        let output = fixture.test_selected_lib();
        assert_success(&output);
        assert!(stdout(&output).contains("cinder-custom-library-harness"));
        assert!(!stderr(&output).contains("Cinder running the validated test executable"));
    }
    assert!(!fixture.state_directory().join("test").exists());
}

#[test]
fn trailing_check_arguments_cannot_be_hidden_by_reuse() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.check_selected_bin());
    assert_success(&fixture.check_selected_bin());

    let output = fixture.check_selected_bin_with_trailing_argument("--definitely-not-a-rustc-arg");
    assert!(!output.status.success());
    assert!(!stderr(&output).contains("Cinder reused"));
}

#[test]
fn quiet_check_commands_keep_cargos_output_contract() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);

    let initial = fixture.check_selected_bin_with_output_flag("--quiet");
    assert_success(&initial);
    let repeated = fixture.check_selected_bin_with_output_flag("--quiet");
    assert_success(&repeated);
    assert!(!stderr(&repeated).contains("Cinder reused"));
    assert!(stderr(&repeated).is_empty(), "quiet Cargo emitted output");
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
    assert!(
        fixture
            .root
            .join("target/debug/deps/cinder_fixture_app_lib.d")
            .is_file(),
        "Cargo did not produce the unhashed multi-crate-type dependency layout"
    );
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
fn changed_package_id_recovers_message_capture_after_one_safe_fallback() {
    let fixture = Fixture::new();
    fixture.write_structural_source(2);
    assert_success(&fixture.build());

    fs::write(
        fixture.root.join("Cargo.toml"),
        "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.1\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fixture.write_structural_source(3);
    let changed_id = fixture.build();
    assert_success(&changed_id);
    assert!(stderr(&changed_id).contains("Compiling cinder-fast-run-fixture"));
    assert!(!stderr(&changed_id).contains("Cinder restored"));

    fixture.write_structural_source(4);
    let recaptured = fixture.build();
    assert_success(&recaptured);
    assert!(stderr(&recaptured).contains("Compiling cinder-fast-run-fixture"));

    let reused = fixture.build();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused"),
        "updated package ID was not recaptured after fallback:\n{}",
        stderr(&reused)
    );
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

    fixture.write_structural_source(9);
    let missed = fixture.build_with_environment("CINDER_TRACE_RUN", "1");
    assert_success(&missed);
    assert!(
        stderr(&missed).contains("Compiling cinder-fast-run-fixture"),
        "an unseen revision did not compile through Cargo:\n{}",
        stderr(&missed)
    );
    assert!(
        stderr(&missed).contains("history source-probes=1 loaded-candidates=0"),
        "revision lookup fully loaded a source-mismatched candidate:\n{}",
        stderr(&missed)
    );

    fixture.write_structural_source(2);
    let restored = fixture.build_with_environment("CINDER_TRACE_RUN", "1");
    assert_success(&restored);
    assert_eq!(fixture.built_stdout(), "15");
    assert!(
        stderr(&restored).contains("Cinder restored a validated previous build"),
        "expected revision-history hit, got:\n{}",
        stderr(&restored)
    );
    assert!(
        stderr(&restored).contains("history source-probes=1 loaded-candidates=1"),
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
fn cargo_message_receipts_enable_safe_build_script_package_optimization() {
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
    assert!(stderr(&receipt_build).contains("Cinder patched src/main.rs"));

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
fn environment_configured_rustc_wrapper_keeps_build_cargo_owned() {
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
    assert!(!fixture.state_directory().join("build").exists());

    fixture.write_ordinary_source("two");
    let rebuilt = fixture.build_with_wrapper(&wrapper, &probe);
    assert_success(&rebuilt);
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
    assert!(!stderr(&rebuilt).contains("Cinder patched"));
}

#[test]
fn environment_configured_workspace_wrapper_keeps_build_cargo_owned() {
    let fixture = Fixture::new();
    fixture.write_ordinary_source("one");
    let wrapper = fixture.root.join("rustc-workspace-wrapper.sh");
    let probe = fixture.root.join("workspace-wrapper-probe");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf x >> \"$CINDER_WRAPPER_PROBE\"\nexec \"$@\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();

    let initial = fixture.build_with_workspace_wrapper(&wrapper, &probe);
    assert_success(&initial);
    assert!(probe.is_file(), "Cargo's workspace wrapper was not invoked");
    assert!(!fixture.state_directory().join("build").exists());

    fixture.write_ordinary_source("two");
    let rebuilt = fixture.build_with_workspace_wrapper(&wrapper, &probe);
    assert_success(&rebuilt);
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
    assert!(!stderr(&rebuilt).contains("Cinder patched"));
}

#[test]
fn manifest_discovery_from_a_child_directory_stays_cargo_owned() {
    let fixture = Fixture::new();
    let child = fixture.root.join("nested");
    fs::create_dir(&child).unwrap();
    fixture.write_ordinary_source("one");

    let initial = fixture.build_from(&child);
    assert_success(&initial);
    assert!(!stderr(&initial).contains("\"reason\":\"compiler-"));

    fixture.write_ordinary_source("two");
    let rebuilt = fixture.build_from(&child);
    assert_success(&rebuilt);
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
    assert!(!stderr(&rebuilt).contains("Cinder patched"));
    assert_eq!(fixture.built_stdout(), "build ordinary-two");
}

#[test]
fn cargo_configured_rustc_wrapper_keeps_build_cargo_owned() {
    let fixture = Fixture::new();
    fixture.write_ordinary_source("one");
    let wrapper = fixture.root.join("rustc-wrapper.sh");
    let probe = fixture.root.join("wrapper-probe");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf x >> {:?}\nexec \"$@\"\n",
            probe.to_string_lossy()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();
    fs::create_dir(fixture.root.join(".cargo")).unwrap();
    fs::write(
        fixture.root.join(".cargo/config.toml"),
        format!("[build]\nrustc-wrapper = {:?}\n", wrapper.to_string_lossy()),
    )
    .unwrap();

    let initial = fixture.build();
    assert_success(&initial);
    assert!(
        probe.is_file(),
        "Cargo's configured wrapper was not invoked"
    );
    assert!(!fixture.state_directory().join("build").exists());

    fixture.write_ordinary_source("two");
    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
    assert!(!stderr(&rebuilt).contains("Cinder patched"));
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
fn unselected_multi_artifact_source_changes_stay_on_cargo() {
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

#[test]
fn usage_collection_is_invisible_to_cargo_and_fast_run_programs() {
    let fixture = Fixture::new();
    fixture.write_usage_environment_source();

    let cargo_run = fixture.run_with_environment("unused", "CINDER_USAGE", "1");
    assert_success(&cargo_run);
    assert_eq!(stdout(&cargo_run), "absent|absent");

    let fast_run = fixture.run_with_environment("unused", "CINDER_USAGE", "1");
    assert_success(&fast_run);
    assert_eq!(stdout(&fast_run), "absent|absent");
    assert!(
        stderr(&fast_run).contains("Cinder restored")
            || stderr(&fast_run).contains("Cinder reusing"),
        "usage evidence changed the fast-run context:\n{}",
        stderr(&fast_run)
    );
}

#[test]
fn compiler_capture_environment_is_invisible_to_built_programs() {
    let fixture = Fixture::new();
    fixture.write_wrapper_environment_source();

    let cargo_run = fixture.run_with_environment("unused", "CINDER_EXPERIMENTAL_DIRECT_CHECK", "1");
    assert_success(&cargo_run);
    assert_eq!(
        stdout(&cargo_run),
        "absent|absent|absent|absent|absent|absent|absent"
    );

    let fast_run = fixture.run_with_environment("unused", "CINDER_EXPERIMENTAL_DIRECT_CHECK", "1");
    assert_success(&fast_run);
    assert_eq!(
        stdout(&fast_run),
        "absent|absent|absent|absent|absent|absent|absent"
    );
    assert!(
        stderr(&fast_run).contains("Cinder restored")
            || stderr(&fast_run).contains("Cinder reusing")
    );
}

#[test]
fn cinder_specific_capture_environment_is_invisible_to_build_scripts() {
    let fixture = Fixture::new();
    fixture.write_ordinary_source("one");
    let probe = fixture.root.join("build-script-environment");
    fs::write(
        fixture.root.join("build.rs"),
        r#"fn main() {
    let keys = [
        "RUSTC_WRAPPER",
        "CINDER_ARTIFACT_RECEIPT_DIRECTORY",
        "CINDER_RUSTC_WRAPPER_MODE",
        "CINDER_WRAPPER_ACTIVE",
        "CINDER_CAPTURE_COMPILER_RECIPE",
        "CINDER_REAL_CARGO",
        "CINDER_SYNCHRONOUS_STATE_RECORDING",
    ];
    let values = keys.map(|key| std::env::var(key).unwrap_or_else(|_| "absent".to_owned()));
    let output = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("build-script-environment");
    std::fs::write(output, values.join("|")).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
"#,
    )
    .unwrap();

    let built = fixture.build();
    assert_success(&built);
    let observed = fs::read_to_string(probe).unwrap();
    assert_eq!(observed, "absent|absent|absent|absent|absent|absent|absent");
}

#[test]
fn transitive_build_script_inputs_invalidate_selected_check_reuse() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let manifest = fs::read_to_string(fixture.root.join("Cargo.toml")).unwrap();
    fs::write(
        fixture.root.join("Cargo.toml"),
        format!(
            "{manifest}\n[dependencies]\ngenerated-dependency = {{ path = \"generated-dependency\" }}\n"
        ),
    )
    .unwrap();
    let dependency = fixture.root.join("generated-dependency");
    fs::create_dir_all(dependency.join("src")).unwrap();
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"generated-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("build.rs"),
        r#"fn main() {
    let value = std::fs::read_to_string("build-input.txt").unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
        .join("generated.rs");
    std::fs::write(output, format!("pub const VALUE: &str = {:?};\n", value.trim())).unwrap();
    println!("cargo:rerun-if-changed=build-input.txt");
}
"#,
    )
    .unwrap();
    fs::write(
        dependency.join("src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n",
    )
    .unwrap();
    fs::write(dependency.join("build-input.txt"), "one\n").unwrap();
    fs::write(
        &fixture.source,
        "pub fn generated() -> &'static str { generated_dependency::VALUE }\n",
    )
    .unwrap();

    let initial = fixture.check_selected_lib();
    assert_success(&initial);
    let reused = fixture.check_selected_lib();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    // Force a new receipt while Cargo considers the dependency build script
    // fresh. Cargo's JSON stream must still carry enough graph state for the
    // next Cinder decision to retain the transitive watch.
    fs::remove_dir_all(fixture.state_directory().join("check")).unwrap();
    fs::write(
        &fixture.source,
        "pub fn generated() -> &'static str { generated_dependency::VALUE }\npub fn marker() -> i32 { 1 }\n",
    )
    .unwrap();
    let fresh_dependency = fixture.check_selected_lib();
    assert_success(&fresh_dependency);
    assert!(stderr(&fresh_dependency).contains("Checking cinder-fast-run-fixture"));
    assert!(fixture.state_directory().join("check").is_dir());

    fs::write(dependency.join("build-input.txt"), "two\n").unwrap();
    let changed = fixture.check_selected_lib();
    assert_success(&changed);
    assert!(
        !stderr(&changed).contains("Cinder reused"),
        "a transitive build-script input reused stale selected output:\n{}",
        stderr(&changed)
    );
    assert!(stderr(&changed).contains("generated-dependency"));

    let refreshed = fixture.check_selected_lib();
    assert_success(&refreshed);
    assert!(stderr(&refreshed).contains("Cinder reused the validated check"));
}

#[test]
fn transitive_build_script_does_not_require_a_selected_package_out_directory() {
    let fixture = Fixture::new();
    let manifest = fs::read_to_string(fixture.root.join("Cargo.toml")).unwrap();
    fs::write(
        fixture.root.join("Cargo.toml"),
        format!(
            "{manifest}\n[dependencies]\ngenerated-dependency = {{ path = \"generated-dependency\" }}\n"
        ),
    )
    .unwrap();
    let dependency = fixture.root.join("generated-dependency");
    fs::create_dir_all(dependency.join("src")).unwrap();
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"generated-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("build.rs"),
        r#"fn main() {
    let value = std::fs::read_to_string("build-input.txt").unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
        .join("generated.rs");
    std::fs::write(output, format!("pub const VALUE: &str = {:?};\n", value.trim())).unwrap();
    println!("cargo:rerun-if-changed=build-input.txt");
}
"#,
    )
    .unwrap();
    fs::write(
        dependency.join("src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n",
    )
    .unwrap();
    fs::write(dependency.join("build-input.txt"), "one\n").unwrap();
    fs::write(
        &fixture.source,
        "fn main() { println!(\"{}\", generated_dependency::VALUE); }\n",
    )
    .unwrap();

    let initial = fixture.build();
    assert_success(&initial);
    let reused = fixture.build();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused"),
        "a selected package without its own build script was not reusable:\n{}",
        stderr(&reused)
    );

    fs::write(dependency.join("build-input.txt"), "two\n").unwrap();
    let changed = fixture.build();
    assert_success(&changed);
    assert!(
        !stderr(&changed).contains("Cinder reused"),
        "a changed transitive build-script input reused stale output:\n{}",
        stderr(&changed)
    );
    assert!(stderr(&changed).contains("generated-dependency"));
}

#[test]
fn external_path_dependency_sources_invalidate_selected_check_reuse() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let dependency_name = format!(
        "{}-external-dependency",
        fixture.root.file_name().unwrap().to_string_lossy()
    );
    let dependency = fixture.root.parent().unwrap().join(&dependency_name);
    fs::create_dir_all(dependency.join("src")).unwrap();
    let dependency_manifest =
        "[package]\nname = \"external-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    fs::write(dependency.join("Cargo.toml"), dependency_manifest).unwrap();
    fs::write(dependency.join("src/lib.rs"), "pub const VALUE: u32 = 1;\n").unwrap();
    let manifest = fs::read_to_string(fixture.root.join("Cargo.toml")).unwrap();
    fs::write(
        fixture.root.join("Cargo.toml"),
        format!(
            "{manifest}\n[dependencies]\nexternal-dependency = {{ path = \"../{dependency_name}\" }}\n"
        ),
    )
    .unwrap();
    fs::write(
        &fixture.source,
        "pub fn value() -> u32 { external_dependency::VALUE }\n",
    )
    .unwrap();

    let initial = fixture.check_selected_lib();
    assert_success(&initial);
    let reused = fixture.check_selected_lib();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    let dependency_artifacts = fs::read_dir(fixture.root.join("target/debug/deps"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension() == Some(OsStr::new("rmeta"))
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with("libexternal_dependency-"))
        })
        .collect::<Vec<_>>();
    let [dependency_artifact] = dependency_artifacts.as_slice() else {
        panic!("expected one external dependency artifact, found {dependency_artifacts:?}");
    };
    fs::remove_file(dependency_artifact).unwrap();
    let missing_dependency_artifact = fixture.check_selected_lib();
    assert_success(&missing_dependency_artifact);
    assert!(
        !stderr(&missing_dependency_artifact).contains("Cinder reused"),
        "a missing dependency artifact reused stale selected state:\n{}",
        stderr(&missing_dependency_artifact)
    );
    assert!(stderr(&missing_dependency_artifact).contains("Checking external-dependency"));

    let dependency_fingerprints = fs::read_dir(fixture.root.join("target/debug/.fingerprint"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with("external-dependency-"))
        })
        .collect::<Vec<_>>();
    let [dependency_fingerprint] = dependency_fingerprints.as_slice() else {
        panic!("expected one external dependency fingerprint, found {dependency_fingerprints:?}");
    };
    let fingerprint_markers = fs::read_dir(dependency_fingerprint)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name == "lib-external_dependency")
        })
        .collect::<Vec<_>>();
    let [fingerprint_marker] = fingerprint_markers.as_slice() else {
        panic!(
            "expected one external dependency fingerprint marker, found {fingerprint_markers:?}"
        );
    };
    fs::remove_file(fingerprint_marker).unwrap();
    let missing_fingerprint_marker = fixture.check_selected_lib();
    assert_success(&missing_fingerprint_marker);
    assert!(
        !stderr(&missing_fingerprint_marker).contains("Cinder reused"),
        "a missing dependency fingerprint marker reused stale selected state:\n{}",
        stderr(&missing_fingerprint_marker)
    );
    assert!(stderr(&missing_fingerprint_marker).contains("Checking external-dependency"));

    // Repairing the marker takes Cargo more than one pass to settle: the pass
    // after the rebuild still re-checks the dependency. The diagnostic-replay
    // recorder proves that with a hidden no-change pass and abandons the
    // not-yet-quiet state, so reuse resumes only once Cargo converges.
    let converging = fixture.check_selected_lib();
    assert_success(&converging);
    let repaired_fingerprint = fixture.check_selected_lib();
    assert_success(&repaired_fingerprint);
    assert!(stderr(&repaired_fingerprint).contains("Cinder reused the validated check"));
    fs::write(fingerprint_marker, b"0000000000000000").unwrap();
    let modified_fingerprint_marker = fixture.check_selected_lib();
    assert_success(&modified_fingerprint_marker);
    assert!(
        !stderr(&modified_fingerprint_marker).contains("Cinder reused"),
        "a modified dependency fingerprint marker reused stale selected state:\n{}",
        stderr(&modified_fingerprint_marker)
    );
    assert!(stderr(&modified_fingerprint_marker).contains("Checking external-dependency"));

    fs::write(
        dependency.join("Cargo.toml"),
        format!("{dependency_manifest}\n[lib]\npath = \"src/missing.rs\"\n"),
    )
    .unwrap();
    let manifest_changed = fixture.check_selected_lib();
    assert!(
        !manifest_changed.status.success(),
        "an external path dependency manifest change reused stale selected output:\n{}",
        stderr(&manifest_changed)
    );
    assert!(!stderr(&manifest_changed).contains("Cinder reused"));
    assert!(stderr(&manifest_changed).contains("src/missing.rs"));

    fs::write(dependency.join("Cargo.toml"), dependency_manifest).unwrap();
    let manifest_restored = fixture.check_selected_lib();
    assert_success(&manifest_restored);
    assert!(!stderr(&manifest_restored).contains("Cinder reused"));

    let dependency_files = fs::read_dir(fixture.root.join("target/debug/deps"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension() == Some(OsStr::new("d"))
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with("external_dependency-"))
        })
        .collect::<Vec<_>>();
    let [dependency_file] = dependency_files.as_slice() else {
        panic!("expected one external dependency file, found {dependency_files:?}");
    };
    fs::remove_file(dependency_file).unwrap();
    let missing_dependency_file = fixture.check_selected_lib();
    assert_success(&missing_dependency_file);
    assert!(
        !stderr(&missing_dependency_file).contains("Cinder reused"),
        "missing dependency graph output reused stale selected state:\n{}",
        stderr(&missing_dependency_file)
    );
    let recaptured_without_dep_info = fixture.check_selected_lib();
    assert_success(&recaptured_without_dep_info);
    assert!(
        stderr(&recaptured_without_dep_info).contains("Cinder reused the validated check"),
        "Cargo fingerprint dependency data did not preserve reusable state:\n{}",
        stderr(&recaptured_without_dep_info)
    );
    fs::write(
        dependency.join("src/lib.rs"),
        "pub const VALUE: &str = \"changed without dep-info\";\n",
    )
    .unwrap();
    let changed_without_dep_info = fixture.check_selected_lib();
    assert!(
        !changed_without_dep_info.status.success(),
        "an external dependency source change reused encoded graph state:\n{}",
        stderr(&changed_without_dep_info)
    );
    assert!(!stderr(&changed_without_dep_info).contains("Cinder reused"));
    assert!(stderr(&changed_without_dep_info).contains("mismatched types"));
    fs::write(dependency.join("src/lib.rs"), "pub const VALUE: u32 = 1;\n").unwrap();
    let restored_without_dep_info = fixture.check_selected_lib();
    assert_success(&restored_without_dep_info);
    assert!(!stderr(&restored_without_dep_info).contains("Cinder reused"));

    let dependency_artifacts = fs::read_dir(fixture.root.join("target/debug/deps"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension() == Some(OsStr::new("rmeta"))
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with("libexternal_dependency-"))
        })
        .collect::<Vec<_>>();
    let [dependency_artifact] = dependency_artifacts.as_slice() else {
        panic!("expected one external dependency artifact, found {dependency_artifacts:?}");
    };
    fs::remove_file(dependency_artifact).unwrap();
    let missing_artifact_without_dep_info = fixture.check_selected_lib();
    assert_success(&missing_artifact_without_dep_info);
    assert!(
        !stderr(&missing_artifact_without_dep_info).contains("Cinder reused"),
        "a missing dependency artifact escaped encoded graph validation:\n{}",
        stderr(&missing_artifact_without_dep_info)
    );
    assert!(stderr(&missing_artifact_without_dep_info).contains("Checking external-dependency"));

    fs::write(dependency.join("build.rs"), "fn main() {}\n").unwrap();
    let new_build_script = fixture.check_selected_lib();
    assert_success(&new_build_script);
    assert!(
        !stderr(&new_build_script).contains("Cinder reused"),
        "a new external dependency build script reused stale selected output:\n{}",
        stderr(&new_build_script)
    );
    fs::remove_file(dependency.join("build.rs")).unwrap();
    let removed_build_script = fixture.check_selected_lib();
    assert_success(&removed_build_script);
    assert!(!stderr(&removed_build_script).contains("Cinder reused"));

    fs::write(
        dependency.join("src/lib.rs"),
        "pub const VALUE: &str = \"changed\";\n",
    )
    .unwrap();
    let changed = fixture.check_selected_lib();
    assert!(
        !changed.status.success(),
        "an external path dependency change reused stale selected output:\n{}",
        stderr(&changed)
    );
    assert!(!stderr(&changed).contains("Cinder reused"));
    assert!(stderr(&changed).contains("mismatched types"));
    fs::remove_dir_all(dependency).unwrap();
}

#[test]
fn encoded_dependency_inputs_preserve_non_rust_watches_when_dep_info_is_missing() {
    let fixture = Fixture::new_library_with_crate_types("\"rlib\"");
    let dependency_name = format!(
        "{}-encoded-dependency",
        fixture.root.file_name().unwrap().to_string_lossy()
    );
    let dependency = fixture.root.parent().unwrap().join(&dependency_name);
    fs::create_dir_all(dependency.join("src")).unwrap();
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"encoded-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("src/lib.rs"),
        "pub const VALUE: &[u8; 1] = include_bytes!(\"value.bin\");\n",
    )
    .unwrap();
    fs::write(dependency.join("src/value.bin"), b"1").unwrap();
    let manifest = fs::read_to_string(fixture.root.join("Cargo.toml")).unwrap();
    fs::write(
        fixture.root.join("Cargo.toml"),
        format!(
            "{manifest}\n[dependencies]\nencoded-dependency = {{ path = \"../{dependency_name}\" }}\n"
        ),
    )
    .unwrap();
    fs::write(
        &fixture.source,
        "pub fn value() -> u8 { encoded_dependency::VALUE[0] }\n",
    )
    .unwrap();

    assert_success(&fixture.check_selected_lib());
    let reused = fixture.check_selected_lib();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    let dependency_files = fs::read_dir(fixture.root.join("target/debug/deps"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension() == Some(OsStr::new("d"))
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with("encoded_dependency-"))
        })
        .collect::<Vec<_>>();
    let [dependency_file] = dependency_files.as_slice() else {
        panic!("expected one encoded dependency file, found {dependency_files:?}");
    };
    fs::remove_file(dependency_file).unwrap();
    let missing_dependency_file = fixture.check_selected_lib();
    assert_success(&missing_dependency_file);
    assert!(!stderr(&missing_dependency_file).contains("Cinder reused"));

    fs::write(dependency.join("src/value.bin"), b"12").unwrap();
    let changed = fixture.check_selected_lib();
    assert!(
        !changed.status.success(),
        "an encoded non-Rust input change reused stale selected output:\n{}",
        stderr(&changed)
    );
    assert!(!stderr(&changed).contains("Cinder reused"));
    assert!(stderr(&changed).contains("mismatched types"));
    fs::remove_dir_all(dependency).unwrap();
}

#[test]
fn cargo_message_capture_preserves_human_diagnostics() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.source,
        "fn main() { let deliberately_unused = 3; }\n",
    )
    .unwrap();

    let warned = fixture.check_selected_bin();
    assert_success(&warned);
    assert!(stdout(&warned).is_empty());
    assert!(stderr(&warned).contains("unused variable: `deliberately_unused`"));
    assert!(!stderr(&warned).contains("\"reason\":\"compiler-message\""));

    fs::write(&fixture.source, "fn main( {\n").unwrap();
    let failed = fixture.check_selected_bin();
    assert!(!failed.status.success());
    assert!(stdout(&failed).is_empty());
    assert!(stderr(&failed).contains("could not compile `cinder-fast-run-fixture`"));
    assert!(!stderr(&failed).contains("\"reason\":\"compiler-message\""));
}

#[test]
fn cargo_run_forwards_json_shaped_program_output_after_build_completion() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.source,
        "fn main() { println!(\"{{\\\"reason\\\":\\\"compiler-artifact\\\",\\\"program\\\":true}}\"); println!(\"tail\"); }\n",
    )
    .unwrap();

    let output = fixture.run("unused");
    assert_success(&output);
    assert_eq!(
        stdout(&output),
        "{\"reason\":\"compiler-artifact\",\"program\":true}\ntail"
    );
}

#[test]
fn touched_but_identical_sources_still_reuse_the_validated_check() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "fn main() {}\n").unwrap();

    assert_success(&fixture.check_selected_bin());
    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    // An editor save that rewrites identical bytes changes the file identity
    // but not the revision; the hit re-reads only that file and still reuses.
    let contents = fs::read(&fixture.source).unwrap();
    fs::write(&fixture.source, &contents).unwrap();
    let touched = fixture.check_selected_bin();
    assert_success(&touched);
    assert!(
        stderr(&touched).contains("Cinder reused the validated check"),
        "a touched-but-identical source must still reuse:\n{}",
        stderr(&touched)
    );
}

#[test]
fn same_length_source_changes_outside_literals_invalidate_check_reuse() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "fn main() { let value = 5; }\n").unwrap();

    assert_success(&fixture.check_selected_bin());
    assert!(stderr(&fixture.check_selected_bin()).contains("Cinder reused the validated check"));

    // Same byte length, different content, outside any string literal.
    fs::write(&fixture.source, "fn main() { let value = 7; }\n").unwrap();
    let changed = fixture.check_selected_bin();
    assert_success(&changed);
    assert!(
        !stderr(&changed).contains("Cinder reused the validated check"),
        "a changed source must not reuse:\n{}",
        stderr(&changed)
    );
    assert!(stderr(&changed).contains("Checking"));
}

#[test]
fn check_reuse_replays_recorded_cargo_warnings() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "fn main() { let unused = 5; }\n").unwrap();

    let initial = fixture.check_selected_bin();
    assert_success(&initial);
    assert!(stderr(&initial).contains("unused variable: `unused`"));

    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    let errors = stderr(&reused);
    assert!(
        errors.contains("Cinder reused the validated check"),
        "no-change check with a warning did not reuse:\n{errors}"
    );
    assert!(
        errors.contains("unused variable: `unused`") && errors.contains("generated 1 warning"),
        "reuse hit did not replay Cargo's cached warning:\n{errors}"
    );
    assert!(
        errors.find("unused variable").unwrap() < errors.find("Cinder reused").unwrap(),
        "warning replay must precede the Cinder marker:\n{errors}"
    );

    fs::write(&fixture.source, "fn main() { let _used = 5; }\n").unwrap();
    let fixed = fixture.check_selected_bin();
    assert_success(&fixed);
    let clean = fixture.check_selected_bin();
    assert_success(&clean);
    let errors = stderr(&clean);
    assert!(errors.contains("Cinder reused the validated check"));
    assert!(
        !errors.contains("warning"),
        "a fixed warning must not be replayed:\n{errors}"
    );
}

#[test]
fn asynchronously_recorded_check_state_replays_warnings() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "fn main() { let unused = 5; }\n").unwrap();

    let initial = fixture.check_selected_targets_with_recording(
        "check",
        &["--bin", "cinder-fast-run-fixture"],
        None,
        false,
    );
    assert_success(&initial);
    fixture.wait_for_state("check");

    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    let errors = stderr(&reused);
    assert!(
        errors.contains("Cinder reused the validated check")
            && errors.contains("unused variable: `unused`")
            && errors.contains("generated 1 warning"),
        "asynchronously recorded state did not replay Cargo's warning:\n{errors}"
    );
}

#[test]
fn build_reuse_replays_recorded_cargo_warnings() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.source,
        "fn main() { let unused = 5; println!(\"built\"); }\n",
    )
    .unwrap();

    let initial = fixture.build();
    assert_success(&initial);
    let reused = fixture.build();
    assert_success(&reused);
    let errors = stderr(&reused);
    assert!(
        errors.contains("Cinder reused")
            && errors.contains("unused variable: `unused`")
            && errors.contains("generated 1 warning"),
        "no-change build did not replay Cargo's cached warning:\ninitial:\n{}\nreused:\n{errors}",
        stderr(&initial)
    );
}

#[test]
fn direct_test_execution_replays_recorded_cargo_warnings() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "fn main() {}\n").unwrap();
    fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn touched() -> u32 { let unused = 5; 7 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn touches_the_library() { assert_eq!(crate::touched(), 7); }\n}\n",
    )
    .unwrap();

    let initial = fixture.test_selected_lib();
    assert_success(&initial);
    assert!(stderr(&initial).contains("unused variable: `unused`"));

    let reused = fixture.test_selected_lib();
    assert_success(&reused);
    let errors = stderr(&reused);
    assert!(
        errors.contains("Cinder running the validated test executable"),
        "no-change test with a warning did not run directly:\n{errors}"
    );
    assert!(
        errors.contains("unused variable: `unused`"),
        "direct test execution did not replay Cargo's cached warning:\n{errors}"
    );
    assert!(
        errors.find("unused variable").unwrap()
            < errors
                .find("Cinder running the validated test executable")
                .unwrap(),
        "warning replay must precede the Cinder marker:\n{errors}"
    );
    assert!(stdout(&reused).contains("running 1 test"));
    assert!(stdout(&reused).contains("test result: ok. 1 passed"));
}

#[test]
fn recorded_warnings_keep_string_patches_on_cargo() {
    let fixture = Fixture::new();
    let warned_source = |value: &str| {
        format!(
            "fn main() {{ let unused = 5; println!(\"{{}} {{}}\", env!(\"CINDER_FIXTURE_VALUE\"), \"ordinary-{value}\"); }}\n"
        )
    };
    fs::write(&fixture.source, warned_source("one")).unwrap();

    let initial = fixture.run("alpha");
    assert_success(&initial);
    assert_eq!(stdout(&initial), "alpha ordinary-one");

    fs::write(&fixture.source, warned_source("two")).unwrap();
    let fallback = fixture.run("alpha");
    assert_success(&fallback);
    assert_eq!(stdout(&fallback), "alpha ordinary-two");
    assert!(
        !stderr(&fallback).contains("Cinder patched"),
        "a state with recorded warnings must not be patched:\n{}",
        stderr(&fallback)
    );
}

#[test]
fn run_reuse_replays_recorded_cargo_warnings() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.source,
        "fn main() { let unused = 5; println!(\"{}\", env!(\"CINDER_FIXTURE_VALUE\")); }\n",
    )
    .unwrap();

    let initial = fixture.run("direct");
    assert_success(&initial);
    let restored = fixture.run("direct");
    assert_success(&restored);
    let errors = stderr(&restored);
    assert!(
        errors.contains("Cinder restored") && errors.contains("unused variable: `unused`"),
        "restored run did not replay Cargo's cached warning:\n{errors}"
    );

    let reused = fixture.run("direct");
    assert_success(&reused);
    let errors = stderr(&reused);
    assert!(
        errors.contains("Cinder reusing") && errors.contains("unused variable: `unused`"),
        "direct no-change run did not replay Cargo's cached warning:\n{errors}"
    );
}

#[test]
fn pinned_color_replays_exact_ansi_warning_bytes() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "fn main() { let unused = 5; }\n").unwrap();
    let check = |fixture: &Fixture| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&fixture.root)
            .args(["check", "--bin", "cinder-fast-run-fixture"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TERM_COLOR", "always")
            .env("CARGO_TARGET_DIR", fixture.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .output()
            .unwrap()
    };

    assert_success(&check(&fixture));
    let reused = check(&fixture);
    assert_success(&reused);
    let errors = stderr(&reused);
    assert!(
        errors.contains("Cinder reused the validated check"),
        "pinned-color check did not reuse:\n{errors}"
    );
    assert!(
        errors.contains('\u{1b}') && errors.contains("unused variable"),
        "pinned always color must replay Cargo's ANSI warning bytes:\n{errors}"
    );
}

#[test]
fn multi_target_build_reuses_and_refuses_string_patch() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.source,
        "fn main() { println!(\"{} {}\", env!(\"CINDER_FIXTURE_VALUE\"), \"ordinary-one\"); }\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn value() -> &'static str { \"library-one\" }\n",
    )
    .unwrap();

    assert_success(&fixture.build());
    assert!(fixture.state_directory().join("build").exists());
    let reused = fixture.build();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated build of 2 targets"),
        "multi-target no-change build did not reuse:\n{}",
        stderr(&reused)
    );

    // An equal-length literal edit is patchable for a single executable, but a
    // multi-root state must stay on Cargo.
    fs::write(
        &fixture.source,
        "fn main() { println!(\"{} {}\", env!(\"CINDER_FIXTURE_VALUE\"), \"ordinary-two\"); }\n",
    )
    .unwrap();
    let rebuilt = fixture.build();
    assert_success(&rebuilt);
    assert!(
        !stderr(&rebuilt).contains("Cinder patched"),
        "a multi-root build state must not be patched:\n{}",
        stderr(&rebuilt)
    );
    assert!(stderr(&rebuilt).contains("Compiling cinder-fast-run-fixture"));
    let reconverged = fixture.build();
    assert_success(&reconverged);
    assert!(stderr(&reconverged).contains("Cinder reused the validated build of 2 targets"));
}

fn workspace_member(root: &Path, name: &str, build_script: bool, library: &str) {
    let member = root.join(name);
    fs::create_dir_all(member.join("src")).unwrap();
    let mut manifest =
        format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n");
    if build_script {
        manifest.push_str("build = \"build.rs\"\n");
        fs::write(member.join("build.rs"), "fn main() {}\n").unwrap();
    }
    fs::write(member.join("Cargo.toml"), manifest).unwrap();
    fs::write(member.join("src/lib.rs"), library).unwrap();
}

#[test]
fn workspace_root_check_reuses_every_member() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-workspace-root-check-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    workspace_member(&root, "alpha", true, "pub fn alpha() -> u32 { 1 }\n");
    workspace_member(
        &root,
        "beta",
        false,
        "pub fn beta() -> u32 { let unused = 5; 2 }\n",
    );
    let fixture = Fixture {
        source: root.join("beta/src/lib.rs"),
        root,
        package: None,
    };

    let initial = fixture.check_default();
    assert_success(&initial);
    // A build-script package reports `Compiling`, not `Checking`.
    assert!(stderr(&initial).contains("alpha v0.0.0"));
    assert!(stderr(&initial).contains("Checking beta"));
    assert!(fixture.state_directory().join("check").exists());

    let reused = fixture.check_default();
    assert_success(&reused);
    let errors = stderr(&reused);
    assert!(
        errors.contains("Cinder reused the validated check of 2 targets"),
        "workspace-root no-change check did not reuse:\n{errors}"
    );
    assert!(
        errors.contains("unused variable: `unused`"),
        "workspace hit did not replay the member warning:\n{errors}"
    );

    // Editing one member invalidates the whole recorded set.
    fs::write(
        &fixture.source,
        "pub fn beta() -> u32 { let unused = 5; 3 }\n",
    )
    .unwrap();
    let changed = fixture.check_default();
    assert_success(&changed);
    assert!(!stderr(&changed).contains("Cinder reused"));
    assert!(stderr(&changed).contains("Checking beta"));
    let reconverged = fixture.check_default();
    assert_success(&reconverged);
    assert!(stderr(&reconverged).contains("Cinder reused the validated check of 2 targets"));

    // A new member changes the selected set through project topology.
    workspace_member(
        &fixture.root,
        "gamma",
        false,
        "pub fn gamma() -> u32 { 4 }\n",
    );
    fs::write(
        fixture.root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"alpha\", \"beta\", \"gamma\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    let extended = fixture.check_default();
    assert_success(&extended);
    assert!(!stderr(&extended).contains("Cinder reused"));
    assert!(stderr(&extended).contains("Checking gamma"));
    let extended_reuse = fixture.check_default();
    assert_success(&extended_reuse);
    assert!(
        stderr(&extended_reuse).contains("Cinder reused the validated check of 3 targets"),
        "extended workspace did not reconverge:\n{}",
        stderr(&extended_reuse)
    );
}

#[test]
fn out_of_root_workspace_members_disable_capture() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let outer = std::env::temp_dir().join(format!(
        "cinder-out-of-root-member-{}-{nonce}",
        std::process::id()
    ));
    let root = outer.join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"alpha\", \"../shared\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    workspace_member(&root, "alpha", false, "pub fn alpha() -> u32 { 1 }\n");
    workspace_member(&outer, "shared", false, "pub fn shared() -> u32 { 2 }\n");
    let fixture = Fixture {
        source: outer.join("shared/src/lib.rs"),
        root,
        package: None,
    };

    // Current Cargo rejects members that are not hierarchically below the
    // root; the error must pass through unchanged and no state may exist.
    // The classifier additionally blocks capture for any Cargo that would
    // accept the declaration.
    let initial = fixture.check_default();
    assert!(!initial.status.success());
    assert!(stderr(&initial).contains("not hierarchically below"));
    assert!(
        !fixture.state_directory().join("check").exists(),
        "an out-of-root member declaration must never record workspace state"
    );
}

#[test]
fn symlinked_workspace_members_disable_capture() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let outer = std::env::temp_dir().join(format!(
        "cinder-symlink-member-{}-{nonce}",
        std::process::id()
    ));
    let root = outer.join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"alpha\", \"linked\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    workspace_member(&root, "alpha", false, "pub fn alpha() -> u32 { 1 }\n");
    workspace_member(&outer, "escaped", false, "pub fn escaped() -> u32 { 2 }\n");
    std::os::unix::fs::symlink(outer.join("escaped"), root.join("linked")).unwrap();
    let fixture = Fixture {
        source: outer.join("escaped/src/lib.rs"),
        root,
        package: None,
    };

    // Whether Cargo accepts or rejects the symlinked member, Cinder must
    // never record a workspace state whose member set it cannot prove
    // in-root: the escaped member's canonical sources would be invisible to
    // later validation.
    let initial = fixture.check_default();
    assert!(
        !fixture.state_directory().join("check").exists(),
        "a symlinked out-of-root member must never record workspace state:\n{}",
        stderr(&initial)
    );
    if initial.status.success() {
        let repeat = fixture.check_default();
        assert_success(&repeat);
        assert!(!stderr(&repeat).contains("Cinder reused"));
    }
}

#[test]
fn procedural_macro_members_are_validated_workspace_check_roots() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-proc-macro-member-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"normal\", \"macros\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    workspace_member(&root, "normal", false, "pub fn normal() -> u32 { 1 }\n");
    let macros = root.join("macros");
    fs::create_dir_all(macros.join("src")).unwrap();
    fs::write(
        macros.join("Cargo.toml"),
        "[package]\nname = \"macros\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n",
    )
    .unwrap();
    fs::write(
        macros.join("src/lib.rs"),
        "use proc_macro::TokenStream;\n#[proc_macro]\npub fn noop(input: TokenStream) -> TokenStream { input }\n",
    )
    .unwrap();
    let fixture = Fixture {
        source: macros.join("src/lib.rs"),
        root,
        package: None,
    };

    let initial = fixture.check_default();
    assert_success(&initial);
    assert!(stderr(&initial).contains("macros"));

    let reused = fixture.check_default();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check of 2 targets"),
        "a proc-macro member must be an ordinary validated check root:\n{}",
        stderr(&reused)
    );

    // The proc-macro member's edits must invalidate and reach Cargo.
    fs::write(
        &fixture.source,
        "use proc_macro::TokenStream;\n#[proc_macro]\npub fn noop(input: TokenStream) -> TokenStream { let unused = 5; input }\n",
    )
    .unwrap();
    let edited = fixture.check_default();
    assert_success(&edited);
    assert!(!stderr(&edited).contains("Cinder reused"));
    assert!(stderr(&edited).contains("unused variable: `unused`"));

    // The warning replays on the re-recorded hit, and the state converges.
    let replayed = fixture.check_default();
    assert_success(&replayed);
    assert!(stderr(&replayed).contains("Cinder reused the validated check of 2 targets"));
    assert!(stderr(&replayed).contains("unused variable: `unused`"));

    // The experimental compiler replay must still refuse recipe publication
    // for a graph containing a proc-macro target, independent of receipts.
    fs::write(
        &fixture.source,
        "use proc_macro::TokenStream;\n#[proc_macro]\npub fn noop(input: TokenStream) -> TokenStream { let _used = 5; input }\n",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
    command.current_dir(&fixture.root).arg("check");
    command
        .env("CINDER_REAL_CARGO", "cargo")
        .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
        .env("CINDER_EXPERIMENTAL_DIRECT_CHECK", "1")
        .env("CINDER_TRACE_RUN", "1")
        .env("CARGO_TARGET_DIR", fixture.root.join("target"))
        .env_remove("CINDER_DISABLE_FAST_CHECK");
    let experimental = command.output().unwrap();
    assert_success(&experimental);
    assert!(
        stderr(&experimental)
            .contains("compiler replay disabled by unsupported Cargo target graph"),
        "a proc-macro member must keep recipe publication disabled:\n{}",
        stderr(&experimental)
    );
    assert!(!stderr(&experimental).contains("Cinder replayed"));
}

#[test]
fn multi_crate_type_members_still_disable_workspace_capture() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-multi-crate-type-member-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"normal\", \"mixed\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    workspace_member(&root, "normal", false, "pub fn normal() -> u32 { 1 }\n");
    let mixed = root.join("mixed");
    fs::create_dir_all(mixed.join("src")).unwrap();
    fs::write(
        mixed.join("Cargo.toml"),
        "[package]\nname = \"mixed\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\ncrate-type = [\"staticlib\", \"rlib\"]\n",
    )
    .unwrap();
    fs::write(mixed.join("src/lib.rs"), "pub fn mixed() -> u32 { 2 }\n").unwrap();
    let fixture = Fixture {
        source: mixed.join("src/lib.rs"),
        root,
        package: None,
    };

    let initial = fixture.check_default();
    assert_success(&initial);
    assert!(
        !fixture.state_directory().join("check").exists(),
        "a multi-crate-type member without a receipt must disable multi-root recording"
    );

    let repeat = fixture.check_default();
    assert_success(&repeat);
    assert!(!stderr(&repeat).contains("Cinder reused"));
}

#[test]
fn an_old_state_format_is_a_quiet_cargo_miss() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "fn main() {}\n").unwrap();

    assert_success(&fixture.check_selected_bin());
    let reused = fixture.check_selected_bin();
    assert_success(&reused);
    assert!(stderr(&reused).contains("Cinder reused the validated check"));

    // A sources file from a previous Cinder version has an older magic.
    let sources = fixture.state_directory().join("check/sources");
    assert!(sources.is_file());
    fs::write(&sources, b"CNDS0001stale-format-payload").unwrap();

    let miss = fixture.check_selected_bin();
    assert_success(&miss);
    let errors = stderr(&miss);
    assert!(
        !errors.contains("unavailable") && !errors.contains("unsupported format"),
        "an old state format must not surface a user-visible error:\n{errors}"
    );
    assert!(!errors.contains("Cinder reused"));
    assert!(errors.contains("Finished"));
}

#[test]
fn color_override_environments_keep_warning_states_on_cargo() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "fn main() { let unused = 5; }\n").unwrap();
    let check = |fixture: &Fixture| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&fixture.root)
            .args(["check", "--bin", "cinder-fast-run-fixture"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("NO_COLOR", "1")
            .env("CARGO_TARGET_DIR", fixture.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .output()
            .unwrap()
    };

    let initial = check(&fixture);
    assert_success(&initial);
    assert!(stderr(&initial).contains("unused variable: `unused`"));

    // The replay cannot be proven under NO_COLOR, so reuse stays on Cargo and
    // Cargo itself keeps replaying the warning.
    let repeat = check(&fixture);
    assert_success(&repeat);
    assert!(!stderr(&repeat).contains("Cinder reused"));
    assert!(stderr(&repeat).contains("unused variable: `unused`"));
}

#[test]
fn workspace_roots_with_target_selectors_stay_on_exact_package_rules() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-workspace-selector-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"rootpkg\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\nmembers = [\".\", \"extra\"]\ndefault-members = [\".\", \"extra\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn root() -> u32 { 1 }\n").unwrap();
    workspace_member(&root, "extra", false, "pub fn extra() -> u32 { 2 }\n");
    let fixture = Fixture {
        source: root.join("src/lib.rs"),
        root,
        package: None,
    };
    let check_lib = |fixture: &Fixture| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&fixture.root).args(["check", "--lib"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", fixture.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .output()
            .unwrap()
    };

    // A selector at a default-members workspace root must not enter workspace
    // capture; the shape stays entirely Cargo-owned.
    let initial = check_lib(&fixture);
    assert_success(&initial);
    assert!(
        !fixture.state_directory().join("check").exists(),
        "a selector shape at a default-members root must not record workspace state"
    );
    let repeat = check_lib(&fixture);
    assert_success(&repeat);
    assert!(!stderr(&repeat).contains("Cinder reused"));
}

#[test]
fn workspace_package_selection_reuses_a_multi_target_member() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-workspace-member-check-{}-{nonce}",
        std::process::id()
    ));
    let member = root.join("tool");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"tool\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"tool\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(member.join("src/lib.rs"), "pub fn value() -> u32 { 7 }\n").unwrap();
    fs::write(
        member.join("src/main.rs"),
        "fn main() { println!(\"{}\", tool::value()); }\n",
    )
    .unwrap();
    let fixture = Fixture {
        source: member.join("src/main.rs"),
        root,
        package: Some("tool"),
    };
    let check_selected = |fixture: &Fixture| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&fixture.root)
            .args(["check", "-p", "tool"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", fixture.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .output()
            .unwrap()
    };

    assert_success(&check_selected(&fixture));
    let reused = check_selected(&fixture);
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check of 2 targets"),
        "workspace-selected multi-target member did not reuse:\n{}",
        stderr(&reused)
    );

    fs::write(
        &fixture.source,
        "fn main() { println!(\"{}!\", tool::value()); }\n",
    )
    .unwrap();
    let changed = check_selected(&fixture);
    assert_success(&changed);
    assert!(!stderr(&changed).contains("Cinder reused"));
}

#[test]
fn explicit_default_members_keep_non_check_commands_on_cargo() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cinder-default-members-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        concat!(
            "[package]\nname = \"root-package\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n",
            "[workspace]\nmembers = [\"extra\"]\ndefault-members = [\".\", \"extra\"]\nresolver = \"3\"\n",
        ),
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    workspace_member(&root, "extra", false, "pub fn extra() -> u32 { 9 }\n");
    let fixture = Fixture {
        source: root.join("src/main.rs"),
        root,
        package: None,
    };

    // Single-package capture would record only the root package while Cargo
    // also builds `extra`; a later reuse could then return a stale result for
    // the other member. Build therefore stays entirely Cargo-owned.
    assert_success(&fixture.build());
    assert!(!fixture.state_directory().join("build").exists());
    let repeated = fixture.build();
    assert_success(&repeated);
    assert!(!stderr(&repeated).contains("Cinder reused"));
    assert!(!fixture.state_directory().join("build").exists());

    // Check uses workspace selection instead, validating every default member.
    assert_success(&fixture.check_default());
    let reused = fixture.check_default();
    assert_success(&reused);
    assert!(
        stderr(&reused).contains("Cinder reused the validated check of 2 targets"),
        "default-members workspace check did not select every member:\n{}",
        stderr(&reused)
    );

    // A stale `extra` must invalidate that workspace check state.
    fs::write(
        fixture.root.join("extra/src/lib.rs"),
        "pub fn extra() -> u32 { 10 }\n",
    )
    .unwrap();
    let changed = fixture.check_default();
    assert_success(&changed);
    assert!(!stderr(&changed).contains("Cinder reused"));
    assert!(stderr(&changed).contains("Checking extra"));
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
            "[package]\nname = \"cinder-fast-run-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\nname = \"cinder_fixture_app_lib\"\ncrate-type = [\"staticlib\", \"cdylib\", \"rlib\"]\n",
        )
        .unwrap();
        fs::write(
            &fixture.source,
            "fn main() { println!(\"{}\", cinder_fixture_app_lib::value()); }\n",
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

    fn write_library_runtime_test_source(&self, value: &str) {
        let package_root = if self.package.is_some() && self.root.join("app/Cargo.toml").is_file() {
            self.root.join("app")
        } else {
            self.root.clone()
        };
        fs::write(
            package_root.join("src/lib.rs"),
            format!(
                r#"pub fn value() -> &'static str {{ "library-{value}" }}

#[cfg(test)]
mod tests {{
    #[test]
    fn runtime_contract() {{
        assert_eq!(
            std::env::current_dir().unwrap(),
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        );
        assert!(std::path::Path::new(&std::env::args_os().next().unwrap()).is_absolute());
        assert!(std::env::var_os("CINDER_TRACE_RUN").is_none());
        assert!(std::env::var_os("CINDER_SYNCHRONOUS_STATE_RECORDING").is_none());
        let linker = std::env::var_os("DYLD_FALLBACK_LIBRARY_PATH")
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<unset>".to_owned());
        let linker_path = std::path::Path::new("target/cinder-runtime-linker");
        match std::fs::read_to_string(linker_path) {{
            Ok(recorded) => assert_eq!(linker, recorded),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {{
                std::fs::write(linker_path, &linker).unwrap();
            }}
            Err(error) => panic!("could not read runtime linker contract: {{error}}"),
        }}
        let result = std::fs::read_to_string("target/cinder-runtime-result").unwrap();
        let count_path = std::path::Path::new("target/cinder-runtime-count");
        let count = std::fs::read_to_string(count_path)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0)
            + 1;
        std::fs::write(count_path, count.to_string()).unwrap();
        println!("cinder-runtime-{value}-{{}}", result.trim());
        assert_eq!(result.trim(), "pass");
    }}
}}
"#
            ),
        )
        .unwrap();
    }

    fn enable_test_runner(&self, probe: &Path) {
        let runner = self.root.join("test-runner.sh");
        fs::write(
            &runner,
            "#!/bin/sh\nprobe=$1\nshift\nprintf 'run\\n' >> \"$probe\"\nexec \"$@\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&runner).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&runner, permissions).unwrap();
        fs::create_dir_all(self.root.join(".cargo")).unwrap();
        fs::write(
            self.root.join(".cargo/config.toml"),
            format!(
                "[target.'cfg(unix)']\nrunner = [{:?}, {:?}]\n",
                runner.to_string_lossy(),
                probe.to_string_lossy()
            ),
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

    fn write_integration_target_tmpdir_source(&self, value: i32) {
        let library = self.root.join("src/lib.rs");
        if !library.is_file() {
            fs::write(library, "pub fn value() -> i32 { 1 }\n").unwrap();
        }
        fs::create_dir_all(self.root.join("tests")).unwrap();
        fs::write(
            self.root.join("tests/smoke.rs"),
            format!(
                "pub const EDIT: i32 = {value};\npub const CARGO_TMP: Option<&str> = option_env!(\"CARGO_TARGET_TMPDIR\");\n#[test]\nfn smoke() {{ assert!(CARGO_TMP.is_some()); }}\n"
            ),
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

    fn write_usage_environment_source(&self) {
        fs::write(
            &self.source,
            "fn main() { println!(\"{}|{}\", option_env!(\"CINDER_USAGE\").unwrap_or(\"absent\"), std::env::var(\"CINDER_USAGE\").unwrap_or_else(|_| \"absent\".to_owned())); }\n",
        )
        .unwrap();
    }

    fn write_wrapper_environment_source(&self) {
        fs::write(
            &self.source,
            "fn main() { println!(\"{}|{}|{}|{}|{}|{}|{}\", option_env!(\"RUSTC_WRAPPER\").unwrap_or(\"absent\"), option_env!(\"CINDER_WRAPPER_ACTIVE\").unwrap_or(\"absent\"), option_env!(\"CINDER_ARTIFACT_RECEIPT_DIRECTORY\").unwrap_or(\"absent\"), option_env!(\"RUSTC_WORKSPACE_WRAPPER\").unwrap_or(\"absent\"), option_env!(\"CINDER_CAPTURE_COMPILER_RECIPE\").unwrap_or(\"absent\"), option_env!(\"CINDER_REAL_CARGO\").unwrap_or(\"absent\"), option_env!(\"CINDER_SYNCHRONOUS_STATE_RECORDING\").unwrap_or(\"absent\")); }\n",
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
        command.current_dir(&self.root).arg("r");
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
        if key == "CINDER_USAGE" {
            command.env("XDG_STATE_HOME", self.root.join("state-home"));
        }
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

    fn build_from(&self, directory: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command.current_dir(directory).output().unwrap()
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

    fn check_selected_bin_with_trailing_argument(&self, argument: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&self.root).args([
            "check",
            "--bin",
            self.package.unwrap_or("cinder-fast-run-fixture"),
            "--",
            argument,
        ]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .output()
            .unwrap()
    }

    fn check_selected_bin_with_output_flag(&self, flag: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&self.root).args([
            "check",
            flag,
            "--bin",
            self.package.unwrap_or("cinder-fast-run-fixture"),
        ]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .output()
            .unwrap()
    }

    fn check_selected_lib(&self) -> Output {
        self.check_selected_targets("check", &["--lib"], None)
    }

    fn check_selected_lib_direct(&self) -> Output {
        self.check_selected_lib_direct_command().output().unwrap()
    }

    fn check_selected_lib_direct_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&self.root).args(["check", "--lib"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CINDER_EXPERIMENTAL_DIRECT_CHECK", "1")
            .env("CINDER_TRACE_RUN", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env("CARGO_HOME", self.root.join(".cinder-test-cargo-home"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .env_remove("CINDER_ARTIFACT_RECEIPT_DIRECTORY")
            .env_remove("CINDER_RUSTC_WRAPPER_MODE")
            .env_remove("CINDER_WRAPPER_ACTIVE")
            .env_remove("CINDER_NEXT_RUSTC_WRAPPER")
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER");
        command
    }

    fn cargo_check_selected_lib(&self) -> Output {
        Command::new("cargo")
            .current_dir(&self.root)
            .args(["check", "--lib"])
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env("CARGO_HOME", self.root.join(".cinder-test-cargo-home"))
            .env_remove("CINDER_EXPERIMENTAL_DIRECT_CHECK")
            .output()
            .unwrap()
    }

    fn cargo_check_selected_integration(&self) -> Output {
        Command::new("cargo")
            .current_dir(&self.root)
            .args(["check", "--test", "smoke"])
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env("CARGO_HOME", self.root.join(".cinder-test-cargo-home"))
            .env_remove("CINDER_EXPERIMENTAL_DIRECT_CHECK")
            .output()
            .unwrap()
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

    fn check_selected_integration_direct(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .args(["check", "--test", "smoke"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "check")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CINDER_EXPERIMENTAL_DIRECT_CHECK", "1")
            .env("CINDER_TRACE_RUN", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env("CARGO_HOME", self.root.join(".cinder-test-cargo-home"))
            .env_remove("CINDER_DISABLE_FAST_CHECK")
            .env_remove("CINDER_ARTIFACT_RECEIPT_DIRECTORY")
            .env_remove("CINDER_RUSTC_WRAPPER_MODE")
            .env_remove("CINDER_WRAPPER_ACTIVE")
            .env_remove("CINDER_NEXT_RUSTC_WRAPPER")
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER")
            .output()
            .unwrap()
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

    fn test_selected_lib(&self) -> Output {
        self.test_selected_lib_with_command("test")
    }

    fn test_selected_lib_alias(&self) -> Output {
        self.test_selected_lib_with_command("t")
    }

    fn test_selected_workspace_lib(&self) -> Output {
        self.test_selected_workspace_lib_with_arguments(&["-p", self.package.unwrap()])
    }

    fn test_selected_workspace_lib_with_equals(&self) -> Output {
        let package = format!("--package={}", self.package.unwrap());
        self.test_selected_workspace_lib_with_arguments(&[package.as_str()])
    }

    fn test_selected_workspace_lib_with_arguments(&self, package_arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .arg("test")
            .args(package_arguments)
            .args(["--lib", "--", "--nocapture"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "test")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CINDER_TRACE_RUN", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_TEST")
            .env_remove("CINDER_ARTIFACT_RECEIPT_DIRECTORY")
            .env_remove("CINDER_RUSTC_WRAPPER_MODE")
            .env_remove("CINDER_WRAPPER_ACTIVE")
            .env_remove("CINDER_NEXT_RUSTC_WRAPPER")
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER")
            .output()
            .unwrap()
    }

    fn test_selected_lib_with_command(&self, command_name: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .args([command_name, "--lib", "--", "--nocapture"]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "test")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CINDER_TRACE_RUN", "1")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_TEST")
            .env_remove("CINDER_ARTIFACT_RECEIPT_DIRECTORY")
            .env_remove("CINDER_RUSTC_WRAPPER_MODE")
            .env_remove("CINDER_WRAPPER_ACTIVE")
            .env_remove("CINDER_NEXT_RUSTC_WRAPPER")
            .env_remove("CINDER_ORIGINAL_RUSTC_WRAPPER")
            .output()
            .unwrap()
    }

    fn test_selected_lib_with_positional_filter(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command.current_dir(&self.root).args([
            "test",
            "--lib",
            "runtime_contract",
            "--",
            "--nocapture",
        ]);
        command
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_FIXTURE_VALUE", "test")
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("CINDER_DISABLE_FAST_TEST")
            .output()
            .unwrap()
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

    fn build_with_workspace_wrapper(&self, wrapper: &Path, probe: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        self.configure_build(&mut command);
        command
            .env("RUSTC_WORKSPACE_WRAPPER", wrapper)
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
        // The detached recorder now also runs hidden no-change Cargo passes,
        // which a fully loaded test machine can starve for several seconds.
        for _ in 0..750 {
            if state.is_file() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("background recorder did not publish {}", state.display());
    }

    fn configure(&self, command: &mut Command, context: &str) {
        command.current_dir(&self.root).arg("run");
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
