# Cargo parity benchmarks

These measurements cover Cinder's selected `build`, no-change `check`, and
selected `test --no-run` paths on real repositories. They were collected on
2026-08-15 on Apple Silicon with macOS 26.2, Cargo 1.93.0, and rustc 1.93.0.
Every Cinder sample required the expected fast-path marker and the exact
recorded artifact's SHA-256 digest was rechecked after every trial.

## Results

Revision restoration alternates between two real source revisions after Cargo
has built both. It represents undo/redo or branch switching, not a first-seen
edit.

| Repository and command | Trials | Cargo median | Cinder median | Result |
| --- | ---: | ---: | ---: | ---: |
| Bun `build -p bun_bin --lib`, real 489-line XML/runtime revision | 7 + 7 | 11.58s | 0.11s | 105.3x faster |
| Cap `build -p cap-cursor-info --example cli`, real 11-line revision | 7 + 7 | 0.42s | 0.05s | 8.4x faster |

The no-change paths skip Cargo only while the exact selected artifact,
dep-info, fingerprint graph, sources, control inputs, compiler context, and
build-script inputs remain valid.

| Repository and command | Cinder / Cargo trials | Cargo median | Cinder median | Result |
| --- | ---: | ---: | ---: | ---: |
| Cap `check -p cap-cursor-info --example cli` | 20 / 20 | 0.252718s | 0.023707s | 10.7x faster |
| Zed `check -p collab --bin collab` | 10 / 10 | 0.641372s | 0.040170s | 16.0x faster |
| Bun `check -p bun_bin --lib` | 10 / 10 | 0.106363s | 0.062303s | 1.7x faster |
| Handy `check -p handy --bin handy`, one Cargo job | 20 / 20 | 0.299787s | 0.008610s | 34.8x faster |
| Cap `test --no-run -p cap-cursor-info --example cli` | 20 / 20 | 0.243916s | 0.022815s | 10.7x faster |
| Cap `test --no-run -p cap-cursor-info --lib` | 20 / 20 | 0.218990s | 0.021506s | 10.2x faster |
| Zed `test --no-run -p auto_update_helper --bin auto_update_helper` | 10 / 10 | 0.387093s | 0.037427s | 10.3x faster |
| Handy `test --no-run -p handy --bin handy`, one Cargo job | 20 / 20 | 0.285046s | 0.008401s | 33.9x faster |

The complete sample arrays are checked in as
[`results-2026-08-15.tsv`](results-2026-08-15.tsv). The reproduction harness
uses zsh's high-resolution `EPOCHREALTIME` around each complete subprocess,
including Handy's sub-10ms paths; the artifact digest is checked immediately
after every timed trial. Cargo jobs, dev debug info, and the volatile shell
`_` value are fixed identically for Cargo and Cinder.

Repository revisions were:

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
