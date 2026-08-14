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
    let fake_cargo = directory.join("cargo");
    fs::write(
        &fake_cargo,
        "#!/bin/sh\nprintf 'arg=<%s>\\n' \"$@\"\nprintf 'probe=<%s>\\n' \"$CINDER_TEST_PROBE\"\nexit 23\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_cargo, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cinder"))
        .env("CINDER_REAL_CARGO", &fake_cargo)
        .env("CINDER_TEST_PROBE", "preserved")
        .args(["check", "--features", "one two"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "arg=<check>\narg=<--features>\narg=<one two>\nprobe=<preserved>\n"
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
