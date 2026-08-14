# Cinder

Cinder is a drop-in Cargo replacement built to make Rust development
dramatically faster.

It runs existing Rust projects unchanged and automatically falls back to Cargo
whenever an optimization cannot be applied safely.

```bash
cinder run
cinder build
cinder check
cinder test
```

Cinder uses your existing `Cargo.toml`, `Cargo.lock`, workspaces, features,
build scripts, proc macros, native dependencies, environment, Cargo
configuration, and Rust toolchain.

## The first result

Cinder's first real-world target is
[Cap](https://github.com/CapSoftware/Cap), a large Tauri desktop application.

On an Apple Silicon Mac, measuring from a representative warm Rust edit until
the relaunched application owned a stable macOS window:

| Workflow | Median |
| --- | ---: |
| Cargo | 12.387s |
| Cinder | 5.088s |

**2.43x faster. 59% less waiting.**

The comparison used seven trials for each workflow under the same prepared
development environment.

## What works today

The first prototype accelerates a deliberately narrow class of development
edits: safe, equal-length data changes inside simple Rust `format!` literals.

For an eligible edit, Cinder:

1. Validates the previous Cargo artifact and complete build context.
2. Clones the development executable using APFS.
3. Patches the uniquely identified data bytes.
4. Re-signs the macOS executable.
5. Hands it back to the project's existing runner and relaunch workflow.

This removes Rust compilation and linking from that edit without requiring Cap
to adopt a Cinder-specific development model.

Everything else falls back to Cargo, including structural Rust changes,
changed features or environment, custom targets, ambiguous artifacts, release
builds, `build`, `check`, and `test`.

Cinder is not yet a general-purpose 2.43x faster Rust compiler. It is an early
proof that owning the complete development loop can produce meaningful gains
while keeping Cargo compatibility and correctness central.

## Where this is going

The goal is to progressively accelerate broader classes of Rust development:

- General warm Rust rebuilds
- Compiler and dependency-graph reuse
- Faster code generation and linking
- Persistent local build infrastructure
- Better workspace scheduling and caching
- Eventually, more platforms and distributed builds

Release workflows will remain with Cargo until Cinder can support them
confidently.

## Status

Cinder is early-stage and not ready for installation yet. The project is being
developed in public, beginning with macOS on Apple Silicon.

## License

Cinder will be released under the MIT License.
