# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-10-28

Initial release.

### Added

- Random Clojure program generator (`clojure-smith --seed N -o prog.clj`)
  with Csmith-style `--seed`, `--max-*`, and `--no-*` options.
- Generated programs are total and deterministic by construction: safe-math
  wrappers with wrap-around semantics, bounds-checked accessors, bounded
  loops, fuel-guarded recursion, no I/O besides one checksum line, and no
  randomness, time, reflection, interop, or concurrency.
- FNV-1a checksum over all global state and function results as the
  differential-test oracle.
- `drive` subcommand: generate programs for many seeds, run each under
  several runners (default: plain vs direct-linking Clojure), and save
  divergences.
- `reduce` subcommand: minimize a failing program while a test command
  still reports it interesting (delta minimization + constant replacement).
- Regression suite: unit tests, structural invariant tests, and end-to-end
  tests that execute generated programs with the Clojure CLI.
