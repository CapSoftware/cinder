# Architecture and compatibility boundary

## Cargo remains the authority

Cinder accepts Cargo-style commands, but does not reimplement dependency
resolution, feature selection, build scripts, proc macros, toolchains, or the
workspace graph. Standard Cargo remains authoritative for those contracts.
Unknown commands and all currently unsupported optimization cases are delegated
with their arguments, environment, exit status, stdout, and stderr intact.

For eligible development `run` commands, Cinder adds a target runner through a
Cargo `--config` value. Cargo still selects and builds the target and prepares
its runtime environment. The runner records the correct artifact after a real
Cargo build, including Cargo's runtime dynamic-library path, and then executes
it with the original program name and arguments. This is why existing Tauri
watching, dynamic dependencies, and relaunch behavior continue to work.

Eligible development `build` commands run Cargo as a child on a miss so Cinder
can record the successfully produced executable afterward. Cinder installs
itself as a transparent `RUSTC_WRAPPER` for that invocation and chains any
existing wrapper. Cargo remains responsible for every compiler invocation;
the wrapper records only the primary binary's exact hashed artifact, target
name, manifest directory, and build-script output directory.

## Guarded fast paths

The first optimization targets small data-only edits: equal-byte-length UTF-8
changes to either an ordinary unescaped Rust string or the data after the sole
`{}` placeholder in a simple `format!` literal. It is enabled only when all of
these checks pass:

1. macOS development `run` or single-executable `build`, with no
   release/custom profile, custom target, custom runner, artifact-reporting
   side effects, or command-line Cargo configuration;
2. the Cargo arguments and complete environment match the reference build;
3. the reference executable's size and timestamp match;
4. Cargo dep-info inputs, ancestor manifests, lockfile, and Cargo configuration
   retain their recorded size and timestamp;
5. the executable's project Rust source set is identified from Cargo's public
   dep-info, falling back to its matching hashed dep-info when necessary,
   rather than assuming a `./src` layout; public dep-info includes local
   library targets linked into the executable;
6. exactly one Rust source file changed and the entire change is contained in
   one supported string token;
7. comments, raw strings, escaped strings, byte/C strings, character literals,
   ambiguous source data, and data with multiple executable occurrences have
   been excluded;
8. the indexed offset still contains the exact expected old bytes.

If any check misses or errors, Cinder runs Cargo. There is no heuristic partial
build.

After validation, Cinder APFS-clones the executable, updates only the indexed
data bytes, ad-hoc-signs the staged Mach-O with Apple's `codesign`, removes
launch provenance/quarantine attributes, and atomically publishes one of two
sibling slots. Sibling placement preserves Cap's executable-adjacent sidecars
and relative framework paths.

For `build`, the same staged transformation is atomically renamed over Cargo's
public development executable. Cinder restores the artifact's prior
modification time after signing. The edited source therefore remains newer
than the artifact, so Cargo's own freshness logic recompiles on the next real
Cargo command. Hashed compiler artifacts and fingerprints are never rewritten.

Direct `cinder run` invocations launch a patched artifact immediately. A Cargo
shim commonly sits beneath a file watcher, where one atomic editor save may be
reported more than once. Shim invocations therefore retain a short coalescing
window: Cinder records a single-use duplicate-event token and waits so the
watcher can cancel the first runner. The replacement consumes the token and
launches the already-prepared executable. A direct watcher integration can opt
into that policy with `CINDER_COALESCE_RUN_EVENTS=1`. Launch policy is part of
the recorded run context, so states cannot cross between the two modes.

## State and correctness

Separate run and build states live under the operating system temporary
directory, keyed by the canonical project directory. Source snapshots and
published state use owner-only directory permissions. They contain:

- a SHA-256 digest of the exact Cargo invocation/environment context, never
  the raw environment values;
- a validated list and snapshot of the executable's project Rust sources;
- reference artifact metadata;
- Cargo dep-info, manifest, lockfile, configuration, project Rust, and
  build-script input metadata;
- Cargo's platform runtime dynamic-library path for fast `run` launches;
- a precomputed index of unique eligible string data.

After Cargo succeeds, source capture and indexing run outside the command's
critical path. The recorder snapshots the exact source baseline, rejects files
newer than the produced artifact, validates every input again before atomic
publication, and abandons state if a rapid subsequent save races recording.
Patched states are derived from the previous proven snapshot plus the exact
accepted source edit, rather than rereading potentially newer live source.

Build-script packages require a compiler receipt before acceleration. Cinder
records explicit `rerun-if-changed` paths, recursively fingerprints watched
directories, treats the complete environment as context, and invalidates on
other project Rust or manifest changes. An unreadable or unenumerated
build-script input disables the optimization. Malformed, missing, stale,
ambiguous, or version-mismatched state is always a Cargo miss.

Public dep-info also lets the same mechanism work from a virtual workspace
root with `-p`; generated target files and build scripts remain ordinary
invalidating inputs rather than patch candidates.

Integration tests compare transformed ordinary and `format!` strings with a
normal Cargo build; cover standalone packages, virtual workspaces, linked
library targets, build-script invalidation, compiler-wrapper chaining, and
Cargo freshness after an in-place build patch; then change compile-time context
and structural source to prove both cases fall back. Application-level
validation additionally requires successful code-signature verification, a
real window-readiness probe, and executable-adjacent sidecar discovery.

## Deliberate non-goals for this milestone

Cinder does not replace release builds, dependency management, distributed
builds, remote caches, non-Apple targets, or general Rust code generation. It
does not claim arbitrary source edits can be transformed safely. Expanding the
eligible set requires a correctness proof and end-to-end benchmark, while the
Cargo fallback remains the compatibility floor.
