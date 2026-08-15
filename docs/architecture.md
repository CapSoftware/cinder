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
can record the successfully produced artifact afterward. Cinder installs
itself as a transparent `RUSTC_WRAPPER` for that invocation and chains any
existing wrapper. Cargo remains responsible for every compiler invocation;
the wrapper records primary linked artifacts, their exact hashed artifact and
dep-info paths, target name/type, manifest directory, and build-script output
directory. Cinder proceeds only when target selection resolves to one
unambiguous public artifact; when a package emits both a library and binary,
the explicitly selected binary remains the primary build result.

## Guarded fast paths

Cinder has two independent accelerators. A proven byte transformation handles
one narrow class of new executable edits. A bounded revision history restores
an exact artifact that Cargo already built for an earlier validated source and
input state.

The first optimization targets small data-only edits: equal-byte-length UTF-8
changes to either an ordinary unescaped Rust string or the data after the sole
`{}` placeholder in a simple `format!` literal. It is enabled only when all of
these checks pass:

1. macOS development `run` or single-executable `build`, with no
   release/custom profile, custom target, custom runner, artifact-reporting
   side effects, or command-line Cargo configuration;
2. the Cargo arguments and build-relevant environment match the reference
   build; Cinder's internal recording controls are excluded, while volatile
   shell `_` is bound only when the selected Cargo unit graph's compiler
   dep-info or build-script output proves that it was observed;
3. the reference executable's recorded filesystem identity matches;
4. Cargo dep-info inputs, ancestor manifests, lockfile, Cargo-home and project
   configuration, and compiler/toolchain identity retain their recorded state;
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

## Exact revision restoration

After a successful Cargo build, Cinder stores the public artifact under a key
derived from the Cargo context, source/input content digests, exact Cargo
output paths, and artifact digest. History matching requires current directory
topology and every recorded source/control/build-script input to describe that
same revision. This permits arbitrary structural and multi-file changes only
when returning to a state Cargo already built; a first-seen revision still
goes through Cargo.

Publication SHA-256 verifies each cached artifact before making it read-only.
The immutable receipt records its size, modification and change timestamps,
device, and inode, so later history hits avoid rereading a large archive or
executable while still rejecting replacement, writes, permission changes, and
metadata-restored tampering. The live Rust source set is content-hashed once
and compared with each candidate's recorded digest; only the matching
candidate's snapshot is then revalidated.

Patched Mach-O artifacts preserve and post-verify ad-hoc signature identifiers,
entitlements, requirements, hardened-runtime flags, and launch/library
constraints. Non-ad-hoc signatures are not transformed because Cinder has no
implicit signer-key contract; those builds remain on Cargo.

Run restoration clones the cached executable to a project-namespaced,
digest-addressed sibling, records an equivalent immutable identity receipt,
and launches it with Cargo's recorded runtime environment. Run patches also
use immutable project-namespaced publication paths, so concurrent invocations
cannot replace the exact file another invocation is about to launch and shared
Cargo target directories cannot cross-prune another workspace. Build
restoration publishes the prior public executable or library artifact while
holding that target directory's Cargo lock. It intentionally leaves the exact
hashed artifact, dep-info, and fingerprint outputs absent/stale. This prevents
Cinder from claiming Cargo freshness it cannot reproduce; the next ordinary
Cargo build recompiles and restores Cargo's complete output set. Conversely,
no-change build reuse requires all three exact recorded outputs to remain
available.

Selection and publication are serialized by the selected target's lock and
cached artifacts are digest-bound. This prevents two Cinder processes from
promoting different revisions through the same public artifact concurrently.

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
- content digests plus a validated list and snapshot of project Rust sources;
- reference artifact filesystem identity and publication-time content digest;
- Cargo dep-info, manifest, lockfile, configuration, project Rust, and
  build-script input full filesystem identities and content digests;
- Cargo's exact hashed artifact, dep-info, and fingerprint paths;
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
directories, content-hashes inputs whose full filesystem identity changed,
treats the build-relevant environment as context, excludes generated target
files from the source identity, and invalidates on other project Rust or
manifest changes. An unreadable or unenumerated build-script input disables
the optimization. Malformed, missing, stale, ambiguous, or version-mismatched
state is always a Cargo miss.

Public dep-info also lets the same mechanism work from a virtual workspace
root with `-p`; generated target files and build scripts remain ordinary
invalidating inputs rather than patch candidates.

Integration tests compare transformed ordinary and `format!` strings with a
normal Cargo build; cover standalone packages, virtual workspaces, linked
library targets, static libraries, structural revision restoration,
build-script content, new target/config topology, toolchain identity,
partial/full Cargo clean behavior, compiler-wrapper chaining, target locking,
cache pruning, and Cargo freshness after an in-place build patch. Real-project
validation additionally checks code signatures for executables and exact
revision digests/archive readability for library artifacts.

Revision history retains at most eight entries per project and command kind.
A global collector enforces a 4 GiB logical-byte budget across complete cache
entries, including source snapshots, state, and target-side run clones derived
from retained revisions, plus a 30-day age limit. It removes state for deleted
workspaces and prunes digest-addressed run siblings that no retained state
references. Active current state is bounded to one run and one build record per
workspace and is not part of the revision-history budget; ordinary Cargo
outputs are also excluded. Superseded immutable patch artifacts are age- and
process-pruned. Crash staging across history, project state, compiler receipts,
recordings, and target artifacts is namespaced by process ID; entries older
than one hour are removed only after that process is no longer live.

## Deliberate non-goals for this milestone

Cinder does not replace release builds, dependency management, distributed
builds, remote caches, non-Apple targets, or general first-seen Rust code
generation. It does not claim arbitrary new source edits can be transformed
safely. Expanding the eligible set requires a correctness proof and end-to-end
benchmark, while the Cargo fallback remains the compatibility floor.
