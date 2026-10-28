# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- `--no-vectors` no longer panics the generator (`doseq`, `for`, `zipmap`,
  and `select-keys`/`merge` key lists are now gated on vectors) and the
  entry point hashes a list instead of a vector.
- `for` comprehensions are suppressed by `--no-loops` (bare `(range n)`
  remains allowed as inert data).
- Loop accumulator growth is capped: an update mentioning a growable
  accumulator twice or more gets few trips, fixing out-of-memory hangs
  like `(vec (interleave acc acc))` (seed 496).
- `(range n)` sizes are shrunk to fit the remaining budget and charged
  per element; previously an 800k range was free.
- `--max-string-len` (≤ 4096) and `--max-vector-len` (≤ 1024) ceilings,
  plus clamped draws for library users who skip validation.
- `drive` warns when `--no-checksum` is forwarded (it disables the oracle).
- Timeouts now kill the runner's whole process group instead of orphaning
  the JVM behind `sh -c`.
- Runner paths are shell-quoted; signal deaths report `signal N` instead
  of `exited with None`; partial output survives lingering grandchildren.
- A generator panic fails one seed instead of aborting the whole drive.
- Failure dirs are cleaned before saving and save errors are reported.
- Seed specs are capped at 1,000,000 total seeds.
- `reduce` distinguishes a broken `--test` command (exit 2) from an
  uninteresting input (exit 1).
- `Rng::below` uses Lemire's unbiased mapping (was modulo) and
  `Rng::chance` is exact at 0.0/1.0; unused `Rng::fork` removed.
- CI pins the Rust toolchain, builds `--locked`, and caches cargo state.

### Added

- `reduce --timeout-is-interesting`: reduce hangs by treating test
  timeouts as interesting (pair with a short `--timeout`).
- Reducer drop pass: lists also try each child (`(println X)` to `X`).
- `drive` exit code 3 for errors without any clean comparison
  (0 = agree, 1 = divergences, 2 = bad usage).
- Structural invariant tests: suppression matrix, loop growth cap, range
  budget, emit/reparse roundtrip, case/map shapes, zero options, RNG
  golden stream; end-to-end suppressed-feature run matrix.

### Changed

- The RNG stream changed (`below`/`chance` mappings): same seed, new
  programs. Re-baseline any pinned checksums.

## [0.1.0] - 2026-10-28

Initial release.

### Added

- Random Clojure program generator (`clojure-smith --seed N -o prog.clj`)
  with Csmith-style `--seed`, `--max-*`, and `--no-*` options.
- Generated programs are total and deterministic by construction: safe-math
  wrappers with wrap-around semantics, bounds-checked accessors, bounded
  loops, fuel-guarded recursion, no I/O besides one checksum line, and no
  randomness, time, reflection, interop, or concurrency.
- Static work budget: per-function call/loop/fuel budgets with shrinking
  (documented in the README), so generated programs run in seconds at any
  option setting instead of timing out on pathological fan-out.
- FNV-1a checksum over all global state and function results as the
  differential-test oracle.
- `drive` subcommand: generate programs for many seeds, run each under
  several runners (default: plain vs direct-linking Clojure), and save
  divergences.
- `reduce` subcommand: minimize a failing program while a test command
  still reports it interesting (delta minimization + constant replacement).
- Regression suite: unit tests, structural invariant tests, and end-to-end
  tests that execute generated programs with the Clojure CLI.
