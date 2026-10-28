# clojure-smith

**clojure-smith is a random generator of Clojure programs.** Its primary
purpose is to find compiler bugs with random programs, using differential
testing as the test oracle — the same idea as
[Csmith](https://github.com/csmith-project/csmith) (which targets C), applied
to Clojure. The tool itself is written in Rust; it generates self-contained
Clojure programs, runs them under multiple configurations, and reports any
output divergence.

```bash
clojure-smith --seed 42 -o prog.clj   # generate
clojure -M prog.clj                   # checksum: -3813667259982852713
```

## Relationship to Csmith

Csmith generates C programs free of undefined behavior and compares their
output across compilers and optimization levels. clojure-smith does the
analogous thing for Clojure: it generates programs free of *nondeterminism
and errors* (no undefined behavior exists in Clojure, but exceptions,
infinite loops, locale dependence, and unordered collections are the
corresponding hazards) and compares their output across Clojure versions,
JVMs, and compiler flags such as direct linking.

| Csmith (C)                | clojure-smith (Clojure)                    |
|---------------------------|--------------------------------------------|
| `csmith > out.c`          | `clojure-smith --seed N -o prog.clj`       |
| safe-math wrappers        | `s+`, `s-quot`, … (wrap-around, total)     |
| `transparent_crc` globals | FNV-1a `smith-hash` over atoms + results   |
| `--seed`, `--max-*`, `--no-*` | same flag style, adapted              |
| inspire C-Reduce          | built-in `reduce` subcommand               |
| tests gcc/clang -O levels | tests Clojure versions / JVMs / `-D` flags |

Like Csmith, when two configurations disagree on a generated program you have
found a bug in one of them — or, much less likely, in the generator itself.

## Install

Prerequisites: a Rust toolchain (`cargo`), plus Java and the
[Clojure CLI](https://clojure.org/guides/install_clojure) for *running*
generated programs (generation itself needs no JVM).

```bash
git clone https://github.com/hhh-pa/clojure-smith.git
cd clojure-smith
cargo build --release
./target/release/clojure-smith --help
```

## Quickstart

Generate one program and run it twice — the output must be identical:

```bash
clojure-smith --seed 1 -o prog.clj
clojure -M prog.clj
clojure -M prog.clj
```

Run the differential loop over 100 seeds (default runners: plain Clojure vs
direct linking); divergences land in `./failures/`:

```bash
clojure-smith drive --seeds 0-99 --jobs 4
```

Minimize a failing program while it still reproduces:

```bash
clojure-smith reduce --test "clojure -M {} | grep -q checksum" failures/seed-7/prog.clj -o small.clj
```

## Usage

### Generate (default mode)

```bash
clojure-smith [OPTIONS]
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--seed N` | random (reported) | RNG seed; same seed + options ⇒ byte-identical output |
| `-o, --output FILE` | stdout | write program to FILE |
| `--max-funcs N` | 5 | max generated functions |
| `--max-params N` | 3 | max params per function (≤ 20) |
| `--max-globals N` | 4 | max global state atoms |
| `--max-records N` | 2 | max record types |
| `--max-record-fields N` | 4 | max fields per record |
| `--max-block-depth N` | 3 | max control-form nesting |
| `--max-block-size N` | 4 | max statements per body |
| `--max-expr-depth N` | 4 | max expression nesting |
| `--max-vector-len N` | 4 | max vector literal length |
| `--max-map-size N` | 4 | max map size (≤ 8, see below) |
| `--max-string-len N` | 16 | max string literal length |
| `--max-loop-iters N` | 16 | max loop trip count |
| `--max-recursion-depth N` | 8 | max recursion fuel |
| `--no-bitops` | off | no bitwise operations |
| `--no-divs` | off | no integer division/remainder |
| `--no-doubles` | off | no doubles |
| `--no-vectors` | off | no vectors |
| `--no-maps` | off | no maps |
| `--no-strings` | off | no strings |
| `--no-records` | off | no records |
| `--no-loops` | off | no loops |
| `--no-recursion` | off | no recursive functions |
| `--no-globals` | off | no global state |
| `--no-higher-order` | off | no mapv/filterv/reduce |
| `--no-safe-math` | off | raw arithmetic (may throw; outcomes stay deterministic) |
| `--no-checksum` | off | print `done` instead of the checksum |
| `--quiet` | off | suppress header comment and stderr chatter |
| `-h, --help` | | show help |
| `--version` | | show version |

### `drive`: differential testing loop

```bash
clojure-smith drive --seeds 0-999 --jobs 8 -- --max-funcs 8 --no-loops
```

Generator options are forwarded after `--` (`--seed`/`--output` excluded).
`--runner CMD` (repeatable, `{}` = program file) defines the configurations
under test; the default pair is:

- `clojure -M {}`
- `clojure -J-Dclojure.compiler.direct-linking=true -M {}`

Useful additions: another Clojure version
(`clojure -Sdeps '{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}}' -M {}`),
another JDK, or extra `-J` flags. A seed is `OK` when every runner exits 0
with byte-identical stdout, `MISMATCH` when outputs differ, and `ERROR` when
any runner fails or times out (`--timeout`, default 120s). Failures are saved
to `--fail-dir/seed-N/` with the program and each runner's outputs.

Any `threw:` line under default options indicates a generator bug — please
report it with the seed.

### `reduce`: test-case minimization

```bash
clojure-smith reduce --test "./interesting.sh {}" prog.clj -o small.clj
```

The test command must exit 0 while the case is still interesting (e.g. still
diverges, still throws, still hangs — compose with `timeout(1)` for hangs).
The reducer alternatingly delta-minimizes top-level forms and replaces
subforms with constants until fixpoint or `--tries` (default 10000). It
understands the plain-Clojure syntax the generator emits; exotic reader
features (syntax-quote, tagged literals, `#()` fns, ratios, bigints) are
rejected with a clear error.

## How it works

Every generated program is a single self-contained file using only
`clojure.core` (plus `clojure.string` when strings are enabled): no
dependencies, no interop, no I/O besides the final line. Generation tracks a
static type for every expression, so programs are well-typed by construction.
Totality and determinism come from a set of deliberate rules:

- **Safe math**: `s+`/`s-`/`s*` wrap on overflow (two's complement),
  `s-quot`/`s-rem` define division by zero, `s-div` retypes double
  division onto the primitive path (raw `/` on `Object` operands throws
  on zero divisors instead of yielding `Inf`/`NaN`), and `s-long`
  defines conversion of infinities and out-of-range doubles. Raw
  operators appear only with `--no-safe-math`, and even then the entry
  point reports throws by class name for a deterministic oracle.
- **Total accessors**: `s-nth`/`s-peek`/`s-assoc-v`/`s-subvec`/`s-get`/
  `s-subs` take defaults or clamp instead of throwing.
- **Bounded loops**: `loop`/`dotimes`/`doseq`/`for`/`range` carry constant
  bounds; `while`, `cycle`, `repeat`, `iterate`, and unbounded `range` are
  never generated. Recursion is direct-only with a fuel parameter guarded by
  `(pos? fuel)`.
- **Terminating state updates**: an atom is never written inside its own
  `swap!` retry context — directly or transitively through called
  functions — which would otherwise retry forever. (Write sets are tracked
  per function for exactly this.)
- **Deterministic state**: maps are capped at 8 entries so they stay
  array-maps with insertion order (hash-map order is hash-dependent);
  locale-sensitive operations (`upper-case`, `format`) are excluded; there
  is no randomness, time, agents, futures, or reflection.
- **Stable oracle**: the checksum is a hand-rolled FNV-1a over `pr-str`,
  never `clojure.core/hash`, whose algorithm a future Clojure could change.
- **Boxed loop bindings**: `loop` inits are wrapped in `identity` so `recur`
  never trips over primitive locals.

Same seed + same options + same clojure-smith version ⇒ byte-identical
program (the RNG stream is pinned via `Cargo.lock`, not promised across
releases).

## Limitations and future work

- JVM Clojure only (generated code sticks to portable `clojure.core`, so
  ClojureScript/Babashka differential testing is a natural extension).
- No nested collections, sets, ratios, bigints, keywords-as-values,
  `nil`-producing control flow, lazy infinite seqs, multimethods,
  transducers, transients, or `deftype`.
- The reducer is structural and simple, not a full C-Reduce equivalent.
- No uberjar/`cargo install` packaging yet.

## Contributing

Issues and pull requests are welcome. Bugs reports should include the
failing seed and options (or, better, a minimized program from `reduce`).

```bash
cargo fmt --check && cargo clippy -- -D warnings && cargo test
```

## License

Distributed under the Eclipse Public License 1.0, the same license as
Clojure. See [LICENSE](LICENSE).

## Acknowledgments

Inspired by [Csmith](https://github.com/csmith-project/csmith) by Xuejun
Yang, Yang Chen, Eric Eide, and John Regehr (University of Utah), and their
paper [*Finding and Understanding Bugs in C
Compilers*](https://www.cs.utah.edu/~regehr/papers/pldi11-preprint.pdf).
