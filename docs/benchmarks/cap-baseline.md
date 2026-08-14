# Cap desktop benchmark and first milestone

Status: local milestone validated on 14 August 2026.

## Pinned conditions

- Cap checkout: pinned Git root supplied at runtime; local path intentionally
  omitted
- Source identity: pinned and checked locally; exact revision and diff identifiers
  intentionally omitted
- Host: Apple Silicon, macOS 26.2 build 25C56
- Rust: 1.88.0 (`6b00bc388`)
- Cargo: 1.88.0 (`873a06493`)
- Power for the authoritative warm comparison: AC power
- Target directory: the same prepared `.cinder/cap-target` for Cargo and Cinder
- Readiness: the new `cap-desktop` process owns a layer-0 macOS application
  window of at least 100 by 100 points for two probes 150ms apart, including
  windows on other Spaces

The warm harness changes the token in the `format!` string in
`apps/desktop/src-tauri/src/flags.rs`. Each measured source is unique and
functionally equivalent. The timer starts after the source write completes and
stops only at the stable-window readiness condition.

## Results

All values below are seconds except sample variance, which is seconds squared.

| Workload | Trials | Raw samples | Median | Sample variance | Std. dev. |
| --- | ---: | --- | ---: | ---: | ---: |
| Cargo warm edit to running app | 7 | 12.345, 12.271, 15.785, 12.447, 12.541, 12.268, 12.387 | 12.387 | 1.668729 | 1.292 |
| Cinder warm edit to running app | 7 | 21.959, 5.107, 5.048, 4.802, 4.972, 5.088, 5.334 | 5.088 | 40.831374 | 6.390 |
| Cargo unchanged development startup | 7 | 8.581, 8.080, 8.112, 8.126, 8.083, 8.183, 8.144 | 8.126 | 0.031493 | 0.177 |
| Cargo clean development build | 3 | 85.394, 105.932, 100.967 | 100.967 | 114.822753 | 10.716 |

The primary result is **2.43x faster** by median (`12.386695 / 5.087968`).
The first Cinder warm sample deliberately falls through to Cargo because the
initial benchmark edit changes the literal's length. Later equal-length edits
use the guarded fast path. Keeping that fallback sample in the seven-sample
set makes the reported median conservative and tests mixed optimized and
unoptimized behavior.

The authoritative local JSON and logs are:

- `target/cinder-bench/results/1786727956-warm.json` and `.log`
- `target/cinder-bench/results/1786731998-warm.json` and `.log`
- `target/cinder-bench/results/1786731822-startup.json` and `.log`
- `target/cinder-bench/results/1786731515-clean.json` and `.log`

These machine-specific artifacts are ignored by Git. Historical copies created
before the privacy-safe schema must not be published. Current result JSON keeps
the dirty-state boolean, allowlisted command class, coarse toolchain and host
categories, raw timing samples, statistics, process-stage timings, and readiness
definition. It omits source revision and diff identifiers, local paths, raw
commands, Git filenames, process IDs, hostnames, device identifiers, email
addresses, and raw environment values. Child-process logs are redacted before
being written.

## Pipeline profile

The normal warm loop was roughly 8.0 seconds through Cargo's finished marker
and another 4.2 seconds to a stable application window. Focused rustc profiles
showed that the top-level library still spent about 1.05 seconds in macro
expansion, 2.44 seconds in type/coherence work, and 1.25 seconds writing
metadata. The final binary spent about 1.27 seconds building the monomorphization
graph, 2.67 seconds in code generation including about 1.16 seconds in LLVM,
and 0.70 seconds in the system linker.

The Cinder fast path removes the compiler and linker from the eligible edit.
Its measured preparation is approximately 0.48 seconds, dominated by safe
ad-hoc code signing. The remaining time is Tauri file-event coalescing and the
real Cap application launch/readiness path.

## Investigated alternatives

- `-Zthreads` did not improve this workload and larger values regressed it.
- `lld` saved only about 80 milliseconds, far below the target.
- Removing development debuginfo invalidated the workspace and produced a
  slower 16.589-second warm median.
- `-Zincremental-ignore-spans` triggered a compiler ICE on this project.
- The Cranelift backend did not support Cap's macOS panic-unwind requirements.
- A dynamic-library split pulled Cap's native frameworks and Swift symbols into
  a large, fragile link boundary and was rejected.
- Incrementally rewriting the Mach-O ad-hoc signature verified correctly, but
  macOS launch-policy validation became slower than a full ad-hoc re-sign.
- `sccache` does not address unique top-level warm edits and conflicts with
  rustc incremental compilation when used as `RUSTC_WRAPPER`.

Longer-term compiler-process retention, compiler forks, and richer artifact
transformation remain research directions. They are not silently enabled in
this milestone.

## Initial provenance stall

The first unnormalized warm-up hit the 600-second timeout before the app could
run. The final `cap-desktop` rustc process used about two CPU seconds and then
blocked in dyld loading `libproc_macro_hack` while `syspolicyd` evaluated a
`com.apple.provenance` attribute. This is recorded as a censored policy stall,
not as a compile-time sample. Numeric trials use an explicitly documented,
identical artifact-provenance state for Cargo and Cinder. It is a censored
diagnostic observation and is not included in the numeric samples above.
