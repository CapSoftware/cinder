# Architecture and compatibility boundary

## Runtime module boundaries

The runtime is divided by responsibility so Cargo compatibility checks remain
reviewable without introducing additional runtime layers:

- `run.rs` coordinates the fast paths, locking, recording, and process launch;
- `run/context.rs` identifies commands, environments, toolchains, and Cargo's
  selected compiler-unit graph;
- `run/capture.rs` records compiler receipts and normalizes Cargo arguments;
- `run/cargo.rs` owns command eligibility, configuration guards, and clean
  integration;
- `run/state.rs` loads, publishes, promotes, and matches validated state;
- `run/cache.rs` owns cache layout, revision retention, and stale-artifact
  collection;
- `run/inputs.rs` validates build inputs and Cargo output/fingerprint state;
- `run/source.rs` handles source snapshots, revision identity, and literal
  analysis;
- `run/patch.rs` performs artifact cloning, patching, and code-signature
  preservation.

These are ordinary Rust modules rather than dynamic abstraction layers. The
compiler monomorphizes and inlines across them normally, so the split changes
code ownership and reviewability without adding dispatch or I/O to a fast path.

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
unambiguous public artifact. Explicit single-bin, single-lib, and
single-example builds are supported; unselected packages that emit multiple
primary artifacts remain on Cargo.

Eligible `check` and `test --no-run` commands use the same transparent wrapper
only to capture Cargo's successful primary compiler unit. A check must resolve
to one unambiguous unit. Test reuse is narrower: `--no-run` must precede the
test-harness argument delimiter and exactly one bin, example, library, or named
integration test must be selected. Normal test execution, workspace-wide
selections, machine-readable artifact output, and ambiguous unit sets remain
entirely Cargo-owned.

## Guarded fast paths

Cinder has three independent accelerators. A proven byte transformation
handles one narrow class of new executable edits. A bounded revision history
restores an exact artifact that Cargo already built for an earlier validated
source and input state. A no-change validation path skips Cargo's graph walk
for selected check/test outputs that still have their complete exact Cargo
state.

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
launch provenance/quarantine attributes, and atomically publishes an immutable,
project-namespaced sibling. Sibling placement preserves Cap's
executable-adjacent sidecars and relative framework paths, while immutable
names prevent concurrent launches from replacing one another's artifact.

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

For the immediate direct-run policy, a current Cinder-owned immutable artifact
can be launched repeatedly after the same full context, source, input, Cargo
output, and artifact checks. Cargo's mutable public executable is deliberately
excluded, so a concurrent Cargo publisher cannot change the file after Cinder
validates it. The first no-change run after Cargo instead uses the immutable
history restoration above.

Selection and publication are serialized by the selected target's lock and
cached artifacts are digest-bound. This prevents two Cinder processes from
promoting different revisions through the same public artifact concurrently.

## Exact no-change check and test reuse

Check and selected `test --no-run` state is current-state only; it is never
placed in revision history. Before returning success without Cargo, Cinder
holds the profile's Cargo target lock and revalidates the context, selected
artifact identity, exact hashed output, rustc dep-info, Cargo fingerprint
directory plus every linked unit fingerprint in its dependency graph, complete
source revision, control inputs, and build-script inputs. Any missing or changed
component is a normal Cargo miss.

Normal `cargo test` is never intercepted. Only one explicitly selected bin,
example, library, or named integration test with command-level `--no-run` is
eligible, and arguments after `--` cannot enable the optimization. Combined or
implicit target sets remain Cargo-owned. This preserves test execution and
harness argument semantics.

Cargo sometimes translates rustc dep-info into a versioned binary file in a
fingerprint directory whose unit hash differs from the `.d` output, notably
for multi-crate-type libraries. Cinder parses only Cargo's documented version
1 encoding to discover compiler-observed environment keys. Unknown, malformed,
missing, or ambiguous encodings disable reuse. This keeps volatile shell `_`
out of ordinary contexts while still invalidating units that compile with
`env!("_")`.

Direct `cinder run` invocations launch a patched artifact immediately. A Cargo
shim commonly sits beneath a file watcher, where one atomic editor save may be
reported more than once. Shim invocations therefore retain a short coalescing
window: Cinder records a single-use duplicate-event token and waits so the
watcher can cancel the first runner. The replacement consumes the token and
launches the already-prepared executable. A direct watcher integration can opt
into that policy with `CINDER_COALESCE_RUN_EVENTS=1`. Launch policy is part of
the recorded run context, so states cannot cross between the two modes, and
immediate runs do not leave watcher tokens behind.

## State and correctness

Separate run, build, check, and selected-test states live under the operating
system temporary directory, keyed by the canonical project directory. Source
snapshots and published state use owner-only directory permissions. They
contain:

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
directories, and models Cargo's default rule by fingerprinting the package
tree when the script emits no file watches. Cargo target and Git metadata are
excluded from that default tree. Cinder content-hashes inputs whose full
filesystem identity changed, treats the build-relevant environment as
context, excludes generated target files from the source identity, and
invalidates on other project Rust or manifest changes. An unreadable,
oversized, or ambiguous build-script input disables the optimization.
Malformed, missing, stale, ambiguous, or version-mismatched state is always a
Cargo miss.

Public dep-info also lets the same mechanism work from a virtual workspace
root with `-p`; generated target files and build scripts remain ordinary
invalidating inputs rather than patch candidates.

Integration tests compare transformed ordinary and `format!` strings with a
normal Cargo build; cover standalone packages, virtual workspaces, linked
library targets, static libraries, structural revision restoration,
build-script content, new target/config topology, toolchain identity,
partial/full Cargo clean behavior, compiler-wrapper chaining, target locking,
cache pruning, global-option and built-in alias routing, Cargo shim recursion,
selected check/test invalidation, encoded Cargo dep-info, and Cargo freshness
after an in-place build patch. Real-project validation additionally checks code
signatures for executables and exact revision digests/archive readability for
library artifacts.

Revision history retains at most eight entries per project and command kind.
A global collector enforces a 4 GiB logical-byte budget across complete cache
entries, including source snapshots, state, and target-side run clones derived
from retained revisions, plus a 30-day age limit. It removes state for deleted
workspaces and prunes digest-addressed run siblings that no retained state
references. Active current state is bounded to one run, build, check, and
selected-test record per workspace and is not part of the revision-history
budget; ordinary Cargo outputs are also excluded. Superseded immutable patch
artifacts are age- and process-pruned. Crash staging across history, project
state, compiler receipts, recordings, and target artifacts is namespaced by
process ID; entries older than one hour are removed only after that process is
no longer live.

## Deliberate non-goals for this milestone

Cinder does not replace release builds, dependency management, distributed
builds, remote caches, non-Apple targets, or general first-seen Rust code
generation. It does not claim arbitrary new source edits can be transformed
safely. Expanding the eligible set requires a correctness proof and end-to-end
benchmark, while the Cargo fallback remains the compatibility floor.
