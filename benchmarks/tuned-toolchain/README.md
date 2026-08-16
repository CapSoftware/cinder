# Tuned development toolchain benchmarks

Collected 2026-08-16 on Apple Silicon, macOS 26.2. The stock reference is
rustup stable `rustc 1.93.0 (254b59607 2026-01-19)`. The tuned candidate,
`cinder-tuned`, is built from the identical 1.93.0 release source with
ThinLTO, `codegen-units = 1`, profile-guided optimization (profiled on real
ripgrep and Cinder builds), the Cranelift backend available, rustdoc included,
and Homebrew LLVM 21.1.8 — the same LLVM version the stock binary links. Its
version string deliberately presents as stable with the stock hash and date,
because real repositories version-sniff the compiler: a `-dev` channel string
changed lint behavior in Bun's workspace and broke its build.

Raw per-sample data for every table is in [`raw/`](raw/).

## Out-of-the-box result: `cinder build` versus `cargo build`

Cinder routes eligible development commands through the tuned toolchain
automatically (separate `target/cinder-tuned` namespace, automatic stock
fallback; see docs/architecture.md for the eligibility and fallback contract).
Medians of 3 cold builds and 6 unique-edit rebuilds per tool; the tuned
namespace was verified present and the stock namespace absent on every Cinder
sample; every Cinder cold time includes Cinder's own capture and recording
overhead.

| Repository | Cold `cargo` | Cold `cinder` | Result | Edit `cargo` | Edit `cinder` |
| --- | ---: | ---: | ---: | ---: | ---: |
| ripgrep 14.1.1 | 3.56s | 3.12s | 12% faster | 0.504s | 0.504s |
| cinder (21k-line crate) | 3.49s | 2.48s | 29% faster | 0.449s | 0.421s |
| fd (HEAD, 1.93-compatible lockfile) | 3.50s | 3.37s | 4% faster | 0.378s | 0.381s |

The honest split: fresh and big-crate compilation gains 4–29%; small-crate
edit rebuilds are link- and orchestration-bound and do not improve through
compiler tuning. That loop is served by Cinder's validation fast paths
(9–25x on no-change and reuse workloads; see benchmarks/cargo-parity/).

## Per-knob attribution (5-cold medians, 6-edit medians per cell)

| | ripgrep cold | cinder cold | fd cold | ripgrep edit | cinder edit | fd edit |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| stock | 3.53 | 3.47 | 3.56 | 0.519 | 0.460 | 0.378 |
| tuned+PGO | 3.40 | 3.27 | 3.59 | 0.509 | 0.422 | 0.375 |
| tuned+PGO+`-Zthreads=8` | 3.07 | 2.54 | 3.40 | 0.516 | 0.419 | 0.384 |
| tuned+PGO+Cranelift | 2.71 | 2.63 | 3.11 | 0.536 | 0.415 | 0.397 |

Attribution: PGO contributes a consistent 3–9%; the parallel frontend
contributes most where large crates dominate the cold graph (Cinder's crate:
-27%); Cranelift gives the largest cold-build wins on dependency-heavy graphs
(ripgrep: -23%) but is slightly *slower* on small incremental edits and is
therefore not part of the default configuration.

## Correctness gates

- fd: 265 tests pass under the tuned toolchain.
- ripgrep: all 22 workspace harnesses including doctests pass (exit 0).
- Cinder's own suite (218 tests) passes with the integration active.
- Cranelift failed a gate: compiling Cap's `schemars` dependency under
  `-Zcodegen-backend=cranelift` produced a type error consistent with a
  build-script capability probe resolving differently. Cranelift is opt-in
  (`CINDER_TUNED_BACKEND=cranelift`) until qualified per project.

## Retained negative results

- The tuned build **without** PGO measured identical to stock across every
  knob on ripgrep (all medians within noise): Apple's shipped compiler is
  well-optimized, and assembling ThinLTO/CGU1 alone reproduces, not beats, it.
- `-Zthreads=8` measured nothing on small-crate edit loops (535.9ms vs
  528.2ms) and nothing on tiny-crate builds; its value is confined to graphs
  with large crates.
- Cranelift measured nothing on a tiny dependency-free crate (87.0ms vs
  87.3ms) and regressed small edit loops by 3–5%.
- Linker configuration on the edit loop: `-Wl,-no_deduplicate` within noise
  (0.447s vs 0.451s), `-ld_classic` 5% slower than the default ld-prime.
  No configuration-level link win exists on this machine; incremental
  Mach-O linking remains an open research spike, not attempted this session.
- Current nightly toolchains cannot compile Bun (lint drift plus E0599/E0658
  API drift) — version proximity, not optimization level, gates real repos.

## The Cap unpin experiment (retained negative)

Overriding Cap's `1.88.0` pin on a buildable member
(`build -p scap-targets`, 3 colds and 6 edits per config,
[`raw/cap_unpin_matrix.json`](raw/cap_unpin_matrix.json)): Cap compiles
*fastest under its own pin*. Cold medians were 15.11s pinned, 15.83s under
stock 1.93 (+4.8%), and 16.32s under tuned 1.93 (+8.0%). Two lessons: the
compiler version a project pins can genuinely be faster for its workload than
a newer stable, which validates pin-respecting routing as a performance
decision and not only a compatibility one; and our PGO profile (trained on
ripgrep and Cinder builds) plus `-Zthreads` coordination overhead can mildly
regress many-small-crate dependency graphs it was not trained on. The correct
route for Cap is a tuned build of its own pinned 1.88 source (Homebrew
LLVM 20 is installed and version-matched), with PGO trained on Cap itself.

## Per-pin tuned builds: Cap on rustc 1.88

Following the unpin experiment, `cinder-tuned-1.88` was built from Cap's
pinned 1.88.0 release source (commit 6b00bc388, Homebrew LLVM 20.1.8 external,
same ThinLTO/CGU1 recipe) with PGO trained on real Cap member builds rather
than generic workloads. Knob qualification on Cap
([`raw/cap_188_quali.json`](raw/cap_188_quali.json),
[`raw/cap_188_decision.json`](raw/cap_188_decision.json)): the tuned base
beat Cap's stock pin on cold builds in every round measured — 15.99s → 15.13s
(scap-targets build) and 18.18s → 17.39s (cursor-info example check) at
5-cold medians — with 10-sample edit loops flat to 3.2% faster. Apparent edit
regressions in the first 6-sample round did not replicate and are recorded as
noise. `-Zthreads=8` was inconsistent on Cap (helped one workload, hurt the
other) and ships disabled for the 1.88 build via its per-toolchain flags
file; the 1.93 build keeps it. Correctness gates: `cap-muxer-protocol`
(7 real tests) and `scap-targets` pass identically under the tuned build.

End-to-end through Cinder with per-pin routing active
([`raw/cap_e2e_final.json`](raw/cap_e2e_final.json)), 3 colds, 6 edits, and
4 no-change runs per cell, the tuned namespace verified on every Cinder
sample:

| Cap workload | `cargo` (pinned) | `cinder` | Result |
| --- | ---: | ---: | ---: |
| `build -p scap-targets` cold | 15.24s | 14.33s | 6% faster |
| `check -p cap-cursor-info --example cli` cold | 19.09s | 15.98s | 16% faster |
| unique-edit rebuilds | 0.535s / 0.374s | 0.552s / 0.352s | +3% / −6% (mixed) |
| no-change reruns | 0.240s / 0.274s | 0.016s / 0.013s | 15x / 21x faster |

The mixed edit row reflects Cinder's known first-seen capture tax partially
offsetting the compiler gain on one workload; cold attribution across
sessions varies with machine state, so the conservative claim is the isolated
4–5% compiler attribution plus Cinder's validation multipliers, not the
best paired cell.

## Handy validation (retained mixed result)

Handy (unpinned Tauri app, real CMake/whisper native build, one persistent
compiler warning) exposes two limits honestly
([`raw/handy_e2e.json`](raw/handy_e2e.json)). First, its link-heavy edit loop
measured 3.46s through Cinder versus 3.10s through Cargo — the 1.93 tuned
build's `-Zthreads=8` plus Cinder's first-seen capture tax regress this
workload, which means knob qualification must eventually be per-project, not
only per-toolchain; until then `CINDER_STOCK=1` is the escape hatch and this
row is the recorded cost. Second, plain `cinder build` cannot record state for
Handy because its library target is multi-crate-type
(staticlib/cdylib/rlib), which the receipt-gap rule counts as a gap and
refuses — correct fail-closed behavior, with multi-crate-type root receipts
recorded as the enhancement that would lift it. The selected-binary path is
unaffected: a no-change `cinder build --bin handy` reuses the tuned-namespace
binary without invoking Cargo, replaying Handy's real warning first — the
diagnostic-replay, tuned-routing, and reuse layers composing in one command.

## Corrections to earlier baselines

Three of the four original bench repositories pin toolchains that rustup
auto-installs silently: Bun pins `nightly-2026-07-20`, Cap pins `1.88.0`,
Zed pins `1.97.1`. Earlier bun_bin numbers recorded this session
(cold 51.3s, big-crate edit 2.25s median) therefore describe Bun's pinned
nightly, not stock stable, and are labeled as such here. Pinned repositories
are ineligible for the tuned toolchain by design — the pin always wins — and
per-pin tuned builds (a 1.88-based build for Cap is feasible with Homebrew
LLVM 20) are the recorded next step.
