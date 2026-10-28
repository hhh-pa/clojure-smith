//! Command-line options for program generation.
//!
//! The flags mirror Csmith's naming style (`--seed`, `--max-*`, `--no-*`),
//! adapted from C concepts to Clojure concepts.

use clap::Args;
use std::path::PathBuf;

/// Options controlling the random program generator.
///
/// Used directly by the default (generate) mode, and also parsed from the
/// trailing arguments of the `drive` subcommand.
#[derive(Args, Debug, Clone)]
pub struct GenerateOptions {
    /// Random seed. When omitted, a random seed is chosen and reported on
    /// stderr so the run can be reproduced.
    #[arg(long)]
    pub seed: Option<i64>,

    /// Write the generated program to FILE instead of stdout.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Maximum number of generated functions.
    #[arg(long, default_value_t = 5)]
    pub max_funcs: u32,

    /// Maximum number of parameters per function.
    #[arg(long, default_value_t = 3)]
    pub max_params: u32,

    /// Maximum number of global state atoms.
    #[arg(long, default_value_t = 4)]
    pub max_globals: u32,

    /// Maximum number of record types.
    #[arg(long, default_value_t = 2)]
    pub max_records: u32,

    /// Maximum number of fields per record type.
    #[arg(long, default_value_t = 4)]
    pub max_record_fields: u32,

    /// Maximum nesting depth of control forms (let/if/loop/case/cond).
    #[arg(long, default_value_t = 3)]
    pub max_block_depth: u32,

    /// Maximum number of statements per body.
    #[arg(long, default_value_t = 4)]
    pub max_block_size: u32,

    /// Maximum nesting depth of expressions.
    #[arg(long, default_value_t = 4)]
    pub max_expr_depth: u32,

    /// Maximum length of generated vector literals (at most 1024).
    #[arg(long, default_value_t = 4)]
    pub max_vector_len: u32,

    /// Maximum size of generated map literals.
    #[arg(long, default_value_t = 4)]
    pub max_map_size: u32,

    /// Maximum length of generated string literals (at most 4096).
    #[arg(long, default_value_t = 16)]
    pub max_string_len: u32,

    /// Maximum trip count of generated loops.
    #[arg(long, default_value_t = 16)]
    pub max_loop_iters: u32,

    /// Maximum initial fuel of recursive calls.
    #[arg(long, default_value_t = 8)]
    pub max_recursion_depth: u32,

    /// Do not generate bitwise operations.
    #[arg(long)]
    pub no_bitops: bool,

    /// Do not generate integer division/remainder.
    #[arg(long)]
    pub no_divs: bool,

    /// Do not generate doubles.
    #[arg(long)]
    pub no_doubles: bool,

    /// Do not generate vectors.
    #[arg(long)]
    pub no_vectors: bool,

    /// Do not generate maps.
    #[arg(long)]
    pub no_maps: bool,

    /// Do not generate strings.
    #[arg(long)]
    pub no_strings: bool,

    /// Do not generate records.
    #[arg(long)]
    pub no_records: bool,

    /// Do not generate loops (`loop`, `dotimes`, `doseq`, `for`).
    /// A bare `(range n)` is still allowed: it is inert unrealized data,
    /// and its realization size is bounded separately (see `gen_vec_op`).
    #[arg(long)]
    pub no_loops: bool,

    /// Do not generate recursive functions.
    #[arg(long)]
    pub no_recursion: bool,

    /// Do not generate global state.
    #[arg(long)]
    pub no_globals: bool,

    /// Do not generate higher-order sequence operations (mapv/filterv/reduce).
    #[arg(long)]
    pub no_higher_order: bool,

    /// Use raw arithmetic (which may throw on overflow/divide-by-zero)
    /// instead of the total safe-math wrappers. Outcomes stay deterministic:
    /// the entry point reports thrown exceptions by class name.
    #[arg(long)]
    pub no_safe_math: bool,

    /// Do not print the checksum of global state; print "done" instead.
    #[arg(long)]
    pub no_checksum: bool,

    /// Suppress the statistics comment and stderr chatter.
    #[arg(long)]
    pub quiet: bool,
}

impl GenerateOptions {
    /// Check option values for consistency. Returns a human-readable error.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_params > 20 {
            return Err("--max-params cannot exceed 20 (Clojure/JVM arity limit)".to_string());
        }
        // Larger maps become PersistentHashMaps, whose iteration/printing
        // order is hash-dependent and therefore not guaranteed stable
        // across Clojure versions. Capping at 8 keeps every map an
        // array-map with deterministic insertion order, which the checksum
        // relies on.
        if self.max_map_size > 8 {
            return Err(
                "--max-map-size cannot exceed 8 (larger maps lose deterministic ordering)"
                    .to_string(),
            );
        }
        // Sizes multiply (a vector literal holds many strings), so these
        // ceilings are much lower than the generic one below; draws are
        // clamped to the same bounds for library users who skip validate().
        if self.max_vector_len > crate::types::MAX_VECTOR_DRAW as u32 {
            return Err(format!(
                "--max-vector-len cannot exceed {} (sizes multiply across nesting)",
                crate::types::MAX_VECTOR_DRAW
            ));
        }
        if self.max_string_len > crate::types::MAX_STRING_DRAW as u32 {
            return Err(format!(
                "--max-string-len cannot exceed {} (sizes multiply across nesting)",
                crate::types::MAX_STRING_DRAW
            ));
        }
        for (name, value) in [
            ("--max-funcs", self.max_funcs),
            ("--max-globals", self.max_globals),
            ("--max-records", self.max_records),
            ("--max-record-fields", self.max_record_fields),
            ("--max-block-depth", self.max_block_depth),
            ("--max-block-size", self.max_block_size),
            ("--max-expr-depth", self.max_expr_depth),
            ("--max-loop-iters", self.max_loop_iters),
            ("--max-recursion-depth", self.max_recursion_depth),
        ] {
            if value > 1_000_000 {
                return Err(format!("{name} is unreasonably large: {value}"));
            }
        }
        Ok(())
    }

    /// One-line summary of the non-default options, for the header comment.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.max_funcs != 5 {
            parts.push(format!("max-funcs={}", self.max_funcs));
        }
        if self.max_params != 3 {
            parts.push(format!("max-params={}", self.max_params));
        }
        if self.max_globals != 4 {
            parts.push(format!("max-globals={}", self.max_globals));
        }
        if self.max_records != 2 {
            parts.push(format!("max-records={}", self.max_records));
        }
        if self.max_record_fields != 4 {
            parts.push(format!("max-record-fields={}", self.max_record_fields));
        }
        if self.max_block_depth != 3 {
            parts.push(format!("max-block-depth={}", self.max_block_depth));
        }
        if self.max_block_size != 4 {
            parts.push(format!("max-block-size={}", self.max_block_size));
        }
        if self.max_expr_depth != 4 {
            parts.push(format!("max-expr-depth={}", self.max_expr_depth));
        }
        if self.max_vector_len != 4 {
            parts.push(format!("max-vector-len={}", self.max_vector_len));
        }
        if self.max_map_size != 4 {
            parts.push(format!("max-map-size={}", self.max_map_size));
        }
        if self.max_string_len != 16 {
            parts.push(format!("max-string-len={}", self.max_string_len));
        }
        if self.max_loop_iters != 16 {
            parts.push(format!("max-loop-iters={}", self.max_loop_iters));
        }
        if self.max_recursion_depth != 8 {
            parts.push(format!("max-recursion-depth={}", self.max_recursion_depth));
        }
        for (flag, set) in [
            ("no-bitops", self.no_bitops),
            ("no-divs", self.no_divs),
            ("no-doubles", self.no_doubles),
            ("no-vectors", self.no_vectors),
            ("no-maps", self.no_maps),
            ("no-strings", self.no_strings),
            ("no-records", self.no_records),
            ("no-loops", self.no_loops),
            ("no-recursion", self.no_recursion),
            ("no-globals", self.no_globals),
            ("no-higher-order", self.no_higher_order),
            ("no-safe-math", self.no_safe_math),
            ("no-checksum", self.no_checksum),
        ] {
            if set {
                parts.push(flag.to_string());
            }
        }
        if parts.is_empty() {
            "defaults".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// Parse generator options from trailing `drive` arguments, e.g.
/// `["--max-funcs", "8", "--no-loops"]`.
pub fn parse_generator_args<I, S>(args: I) -> Result<GenerateOptions, String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    use clap::Parser;
    #[derive(Parser)]
    #[command(name = "clojure-smith")]
    struct GenCli {
        #[command(flatten)]
        opts: GenerateOptions,
    }
    let argv: Vec<std::ffi::OsString> = std::iter::once("clojure-smith".into())
        .chain(args.into_iter().map(Into::into))
        .collect();
    match GenCli::try_parse_from(argv) {
        Ok(cli) => Ok(cli.opts),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> GenerateOptions {
        parse_generator_args::<Vec<String>, String>(Vec::new()).expect("defaults parse")
    }

    #[test]
    fn default_values() {
        let o = defaults();
        assert_eq!(o.max_funcs, 5);
        assert_eq!(o.max_params, 3);
        assert_eq!(o.max_map_size, 4);
        assert!(!o.no_safe_math);
        assert!(o.seed.is_none());
        assert!(o.output.is_none());
        o.validate().expect("defaults validate");
        assert_eq!(o.summary(), "defaults");
    }

    #[test]
    fn parses_long_and_short_forms() {
        let o = parse_generator_args(["--seed", "42", "--max-funcs=10", "-o", "out.clj"]).unwrap();
        assert_eq!(o.seed, Some(42));
        assert_eq!(o.max_funcs, 10);
        assert_eq!(o.output, Some(PathBuf::from("out.clj")));
        let o = parse_generator_args(["--seed=-7", "--no-loops", "--no-safe-math"]).unwrap();
        assert_eq!(o.seed, Some(-7));
        assert!(o.no_loops);
        assert!(o.no_safe_math);
    }

    #[test]
    fn ceilings_reject_multiplicative_sizes() {
        let mut o = defaults();
        o.max_string_len = crate::types::MAX_STRING_DRAW as u32 + 1;
        assert!(o.validate().is_err());
        let mut o = defaults();
        o.max_vector_len = crate::types::MAX_VECTOR_DRAW as u32 + 1;
        assert!(o.validate().is_err());
        let mut o = defaults();
        o.max_string_len = crate::types::MAX_STRING_DRAW as u32;
        o.max_vector_len = crate::types::MAX_VECTOR_DRAW as u32;
        o.validate().expect("ceilings themselves validate");
    }

    #[test]
    fn rejects_unknown_flags_and_bad_values() {
        assert!(parse_generator_args(["--bogus"]).is_err());
        assert!(parse_generator_args(["--seed", "abc"]).is_err());
        assert!(parse_generator_args(["--seed"]).is_err());
        assert!(parse_generator_args(["positional"]).is_err());
    }

    #[test]
    fn validation_caps() {
        let mut o = defaults();
        o.max_params = 21;
        assert!(o.validate().is_err());
        o.max_params = 20;
        assert!(o.validate().is_ok());
        o.max_map_size = 9;
        assert!(o.validate().is_err());
        o.max_map_size = 8;
        assert!(o.validate().is_ok());
        o.max_funcs = 1_000_001;
        assert!(o.validate().is_err());
    }

    #[test]
    fn summary_lists_overrides() {
        let o = parse_generator_args(["--max-funcs", "1", "--no-divs"]).unwrap();
        assert_eq!(o.summary(), "max-funcs=1 no-divs");
    }
}
