# Architecture and compatibility boundary

## Runtime module boundaries

The runtime is divided by responsibility so Cargo compatibility checks remain
reviewable without introducing additional runtime layers:

- `run.rs` coordinates the fast paths, locking, recording, and process launch;
- `run/context.rs` identifies commands, environments, toolchains, and Cargo's
  selected compiler-unit graph;
- `run/capture.rs` normalizes Cargo and compiler arguments;
- `run/cargo.rs` owns command eligibility, configuration guards, and clean
  integration;
- `run/messages.rs` consumes Cargo's stable JSON artifact stream and records
  exact outputs without changing the compiler environment;
- `run/observe.rs` performs best-effort, read-only observation of a Cargo child
  on macOS for the opt-in compiler-recipe experiment;
- `run/replay.rs` validates, stores, and replays those experimental recipes;
- `run/state.rs` loads, publishes, promotes, and matches validated state;
- `run/cache.rs` owns cache layout, revision retention, and stale-artifact
  collection;
- `run/inputs.rs` validates build inputs and Cargo output/fingerprint state;
- `run/source.rs` handles source snapshots, revision identity, and literal
  analysis;
- `run/patch.rs` performs artifact cloning, patching, and code-signature
  preservation.
- `usage.rs` records opt-in, privacy-safe acceleration outcomes without adding
  I/O to default command execution.
- `run_windows.rs` and `usage_windows.rs` provide a compile-checked strict Cargo
  proxy on Windows. They expose no artifact fast path until Windows has an
  equivalent filesystem-identity, publication, and process-launch proof.

These are ordinary Rust modules rather than dynamic abstraction layers. The
compiler monomorphizes and inlines across them normally, so the split changes
code ownership and reviewability without adding dispatch or I/O to a fast path.

## Cargo remains the authority

Cinder accepts Cargo-style commands, but does not reimplement dependency
resolution, feature selection, build scripts, proc macros, toolchains, or the
workspace graph. Standard Cargo remains authoritative for those contracts.
Unknown commands and all currently unsupported optimization cases are delegated
with their arguments, environment, exit status, stdout, and stderr intact.
Linux and Windows currently take this proxy path for every command. This keeps
Cargo command parity portable without treating a successful cross-compile as
runtime acceleration evidence.

For eligible development `run` commands, Cinder adds a target runner through a
Cargo `--config` value. Cargo still selects and builds the target and prepares
its runtime environment. The runner records the correct artifact after a real
Cargo build, including Cargo's runtime dynamic-library path, and then executes
it with the original program name and arguments. This is why existing Tauri
watching, dynamic dependencies, and relaunch behavior continue to work.

Eligible development `run`, `build`, `check`, and selected test commands run
Cargo as a child on a miss. Cinder asks Cargo for
`json-render-diagnostics`: Cargo continues rendering normal status and compiler
diagnostics to stderr, while its documented stdout records identify exact
compiler artifacts and build-script output directories. Cinder consumes only
known Cargo records through `build-finished`; unknown output is forwarded, and
all later bytes are application output and are forwarded without parsing.

Before enabling capture, Cinder establishes the exact selected package set.
For a normal package-directory invocation without `-p`, it matches Cargo's
artifact messages to the canonical manifest path after verifying that the
manifest has a package table and no workspace table that changes the selected
set. Workspace package selections are resolved to Cargo's exact package ID;
that result is cached under the hashed command context so repeated first-seen
edits do not spawn a second Cargo process just for resolution. The cache is
owner-local, bounded, context-bound, cleared by `cinder clean`, and
self-invalidates whenever the message stream has no artifact for the cached ID.
A workspace-root `check` — a manifest with a `[workspace]` table, no package
selection, and no target selectors — instead selects every member unit whose
canonical manifest lies under the canonical invocation root, with build-script
output directories resolved per receipt by package ID. Every `members` and
`default-members` declaration must be a relative path or glob without a
parent-directory component: a member declared outside the root would produce
units the root matcher can never select, so its edits could not invalidate the
recorded state, and such workspaces disable capture entirely. A root-package
manifest with explicit `default-members` can make any other command build
members outside one package's dependency closure, so those shapes disable
capture entirely rather than recording only part of what Cargo built.

Unsupported Cargo implementations, ambiguous default workspace members,
complex package selections that cannot be resolved exactly, and failed probes
disable capture and preserve the original command unchanged. The successful
message path records primary linked artifacts, exact hashed artifacts and
dep-info, target type, manifest directory, and selected build-script output.
Explicit single-bin, single-lib, single-example, and narrow check/test targets
remain supported exactly as before. A default target set that produces several
root units — a lib-and-bin package `check` or `build`, or a workspace-root
`check` — records every selected unit as a root, bounded to 256 roots; the
lexicographically first artifact is the primary root and the rest become
sibling roots whose reachable unit graphs are merged into one validated Cargo
output graph. Procedural-macro units are ordinary recorded `check` roots: each
of Cargo's metadata and compiled-macro units carries its own hashed artifact,
dep-info, and fingerprint, and macro-time environment reads are bound by the
whole-environment context identity. `build` recording still refuses
proc-macro roots, and the experimental compiler replay keeps rejecting any
graph containing a proc-macro target. A selected unit that yields no artifact
receipt for any reason other than being a build script — a multi-crate-type
target, an unknown layout — is a counted receipt gap, and a default target
set with any gap refuses to record: a state that silently omitted a built
unit could later reuse while that unit fails. Each sibling keeps its own artifact identity, content digest,
dep-info, hashed artifact, and fingerprint directory, all bounded and rejected
on truncation or trailing data. A root whose package declares a build script
must carry that script's exact output receipt, and every executed build script
must be reachable from some root's fingerprint graph. Because an unchanged
sibling keeps its older artifact timestamp, each root bounds its own consumed
sources: a source newer than both that root's artifact and the staged
invocation written before the Cargo child spawned may be a mid-command edit
the root never saw, and the recording is abandoned rather than binding new
source content to a stale sibling artifact. Control inputs, which Cargo itself
may rewrite after the command starts, use the widest bound — the staged
invocation or the earliest root artifact Cargo rebuilt during this command. Multi-root states are
current-state-only: never retained in revision history, never patched, never
directly executed, and never replayed through the experimental compiler path.
A reuse hit revalidates every root and prints one line naming the root count,
while single-root states keep the existing marker byte-for-byte. Ambiguous
output sets and root counts beyond the bound remain on Cargo.

Neither default capture nor the opt-in recipe experiment installs or replaces
`RUSTC_WRAPPER`, and Cinder's internal receipt controls are not placed in
Cargo's environment. Projects with an existing environment- or
configuration-defined compiler wrapper remain entirely Cargo-owned, preserving
arbitrary wrapper behavior rather than assuming it is safe to skip. On macOS,
the opt-in experiment only observes processes started by Cinder's own Cargo
child. If observation is unavailable or misses the selected compiler process,
no recipe is recorded and the next edit remains Cargo-owned.

Test reuse remains narrower. Build-only reuse requires command-level `--no-run`
before the test-harness argument delimiter and exactly one selected bin,
example, library, or named integration test. Direct execution is narrower
again: only one standard library harness selected by `test --lib`, optionally
with one exact `-p`/`--package` selection, is eligible. Named and integration
targets, `harness = false`,
positional Cargo test filters, custom target runners, workspace-wide selections,
machine-readable artifact output requested by the user, and ambiguous unit sets
remain entirely Cargo-owned.

## Guarded fast paths

Cinder has three independent accelerators. A proven byte transformation
handles one narrow class of new executable edits. A bounded revision history
restores an exact artifact that Cargo already built for an earlier validated
source and input state. A no-change validation path skips Cargo's graph walk
for selected check/test outputs that still have their complete exact Cargo
state; for the narrow standard-library test shape, it then runs the exact
validated Cargo-built harness rather than suppressing test execution.

Cargo replays cached compiler warnings and manifest diagnostics on every
no-change command, so a reuse hit that printed only Cinder's marker would
silently drop user-visible output. After Cargo succeeds, the recorder therefore
runs a hidden no-change Cargo pass with piped output: the recorded command for
`build` and `check`, the command with `--no-run` and without harness arguments
for `test`, and the equivalent selected `build` for `run`. The pass must exit
successfully and print exactly one `Finished` status line; the raw stderr bytes
before that line are stored with the state, bounded to 4 MiB per variant. Any
compile, download, or other status marker proves Cargo had not converged, and
the entire recording is abandoned rather than published without proven replay
bytes. Transient `Blocking` lock-wait lines from a concurrent Cargo are
removed from the stored region: the pass still completed normally, rendered
diagnostics gutter every source line so a diagnostic body cannot forge that
status shape, and an uncontended future no-change command prints the same
bytes without them.
Before any hidden pass spawns, every source consumed by the staged receipts
must predate the staged invocation file; a newer source means the user kept
editing after the command, and the pass is skipped so the detached recorder
can never start a real, unrequested background compile. The status
classification is a conservative list of Cargo's current status verbs under
the pinned Cargo versions Cinder is validated against; a future Cargo status
verb unknown to that list would be recorded into the replay region rather than
rejected, which is a known limitation of parsing rendered output — Cargo's
JSON mode cannot replace it because it suppresses the per-crate warning-count
summary line.
When the region is nonempty and the user pinned Cargo's color choice through
`CARGO_TERM_COLOR` or a configured `term.color` other than `auto`, one pass
records the single pinned rendering; otherwise two passes record Cargo's plain
and ANSI renderings, and a hit selects the variant matching whether Cinder's
stderr is a terminal. Cargo's automatic choice additionally honors `NO_COLOR`,
`CLICOLOR_FORCE`, and a dumb terminal; Cinder does not model that rendering
matrix, so a nonempty region recorded under any of those variables abandons
the recording instead. Every exact-state reuse hit, direct test execution,
restored revision, and fast run replays the recorded bytes before Cinder's
marker; the reuse test suite verifies the replayed warning region byte-for-byte
against Cargo's own no-change replay. States recorded before this contract
existed are ordinary misses. The equal-length string patch and the experimental
compiler-recipe replay produce source Cargo never rendered, so both require a
state with no recorded diagnostics and otherwise stay on Cargo.

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
metadata-restored tampering. A lightweight prefilter reads only each candidate's
context, artifact, source manifest, and recorded source digest. The live Rust
source set is content-hashed once and compared with those digests; only a
source-matching candidate is fully decoded. Its context, target, source,
artifact, Cargo-output, input-graph, history-key, snapshot, and cached-artifact
checks all remain mandatory before publication.

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

## Exact no-change check and test execution

Check and selected `test --no-run` state is current-state only; it is never
placed in revision history. Before returning success without Cargo, Cinder
holds the profile's Cargo target lock and revalidates the context, selected
artifact identity, exact hashed output, rustc dep-info, Cargo fingerprint
directory plus every linked unit fingerprint in its dependency graph, complete
source revision, control inputs, and build-script inputs. Any missing or changed
component is a normal Cargo miss.

The three bounded persisted graphs (inputs, project topology, and Cargo
outputs) are each opened and read once, size-checked before allocation, decoded
from an in-memory slice, and rejected on truncation or trailing data. Cargo's
reachable output graph and the independent input graph are then validated in
parallel while the target lock is held. A panic or error on either side is a
cache miss, never a successful decision.

For command-level `--no-run`, only one explicitly selected bin, example,
library, or named integration test is eligible, and arguments after `--` cannot
enable that build-only optimization. Combined or implicit target sets remain
Cargo-owned.

For standard `test --lib`, including an exact package selection from a workspace
root, the first successful command remains fully Cargo-owned. Cinder injects an
inline target runner only after rejecting every project or environment runner.
Cargo still selects and builds the test target, sets its working directory and
runtime environment, and invokes the runner. Cargo's artifact receipt identifies
the selected manifest directory; the runner requires its observed working
directory to match that canonical package directory and rejects a nonstandard
library harness before publishing state. On a later exact-state hit, Cinder
holds Cargo's target lock, repeats the full artifact/source/input/output
validation above, re-resolves the recorded package directory without following
a replacement symlink, verifies that the recorded path still matches its
publication-time digest, and requires its canonical `Cargo.toml` to remain an
exact entry in Cargo's unchanged receipt-backed input graph. It then applies
the current arguments after `--` and executes the recorded harness with Cargo's
captured runtime library environment. A failure is returned as status 101 and
is never retried. This removes Cargo orchestration, not test execution.

Cargo sometimes translates rustc dep-info into a versioned binary file in a
fingerprint directory whose unit hash differs from the `.d` output, notably
for multi-crate-type libraries. Cinder parses only Cargo's documented version
1 encoding to discover compiler-observed environment keys. Unknown, malformed,
missing, or ambiguous encodings disable reuse. This keeps volatile shell `_`
out of ordinary contexts while still invalidating units that compile with
`env!("_")`.

Some multi-crate-type packages, including Handy's staticlib/cdylib/rlib target,
publish an unhashed dependency file beside several unhashed public libraries.
Cinder accepts that layout only when the hash-indexed dep-info is absent, the
descriptor yields one exact Cargo unit name, and the dependency file itself
names the real compiler outputs. Ambiguous unhashed output sets stay with Cargo.

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
- content digests plus a validated list and snapshot of project Rust sources,
  where each listed source also records its publication-time filesystem
  identity and content digest so a no-change hit trusts unchanged identities,
  re-reads only an identity-changed file, and treats a touched-but-identical
  file as the same revision (this trust model assumes the accelerated
  platform's nanosecond filesystem timestamps, as APFS provides);
- reference artifact filesystem identity and publication-time content digest;
- Cargo dep-info, manifest, lockfile, configuration, consumed Rust source, and
  build-script input full filesystem identities and content digests;
- Cargo's exact hashed artifact, dep-info, and fingerprint paths;
- Cargo's platform runtime dynamic-library path for fast `run` and selected
  library-test launches;
- a precomputed index of unique eligible string data;
- the proven no-change diagnostic replay: nothing, one pinned-color rendering,
  or Cargo's plain and ANSI renderings, each bounded and rejected on
  truncation or trailing data;
- the recorded sibling roots of a multi-target command, each with its exact
  artifact identity, content digest, dep-info, hashed artifact, and
  fingerprint directory, bounded and rejected on truncation or trailing data.
  States recorded before sibling roots existed are ordinary misses.

After Cargo succeeds, source capture and indexing run outside the command's
critical path. The recorder snapshots the exact source baseline, rejects files
newer than the produced artifact, validates every input again before atomic
publication, and abandons state if a rapid subsequent save races recording.
Patched states are derived from the previous proven snapshot plus the exact
accepted source edit, rather than rereading potentially newer live source.

Build-script packages require exact Cargo artifact and output-directory
receipts before acceleration. Cinder retains Cargo's complete active
build-script graph, not only the selected package's script, and records every
script's explicit `rerun-if-changed` paths. It recursively fingerprints watched
directories and models Cargo's default rule by fingerprinting that script's
package tree when the script emits no file watches. Cargo target and Git
metadata are excluded from that default tree. Cinder content-hashes inputs
whose full filesystem identity changed, treats the build-relevant environment
as context, excludes generated target files from the source identity, and
invalidates on consumed Rust or manifest changes. A transitive build script's
receipt is not mistaken for a selected-package `OUT_DIR`: the selected package
must provide that directory only when its own manifest declares a build script,
while all transitive receipt directories must still exactly match Cargo's
fingerprint graph. A missing mapping or an unreadable, oversized, or ambiguous
build-script input disables the optimization.

Project topology separately records Rust/Cargo path names and directory
identities so a new auto target, manifest, build script, or configuration file
cannot hide behind unchanged compiler dep-info. Existing unrelated Rust file
contents are not treated as compiler inputs merely because they share a large
workspace; Cargo dep-info remains authoritative for content dependencies.
Each recorded directory also carries a digest of the relevant Rust/Cargo path
names below it. When an editor atomically replaces one source file, Cinder
rechecks only the smallest changed recorded subtree and accepts it only when
that path-name digest is unchanged. A missing subtree digest, an unreadable
local scan, or old-format state falls back to the full project digest; an added
or removed Rust/Cargo path invalidates immediately. Parent-first ordering avoids
rescanning descendants already covered by a changed ancestor.
Ancestor directories outside the project use a separate digest containing only
Cargo-discoverable manifest, lockfile, toolchain, and configuration names. An
unrelated sibling created in a shared temporary directory therefore requires
only a constant-size control check, while a new ancestor `.cargo/config.toml`
still invalidates the state. External control checks never suppress the stricter
project-subtree validation even when the ancestor is a path prefix.
Internal directory symlinks are traversed once by canonical target, while their
logical aliases remain explicit inputs. Cycles are bounded, removal or retarget
invalidates, and any directory symlink escaping the workspace/package fails
closed. This supports Bun's tracked `src/cli -> runtime/cli` layout without
allowing an alias to bypass topology validation.

Malformed, missing, stale, ambiguous, or version-mismatched state is always a
Cargo miss.

## Local evidence boundary

When `CINDER_USAGE=1` is explicitly enabled, Cinder appends a fixed-size local
record after each `run`, `build`, `check`, or selected `test` acceleration
decision. Records contain a command enum, outcome enum, bounded decision time,
and day number. They contain no repository identifier, path, arguments, source,
environment values, artifact name, or Cargo output. The evidence file lives in
the user's state directory, is owner-only, is bounded to 16 MiB, and ignores
malformed or partial records.
Recording failures never change command success or prevent Cargo fallback.

The control variable is excluded from Cinder's build-context identity and is
removed before Cargo, rustc, build scripts, and launched artifacts can observe
it. This makes evidence collection a property of Cinder itself rather than an
input to the program being built. Default execution remains free of evidence
I/O; enabled overhead is measured separately from fast-path decision time.

The same boundary applies to Cinder's explicitly enumerated implementation
controls, including Cargo selection, tracing, synchronous test recording,
fast-path disabling, receipt transport, and legacy wrapper handoff. They do not
participate in context identity and are removed from Cargo, rustc, build-script,
and launched-program environments. Unknown `CINDER_*` variables are not treated
as controls: they remain ordinary user environment and continue to invalidate
state, so Cinder cannot silently hide a project's own variable by prefix alone.

## Experimental first-seen check replay

When `CINDER_EXPERIMENTAL_DIRECT_CHECK=1` is set, Cinder captures the exact
compiler executable, arguments, and working directory for a successfully
validated selected unit by observing the direct children of the Cargo process
that Cinder started. It does not change Cargo's command, compiler command, or
the environment visible to rustc and build scripts. Cargo-generated values that
the selected unit actually read are restored from rustc's dep-info; `OUT_DIR`
comes from Cargo's build-script message. User environment remains inherited
from Cinder and is already included in the command-context identity. Neither
recipe persistence nor re-execution occurs by default.

On macOS, the executable stored in the recipe comes from the same
`KERN_PROCARGS2` snapshot as the argument vector. This preserves the invoked
`rustc` path even when rustup uses proxy hard links and `proc_pidpath` reports a
different link name such as `cargo`. Independent before/after process-path
reads reject a process that exited or changed executable while it was being
observed; stale recipe formats are invalidated rather than replayed.

The restoration allowlist includes Cargo's documented crate variables,
including `CARGO_TARGET_TMPDIR` for integration-test and benchmark units, plus
Cargo's platform dynamic-library path when rustc recorded it as a source
dependency. Any other environment dependency must exactly match the value
inherited by Cinder (including absence), or recipe publication is rejected.
Recipe format changes invalidate older captures.

Compiler arguments are not the complete compiler input when procedural macros
are involved. A stable procedural macro can call `std::env::var` while expanding
the selected crate, including for values Cargo adds only to the compiler
environment, without causing rustc to list that access in dep-info. Tracked
accesses are covered without storing arbitrary environment data: Cinder
records per-recipe salted fingerprints of every present and absent environment
dependency in Cargo's previous rustc dep-info, and after a replayed rustc
succeeds, each `env!` or `option_env!` access in the new dep-info must match a
prior fingerprint — a newly introduced access or changed value rejects the
replay and lets Cargo establish a new baseline. The fingerprints contain
neither raw keys nor raw values and are format-bound so older recipes cannot
bypass the check.

Untracked reads are covered by a probe-verified environment witness
(`analysis/env-parity-probe-2026-08-16.md`). Once per toolchain and launch
context, Cinder compiles an offline sentinel probe workspace — primary and
dependency shapes, a build script, and a procedural macro that executes at
expansion time — through real Cargo with Cinder itself as the compiler
wrapper, recording the complete environment of every compiler process. Every
injected variable must classify against a witnessed derivation (manifest
field, version part, crate name, manifest location, launch-layer constant,
`OUT_DIR`, the dynamic-loader path forms, or the jobserver variable, which is
never restored); an unclassifiable variable marks the toolchain unsupported.
During the probe builds the `KERN_PROCARGS2` environment tail — which has no
`argc`-style count separating `envp` from Apple's auxiliary strings — is
parsed and must reproduce the wrapper-recorded environment byte for byte,
proving the parser exact on this machine before it is ever trusted. A real
capture then stores a full-witnessed recipe only when every observed variable
is byte-inherited from Cinder's controlled base environment, witnessed with
its derivation cross-checked, or the jobserver name; a proc-macro unit in
Cargo's message stream restricts recipe publication to full-witnessed
recipes, so an untracked expansion-time read observes exactly the values
Cargo would have provided. Unwitnessed variables, unprobed cargo versions,
missing observations, or malformed target-kind fields leave such graphs
refused exactly as before. Guessing likely Cargo variables is still
deliberately not treated as parity — only witnessed evidence is.

The macOS process-inspection interfaces used by the observer are best-effort
and subject to operating-system change. Observation failure, an incomplete
process record, an unmatched artifact, or any unsupported platform simply
omits the recipe. It never weakens the normal Cargo fallback.

The experimental path is narrower than ordinary check reuse. It requires the
same compiler context, available exact Cargo outputs, unchanged non-source
inputs and source topology, Cargo's target lock, and a selected package without
an `OUT_DIR` from its own build script or a procedural macro anywhere in the
active graph. The replay runs the exact compiler recipe against the same Cargo
target outputs. Cinder accepts only a successful compiler process whose output
consists exclusively of rustc's internal artifact notifications. Warnings,
errors, malformed output, source-set changes, state publication failures, and
every ambiguous case are discarded and rerun through Cargo so Cargo remains
responsible for user-facing diagnostics.

Successful replay publishes new Cinder validation state but deliberately does
not fabricate or update Cargo fingerprints. A later real Cargo command can
therefore rebuild conservatively. The experimental control variable is excluded
from Cinder's context identity and removed before Cargo or rustc can observe it.
This path remains opt-in until realistic cross-project, build-script,
diagnostic, concurrency, and artifact-parity evidence is broad enough for a
default Cargo-compatibility claim.

Public dep-info also lets the same mechanism work from a virtual workspace
root with `-p`; generated target files and build scripts remain ordinary
invalidating inputs rather than patch candidates.

Integration tests compare transformed ordinary and `format!` strings with a
normal Cargo build; cover standalone packages, virtual workspaces, linked
library targets, static libraries, structural revision restoration,
build-script content, new target/config topology, toolchain identity,
partial/full Cargo clean behavior, compiler-wrapper fallback, target locking,
cache pruning, global-option and built-in alias routing, Cargo shim recursion,
selected check/test invalidation, encoded Cargo dep-info, and Cargo freshness
after an in-place build patch. Cargo-message coverage also verifies human
warnings/errors, build-script environment isolation, static libraries,
test-harness output layouts, transactional rejection of unknown artifact
layouts, workspace-wrapper and nested-manifest fallback, asynchronous
recording, and JSON-shaped program output after build completion. Real-project
validation additionally checks code signatures for executables and exact
revision digests/archive readability for library artifacts.

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

## The tuned development toolchain

On macOS, Cinder can route eligible development `build`, `check`, `test`, and
`run` commands through a locally built `cinder-tuned` rustup toolchain: the
same rustc source as the active stock toolchain, rebuilt with ThinLTO,
codegen-units 1, profile-guided optimization, and the parallel frontend
enabled through `RUSTC_BOOTSTRAP` at Cinder's own invocations. Tuned
artifacts are functionally equivalent but not byte-identical to stock
Cargo's. That deliberately amends the earlier byte-identity contract for
development artifacts only, and two rules keep the amendment safe: tuned
commands always build inside a separate `target/cinder-tuned` namespace so
tuned and stock artifacts can never mix, and release or otherwise
profile-selecting builds always use the stock toolchain unchanged.

Routing is fail-closed. It applies only when no explicit toolchain choice is
in force (no `+toolchain` argument, no `RUSTUP_TOOLCHAIN`), no
compiler-affecting environment is present (`RUSTC`, `RUSTDOC`,
`RUSTC_BOOTSTRAP`, wrappers), the user has not selected a target directory,
the command names no profile, target, or `-Z` flag, and `CINDER_STOCK` is
unset. An unpinned project targets the default `cinder-tuned` build. A
project whose nearest `rust-toolchain` file pins a plain version — `1.88` or
`1.88.0`, never a dated nightly, `stable`, or a full toolchain name — can
instead route through a matching per-pin build named `cinder-tuned-1.88` or
`cinder-tuned-1.88.0`, but only when that build is installed, the pinned
stock toolchain itself is already installed (Cinder never triggers a rustup
download), and the probe below passes; any miss honors the pin with stock
rustup behavior exactly as before. The tuned compiler must pass a cached
health probe: its `--version` line must contain the reference compiler's
full version line — the stock default for unpinned projects, the pinned
stock toolchain for pinned ones; the same-source guarantee that real
projects enforce implicitly through version sniffing — and it must compile a
trivial crate, proving the sysroot. Probe verdicts are cached per tuned
toolchain against the filesystem identities of the tuned compiler, the
reference compiler, and rustup's settings, so an unchanged installation
costs a few stats per command and multiple tuned builds coexist.

Each tuned build declares its own compiler flags in a bounded
`cinder-tuned-flags` file inside the toolchain directory: the default
stable build carries `-Zthreads=8` (the parallel frontend measured faster
there), while the Cap-matched `cinder-tuned-1.88` carries none, because
`-Zthreads` measured inconsistently on that workspace's dependency graphs.
An absent or empty file means no extra flags; an oversized or malformed file
makes that tuned build ineligible rather than guessed at.
`RUSTC_BOOTSTRAP` is set only when an applied flag requires it.

Failures never strand a command. A tuned child that cannot launch disables
tuned routing machine-wide and reruns the command through stock; a tuned
child that dies to a signal (a crashed compiler) disables tuned routing for
that project and reruns through stock. Ordinary compile errors are trusted
as-is — the compiler shares the stock compiler's source — so a red build is
never compiled twice. Markers and the tuned namespace are cleared by
`cinder clean`. The optional Cranelift debug backend
(`CINDER_TUNED_BACKEND=cranelift`) stays opt-in: it measurably speeds cold
builds but slightly slows small incremental edits, and one real dependency
(`schemars` under Cap) miscompiled its capability probe under Cranelift
during qualification, so it is not part of the default contract. Because the
routed environment is applied before the command context is computed, tuned
and stock fast-path states separate naturally through context identity, and
the recorder's hidden diagnostic passes inherit the same tuned world they
must validate against.

## Deliberate non-goals for this milestone

Cinder does not replace release builds, dependency management, distributed
builds, remote caches, non-Apple artifact acceleration, or general first-seen
Rust code generation. It does not claim arbitrary new source edits can be
transformed safely. Expanding the eligible set requires a correctness proof and
end-to-end benchmark, while the Cargo fallback remains the compatibility floor.
