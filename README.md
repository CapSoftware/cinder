# Cinder

Cinder is an experimental Cargo-compatible development accelerator for Rust.
It runs existing projects without a Cinder-specific project model, uses Cargo
as the source of truth, and applies an optimized path only when it can prove
the result is safe. Every other command or edit falls back to Cargo.

```bash
cinder run
cinder build
cinder check
cinder test
```

Cargo arguments continue to work, including package selection from a workspace
root:

```bash
cinder run -p my-app
cinder build -p my-app
```

Cinder preserves the project's `Cargo.toml`, `Cargo.lock`, workspace, feature,
build-script, proc-macro, native dependency, environment, Cargo configuration,
compiler-wrapper, and Rust toolchain contracts.

## Benchmarks

The current prototype has been measured on real Rust desktop applications on
Apple Silicon. Times are median wall-clock durations with a warm target cache.
Each row measures a complete development outcome, not just compiler time.

| Scenario | Trials per phase | Cargo | Cinder | Result |
| --- | ---: | ---: | ---: | ---: |
| [Cap](https://github.com/CapSoftware/Cap) desktop, warm edit to stable visible window | 7 | 12.387s | 5.088s | **2.43x faster** (59.0% less time) |
| Separate multi-package desktop workspace, `run -p` from its root | 7 | 4.603s | 1.739s | **2.65x faster** (62.2% less time) |
| Separate split-library Tauri app, executable `build -p` | 5 | 4.537s | 0.708s | **6.41x faster** (84.4% less time) |

The workspace and build comparisons used a Cargo A → Cinder → Cargo B sequence
to reduce time-order bias. The reported Cargo value is the mean of the two
bracketing Cargo medians. Source values were alternated between equal-length
states, warm-ups were excluded, and the original source and Git state were
restored exactly after each run.

Every optimized Cinder trial was checked to confirm that the artifact was
patched without compiling. The projects did not require source changes or a
Cinder-specific integration.

Publishable benchmark artifacts omit source revision and diff identifiers,
absolute repository and home paths, raw commands, Git status filenames,
process IDs, hostnames, device identifiers, email addresses, and environment
values. Logs are redacted before they reach the terminal or disk; only
allowlisted toolchain, operating-system, architecture, power-source, and
thermal-state categories are retained.

These are prototype measurements, not a claim that every Rust edit is 2.43x to
6.41x faster. The rows exercise Cinder's current narrow fast path; unsupported
edits still use Cargo.

## What Cinder accelerates today

The current fast path supports macOS development `run` commands and
single-executable `build` commands. It recognizes one equal-byte-length UTF-8
edit in one project Rust source file when the edited data is both unambiguous
and unique in the compiled executable.

Eligible data currently includes:

- an ordinary unescaped Rust string containing at least eight bytes;
- the static suffix after the sole `{}` placeholder in a simple format string;
- strings in the selected binary or a linked local library target;
- sources discovered through Cargo dep-info, including nested workspace
  packages and custom module layouts.

After a normal Cargo build, Cinder records the selected executable and prepares
its source index outside the command's critical path. On the next eligible edit
it:

1. verifies the Cargo invocation, environment, artifact, manifests, lockfile,
   configuration, dep-info, project sources, and build-script inputs;
2. APFS-clones the development executable and changes only the proven data
   bytes;
3. ad-hoc signs the staged Mach-O and publishes it atomically;
4. launches it with Cargo's original program name, arguments, runner behavior,
   and runtime library environment for `cinder run`.

`cinder run` leaves Cargo's executable untouched and launches a signed sibling.
`cinder build` publishes the transformed executable at Cargo's normal public
artifact path, but preserves its previous modification time. The edited source
therefore remains newer, so a later real Cargo command still recompiles it. An
unchanged recorded executable build can also be reused without invoking Cargo.

Existing `RUSTC_WRAPPER` tools are chained during artifact capture rather than
replaced. Watcher-driven workflows can opt into duplicate file-event
coalescing with `CINDER_COALESCE_RUN_EVENTS=1`; using Cinder as a `cargo` shim
enables that launch policy automatically.

## Cargo fallback

Cargo remains the compatibility floor. Cinder delegates unknown commands and
unsupported optimization cases with their arguments, environment, output, and
exit status intact. `check` and `test` currently always use Cargo.

The fast path is rejected for cases including:

- structural Rust changes, different-length strings, escaped or raw strings,
  ambiguous data, and changes across multiple source files;
- changed features, environment, manifests, lockfiles, Cargo configuration,
  build-script inputs, compiler context, or other project Rust inputs;
- release or custom profiles, custom targets or runners, command-line Cargo
  configuration, examples, tests, benches, multiple executables, and
  library-only builds;
- missing reproducibility inputs, stale state, unsupported artifacts, and
  non-macOS targets.

There is no heuristic partial build. A failed validation is a normal Cargo
cache miss.

## Correctness and compatibility

Cinder does not reimplement dependency resolution, feature selection, build
scripts, proc macros, toolchain selection, or the workspace graph. Cargo still
performs every reference build and remains authoritative for those contracts.

Accelerated state is stored in owner-only temporary directories. It contains a
SHA-256 digest of the invocation and environment rather than raw environment
values, source snapshots, public Cargo dep-info, artifact metadata, and the
inputs needed to invalidate the fast path. Artifact publication is staged,
signed, and atomic. Malformed, stale, incomplete, or ambiguous state falls back
to Cargo.

Integration tests compare transformed binaries with normal Cargo output and
cover standalone packages, virtual workspaces, linked library targets,
build-script invalidation, compiler-wrapper chaining, runtime linker state,
Cargo freshness after a build patch, and unsupported-edit fallback.

For the full safety boundary, see [the architecture notes](docs/architecture.md).

## Trying the prototype

Cinder is not distributed as a stable release yet. To try the current source
build on macOS:

```bash
cargo build --release
# Add this repository's target/release directory to PATH.
cd /path/to/an/existing/rust/project
cinder run
```

The project currently requires Rust 1.85 or newer to build. No project files
need to be migrated.

## Roadmap

The next goal is to make the fast path useful across progressively broader
classes of ordinary Rust development while keeping Cargo fallback safe and
cheap:

- general warm Rust rebuilds beyond data-only edits;
- persistent compiler and dependency-graph reuse;
- faster code generation and linking;
- project-aware workspace scheduling and local caching;
- portable artifact backends beyond macOS and APFS;
- reproducible benchmarks across a wider set of repositories and toolchains.

Release builds, dependency management, remote caches, and distributed builds
remain deliberate non-goals for the current milestone.

## Status

Cinder is early-stage and not ready for production use. Development currently
focuses on macOS and Apple Silicon, with Cargo correctness as the compatibility
boundary.

## License

Cinder is licensed under the [MIT License](LICENSE).
