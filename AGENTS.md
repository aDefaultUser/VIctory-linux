# Working in this repository

Guidance for contributors and coding agents. Read [README.md](README.md) for
what the project is and [docs/architecture.md](docs/architecture.md) for how
the crates fit together.

## Assets come first

Nothing from the game or any other proprietary title is ever committed: no
assets, executable bytes, shader dumps, frame captures, extracted tables,
screenshots or logs that quote title data. Derived data lives in the cache and
data folders outside the repository, and code that writes it calls
`victory_core::ensure_outside_repo`. Tests use synthetic data made by our own
code. [docs/asset-policy.md](docs/asset-policy.md) is the full rule. If
something slips into a commit, tell the maintainers; they decide how to remove
it from history.

## Workspace conventions

- Rust workspace: `apps/victory` is the command-line program, `crates/*` the
  libraries, `xtask` the repository tooling. The toolchain and Windows target
  are pinned in `rust-toolchain.toml`.
- `victory.exe` is a Windows x86-64 program. On Linux it is cross-built and run
  under Wine as [docs/linux.md](docs/linux.md) describes. Windows-only code
  and tests sit behind `cfg(windows)`.
- Format with `cargo fmt` (`rustfmt.toml`). Clippy warnings fail CI.
- New or risky behaviour goes behind a `VICTORY_*` environment switch, off by
  default, described in a comment where it is read. Code that approximates
  what the console does carries a `shortcut:` comment.
- The shader cache is keyed by `victory_shader::TRANSLATOR_VERSION` and the
  switches the translator reads. Bump the version with any change that
  changes the translation of a shader that already translated.
- Keep the README short. Linux setup, measurements and known problems belong
  in `docs/linux.md`, ray tracing in `docs/fork-ray-tracing.md`.
- Commit subjects state the behaviour change as a plain sentence.

## Reproducing renderer behaviour

Reduce a rendering bug to a unit test next to the code it exercises, built
from synthetic inputs: a small shader program, a texture layout, a descriptor
table. Most translator tests run on the processor. Tests that need a host
GPU use the host Direct3D 12 device. Run GPU tests with `--nocapture` to
check that a device was actually used. Add `--ignored` for tests marked
`#[ignore]`. Tests that need files outside the
repository read their paths from environment variables (for example
`VICTORY_XMA_STREAMS`, `VICTORY_BCPACK_TABLES`) and are also `#[ignore]`d.

## Runs on real hardware

- A run of the title uses the tester's own game files and private state: the
  Wine prefix, shader and pipeline caches, saves, captures and logs live in
  the launcher's state, data and cache folders (`RUN_LINUX_STATE`,
  `RUN_LINUX_DATA`, `RUN_LINUX_CACHE`), outside the repository, and stay with
  the tester.
- Claims about rendering or speed come from runs of the title on real
  hardware, not from unit tests or warm-ups. Note the date, machine, GPU
  driver, Wine or Proton build and the `VICTORY_*` settings, and measure frame
  rates with frame capture turned off.
- Compatibility notes say what was tested and how: which Wine or Proton build,
  through `scripts/run-linux.sh` or another launcher, and how far the title
  got. Mark anything untested as untested, and keep dated results in
  `docs/linux.md`.
- Title runs and GPU stress tests can reset the display driver.

## Validation

These are the checks CI runs, on Windows:

```sh
cargo xtask policy
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

`cargo test` also runs the policy check (`repository_passes_policy`). Run
`cargo xtask policy` alone for documentation-only changes.

A passing test run is not proof that a test ran:

- On Linux, tests behind `cfg(windows)` are not compiled.
- Several GPU tests return early and pass when there is no host GPU.
- `#[ignore]`d tests run only when asked for.

When a change depends on one of these tests, run it where it can actually
execute, check its output with `--nocapture`, and report which tests ran and
which were skipped.
