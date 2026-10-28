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
- Realistic ceilings for every `--max-*` count/depth option (1000 funcs /
  1000 globals / 100 records / 64 fields / 32 block depth / 128 block size
  / 128 expr depth); absurd values are rejected instead of hanging.
- Function arities never exceed `--max-params`, even with many globals
  and the fuel parameter competing for slots.
- Literals are charged by printed bytes and shrink to fit the budget;
  global initializers are capped at 1 MiB; the generator hard-stops on
  an empty budget instead of overshooting (a hostile maxima combination
  went from 43 s / 61 MiB to ~2 s bounded).
- New wall-clock generation deadline (library `gen_timeout`); an expired
  deadline fails one seed as a drive `Error`, never a hang.
- `sort-by` is never generated over doubles (NaN breaks sort's contract);
  superlinear data ops carry a budget premium and nested loops cap trips.
- A seed where every runner throws is saved under default options (likely
  generator bug) and still `Error`, not `OK`.
- `drive` drains runner output without ever blocking or being SIGPIPE'd
  (discards past 1 MiB, flagged), fixes the timeout/exit race, kills the
  child even when the timeout arm errors, caps `--jobs` at 256, quotes
  non-UTF-8 paths, and stages programs in unpredictable `O_EXCL` tmp dirs.
- `reduce` aborts after 3 consecutive environment failures instead of
  burning `--tries` on a broken setup, and verifies a 127 exit against
  the real `PATH` before blaming the test command.
- The reader enforces a nesting limit (was stack overflow / SIGABRT),
  supports non-ASCII symbols, char delimiters (`\a`, `\1`), verbatim
  regex escapes, and `#!` shebang lines, with clearer error messages.
- `Rng::range_usize` covers the full span and `pick_index_weighted`
  cannot overflow on huge weights.
- Fixed an integration-test flake on cold machines: concurrent
  first-run `clojure` invocations race populating the dependency cache
  and losers die with `ClassNotFoundException: clojure.main`. The
  suite now warms the cache once, serially, before spawning JVMs.
- The reduce test module's `unique_tag` import is unix-gated (Windows
  CI tripped `-D warnings` on it).
- Drive-test failures now dump the saved per-runner outputs into the
  test log instead of reporting only "exit 1".

### Added

- `reduce --timeout-is-interesting`: reduce hangs by treating test
  timeouts as interesting (pair with a short `--timeout`).
- Reducer drop pass: lists also try each child (`(println X)` to `X`).
- `drive` exit code 3 for errors without any clean comparison
  (0 = agree, 1 = divergences, 2 = bad usage).
- Structural invariant tests: suppression matrix, loop growth cap, range
  budget, emit/reparse roundtrip, case/map shapes, zero options, RNG
  golden stream; end-to-end suppressed-feature run matrix.
- `drive --show-outputs`: print each runner's stdout on mismatch.
- New invariant/unit coverage: option ceilings, arity limits, literal
  byte costs, generation deadline, partial-op bans, timeout errors,
  reader edge cases, RNG span/overflow, tricky-string roundtrips; a
  Windows CI job for the non-Unix cfg branches.

### Changed

- The RNG stream changed (`below`/`chance` mappings): same seed, new
  programs. Re-baseline any pinned checksums.
- The RNG stream changed again (`range_usize` span, weighted-index
  mapping): same seed, new programs. Re-baseline any pinned checksums.

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
