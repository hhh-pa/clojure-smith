//! The value-type lattice tracked during generation.
//!
//! Every generated expression has a statically known [`ValType`], which is
//! how the generator keeps programs well-typed by construction: indexing,
//! map lookup, division, and string slicing all go through total helpers,
//! and every operand is generated at the type its operator requires.
//!
//! Composites are homogeneous with scalar-or-record elements; record fields
//! are scalars. This deliberately excludes nested collections, which are
//! future work (see the README).

use crate::form::{Form, print_compact};
use crate::options::GenerateOptions;
use crate::rng::Rng;

/// A type of runtime values in generated programs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ValType {
    Long,
    Double,
    Bool,
    Str,
    /// Homogeneous vector.
    Vec(Box<ValType>),
    /// Map with keyword keys and homogeneous values.
    Map(Box<ValType>),
    /// Index into the program's record table.
    Record(usize),
}

/// A generated `defrecord` type.
#[derive(Clone, Debug)]
pub struct RecordDef {
    /// Bare name, e.g. `"rec0"`.
    pub name: String,
    /// `(field-name, field-type)` pairs; field types are always scalars.
    pub fields: Vec<(String, ValType)>,
}

impl RecordDef {
    /// The positional factory name, e.g. `"->rec0"`.
    pub fn ctor_name(&self) -> String {
        format!("->{}", self.name)
    }
}

/// The fixed pool of map keys. Associating only pooled keys bounds every
/// map at 8 entries, which keeps all maps array-maps with deterministic
/// insertion order (larger maps would become hash-maps with
/// hash-dependent order).
pub const MAP_KEYS: [&str; 8] = ["k0", "k1", "k2", "k3", "k4", "k5", "k6", "k7"];

impl ValType {
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            ValType::Long | ValType::Double | ValType::Bool | ValType::Str
        )
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, ValType::Long | ValType::Double)
    }
}

/// A random scalar type honoring the `--no-*` flags.
pub fn random_scalar_type(rng: &mut Rng, opts: &GenerateOptions) -> ValType {
    // Tags: 0 = long, 1 = double, 2 = bool, 3 = string.
    let mut kinds: Vec<(u64, u8)> = vec![(30, 0), (15, 2)];
    if !opts.no_doubles {
        kinds.push((12, 1));
    }
    if !opts.no_strings {
        kinds.push((10, 3));
    }
    let weights: Vec<u64> = kinds.iter().map(|(w, _)| *w).collect();
    match kinds[rng.pick_index_weighted(&weights)].1 {
        0 => ValType::Long,
        1 => ValType::Double,
        2 => ValType::Bool,
        _ => ValType::Str,
    }
}

/// A random scalar-or-record type: the element type of composites.
pub fn random_elem_type(rng: &mut Rng, opts: &GenerateOptions, records: &[RecordDef]) -> ValType {
    if !opts.no_records && !records.is_empty() && rng.chance(0.15) {
        ValType::Record(rng.below(records.len()))
    } else {
        random_scalar_type(rng, opts)
    }
}

/// A random type of any kind, honoring the `--no-*` flags.
pub fn random_type(rng: &mut Rng, opts: &GenerateOptions, records: &[RecordDef]) -> ValType {
    // Tags: 0 = long, 1 = double, 2 = bool, 3 = string, 4 = vec, 5 = map,
    // 6 = record.
    let mut kinds: Vec<(u64, u8)> = vec![(30, 0), (15, 2)];
    if !opts.no_doubles {
        kinds.push((12, 1));
    }
    if !opts.no_strings {
        kinds.push((10, 3));
    }
    if !opts.no_vectors {
        kinds.push((12, 4));
    }
    if !opts.no_maps {
        kinds.push((10, 5));
    }
    if !opts.no_records && !records.is_empty() {
        kinds.push((8, 6));
    }
    let weights: Vec<u64> = kinds.iter().map(|(w, _)| *w).collect();
    match kinds[rng.pick_index_weighted(&weights)].1 {
        0 => ValType::Long,
        1 => ValType::Double,
        2 => ValType::Bool,
        3 => ValType::Str,
        4 => ValType::Vec(Box::new(random_elem_type(rng, opts, records))),
        5 => ValType::Map(Box::new(random_elem_type(rng, opts, records))),
        _ => ValType::Record(rng.below(records.len())),
    }
}

const LONG_EDGES: &[i64] = &[
    0,
    1,
    -1,
    2,
    -2,
    7,
    10,
    42,
    100,
    127,
    128,
    255,
    256,
    32767,
    65535,
    65536,
    1_000_000,
    i32::MAX as i64,
    i32::MIN as i64,
    1 << 32,
    -(1 << 32),
    i64::MAX,
    i64::MIN,
    i64::MAX - 1,
    i64::MIN + 1,
];

fn gen_long(rng: &mut Rng) -> i64 {
    let r = rng.below(100);
    if r < 45 {
        *rng.pick(LONG_EDGES)
    } else if r < 75 {
        rng.range(-1000, 1000)
    } else {
        rng.next_u64() as i64
    }
}

// The short decimals below are intentional approximations (they exercise
// decimal literal parsing, not the constants themselves).
#[allow(clippy::approx_constant)]
const DOUBLE_EDGES: &[f64] = &[
    0.0,
    1.0,
    -1.0,
    0.5,
    -0.5,
    2.5,
    100.0,
    -100.0,
    1e10,
    -1e10,
    1e-10,
    1e15,
    3.14159,
    2.71828,
    f64::MAX,
    f64::MIN,
    -0.0,
    5e-324,
    -5e-324,
    f64::MIN_POSITIVE,
    f64::INFINITY,
    f64::NEG_INFINITY,
];

/// Random double literal. NaN is deliberately excluded: it is deterministic
/// but poisons comparisons without adding bug-finding signal.
fn gen_double(rng: &mut Rng) -> f64 {
    if rng.chance(0.45) {
        *rng.pick(DOUBLE_EDGES)
    } else {
        let u = rng.next_u64() as f64 / u64::MAX as f64;
        let mut v = u * 2000.0 - 1000.0;
        match rng.below(100) {
            r if r < 10 => v *= 1e15,
            r if r < 20 => v *= 1e-15,
            _ => {}
        }
        v
    }
}

const TRICKY_STRINGS: &[&str] = &[
    "",
    "\"",
    "\\",
    "\n",
    "\t",
    "a\nb",
    "  padded  ",
    "nil",
    "true",
    "123",
    "##Inf",
    ":keyword",
    "(list)",
    "{map}",
    "[vec]",
    "a,b,c",
    "%s %d",
    "héllo",
    "tab\there",
    "quote\"inside",
    "\r",
    "a\rb",
    "\u{8}",
    "\u{c}",
    "\u{7f}",
];

const STR_ALPHABET: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 _-+.,!?";

/// Hard ceiling on a drawn string length. String and vector sizes
/// multiply (a vector literal holds many strings), so one uncapped draw
/// could allocate gigabytes in a single literal; totals beyond that are
/// bounded by budget charging of literal sizes.
pub const MAX_STRING_DRAW: usize = 4096;

/// Hard ceiling on a drawn vector literal length (see `MAX_STRING_DRAW`).
pub const MAX_VECTOR_DRAW: usize = 1024;

fn gen_string(rng: &mut Rng, max_len: usize, budget: u64) -> String {
    if rng.chance(0.15) {
        // Fixed short strings; the few bytes of overshoot past `budget`
        // are negligible and keep tricky values in circulation.
        return rng.pick(TRICKY_STRINGS).to_string();
    }
    let n = rng.below(max_len.min(MAX_STRING_DRAW).min(budget_usize(budget)) + 1);
    (0..n).map(|_| *rng.pick(STR_ALPHABET) as char).collect()
}

fn budget_usize(budget: u64) -> usize {
    budget.min(usize::MAX as u64) as usize
}

/// A random literal of type `t`, with its work cost in printed bytes.
///
/// Draws are clamped to `budget` (split evenly across composite
/// elements), so one literal can never allocate gigabytes no matter the
/// option maxima; the cost accounts the literal's actual printed size,
/// which bounds total literal bytes by the caller's budget.
pub fn gen_literal(
    rng: &mut Rng,
    opts: &GenerateOptions,
    records: &[RecordDef],
    t: &ValType,
    budget: u64,
) -> (Form, u64) {
    let form = match t {
        ValType::Long => Form::int(gen_long(rng)),
        ValType::Double => Form::float(gen_double(rng)),
        ValType::Bool => Form::boolean(rng.next_bool()),
        ValType::Str => Form::string(gen_string(rng, opts.max_string_len as usize, budget)),
        ValType::Vec(elem) => {
            let max = (opts.max_vector_len as usize)
                .min(MAX_VECTOR_DRAW)
                .min(budget_usize(budget));
            let n = rng.below(max + 1);
            let per = budget / n.max(1) as u64;
            Form::vector(
                (0..n)
                    .map(|_| gen_literal(rng, opts, records, elem, per).0)
                    .collect(),
            )
        }
        ValType::Map(val) => {
            let max = (opts.max_map_size.min(8) as usize).min(budget_usize(budget));
            let n = rng.below(max + 1);
            let per = budget / n.max(1) as u64;
            let mut keys = MAP_KEYS.to_vec();
            rng.shuffle(&mut keys);
            Form::map(
                keys.into_iter()
                    .take(n)
                    .map(|k| (Form::kw(k), gen_literal(rng, opts, records, val, per).0))
                    .collect(),
            )
        }
        ValType::Record(i) => {
            let r = &records[*i];
            let per = budget / r.fields.len().max(1) as u64;
            Form::call(
                &r.ctor_name(),
                r.fields
                    .iter()
                    .map(|(_, ft)| gen_literal(rng, opts, records, ft, per).0)
                    .collect(),
            )
        }
    };
    let cost = print_compact(&form).len() as u64;
    (form, cost)
}

/// A deterministic default value of type `t`, for total accessors.
pub fn default_value(records: &[RecordDef], t: &ValType) -> Form {
    match t {
        ValType::Long => Form::int(0),
        ValType::Double => Form::float(0.0),
        ValType::Bool => Form::boolean(false),
        ValType::Str => Form::string(""),
        ValType::Vec(_) => Form::vector(vec![]),
        ValType::Map(_) => Form::map(vec![]),
        ValType::Record(i) => {
            let r = &records[*i];
            Form::call(
                &r.ctor_name(),
                r.fields
                    .iter()
                    .map(|(_, ft)| default_value(records, ft))
                    .collect(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::{parse_one, print_compact};
    use crate::options::parse_generator_args;

    fn opts(args: &[&str]) -> GenerateOptions {
        parse_generator_args(args.iter().map(|s| s.to_string())).unwrap()
    }

    #[test]
    fn scalar_types_honor_flags() {
        let o = opts(&["--no-doubles", "--no-strings"]);
        let mut rng = Rng::new(1);
        for _ in 0..200 {
            assert!(matches!(
                random_scalar_type(&mut rng, &o),
                ValType::Long | ValType::Bool
            ));
            let t = random_type(&mut rng, &o, &[]);
            assert!(!contains(&t, &ValType::Double));
            assert!(!contains(&t, &ValType::Str));
        }
    }

    fn contains(t: &ValType, want: &ValType) -> bool {
        t == want
            || match t {
                ValType::Vec(e) | ValType::Map(e) => contains(e, want),
                _ => false,
            }
    }

    #[test]
    fn literal_draws_clamp_without_validate() {
        // Library users can skip validate(): absurd maxima must still
        // draw bounded literals, never gigabyte allocations.
        let o = opts(&["--max-string-len", "1000000", "--max-vector-len", "1000000"]);
        let mut rng = Rng::new(7);
        for _ in 0..50 {
            let lit = gen_literal(&mut rng, &o, &[], &ValType::Str, u64::MAX).0;
            if let Form::Str(s) = lit {
                assert!(s.len() <= MAX_STRING_DRAW, "string len {}", s.len());
            } else {
                panic!("expected string literal, got {lit:?}");
            }
            let lit = gen_literal(
                &mut rng,
                &o,
                &[],
                &ValType::Vec(Box::new(ValType::Long)),
                u64::MAX,
            )
            .0;
            if let Form::Vector(v) = lit {
                assert!(v.len() <= MAX_VECTOR_DRAW, "vector len {}", v.len());
            } else {
                panic!("expected vector literal, got {lit:?}");
            }
        }
    }

    #[test]
    fn literal_cost_is_printed_bytes() {
        let o = opts(&[]);
        let mut rng = Rng::new(9);
        let types = [
            ValType::Long,
            ValType::Double,
            ValType::Bool,
            ValType::Str,
            ValType::Vec(Box::new(ValType::Long)),
            ValType::Vec(Box::new(ValType::Str)),
            ValType::Map(Box::new(ValType::Str)),
        ];
        for t in &types {
            for _ in 0..20 {
                let (lit, cost) = gen_literal(&mut rng, &o, &[], t, u64::MAX);
                assert_eq!(cost, print_compact(&lit).len() as u64, "{t:?}");
            }
        }
    }

    #[test]
    fn literal_draws_fit_budget() {
        // Even with absurd maxima, a small budget yields a small literal:
        // punctuation overhead is the only slack over the budget.
        let o = opts(&["--max-string-len", "1000000", "--max-vector-len", "1000000"]);
        let mut rng = Rng::new(11);
        for _ in 0..50 {
            let (lit, cost) = gen_literal(
                &mut rng,
                &o,
                &[],
                &ValType::Vec(Box::new(ValType::Str)),
                1000,
            );
            let bytes = print_compact(&lit).len();
            assert_eq!(cost, bytes as u64);
            assert!(bytes <= 8000, "budget 1000 drew {bytes} bytes");
        }
        // Zero budget still yields well-typed (empty) literals.
        let (lit, _) = gen_literal(&mut rng, &o, &[], &ValType::Str, 0);
        assert!(matches!(lit, Form::Str(s) if s.len() <= 12));
    }

    #[test]
    fn double_edges_cover_the_nasty_values() {
        // The differential oracle is only as good as the edges it draws:
        // infinities, NaN, signed zero, smallest normal/denormal, and
        // extremes must all survive future edits of the table.
        assert!(DOUBLE_EDGES.contains(&f64::INFINITY));
        assert!(DOUBLE_EDGES.contains(&f64::NEG_INFINITY));
        // NaN stays OUT of the table by design (see gen_double): it
        // poisons comparisons without adding signal. Runtime NaN (0/0)
        // still exists, which is why sort-over-doubles is banned.
        assert!(DOUBLE_EDGES.iter().all(|x| !x.is_nan()));
        // (`contains` is useless for signed zero: -0.0 == 0.0.)
        assert!(DOUBLE_EDGES.iter().any(|x| x.to_bits() == 0.0f64.to_bits()));
        assert!(
            DOUBLE_EDGES
                .iter()
                .any(|x| x.to_bits() == (-0.0f64).to_bits())
        );
        assert!(DOUBLE_EDGES.contains(&f64::MIN));
        assert!(DOUBLE_EDGES.contains(&f64::MAX));
        assert!(DOUBLE_EDGES.contains(&f64::MIN_POSITIVE));
        assert!(DOUBLE_EDGES.contains(&5e-324));
        assert!(DOUBLE_EDGES.contains(&-5e-324));
        assert!(LONG_EDGES.contains(&i64::MIN));
        assert!(LONG_EDGES.contains(&i64::MAX));
    }

    #[test]
    fn tricky_strings_survive_printer_roundtrip() {
        // Every tricky string must print and re-parse to the identical string.
        for s in TRICKY_STRINGS {
            let printed = print_compact(&Form::Str((*s).to_owned()));
            let parsed = parse_one(&printed).unwrap_or_else(|e| panic!("{s:?} -> {printed} : {e}"));
            assert_eq!(
                parsed,
                Form::Str((*s).to_owned()),
                "'{printed}' changed meaning"
            );
        }
    }

    #[test]
    fn literals_print_and_vary() {
        let o = opts(&[]);
        let mut rng = Rng::new(2);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let lit = gen_literal(&mut rng, &o, &[], &ValType::Long, u64::MAX).0;
            seen.insert(print_compact(&lit));
        }
        assert!(seen.len() > 10, "long literals lack variety: {seen:?}");
        // Map literals stay within the key pool.
        for _ in 0..50 {
            let lit = gen_literal(
                &mut rng,
                &o,
                &[],
                &ValType::Map(Box::new(ValType::Long)),
                u64::MAX,
            )
            .0;
            if let Form::Map(pairs) = lit {
                assert!(pairs.len() <= 4);
                for (k, _) in &pairs {
                    match k {
                        Form::Keyword(k) => assert!(MAP_KEYS.contains(&k.as_str())),
                        other => panic!("non-keyword map key: {other:?}"),
                    }
                }
            } else {
                panic!("expected map literal");
            }
        }
    }

    #[test]
    fn record_literals_and_defaults() {
        let o = opts(&[]);
        let mut rng = Rng::new(3);
        let records = vec![RecordDef {
            name: "rec0".to_string(),
            fields: vec![
                ("f0".to_string(), ValType::Long),
                ("f1".to_string(), ValType::Str),
            ],
        }];
        let lit = gen_literal(&mut rng, &o, &records, &ValType::Record(0), u64::MAX).0;
        let text = print_compact(&lit);
        assert!(text.starts_with("(->rec0 "), "{text}");
        assert_eq!(
            print_compact(&default_value(&records, &ValType::Record(0))),
            "(->rec0 0 \"\")"
        );
    }
}
