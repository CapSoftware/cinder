# Unit-cache benchmarks: warm-cache cold builds

## Phase 2 series (build-script groups + OUT_DIR trees in the cache)

Same protocol as below; every arm ran with synchronous recording, so the
cache-cold arm now charges the full record pass (hashing every qualified
unit and OUT_DIR tree) inside its timed window — the production path records
in the background after the command returns.

| workload | cargo | cinder-cold | cinder-warm | restored / baseline compiles | warm vs cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| ripgrep `build` | 3.104s | 3.736s | 2.052s | 24 / 33 → 9 compiled | **-33.9%** |
| fd `build` | 3.289s | 4.898s | 2.238s | 54 / 58 → 4 compiled | **-32.0%** |
| cinder `build` | 3.215s | 3.060s | 2.293s | 22 / 26 → 6 compiled | **-28.7%** |
| Cap `-p cap-cursor-info --example cli` | 27.979s | 28.116s | 18.622s | 218 / 339 → 141 compiled | **-33.4%** |

```
ripgrep2-cargo        3.015375 3.166855 3.104012 2.999287 3.088623
ripgrep2-cinder-cold  3.731453 3.805533 3.859282 3.735847 3.735880
ripgrep2-cinder-warm  2.054494 2.084650 2.043499 2.052436 2.052161
fd2-cargo             3.989336 3.288417 3.312639 3.261856 3.289981
fd2-cinder-cold       4.914469 4.901475 4.769780 5.098628 4.898247
fd2-cinder-warm       2.216695 2.238471 2.260165 2.322209 2.152144
cinder2-cargo         3.558007 3.267089 3.215400 3.106377 3.132765
cinder2-cinder-cold   3.068624 3.009792 3.060055 2.949243 3.060542
cinder2-cinder-warm   2.399049 2.278647 2.361368 2.292478 2.259063
cap-cursor2-cargo        30.197937 26.814444 27.978958
cap-cursor2-cinder-cold  27.154008 29.699215 28.116064
cap-cursor2-cinder-warm  17.992627 18.622168 18.912257
```

Warm cache sizes: ripgrep 93.9 MB, fd 125.9 MB, cinder 41.1 MB, Cap cursor
310.4 MB. Every arm's final artifact digest identical across its rounds.
fd is the clearest picture of the lifted boundary: Phase 1 restored 32 of
58 compiles for a 7% end-to-end win because `libc`, `serde`, and the other
build-scripted crates recompiled natively; with their groups cached, 54 of
58 restore, the four remaining compiles are the `fd` binary itself plus
OUT_DIR-reading libraries (whose rlibs embed generated-file paths and are
deliberately never cached), and the end-to-end win is 32%. The remaining
141 compiles in Cap's graph are workspace members, git-source dependencies,
proc-macro dylibs, and OUT_DIR-reading libraries — each an evidence-based
exclusion, not an accident. The cinder-cold rows show the synchronous
record pass costing roughly 0.6–1.6s on these graphs; the async production
path hides it after the command returns.

## Handy whisper/CMake qualification (Phase 2 negative result, fully traced)

`cinder build -p transcribe-cpp-sys` in Handy (10m07s: the whisper.cpp CMake
tree, 425 output files, 34 MB OUT_DIR) recorded 13 qualified units across 9
packages — `cmake`, `cc`, `serde_json`/`serde_core` script pairs, and the
plain libraries — but **no `transcribe-cpp-sys` group**, for a reason more
fundamental than CMake path relocatability:

1. `serde_core`'s *library* reads OUT_DIR (`# env-dep:OUT_DIR=…` plus a
   tracked generated source in its encoded dep-info), so its rlib embeds
   generated-file absolute paths and can never be cached byte-faithfully
   (the OUT_DIR-reader rule, verified earlier by a real cross-project rlib
   divergence).
2. `serde_json`'s library depends on `serde_core`'s library. Restoring it
   could never help: at planning time Cargo marks `serde_core` dirty
   (missing output), and dirtiness propagates regardless of the eventual
   byte-identical native recompile. The cache correctly refuses it.
3. `transcribe-cpp-sys`'s build-script **compile** unit lists `serde_json`
   as a build-dependency. The same cascade therefore reaches the script
   itself: even a restored script pair would be re-run by Cargo because its
   own dependency closure cannot be made fresh.

So the ten-minute CMake execution is trapped behind a one-second
`serde_core` recompile — a Cargo planning-model boundary (no early cutoff),
not a relocatability failure and not a cache defect. The whisper OUT_DIR
tree itself was within every structural cap (largest file 2.8 MB, depth 15
of 24); its `.a` archives would have excluded it at the relocatability scan
(donor paths inside binary files) had qualification reached that stage.
This is the recorded negative for the Handy target: packages whose
build-script *build-dependencies* transitively include an OUT_DIR-reading
library are unreachable for the unit cache under Cargo's planning rules.
The in-session early-cutoff work (Phase 3) is the mechanism that can absorb
such cascades, not the cross-project cache.

## Phase 1 series (library units only — kept for scope comparison)

Date: 2026-08-16. Machine: Apple Silicon (aarch64-apple-darwin), 8 jobs,
`CARGO_INCREMENTAL=0` in every arm so final binaries are byte-comparable
(dev-profile incremental session ids otherwise make any two plain Cargo
builds differ; see `analysis/unit-restore-proof-2026-08-16.md`).

## Protocol

`benchmark.zsh <label> <repo> <rounds> <jobs> -- <build args>` runs three
arms interleaved per round, every trial from a fully cleaned target
directory:

- **cargo** — stock `cargo build` (baseline);
- **cinder-cold** — `cinder build` with an empty unit cache, wiped before
  every trial (measures cache-cold overhead plus tuned-toolchain routing);
- **cinder-warm** — `cinder build` with a pre-populated unit cache, restored
  from a saved copy before every trial (the headline arm).

Per trial we record wall-clock seconds, the number of `Compiling` lines, the
restored-unit count from the trace line, and the final artifact digest. The
background recorder is awaited between trials so it never overlaps a timed
window. The cargo arm uses the stock toolchain while the cinder arms route
through the tuned toolchain, so cargo-vs-cinder is the end-to-end product
comparison and cinder-cold-vs-cinder-warm isolates the cache effect at a
fixed toolchain. Repositories are the pinned clones under
`target/cinder-bench/repos`; the Cinder workload uses the committed-state
clone `cinder-copy` because cleaning the real repository would delete the
bench clones.

## Results (ripgrep/fd/cinder 5 rounds, Cap members 3 rounds; medians, raw arrays below)

| workload | cargo | cinder-cold | cinder-warm | restored / baseline compiles | warm vs cargo | warm vs cinder-cold |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep `build` | 3.108s | 3.016s | 2.206s | 16 / 33 (48%) | **-29.0%** | -26.9% |
| fd `build` | 3.363s | 3.789s | 3.129s | 32 / 58 (55%) | **-7.0%** | -17.4% |
| cinder `build` | 3.284s | 2.591s | 2.545s | 10 / 26 (38%) | **-22.5%** | -1.8% |
| Cap `-p cap-cursor-info --example cli` | 24.647s | 22.931s | 20.269s | 143 / 339 (42%) | **-17.8%** | -11.6% |
| Cap `-p cap-muxer-protocol --lib` | 1.893s | 2.098s | 2.024s | 3 / 10 (30%) | +6.9% | -3.5% |

Raw arrays (seconds):

```
ripgrep-cargo        3.230527 3.137691 3.081439 3.046947 3.107177
ripgrep-cinder-cold  3.042035 3.009868 3.017301 2.995399 3.016314
ripgrep-cinder-warm  2.236926 2.259874 2.205723 2.143630 2.154575
fd-cargo             4.005825 3.294512 3.290293 3.363075 3.433215
fd-cinder-cold       3.775899 3.759640 3.789037 3.924770 3.973507
fd-cinder-warm       3.118021 3.156568 3.158575 3.067182 3.128882
cinder-cargo         3.606881 3.284292 3.229610 3.264215 3.311493
cinder-cinder-cold   2.633713 2.545579 2.577361 2.854878 2.590919
cinder-cinder-warm   2.592795 2.549191 2.500877 2.449924 2.544685
cap-cursor-cargo        27.038017 22.914977 24.646774
cap-cursor-cinder-cold  22.930700 22.771251 24.344677
cap-cursor-cinder-warm  20.196659 20.269374 21.338227
cap-muxer-cargo         2.176517 1.892866 1.847656
cap-muxer-cinder-cold   2.100619 2.098071 1.838910
cap-muxer-cinder-warm   2.068834 2.024285 2.003383
```

Warm cache size: ripgrep 64.0 MB, fd 60.3 MB, cinder 26.7 MB, Cap cursor
148.0 MB, Cap muxer 0.8 MB. Every arm's final artifact digest was identical
across all of its rounds. The Cap series ran with the pinned 1.88 toolchain
(stock for the cargo arm, `cinder-tuned-1.88` routing for the cinder arms)
and used `CINDER_SYNCHRONOUS_STATE_RECORDING=1`, charging all recording
inside the timed windows. Full per-trial logs and count files are retained
under `target/cinder-bench/unit-cache-results/`.

Cap's cursor workload is the clearest picture of the Phase 1 boundary: 143
restored units still leave 196 compiles, and the wall-clock only drops 12%
at a fixed toolchain because the graph's heavy crates sit above build
scripts (`serde`, `libc`, `objc2`, windowing stacks), which Phase 1 must
recompile natively. The cap-muxer row is a small-workload negative result
kept deliberately: three restored units cannot beat process and validation
overhead on a two-second build.

## Reading the numbers honestly

- **Cache-cold overhead is not measurable above toolchain effects.** On
  ripgrep and cinder, the cache-cold arm is *faster* than stock cargo (the
  tuned toolchain dominates); on fd it is ~12% slower, which matches fd's
  known weaker tuned-toolchain response, not cache cost: the restore step on
  a cold cache measures ~10ms and recording runs after the timed window.
- **Wall-clock gains track the critical path, not the restored fraction.**
  fd restores 55% of its compilations but its cold build is dominated by the
  large `fd` crate itself plus build-scripted dependencies, so the wall-clock
  gain is small. ripgrep's restored units sit on the wider part of its graph,
  so 48% restored turns into 27% wall-clock at a fixed toolchain.
- **Phase 1 scope caps the restored fraction.** Only units whose transitive
  closure is free of build scripts qualify (a missing build-script unit
  dirties its dependents at Cargo's planning time), so `libc`, `serde`,
  `proc-macro2` and everything above them recompile natively. Phase 2
  (build-script outputs under the same evidence rules) exists to lift this.
- **One nondeterministic crate was ratcheted out.** Under the tuned
  toolchain's parallel frontend, `bstr`'s rlib bytes differed between two
  independent cold ripgrep builds (stock cargo was byte-deterministic for
  every unit in the same test). The divergence ratchet permanently marked
  that key unstable; such crates compile normally forever after. This is the
  designed response, not a benchmark anomaly.
