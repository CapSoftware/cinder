# Direct-replay measurement: the save loop on a proc-macro graph

Date: 2026-08-16. Cap (pinned rustc 1.88.0, routed through `cinder-tuned-1.88`),
member `cap-project` — a real workspace crate whose graph contains procedural
macros (`serde`/`specta` derives). Before the probe-verified environment
witness (`analysis/env-parity-probe-2026-08-16.md`,
`docs/architecture.md` § experimental replay), any proc-macro unit in the
message stream disabled recipe capture for the whole command, so this
workload had no direct-replay path at all.

## Protocol

Prime `cinder check -p cap-project --lib` under
`CINDER_EXPERIMENTAL_DIRECT_CHECK=1` (the first capture generates the
toolchain witness once). Then seven paired trials, each appending a comment
line to `crates/project/src/lib.rs` before each tool's run, alternating
Cinder and stock Cargo. Every Cinder trial was required to print the
`Cinder replayed Cargo's validated compiler recipe` marker (7 of 7 did).

## Results (seconds)

```
cinder direct replay  0.4494 0.4614 0.4543 0.4541 0.4727 0.4827 0.4579   median 0.4579
stock cargo check     0.6323 0.5882 0.5713 0.5684 0.5703 0.5825 0.5902   median 0.5825
```

Median 0.458s vs 0.583s — **21% faster** on the plain save loop, with the
replayed rmeta produced by the exact captured rustc invocation under the
witnessed environment (untracked expansion-time environment reads included)
and every guard active: silent-success acceptance, env-dep subset
verification, post-replay source-topology and input checks. The number is
modest because `cap-project`'s own check dominates; the replay's saving is
Cargo's planning and fingerprint walk. The qualitative change is the point:
the path exists at all for proc-macro graphs.

## Pinned-toolchain regression this trial caught

The first trial run declined every replay: witness generation had built its
probe workspace in a pinless staging directory, so rustup resolved the
default toolchain while the witness key came from Cap's pinned context — the
per-recipe `CARGO` derivation check then failed closed, permanently. The fix
snapshots one shared context for key and generation, mirrors the project's
`rust-toolchain` pin into the probe workspace, validates post-generation
that the probe actually resolved the keyed toolchain, and version-bumps the
witness format so no pre-fix witness can serve any context
(`witness_generation_follows_the_pinned_toolchain` in `tests/env_witness.rs`
locks it in). Fail-closed behavior did its job: the bug cost replays, never
correctness.

## Why there is no mid-stack cascade benchmark

The planned headline — collapsing a Cap mid-stack edit cascade to one crate
via rmeta byte-identity — is provably unviable at byte fidelity: rustc
embeds per-file source content hashes in crate metadata and imports
dependency source maps into every dependent's metadata, so an
interface-preserving edit still changes every output in the dependent cone.
The full negative proof with reproduction is
`analysis/early-cutoff-negative-2026-08-16.md`; the interface-changed and
mid-stack cases remain honestly full-cost with Cargo.
