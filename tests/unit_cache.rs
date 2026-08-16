#![cfg(target_os = "macos")]

//! End-to-end proof that the unit cache moves Cargo's own dependency-unit
//! bytes between projects: a second project's cold build restores the units
//! — including build-script groups and their OUT_DIR trees — Cargo reports
//! them fresh without rerunning any build script, and the final binary is
//! byte-identical to a from-scratch build. A nondeterministic build script
//! must retire its group permanently, and `cinder clean` must empty the
//! store.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct CacheFixture {
    root: PathBuf,
    nondeterministic: bool,
}

impl CacheFixture {
    fn new() -> Self {
        Self::build(false)
    }

    /// A fixture whose vendor set adds a nondeterministic build script.
    fn with_nondeterministic_dep() -> Self {
        Self::build(true)
    }

    fn build(nondeterministic: bool) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cinder-unit-cache-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        let fixture = Self {
            root,
            nondeterministic,
        };
        fixture.write_vendor();
        fixture.write_project("appa");
        fixture.write_project("appb");
        fixture
    }

    fn cache_root(&self) -> PathBuf {
        self.root.join("unit-cache")
    }

    fn project(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn write_vendor(&self) {
        let vendor = self.root.join("vendor");
        let plain = vendor.join("vendored-dep-1.0.0");
        fs::create_dir_all(plain.join("src")).unwrap();
        fs::write(
            plain.join("Cargo.toml"),
            "[package]\nname = \"vendored-dep\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            plain.join("src/lib.rs"),
            "pub fn shared_value() -> i32 { 40 }\npub fn shared_text() -> &'static str { \"cached\" }\n",
        )
        .unwrap();
        fs::write(
            plain.join(".cargo-checksum.json"),
            "{\"files\":{},\"package\":\"\"}",
        )
        .unwrap();
        // A dependency edge whose restore order is adversarial: `aa-user`
        // sorts before `zz-base`, so a restore that stamps files with
        // write-order timestamps would leave the dependency's rlib newer
        // than the dependent's dep-info and spuriously recompile `aa-user`.
        let base = vendor.join("zz-base-1.0.0");
        fs::create_dir_all(base.join("src")).unwrap();
        fs::write(
            base.join("Cargo.toml"),
            "[package]\nname = \"zz-base\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            base.join("src/lib.rs"),
            "pub fn base_value() -> i32 { 5 }\n",
        )
        .unwrap();
        fs::write(
            base.join(".cargo-checksum.json"),
            "{\"files\":{},\"package\":\"\"}",
        )
        .unwrap();
        let user = vendor.join("aa-user-1.0.0");
        fs::create_dir_all(user.join("src")).unwrap();
        fs::write(
            user.join("Cargo.toml"),
            "[package]\nname = \"aa-user\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nzz-base = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            user.join("src/lib.rs"),
            "pub fn combined() -> i32 { zz_base::base_value() + 2 }\n",
        )
        .unwrap();
        fs::write(
            user.join(".cargo-checksum.json"),
            "{\"files\":{},\"package\":\"\"}",
        )
        .unwrap();
        let outdir = vendor.join("outdir-dep-1.0.0");
        fs::create_dir_all(outdir.join("src")).unwrap();
        fs::write(
            outdir.join("Cargo.toml"),
            "[package]\nname = \"outdir-dep\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            outdir.join("build.rs"),
            "use std::{env, fs, path::Path};\n\
             fn main() {\n    let out = env::var(\"OUT_DIR\").unwrap();\n    \
             fs::write(Path::new(&out).join(\"gen.rs\"), \"pub fn g() -> i32 { 7 }\\n\").unwrap();\n    \
             println!(\"cargo::rerun-if-changed=build.rs\");\n}\n",
        )
        .unwrap();
        fs::write(
            outdir.join("src/lib.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n",
        )
        .unwrap();
        fs::write(
            outdir.join(".cargo-checksum.json"),
            "{\"files\":{},\"package\":\"\"}",
        )
        .unwrap();
        if self.nondeterministic {
            let ndet = vendor.join("ndet-dep-1.0.0");
            fs::create_dir_all(ndet.join("src")).unwrap();
            fs::write(
                ndet.join("Cargo.toml"),
                "[package]\nname = \"ndet-dep\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            fs::write(
                ndet.join("build.rs"),
                "use std::{env, fs, path::Path, time::{SystemTime, UNIX_EPOCH}};\n\
                 fn main() {\n    let out = env::var(\"OUT_DIR\").unwrap();\n    \
                 let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();\n    \
                 fs::write(Path::new(&out).join(\"gen.rs\"), format!(\"pub fn n() -> u128 {{ {nanos} }}\\n\")).unwrap();\n    \
                 println!(\"cargo::rerun-if-changed=build.rs\");\n    \
                 println!(\"cargo::rerun-if-env-changed=CINDER_TEST_NDET\");\n}\n",
            )
            .unwrap();
            fs::write(
                ndet.join("src/lib.rs"),
                "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n",
            )
            .unwrap();
            fs::write(
                ndet.join(".cargo-checksum.json"),
                "{\"files\":{},\"package\":\"\"}",
            )
            .unwrap();
        }
        let scripted = vendor.join("scripted-dep-1.0.0");
        fs::create_dir_all(scripted.join("src")).unwrap();
        fs::write(
            scripted.join("Cargo.toml"),
            "[package]\nname = \"scripted-dep\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            scripted.join("build.rs"),
            "fn main() { println!(\"cargo::rustc-cfg=scripted_ready\"); }\n",
        )
        .unwrap();
        fs::write(
            scripted.join("src/lib.rs"),
            "pub fn scripted_value() -> i32 { if cfg!(scripted_ready) { 2 } else { 1 } }\n",
        )
        .unwrap();
        fs::write(
            scripted.join(".cargo-checksum.json"),
            "{\"files\":{},\"package\":\"\"}",
        )
        .unwrap();
    }

    fn write_project(&self, name: &str) {
        let project = self.project(name);
        fs::create_dir_all(project.join("src")).unwrap();
        fs::create_dir_all(project.join(".cargo")).unwrap();
        fs::write(
            project.join(".cargo/config.toml"),
            format!(
                "[source.crates-io]\nreplace-with = \"vendored\"\n\n\
                 [source.vendored]\ndirectory = \"{}\"\n",
                self.root.join("vendor").display()
            ),
        )
        .unwrap();
        fs::write(
            project.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [dependencies]\nvendored-dep = \"1.0.0\"\nscripted-dep = \"1.0.0\"\n\
                 aa-user = \"1.0.0\"\noutdir-dep = \"1.0.0\"\n{}",
                if self.nondeterministic {
                    "ndet-dep = \"1.0.0\"\n"
                } else {
                    ""
                }
            ),
        )
        .unwrap();
        let ndet_value = if self.nondeterministic {
            ", ndet_dep::n()"
        } else {
            ""
        };
        let ndet_slot = if self.nondeterministic { " {}" } else { "" };
        fs::write(
            project.join("src/main.rs"),
            format!(
                "fn main() {{\n    println!(\"{{}} {{}} {{}} {{}} {{}}{ndet_slot}\", vendored_dep::shared_value(), \
                 vendored_dep::shared_text(), scripted_dep::scripted_value(), \
                 aa_user::combined(), outdir_dep::g(){ndet_value});\n}}\n"
            ),
        )
        .unwrap();
    }

    fn cinder(&self, project: &str, arguments: &[&str]) -> Output {
        let project = self.project(project);
        Command::new(env!("CARGO_BIN_EXE_cinder"))
            .current_dir(&project)
            .args(arguments)
            .env("CINDER_REAL_CARGO", "cargo")
            .env("CINDER_SYNCHRONOUS_STATE_RECORDING", "1")
            .env("CINDER_TRACE_RUN", "1")
            .env("CINDER_UNIT_CACHE", self.cache_root())
            .env("CARGO_TARGET_DIR", project.join("target"))
            .env("CARGO_INCREMENTAL", "0")
            .output()
            .unwrap()
    }

    fn cargo(&self, project: &str, arguments: &[&str]) -> Output {
        let project = self.project(project);
        Command::new("cargo")
            .current_dir(&project)
            .args(arguments)
            .env("CARGO_TARGET_DIR", project.join("target"))
            .env("CARGO_INCREMENTAL", "0")
            .output()
            .unwrap()
    }

    fn binary(&self, project: &str) -> PathBuf {
        self.project(project).join("target/debug").join(project)
    }

    fn cached_packages(&self) -> Vec<String> {
        let index = self.cache_root().join("v1/index");
        let Ok(entries) = fs::read_dir(index) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names
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

fn file_digest(path: &Path) -> Vec<u8> {
    // A plain byte read suffices for equality comparison.
    fs::read(path).unwrap()
}

#[test]
fn cross_project_restore_is_fresh_and_byte_identical() {
    let fixture = CacheFixture::new();

    // Control: appb built from scratch by plain Cargo, no cache anywhere.
    let control = fixture.cargo("appb", &["build"]);
    assert_success(&control);
    let control_binary = file_digest(&fixture.binary("appb"));
    assert_success(&fixture.cargo("appb", &["clean"]));

    // Donor: appa built through Cinder records qualifying units.
    let donor = fixture.cinder("appa", &["build"]);
    assert_success(&donor);
    let cached = fixture.cached_packages();
    assert_eq!(
        cached,
        vec![
            "aa-user-1.0.0".to_owned(),
            "outdir-dep-1.0.0".to_owned(),
            "scripted-dep-1.0.0".to_owned(),
            "vendored-dep-1.0.0".to_owned(),
            "zz-base-1.0.0".to_owned(),
        ],
        "every dependency, including build-script groups, should be cached; stderr:\n{}",
        stderr(&donor)
    );

    // Cold build of appb through Cinder restores the units before Cargo runs.
    let restored = fixture.cinder("appb", &["build", "-v"]);
    assert_success(&restored);
    let restored_errors = stderr(&restored);
    assert!(
        restored_errors.contains("unit cache restored 5 of 5 candidates"),
        "expected five restored units; stderr:\n{restored_errors}"
    );
    for fresh in [
        "Fresh vendored-dep v1.0.0",
        "Fresh zz-base v1.0.0",
        "Fresh aa-user v1.0.0",
        "Fresh scripted-dep v1.0.0",
    ] {
        assert!(
            restored_errors.contains(fresh),
            "Cargo did not treat a restored unit as fresh ({fresh}); stderr:\n{restored_errors}"
        );
    }
    for recompiled in [
        "Compiling vendored-dep",
        "Compiling zz-base",
        "Compiling aa-user",
        "Compiling scripted-dep",
    ] {
        assert!(
            !restored_errors.contains(recompiled),
            "a restored dependency was recompiled ({recompiled}); stderr:\n{restored_errors}"
        );
    }
    // An OUT_DIR-reading library recompiles natively (its rlib embeds the
    // generated file's absolute path), but its restored build-script group
    // means the script itself never reruns.
    assert!(
        restored_errors.contains("Compiling outdir-dep"),
        "the OUT_DIR-reading library must compile natively; stderr:\n{restored_errors}"
    );
    assert!(
        !restored_errors.contains("build-script-build`"),
        "no restored build script may rerun; stderr:\n{restored_errors}"
    );

    // The final binary is byte-identical to the from-scratch control build.
    assert_eq!(
        file_digest(&fixture.binary("appb")),
        control_binary,
        "restored build binary differs from the from-scratch control"
    );
}

#[test]
fn restore_never_overwrites_an_existing_destination_file() {
    let fixture = CacheFixture::new();

    let donor = fixture.cinder("appa", &["build"]);
    assert_success(&donor);
    assert_eq!(fixture.cached_packages().len(), 5);
    assert_success(&fixture.cargo("appb", &["generate-lockfile"]));

    // Find the recorded rlib name and plant junk at its destination in appb.
    let donor_deps = fixture.project("appa").join("target/debug/deps");
    let rlib_name = fs::read_dir(&donor_deps)
        .unwrap()
        .flatten()
        .find_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            (name.starts_with("libvendored_dep-") && name.ends_with(".rlib")).then_some(name)
        })
        .expect("donor build produced no vendored-dep rlib");
    let planted = fixture
        .project("appb")
        .join("target/debug/deps")
        .join(&rlib_name);
    fs::create_dir_all(planted.parent().unwrap()).unwrap();
    fs::write(&planted, b"planted-junk").unwrap();

    let output = fixture.cinder("appb", &["build"]);
    assert_success(&output);
    let errors = stderr(&output);
    assert!(
        errors.contains("unit cache restored 4 of 5 candidates (existing 1"),
        "an occupied destination must skip exactly the planted unit; stderr:\n{errors}"
    );
}

#[test]
fn a_reusing_projects_record_pass_does_not_poison_the_entry() {
    let fixture = CacheFixture::new();

    assert_success(&fixture.cinder("appa", &["build"]));
    assert_eq!(fixture.cached_packages().len(), 5);
    assert_success(&fixture.cargo("appb", &["generate-lockfile"]));

    // appb restores the units, builds, and its synchronous record pass runs
    // over a target directory whose dependency files carry appb's prefix.
    let restored = fixture.cinder("appb", &["build"]);
    assert_success(&restored);
    let errors = stderr(&restored);
    assert!(
        errors.contains("unit cache restored 5 of 5 candidates"),
        "expected restored units; stderr:\n{errors}"
    );
    assert!(
        !errors.contains("unstable"),
        "a legitimate cross-project reuse must not mark the entry unstable; stderr:\n{errors}"
    );

    // A third build sees the entry still restorable state after appb's pass.
    assert_success(&fixture.cargo("appb", &["clean"]));
    let again = fixture.cinder("appb", &["build"]);
    assert_success(&again);
    assert!(
        stderr(&again).contains("unit cache restored 5 of 5 candidates"),
        "the entries must survive a reusing project's record pass; stderr:\n{}",
        stderr(&again)
    );
}

#[test]
fn clean_empties_the_unit_cache_store() {
    let fixture = CacheFixture::new();

    let donor = fixture.cinder("appa", &["build"]);
    assert_success(&donor);
    assert!(!fixture.cached_packages().is_empty());

    let cleaned = fixture.cinder("appa", &["clean"]);
    assert_success(&cleaned);
    assert!(
        !fixture.cache_root().exists(),
        "cinder clean must remove the unit cache store"
    );
}

#[test]
fn a_nondeterministic_build_script_group_is_retired_permanently() {
    let fixture = CacheFixture::with_nondeterministic_dep();

    // Donor: records the ndet run/compile pair alongside the deterministic
    // packages (its OUT_DIR-reading library never caches).
    assert_success(&fixture.cinder("appa", &["build"]));
    assert!(
        fixture
            .cached_packages()
            .contains(&"ndet-dep-1.0.0".to_owned())
    );

    // An edited build script reruns under the same run-fingerprint total
    // (rerun-if-changed is mtime-based); the rerun generates different
    // bytes, and the record pass must retire the entry as unstable.
    let build_script = fixture.root.join("vendor/ndet-dep-1.0.0/build.rs");
    let mut contents = fs::read(&build_script).unwrap();
    contents.extend_from_slice(b"// touched\n");
    fs::write(&build_script, contents).unwrap();
    let diverged = fixture.cinder("appa", &["build"]);
    assert_success(&diverged);
    let diverged_errors = stderr(&diverged);
    assert!(
        diverged_errors.contains("unstable"),
        "divergent build-script bytes must retire the entry; stderr:\n{diverged_errors}"
    );

    // A later cold build in another project compiles the retired package
    // normally — including a real build-script run — while the
    // deterministic packages keep restoring.
    assert_success(&fixture.cargo("appb", &["generate-lockfile"]));
    let after = fixture.cinder("appb", &["build", "-v"]);
    assert_success(&after);
    let after_errors = stderr(&after);
    assert!(
        after_errors.contains("Compiling ndet-dep"),
        "a retired group must compile normally; stderr:\n{after_errors}"
    );
    assert!(
        after_errors.contains("build-script-build`"),
        "a retired group's build script must actually run; stderr:\n{after_errors}"
    );
    assert!(
        after_errors.contains("unit cache restored 5 of 5 candidates"),
        "the deterministic packages must keep restoring; stderr:\n{after_errors}"
    );
}
