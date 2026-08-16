//! Tuned-toolchain routing integration tests.
//!
//! These tests exercise the real `cinder-tuned` rustup toolchain when it is
//! installed on the machine and skip cleanly when it is not, so the suite
//! stays portable to environments that never built the tuned compiler.

use std::{
    env,
    ffi::OsStr,
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

fn rustup_toolchains() -> Option<PathBuf> {
    env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")))
        .map(|home| home.join("toolchains"))
}

fn toolchain_installed(name: &str) -> bool {
    rustup_toolchains().is_some_and(|toolchains| toolchains.join(name).join("bin/rustc").is_file())
}

fn tuned_toolchain_available() -> bool {
    toolchain_installed("cinder-tuned")
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str, main_source: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "cinder-toolchain-{name}-{unique}-{}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"toolchain-fixture-{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
            ),
        )
        .unwrap();
        fs::write(root.join("src/main.rs"), main_source).unwrap();
        Self { root }
    }

    fn cinder(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cinder"));
        command
            .current_dir(&self.root)
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1");
        for disqualifier in [
            "RUSTUP_TOOLCHAIN",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
            "RUSTC",
            "RUSTDOC",
            "RUSTC_BOOTSTRAP",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "CINDER_STOCK",
            "CINDER_TUNED_BACKEND",
        ] {
            command.env_remove(disqualifier);
        }
        command
    }

    fn tuned_target(&self) -> PathBuf {
        self.root.join("target/cinder-tuned")
    }

    fn stock_target(&self) -> PathBuf {
        self.root.join("target/debug")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn skip_or_panic(name: &str) -> bool {
    if tuned_toolchain_available() {
        return false;
    }
    eprintln!("skipping {name}: the cinder-tuned toolchain is not installed");
    true
}

const WORKING_MAIN: &str = "fn main() { println!(\"toolchain-fixture-ok\"); }\n";

#[test]
fn eligible_build_routes_through_the_tuned_namespace() {
    if skip_or_panic("eligible_build_routes_through_the_tuned_namespace") {
        return;
    }
    let fixture = Fixture::new("eligible", WORKING_MAIN);
    let output = fixture.cinder().arg("build").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        fixture.tuned_target().join("debug").is_dir(),
        "tuned namespace missing: {}",
        stderr(&output)
    );
    assert!(
        !fixture.stock_target().exists(),
        "stock target must stay untouched under tuned routing"
    );
    let binary = fixture
        .tuned_target()
        .join("debug")
        .join(format!("toolchain-fixture-{}", "eligible"));
    let run = Command::new(binary).output().unwrap();
    assert!(run.status.success());
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        "toolchain-fixture-ok\n"
    );
}

#[test]
fn toolchain_pins_keep_the_stock_path() {
    if skip_or_panic("toolchain_pins_keep_the_stock_path") {
        return;
    }
    let fixture = Fixture::new("pinned", WORKING_MAIN);
    fs::write(
        fixture.root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    let output = fixture.cinder().arg("build").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !fixture.tuned_target().exists(),
        "a pinned project must never route through the tuned toolchain"
    );
    assert!(fixture.stock_target().is_dir());
}

#[test]
fn explicit_toolchain_and_escape_hatch_environments_stay_stock() {
    if skip_or_panic("explicit_toolchain_and_escape_hatch_environments_stay_stock") {
        return;
    }
    for (key, value) in [
        ("RUSTUP_TOOLCHAIN", OsStr::new("stable")),
        ("CINDER_STOCK", OsStr::new("1")),
    ] {
        let fixture = Fixture::new("env-stock", WORKING_MAIN);
        let output = fixture
            .cinder()
            .env(key, value)
            .arg("build")
            .output()
            .unwrap();
        assert!(output.status.success(), "{key}: {}", stderr(&output));
        assert!(
            !fixture.tuned_target().exists(),
            "{key} must force the stock path"
        );
        assert!(fixture.stock_target().is_dir(), "{key}");
    }
}

#[test]
fn user_target_directories_and_release_builds_stay_stock() {
    if skip_or_panic("user_target_directories_and_release_builds_stay_stock") {
        return;
    }
    let fixture = Fixture::new("user-target", WORKING_MAIN);
    let custom = fixture.root.join("custom-target");
    let output = fixture
        .cinder()
        .env("CARGO_TARGET_DIR", &custom)
        .arg("build")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(custom.is_dir());
    assert!(
        !fixture.tuned_target().exists(),
        "a user-selected target directory must never be redirected"
    );

    let release = Fixture::new("release", WORKING_MAIN);
    let output = release
        .cinder()
        .args(["build", "--release"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !release.tuned_target().exists(),
        "--release must always use the stock toolchain"
    );
    assert!(release.root.join("target/release").is_dir());
}

#[test]
fn tuned_compile_errors_surface_once_without_a_stock_rerun() {
    if skip_or_panic("tuned_compile_errors_surface_once_without_a_stock_rerun") {
        return;
    }
    let fixture = Fixture::new(
        "compile-error",
        "fn main() { let broken: u32 = \"toolchain-fixture-type-error\"; }\n",
    );
    let output = fixture.cinder().arg("build").output().unwrap();
    assert!(!output.status.success());
    let errors = stderr(&output);
    assert!(
        errors.contains("mismatched types"),
        "the tuned compiler's diagnostics must reach the user: {errors}"
    );
    assert_eq!(
        errors
            .matches("Compiling toolchain-fixture-compile-error")
            .count(),
        1,
        "an ordinary compile error must not trigger a stock rerun: {errors}"
    );
    assert!(
        fixture.tuned_target().is_dir(),
        "the failed build should still have used the tuned namespace"
    );
}

#[test]
fn fast_path_reuse_still_engages_in_the_tuned_namespace() {
    if skip_or_panic("fast_path_reuse_still_engages_in_the_tuned_namespace") {
        return;
    }
    let fixture = Fixture::new("reuse", WORKING_MAIN);
    let first = fixture.cinder().arg("build").output().unwrap();
    assert!(first.status.success(), "{}", stderr(&first));
    let second = fixture.cinder().arg("build").output().unwrap();
    assert!(second.status.success(), "{}", stderr(&second));
    let errors = stderr(&second);
    assert!(
        errors.contains("Cinder reused"),
        "the no-change fast path must engage on tuned state: {errors}"
    );
    assert!(
        !fixture.stock_target().exists(),
        "reuse must not touch the stock namespace"
    );
}

#[test]
fn plain_version_pins_route_through_the_matching_tuned_build() {
    if !toolchain_installed("cinder-tuned-1.88")
        || !toolchain_installed("1.88.0-aarch64-apple-darwin")
    {
        eprintln!(
            "skipping plain_version_pins_route_through_the_matching_tuned_build: \
             cinder-tuned-1.88 or stock 1.88.0 is not installed"
        );
        return;
    }
    let fixture = Fixture::new("pin-match", WORKING_MAIN);
    fs::write(
        fixture.root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.88.0\"\n",
    )
    .unwrap();
    let output = fixture.cinder().arg("build").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        fixture.tuned_target().join("debug").is_dir(),
        "a 1.88.0 pin with cinder-tuned-1.88 installed must route tuned: {}",
        stderr(&output)
    );
    assert!(
        !fixture.stock_target().exists(),
        "the pinned stock namespace must stay untouched under per-pin routing"
    );
    let binary = fixture
        .tuned_target()
        .join("debug")
        .join("toolchain-fixture-pin-match");
    let run = Command::new(binary).output().unwrap();
    assert!(run.status.success());
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        "toolchain-fixture-ok\n"
    );
}

#[test]
fn plain_version_pins_without_a_matching_tuned_build_stay_stock() {
    if skip_or_panic("plain_version_pins_without_a_matching_tuned_build_stay_stock") {
        return;
    }
    if !toolchain_installed("1.85.0-aarch64-apple-darwin")
        || toolchain_installed("cinder-tuned-1.85")
        || toolchain_installed("cinder-tuned-1.85.0")
    {
        eprintln!(
            "skipping plain_version_pins_without_a_matching_tuned_build_stay_stock: \
             needs stock 1.85.0 installed and no cinder-tuned-1.85"
        );
        return;
    }
    let fixture = Fixture::new("pin-unmatched", WORKING_MAIN);
    fs::write(
        fixture.root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.85.0\"\n",
    )
    .unwrap();
    let output = fixture.cinder().arg("build").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !fixture.tuned_target().exists(),
        "a pin without a matching tuned build must stay entirely stock"
    );
    assert!(fixture.stock_target().is_dir());
}

#[test]
fn non_version_pins_never_route_tuned() {
    if skip_or_panic("non_version_pins_never_route_tuned") {
        return;
    }
    let fixture = Fixture::new("pin-nightly", WORKING_MAIN);
    fs::write(
        fixture.root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2026-07-20\"\n",
    )
    .unwrap();
    // The build may fail if the pinned nightly is absent (auto-install is
    // disabled so the test can never download a toolchain); either way the
    // tuned namespace must not appear.
    let output = fixture
        .cinder()
        .env("RUSTUP_AUTO_INSTALL", "0")
        .arg("build")
        .output()
        .unwrap();
    let _ = output;
    assert!(
        !fixture.tuned_target().exists(),
        "a non-version pin must never route through a tuned toolchain"
    );
}

#[test]
fn cinder_clean_removes_the_tuned_namespace() {
    if skip_or_panic("cinder_clean_removes_the_tuned_namespace") {
        return;
    }
    let fixture = Fixture::new("clean", WORKING_MAIN);
    let build = fixture.cinder().arg("build").output().unwrap();
    assert!(build.status.success(), "{}", stderr(&build));
    assert!(fixture.tuned_target().is_dir());
    let clean = fixture.cinder().arg("clean").output().unwrap();
    assert!(clean.status.success(), "{}", stderr(&clean));
    assert!(
        !fixture.tuned_target().exists(),
        "cinder clean must remove the tuned namespace"
    );
}
