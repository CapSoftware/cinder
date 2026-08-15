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

The original executable-patching path produced these results:

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

Cinder now also retains a bounded history of exact Cargo-built revisions. The
following trials alternated between two real source states after both had been
built and validated once. They measure undo/redo, branch switching, and hot
reload returning to a recent revision; they do not claim to accelerate a
first-seen structural edit.

| Repository and real change | Trials | Cargo | Cinder | Result |
| --- | ---: | ---: | ---: | ---: |
| [Cap](https://github.com/CapSoftware/Cap), 97-line desktop refactor, `build -p cap-desktop --bin cap-desktop` | 7 | 18.79s | 0.49s | **38.3x faster** (97.4% less time) |
| [Handy](https://github.com/cjpais/Handy), 18-line Tauri change | 7 | 3.17s | 0.23s | **13.8x faster** (92.7% less time) |
| [Zed](https://github.com/zed-industries/zed), four-file Git/protobuf change, `build -p collab --bin collab` | 7 | 19.68s | 0.40s | **49.2x faster** (98.0% less time) |
| [Zed](https://github.com/zed-industries/zed), the same change, `run -p collab --bin collab -- version` | 7 | 18.59s | 0.14s | **132.8x faster** (99.2% less time) |
| [Bun](https://github.com/oven-sh/bun), 489-line XML change, `build -p bun_bin` producing `libbun_rust.a` | 7 | 10.68s | 0.08s | **133.5x faster** (99.3% less time) |

Each Cinder trial required an explicit history-hit marker. Executables passed
strict code-signature verification. Bun's static archive was opened with
`ar`, and its SHA-256 digest was checked against the requested source revision
on every trial. All benchmark source trees were restored to their exact Git
state afterward.

## What Cinder accelerates today

The current macOS development fast path has two layers:

- a direct executable transformation for one equal-byte-length UTF-8 edit when
  the edited bytes are unambiguous and unique in the compiled executable;
- exact restoration of a recently validated Cargo-built revision, including
  structural and multi-file changes, for `run` executables and `build`
  commands that produce one unambiguous primary artifact. Static-library
  restoration is covered end to end.

Eligible data currently includes:

- an ordinary unescaped Rust string containing at least eight bytes;
- the static suffix after the sole `{}` placeholder in a simple format string;
- strings in the selected binary or a linked local library target;
- sources discovered through Cargo dep-info, including nested workspace
  packages and custom module layouts.

After a normal Cargo build, Cinder records the selected artifact and prepares
its source/input state outside the command's critical path. On the next
eligible executable edit it:

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
When patching an ad-hoc signed executable, Cinder preserves and verifies its
identifier, entitlements, requirements, hardened-runtime flags, and launch and
library constraints. A non-ad-hoc signing identity falls back to Cargo because
Cinder does not assume access to its private key.

For revision restoration, Cinder validates source contents, manifests,
configuration, toolchain identity, build-script inputs, environment context,
and a digest-bound immutable artifact receipt before publishing the prior
public artifact. Cached artifacts are content-hashed when published, made
read-only, and then checked by their full filesystem identity on reuse; source
sets are content-hashed once per history lookup. Manifests, configuration, and
other control inputs retain both a content digest and full filesystem identity,
so same-size replacements with preserved modification times still invalidate.
Cinder deliberately does not fabricate Cargo's hashed outputs or fingerprints,
so the next ordinary Cargo build refreshes its own complete output set. Current
no-change reuse is allowed only while the exact recorded hashed artifact,
dep-info, and fingerprint directory still exist.

Retained history is bounded to eight revisions per project and command kind,
with a global 4 GiB logical-byte and 30-day limit over its artifacts, source
snapshots, state, and restored run siblings. The one active run state and one
active build state per workspace are outside that history budget, as are
ordinary Cargo outputs. Staging left by a terminated process is age-pruned
without touching live publishers. Deleted workspaces, expired entries, and
orphaned or superseded fast-run siblings are pruned.

Existing `RUSTC_WRAPPER` tools are chained during artifact capture rather than
replaced. Watcher-driven workflows can opt into duplicate file-event
coalescing with `CINDER_COALESCE_RUN_EVENTS=1`; using Cinder as a `cargo` shim
enables that launch policy automatically.

## Cargo fallback

Cargo remains the compatibility floor. Cinder delegates unknown commands and
unsupported optimization cases with their arguments, environment, output, and
exit status intact. `check` and `test` currently always use Cargo.

The fast path is rejected for cases including:

- first-seen structural Rust changes, different-length strings that are not an
  exact retained revision, escaped or raw strings, and ambiguous data;
- changed features, environment, manifests, lockfiles, Cargo configuration,
  build-script inputs, compiler context, or other project Rust inputs that do
  not match a retained validated state;
- release or custom profiles, custom targets or runners, command-line Cargo
  configuration, examples, tests, benches, multiple executables, and
  explicit multi-target/library selectors;
- missing reproducibility inputs, stale state, unsupported artifacts, and
  non-macOS targets.

There is no heuristic partial build. A failed validation is a normal Cargo
cache miss.

## Correctness and compatibility

Cinder does not reimplement dependency resolution, feature selection, build
scripts, proc macros, toolchain selection, or the workspace graph. Cargo still
performs every reference build and remains authoritative for those contracts.

Accelerated state is stored in owner-only temporary directories. It contains a
SHA-256 digest of the invocation and build-relevant environment rather than raw
environment values, content-addressed source/input state, exact Cargo output
paths, artifact metadata and digest, and the inputs needed to invalidate the
fast path. Volatile shell `_` is omitted unless the selected Cargo unit graph's
compiler dep-info or build-script output reports that it was observed, avoiding
false hot-path misses without making `env!("_")` or `rerun-if-env-changed=_`
stale. Artifact publication is staged and atomic; executable publication is
also ad-hoc signed. Malformed, stale, incomplete, or ambiguous state falls back
to Cargo.

Integration tests compare transformed artifacts with normal Cargo output and
cover standalone packages, virtual workspaces, linked library targets, static
libraries, revision replay, build-script content changes, new auto targets,
Cargo-home configuration, compiler identity, wrapper chaining, runtime linker
state, Cargo clean/partial-clean behavior, target locking, cache pruning, Cargo
freshness after a build patch, and unsupported-edit fallback.

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
