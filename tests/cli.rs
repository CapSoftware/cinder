#[cfg(unix)]
#[test]
fn forwards_arguments_environment_and_exit_status_to_cargo() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("cinder-cli-{}-{unique}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let temporary = directory.join("tmp");
    fs::create_dir(&temporary).unwrap();
    let fake_cargo = directory.join("cargo");
    fs::write(
        &fake_cargo,
        "#!/bin/sh\nprintf 'arg=<%s>\\n' \"$@\"\nprintf 'probe=<%s>\\n' \"$CINDER_TEST_PROBE\"\nprintf 'usage=<%s>\\n' \"$CINDER_USAGE\"\nprintf 'direct=<%s>\\n' \"$CINDER_EXPERIMENTAL_DIRECT_CHECK\"\nexit 23\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_cargo, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cinder"))
        .current_dir(&directory)
        .env("CINDER_REAL_CARGO", &fake_cargo)
        .env("CINDER_TEST_PROBE", "preserved")
        .env("CINDER_USAGE", "1")
        .env("CINDER_EXPERIMENTAL_DIRECT_CHECK", "1")
        .env("XDG_STATE_HOME", &temporary)
        .args(["check", "--features", "one two"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "arg=<check>\narg=<--features>\narg=<one two>\nprobe=<preserved>\nusage=<>\ndirect=<>\n"
    );

    let report = Command::new(env!("CARGO_BIN_EXE_cinder"))
        .env("XDG_STATE_HOME", &temporary)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(report.status.success());
    let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["recorded_decisions"], 1);
    assert_eq!(report["fast_path_selections"], 0);
    assert_eq!(report["cargo_fallbacks"], 1);
    assert_eq!(report["commands"][2]["command"], "check");
    assert_eq!(report["commands"][2]["outcomes"]["cargo_fallback"], 1);

    let evidence = fs::read(temporary.join("cinder/events-v1")).unwrap();
    assert_eq!(evidence.len(), 16);
    assert!(!evidence.windows(7).any(|window| window == b"one two"));
    assert!(!evidence.windows(9).any(|window| window == b"preserved"));
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn cargo_shim_finds_the_next_real_cargo_on_path() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("cinder-cargo-shim-{}-{unique}", std::process::id()));
    let shim_directory = directory.join("shim");
    let real_directory = directory.join("real");
    fs::create_dir_all(&shim_directory).unwrap();
    fs::create_dir_all(&real_directory).unwrap();
    let shim = shim_directory.join("cargo");
    symlink(env!("CARGO_BIN_EXE_cinder"), &shim).unwrap();
    let real_cargo = real_directory.join("cargo");
    fs::write(
        &real_cargo,
        "#!/bin/sh\nprintf 'real-cargo arg=<%s> probe=<%s>\\n' \"$1\" \"$CINDER_TEST_PROBE\"\nexit 29\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&real_cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&real_cargo, permissions).unwrap();

    let search_path = std::env::join_paths([&shim_directory, &real_directory]).unwrap();
    let output = Command::new(&shim)
        .env("PATH", search_path)
        .env_remove("CINDER_REAL_CARGO")
        .env("CINDER_TEST_PROBE", "preserved")
        .arg("check")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(29));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "real-cargo arg=<check> probe=<preserved>\n"
    );
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn cargo_shim_rejects_a_bare_override_that_resolves_to_itself() {
    use std::{
        fs,
        os::unix::fs::symlink,
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("cinder-cargo-loop-{}-{unique}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let shim = directory.join("cargo");
    symlink(env!("CARGO_BIN_EXE_cinder"), &shim).unwrap();

    let output = Command::new(&shim)
        .env("PATH", &directory)
        .env("CINDER_REAL_CARGO", "cargo")
        .arg("--version")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("resolves to Cinder itself")
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn prints_cinder_help_without_starting_cargo() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cinder"))
        .arg("--help")
        .env("CINDER_REAL_CARGO", "/path/that/must/not/run")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("cinder <COMMAND>")
    );
}
