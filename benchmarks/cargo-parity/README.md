# Cargo parity benchmarks

These measurements cover Cinder's selected `build`, no-change `check`, selected
`test --no-run`, and repeated package or workspace-selected `test --lib` paths on real
repositories. The main series was
collected on 2026-08-15 and the wrapper-free observer series on 2026-08-16, on
Apple Silicon with macOS 26.2, Cargo 1.93.0, and rustc 1.93.0, except that
Zed honored its checked-in Rust 1.97.1 toolchain.
Every exact-reuse Cinder sample required the expected fast-path marker and the
recorded Cargo artifact's SHA-256 digest was rechecked after every trial.
Patched executables and restored revisions used the content, archive, and
code-signature checks described with their individual series below.

## Final candidate results

The 2026-08-16 candidate was rebuilt after every compatibility fix and measured
from freshly published Cinder state. The no-change paths skip Cargo only while
the exact selected artifact, dep-info, complete reachable fingerprint/artifact
graph, sources, control inputs, compiler context, topology, and build-script
inputs remain valid. Input-graph and Cargo-output validation run concurrently;
bounded state files are opened and decoded once rather than through thousands
of tiny reads.

| Repository and command | Cinder / Cargo trials | Cargo median | Cinder median | Result |
| --- | ---: | ---: | ---: | ---: |
| Cap `build --locked -p cap-cursor-info --example cli` | 15 / 15 | 0.237942s | 0.016238s | 14.65x faster |
| Cap `check --locked -p cap-cursor-info --example cli` | 15 / 15 | 0.248205s | 0.015187s | 16.34x faster |
| Cap `test --no-run --locked -p cap-cursor-info --example cli` | 15 / 15 | 0.236475s | 0.014766s | 16.01x faster |
| Cap `test --lib -- --nocapture`, 7 real `cap-muxer-protocol` tests | 21 / 21 | 0.127383s | 0.005036s | 25.29x faster |
| Cap workspace root, `test -p cap-muxer-protocol --lib -- --nocapture`, same 7 tests | 21 / 21 | 0.126685s | 0.005935s | 21.35x faster |
| Current Cap snapshot, same workspace command after receipt/input binding | 21 / 21 | 0.134066s | 0.013876s | 9.66x faster |
| Bun `check --locked -p bun_opaque --lib` | 15 / 15 | 0.065033s | 0.005965s | 10.90x faster |
| Bun `test --lib -- --nocapture`, real crate with 0 unit tests | 21 / 21 | 0.058378s | 0.004340s | 13.45x faster |
| Bun workspace root, `test -p bun_opaque --lib -- --nocapture`, 0-test control | 21 / 21 | 0.059677s | 0.007478s | 7.98x faster |
| Handy `check --locked -p handy --bin handy -j 1` | 15 / 15 | 0.278439s | 0.024392s | 11.42x faster |
| Handy `test --no-run --locked -p handy --bin handy -j 1` | 15 / 15 | 0.271463s | 0.024393s | 11.13x faster |
| Zed `check -p collab --bin collab` | 10 / 10 | 0.587644s | 0.033711s | 17.43x faster |
| Zed `test --no-run -p auto_update_helper --bin auto_update_helper` | 10 / 10 | 0.341766s | 0.005333s | 64.08x faster |

A separate real `cinder run -- --help` comparison used Cinder itself. All 30
stdout payloads matched the Cargo reference byte-for-byte. Cinder measured
8.085ms median versus Cargo's 212.333ms, or 26.26x faster. The first Cinder
sample was a retained 181.347ms outlier; it was not removed from the raw array.

### Repeated real library-test execution

The test-execution series alternated command order across 21 Cargo/Cinder pairs
and timed each complete subprocess. Every Cinder sample required the direct
validated-test marker, no Cargo completion marker, a successful exit, and the
expected libtest result. Every Cargo sample required Cargo's completion marker
and the same libtest result. The test executable itself was never skipped.

Cap's real `cap-muxer-protocol` library ran all seven existing unit tests on
every sample. Cargo measured 127.383ms median (125.781ms p10, 132.224ms p90),
while Cinder measured 5.036ms (4.761ms p10, 5.459ms p90), a 25.29x speedup.
This is a real nonempty test suite and the primary product result.

The same seven tests were then invoked from Cap's real workspace root with
`-p cap-muxer-protocol`. Before implementation, 15 interleaved pairs measured
127.199ms for Cargo and 127.909ms for Cinder, a 0.56% compatibility overhead;
every Cinder sample went through Cargo. After Cinder began recording Cargo's
selected package working directory, 21 interleaved pairs measured 126.685ms for
Cargo and 5.935ms for Cinder, a 21.35x speedup. Every sample still ran all seven
tests, and every Cinder sample required the direct marker and no Cargo marker.
The first pre-change Cinder sample was a retained 562.632ms outlier; the robust
median, not a selectively trimmed sample set, is reported above.

After the final guard was added to require the selected package manifest in
Cinder's unchanged receipt-backed input graph, the exact release candidate was
remeasured on current Cap revision `ccd8df6b98e67f3e022613a3067ce1154c7fc7f8`.
Cargo and Cinder used two clean local clones with their normal in-repository
target directories so neither command could touch the other's fingerprints.
Across 21 alternating pairs, Cargo measured 134.066ms and Cinder 13.876ms, or
9.66x faster. All 42 samples ran the same seven tests; every Cinder sample
required direct execution with no Cargo completion marker. This refresh is
slower than the older pinned-snapshot series above, and is reported separately
rather than substituted into it.

Bun's real dependency-free `bun_opaque` crate has no unit tests. It remains a
useful orchestration control, but is not presented as proof of faster test
work. Cargo measured 58.378ms median and Cinder 4.340ms, a 13.45x speedup. The
zero-test limitation is stated explicitly rather than hidden behind the timing.
From Bun's workspace root, explicit `-p bun_opaque` selection measured 59.677ms
for Cargo and 7.478ms for Cinder across 21 pairs, or 7.98x. This remains a
zero-test orchestration control.

The broader existing `test --no-run` path was remeasured after the direct-test
guards were added. Fifteen interleaved Cap example pairs measured 236.475ms for
Cargo and 14.766ms for Cinder, or 16.01x. This is within 0.6% of the previous
14.678ms Cinder median and provides a current-candidate regression check rather
than assuming the new eligibility work was free.

Revision restoration alternates between two real source revisions after Cargo
has built and Cinder has validated both. It represents undo/redo or branch
switching, not a first-seen edit. Cinder state was moved aside before the final
preparation so no older history entry could satisfy a sample.

| Repository and command | Trials | Cargo median | Cinder median | Result |
| --- | ---: | ---: | ---: | ---: |
| Bun `build -p bun_bin --lib`, real 489-line XML/runtime revision | 7 + 7 | 12.06s | 0.22s | 54.82x faster |
| Handy `build --bin handy`, real Tauri revision | 7 + 7 | 4.12s | 0.31s | 13.29x faster |
| Cap `build -p cap-cursor-info --example cli`, real 11-line revision | 7 + 7 | 0.41s | 0.22s | 1.86x faster |

The final Handy literal-edit benchmark changed the real Clap description to
five unique equal-length values. Every Cinder artifact contained the requested
bytes and passed strict code-signature verification.

| Scenario | Cinder / Cargo trials | Cargo median | Cinder median | Result |
| --- | ---: | ---: | ---: | ---: |
| Handy real CLI literal edit, `build --locked --bin handy -j 1` | 5 / 10 | 3.657015s | 0.718890s | 5.09x faster |
| Handy unique first-seen structural edits | 10 / 10 | 3.924738s | 4.098579s | Cinder 4.43% slower |

Every structural Cinder sample showed no fast-path marker and compiled through
Cargo's ordinary path. After the timed command, the asynchronous recorder was
allowed to finish and an immediate Cinder reuse proved the published state.
All resulting artifacts passed strict code-signature verification. This is the
measured compatibility tax for unsupported edits, not a speedup claim. Timed
intervals began before the source write, and the exact source plus a real Cargo
build were restored outside each interval.

### Follow-up history-miss optimization

History lookup originally decoded and fully validated every retained revision
before learning that its recorded source digest could not match a first-seen
edit. The follow-up candidate reads only the small context, artifact, and source
metadata files first, hashes the live source set once, and fully loads only a
source-matching candidate. All authorization checks still run before a history
hit can be used.

Ten alternating Handy trials changed the real user-visible Clap description to
unique, different-length values. Every Cinder trial compiled `handy`, emitted
no fast-path marker, contained the requested description, completed state
recording, and passed strict code-signature verification. Cargo controls used
the same edits and validation.

| Scenario | Trials | Median | Result |
| --- | ---: | ---: | ---: |
| Source-prefilter candidate | 10 | 4.005s | Cinder |
| Direct Cargo control | 10 | 3.960s | Cinder 45ms, or 1.14%, slower |

Because this workload differs from the earlier structural-edit control, its
1.14% result is not presented as a direct 4.43%-to-1.14% improvement. A second
five-pair run compared the source-prefilter candidate against the exact prior
review tree on the same edits. The candidate measured 4.020s median and the
prior tree 4.050s, a small 0.74% candidate advantage. One prior-tree sample was
4.34s and remains in the raw array. Trace timing separately reduced the
source-miss history search from 19ms to 3ms. The validation trace and artifact
checks show that the lower tax did not come from skipping Cargo or the
authorization chain; five A/B pairs are not enough for a broad performance
claim.

The complete sample arrays are checked in as
[`results-2026-08-15.tsv`](results-2026-08-15.tsv). The reproduction harness
uses zsh's high-resolution `EPOCHREALTIME` around each complete subprocess,
including Handy's sub-10ms paths; the artifact digest is checked immediately
after every timed trial. Cargo jobs, dev debug info, and the volatile shell
`_` value are fixed identically for Cargo and Cinder.

Repository revisions were:

- final-candidate Cap `70f535d084afce06138c4bfa7607c68600152348`;
- current-release Cap revalidation `ccd8df6b98e67f3e022613a3067ce1154c7fc7f8`;
- Cap `70f535d084afce06138c4bfa7607c68600152348` for check/test, and
  `3c8e4a36fdb3c52f99201143d39bccbe5c72da23` for the example-build revision;
- Zed `f0685e0a4f4ea45a5062388ef043984bc2fe5ccf`;
- Bun `4bf3f364568b75d5eda689e5019ea4d959192dbd`;
- Handy `9e534a3d399b937382322acda9d30b7a302c7d42`.

## Reproduction

Build Cinder first, force one honest selected compilation if Cargo is already
fresh, then run the harness. For example:

```console
cargo build --release
cd /path/to/Cap
/path/to/cinder/target/release/cinder clean -p cap-cursor-info
cd /path/to/cinder
benchmarks/cargo-parity/benchmark.zsh check cap-cursor /path/to/Cap 20 8 -- \
  -p cap-cursor-info --example cli
```

Use mode `test` for `test --no-run`; the harness supplies `--no-run` itself.
It refuses to report a Cinder sample without the expected reuse marker or if
the recorded artifact digest changes.

Two attempted measurements were intentionally excluded. Zed's full `collab`
test harness required an optional Apple Metal toolchain unavailable on the
machine. Handy's nested `transcribe-cpp-sys` CMake build deadlocked when the
benchmark forced eight Cargo jobs; one Cargo job completed normally, so only
the one-job Handy comparisons are reported. Neither failed attempt is counted
as a Cinder performance result.

## Opt-in evidence overhead

The privacy-safe `CINDER_USAGE=1` recorder was measured separately on Cinder's
own exact no-change selected check. Two hundred enabled and two hundred disabled
full process invocations were alternated against one prepared state and target:

| Collection | Median | p95 | Mean |
| --- | ---: | ---: | ---: |
| Disabled | 7.762ms | 8.532ms | 7.826ms |
| Enabled | 7.824ms | 8.485ms | 7.927ms |

The enabled median was 0.062ms, or about 0.8%, higher. The p95 difference was
within run noise. All 200 enabled records decoded as complete current-reuse
events with no malformed records. This measures the cost of collecting evidence;
it is not an estimate of time saved by Cinder.

## Experimental first-seen selected checks

An isolated tracked-source Cap workspace replayed the real five-line
`cap-cursor-info` cursor-hotspot commit in both directions. Cargo and Cinder used
separate prepared target directories, trial order alternated, normal incremental
compilation remained enabled, and every Cargo sample proved that the selected
crate compiled. The cold preparation check took 20.610s.

The first 20-edit series measured the complete Cinder command, including state
validation, target locking, exact compiler replay, artifact validation, source
snapshotting, and state publication:

| Command | Median | p95 | Mean |
| --- | ---: | ---: | ---: |
| Cargo check | 304.229ms | 309.579ms | 304.213ms |
| Experimental Cinder check | 84.419ms | 99.606ms | 103.851ms |

Cinder accepted 19 of 20 direct replays and conservatively fell through to
Cargo once, producing a 445.587ms maximum. All 20 resulting metadata artifacts
matched the independent Cargo target byte-for-byte. An immediate second 20-edit
series accepted 20 of 20 replays, matched all 20 artifacts, and measured
83.461ms Cinder versus 301.889ms Cargo median. Five additional fresh recipe
captures accepted their first changed edit in all five trials.

A separate exact-invocation control measured the selected rustc process at
45.862ms median versus 312.106ms for Cargo over 20 edits, with 20 byte-identical
artifacts. The gap between 45.862ms and the full Cinder result is Cinder's real
validation and publication cost; it is not removed from the product number.

This is evidence for an opt-in prototype, not a default speed claim. These
numbers came from the earlier temporary-wrapper capture candidate and do not by
themselves validate the current observer candidate. The current implementation
does not install a Cinder compiler wrapper: on macOS it observes the compiler
child of the Cargo process it started and omits the recipe if observation is
unavailable or incomplete. It still rejects selected-package build scripts,
compiler warnings, compiler errors, changed source topology, corrupt recipes,
and ambiguous output back to Cargo. Fresh observer-candidate measurements are
required before promotion. Projects with their own compiler wrapper remain
Cargo-owned.

### Rejected pre-guard observer candidate

The current release candidate was then rebuilt and measured on an isolated,
clean checkout of current Cap revision
`ccd8df6b98e67f3e022613a3067ce1154c7fc7f8`. The edit changed a real cursor
hotspot value in `cap-cursor-info`; each timed interval began before the source
write and ended after the complete process. Cargo and Cinder used separate
prepared target directories for timing, normal incremental compilation stayed
enabled, and every sample required its expected Cargo-compile or Cinder-replay
marker.

| Command | Trials | Median | p95 | Mean |
| --- | ---: | ---: | ---: | ---: |
| Cargo check | 20 | 293.887ms | 299.266ms | 293.940ms |
| Experimental Cinder check | 20 | 129.809ms | 137.330ms | 130.867ms |

All 20 observer-candidate replays were accepted, a 2.26x median speedup that
saved 164.078ms per edit in this selected crate. In five additional independent
fresh-capture trials, Cinder observed the selected compiler without a wrapper,
replayed the next semantic edit, and produced metadata byte-for-byte identical
to Cargo rebuilding that same target. Comparing the same output path avoids
mistaking target-directory-specific Rust metadata for a parity failure.

Those artifact comparisons were necessary but not sufficient. A subsequent
adversarial test used a real procedural macro that read `CARGO_PKG_VERSION`
through ordinary `std::env::var`. That access is not present in rustc dep-info,
and the observed compiler arguments do not contain Cargo's compiler environment.
The pre-guard replay therefore succeeded with the wrong expansion. The current
implementation rejects the entire active graph when Cargo reports any
`proc-macro` target, including an already-fresh dependency. Current Cap contains
such targets, so the guarded implementation falls back to Cargo and the 2.26x
number above is retained only as rejected prototype evidence. It is not a speed
claim for the current implementation.

Observer capture cost was measured separately over 20 paired first-seen Cargo
compilations per mode. Both modes used Cinder's ordinary Cargo-message capture;
only the observer was toggled. Default capture measured 325.071ms median and
observer capture 327.274ms. The paired observer tax was 1.678ms median, about
0.52%. State publication completed between samples but was outside the measured
command, matching Cinder's normal asynchronous first-seen path.

The observer-overhead result still measures the cost of process inspection, but
the replay results do not validate a promotable path. In a separate real Handy
run, Cargo reported the selected package's build-script `OUT_DIR`; Cinder
correctly kept the changed source check on Cargo rather than replaying it. These
Cap and Handy fallbacks preserve parity, but they also show that the guarded
first-seen experiment does not yet add broad real-project performance value.

### Guarded real-project qualification

The procedural-macro guard was then measured, rather than merely asserted, on
ten additional unique Cap edits. Every Cinder sample showed the real
`Checking cap-cursor-info` marker, the procedural-macro rejection marker, and
no replay marker. State recording completed outside the timed command, as it
does normally. The source-change miss path checks the selected source digest
before the expensive project-topology validation, because a changed source with
no eligible recipe must use Cargo regardless of topology. This reduced Cinder's
median from about 343ms in the initial guarded run to 322.755ms. Cargo measured
309.180ms median, leaving 13.575ms, or 4.39%, of safe fallback overhead.

The actual Cargo dependency graphs of Cap, Handy, and Zed contained no workspace
package that both lacked a build script and had no transitive procedural macro.
Bun had four conservative candidates. `bun_opaque`, a real dependency-free Bun
FFI utility crate, was selected for positive qualification rather than creating
a benchmark-only project. Five independent capture/replay/Cargo controls
produced byte-for-byte identical metadata. Twenty complete edit-and-check
samples then measured:

| Command | Trials | Median | p95 | Mean |
| --- | ---: | ---: | ---: | ---: |
| Cargo check | 20 | 99.389ms | 100.153ms | 99.154ms |
| Experimental Cinder check | 20 | 361.951ms | 368.350ms | 362.940ms |

Cinder was 3.64x slower. The replay itself succeeded, but exact workspace-input
validation dominated a crate Cargo could incrementally compile in about 99ms.
This is retained as negative product evidence rather than silently replaced.

The follow-up candidate records a digest of relevant Rust/Cargo path names for
each project directory. An atomic editor save still changes the source
directory's filesystem identity, but Cinder can rescan that one subtree instead
of walking the complete Bun workspace. On the same real crate and source edit,
the traced input-validation stage fell from 121ms to 3ms while the compiler
replay remained successful.

Twenty new alternating edit-and-check pairs changed a real inline panic message
to a unique value before every command. Timing began before the atomic source
replacement and ended after complete synchronous state publication. Every
Cinder sample required the replay marker, exactly one project-subtree rescan
and no full rescan; every Cargo sample required `Checking bun_opaque`. The metadata
artifact digest changed after every sample in both groups.

| Command | Trials | Median | p95 | Mean |
| --- | ---: | ---: | ---: | ---: |
| Cargo check | 20 | 92.698ms | 94.598ms | 92.621ms |
| Experimental Cinder check | 20 | 70.716ms | 81.690ms | 72.990ms |

That is a 1.31x median speedup, or 23.7% less edit-to-completed-check time, on
this narrow real workload. It establishes that the local topology design can
create product value where the earlier implementation did not. It does not
establish broad first-seen value: the path remains opt-in while Cargo compiler
process parity for procedural macros remains unresolved and more eligible real
crates are qualified.

After hardening macOS rustup-proxy observation, five additional independent
fresh recipe captures replayed five more real panic-message edits. Each replay
had the Cinder marker, each same-source Cargo control rebuilt `bun_opaque`, and
all five metadata pairs were byte-for-byte identical. A fixed minimal
environment avoided measuring shell-session churn. Coarse command-only timing
(`time -p`, 10ms resolution) was 60–70ms for Cinder and 80–90ms for Cargo, with
70ms and 90ms medians. This is current-candidate regression qualification, not
a replacement for the 20-trial edit-to-completed-check result above.

After recipe format 6 replaced ambiguous process-tail evidence with the
post-replay proof against Cargo's previous dep-info, one more independent Bun
capture/replay/Cargo control at revision `4bf3f364568b75d5eda689e5019ea4d959192dbd`
changed the real `opaque_deref` panic message twice. The fresh capture came from
Cargo, the second edit used the guarded compiler replay, and the new dep-info
reported zero environment accesses. Same-source Cargo then emitted the real
`Checking bun_opaque` marker and produced the identical metadata SHA-256,
`16e7cc1f88f667eef9006ac33aad86589b7cec4811769a188eb9c63cb33ff9e1`.
This untimed control confirms that the stricter current recipe format preserves
the qualified Bun path rather than merely adding fallbacks. The clone was
restored to a clean revision afterward.

### Real Cap reuse with the procedural-macro guard active

The final guarded candidate was also rerun against Cap revision
`70f535d084afce06138c4bfa7607c68600152348`. Two real edits changed the macOS
arrow hotspot in `cap-cursor-info`, using
`check -p cap-cursor-info --example cli`. Both changed-source commands showed
the real `Checking cap-cursor-info` marker and the unsupported-target-graph
trace, including the second edit after the procedural-macro dependencies were
already fresh. Neither command published or replayed a compiler recipe.

The resulting Cargo-owned artifact and input graph then qualified Cinder's
ordinary unchanged-state reuse. Fifteen Cinder/Cargo pairs were run
interleaved under the same fixed minimal environment. Each interval covered
the complete command process; no source write was included because this series
specifically measures the no-change developer loop.

| Command | Trials | Median | p95 | Mean |
| --- | ---: | ---: | ---: | ---: |
| Cargo check | 15 | 218.174ms | 237.830ms | 219.402ms |
| Guarded Cinder check | 15 | 16.560ms | 17.240ms | 16.401ms |

That is a 13.18x median speedup, saving 201.614ms per no-change check in this
real Cap target while changed source remains Cargo-owned. It is genuine value,
but deliberately narrower than a claim that Cinder accelerates novel edits in
proc-macro-heavy applications.
