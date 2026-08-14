# Cap desktop benchmark

The benchmark measures three different workloads against one pinned Cap
checkout and toolchain:

1. `warm`: edit a representative Rust file while the normal Tauri development
   watcher is running, then wait for the replacement `cap-desktop` process to
   own a stable CoreGraphics application window.
2. `startup`: invoke the normal development command with no source changes,
   then wait for `cap-desktop` to own a stable CoreGraphics application window.

The probe includes windows on other macOS Spaces. This avoids moving or
activating the developer's foreground Space while still proving that the app
created a real layer-0 window of at least 100 by 100 points.
3. `clean`: compile the development target in a fresh isolated target directory.

The warm benchmark writes a unique JavaScript comment token into the string in
`apps/desktop/src-tauri/src/flags.rs` for every trial. Every form is
functionally equivalent, but the used Rust function and resulting binary
change. Unique tokens prevent repeated A/B inputs from turning compiler-cache
hits into an artificial speedup. The harness restores the exact original bytes
in all handled exit paths and refuses to edit a dirty file. Its crash-recovery
copy is owner-readable only, lives outside the result directory, and is removed
as soon as restoration succeeds.

Build the optimized Cinder binary first:

```console
cargo build --release
```

The startup and warm modes require ownership of Cap's
`desktop-macos-runtime` coordination lease. A typical invocation is:

```console
target/release/cinder __bench-cap warm \
  --repo /path/to/Cap \
  --trials 7 \
  --runtime-lease-owner "$SESSION_ID" \
  -- pnpm dev:desktop
```

Run startup trials independently so each sample includes command
orchestration:

```console
target/release/cinder __bench-cap startup \
  --repo /path/to/Cap \
  --trials 7 \
  --runtime-lease-owner "$SESSION_ID" \
  -- pnpm dev:desktop
```

Clean builds use a fresh temporary `CARGO_TARGET_DIR` per sample and reuse only
the checkout's prepared native dependency directory:

```console
target/release/cinder __bench-cap clean \
  --repo /path/to/Cap \
  --trials 3 \
  -- cargo build --locked -p cap-desktop --timings
```

Each run writes publishable JSON measurements and a redacted child-process log
under `target/cinder-bench/results/`. Report every sample, median, sample
variance, standard deviation, minimum, and maximum. Do not compare results
collected under different source revisions, dirty diffs, toolchains, power
modes, or thermal conditions without calling out the difference.

The result schema deliberately excludes source revision and diff identifiers,
canonical repository and home paths, raw command arguments, Git status
filenames, process IDs, hostnames, hardware or battery identifiers, email
addresses, and raw environment values. Command metadata is reduced to an
allowlisted command class and argument count. The log redactor replaces
repository and user-directory paths and drops lines that may contain
credentials, URLs, or email addresses. Result and log files must pass the
privacy regression tests before they are shared.
