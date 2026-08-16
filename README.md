# Cinder

Cinder is an experimental Cargo-compatible development accelerator for Rust.
It uses Cargo as the source of truth and takes a faster path only when it can
prove that Cargo's result is still valid. Everything else runs through Cargo.

```bash
cinder run
cinder build -p my-app
cinder check -p my-library --lib
cinder test -p my-library --lib
```

Existing projects need no migration or Cinder-specific configuration. Cargo
arguments, workspaces, manifests, lockfiles, features, build scripts, proc
macros, native dependencies, configuration, wrappers, and toolchains remain
authoritative.

## Try it

Cinder currently requires Rust 1.85 or newer. Build it from source, put the
release binary on `PATH`, and use `cinder` where you would use `cargo`:

```bash
cargo build --release
export PATH="/path/to/cinder/target/release:$PATH"

cd /path/to/an/existing/rust/project
cinder check
```

The accelerated artifact paths currently target macOS. Linux and Windows use
the strict Cargo proxy, preserving command behavior without artifact
acceleration.

## What it accelerates

On a proven Cargo baseline, Cinder can:

- reuse an unchanged selected `build`, `check`, or `test --no-run` result;
- reuse an unchanged plain `check` or `build` of a multi-target package, and an
  unchanged plain `check` from a workspace root, validating every recorded
  member target;
- run the exact validated Cargo-built standard library test harness directly;
- restore one of the last eight validated source revisions for selected builds
  and runs;
- patch one unambiguous equal-length Rust string in a development executable on
  macOS, then sign and publish the result atomically.

Every reuse hit also replays the cached compiler warnings and manifest
diagnostics that a real no-change Cargo command would print, byte-for-byte;
a state whose replay cannot be proven is not published.

Test bodies still execute on every test invocation. First-seen structural
changes, unsupported command shapes, ambiguous artifacts, stale state, custom
runners, nonstandard harnesses, release builds, and failed validation stay with
Cargo.

## The tuned development toolchain

For compilation Cinder cannot skip, it can make the compiler itself faster.
When a `cinder-tuned` toolchain is present — the same rustc release source as
the active stable, rebuilt with ThinLTO, one codegen unit, and profile-guided
optimization, with the parallel frontend enabled at run time — eligible
development `build`/`check`/`test`/`run` commands route through it
automatically. Measured out of the box against plain Cargo, fresh builds were
12% faster on ripgrep, 29% faster on Cinder's own crate, and 4% faster on fd,
with every artifact kept in a separate `target/cinder-tuned` namespace so
tuned and stock outputs can never mix.

Pinned projects can have their own matched build: a project pinning rustc X
routes through `cinder-tuned-X` when that build exists and proves the same
version identity, with knobs qualified per toolchain. Cap's pinned 1.88,
rebuilt from its own release source with Cap-trained PGO, measured 4–6%
faster cold member builds than the stock pin (16% in the best paired run),
edits flat to 6% better, on top of 15–21x no-change reuse — with Cap member
test suites passing identically.

Development artifacts from the tuned toolchain are functionally equivalent
but not byte-identical to stock Cargo's; release builds always use the stock
toolchain unchanged. A project toolchain pin without a matching tuned build,
an explicit toolchain choice, a user-set target directory, or any tuned-path
failure returns the command to stock Cargo automatically. Full per-knob
attribution, correctness gates, and retained negative results are in the
[tuned toolchain report](benchmarks/tuned-toolchain/README.md).

## The cross-project unit cache

Dependencies that were already compiled on this machine should never compile
again — in any project. On macOS, Cinder keeps a machine-local,
content-addressed cache of registry dependency units: after a successful real
Cargo build it captures each qualifying unit's rlib, rmeta, dep-info, and
Cargo's own fingerprint bytes verbatim, and before the next cold build — in
the same project or any other — it restores proven-identical units into the
target directory. Cargo then recomputes every fingerprint itself and links a
restored unit only when its own comparison succeeds, so a wrong or stale
restore can cost at most a normal recompile, never a wrong reuse.

Units qualify only when the soundness proof showed their bytes are
project-independent: registry sources, host target, no proc-macro dylibs.
Build-scripted packages cache as atomic groups — the script's executable,
its parsed output, and the whole OUT_DIR tree — so a warm cache never
re-executes a build script it has proven relocatable; a library that reads
OUT_DIR is itself never cached (its debug info embeds generated-file paths)
but its script group still restores. A key that ever observes two different
byte sets is permanently retired. The store lives in the user cache
directory, is owner-only, bounded to 10 GiB with least-recently-used
eviction, and is cleared by `cinder clean`.

Measured on warm-cache cold builds (medians, byte-stable artifacts in every
arm; full protocol, raw arrays, and the per-phase scope comparison in the
[unit-cache report](benchmarks/unit-cache/README.md)):

| Cold `build` workload | Cargo | Cinder, warm cache | Units restored | Result |
| --- | ---: | ---: | ---: | ---: |
| ripgrep | 3.104s | 2.052s | 24 of 33 compiles | **34% faster** |
| fd | 3.289s | 2.238s | 54 of 58 | **32% faster** |
| Cinder | 3.215s | 2.293s | 22 of 26 | **29% faster** |
| Cap example (339-unit graph) | 27.979s | 18.622s | 218 of 339 | **33% faster** |

What still compiles on a warm cache is exactly the evidence-based residue:
the project's own crates, git-source dependencies, proc-macro dylibs, and
OUT_DIR-reading libraries.

## Results

These are median end-to-end timings from real repositories on Apple Silicon.
They include Cinder's validation and locking. Every fast sample required the
expected path marker and a verified artifact or runtime result; the test row ran
all seven tests in every sample.

| Real workload | Trials | Cargo | Cinder | Result |
| --- | ---: | ---: | ---: | ---: |
| [Cap](https://github.com/CapSoftware/Cap) warm edit to stable visible desktop window | 7 cycles | 12.387s | 5.088s | **2.43x faster** |
| Cap selected example `build` | 15 + 15 | 237.942ms | 16.238ms | **14.65x faster** |
| Cap selected example `check` | 15 + 15 | 248.205ms | 15.187ms | **16.34x faster** |
| Current Cap workspace, `test -p cap-muxer-protocol --lib`, 7 tests | 21 + 21 | 134.066ms | 13.876ms | **9.66x faster** |
| [Handy](https://github.com/cjpais/Handy) selected binary `check` | 15 + 15 | 278.439ms | 24.392ms | **11.42x faster** |
| [Zed](https://github.com/zed-industries/zed) selected binary `check` | 10 + 10 | 587.644ms | 33.711ms | **17.43x faster** |
| [Bun](https://github.com/oven-sh/bun) workspace root, plain `check`, 108 member targets | 15 + 15 | 161.3ms | 12.6ms | **12.78x faster** |
| Cap real lib+bin member, plain `check -p scap-targets` | 15 + 15 | 257.3ms | 15.3ms | **16.85x faster** |
| [Bun](https://github.com/oven-sh/bun) real 489-line retained revision | 7 + 7 | 12.06s | 0.22s | **54.82x faster** |

These results describe the named workloads, not universal Rust compilation
speed. Unsupported first-seen edits measured a 4.09% safe-fallback overhead on
the same machine. Full commands, revisions, sample arrays, negative controls,
rejected experiments, and reproduction details are in the
[benchmark report](benchmarks/cargo-parity/README.md) and the raw results
([2026-08-15](benchmarks/cargo-parity/results-2026-08-15.tsv),
[2026-08-16](benchmarks/cargo-parity/results-2026-08-16.tsv)).

## Safety model

Cinder does not reimplement dependency resolution, feature selection, build
scripts, proc macros, or toolchain selection. Before reuse it validates the
Cargo invocation and environment, source and control inputs, project topology,
artifact identity and digest, dep-info, fingerprints, build-script evidence,
and relevant runtime contracts.

State is bounded, versioned, private to the user, and published through staged
atomic promotion. Malformed, incomplete, changed, or ambiguous evidence is a
normal cache miss. Cinder then delegates the original arguments, environment,
output, and exit behavior to the real Cargo executable.

See [the architecture notes](docs/architecture.md) for the complete boundary,
including revision storage, compiler observation, code signing, concurrency,
cleanup, and the opt-in first-seen `check` experiment.

## Local evidence

Usage evidence is optional and privacy-safe:

```bash
CINDER_USAGE=1 cinder check -p my-package --lib
cinder stats
cinder stats --json
```

It records only command class, proven outcome, decision latency, and calendar
day. It does not store repository identifiers, paths, arguments, source,
commands, or environment values, and it never estimates time saved.

## Development

The primary local gates are:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo +1.85.0 test
```

Cinder is early-stage and is not yet distributed as a stable release.

## License

Cinder is licensed under the [MIT License](LICENSE).
